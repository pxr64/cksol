use super::{operating_top_up_account, process_operating_top_up};
use crate::{
    constants::OPERATING_TOP_UP_SUBACCOUNT,
    guard::operating_top_up_guard,
    numeric::LedgerBurnIndex,
    state::{event::EventType, read_state, reset_state},
    storage::reset_events,
    test_fixtures::{
        EventsAssert, MINTER_ACCOUNT, init_state, ledger_canister_id,
        runtime::{TEST_CANISTER_ID, TestCanisterRuntime},
    },
};
use candid::Nat;
use cksol_types::{BurnMemo, Memo, OperatingTopUp, ProcessOperatingTopUpError};
use ic_canister_runtime::IcError;
use icrc_ledger_types::icrc1::{
    account::Account,
    transfer::{TransferArg, TransferError},
};

const TOP_UP: u64 = 1_000_000_000;
const BURN_BLOCK_INDEX: u64 = 42;

fn top_up_account() -> Account {
    operating_top_up_account(TEST_CANISTER_ID)
}

fn runtime_with_top_up_balance() -> TestCanisterRuntime {
    TestCanisterRuntime::new().add_stub_response(Nat::from(TOP_UP))
}

#[tokio::test]
async fn should_burn_the_top_up_balance_into_the_operating_balance() {
    init_state();
    let runtime = runtime_with_top_up_balance()
        .add_stub_response(Ok::<Nat, TransferError>(Nat::from(BURN_BLOCK_INDEX)))
        .with_increasing_time();

    assert_eq!(
        process_operating_top_up(&runtime).await,
        Ok(OperatingTopUp {
            block_index: BURN_BLOCK_INDEX,
            amount: TOP_UP,
        })
    );

    let calls = runtime.sent_update_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].canister_id, ledger_canister_id());
    assert_eq!(calls[0].method, "icrc1_balance_of");
    assert_eq!(calls[0].single_arg::<Account>(), top_up_account());
    assert_eq!(calls[1].canister_id, ledger_canister_id());
    assert_eq!(calls[1].method, "icrc1_transfer");
    assert_eq!(
        calls[1].single_arg::<TransferArg>(),
        TransferArg {
            from_subaccount: Some(OPERATING_TOP_UP_SUBACCOUNT),
            to: MINTER_ACCOUNT,
            fee: None,
            created_at_time: None,
            memo: Some(Memo::from(BurnMemo::TopUp).into()),
            amount: Nat::from(TOP_UP),
        }
    );
    EventsAssert::from_recorded()
        .expect_event_eq(EventType::ToppedUpOperatingBalance {
            burn_block_index: LedgerBurnIndex::new(BURN_BLOCK_INDEX),
            amount: TOP_UP,
        })
        .assert_no_more_events();
    read_state(|state| {
        assert_eq!(state.operating_balance(), TOP_UP);
        assert_eq!(state.balance(), 0);
    });
}

#[tokio::test]
async fn should_not_burn_an_empty_top_up_account() {
    init_state();
    let runtime = TestCanisterRuntime::new().add_stub_response(Nat::from(0_u8));

    assert_eq!(
        process_operating_top_up(&runtime).await,
        Err(ProcessOperatingTopUpError::NoFunds)
    );

    assert_eq!(runtime.sent_update_calls().len(), 1);
    EventsAssert::assert_no_events_recorded();
}

#[tokio::test]
async fn should_record_nothing_when_the_ledger_fails() {
    let balance_read_fails = TestCanisterRuntime::new().add_stub_error(IcError::CallPerformFailed);
    let burn_call_fails = runtime_with_top_up_balance().add_stub_error(IcError::CallPerformFailed);
    let burn_rejected = runtime_with_top_up_balance().add_stub_response(Err::<Nat, TransferError>(
        TransferError::TemporarilyUnavailable,
    ));
    let burn_generic_error =
        runtime_with_top_up_balance().add_stub_response(Err::<Nat, TransferError>(
            TransferError::GenericError {
                error_code: Nat::from(1_u8),
                message: "error".to_string(),
            },
        ));

    for runtime in [
        balance_read_fails,
        burn_call_fails,
        burn_rejected,
        burn_generic_error,
    ] {
        reset_state();
        reset_events();
        init_state();

        assert_matches::assert_matches!(
            process_operating_top_up(&runtime).await,
            Err(ProcessOperatingTopUpError::TemporarilyUnavailable(_))
        );

        EventsAssert::assert_no_events_recorded();
        read_state(|state| assert_eq!(state.operating_balance(), 0));
        assert!(operating_top_up_guard(top_up_account()).is_ok());
    }
}

#[tokio::test]
async fn should_reject_a_concurrent_top_up() {
    init_state();
    let _guard = operating_top_up_guard(top_up_account()).unwrap();
    let runtime = TestCanisterRuntime::new();

    assert_eq!(
        process_operating_top_up(&runtime).await,
        Err(ProcessOperatingTopUpError::AlreadyProcessing)
    );

    assert!(runtime.sent_update_calls().is_empty());
}
