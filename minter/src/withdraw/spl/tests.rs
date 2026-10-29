use super::*;
use crate::{
    constants::LEDGER_DEDUPLICATION_WINDOW,
    guard::{TimerGuard, withdrawal_guard},
    state::{
        TaskType, TokenProgram, audit::process_event, event::EventType, mutate_state, reset_state,
    },
    storage::reset_events,
    test_fixtures::{
        EventsAssert, MINTER_ADDRESS, NONCE_ACCOUNT, account, init_balance_to,
        init_schnorr_master_key, init_state, ledger_canister_id, minter_signature,
        runtime::TestCanisterRuntime, signer::sign_as_minter, spl_token,
    },
};
use candid::Nat;
use ic_canister_runtime::IcError;
use ic_cdk::call::CallRejected;
use ic_cdk_management_canister::SignCallError;
use icrc_ledger_types::{
    icrc1::{account::Account, transfer::TransferError},
    icrc2::transfer_from::{TransferFromArgs, TransferFromError},
};
use solana_hash::Hash;
use std::panic::{AssertUnwindSafe, catch_unwind};

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

#[tokio::test]
async fn should_retry_signing_the_recorded_spl_message_then_record_submission() {
    for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
        for create_destination_account in [false, true] {
            reset_state();
            reset_events();
            init_state();
            init_balance_to(500_000_000);
            let token = register_token(token_program);
            let mut terms = withdrawal_terms(&token);
            terms.create_destination_account = create_destination_account;
            let runtime = TestCanisterRuntime::new().with_time(123);
            mutate_state(|state| {
                process_event(
                    state,
                    EventType::AcceptedSplWithdrawalRequest(SplWithdrawalRequest {
                        terms,
                        cksol_burn_block_index: 42.into(),
                        ckspl_burn_block_index: 7.into(),
                    }),
                    &runtime,
                );
                process_event(
                    state,
                    EventType::CreatedSplWithdrawalTransaction {
                        cksol_burn_block_index: 42.into(),
                        nonce_account: NONCE_ACCOUNT,
                        nonce_value: Hash::new_from_array([18; 32]),
                    },
                    &runtime,
                );
            });
            let bound_state = read_state(Clone::clone);
            let message = bound_state
                .bound_spl_withdrawal_message(
                    &NONCE_ACCOUNT,
                    &bound_state.created_spl_withdrawal_txs()[&NONCE_ACCOUNT],
                )
                .unwrap();
            let events_before = EventsAssert::from_recorded();
            let error = SignCallError::CallFailed(
                CallRejected::with_rejection(4, "signing service unavailable".to_string()).into(),
            );
            let runtime = TestCanisterRuntime::new().with_time(321).add_signer(
                sign_as_minter()
                    .of_message(message.serialize())
                    .expect([Err(error), Ok(minter_signature())]),
            );

            assert!(matches!(
                sign_created_transaction(&runtime, &NONCE_ACCOUNT).await,
                Err(CreateTransferError::SigningFailed(_))
            ));
            read_state(|state| assert_eq!(state, &bound_state));
            assert_eq!(EventsAssert::from_recorded(), events_before);

            let transaction = sign_created_transaction(&runtime, &NONCE_ACCOUNT)
                .await
                .unwrap();
            assert_eq!(transaction.message, message);
            assert_eq!(transaction.signatures, vec![minter_signature()]);
            read_state(|state| {
                assert!(state.created_spl_withdrawal_txs().is_empty());
                assert!(state.created_spl_withdrawal_requests().is_empty());
                assert_eq!(
                    state.sent_spl_withdrawal_requests()[&42.into()].signature,
                    minter_signature()
                );
                assert_eq!(state.nonce_pool().num_free_accounts(), 0);
                assert_eq!(state.balance(), bound_state.balance());
                assert!(
                    state
                        .submitted_transactions()
                        .get(&minter_signature())
                        .is_some()
                );
            });
            assert_eq!(
                EventsAssert::from_recorded().expect_contains_event_eq(
                    EventType::SubmittedTransaction {
                        signature: minter_signature(),
                        message: message.into(),
                        signers: vec![Signer::Minter],
                        purpose: TransactionPurpose::SplWithdrawal {
                            cksol_burn_block_index: 42.into()
                        },
                    }
                ),
                events_before
            );
            assert!(runtime.sent_transactions().is_empty());
        }
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
        Err(WithdrawSplError::TokenNotSupported { mint: mint.into() })
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
async fn should_burn_the_cksol_fee_without_recording_an_event() {
    init_state();
    init_schnorr_master_key();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let _guard = withdrawal_guard(request.account).unwrap();
    let runtime = TestCanisterRuntime::new()
        .add_stub_response(Ok::<Nat, TransferFromError>(42_u64.into()))
        .with_time(123);

    let events_before = EventsAssert::from_recorded();
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
    assert_eq!(EventsAssert::from_recorded(), events_before);
    read_state(|state| {
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
async fn should_return_the_ledger_rejection_of_the_fee_burn() {
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
            assert!(state.pending_spl_withdrawal_requests().is_empty());
            assert!(state.pending_spl_withdrawal_fee_reimbursements().is_empty());
        });
    }
}

#[tokio::test]
async fn should_return_a_failed_fee_burn_call_as_temporarily_unavailable() {
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
        assert!(state.pending_spl_withdrawal_requests().is_empty());
        assert!(state.pending_spl_withdrawal_fee_reimbursements().is_empty());
    });
}

fn assert_nothing_recorded(token: SupportedSplToken) {
    read_state(|state| {
        assert!(state.pending_spl_withdrawal_requests().is_empty());
        assert!(state.pending_spl_withdrawal_fee_reimbursements().is_empty());
    });
    assert_only_registration(token);
}

#[tokio::test]
async fn should_burn_both_ledgers_then_accept_the_spl_withdrawal() {
    init_state();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let _guard = withdrawal_guard(request.account).unwrap();
    let runtime = TestCanisterRuntime::new()
        .add_stub_response(Ok::<Nat, TransferFromError>(42_u64.into()))
        .add_stub_response(Ok::<Nat, TransferFromError>(7_u64.into()))
        .with_time(123);

    let fee_index = burn_withdrawal_fee(&runtime, &request).await.unwrap();
    assert_eq!(
        burn_withdrawal_amount(&runtime, fee_index, &request).await,
        Ok(WithdrawSplOk {
            cksol_burn_block_index: 42,
            ckspl_burn_block_index: 7,
        })
    );

    let calls = runtime.sent_update_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].canister_id, ledger_canister_id());
    assert_eq!(
        calls[0].single_arg::<TransferFromArgs>().amount,
        Nat::from(request.fee)
    );
    assert_eq!(calls[1].canister_id, token.ledger_id);
    assert_eq!(calls[1].method, "icrc2_transfer_from");
    assert_eq!(
        calls[1].single_arg::<TransferFromArgs>(),
        TransferFromArgs {
            spender_subaccount: None,
            from: request.account,
            to: runtime.canister_self().into(),
            amount: request.amount.into(),
            fee: None,
            memo: Some(Memo::spl_convert(42, request.address).into()),
            created_at_time: None,
        }
    );
    let accepted = SplWithdrawalRequest {
        terms: request.clone(),
        cksol_burn_block_index: 42.into(),
        ckspl_burn_block_index: 7.into(),
    };
    EventsAssert::from_recorded()
        .expect_event_eq(EventType::AddedSplToken(token))
        .expect_event_eq(EventType::AcceptedSplWithdrawalRequest(accepted.clone()))
        .assert_no_more_events();
    read_state(|state| {
        assert!(state.pending_spl_withdrawal_fee_reimbursements().is_empty());
        assert_eq!(state.pending_spl_withdrawal_requests().len(), 1);
        assert_eq!(
            state.pending_spl_withdrawal_requests()[&42.into()],
            crate::state::PendingSplWithdrawalRequest {
                request: accepted,
                created_at: 123,
            }
        );
    });
}

#[tokio::test]
#[should_panic(expected = "BUG: unexpected ckSPL burn error")]
async fn should_reject_an_unexpected_duplicate_token_burn_reply() {
    init_state();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let _guard = withdrawal_guard(request.account).unwrap();
    let runtime =
        TestCanisterRuntime::new().add_stub_response(Err::<Nat, _>(TransferFromError::Duplicate {
            duplicate_of: 7_u64.into(),
        }));
    let _ = burn_withdrawal_amount(&runtime, 42.into(), &request).await;
}

#[tokio::test]
async fn should_return_the_ledger_rejection_of_the_token_burn_without_recording_an_event() {
    init_state();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let _guard = withdrawal_guard(request.account).unwrap();
    for error in [
        TransferFromError::InsufficientFunds {
            balance: 10_u64.into(),
        },
        TransferFromError::InsufficientAllowance {
            allowance: 20_u64.into(),
        },
        TransferFromError::TemporarilyUnavailable,
    ] {
        let runtime = TestCanisterRuntime::new().add_stub_response(Err::<Nat, _>(error.clone()));

        assert_eq!(
            burn_withdrawal_amount(&runtime, 42.into(), &request).await,
            Err(ledger::spl::SplBurnError::TransferFrom(error))
        );

        let calls = runtime.sent_update_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].canister_id, token.ledger_id);
        assert_eq!(
            calls[0].single_arg::<TransferFromArgs>().created_at_time,
            None
        );
        assert_nothing_recorded(token.clone());
    }
}

#[tokio::test]
async fn should_return_a_failed_token_burn_call_without_recording_an_event() {
    init_state();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let _guard = withdrawal_guard(request.account).unwrap();
    let error = IcError::CandidDecodeFailed {
        message: "invalid ledger reply".to_string(),
    };
    let runtime = TestCanisterRuntime::new().add_stub_error(error.clone());

    assert_eq!(
        burn_withdrawal_amount(&runtime, 42.into(), &request).await,
        Err(ledger::spl::SplBurnError::Call(error))
    );

    assert_eq!(runtime.sent_update_calls().len(), 1);
    assert_nothing_recorded(token);
}

#[tokio::test]
async fn should_queue_the_appropriate_fee_refund_after_a_returned_token_burn_error() {
    init_state();
    let token = register_token(TokenProgram::Classic);
    let mut request = withdrawal_terms(&token);
    request.amount = 1;
    let _guard = withdrawal_guard(request.account).unwrap();
    let ledger_fee = 10_000;
    let cases = [
        (
            TransferFromError::InsufficientFunds {
                balance: 0_u64.into(),
            },
            request.fee - ledger_fee,
        ),
        (
            TransferFromError::InsufficientAllowance {
                allowance: 0_u64.into(),
            },
            request.fee - ledger_fee,
        ),
        (
            TransferFromError::BadBurn {
                min_burn_amount: 2_u64.into(),
            },
            request.fee - ledger_fee,
        ),
        (TransferFromError::TemporarilyUnavailable, request.fee),
        (
            TransferFromError::GenericError {
                error_code: 1_u64.into(),
                message: "ledger error".to_string(),
            },
            request.fee,
        ),
    ];
    let mut expected_refunds = Vec::new();
    for (index, (error, reimbursed_amount)) in cases.into_iter().enumerate() {
        let burn_index = LedgerBurnIndex::new(42 + index as u64);
        let runtime = TestCanisterRuntime::new()
            .add_stub_response(Err::<Nat, _>(error))
            .with_time(999);

        let error = burn_withdrawal_amount(&runtime, burn_index, &request)
            .await
            .unwrap_err();
        schedule_withdrawal_fee_reimbursement(&runtime, burn_index, &request, &error, ledger_fee);

        let calls = runtime.sent_update_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].canister_id, token.ledger_id);
        assert_eq!(calls[0].method, "icrc2_transfer_from");
        let refund = SplWithdrawalFeeReimbursementRequest {
            cksol_burn_block_index: burn_index,
            reimbursed_amount,
            to: request.account,
        };
        read_state(|state| {
            assert!(state.pending_spl_withdrawal_requests().is_empty());
            assert_eq!(
                state.pending_spl_withdrawal_fee_reimbursements().len(),
                index + 1
            );
            assert_eq!(
                state.pending_spl_withdrawal_fee_reimbursements()[&burn_index],
                crate::state::PendingSplWithdrawalFeeReimbursement {
                    request: refund.clone(),
                    created_at: 999,
                }
            );
        });
        expected_refunds.push(refund);
    }
    let mut events = EventsAssert::from_recorded().expect_event_eq(EventType::AddedSplToken(token));
    for refund in expected_refunds {
        events = events.expect_event_eq(EventType::FailedSplWithdrawalRequest(refund));
    }
    events.assert_no_more_events();
}

#[test]
fn should_keep_unexpected_ledger_errors_out_of_the_refund_queue() {
    init_state();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let _guard = withdrawal_guard(request.account).unwrap();
    let runtime = TestCanisterRuntime::new();
    for error in [
        ledger::spl::SplBurnError::TransferFrom(TransferFromError::TooOld),
        ledger::spl::SplBurnError::TransferFrom(TransferFromError::CreatedInFuture {
            ledger_time: 0,
        }),
        ledger::spl::SplBurnError::TransferFrom(TransferFromError::BadFee {
            expected_fee: 1_u64.into(),
        }),
        ledger::spl::SplBurnError::TransferFrom(TransferFromError::Duplicate {
            duplicate_of: 7_u64.into(),
        }),
    ] {
        schedule_withdrawal_fee_reimbursement(&runtime, 42.into(), &request, &error, 10_000);
    }

    assert!(runtime.sent_update_calls().is_empty());
    assert_nothing_recorded(token);
}

#[test]
fn should_skip_zero_refunds_without_underflow() {
    init_state();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let _guard = withdrawal_guard(request.account).unwrap();
    let runtime = TestCanisterRuntime::new();
    let error = ledger::spl::SplBurnError::TransferFrom(TransferFromError::InsufficientFunds {
        balance: 0_u64.into(),
    });
    for ledger_fee in [request.fee, request.fee + 1, u64::MAX] {
        schedule_withdrawal_fee_reimbursement(&runtime, 42.into(), &request, &error, ledger_fee);
    }

    assert!(runtime.sent_update_calls().is_empty());
    assert_nothing_recorded(token);
}

#[test]
fn should_not_schedule_a_second_refund_for_the_same_fee_burn() {
    init_state();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let _guard = withdrawal_guard(request.account).unwrap();
    let runtime = TestCanisterRuntime::new().with_time(999);
    let error = ledger::spl::SplBurnError::TransferFrom(TransferFromError::TemporarilyUnavailable);
    schedule_withdrawal_fee_reimbursement(&runtime, 42.into(), &request, &error, 10_000);

    assert!(
        catch_unwind(AssertUnwindSafe(|| schedule_withdrawal_fee_reimbursement(
            &runtime,
            42.into(),
            &request,
            &error,
            10_000,
        )))
        .is_err()
    );

    assert!(runtime.sent_update_calls().is_empty());
    read_state(|state| {
        assert_eq!(state.pending_spl_withdrawal_fee_reimbursements().len(), 1);
    });
    EventsAssert::from_recorded()
        .expect_event_eq(EventType::AddedSplToken(token))
        .expect_event_eq(EventType::FailedSplWithdrawalRequest(
            SplWithdrawalFeeReimbursementRequest {
                cksol_burn_block_index: 42.into(),
                reimbursed_amount: request.fee,
                to: request.account,
            },
        ))
        .assert_no_more_events();
}

#[test]
fn should_build_fee_reimbursement_mint_arguments_from_the_recorded_request() {
    for (subaccount, reimbursed_amount) in [(None, 100_000_u64), (Some([7; 32]), 90_000)] {
        let to = Account {
            subaccount,
            ..account(1)
        };
        let pending = PendingSplWithdrawalFeeReimbursement {
            request: SplWithdrawalFeeReimbursementRequest {
                cksol_burn_block_index: 42.into(),
                reimbursed_amount,
                to,
            },
            created_at: 999,
        };

        let args = fee_reimbursement_transfer_arg(&pending);

        assert_eq!(args.to, to);
        assert_eq!(args.amount, Nat::from(reimbursed_amount));
        assert_eq!(args.from_subaccount, None);
        assert_eq!(args.fee, None);
        assert_eq!(args.created_at_time, Some(999));
        assert_eq!(
            minicbor::decode::<Memo>(&args.memo.unwrap().0).unwrap(),
            Memo::Mint(MintMemo::SplWithdrawalFeeReimbursement {
                cksol_burn_block_index: 42,
            }),
        );
    }
}

fn queued_fee_reimbursement() -> PendingSplWithdrawalFeeReimbursement {
    reset_state();
    reset_events();
    init_state();
    let token = register_token(TokenProgram::Classic);
    queue_fee_reimbursement(&token, 42.into())
}

fn queue_fee_reimbursement(
    token: &SupportedSplToken,
    cksol_burn_block_index: LedgerBurnIndex,
) -> PendingSplWithdrawalFeeReimbursement {
    schedule_withdrawal_fee_reimbursement(
        &TestCanisterRuntime::new().with_time(999),
        cksol_burn_block_index,
        &withdrawal_terms(token),
        &SplBurnError::TransferFrom(TransferFromError::TemporarilyUnavailable),
        10_000,
    );
    read_state(|state| {
        state.pending_spl_withdrawal_fee_reimbursements()[&cksol_burn_block_index].clone()
    })
}

fn reimbursement_runtime<I: IntoIterator<Item = Result<Nat, TransferError>>>(
    replies: I,
) -> TestCanisterRuntime {
    let mut runtime = TestCanisterRuntime::new().with_increasing_time_from(1_000);
    for reply in replies {
        runtime = runtime.add_stub_response(reply);
    }
    runtime
}

fn assert_reimbursed(cksol_burn_block_index: LedgerBurnIndex, mint_block_index: u64) {
    read_state(|state| {
        assert!(
            !state
                .pending_spl_withdrawal_fee_reimbursements()
                .contains_key(&cksol_burn_block_index)
        );
        assert_eq!(
            state.completed_spl_withdrawal_fee_reimbursements()[&cksol_burn_block_index]
                .mint_block_index,
            mint_block_index.into()
        );
    });
    EventsAssert::from_recorded().expect_contains_event_eq(EventType::ReimbursedSplWithdrawalFee {
        cksol_burn_block_index,
        mint_block_index: mint_block_index.into(),
    });
}

#[tokio::test]
async fn should_reimburse_a_pending_fee_with_fixed_arguments() {
    let pending = queued_fee_reimbursement();
    let runtime = reimbursement_runtime([Ok(100_u64.into())]);

    process_pending_fee_reimbursements(runtime.clone()).await;

    let calls = runtime.sent_update_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].canister_id, ledger_canister_id());
    assert_eq!(calls[0].method, "icrc1_transfer");
    let (args,): (TransferArg,) = calls[0].args();
    assert_eq!(args, fee_reimbursement_transfer_arg(&pending));
    assert_reimbursed(42.into(), 100);
}

#[tokio::test]
async fn should_record_a_duplicate_reply_as_reimbursed() {
    queued_fee_reimbursement();
    let runtime = reimbursement_runtime([Err(TransferError::Duplicate {
        duplicate_of: 100_u64.into(),
    })]);

    process_pending_fee_reimbursements(runtime).await;

    assert_reimbursed(42.into(), 100);
}

#[tokio::test]
async fn should_reimburse_every_pending_fee_in_one_round() {
    queued_fee_reimbursement();
    let token = read_state(|state| state.supported_spl_token(&[3; 32].into()).cloned()).unwrap();
    queue_fee_reimbursement(&token, 43.into());
    let runtime = reimbursement_runtime([Ok(100_u64.into()), Ok(101_u64.into())]);

    process_pending_fee_reimbursements(runtime.clone()).await;

    assert_eq!(runtime.sent_update_calls().len(), 2);
    assert_reimbursed(42.into(), 100);
    assert_reimbursed(43.into(), 101);
}

#[tokio::test]
async fn should_keep_the_reimbursement_pending_after_a_transient_failure() {
    for runtime in [
        reimbursement_runtime([Err(TransferError::TemporarilyUnavailable)]),
        reimbursement_runtime([]).add_stub_error(IcError::CallPerformFailed),
    ] {
        let pending = queued_fee_reimbursement();
        let before = read_state(Clone::clone);

        process_pending_fee_reimbursements(runtime.clone()).await;

        assert_eq!(runtime.sent_update_calls().len(), 1);
        read_state(|state| {
            assert_eq!(state, &before);
            assert_eq!(
                state.pending_spl_withdrawal_fee_reimbursements()[&42.into()],
                pending
            );
        });
    }
}

#[tokio::test]
async fn should_quarantine_an_expired_reimbursement_without_calling_the_ledger() {
    let pending = queued_fee_reimbursement();
    let expired = pending.created_at + LEDGER_DEDUPLICATION_WINDOW.as_nanos() as u64 + 1;
    let runtime = TestCanisterRuntime::new().with_time(expired);

    process_pending_fee_reimbursements(runtime.clone()).await;

    assert!(runtime.sent_update_calls().is_empty());
    assert_quarantined(pending);
}

#[tokio::test]
async fn should_quarantine_a_reimbursement_the_ledger_rejects_as_too_old() {
    let pending = queued_fee_reimbursement();
    let runtime = reimbursement_runtime([Err(TransferError::TooOld)]);

    process_pending_fee_reimbursements(runtime).await;

    assert_quarantined(pending);
}

#[tokio::test]
async fn should_skip_the_round_while_another_one_runs() {
    let pending = queued_fee_reimbursement();
    let runtime = reimbursement_runtime([Ok(100_u64.into())]);
    let _guard = TimerGuard::new(TaskType::ReimburseSplWithdrawalFees).unwrap();

    process_pending_fee_reimbursements(runtime.clone()).await;

    assert!(runtime.sent_update_calls().is_empty());
    read_state(|state| {
        assert_eq!(
            state.pending_spl_withdrawal_fee_reimbursements()[&42.into()],
            pending
        )
    });
}

fn assert_quarantined(pending: PendingSplWithdrawalFeeReimbursement) {
    let cksol_burn_block_index = pending.request.cksol_burn_block_index;
    read_state(|state| {
        assert!(
            !state
                .pending_spl_withdrawal_fee_reimbursements()
                .contains_key(&cksol_burn_block_index)
        );
        assert_eq!(
            state.quarantined_spl_withdrawal_fee_reimbursements()[&cksol_burn_block_index],
            pending
        );
    });
    EventsAssert::from_recorded().expect_contains_event_eq(
        EventType::QuarantinedSplWithdrawalFeeReimbursement {
            cksol_burn_block_index,
        },
    );
}

#[tokio::test]
async fn should_reject_invalid_withdrawals_before_either_burn() {
    init_state();
    init_schnorr_master_key();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let before = read_state(Clone::clone);
    let runtime = TestCanisterRuntime::new();
    assert!(matches!(
        withdraw(
            &runtime,
            request.account,
            token.mint,
            0,
            &request.address.to_string(),
            true
        )
        .await,
        Err(WithdrawSplError::ValueTooSmall { .. })
    ));
    assert!(matches!(
        withdraw(
            &runtime,
            request.account,
            token.mint,
            1,
            "not an address",
            true
        )
        .await,
        Err(WithdrawSplError::MalformedAddress(_))
    ));
    assert!(runtime.sent_update_calls().is_empty());
    read_state(|state| assert_eq!(state, &before));
}

async fn withdraw_request(
    runtime: &TestCanisterRuntime,
    request: &SplWithdrawalRequestTerms,
) -> Result<WithdrawSplOk, WithdrawSplError> {
    withdraw(
        runtime,
        request.account,
        request.mint,
        request.amount,
        &request.address.to_string(),
        request.create_destination_account,
    )
    .await
}

#[test]
fn should_preserve_ledger_amounts_larger_than_u64_in_spl_burn_errors() {
    let amount = Nat::from(u128::MAX);
    for (error, expected) in [
        (
            TransferFromError::InsufficientFunds {
                balance: amount.clone(),
            },
            LedgerError::InsufficientFunds {
                balance: amount.clone(),
                failed_burn_amount: u64::MAX.into(),
                token_symbol: "solUSDC".to_string(),
                ledger_id: ledger_canister_id(),
            },
        ),
        (
            TransferFromError::InsufficientAllowance {
                allowance: amount.clone(),
            },
            LedgerError::InsufficientAllowance {
                allowance: amount.clone(),
                failed_burn_amount: u64::MAX.into(),
                token_symbol: "solUSDC".to_string(),
                ledger_id: ledger_canister_id(),
            },
        ),
        (
            TransferFromError::BadBurn {
                min_burn_amount: amount.clone(),
            },
            LedgerError::AmountTooLow {
                minimum_burn_amount: amount.clone(),
                failed_burn_amount: u64::MAX.into(),
                token_symbol: "solUSDC".to_string(),
                ledger_id: ledger_canister_id(),
            },
        ),
    ] {
        assert_eq!(
            ckspl_burn_error(
                SplBurnError::TransferFrom(error),
                u64::MAX,
                &CkLedger {
                    token_symbol: "solUSDC".to_string(),
                    id: ledger_canister_id()
                }
            ),
            expected
        );
    }
}

#[tokio::test]
async fn should_accept_a_withdrawal_with_both_burns_under_one_guard() {
    init_state();
    init_schnorr_master_key();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let runtime = TestCanisterRuntime::new()
        .with_increasing_time_from(123)
        .add_stub_response(Ok::<Nat, TransferFromError>(42_u64.into()))
        .add_stub_response(Ok::<Nat, TransferFromError>(7_u64.into()));
    assert_eq!(
        withdraw_request(&runtime, &request).await,
        Ok(WithdrawSplOk {
            cksol_burn_block_index: 42,
            ckspl_burn_block_index: 7
        })
    );
    assert_eq!(runtime.sent_update_calls().len(), 2);
    read_state(|state| {
        assert_eq!(
            state.pending_spl_withdrawal_requests()[&42.into()]
                .request
                .terms,
            request
        )
    });
    assert!(withdrawal_guard(request.account).is_ok());
}

#[tokio::test]
async fn should_refund_token_burn_errors_like_ckerc20() {
    for call_error in [
        None,
        Some(IcError::CallPerformFailed),
        Some(IcError::CandidDecodeFailed {
            message: "invalid reply".to_string(),
        }),
    ] {
        let full_refund = call_error.is_some();
        reset_state();
        reset_events();
        init_state();
        init_schnorr_master_key();
        let token = register_token(TokenProgram::Classic);
        let request = withdrawal_terms(&token);
        let runtime = TestCanisterRuntime::new()
            .with_increasing_time_from(123)
            .add_stub_response(Ok::<Nat, TransferFromError>(42_u64.into()));
        let runtime = if let Some(error) = call_error {
            runtime.add_stub_error(error)
        } else {
            runtime.add_stub_response(Err::<Nat, _>(TransferFromError::InsufficientFunds {
                balance: 0_u64.into(),
            }))
        };
        let error = withdraw_request(&runtime, &request).await.unwrap_err();
        assert!(matches!(&error,
            WithdrawSplError::CkSplLedgerError { cksol_burn_block_index, .. } if *cksol_burn_block_index == 42));
        if full_refund {
            assert!(matches!(&error, WithdrawSplError::CkSplLedgerError {
                error: LedgerError::TemporarilyUnavailable(message), ..
            } if message.contains(token.token_symbol.as_ref().unwrap())));
        }
        if !full_refund {
            assert_eq!(
                error,
                WithdrawSplError::CkSplLedgerError {
                    cksol_burn_block_index: 42,
                    error: LedgerError::InsufficientFunds {
                        balance: 0_u64.into(),
                        failed_burn_amount: request.amount.into(),
                        token_symbol: token.token_symbol.clone().unwrap(),
                        ledger_id: token.ledger_id,
                    },
                }
            );
        }
        read_state(|state| {
            assert!(state.pending_spl_withdrawal_requests().is_empty());
            assert_eq!(
                state.pending_spl_withdrawal_fee_reimbursements()[&42.into()]
                    .request
                    .reimbursed_amount,
                if full_refund {
                    request.fee
                } else {
                    request.fee - CKSOL_LEDGER_TRANSACTION_FEE
                },
            );
        });
        assert!(withdrawal_guard(request.account).is_ok());
    }
}

#[tokio::test]
async fn should_stop_before_token_burn_if_the_guard_or_fee_burn_fails() {
    init_state();
    init_schnorr_master_key();
    let token = register_token(TokenProgram::Classic);
    let request = withdrawal_terms(&token);
    let runtime = TestCanisterRuntime::new().add_stub_response(Err::<Nat, _>(
        TransferFromError::InsufficientAllowance {
            allowance: 0_u64.into(),
        },
    ));
    let events_before = EventsAssert::from_recorded();
    let guard = withdrawal_guard(request.account).unwrap();
    assert_eq!(
        withdraw_request(&runtime, &request).await,
        Err(WithdrawSplError::AlreadyProcessing)
    );
    assert!(runtime.sent_update_calls().is_empty());
    drop(guard);
    assert_eq!(
        withdraw_request(&runtime, &request).await,
        Err(WithdrawSplError::CkSolLedgerError {
            error: LedgerError::InsufficientAllowance {
                allowance: 0_u64.into(),
                failed_burn_amount: request.fee.into(),
                token_symbol: "ckSOL".to_string(),
                ledger_id: ledger_canister_id(),
            }
        })
    );
    assert_eq!(runtime.sent_update_calls().len(), 1);
    assert!(withdrawal_guard(request.account).is_ok());
    assert_eq!(EventsAssert::from_recorded(), events_before);
}

#[test]
fn should_preserve_large_ledger_values_in_the_public_withdrawal_error() {
    let balance = Nat::from(u128::MAX);
    let error = WithdrawSplError::CkSplLedgerError {
        cksol_burn_block_index: u64::MAX,
        error: LedgerError::InsufficientFunds {
            balance: balance.clone(),
            failed_burn_amount: u64::MAX.into(),
            token_symbol: "solUSDC".to_string(),
            ledger_id: ledger_canister_id(),
        },
    };
    let encoded = candid::encode_one(&error).unwrap();
    let decoded: cksol_types::WithdrawSplError = candid::decode_one(&encoded).unwrap();
    assert_eq!(
        decoded,
        cksol_types::WithdrawSplError::CkSplLedgerError {
            cksol_burn_block_index: u64::MAX,
            error: LedgerError::InsufficientFunds {
                balance,
                failed_burn_amount: u64::MAX.into(),
                token_symbol: "solUSDC".to_string(),
                ledger_id: ledger_canister_id(),
            },
        }
    );
}

#[tokio::test]
async fn should_not_burn_before_a_legacy_ledger_symbol_is_configured() {
    init_state();
    init_schnorr_master_key();
    let mut token = spl_token([3; 32].into(), TokenProgram::Classic);
    token.token_symbol = None;
    mutate_state(|state| {
        process_event(
            state,
            EventType::AddedSplToken(token.clone()),
            &TestCanisterRuntime::new().with_time(1),
        )
    });
    let request = withdrawal_terms(&token);
    let runtime = TestCanisterRuntime::new();
    assert!(matches!(withdraw_request(&runtime, &request).await,
        Err(WithdrawSplError::TemporarilyUnavailable(message)) if message.contains("symbol")));
    assert!(runtime.sent_update_calls().is_empty());
}

#[test]
fn should_identify_the_native_ledger_from_the_configured_network() {
    for (network, symbol) in [
        (SolanaNetwork::Mainnet, "ckSOL"),
        (SolanaNetwork::Devnet, "ckDevnetSOL"),
        (SolanaNetwork::Testnet, "ckTestnetSOL"),
    ] {
        reset_state();
        let mut args = crate::test_fixtures::valid_init_args();
        args.solana_network = network;
        crate::test_fixtures::init_state_with_args(args);
        assert_eq!(
            cksol_ledger(),
            CkLedger {
                token_symbol: symbol.to_string(),
                id: ledger_canister_id()
            }
        );
    }
}
