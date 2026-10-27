use super::*;
use crate::{
    guard::withdrawal_guard,
    state::{TokenProgram, audit::process_event, event::EventType, mutate_state},
    test_fixtures::{
        EventsAssert, MINTER_ADDRESS, NONCE_ACCOUNT, account, init_schnorr_master_key, init_state,
        ledger_canister_id, runtime::TestCanisterRuntime, spl_token,
    },
};
use candid::Nat;
use ic_canister_runtime::IcError;
use icrc_ledger_types::{
    icrc1::account::Account,
    icrc2::transfer_from::{TransferFromArgs, TransferFromError},
};

fn register_token(token_program: TokenProgram) -> SupportedSplToken {
    let mut token = spl_token([3; 32].into(), token_program);
    token.minimum_deposit_amount = 1_000;
    mutate_state(|state| {
        process_event(
            state,
            EventType::AddedSplToken(token.clone()),
            &TestCanisterRuntime::new().with_time(1),
        )
    });
    token
}

fn assert_only_registration(token: SupportedSplToken) {
    EventsAssert::from_recorded()
        .expect_event_eq(EventType::AddedSplToken(token))
        .assert_no_more_events();
}

fn withdrawal_terms(token: &SupportedSplToken) -> SplWithdrawalRequestTerms {
    SplWithdrawalRequestTerms {
        account: Account {
            subaccount: Some([7; 32]),
            ..account(1)
        },
        mint: token.mint,
        ledger_id: token.ledger_id,
        address: "E4MpwNnMWs2XtW5gVrxZvyS7fMq31QD5HvbxmwP45Tz3"
            .parse()
            .unwrap(),
        amount: u64::MAX,
        fee: quote_withdrawal_fee(
            read_state(|state| state.withdrawal_fee()),
            token.token_program,
            true,
        )
        .unwrap(),
        create_destination_account: true,
    }
}

#[test]
fn should_reject_a_zero_token_amount() {
    init_state();
    let token = register_token(TokenProgram::Classic);

    assert_eq!(
        validate_token(token.mint, 0),
        Err(WithdrawSplError::ValueTooSmall {
            minimum_withdrawal_amount: 1,
            withdrawal_amount: 0,
        })
    );
    assert_only_registration(token);
}

#[test]
fn should_reject_an_unregistered_mint() {
    init_state();
    let mint = [4; 32].into();

    assert_eq!(
        validate_token(mint, 1),
        Err(WithdrawSplError::TokenNotSupported { mint })
    );
    EventsAssert::assert_no_events_recorded();
}

#[test]
fn should_accept_a_positive_amount_below_the_deposit_minimum() {
    init_state();
    let token = register_token(TokenProgram::Classic);
    assert!(token.minimum_deposit_amount > 1);

    assert_eq!(validate_token(token.mint, 1), Ok(token.clone()));
    assert_only_registration(token);
}

#[test]
fn should_select_the_registered_token_2022_ledger() {
    init_state();
    let token = register_token(TokenProgram::Token2022);

    assert_eq!(validate_token(token.mint, u64::MAX), Ok(token.clone()));
    assert_only_registration(token);
}

#[test]
fn should_report_a_shared_withdrawal_guard_conflict() {
    init_state();
    let from = account(1);
    let guard = withdrawal_guard(from).unwrap();

    assert_eq!(
        withdrawal_guard(from).map_err(WithdrawSplError::from).err(),
        Some(WithdrawSplError::AlreadyProcessing)
    );
    assert!(withdrawal_guard(account(2)).is_ok());

    drop(guard);
    assert!(withdrawal_guard(from).is_ok());
    EventsAssert::assert_no_events_recorded();
}

#[test]
fn should_reject_a_malformed_destination() {
    init_state();
    let token = spl_token([3; 32].into(), TokenProgram::Classic);

    for address in ["", "not-a-solana-address", "111"] {
        assert!(matches!(
            validate_destination(&token, address),
            Err(WithdrawSplError::MalformedAddress(_))
        ));
    }
    EventsAssert::assert_no_events_recorded();
}

#[test]
fn should_reject_reserved_minter_nonce_and_treasury_destinations() {
    init_state();
    init_schnorr_master_key();

    for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
        let token = spl_token([3; 32].into(), token_program);
        let treasury = associated_token_address(&MINTER_ADDRESS, &token.mint, &token_program.id());
        for address in [
            "11111111111111111111111111111111".to_string(),
            "SysvarC1ock11111111111111111111111111111111".to_string(),
            MINTER_ADDRESS.to_string(),
            NONCE_ACCOUNT.to_string(),
            treasury.to_string(),
        ] {
            assert!(
                matches!(
                    validate_destination(&token, &address),
                    Err(WithdrawSplError::InvalidDestination(_))
                ),
                "{address}"
            );
        }
    }
    EventsAssert::assert_no_events_recorded();
}

#[test]
fn should_return_the_parsed_destination_wallet() {
    init_state();
    init_schnorr_master_key();
    let destination = Address::from_str("E4MpwNnMWs2XtW5gVrxZvyS7fMq31QD5HvbxmwP45Tz3").unwrap();

    for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
        let token = spl_token([3; 32].into(), token_program);
        assert_eq!(
            validate_destination(&token, &destination.to_string()),
            Ok(destination)
        );
    }
    EventsAssert::assert_no_events_recorded();
}

#[test]
fn should_be_temporarily_unavailable_without_the_minter_public_key() {
    init_state();
    let token = spl_token([3; 32].into(), TokenProgram::Classic);

    assert!(matches!(
        validate_destination(&token, "E4MpwNnMWs2XtW5gVrxZvyS7fMq31QD5HvbxmwP45Tz3"),
        Err(WithdrawSplError::TemporarilyUnavailable(_))
    ));
    EventsAssert::assert_no_events_recorded();
}

#[test]
fn should_quote_only_the_configured_fee_when_creation_is_not_allowed() {
    for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
        for fee in [12_345, 1_000_000, u64::MAX] {
            assert_eq!(quote_withdrawal_fee(fee, token_program, false), Ok(fee));
        }
    }
}

#[test]
fn should_include_the_token_programs_ata_rent_when_creation_is_allowed() {
    assert_eq!(
        quote_withdrawal_fee(1_000_000, TokenProgram::Classic, true),
        Ok(3_039_280)
    );
    assert_eq!(
        quote_withdrawal_fee(1_000_000, TokenProgram::Token2022, true),
        Ok(3_074_080)
    );
    assert_eq!(
        quote_withdrawal_fee(2_000_000, TokenProgram::Classic, true),
        Ok(4_039_280)
    );
}

#[test]
fn should_accept_the_largest_fee_that_can_include_rent() {
    for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
        let fee = u64::MAX - token_program.account_rent_exemption();
        assert_eq!(quote_withdrawal_fee(fee, token_program, true), Ok(u64::MAX));
    }
}

#[test]
fn should_reject_a_fee_that_overflows_when_rent_is_added() {
    for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
        let fee = u64::MAX - token_program.account_rent_exemption() + 1;
        assert!(matches!(
            quote_withdrawal_fee(fee, token_program, true),
            Err(WithdrawSplError::TemporarilyUnavailable(_))
        ));
    }
}

#[tokio::test]
async fn should_record_the_fee_checkpoint_after_the_cksol_burn() {
    init_state();
    init_schnorr_master_key();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let _guard = withdrawal_guard(request.account).unwrap();
    let runtime = TestCanisterRuntime::new()
        .add_stub_response(Ok::<Nat, TransferFromError>(42_u64.into()))
        .with_time(123);

    assert_eq!(burn_withdrawal_fee(&runtime, &request).await, Ok(42.into()));

    let calls = runtime.sent_update_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].canister_id, ledger_canister_id());
    assert_eq!(calls[0].method, "icrc2_transfer_from");
    assert_eq!(
        calls[0].single_arg::<TransferFromArgs>(),
        TransferFromArgs {
            spender_subaccount: None,
            from: request.account,
            to: runtime.canister_self().into(),
            amount: request.fee.into(),
            fee: None,
            memo: Some(
                Memo::spl_withdrawal_fee(request.mint, request.amount, request.address).into()
            ),
            created_at_time: None,
        }
    );
    EventsAssert::from_recorded()
        .expect_event_eq(EventType::AddedSplToken(token))
        .expect_event_eq(EventType::BurnedCkSolWithdrawalFee {
            cksol_burn_block_index: 42.into(),
            request: request.clone(),
        })
        .assert_no_more_events();
    read_state(|state| {
        assert_eq!(state.fee_burned_spl_withdrawal_requests().len(), 1);
        assert_eq!(
            state.fee_burned_spl_withdrawal_requests()[&42.into()],
            crate::state::FeeBurnedSplWithdrawalRequest {
                request: request.clone(),
                created_at: 123,
            }
        );
        assert!(state.pending_spl_withdrawal_requests().is_empty());
        assert!(state.pending_spl_withdrawal_fee_reimbursements().is_empty());
    });
    // The caller retains the guard for the next token burn.
    assert_eq!(
        withdrawal_guard(request.account).err(),
        Some(GuardError::AlreadyProcessing)
    );
}

#[tokio::test]
async fn should_not_record_a_fee_checkpoint_when_the_ledger_rejects_the_burn() {
    init_state();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let _guard = withdrawal_guard(request.account).unwrap();
    for (error, expected) in [
        (
            TransferFromError::InsufficientFunds {
                balance: 10_u64.into(),
            },
            BurnError::InsufficientFunds { balance: 10 },
        ),
        (
            TransferFromError::InsufficientAllowance {
                allowance: 20_u64.into(),
            },
            BurnError::InsufficientAllowance { allowance: 20 },
        ),
        (
            TransferFromError::TemporarilyUnavailable,
            BurnError::TemporarilyUnavailable("Ledger is temporarily unavailable".to_string()),
        ),
    ] {
        let runtime = TestCanisterRuntime::new().add_stub_response(Err::<Nat, _>(error));

        assert_eq!(burn_withdrawal_fee(&runtime, &request).await, Err(expected));

        assert_eq!(runtime.sent_update_calls().len(), 1);
        assert_only_registration(token.clone());
        read_state(|state| {
            assert!(state.fee_burned_spl_withdrawal_requests().is_empty());
            assert!(state.pending_spl_withdrawal_requests().is_empty());
            assert!(state.pending_spl_withdrawal_fee_reimbursements().is_empty());
        });
    }
}

#[tokio::test]
async fn should_not_record_a_fee_checkpoint_when_the_ledger_call_fails() {
    init_state();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let _guard = withdrawal_guard(request.account).unwrap();
    let runtime = TestCanisterRuntime::new().add_stub_error(IcError::CallPerformFailed);

    assert!(matches!(burn_withdrawal_fee(&runtime, &request).await,
        Err(BurnError::TemporarilyUnavailable(message)) if message.contains("Failed to burn tokens")));

    assert_eq!(runtime.sent_update_calls().len(), 1);
    assert_only_registration(token);
    read_state(|state| {
        assert!(state.fee_burned_spl_withdrawal_requests().is_empty());
        assert!(state.pending_spl_withdrawal_requests().is_empty());
        assert!(state.pending_spl_withdrawal_fee_reimbursements().is_empty());
    });
}
