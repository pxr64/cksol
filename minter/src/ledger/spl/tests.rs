use super::*;
use crate::test_fixtures::runtime::TestCanisterRuntime;
use candid::Nat;

const LEDGER_ID: Principal = Principal::from_slice(&[1; 10]);
const CREATED_AT_TIME: u64 = 123_456;
const AMOUNT: u64 = 1_000_000;

fn minting_account() -> Account {
    Account {
        owner: Principal::from_slice(&[2; 10]),
        subaccount: Some([3; 32]),
    }
}

fn from_account() -> Account {
    Account {
        owner: Principal::from_slice(&[4; 10]),
        subaccount: Some([5; 32]),
    }
}

fn burn_memo() -> BurnMemo {
    BurnMemo::convert(solana_address::Address::from([6; 32]))
}

async fn burn_tokens(runtime: &TestCanisterRuntime) -> Result<u64, BurnError> {
    burn(
        runtime,
        LEDGER_ID,
        minting_account(),
        from_account(),
        AMOUNT,
        burn_memo(),
        CREATED_AT_TIME,
    )
    .await
}

#[tokio::test]
async fn should_burn_on_token_ledger_with_fixed_arguments() {
    let runtime = TestCanisterRuntime::new()
        .add_stub_response(Ok::<Nat, TransferFromError>(Nat::from(42_u64)));

    assert_eq!(burn_tokens(&runtime).await, Ok(42));

    let calls = runtime.sent_update_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].canister_id, LEDGER_ID);
    assert_eq!(calls[0].method, "icrc2_transfer_from");
    assert_eq!(
        calls[0].single_arg::<TransferFromArgs>(),
        TransferFromArgs {
            spender_subaccount: None,
            from: from_account(),
            to: minting_account(),
            amount: AMOUNT.into(),
            fee: None,
            memo: Some(Memo::from(burn_memo()).into()),
            created_at_time: Some(CREATED_AT_TIME),
        }
    );
}

#[tokio::test]
async fn should_recover_original_burn_after_unknown_call_outcome() {
    let error = IcError::CandidDecodeFailed {
        message: "invalid ledger reply".to_string(),
    };
    let runtime = TestCanisterRuntime::new()
        .add_stub_error(error.clone())
        .add_stub_response(Err::<Nat, _>(TransferFromError::Duplicate {
            duplicate_of: 42_u64.into(),
        }));

    assert_eq!(burn_tokens(&runtime).await, Err(BurnError::Call(error)));
    assert_eq!(burn_tokens(&runtime).await, Ok(42));

    let calls = runtime.sent_update_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].canister_id, calls[1].canister_id);
    assert_eq!(calls[0].method, calls[1].method);
    assert_eq!(
        calls[0].single_arg::<TransferFromArgs>(),
        calls[1].single_arg::<TransferFromArgs>()
    );
}

#[tokio::test]
async fn should_preserve_ledger_errors_for_recovery() {
    for error in [
        TransferFromError::InsufficientFunds {
            balance: 10_u64.into(),
        },
        TransferFromError::InsufficientAllowance {
            allowance: 20_u64.into(),
        },
        TransferFromError::BadFee {
            expected_fee: 30_u64.into(),
        },
        TransferFromError::BadBurn {
            min_burn_amount: 40_u64.into(),
        },
        TransferFromError::TooOld,
        TransferFromError::CreatedInFuture { ledger_time: 50 },
        TransferFromError::TemporarilyUnavailable,
        TransferFromError::GenericError {
            error_code: 60_u64.into(),
            message: "ledger error".to_string(),
        },
    ] {
        let runtime = TestCanisterRuntime::new().add_stub_response(Err::<Nat, _>(error.clone()));

        assert_eq!(
            burn_tokens(&runtime).await,
            Err(BurnError::TransferFrom(error))
        );
        assert_eq!(runtime.sent_update_calls().len(), 1);
    }
}

#[tokio::test]
async fn should_keep_too_old_after_unknown_outcome_as_an_error() {
    let runtime = TestCanisterRuntime::new()
        .add_stub_error(IcError::CandidDecodeFailed {
            message: "invalid ledger reply".to_string(),
        })
        .add_stub_response(Err::<Nat, _>(TransferFromError::TooOld));

    assert!(matches!(
        burn_tokens(&runtime).await,
        Err(BurnError::Call(_))
    ));
    assert_eq!(
        burn_tokens(&runtime).await,
        Err(BurnError::TransferFrom(TransferFromError::TooOld))
    );
    let calls = runtime.sent_update_calls();
    assert_eq!(
        calls[0].single_arg::<TransferFromArgs>(),
        calls[1].single_arg::<TransferFromArgs>()
    );
}

#[tokio::test]
async fn should_read_token_ledger_minting_account() {
    for account in [Some(minting_account()), None] {
        let runtime = TestCanisterRuntime::new().add_stub_response(account);
        let client = LedgerClient::new(runtime.inter_canister_call_runtime(), LEDGER_ID);

        assert_eq!(client.minting_account().await, Ok(account));

        let calls = runtime.sent_update_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].canister_id, LEDGER_ID);
        assert_eq!(calls[0].method, "icrc1_minting_account");
        calls[0].args::<()>();
    }
}

#[tokio::test]
async fn should_propagate_minting_account_call_error() {
    let runtime = TestCanisterRuntime::new().add_stub_error(IcError::CallPerformFailed);
    let client = LedgerClient::new(runtime.inter_canister_call_runtime(), LEDGER_ID);

    assert_eq!(
        client.minting_account().await,
        Err(IcError::CallPerformFailed)
    );
    assert_eq!(runtime.sent_update_calls().len(), 1);
}
