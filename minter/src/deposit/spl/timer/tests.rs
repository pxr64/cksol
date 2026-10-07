use super::*;
use crate::sol_transfer::{MAX_SIGNATURES, MAX_TX_SIZE};
use crate::{
    lifecycle,
    state::{
        TokenProgram,
        audit::{process_event, replay_events},
        event::{Event, EventType, TransactionPurpose},
        mutate_state, read_state,
    },
    storage::with_event_iter,
    test_fixtures::{
        DEFAULT_BLOCK_HEIGHT, EventsAssert, account, confirmed_block, minter_signature,
        minter_signature_nth, queued_spl_deposit,
        runtime::TestCanisterRuntime,
        schnorr_master_key, schnorr_master_key_response, signature,
        signer::{sign_as_minter, sign_for},
        spl_token, valid_init_args,
    },
};
use assert_matches::assert_matches;
use ic_canister_runtime::IcError;
use ic_cdk::call::CallRejected;
use ic_cdk_management_canister::SignCallError;
use ic_stable_structures::Storable;
use sol_rpc_types::{MultiRpcResult, RpcError};

fn submit_batch(ids: &[u64]) -> (Signature, SplSweep) {
    let (deposits, tokens) = read_state(|state| {
        let deposits = ids
            .iter()
            .map(|id| (*id, state.spl_deposits().queued()[id].clone()))
            .collect::<Vec<_>>();
        let tokens = deposits
            .iter()
            .map(|(_, deposit)| {
                (
                    deposit.mint,
                    state.supported_spl_token(&deposit.mint).cloned().unwrap(),
                )
            })
            .collect();
        (deposits, tokens)
    });
    let plan = SplSweep::plan(deposits, &tokens, &schnorr_master_key());
    let message = plan.sweep_message(Hash::default());
    let signature =
        signature(100 + ids[0] as usize + read_state(|state| state.failed_transactions().len()));
    mutate_state(|state| {
        process_event(
            state,
            EventType::SubmittedTransaction {
                signature,
                signers: plan.signers(&message),
                message: message.into(),
                purpose: TransactionPurpose::SweepSplDeposits {
                    deposit_ids: ids.to_vec(),
                },
                block_height: DEFAULT_BLOCK_HEIGHT,
            },
            &TestCanisterRuntime::new().with_increasing_time(),
        )
    });
    (signature, plan)
}

fn split_batch(ids: &[u64]) {
    let (signature, _) = submit_batch(ids);
    mutate_state(|state| {
        process_event(
            state,
            EventType::SplitFailedSplSweep { signature },
            &TestCanisterRuntime::new().with_increasing_time(),
        )
    });
}

fn failed_status(
    confirmation: sol_rpc_types::TransactionConfirmationStatus,
) -> sol_rpc_types::TransactionStatus {
    sol_rpc_types::TransactionStatus {
        slot: block().slot,
        status: Err(sol_rpc_types::TransactionError::InsufficientFundsForFee),
        err: Some(sol_rpc_types::TransactionError::InsufficientFundsForFee),
        confirmation_status: Some(confirmation),
    }
}

#[test]
fn should_keep_individual_retries_separate_from_normal_batches() {
    queue_deposits(6, false, true, Some(TokenProgram::Classic));
    split_batch(&[2, 3]);
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));
    let ids = round
        .batches
        .iter()
        .map(|sweep| sweep.deposits().keys().copied().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![vec![0, 1], vec![2], vec![3], vec![4, 5]]);
    assert!(!round.leaves_deposits_queued);
}

#[test]
fn should_cap_individual_retries_at_the_existing_round_limit() {
    queue_deposits(12, false, true, Some(TokenProgram::Classic));
    split_batch(&[0, 1, 2, 3, 4, 5]);
    split_batch(&[6, 7, 8, 9, 10, 11]);
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));
    assert_eq!(round.batches.len(), MAX_CONCURRENT_RPC_CALLS);
    assert!(
        round
            .batches
            .iter()
            .all(|batch| batch.deposits().len() == 1)
    );
    assert!(round.leaves_deposits_queued);
}

#[test]
#[should_panic(expected = "marked for an individual sweep")]
fn should_reject_rebatching_individual_spl_retries() {
    queue_deposits(2, false, true, Some(TokenProgram::Classic));
    split_batch(&[0, 1]);
    submit_batch(&[0, 1]);
}

#[tokio::test]
async fn should_split_only_finalized_failed_spl_batches() {
    queue_deposits(2, false, true, Some(TokenProgram::Classic));
    let (signature, plan) = submit_batch(&[0, 1]);
    let runtime = TestCanisterRuntime::new()
        .with_increasing_time()
        .add_recent_block(Ok(block().slot))
        .add_stub_response(MultiRpcResult::Consistent(Ok(vec![Some(failed_status(
            sol_rpc_types::TransactionConfirmationStatus::Finalized,
        ))])));
    crate::monitor::finalize_transactions(runtime).await;
    EventsAssert::from_recorded()
        .expect_contains_event_eq(EventType::SplitFailedSplSweep { signature });
    let replayed = replay_events(with_event_iter(|events| {
        events
            .map(|event| Event::from_bytes(event.to_bytes()))
            .collect::<Vec<_>>()
    }));
    read_state(|state| {
        assert_eq!(state, &replayed);
        assert_eq!(state.spl_deposits().queued(), plan.deposits());
        assert!(state.spl_deposits().dropped().is_empty());
        assert!(state.failed_transactions().contains_key(&signature));
    });
}

#[tokio::test]
async fn should_keep_uncertain_or_unfinalized_spl_batches_tracked() {
    for case in 0..3 {
        crate::state::reset_state();
        crate::storage::reset_events();
        queue_deposits(2, false, true, Some(TokenProgram::Classic));
        submit_batch(&[0, 1]);
        let result = match case {
            0 => MultiRpcResult::<Vec<Option<sol_rpc_types::TransactionStatus>>>::Inconsistent(
                vec![],
            ),
            1 => MultiRpcResult::Consistent(Ok(vec![None])),
            _ => MultiRpcResult::Consistent(Ok(vec![Some(failed_status(
                sol_rpc_types::TransactionConfirmationStatus::Confirmed,
            ))])),
        };
        let before = EventsAssert::from_recorded();
        let runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_recent_block(Ok(block().slot))
            .add_stub_response(result);
        crate::monitor::finalize_transactions(runtime).await;
        assert_eq!(EventsAssert::from_recorded(), before);
        read_state(|state| {
            assert!(state.spl_deposits().queued().is_empty());
            assert_eq!(state.spl_deposits().swept().len(), 1);
            assert_eq!(state.submitted_transactions().len(), 1);
        });
    }
}

#[tokio::test]
async fn should_submit_individual_retries_and_drop_only_the_failed_deposit() {
    queue_deposits(2, false, true, Some(TokenProgram::Classic));
    split_batch(&[0, 1]);
    let runtime = TestCanisterRuntime::new()
        .with_increasing_time()
        .with_schnorr_public_key(schnorr_master_key_response())
        .add_recent_block(Ok(block().slot))
        .add_signer(sign_as_minter().times(2))
        .add_signer(sign_for(&account(1)))
        .add_signer(sign_for(&account(2)))
        .add_stub_response(MultiRpcResult::<sol_rpc_types::Signature>::Consistent(Ok(
            minter_signature().into(),
        )))
        .add_stub_response(MultiRpcResult::<sol_rpc_types::Signature>::Consistent(Ok(
            minter_signature_nth(1).into(),
        )));
    sweep_queued_spl_deposits(runtime).await;
    read_state(|state| {
        assert!(state.spl_deposits().queued().is_empty());
        assert_eq!(state.spl_deposits().swept().len(), 2);
        assert!(!state.spl_deposits().requires_individual_sweep(0));
        assert!(!state.spl_deposits().requires_individual_sweep(1));
    });
    let statuses = read_state(|state| {
        state
            .submitted_transactions()
            .keys()
            .map(|signature| {
                let mut status =
                    failed_status(sol_rpc_types::TransactionConfirmationStatus::Finalized);
                if *signature != minter_signature() {
                    status.err = None;
                    status.status = Ok(());
                }
                Some(status)
            })
            .collect::<Vec<_>>()
    });
    let runtime = TestCanisterRuntime::new()
        .with_increasing_time()
        .add_recent_block(Ok(block().slot))
        .add_stub_response(MultiRpcResult::Consistent(Ok(statuses)))
        .add_stub_response(crate::test_fixtures::GetTransactionResult::Consistent(Ok(
            None,
        )));
    crate::monitor::finalize_transactions(runtime).await;
    read_state(|state| {
        assert_eq!(state.spl_deposits().dropped().len(), 1);
        assert!(state.spl_deposits().dropped().contains_key(&0));
        assert_eq!(
            state
                .spl_deposits()
                .finalized()
                .get(&minter_signature_nth(1))
                .unwrap()
                .deposits()
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![1]
        );
        let dropped = &state.spl_deposits().dropped()[&0].deposit;
        assert!(
            state
                .spl_deposits()
                .in_flight_id(&dropped.account, &dropped.mint)
                .is_none()
        );
    });
    let mut replayed = replay_events(with_event_iter(|events| {
        events
            .map(|event| Event::from_bytes(event.to_bytes()))
            .collect::<Vec<_>>()
    }));
    if let Some(key) = read_state(|state| state.minter_public_key().cloned()) {
        replayed.cache_minter_public_key(key);
    }
    read_state(|state| assert_eq!(state, &replayed));
}
use solana_hash::Hash;
use solana_transaction::Transaction;

fn queued_sweep() -> SplSweep {
    read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()))
        .batches
        .into_iter()
        .next()
        .unwrap()
}

fn block() -> Block {
    Block {
        slot: 300_000_000,
        blockhash: Hash::new_from_array([0xBB; 32]),
        block_height: DEFAULT_BLOCK_HEIGHT,
    }
}

fn signing_runtime() -> TestCanisterRuntime {
    TestCanisterRuntime::new()
        .with_increasing_time()
        .add_signer(sign_as_minter())
        .add_signer(sign_for(&account(1)))
}

fn fetched_block() -> Block {
    Block {
        blockhash: confirmed_block().blockhash.to_string().parse().unwrap(),
        ..block()
    }
}

#[tokio::test]
async fn should_return_early_when_spl_queue_is_empty() {
    queue_deposits(0, false, true, Some(TokenProgram::Classic));
    let before = EventsAssert::from_recorded();
    let runtime = TestCanisterRuntime::new();

    sweep_queued_spl_deposits(runtime.clone()).await;

    assert_eq!(EventsAssert::from_recorded(), before);
    assert!(runtime.sent_update_calls().is_empty());
    assert_eq!(runtime.schnorr_public_key_call_count(), 0);
    assert_eq!(runtime.set_timer_call_count(), 0);
    assert!(TimerGuard::new(TaskType::SweepSplDeposits).is_ok());
}

#[tokio::test]
async fn should_return_early_when_spl_sweep_timer_is_active() {
    queue_deposits(2, true, false, None);
    let guard = TimerGuard::new(TaskType::SweepSplDeposits).unwrap();
    let before = EventsAssert::from_recorded();
    let runtime = TestCanisterRuntime::new();

    sweep_queued_spl_deposits(runtime.clone()).await;

    assert_eq!(EventsAssert::from_recorded(), before);
    assert!(runtime.sent_update_calls().is_empty());
    assert_eq!(runtime.schnorr_public_key_call_count(), 0);
    assert!(TimerGuard::new(TaskType::SweepSplDeposits).is_err());
    drop(guard);
    assert!(TimerGuard::new(TaskType::SweepSplDeposits).is_ok());
}

#[tokio::test]
async fn should_leave_spl_queue_unchanged_when_fetching_block_fails() {
    queue_deposits(2, true, false, None);
    let before = EventsAssert::from_recorded();
    let runtime = TestCanisterRuntime::new()
        .with_schnorr_public_key(schnorr_master_key_response())
        .add_recent_block(Err(RpcError::ValidationError(
            "block unavailable".to_string(),
        )));

    sweep_queued_spl_deposits(runtime.clone()).await;

    assert_eq!(EventsAssert::from_recorded(), before);
    assert_eq!(runtime.schnorr_public_key_call_count(), 1);
    assert_eq!(runtime.set_timer_call_count(), 0);
    assert!(TimerGuard::new(TaskType::SweepSplDeposits).is_ok());
    read_state(|state| assert_eq!(state.spl_deposits().queued().len(), 2));
}

#[tokio::test]
async fn should_sweep_spl_deposits_while_sol_sweep_timer_is_active() {
    queue_deposits(2, true, false, None);
    let sol_guard = TimerGuard::new(TaskType::SweepDeposits).unwrap();
    let sweep = queued_sweep();
    let runtime = signing_runtime()
        .add_recent_block(Ok(block().slot))
        .with_schnorr_public_key(schnorr_master_key_response())
        .add_stub_response(MultiRpcResult::<sol_rpc_types::Signature>::Consistent(Ok(
            minter_signature().into(),
        )));

    sweep_queued_spl_deposits(runtime.clone()).await;

    assert_eq!(runtime.schnorr_public_key_call_count(), 1);
    assert_eq!(runtime.set_timer_call_count(), 0);
    assert!(TimerGuard::new(TaskType::SweepDeposits).is_err());
    assert!(TimerGuard::new(TaskType::SweepSplDeposits).is_ok());
    drop(sol_guard);
    assert_submission_is_tracked(&sweep, fetched_block());
}

#[tokio::test]
async fn should_keep_spl_sweep_tracked_when_timer_submission_fails() {
    queue_deposits(2, true, false, None);
    let sweep = queued_sweep();
    let runtime = signing_runtime()
        .add_recent_block(Ok(block().slot))
        .with_schnorr_public_key(schnorr_master_key_response())
        .add_stub_error(IcError::CallPerformFailed);

    sweep_queued_spl_deposits(runtime.clone()).await;

    assert_submission_is_tracked(&sweep, fetched_block());
    assert_eq!(runtime.set_timer_call_count(), 0);
    assert!(TimerGuard::new(TaskType::SweepSplDeposits).is_ok());
}

#[tokio::test]
async fn should_leave_spl_deposits_queued_when_timer_signing_fails() {
    queue_deposits(2, true, false, None);
    let before = EventsAssert::from_recorded();
    let runtime = TestCanisterRuntime::new()
        .add_recent_block(Ok(block().slot))
        .with_schnorr_public_key(schnorr_master_key_response())
        .add_signer(sign_as_minter().expect([Err(SignCallError::CallFailed(
            CallRejected::with_rejection(4, "signing unavailable".to_string()).into(),
        ))]));

    sweep_queued_spl_deposits(runtime.clone()).await;

    assert_eq!(EventsAssert::from_recorded(), before);
    assert_eq!(runtime.set_timer_call_count(), 0);
    assert!(
        !runtime
            .sent_update_calls()
            .iter()
            .any(|call| call.method == "sendTransaction")
    );
    assert!(TimerGuard::new(TaskType::SweepSplDeposits).is_ok());
    read_state(|state| {
        assert_eq!(state.spl_deposits().queued().len(), 2);
        assert!(state.submitted_transactions().is_empty());
    });
}

#[tokio::test]
async fn should_reschedule_spl_sweep_round_until_all_deposits_are_submitted() {
    queue_deposits(100, false, true, Some(TokenProgram::Classic));
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));
    assert_eq!(round.batches.len(), MAX_CONCURRENT_RPC_CALLS);
    assert!(round.leaves_deposits_queued);
    let first_round_count = round
        .batches
        .iter()
        .map(|batch| batch.deposits().len())
        .sum::<usize>();
    let (remaining, tokens) = read_state(|state| {
        let deposits: Vec<_> = state
            .spl_deposits()
            .queued()
            .iter()
            .skip(first_round_count)
            .map(|(id, deposit)| (*id, deposit.clone()))
            .collect();
        let tokens = deposits
            .iter()
            .map(|(_, deposit)| {
                (
                    deposit.mint,
                    state.supported_spl_token(&deposit.mint).cloned().unwrap(),
                )
            })
            .collect();
        (deposits, tokens)
    });
    let first_batch_size = round.batches[0].deposits().len();
    let second_round_batches = remaining
        .chunks(first_batch_size)
        .map(|batch| SplSweep::plan(batch.iter().cloned(), &tokens, &schnorr_master_key()))
        .collect::<Vec<_>>();
    let total_batches = round.batches.len() + second_round_batches.len();
    let mut runtime = TestCanisterRuntime::new()
        .with_increasing_time()
        .with_schnorr_public_key(schnorr_master_key_response())
        .add_signer(sign_as_minter().times(total_batches));
    for id in 1..=100 {
        runtime = runtime.add_signer(sign_for(&account(id)));
    }
    runtime = runtime.add_recent_block(Ok(block().slot));
    for batch in 0..round.batches.len() {
        runtime =
            runtime.add_stub_response(MultiRpcResult::<sol_rpc_types::Signature>::Consistent(Ok(
                minter_signature_nth(batch).into(),
            )));
    }
    runtime = runtime.add_recent_block(Ok(block().slot));
    for batch in round.batches.len()..total_batches {
        runtime =
            runtime.add_stub_response(MultiRpcResult::<sol_rpc_types::Signature>::Consistent(Ok(
                minter_signature_nth(batch).into(),
            )));
    }

    sweep_queued_spl_deposits(runtime.clone()).await;

    assert_eq!(runtime.set_timer_call_count(), 1);
    read_state(|state| {
        assert_eq!(
            state.submitted_transactions().len(),
            MAX_CONCURRENT_RPC_CALLS
        );
        assert_eq!(state.spl_deposits().queued().len(), 100 - first_round_count);
    });

    sweep_queued_spl_deposits(runtime.clone()).await;

    assert_eq!(runtime.set_timer_call_count(), 1);
    assert_eq!(runtime.schnorr_public_key_call_count(), 1);
    assert!(TimerGuard::new(TaskType::SweepSplDeposits).is_ok());
    read_state(|state| {
        assert!(state.spl_deposits().queued().is_empty());
        assert_eq!(state.spl_deposits().swept().deposit_count(), 100);
        assert_eq!(state.submitted_transactions().len(), total_batches);
    });
}

fn assert_submission_is_tracked(sweep: &SplSweep, block: Block) {
    let message = sweep.sweep_message(block.blockhash);
    let expected = EventType::SubmittedTransaction {
        signature: minter_signature(),
        message: message.clone().into(),
        signers: sweep.signers(&message),
        purpose: TransactionPurpose::SweepSplDeposits {
            deposit_ids: sweep.deposits().keys().copied().collect(),
        },
        block_height: block.block_height,
    };
    assert!(EventsAssert::from_recorded().contains_event(&expected));
    read_state(|state| {
        assert!(state.spl_deposits().queued().is_empty());
        assert_eq!(
            state.spl_deposits().swept().get(&minter_signature()),
            Some(sweep)
        );
        let submitted = state
            .submitted_transactions()
            .get(&minter_signature())
            .unwrap();
        assert_eq!(submitted.message, message.into());
        assert_eq!(submitted.block_height, block.block_height);
        assert!(state.failed_transactions().is_empty());
        assert!(state.succeeded_transactions().is_empty());
        for (deposit_id, deposit) in sweep.deposits() {
            assert_eq!(
                state
                    .spl_deposits()
                    .in_flight_id(&deposit.account, &deposit.mint),
                Some(*deposit_id)
            );
        }
    });
    let mut replayed = replay_events(with_event_iter(|events| {
        events
            .map(|event| Event::from_bytes(event.to_bytes()))
            .collect::<Vec<_>>()
    }));
    // The public key is a transient cache, fetched again after upgrade.
    if let Some(key) = read_state(|state| state.minter_public_key().cloned()) {
        replayed.cache_minter_public_key(key);
    }
    read_state(|state| assert_eq!(state, &replayed));
}

#[tokio::test]
async fn should_sign_record_and_submit_spl_sweep() {
    queue_deposits(2, true, false, None);
    let sweep = queued_sweep();
    let before = EventsAssert::from_recorded().len();
    let runtime = signing_runtime().add_stub_response(
        MultiRpcResult::<sol_rpc_types::Signature>::Consistent(Ok(minter_signature().into())),
    );

    let signature = submit_spl_sweep_transaction(&runtime, sweep.clone(), block())
        .await
        .unwrap();

    assert_eq!(signature, minter_signature());
    assert_eq!(EventsAssert::from_recorded().len(), before + 1);
    assert_submission_is_tracked(&sweep, block());
    let calls = runtime.sent_update_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method, "sendTransaction");
    assert_eq!(runtime.set_timer_call_count(), 0);
}

#[tokio::test]
async fn should_keep_spl_sweep_tracked_when_rpc_submission_fails() {
    queue_deposits(2, true, false, None);
    let sweep = queued_sweep();
    let error = RpcError::ValidationError("submission failed".to_string());
    let runtime = signing_runtime().add_stub_response(
        MultiRpcResult::<sol_rpc_types::Signature>::Consistent(Err(error.clone())),
    );

    let result = submit_spl_sweep_transaction(&runtime, sweep.clone(), block()).await;

    assert_matches!(result, Err(SweepError::SubmitTransactionFailed(SubmitTransactionError::RpcError(actual))) if actual == error);
    assert_submission_is_tracked(&sweep, block());
    assert_eq!(runtime.sent_update_calls().len(), 1);
}

#[tokio::test]
async fn should_keep_spl_sweep_tracked_when_inter_canister_submission_fails() {
    queue_deposits(2, true, false, None);
    let sweep = queued_sweep();
    let runtime = signing_runtime().add_stub_error(IcError::CallPerformFailed);

    let result = submit_spl_sweep_transaction(&runtime, sweep.clone(), block()).await;

    assert_matches!(
        result,
        Err(SweepError::SubmitTransactionFailed(
            SubmitTransactionError::IcError(IcError::CallPerformFailed)
        ))
    );
    assert_submission_is_tracked(&sweep, block());
}

#[tokio::test]
async fn should_leave_spl_deposits_queued_when_signing_fails() {
    queue_deposits(2, true, false, None);
    let sweep = queued_sweep();
    let before = EventsAssert::from_recorded();
    let runtime = TestCanisterRuntime::new().add_signer(sign_as_minter().expect([Err(
        SignCallError::CallFailed(
            CallRejected::with_rejection(4, "signing service unavailable".to_string()).into(),
        ),
    )]));

    let result = submit_spl_sweep_transaction(&runtime, sweep.clone(), block()).await;

    assert_matches!(
        result,
        Err(SweepError::CreateTransactionFailed(
            CreateTransferError::SigningFailed(_)
        ))
    );
    assert_eq!(EventsAssert::from_recorded(), before);
    assert!(runtime.sent_update_calls().is_empty());
    read_state(|state| {
        assert_eq!(state.spl_deposits().queued(), sweep.deposits());
        assert!(state.spl_deposits().swept().is_empty());
        assert!(state.submitted_transactions().is_empty());
    });
}

fn queue_deposits(
    count: usize,
    same_owner: bool,
    same_mint: bool,
    token_program: Option<TokenProgram>,
) {
    let runtime = TestCanisterRuntime::new().with_increasing_time();
    lifecycle::init(valid_init_args(), runtime.clone());
    for id in 0..count {
        let mint: solana_address::Address = if same_mint {
            [1; 32]
        } else {
            [id as u8 + 1; 32]
        }
        .into();
        let token_program = token_program.unwrap_or(if id % 2 == 0 {
            TokenProgram::Classic
        } else {
            TokenProgram::Token2022
        });
        let account = account(if same_owner { 1 } else { id + 1 });
        let deposit = queued_spl_deposit(account, mint, token_program);
        mutate_state(|state| {
            if state.supported_spl_token(&mint).is_none() {
                process_event(
                    state,
                    EventType::AddedSplToken(spl_token(mint, token_program)),
                    &runtime,
                );
            }
            process_event(
                state,
                EventType::QueuedSplDeposit {
                    deposit_id: id as u64,
                    account,
                    mint,
                    address: deposit.address,
                    balance: deposit.balance,
                },
                &runtime,
            );
        });
    }
}

fn assert_batches_are_maximal(round: &SweepRound) {
    for pair in round.batches.windows(2) {
        let mut deposits = pair[0].deposits().clone();
        let (id, next) = pair[1].deposits().first_key_value().unwrap();
        deposits.insert(*id, next.clone());
        let tokens = read_state(|state| {
            deposits
                .values()
                .map(|deposit| {
                    (
                        deposit.mint,
                        state.supported_spl_token(&deposit.mint).cloned().unwrap(),
                    )
                })
                .collect()
        });
        let candidate = SplSweep::plan(deposits, &tokens, &schnorr_master_key());
        let signatures = candidate
            .sweep_message(Hash::default())
            .header
            .num_required_signatures as u64;
        assert!(wire_size(&candidate) > MAX_TX_SIZE || signatures > MAX_SIGNATURES);
    }
}

fn wire_size(sweep: &SplSweep) -> usize {
    // An independent bincode measurement, to cross-check `transaction_size`.
    bincode::serialize(&Transaction::new_unsigned(
        sweep.sweep_message(Hash::default()),
    ))
    .unwrap()
    .len()
}

#[test]
fn should_return_empty_round_when_no_deposits_are_queued() {
    queue_deposits(0, false, true, Some(TokenProgram::Classic));
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert!(round.batches.is_empty());
    assert!(!round.leaves_deposits_queued);
}

#[test]
fn should_select_one_deposit_without_changing_the_queue() {
    queue_deposits(1, false, true, Some(TokenProgram::Token2022));
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert_eq!(round.batches.len(), 1);
    assert_eq!(
        round.batches[0]
            .deposits()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![0]
    );
    assert!(wire_size(&round.batches[0]) <= MAX_TX_SIZE);
    assert!(!round.leaves_deposits_queued);
    read_state(|state| assert_eq!(state.spl_deposits().queued().len(), 1));
}

#[test]
fn should_fit_more_than_four_deposits_when_the_mint_is_shared() {
    queue_deposits(20, false, true, Some(TokenProgram::Classic));
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert!(round.batches.len() > 1);
    assert!(!round.leaves_deposits_queued);
    assert!(round.batches[0].deposits().len() > 4);
    assert_batches_are_maximal(&round);
    for batch in &round.batches {
        assert!(wire_size(batch) <= MAX_TX_SIZE);
        assert!(
            batch
                .sweep_message(Hash::default())
                .header
                .num_required_signatures as u64
                <= MAX_SIGNATURES
        );
    }
    let selected: Vec<_> = round
        .batches
        .iter()
        .flat_map(|batch| batch.deposits().keys().copied())
        .collect();
    assert_eq!(selected, (0..20).collect::<Vec<_>>());
    read_state(|state| assert_eq!(state.spl_deposits().queued().len(), 20));
}

#[test]
fn should_include_signatures_and_destinations_in_shared_owner_batches() {
    queue_deposits(20, true, false, Some(TokenProgram::Token2022));
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert!(round.batches.len() > 1);
    assert!(!round.leaves_deposits_queued);
    assert_batches_are_maximal(&round);
    for batch in &round.batches {
        let message = batch.sweep_message(Hash::default());
        assert_eq!(message.header.num_required_signatures, 2);
        assert_eq!(message.instructions.len(), batch.deposits().len() * 2);
        assert!(wire_size(batch) <= MAX_TX_SIZE);
    }
    let selected: Vec<_> = round
        .batches
        .iter()
        .flat_map(|batch| batch.deposits().keys().copied())
        .collect();
    assert_eq!(selected, (0..20).collect::<Vec<_>>());
}

#[test]
fn should_leave_deposits_queued_when_the_round_limit_is_reached() {
    let count = 100;
    queue_deposits(count, false, true, Some(TokenProgram::Classic));
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert_eq!(round.batches.len(), MAX_CONCURRENT_RPC_CALLS);
    assert!(round.leaves_deposits_queued);
    let selected: Vec<_> = round
        .batches
        .iter()
        .flat_map(|batch| batch.deposits().keys().copied())
        .collect();
    assert!(selected.len() < count);
    assert_eq!(selected, (0..selected.len() as u64).collect::<Vec<_>>());
    for batch in &round.batches {
        assert!(wire_size(batch) <= MAX_TX_SIZE);
    }
    read_state(|state| assert_eq!(state.spl_deposits().queued().len(), count));
}

#[test]
fn should_limit_distinct_owners_and_mints_by_actual_size() {
    queue_deposits(20, false, false, None);
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert_eq!(round.batches[0].deposits().len(), 4);
    assert!(!round.leaves_deposits_queued);
    assert_batches_are_maximal(&round);
    for batch in &round.batches {
        assert!(wire_size(batch) <= MAX_TX_SIZE);
    }
}

#[test]
fn should_pack_more_deposits_when_owners_and_mints_are_shared() {
    let runtime = TestCanisterRuntime::new().with_increasing_time();
    lifecycle::init(valid_init_args(), runtime.clone());
    for mint_index in 0..4 {
        let mint = [mint_index as u8 + 1; 32].into();
        let token_program = if mint_index % 2 == 0 {
            TokenProgram::Classic
        } else {
            TokenProgram::Token2022
        };
        mutate_state(|state| {
            process_event(
                state,
                EventType::AddedSplToken(spl_token(mint, token_program)),
                &runtime,
            )
        });
        for owner_index in 0..3 {
            let account = account(owner_index + 1);
            let deposit = queued_spl_deposit(account, mint, token_program);
            mutate_state(|state| {
                process_event(
                    state,
                    EventType::QueuedSplDeposit {
                        deposit_id: (mint_index * 3 + owner_index) as u64,
                        account,
                        mint,
                        address: deposit.address,
                        balance: deposit.balance,
                    },
                    &runtime,
                )
            });
        }
    }

    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert_eq!(round.batches.len(), 2);
    assert_eq!(round.batches[0].deposits().len(), 9);
    assert_eq!(round.batches[1].deposits().len(), 3);
    assert!(!round.leaves_deposits_queued);
    assert_batches_are_maximal(&round);
    for batch in &round.batches {
        assert!(wire_size(batch) <= MAX_TX_SIZE);
    }
}

#[test]
fn should_fit_a_single_deposit_in_a_sweep_of_its_own() {
    for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
        let mint = [1; 32].into();
        let tokens = BTreeMap::from([(mint, spl_token(mint, token_program))]);
        let sweep = SplSweep::plan(
            [(0, queued_spl_deposit(account(1), mint, token_program))],
            &tokens,
            &schnorr_master_key(),
        );
        let message = sweep.sweep_message(Hash::default());

        assert_eq!(transaction_size(&message), wire_size(&sweep));
        assert!(transaction_size(&message) <= MAX_TX_SIZE);
        assert!(u64::from(message.header.num_required_signatures) <= MAX_SIGNATURES);
    }
}
