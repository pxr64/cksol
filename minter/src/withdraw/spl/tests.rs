use super::*;
use crate::{
    guard::withdrawal_guard,
    state::{TokenProgram, audit::process_event, event::EventType, mutate_state},
    test_fixtures::{
        EventsAssert, MINTER_ADDRESS, NONCE_ACCOUNT, account, init_schnorr_master_key, init_state,
        runtime::TestCanisterRuntime, spl_token,
    },
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
