use super::*;
use crate::{
    lifecycle,
    monitor::finalize_transactions,
    state::{
        TokenProgram,
        audit::{process_event, replay_events},
        event::{CreditedSplDeposit, Event, TransactionPurpose},
        mutate_state, read_state, reset_state,
    },
    storage::{reset_events, with_event_iter},
    test_fixtures::{
        DEFAULT_BLOCK_HEIGHT, EventsAssert, GetTransactionResult, account, queued_spl_deposit,
        runtime::TestCanisterRuntime, schnorr_master_key, signature, spl_sweep::balanced_outcome,
        spl_token, valid_init_args,
    },
};
use ic_stable_structures::Storable;
use solana_hash::Hash;
use solana_signature::Signature;
use solana_transaction_status_client_types::{
    EncodedConfirmedTransactionWithStatusMeta, option_serializer::OptionSerializer,
};
use std::collections::BTreeMap;

const CREDIT_TIMESTAMP: u64 = 1_234_000_000;

fn setup() {
    reset_state();
    reset_events();
    let runtime = TestCanisterRuntime::new().with_increasing_time();
    lifecycle::init(valid_init_args(), runtime.clone());
    for program in [TokenProgram::Classic, TokenProgram::Token2022] {
        let mint = if program == TokenProgram::Classic {
            [1; 32]
        } else {
            [2; 32]
        };
        mutate_state(|state| {
            process_event(
                state,
                EventType::AddedSplToken(spl_token(mint.into(), program)),
                &runtime,
            )
        });
    }
}

fn finalize_sweep(index: usize) -> (Signature, SplSweep) {
    let runtime = TestCanisterRuntime::new().with_increasing_time();
    let mut deposits = vec![];
    let mut tokens = BTreeMap::new();
    for (offset, program) in [TokenProgram::Classic, TokenProgram::Token2022]
        .into_iter()
        .enumerate()
    {
        let mint = if program == TokenProgram::Classic {
            [1; 32]
        } else {
            [2; 32]
        };
        let token = spl_token(mint.into(), program);
        let deposit = queued_spl_deposit(account(index + 1), token.mint, program);
        let deposit_id = (index * 2 + offset) as u64;
        mutate_state(|state| {
            process_event(
                state,
                EventType::QueuedSplDeposit {
                    deposit_id,
                    account: deposit.account,
                    mint: deposit.mint,
                    address: deposit.address,
                    balance: deposit.balance,
                },
                &runtime,
            )
        });
        tokens.insert(token.mint, token);
        deposits.push((deposit_id, deposit));
    }
    let sweep = SplSweep::plan(deposits, &tokens, &schnorr_master_key());
    let message = sweep.sweep_message(Hash::default());
    let signature = signature(index);
    mutate_state(|state| {
        process_event(
            state,
            EventType::SubmittedTransaction {
                signature,
                signers: sweep.signers(&message),
                message: message.into(),
                purpose: TransactionPurpose::SweepSplDeposits {
                    deposit_ids: sweep.deposits().keys().copied().collect(),
                },
                block_height: DEFAULT_BLOCK_HEIGHT,
            },
            &runtime,
        );
        process_event(
            state,
            EventType::SucceededTransaction { signature },
            &runtime,
        );
    });
    (signature, sweep)
}

fn response(outcome: EncodedConfirmedTransactionWithStatusMeta) -> GetTransactionResult {
    GetTransactionResult::Consistent(Ok(Some(outcome.try_into().unwrap())))
}

fn runtime() -> TestCanisterRuntime {
    TestCanisterRuntime::new().add_times([CREDIT_TIMESTAMP; 2])
}

fn assert_replay_matches() {
    let replayed = replay_events(with_event_iter(|events| {
        events
            .map(|event| Event::from_bytes(event.to_bytes()))
            .collect::<Vec<_>>()
    }));
    read_state(|state| assert_eq!(state, &replayed));
}

#[tokio::test]
async fn should_do_nothing_without_finalized_spl_sweeps() {
    setup();
    let before = EventsAssert::from_recorded();
    assert!(!credit_finalized_spl_sweeps(&TestCanisterRuntime::new()).await);
    assert_eq!(EventsAssert::from_recorded(), before);
}

#[tokio::test]
async fn should_credit_and_replay_finalized_spl_sweep() {
    setup();
    let (signature, sweep) = finalize_sweep(0);
    let runtime =
        runtime().add_stub_response(response(balanced_outcome(&sweep, Hash::default(), true)));
    assert!(!credit_finalized_spl_sweeps(&runtime).await);
    EventsAssert::from_recorded().expect_contains_event_eq(EventType::CreditedSplSweep {
        signature,
        lamports_spent: 5_000,
        mints: sweep
            .deposits()
            .iter()
            .map(|(id, deposit)| CreditedSplDeposit {
                deposit_id: *id,
                amount_to_mint: deposit.balance,
            })
            .collect(),
    });
    read_state(|state| {
        assert!(state.spl_deposits().finalized().is_empty());
        assert_eq!(state.spl_deposits().pending_mints().len(), 2);
        assert!(state.spl_deposits().quarantined().is_empty());
        assert_eq!(state.balance(), 0);
        for (id, deposit) in sweep.deposits() {
            let pending = &state.spl_deposits().pending_mints()[id];
            assert_eq!(pending.amount_to_mint, deposit.balance);
            assert_eq!(pending.created_at_time, CREDIT_TIMESTAMP);
            assert_eq!(
                state
                    .spl_deposits()
                    .in_flight_id(&deposit.account, &deposit.mint),
                Some(*id)
            );
        }
    });
    assert_replay_matches();
    let before = EventsAssert::from_recorded();
    assert!(!credit_finalized_spl_sweeps(&TestCanisterRuntime::new()).await);
    assert_eq!(EventsAssert::from_recorded(), before);
}

#[tokio::test]
async fn should_retry_unavailable_or_unreadable_spl_outcomes_later() {
    for deviation in 0..4 {
        setup();
        let (_, sweep) = finalize_sweep(0);
        let result = match deviation {
            0 => GetTransactionResult::Consistent(Ok(None)),
            1 => GetTransactionResult::Inconsistent(vec![]),
            _ => {
                let mut outcome = balanced_outcome(&sweep, Hash::default(), false);
                if deviation == 2 {
                    outcome.transaction.meta = None;
                } else {
                    outcome
                        .transaction
                        .meta
                        .as_mut()
                        .unwrap()
                        .post_token_balances = OptionSerializer::None;
                }
                response(outcome)
            }
        };
        let before = EventsAssert::from_recorded();
        assert!(!credit_finalized_spl_sweeps(&runtime().add_stub_response(result)).await);
        assert_eq!(EventsAssert::from_recorded(), before);
        read_state(|state| {
            assert_eq!(state.spl_deposits().finalized().len(), 1);
            assert!(state.spl_deposits().pending_mints().is_empty());
            assert!(state.spl_deposits().quarantined().is_empty());
        });
    }
}

#[tokio::test]
async fn should_quarantine_and_replay_spl_outcome_mismatch() {
    setup();
    let (signature, sweep) = finalize_sweep(0);
    let mut outcome = balanced_outcome(&sweep, Hash::default(), false);
    let OptionSerializer::Some(balances) = &mut outcome
        .transaction
        .meta
        .as_mut()
        .unwrap()
        .post_token_balances
    else {
        unreachable!()
    };
    balances[0].ui_token_amount.amount = "1".to_string();
    assert!(!credit_finalized_spl_sweeps(&runtime().add_stub_response(response(outcome))).await);
    EventsAssert::from_recorded()
        .expect_contains_event_eq(EventType::QuarantinedSplSweep { signature });
    read_state(|state| {
        assert!(state.spl_deposits().finalized().is_empty());
        assert!(state.spl_deposits().pending_mints().is_empty());
        assert_eq!(state.spl_deposits().quarantined().len(), 2);
        for (id, deposit) in sweep.deposits() {
            assert_eq!(state.spl_deposits().quarantined()[id].signature, signature);
            assert_eq!(
                state
                    .spl_deposits()
                    .in_flight_id(&deposit.account, &deposit.mint),
                Some(*id)
            );
        }
    });
    assert_replay_matches();
}

#[tokio::test]
async fn should_request_another_round_only_after_progress_with_sweeps_left() {
    for progress in [false, true] {
        setup();
        for index in 0..=MAX_CONCURRENT_RPC_CALLS {
            finalize_sweep(index);
        }
        let round = read_state(|state| {
            state
                .spl_deposits()
                .finalized()
                .iter()
                .take(MAX_CONCURRENT_RPC_CALLS)
                .map(|(_, sweep)| sweep.clone())
                .collect::<Vec<_>>()
        });
        let mut runtime = runtime();
        for (index, sweep) in round.iter().enumerate() {
            runtime = runtime.add_stub_response(if progress && index == 0 {
                response(balanced_outcome(sweep, Hash::default(), false))
            } else {
                GetTransactionResult::Consistent(Ok(None))
            });
        }
        assert_eq!(credit_finalized_spl_sweeps(&runtime).await, progress);
        read_state(|state| {
            assert_eq!(
                state.spl_deposits().finalized().len(),
                MAX_CONCURRENT_RPC_CALLS + 1 - usize::from(progress)
            )
        });
    }
}

#[tokio::test]
async fn should_process_spl_sweeps_in_shared_finalization_timer() {
    setup();
    let (_, sweep) = finalize_sweep(0);
    let runtime =
        runtime().add_stub_response(response(balanced_outcome(&sweep, Hash::default(), false)));
    finalize_transactions(runtime.clone()).await;
    assert_eq!(runtime.set_timer_call_count(), 0);
    read_state(|state| assert_eq!(state.spl_deposits().pending_mints().len(), 2));
    assert_replay_matches();
}

#[tokio::test]
async fn should_reschedule_shared_finalizer_after_spl_progress() {
    setup();
    for index in 0..=MAX_CONCURRENT_RPC_CALLS {
        finalize_sweep(index);
    }
    let sweep = read_state(|state| {
        state
            .spl_deposits()
            .finalized()
            .iter()
            .next()
            .unwrap()
            .1
            .clone()
    });
    let mut runtime =
        runtime().add_stub_response(response(balanced_outcome(&sweep, Hash::default(), false)));
    for _ in 1..MAX_CONCURRENT_RPC_CALLS {
        runtime = runtime.add_stub_response(GetTransactionResult::Consistent(Ok(None)));
    }
    finalize_transactions(runtime.clone()).await;
    assert_eq!(runtime.set_timer_call_count(), 1);
    read_state(|state| {
        assert_eq!(state.spl_deposits().pending_mints().len(), 2);
        assert_eq!(
            state.spl_deposits().finalized().len(),
            MAX_CONCURRENT_RPC_CALLS
        );
    });
}
