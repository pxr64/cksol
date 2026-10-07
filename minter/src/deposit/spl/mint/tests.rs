use super::*;
use crate::{
    constants::{LEDGER_DEDUPLICATION_WINDOW, MAX_PENDING_MINTS_PER_ROUND},
    guard::TimerGuard,
    lifecycle,
    state::{
        TokenProgram,
        audit::{process_event, replay_events},
        event::{CreditedSplDeposit, Event, TransactionPurpose},
        mutate_state, reset_state,
    },
    storage::{reset_events, with_event_iter},
    test_fixtures::{
        BLOCK_INDEX, DEFAULT_BLOCK_HEIGHT, EventsAssert, account, queued_spl_deposit,
        runtime::TestCanisterRuntime, schnorr_master_key, signature, spl_token, valid_init_args,
    },
};
use candid::Nat;
use futures::FutureExt;
use ic_canister_runtime::IcError;
use ic_stable_structures::Storable;
use icrc_ledger_types::icrc1::transfer::{BlockIndex, TransferError};
use solana_hash::Hash;
use std::{collections::BTreeMap, panic::AssertUnwindSafe};

type MintResult = Result<BlockIndex, TransferError>;
const CREDITED_AT: u64 = 1_234;

fn setup_pending(count: usize) -> Vec<(DepositSplId, PendingSplMint)> {
    reset_state();
    reset_events();
    let runtime = TestCanisterRuntime::new().with_increasing_time();
    lifecycle::init(valid_init_args(), runtime.clone());
    for (index, program) in [TokenProgram::Classic, TokenProgram::Token2022]
        .into_iter()
        .enumerate()
    {
        mutate_state(|state| {
            process_event(
                state,
                EventType::AddedSplToken(spl_token([index as u8 + 1; 32].into(), program)),
                &runtime,
            )
        });
    }
    for id in 0..count {
        let program = if id % 2 == 0 {
            TokenProgram::Classic
        } else {
            TokenProgram::Token2022
        };
        let mint = [id as u8 % 2 + 1; 32].into();
        let token = spl_token(mint, program);
        let deposit = queued_spl_deposit(account(id / 2 + 1), mint, program);
        let sweep = crate::state::SplSweep::plan(
            [(id as u64, deposit.clone())],
            &BTreeMap::from([(mint, token)]),
            &schnorr_master_key(),
        );
        let message = sweep.sweep_message(Hash::default());
        let signature = signature(id);
        mutate_state(|state| {
            process_event(
                state,
                EventType::QueuedSplDeposit {
                    deposit_id: id as u64,
                    account: deposit.account,
                    mint,
                    address: deposit.address,
                    balance: deposit.balance,
                },
                &runtime,
            );
            process_event(
                state,
                EventType::SubmittedTransaction {
                    signature,
                    signers: sweep.signers(&message),
                    message: message.into(),
                    purpose: TransactionPurpose::SweepSplDeposits {
                        deposit_ids: vec![id as u64],
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
            process_event(
                state,
                EventType::CreditedSplSweep {
                    signature,
                    lamports_spent: 5_000,
                    mints: vec![CreditedSplDeposit {
                        deposit_id: id as u64,
                        amount_to_mint: deposit.balance,
                    }],
                },
                &TestCanisterRuntime::new().add_times([CREDITED_AT; 2]),
            );
        });
    }
    read_state(|state| {
        state
            .spl_deposits()
            .pending_mints()
            .iter()
            .map(|(id, pending)| (*id, pending.clone()))
            .collect()
    })
}

fn mint_runtime(results: impl IntoIterator<Item = MintResult>) -> TestCanisterRuntime {
    let mut runtime = TestCanisterRuntime::new().with_increasing_time();
    for result in results {
        runtime = runtime.add_stub_response(result);
    }
    runtime
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
async fn should_return_early_without_pending_spl_mints() {
    setup_pending(0);
    let before = EventsAssert::from_recorded();
    let runtime = TestCanisterRuntime::new();
    process_pending_spl_mints(runtime.clone()).await;
    assert_eq!(EventsAssert::from_recorded(), before);
    assert!(runtime.sent_update_calls().is_empty());
    assert_eq!(runtime.set_timer_call_count(), 0);
}

#[tokio::test]
async fn should_return_early_when_spl_mint_task_is_active() {
    setup_pending(1);
    let guard = TimerGuard::new(TaskType::MintSpl).unwrap();
    let runtime = TestCanisterRuntime::new();
    process_pending_spl_mints(runtime.clone()).await;
    assert!(runtime.sent_update_calls().is_empty());
    assert!(TimerGuard::new(TaskType::MintSpl).is_err());
    drop(guard);
}

#[tokio::test]
async fn should_mint_to_each_registered_token_ledger_and_replay() {
    let pending = setup_pending(2);
    let native_guard = TimerGuard::new(TaskType::Mint).unwrap();
    let runtime = mint_runtime([Ok(BLOCK_INDEX.into()), Ok((BLOCK_INDEX + 1).into())]);
    process_pending_spl_mints(runtime.clone()).await;
    drop(native_guard);
    let calls = runtime.sent_update_calls();
    assert_eq!(calls.len(), 2);
    for (index, (id, pending)) in pending.iter().enumerate() {
        let ledger_id = read_state(|state| {
            state
                .supported_spl_token(&pending.deposit.deposit.mint)
                .unwrap()
                .ledger_id
        });
        assert_eq!(calls[index].canister_id, ledger_id);
        assert_eq!(calls[index].method, "icrc1_transfer");
        let expected = TransferArg {
            from_subaccount: None,
            to: pending.account(),
            fee: None,
            created_at_time: Some(CREDITED_AT),
            memo: Some(Memo::from(MintMemo::sweep(pending.sweep_signature(), *id)).into()),
            amount: NumTokens::from(pending.amount_to_mint),
        };
        assert_eq!(calls[index].single_arg::<TransferArg>(), expected);
        read_state(|state| {
            assert!(
                state
                    .spl_deposits()
                    .in_flight_id(&pending.account(), &pending.deposit.deposit.mint)
                    .is_none()
            );
            let minted = &state.spl_deposits().minted()[id];
            assert_eq!(minted.minted_amount, pending.amount_to_mint);
            assert_eq!(*minted.mint_block_index.get(), BLOCK_INDEX + index as u64);
            assert_eq!(minted.deposit, pending.deposit);
        });
        EventsAssert::from_recorded().expect_contains_event_eq(EventType::MintedSweptSplDeposit {
            deposit_id: *id,
            mint_block_index: (BLOCK_INDEX + index as u64).into(),
        });
    }
    read_state(|state| {
        assert!(state.spl_deposits().pending_mints().is_empty());
        assert_eq!(state.balance(), 0);
    });
    assert_eq!(runtime.set_timer_call_count(), 0);
    assert_replay_matches();
}

#[tokio::test]
async fn should_accept_duplicate_reply_as_minted() {
    setup_pending(1);
    process_pending_spl_mints(mint_runtime([Err(TransferError::Duplicate {
        duplicate_of: BLOCK_INDEX.into(),
    })]))
    .await;
    read_state(|state| {
        assert_eq!(
            *state.spl_deposits().minted()[&0].mint_block_index.get(),
            BLOCK_INDEX
        )
    });
    assert_replay_matches();
}

#[tokio::test]
async fn should_release_only_the_minted_account_and_token_pair() {
    let mut pending = setup_pending(2);
    let (id, first) = pending.remove(0);
    let runtime = mint_runtime([Ok(BLOCK_INDEX.into())]);
    process_pending_spl_mint(&runtime, id, first.clone()).await;
    let (_, second) = &pending[0];
    read_state(|state| {
        assert!(
            state
                .spl_deposits()
                .in_flight_id(&first.account(), &first.deposit.deposit.mint)
                .is_none()
        );
        assert_eq!(
            state
                .spl_deposits()
                .in_flight_id(&second.account(), &second.deposit.deposit.mint),
            Some(1)
        );
    });
    let deposit = first.deposit.deposit;
    mutate_state(|state| {
        process_event(
            state,
            EventType::QueuedSplDeposit {
                deposit_id: 2,
                account: deposit.account,
                mint: deposit.mint,
                address: deposit.address,
                balance: deposit.balance,
            },
            &runtime,
        )
    });
    assert_replay_matches();
}

#[tokio::test]
async fn should_retry_transient_failures_with_identical_transfer_arguments() {
    for failure in 0..4 {
        setup_pending(1);
        let before = EventsAssert::from_recorded();
        let runtime = match failure {
            0 => mint_runtime([Err(TransferError::TemporarilyUnavailable)]),
            1 => mint_runtime([Err(TransferError::GenericError {
                error_code: Nat::from(42u8),
                message: "retry".to_string(),
            })]),
            2 => mint_runtime([Err(TransferError::CreatedInFuture { ledger_time: 0 })]),
            _ => TestCanisterRuntime::new()
                .with_increasing_time()
                .add_stub_error(IcError::CallPerformFailed),
        };
        process_pending_spl_mints(runtime.clone()).await;
        assert_eq!(EventsAssert::from_recorded(), before);
        assert_eq!(runtime.set_timer_call_count(), 0);
        let retry = mint_runtime([Ok(BLOCK_INDEX.into())]);
        process_pending_spl_mints(retry.clone()).await;
        let initial = runtime.sent_update_calls();
        let retries = retry.sent_update_calls();
        assert_eq!(initial[0].canister_id, retries[0].canister_id);
        assert_eq!(
            initial[0].single_arg::<TransferArg>(),
            retries[0].single_arg::<TransferArg>()
        );
        assert_replay_matches();
    }
}

#[tokio::test]
async fn should_quarantine_pending_spl_mint_when_retry_is_unsafe() {
    for failure in 0..6 {
        let pending = setup_pending(1).remove(0).1;
        let runtime = match failure {
            0 => TestCanisterRuntime::new()
                .add_times([CREDITED_AT + LEDGER_DEDUPLICATION_WINDOW.as_nanos() as u64 + 1; 3]),
            1 => mint_runtime([Err(TransferError::TooOld)]),
            2 => mint_runtime([Err(TransferError::BadFee {
                expected_fee: 1u8.into(),
            })]),
            3 => mint_runtime([Err(TransferError::BadBurn {
                min_burn_amount: 1u8.into(),
            })]),
            4 => mint_runtime([Err(TransferError::InsufficientFunds {
                balance: 0u8.into(),
            })]),
            _ => mint_runtime([Ok(Nat::from(u128::MAX))]),
        };
        let result = AssertUnwindSafe(process_pending_spl_mints(runtime.clone()))
            .catch_unwind()
            .await;
        assert_eq!(result.is_err(), failure == 5);
        assert_eq!(runtime.sent_update_calls().len(), usize::from(failure != 0));
        read_state(|state| {
            assert!(state.spl_deposits().pending_mints().is_empty());
            assert!(state.spl_deposits().minted().is_empty());
            assert_eq!(state.spl_deposits().quarantined()[&0], pending.deposit);
            assert_eq!(
                state
                    .spl_deposits()
                    .in_flight_id(&pending.account(), &pending.deposit.deposit.mint),
                Some(0)
            );
        });
        assert_replay_matches();
    }
}

#[tokio::test]
async fn should_reschedule_until_all_pending_spl_mints_are_processed() {
    setup_pending(MAX_PENDING_MINTS_PER_ROUND + 1);
    let first =
        mint_runtime((0..MAX_PENDING_MINTS_PER_ROUND).map(|index| Ok((index as u64).into())));
    process_pending_spl_mints(first.clone()).await;
    assert_eq!(first.set_timer_call_count(), 1);
    read_state(|state| assert_eq!(state.spl_deposits().pending_mints().len(), 1));
    let second = mint_runtime([Ok(BLOCK_INDEX.into())]);
    process_pending_spl_mints(second.clone()).await;
    assert_eq!(second.set_timer_call_count(), 0);
    read_state(|state| {
        assert!(state.spl_deposits().pending_mints().is_empty());
        assert_eq!(
            state.spl_deposits().minted().len(),
            MAX_PENDING_MINTS_PER_ROUND + 1
        );
    });
    assert_replay_matches();
}

#[tokio::test]
async fn should_not_reschedule_a_round_of_transient_spl_mint_failures() {
    setup_pending(MAX_PENDING_MINTS_PER_ROUND + 1);
    let runtime = mint_runtime(
        (0..MAX_PENDING_MINTS_PER_ROUND).map(|_| Err(TransferError::TemporarilyUnavailable)),
    );
    process_pending_spl_mints(runtime.clone()).await;
    assert_eq!(runtime.set_timer_call_count(), 0);
    assert_eq!(
        runtime.sent_update_calls().len(),
        MAX_PENDING_MINTS_PER_ROUND
    );
    read_state(|state| {
        assert_eq!(
            state.spl_deposits().pending_mints().len(),
            MAX_PENDING_MINTS_PER_ROUND + 1
        )
    });
}
