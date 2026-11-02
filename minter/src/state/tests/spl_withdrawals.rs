use crate::{
    constants::{FEE_PER_SIGNATURE, RENT_EXEMPTION_THRESHOLD},
    lifecycle,
    numeric::LedgerBurnIndex,
    sol_transfer::BATCH_WITHDRAWAL_TX_FEE,
    spl_transfer::{build_spl_withdrawal_instructions, build_spl_withdrawal_message},
    state::{
        CompletedSplWithdrawalFeeReimbursement, MinterTransaction,
        PendingSplWithdrawalFeeReimbursement, PendingSplWithdrawalRequest, State, TokenProgram,
        WithdrawalPlan,
        audit::{process_event, replay_events},
        event::{
            Event, EventType, Signer, SplWithdrawalFeeReimbursementRequest, SplWithdrawalRequest,
            SplWithdrawalRequestTerms, TransactionPurpose, VersionedMessage, WithdrawalRequest,
        },
        mutate_state, read_state, reset_state,
    },
    storage::{reset_events, with_event_iter},
    test_fixtures::{
        MINTER_ADDRESS, NONCE_ACCOUNT, account, events, init_args_with_nonce_account,
        init_balance_to, minter_public_key_fetched_event, minter_signature,
        runtime::TestCanisterRuntime, spl_token, valid_init_args,
    },
};
use candid::Principal;
use cksol_types::{TxFinalizedStatus, WithdrawSplStatus};
use cksol_types_internal::InitArgs;
use ic_stable_structures::Storable;
use sol_rpc_types::Lamport;
use solana_hash::Hash;
use solana_transaction::Message;
use std::{
    collections::BTreeMap,
    panic::{AssertUnwindSafe, catch_unwind},
};

const FUNDED_BALANCE: Lamport = 500_000_000;
const FUNDED_TOKENS: u64 = 10_000_000;

fn request(mint_byte: u8) -> SplWithdrawalRequestTerms {
    let token = spl_token([mint_byte; 32].into(), TokenProgram::Classic);
    SplWithdrawalRequestTerms {
        account: account(1),
        mint: token.mint,
        ledger_id: token.ledger_id,
        address: [9; 32].into(),
        amount: 1_000_000,
        fee: 1_000_000,
        create_destination_account: false,
    }
}

fn record(payload: EventType, timestamp: u64) {
    let runtime = TestCanisterRuntime::new().with_time(timestamp);
    mutate_state(|state| {
        // Failed IC message execution rolls state changes back. Apply to a copy
        // so caught assertion failures in these tests model that behavior.
        let mut updated = state.clone();
        process_event(&mut updated, payload, &runtime);
        *state = updated;
    });
}

fn setup() -> SplWithdrawalRequestTerms {
    setup_with(valid_init_args())
}

fn setup_with(init_args: InitArgs) -> SplWithdrawalRequestTerms {
    lifecycle::init(init_args, TestCanisterRuntime::new().with_time(1));
    let request = request(2);
    record(
        EventType::AddedSplToken(spl_token(request.mint, TokenProgram::Classic)),
        2,
    );
    request
}

fn accepted_request(
    id: u64,
    token_burn: u64,
    terms: &SplWithdrawalRequestTerms,
) -> SplWithdrawalRequest {
    SplWithdrawalRequest {
        terms: terms.clone(),
        cksol_burn_block_index: id.into(),
        ckspl_burn_block_index: token_burn.into(),
    }
}

fn spl_transaction_created(burn_indices: &[u64]) -> EventType {
    EventType::CreatedSplWithdrawalTransaction {
        burn_indices: burn_indices
            .iter()
            .copied()
            .map(LedgerBurnIndex::new)
            .collect(),
        nonce_account: NONCE_ACCOUNT,
        nonce_value: Hash::new_from_array([18; 32]),
    }
}

/// The message of the withdrawal bound to the test nonce account.
fn bound_message(state: &State) -> Message {
    let created = &state.created_spl_withdrawal_txs()[&NONCE_ACCOUNT];
    state
        .bound_spl_withdrawal_message(&NONCE_ACCOUNT, created)
        .unwrap()
}

fn submitted_spl_withdrawal(id: u64) -> EventType {
    submitted_spl_withdrawals(&[id])
}

fn submitted_spl_withdrawals(burn_indices: &[u64]) -> EventType {
    EventType::SubmittedTransaction {
        signature: minter_signature(),
        message: read_state(bound_message).into(),
        signers: vec![Signer::Minter],
        purpose: TransactionPurpose::SplWithdrawal {
            burn_indices: burn_indices
                .iter()
                .copied()
                .map(LedgerBurnIndex::new)
                .collect(),
        },
    }
}

fn setup_created_spl_withdrawal(
    token_program: TokenProgram,
    create_destination_account: bool,
) -> SplWithdrawalRequestTerms {
    let mut request = setup_with_nonce();
    let token = spl_token([4; 32].into(), token_program);
    record(EventType::AddedSplToken(token.clone()), 10);
    events::fund_spl_treasury(&token, FUNDED_TOKENS);
    request.mint = token.mint;
    request.ledger_id = token.ledger_id;
    request.create_destination_account = create_destination_account;
    accept(100, 7, &request);
    record(spl_transaction_created(&[100]), 30);
    request
}

fn setup_created_spl_batch() -> [SplWithdrawalRequestTerms; 2] {
    let mut first = setup_with_nonce();
    first.create_destination_account = true;
    accept(100, 7, &first);
    let mut token = spl_token([5; 32].into(), TokenProgram::Token2022);
    token.decimals = 9;
    record(EventType::AddedSplToken(token.clone()), 40);
    events::fund_spl_treasury(&token, 200_000_000);
    let second = SplWithdrawalRequestTerms {
        mint: token.mint,
        ledger_id: token.ledger_id,
        amount: 123_456_789,
        ..first.clone()
    };
    record(
        EventType::AcceptedSplWithdrawalRequest(accepted_request(101, 8, &second)),
        50,
    );

    record(spl_transaction_created(&[100, 101]), 60);
    [first, second]
}

/// Submits either five requests sharing one ATA or two requests using different token programs.
fn setup_submitted_spl_batch(shared_destination: bool) -> (Vec<u64>, Lamport) {
    reset_state();
    reset_events();
    let (indices, reserved) = if shared_destination {
        let mut request = setup_with_nonce();
        for id in 100..105 {
            request.create_destination_account = id != 100;
            accept(id, id, &request);
        }
        let indices = vec![100, 101, 102, 103, 104];
        record(spl_transaction_created(&indices), 60);
        (
            indices,
            FEE_PER_SIGNATURE + TokenProgram::Classic.account_rent_exemption(),
        )
    } else {
        setup_created_spl_batch();
        (
            vec![100, 101],
            FEE_PER_SIGNATURE
                + TokenProgram::Classic.account_rent_exemption()
                + TokenProgram::Token2022.account_rent_exemption(),
        )
    };
    record(submitted_spl_withdrawals(&indices), 70);
    (indices, reserved)
}

/// Records the terminal event once, rejects a repeat, and checks the shared outcome:
/// the request left the sent map and the nonce account is free again.
fn finalize(terminal: EventType) -> State {
    record(terminal.clone(), 50);
    assert!(catch_unwind(AssertUnwindSafe(|| record(terminal, 51))).is_err());
    let replayed = assert_replay_matches();
    assert!(replayed.sent_spl_withdrawal_requests().is_empty());
    assert!(
        replayed
            .submitted_transactions()
            .get(&minter_signature())
            .is_none()
    );
    assert_eq!(replayed.nonce_pool().num_free_accounts(), 1);
    replayed
}

/// Inits with a nonce account and funds the balance through a swept deposit.
fn setup_with_nonce() -> SplWithdrawalRequestTerms {
    let request = setup_with(init_args_with_nonce_account());
    record(minter_public_key_fetched_event(), 3);
    init_balance_to(FUNDED_BALANCE);
    events::fund_spl_treasury(
        &spl_token(request.mint, TokenProgram::Classic),
        FUNDED_TOKENS,
    );
    request
}

fn assert_reserves(create_destination_account: bool, cost: Lamport) {
    let mut request = setup_with_nonce();
    request.create_destination_account = create_destination_account;
    accept(100, 7, &request);
    let balance_before = read_state(|state| state.balance());
    record(spl_transaction_created(&[100]), 30);
    assert_eq!(assert_replay_matches().balance(), balance_before - cost);
}

/// Records an accepted SPL withdrawal without calling either ledger.
/// `id` is the ckSOL fee burn block index identifying the withdrawal;
/// `token_burn` is the separate ckSPL token burn block index.
fn accept(id: u64, token_burn: u64, terms: &SplWithdrawalRequestTerms) {
    record(
        EventType::AcceptedSplWithdrawalRequest(accepted_request(id, token_burn, terms)),
        20,
    );
}

fn reimburse_fee(id: u64, amount: u64, to: icrc_ledger_types::icrc1::account::Account) {
    record(
        EventType::FailedSplWithdrawalRequest(SplWithdrawalFeeReimbursementRequest {
            cksol_burn_block_index: id.into(),
            reimbursed_amount: amount,
            to,
        }),
        30,
    );
}

fn assert_replay_matches() -> State {
    let events = with_event_iter(|events| {
        events
            .map(|event| Event::from_bytes(event.to_bytes()))
            .collect::<Vec<_>>()
    });
    let replayed = replay_events(events);
    read_state(|state| assert_eq!(state, &replayed));
    replayed
}

#[test]
fn should_replay_a_completed_fee_reimbursement_and_preserve_other_pending_refunds() {
    let request = setup();
    for id in [100_u64, 101] {
        reimburse_fee(id, request.fee, request.account);
    }
    let completed = EventType::ReimbursedSplWithdrawalFee {
        cksol_burn_block_index: 100.into(),
        mint_block_index: 200.into(),
    };
    record(completed.clone(), 40);
    assert!(catch_unwind(AssertUnwindSafe(|| record(completed, 50))).is_err());
    assert!(
        catch_unwind(AssertUnwindSafe(|| reimburse_fee(
            100,
            request.fee,
            request.account
        )))
        .is_err()
    );
    assert!(catch_unwind(AssertUnwindSafe(|| accept(100, 7, &request))).is_err());
    let replayed = assert_replay_matches();
    assert_eq!(
        replayed.completed_spl_withdrawal_fee_reimbursements(),
        &BTreeMap::from([(
            100.into(),
            CompletedSplWithdrawalFeeReimbursement {
                request: SplWithdrawalFeeReimbursementRequest {
                    cksol_burn_block_index: 100.into(),
                    reimbursed_amount: request.fee,
                    to: request.account,
                },
                mint_block_index: 200.into(),
                created_at: 30,
                reimbursed_at: 40,
            }
        )])
    );
    assert_eq!(
        replayed
            .pending_spl_withdrawal_fee_reimbursements()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![101.into()]
    );
}

#[test]
fn should_reject_fee_reimbursement_completion_without_a_pending_refund() {
    setup();
    for id in [100_u64, 101] {
        assert!(
            catch_unwind(AssertUnwindSafe(|| record(
                EventType::ReimbursedSplWithdrawalFee {
                    cksol_burn_block_index: id.into(),
                    mint_block_index: 200.into(),
                },
                40
            )))
            .is_err()
        );
    }
    let replayed = assert_replay_matches();
    assert!(
        replayed
            .completed_spl_withdrawal_fee_reimbursements()
            .is_empty()
    );
}

#[test]
fn should_replay_quarantine_and_keep_the_original_refund_out_of_the_pending_queue() {
    let request = setup();
    for id in [100_u64, 101] {
        reimburse_fee(id, request.fee, request.account);
    }
    let original =
        read_state(|state| state.pending_spl_withdrawal_fee_reimbursements()[&100.into()].clone());
    let quarantined = EventType::QuarantinedSplWithdrawalFeeReimbursement {
        cksol_burn_block_index: 100.into(),
    };
    record(quarantined.clone(), 40);
    assert!(catch_unwind(AssertUnwindSafe(|| record(quarantined, 50))).is_err());
    assert!(
        catch_unwind(AssertUnwindSafe(|| reimburse_fee(
            100,
            request.fee,
            request.account
        )))
        .is_err()
    );
    assert!(
        catch_unwind(AssertUnwindSafe(|| record(
            EventType::ReimbursedSplWithdrawalFee {
                cksol_burn_block_index: 100.into(),
                mint_block_index: 200.into(),
            },
            50
        )))
        .is_err()
    );
    let replayed = assert_replay_matches();
    assert_eq!(
        replayed.quarantined_spl_withdrawal_fee_reimbursements(),
        &BTreeMap::from([(100.into(), original)])
    );
    assert_eq!(
        replayed
            .pending_spl_withdrawal_fee_reimbursements()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![101.into()]
    );
}

#[test]
fn should_reject_quarantine_without_a_pending_refund() {
    setup();
    for id in [100_u64, 101] {
        assert!(
            catch_unwind(AssertUnwindSafe(|| record(
                EventType::QuarantinedSplWithdrawalFeeReimbursement {
                    cksol_burn_block_index: id.into(),
                },
                40
            )))
            .is_err()
        );
    }
    assert!(
        assert_replay_matches()
            .quarantined_spl_withdrawal_fee_reimbursements()
            .is_empty()
    );
}

#[test]
fn should_replay_submitted_spl_withdrawal_and_keep_its_nonce_bound() {
    let request = setup_created_spl_withdrawal(TokenProgram::Classic, false);
    accept(101, 8, &request);
    let submitted = submitted_spl_withdrawal(100);
    record(submitted.clone(), 40);
    assert!(catch_unwind(AssertUnwindSafe(|| record(submitted, 41))).is_err());

    let replayed = assert_replay_matches();
    assert!(replayed.created_spl_withdrawal_txs().is_empty());
    assert!(replayed.created_spl_withdrawal_requests().is_empty());
    assert!(
        replayed
            .pending_spl_withdrawal_requests()
            .contains_key(&101.into())
    );
    let sent = &replayed.sent_spl_withdrawal_requests()[&100.into()];
    assert_eq!(sent.request, accepted_request(100, 7, &request));
    assert_eq!(sent.created_at, 20);
    assert_eq!(sent.signature, minter_signature());
    assert_eq!(replayed.nonce_pool().num_free_accounts(), 0);
    assert!(matches!(
        replayed.submitted_transactions().get(&sent.signature),
        Some(MinterTransaction::Withdrawal {
            nonce_account, nonce_value, submitted_at: 40, ..
        }) if *nonce_account == NONCE_ACCOUNT && *nonce_value == Hash::new_from_array([18; 32])
    ));
    assert!(
        catch_unwind(AssertUnwindSafe(|| record(
            EventType::ExpiredTransaction {
                signature: sent.signature
            },
            50
        )))
        .is_err()
    );
    assert_replay_matches();
}

#[test]
fn should_reject_spl_submission_that_does_not_match_the_binding() {
    setup_created_spl_withdrawal(TokenProgram::Token2022, true);
    let original = read_state(Clone::clone);
    for change in 0..5 {
        let mut submitted = submitted_spl_withdrawal(100);
        let EventType::SubmittedTransaction {
            message: VersionedMessage::Legacy(message),
            signers,
            purpose,
            ..
        } = &mut submitted
        else {
            unreachable!()
        };
        match change {
            0 => {
                *purpose = TransactionPurpose::SplWithdrawal {
                    burn_indices: vec![101.into()],
                }
            }
            1 => message.recent_blockhash = Hash::new_from_array([19; 32]),
            2 => message.instructions.last_mut().unwrap().data.push(0),
            3 => signers.clear(),
            4 => {
                message.instructions.remove(0);
            }
            _ => unreachable!(),
        }
        assert!(catch_unwind(AssertUnwindSafe(|| record(submitted, 40))).is_err());
        assert_eq!(assert_replay_matches(), original);
    }
}

#[test]
fn should_move_a_succeeded_spl_withdrawal_to_successful() {
    for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
        reset_state();
        reset_events();
        let request = setup_created_spl_withdrawal(token_program, true);
        record(submitted_spl_withdrawal(100), 40);
        let balance_before = read_state(|state| state.balance());

        let replayed = finalize(EventType::SucceededTransaction {
            signature: minter_signature(),
        });

        assert_eq!(
            replayed.successful_spl_withdrawal_requests()[&100.into()].request,
            accepted_request(100, 7, &request)
        );
        assert!(replayed.failed_spl_withdrawal_requests().is_empty());
        assert_eq!(replayed.balance(), balance_before);
    }
}

#[test]
fn should_return_the_unspent_rent_of_a_failed_spl_withdrawal() {
    for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
        for create_destination_account in [false, true] {
            reset_state();
            reset_events();
            let request = setup_created_spl_withdrawal(token_program, create_destination_account);
            record(submitted_spl_withdrawal(100), 40);

            let replayed = finalize(EventType::FailedTransaction {
                signature: minter_signature(),
            });

            assert_eq!(
                replayed.failed_spl_withdrawal_requests()[&100.into()].request,
                accepted_request(100, 7, &request)
            );
            assert!(replayed.successful_spl_withdrawal_requests().is_empty());
            assert_eq!(replayed.balance(), FUNDED_BALANCE - FEE_PER_SIGNATURE);
        }
    }
}

#[test]
fn should_replay_the_actual_spend_of_a_successful_spl_withdrawal() {
    for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
        for spent_rent in [false, true] {
            reset_state();
            reset_events();
            setup_created_spl_withdrawal(token_program, true);
            record(submitted_spl_withdrawal(100), 40);
            let lamports_spent = FEE_PER_SIGNATURE
                + if spent_rent {
                    token_program.account_rent_exemption()
                } else {
                    0
                };

            let replayed = finalize(EventType::SucceededSplWithdrawalTransaction {
                signature: minter_signature(),
                lamports_spent,
            });

            assert_eq!(replayed.balance(), FUNDED_BALANCE - lamports_spent);
            assert!(
                replayed
                    .successful_spl_withdrawal_requests()
                    .contains_key(&100.into())
            );
        }
    }
}

#[test]
fn should_reject_spl_success_with_spend_above_the_reservation() {
    setup_created_spl_withdrawal(TokenProgram::Classic, true);
    record(submitted_spl_withdrawal(100), 40);
    let original = read_state(Clone::clone);
    let reserved = original
        .spl_withdrawal_reserved_cost(&minter_signature())
        .unwrap();
    for lamports_spent in [reserved + 1, u64::MAX] {
        assert!(
            catch_unwind(AssertUnwindSafe(|| record(
                EventType::SucceededSplWithdrawalTransaction {
                    signature: minter_signature(),
                    lamports_spent,
                },
                50,
            )))
            .is_err()
        );
        assert_eq!(assert_replay_matches(), original);
    }
}

#[test]
fn should_not_reuse_the_index_of_a_finalized_spl_withdrawal() {
    let request = setup_created_spl_withdrawal(TokenProgram::Classic, false);
    record(submitted_spl_withdrawal(100), 40);
    finalize(EventType::SucceededTransaction {
        signature: minter_signature(),
    });
    assert!(catch_unwind(AssertUnwindSafe(|| accept(100, 8, &request))).is_err());
    assert!(
        catch_unwind(AssertUnwindSafe(|| reimburse_fee(
            100,
            request.fee,
            request.account
        )))
        .is_err()
    );
    assert_replay_matches();
}

#[test]
fn should_replay_spl_withdrawal_bound_to_nonce() {
    let request = setup_with_nonce();
    accept(100, 7, &request);
    accept(101, 8, &request);
    record(spl_transaction_created(&[100]), 30);

    let replayed = assert_replay_matches();
    let nonce_account = NONCE_ACCOUNT;
    assert_eq!(replayed.pending_spl_withdrawal_requests().len(), 1);
    assert!(
        replayed
            .pending_spl_withdrawal_requests()
            .contains_key(&101.into())
    );
    let created = &replayed.created_spl_withdrawal_txs()[&nonce_account];
    assert_eq!(created.burn_indices, vec![100.into()]);
    assert_eq!(replayed.created_spl_withdrawal_requests().len(), 1);
    let withdrawal = &replayed.created_spl_withdrawal_requests()[&100.into()];
    assert_eq!(withdrawal.request, accepted_request(100, 7, &request));
    assert_eq!(withdrawal.created_at, 20);
    assert_eq!(created.nonce_value, Hash::new_from_array([18; 32]));
    assert_eq!(replayed.nonce_pool().num_free_accounts(), 0);
    assert!(
        replayed
            .nonce_pool()
            .has_seen(&nonce_account, &created.nonce_value)
    );
}

#[test]
fn should_rebuild_the_bound_spl_withdrawal_message_after_replay() {
    for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
        for create_destination_account in [false, true] {
            reset_state();
            reset_events();
            let mut request = setup_with_nonce();
            let mut token = spl_token([4; 32].into(), token_program);
            token.decimals = 9;
            record(EventType::AddedSplToken(token.clone()), 10);
            events::fund_spl_treasury(&token, 200_000_000);
            request.mint = token.mint;
            request.ledger_id = token.ledger_id;
            request.amount = 123_456_789;
            request.create_destination_account = create_destination_account;
            accept(100, 7, &request);
            record(spl_transaction_created(&[100]), 30);
            let instructions = build_spl_withdrawal_instructions(
                &MINTER_ADDRESS,
                &token,
                &request.address,
                request.amount,
                create_destination_account,
            );
            let expected = build_spl_withdrawal_message(
                &MINTER_ADDRESS,
                &NONCE_ACCOUNT,
                Hash::new_from_array([18; 32]),
                &instructions,
            )
            .unwrap();
            let message = read_state(bound_message);
            assert_eq!(message, expected);

            let replayed = assert_replay_matches();
            assert_eq!(bound_message(&replayed), message);
        }
    }
}

#[test]
fn should_rebuild_a_mixed_mint_spl_batch_from_created_requests() {
    let [first, second] = setup_created_spl_batch();
    let balance_before = FUNDED_BALANCE;
    let state = assert_replay_matches();
    let message = bound_message(&state);
    assert_eq!(
        state.balance(),
        balance_before
            - FEE_PER_SIGNATURE
            - TokenProgram::Classic.account_rent_exemption()
            - TokenProgram::Token2022.account_rent_exemption()
    );
    assert_eq!(state.nonce_pool().num_free_accounts(), 0);
    assert_eq!(
        state.created_spl_withdrawal_txs()[&NONCE_ACCOUNT].burn_indices,
        vec![100.into(), 101.into()]
    );
    assert!(state.pending_spl_withdrawal_requests().is_empty());
    assert_eq!(
        state.spl_treasury_balance(&first.mint),
        FUNDED_TOKENS - first.amount
    );
    assert_eq!(
        state.spl_treasury_balance(&second.mint),
        200_000_000 - second.amount
    );
    assert_eq!(message, read_state(bound_message));
    assert_eq!(message.recent_blockhash, Hash::new_from_array([18; 32]));
    assert_eq!(message.signer_keys(), vec![&MINTER_ADDRESS]);
    assert_eq!(message.instructions.len(), 5);
    assert_eq!(message.instructions[0].data, vec![4, 0, 0, 0]);
    for (instruction, terms, program, decimals) in [
        (&message.instructions[3], first, TokenProgram::Classic, 6),
        (&message.instructions[4], second, TokenProgram::Token2022, 9),
    ] {
        assert_eq!(
            message.account_keys[instruction.program_id_index as usize],
            program.id()
        );
        assert_eq!(
            message.account_keys[instruction.accounts[1] as usize],
            terms.mint
        );
        assert_eq!(
            spl_token_2022_interface::instruction::TokenInstruction::unpack(&instruction.data)
                .unwrap(),
            spl_token_2022_interface::instruction::TokenInstruction::TransferChecked {
                amount: terms.amount,
                decimals
            }
        );
    }
}

#[test]
fn should_replay_a_submitted_spl_batch_with_all_requests_under_one_signature() {
    setup_created_spl_batch();
    let balance_before = read_state(|state| state.balance());
    let message = read_state(bound_message);
    record(submitted_spl_withdrawals(&[100, 101]), 70);
    let state = assert_replay_matches();
    assert!(state.created_spl_withdrawal_requests().is_empty());
    assert!(state.created_spl_withdrawal_txs().is_empty());
    assert_eq!(state.sent_spl_withdrawal_requests().len(), 2);
    assert_eq!(state.submitted_transactions().len(), 1);
    assert_eq!(state.balance(), balance_before);
    assert_eq!(state.nonce_pool().num_free_accounts(), 0);
    for id in [100_u64, 101] {
        let sent = &state.sent_spl_withdrawal_requests()[&id.into()];
        assert_eq!(sent.signature, minter_signature());
        assert_eq!(
            state.spl_withdrawal_status(id),
            WithdrawSplStatus::TxSent {
                transaction_id: minter_signature().into()
            }
        );
    }
    assert!(
        matches!(state.submitted_transactions().get(&minter_signature()),
        Some(MinterTransaction::Withdrawal { message: VersionedMessage::Legacy(actual), nonce_account, submitted_at: 70, .. })
        if actual == &message && *nonce_account == NONCE_ACCOUNT)
    );
}

#[test]
fn should_reconcile_the_whole_spl_batch_reservation_on_success() {
    for shared_destination in [false, true] {
        for spend_all_rent in [false, true] {
            let (indices, reserved) = setup_submitted_spl_batch(shared_destination);
            assert_eq!(
                read_state(|state| state.spl_withdrawal_reserved_cost(&minter_signature())),
                Some(reserved)
            );
            assert_eq!(
                read_state(|state| state.balance()),
                FUNDED_BALANCE - reserved
            );
            let lamports_spent = if spend_all_rent {
                reserved
            } else {
                FEE_PER_SIGNATURE
            };
            record(
                EventType::SucceededSplWithdrawalTransaction {
                    signature: minter_signature(),
                    lamports_spent,
                },
                80,
            );
            let state = assert_replay_matches();
            assert_eq!(state.balance(), FUNDED_BALANCE - lamports_spent);
            assert_eq!(
                state.successful_spl_withdrawal_requests().len(),
                indices.len()
            );
            assert!(state.sent_spl_withdrawal_requests().is_empty());
            assert_eq!(
                state.spl_withdrawal_reserved_cost(&minter_signature()),
                None
            );
            assert_eq!(state.nonce_pool().num_free_accounts(), 1);
        }
    }
}

#[test]
fn should_return_each_unused_ata_rent_once_after_an_spl_batch_fails() {
    for shared_destination in [false, true] {
        let (indices, reserved) = setup_submitted_spl_batch(shared_destination);
        assert_eq!(
            read_state(|state| state.spl_withdrawal_reserved_cost(&minter_signature())),
            Some(reserved)
        );
        record(
            EventType::FailedTransaction {
                signature: minter_signature(),
            },
            80,
        );
        let state = assert_replay_matches();
        assert_eq!(state.balance(), FUNDED_BALANCE - FEE_PER_SIGNATURE);
        assert_eq!(state.failed_spl_withdrawal_requests().len(), indices.len());
        assert!(state.sent_spl_withdrawal_requests().is_empty());
        assert_eq!(
            state.spl_withdrawal_reserved_cost(&minter_signature()),
            None
        );
        assert_eq!(state.nonce_pool().num_free_accounts(), 1);
        assert_eq!(state.spl_treasury_balance(&[2; 32].into()), FUNDED_TOKENS);
        if !shared_destination {
            assert_eq!(state.spl_treasury_balance(&[5; 32].into()), 200_000_000);
        }
    }
}

#[test]
fn should_split_a_failed_spl_batch_without_reimbursing_its_requests() {
    for shared_destination in [false, true] {
        let (indices, _) = setup_submitted_spl_batch(shared_destination);
        let sent = read_state(|state| state.sent_spl_withdrawal_requests().clone());
        let mut fresh = sent.values().next().unwrap().request.terms.clone();
        fresh.create_destination_account = false;
        accept(200, 200, &fresh);
        record(
            EventType::SplitFailedSplWithdrawalBatch {
                signature: minter_signature(),
            },
            80,
        );

        let state = assert_replay_matches();
        assert_eq!(state.balance(), FUNDED_BALANCE - FEE_PER_SIGNATURE);
        assert_eq!(state.nonce_pool().num_free_accounts(), 1);
        assert!(state.submitted_transactions().is_empty());
        assert!(
            state
                .failed_transactions()
                .contains_key(&minter_signature())
        );
        assert!(state.sent_spl_withdrawal_requests().is_empty());
        assert!(state.failed_spl_withdrawal_requests().is_empty());
        assert!(state.pending_spl_withdrawal_reimbursements().is_empty());
        assert!(state.pending_spl_withdrawal_fee_reimbursements().is_empty());
        assert_eq!(
            state.spl_withdrawal_reserved_cost(&minter_signature()),
            None
        );
        assert_eq!(state.spl_treasury_balance(&[2; 32].into()), FUNDED_TOKENS);
        if !shared_destination {
            assert_eq!(state.spl_treasury_balance(&[5; 32].into()), 200_000_000);
        }
        assert_eq!(
            state.retrying_spl_withdrawal_requests().len(),
            indices.len()
        );
        for id in indices {
            let before = &sent[&id.into()];
            assert_eq!(
                state.retrying_spl_withdrawal_requests()[&id.into()],
                PendingSplWithdrawalRequest {
                    request: before.request.clone(),
                    created_at: before.created_at,
                }
            );
            assert_eq!(state.spl_withdrawal_status(id), WithdrawSplStatus::Pending);
        }
        assert_eq!(state.pending_spl_withdrawal_requests().len(), 1);
        assert!(
            state
                .pending_spl_withdrawal_requests()
                .contains_key(&200.into())
        );
    }
}

#[test]
fn should_reject_refunds_and_reused_burns_while_an_spl_withdrawal_is_retrying() {
    setup_submitted_spl_batch(false);
    let sent = read_state(|state| state.sent_spl_withdrawal_requests()[&100.into()].clone());
    record(
        EventType::SplitFailedSplWithdrawalBatch {
            signature: minter_signature(),
        },
        80,
    );
    let before = read_state(Clone::clone);
    for event in [
        EventType::AcceptedSplWithdrawalRequest(sent.request.clone()),
        EventType::AcceptedSplWithdrawalRequest(SplWithdrawalRequest {
            cksol_burn_block_index: 200.into(),
            ..sent.request.clone()
        }),
        EventType::FailedSplWithdrawalRequest(SplWithdrawalFeeReimbursementRequest {
            cksol_burn_block_index: 100.into(),
            reimbursed_amount: sent.request.terms.fee,
            to: sent.request.terms.account,
        }),
        EventType::SplitFailedSplWithdrawalBatch {
            signature: minter_signature(),
        },
        EventType::FailedTransaction {
            signature: minter_signature(),
        },
        EventType::SucceededSplWithdrawalTransaction {
            signature: minter_signature(),
            lamports_spent: FEE_PER_SIGNATURE,
        },
    ] {
        assert!(catch_unwind(AssertUnwindSafe(|| record(event, 90))).is_err());
        read_state(|state| assert_eq!(state, &before));
    }
    assert_replay_matches();
}

#[test]
fn should_reject_spl_withdrawal_splitting_without_a_submitted_batch() {
    for submitted in [false, true] {
        reset_state();
        reset_events();
        setup_created_spl_withdrawal(TokenProgram::Classic, false);
        if submitted {
            record(submitted_spl_withdrawal(100), 40);
        }
        let before = read_state(Clone::clone);
        assert!(
            catch_unwind(AssertUnwindSafe(|| record(
                EventType::SplitFailedSplWithdrawalBatch {
                    signature: minter_signature()
                },
                80
            )))
            .is_err()
        );
        read_state(|state| assert_eq!(state, &before));
        assert_replay_matches();
    }
}

#[test]
fn should_reject_spl_batch_spend_above_the_whole_transaction_reservation() {
    let (_, reserved) = setup_submitted_spl_batch(false);
    let before = read_state(Clone::clone);
    assert!(
        catch_unwind(AssertUnwindSafe(|| record(
            EventType::SucceededSplWithdrawalTransaction {
                signature: minter_signature(),
                lamports_spent: reserved + 1,
            },
            80
        )))
        .is_err()
    );
    read_state(|state| assert_eq!(state, &before));
    assert_replay_matches();
}

#[test]
fn should_reject_spl_batch_submission_that_does_not_match_the_bound_requests() {
    setup_created_spl_batch();
    for indices in [vec![], vec![100], vec![101, 100], vec![100, 101, 102]] {
        let before = read_state(Clone::clone);
        assert!(
            catch_unwind(AssertUnwindSafe(|| record(
                submitted_spl_withdrawals(&indices),
                70
            )))
            .is_err()
        );
        read_state(|state| assert_eq!(state, &before));
        assert_replay_matches();
    }
    for change in 0..2 {
        let before = read_state(Clone::clone);
        let mut event = submitted_spl_withdrawals(&[100, 101]);
        let EventType::SubmittedTransaction {
            message: VersionedMessage::Legacy(message),
            signers,
            ..
        } = &mut event
        else {
            unreachable!()
        };
        if change == 0 {
            message.instructions.last_mut().unwrap().data.push(0);
        } else {
            signers.push(Signer::Account(account(2)));
        }
        assert!(catch_unwind(AssertUnwindSafe(|| record(event, 70))).is_err());
        read_state(|state| assert_eq!(state, &before));
        assert_replay_matches();
    }
}

#[test]
fn should_reserve_one_signature_and_one_rent_per_destination_in_an_spl_batch() {
    let mut request = setup_with_nonce();
    for id in 100..105 {
        request.create_destination_account = id != 100;
        accept(id, id, &request);
    }
    let balance_before = read_state(|state| state.balance());
    record(spl_transaction_created(&[100, 101, 102, 103, 104]), 30);
    let state = assert_replay_matches();
    assert_eq!(
        state.balance(),
        balance_before - FEE_PER_SIGNATURE - TokenProgram::Classic.account_rent_exemption()
    );
    assert_eq!(
        state.spl_treasury_balance(&request.mint),
        FUNDED_TOKENS - 5 * request.amount
    );
    assert_eq!(state.created_spl_withdrawal_requests().len(), 5);
    assert_eq!(state.created_spl_withdrawal_txs().len(), 1);
    assert_eq!(bound_message(&state).instructions.len(), 7);
}

#[test]
fn should_reject_invalid_spl_batch_bindings_without_changing_state() {
    let request = setup_with_nonce();
    for id in 100..106 {
        accept(id, id, &request);
    }
    for indices in [
        vec![],
        vec![100, 100],
        vec![100, 101, 102, 103, 104, 105],
        vec![100, 106],
    ] {
        let before = read_state(Clone::clone);
        assert!(
            catch_unwind(AssertUnwindSafe(|| record(
                spl_transaction_created(&indices),
                30
            )))
            .is_err()
        );
        read_state(|state| assert_eq!(state, &before));
        assert_replay_matches();
    }
}

#[test]
fn should_reject_an_spl_batch_whose_combined_tokens_exceed_the_treasury() {
    let mut request = setup_with_nonce();
    request.amount = FUNDED_TOKENS / 2 + 1;
    accept(100, 7, &request);
    accept(101, 8, &request);
    let before = read_state(Clone::clone);
    assert!(
        catch_unwind(AssertUnwindSafe(|| record(
            spl_transaction_created(&[100, 101]),
            30
        )))
        .is_err()
    );
    read_state(|state| assert_eq!(state, &before));
    assert_replay_matches();
}

#[test]
fn should_plan_withdrawals_of_both_kinds_in_burn_index_order_within_one_budget() {
    let request = setup_with_nonce();
    let sol = WithdrawalRequest {
        account: request.account,
        solana_address: request.address.to_bytes(),
        burn_block_index: 50.into(),
        burned_amount: 20_000,
        amount_to_transfer: 10_000,
    };
    record(EventType::AcceptedWithdrawalRequest(sol.clone()), 15);
    accept(100, 7, &request);
    accept(101, 8, &request);
    let plans: Vec<_> = read_state(|state| state.withdrawal_plans().collect());
    assert_eq!(
        plans,
        vec![
            WithdrawalPlan::Sol(vec![sol.clone()]),
            WithdrawalPlan::Spl(accepted_request(100, 7, &request)),
            WithdrawalPlan::Spl(accepted_request(101, 8, &request)),
        ]
    );

    mutate_state(|state| {
        state.balance = RENT_EXEMPTION_THRESHOLD
            + sol.amount_to_transfer
            + BATCH_WITHDRAWAL_TX_FEE
            + FEE_PER_SIGNATURE
    });
    let plans: Vec<_> = read_state(|state| state.withdrawal_plans().collect());
    assert_eq!(
        plans,
        vec![
            WithdrawalPlan::Sol(vec![sol]),
            WithdrawalPlan::Spl(accepted_request(100, 7, &request)),
        ]
    );
}

#[test]
fn should_not_batch_a_sol_request_newer_than_a_waiting_spl_request() {
    let request = setup_with_nonce();
    let sol = |burn_block_index: u64| WithdrawalRequest {
        account: request.account,
        solana_address: request.address.to_bytes(),
        burn_block_index: burn_block_index.into(),
        burned_amount: 20_000,
        amount_to_transfer: 10_000,
    };
    record(EventType::AcceptedWithdrawalRequest(sol(90)), 10);
    accept(100, 7, &request);
    record(EventType::AcceptedWithdrawalRequest(sol(110)), 30);
    let plans: Vec<_> = read_state(|state| state.withdrawal_plans().collect());
    assert_eq!(
        plans,
        vec![
            WithdrawalPlan::Sol(vec![sol(90)]),
            WithdrawalPlan::Spl(accepted_request(100, 7, &request)),
            WithdrawalPlan::Sol(vec![sol(110)]),
        ]
    );

    // The budget covers the first SOL request and the SPL request, not the later SOL one.
    mutate_state(|state| {
        state.balance =
            RENT_EXEMPTION_THRESHOLD + 10_000 + BATCH_WITHDRAWAL_TX_FEE + FEE_PER_SIGNATURE
    });
    let plans: Vec<_> = read_state(|state| state.withdrawal_plans().collect());
    assert_eq!(
        plans,
        vec![
            WithdrawalPlan::Sol(vec![sol(90)]),
            WithdrawalPlan::Spl(accepted_request(100, 7, &request)),
        ]
    );
}

#[test]
fn should_credit_the_treasury_with_the_swept_token_balance() {
    let request = setup_with_nonce();
    let replayed = assert_replay_matches();
    assert_eq!(replayed.spl_treasury_balance(&request.mint), FUNDED_TOKENS);
    assert_eq!(replayed.spl_treasury_balance(&[4; 32].into()), 0);
}

#[test]
fn should_reserve_treasury_tokens_on_creation_and_return_them_on_failure() {
    let request = setup_created_spl_withdrawal(TokenProgram::Classic, false);
    assert_eq!(
        assert_replay_matches().spl_treasury_balance(&request.mint),
        FUNDED_TOKENS - request.amount
    );
    record(submitted_spl_withdrawal(100), 40);
    let replayed = finalize(EventType::FailedTransaction {
        signature: minter_signature(),
    });
    assert_eq!(replayed.spl_treasury_balance(&request.mint), FUNDED_TOKENS);
}

#[test]
fn should_keep_the_tokens_of_a_succeeded_spl_withdrawal_out_of_the_treasury() {
    let request = setup_created_spl_withdrawal(TokenProgram::Classic, false);
    record(submitted_spl_withdrawal(100), 40);
    let replayed = finalize(EventType::SucceededTransaction {
        signature: minter_signature(),
    });
    assert_eq!(
        replayed.spl_treasury_balance(&request.mint),
        FUNDED_TOKENS - request.amount
    );
}

#[test]
#[should_panic(expected = "insufficient treasury tokens for SPL withdrawal")]
fn should_reject_spl_binding_without_treasury_tokens() {
    let request = setup_with(init_args_with_nonce_account());
    record(minter_public_key_fetched_event(), 3);
    init_balance_to(FUNDED_BALANCE);
    accept(100, 7, &request);
    record(spl_transaction_created(&[100]), 30);
}

#[test]
fn should_skip_spl_plans_the_treasury_cannot_pay_and_keep_sol_going() {
    let request = setup_with_nonce();
    let mut unfunded = request.clone();
    unfunded.amount = FUNDED_TOKENS + 1;
    accept(100, 7, &unfunded);
    accept(101, 8, &request);
    let sol = WithdrawalRequest {
        account: request.account,
        solana_address: request.address.to_bytes(),
        burn_block_index: 200.into(),
        burned_amount: 20_000,
        amount_to_transfer: 10_000,
    };
    record(EventType::AcceptedWithdrawalRequest(sol.clone()), 15);
    let plans: Vec<_> = read_state(|state| state.withdrawal_plans().collect());
    assert_eq!(plans, vec![WithdrawalPlan::Sol(vec![sol])]);
}

#[test]
fn should_skip_sol_plans_the_balance_cannot_pay_and_keep_spl_going() {
    let request = setup_with_nonce();
    let sol = WithdrawalRequest {
        account: request.account,
        solana_address: request.address.to_bytes(),
        burn_block_index: 90.into(),
        burned_amount: FUNDED_BALANCE,
        amount_to_transfer: FUNDED_BALANCE,
    };
    record(EventType::AcceptedWithdrawalRequest(sol), 15);
    accept(100, 7, &request);
    let plans: Vec<_> = read_state(|state| state.withdrawal_plans().collect());
    assert_eq!(
        plans,
        vec![WithdrawalPlan::Spl(accepted_request(100, 7, &request))]
    );
}

#[test]
fn should_report_the_spl_withdrawal_status_through_the_transaction_stages() {
    let request = setup_created_spl_withdrawal(TokenProgram::Classic, false);
    let status = |id: u64| read_state(|state| state.spl_withdrawal_status(id));
    assert_eq!(status(100), WithdrawSplStatus::Pending);
    assert_eq!(status(101), WithdrawSplStatus::NotFound);
    accept(101, 8, &request);
    assert_eq!(status(101), WithdrawSplStatus::Pending);

    record(submitted_spl_withdrawal(100), 40);
    assert_eq!(
        status(100),
        WithdrawSplStatus::TxSent {
            transaction_id: minter_signature().into(),
        }
    );

    record(
        EventType::SucceededSplWithdrawalTransaction {
            signature: minter_signature(),
            lamports_spent: FEE_PER_SIGNATURE,
        },
        50,
    );
    assert_eq!(
        status(100),
        WithdrawSplStatus::TxFinalized(TxFinalizedStatus::Success {
            transaction_id: minter_signature().into(),
        })
    );
    assert_eq!(status(101), WithdrawSplStatus::Pending);
}

#[test]
fn should_report_a_pending_token_reimbursement_after_a_failed_spl_transaction() {
    setup_created_spl_withdrawal(TokenProgram::Classic, false);
    record(submitted_spl_withdrawal(100), 40);
    record(
        EventType::FailedTransaction {
            signature: minter_signature(),
        },
        50,
    );
    assert_eq!(
        read_state(|state| state.spl_withdrawal_status(100)),
        WithdrawSplStatus::ReimbursementPending {
            transaction_id: minter_signature().into(),
            reimbursed_amount: 1_000_000,
        }
    );
}

#[test]
fn should_replay_token_reimbursement_completion_and_quarantine() {
    for quarantine in [false, true] {
        reset_state();
        reset_events();
        let request = setup_created_spl_withdrawal(TokenProgram::Classic, true);
        record(submitted_spl_withdrawal(100), 40);
        record(
            EventType::FailedTransaction {
                signature: minter_signature(),
            },
            50,
        );
        let pending =
            assert_replay_matches().pending_spl_withdrawal_reimbursements()[&100.into()].clone();
        assert_eq!(pending.request, accepted_request(100, 7, &request));
        assert_eq!(pending.created_at, 50);
        let event = if quarantine {
            EventType::QuarantinedSplWithdrawalReimbursement {
                cksol_burn_block_index: 100.into(),
            }
        } else {
            EventType::ReimbursedSplWithdrawal {
                cksol_burn_block_index: 100.into(),
                mint_block_index: 200.into(),
            }
        };
        record(event.clone(), 60);
        assert!(catch_unwind(AssertUnwindSafe(|| record(event, 61))).is_err());
        let replayed = assert_replay_matches();
        assert!(replayed.pending_spl_withdrawal_reimbursements().is_empty());
        if quarantine {
            assert_eq!(
                replayed.quarantined_spl_withdrawal_reimbursements()[&100.into()],
                pending
            );
            assert_eq!(
                replayed.spl_withdrawal_status(100),
                WithdrawSplStatus::ReimbursementQuarantined {
                    transaction_id: minter_signature().into(),
                    reimbursed_amount: request.amount,
                }
            );
        } else {
            let completed = &replayed.completed_spl_withdrawal_reimbursements()[&100.into()];
            assert_eq!(completed.request, pending.request);
            assert_eq!(completed.created_at, 50);
            assert_eq!(completed.reimbursed_at, 60);
            assert_eq!(
                replayed.spl_withdrawal_status(100),
                WithdrawSplStatus::Reimbursed {
                    transaction_id: minter_signature().into(),
                    reimbursed_amount: request.amount,
                    mint_block_index: 200,
                }
            );
        }
    }
}

#[test]
fn should_report_the_fee_reimbursement_stages() {
    let request = setup();
    let status = |id: u64| read_state(|state| state.spl_withdrawal_status(id));
    for id in [100_u64, 101, 102] {
        reimburse_fee(id, request.fee, request.account);
        assert_eq!(
            status(id),
            WithdrawSplStatus::FeeReimbursementPending {
                reimbursed_amount: request.fee,
            }
        );
    }
    record(
        EventType::ReimbursedSplWithdrawalFee {
            cksol_burn_block_index: 100.into(),
            mint_block_index: 200.into(),
        },
        40,
    );
    record(
        EventType::QuarantinedSplWithdrawalFeeReimbursement {
            cksol_burn_block_index: 101.into(),
        },
        41,
    );
    assert_eq!(
        status(100),
        WithdrawSplStatus::FeeReimbursed {
            reimbursed_amount: request.fee,
            mint_block_index: 200,
        }
    );
    assert_eq!(
        status(101),
        WithdrawSplStatus::FeeReimbursementQuarantined {
            reimbursed_amount: request.fee,
        }
    );
    assert_eq!(
        status(102),
        WithdrawSplStatus::FeeReimbursementPending {
            reimbursed_amount: request.fee,
        }
    );
    assert_replay_matches();
}

#[test]
fn should_reserve_the_signature_fee_from_the_balance() {
    assert_reserves(false, FEE_PER_SIGNATURE);
}

#[test]
fn should_reserve_the_destination_rent_with_the_signature_fee() {
    assert_reserves(
        true,
        FEE_PER_SIGNATURE + TokenProgram::Classic.account_rent_exemption(),
    );
}

#[test]
fn should_reject_spl_binding_the_balance_cannot_pay() {
    let request = setup_with(init_args_with_nonce_account());
    accept(100, 7, &request);
    assert!(
        catch_unwind(AssertUnwindSafe(|| record(
            spl_transaction_created(&[100]),
            30
        )))
        .is_err()
    );
    let replayed = assert_replay_matches();
    assert_eq!(replayed.balance(), 0);
    assert!(replayed.created_spl_withdrawal_txs().is_empty());
    assert_eq!(replayed.nonce_pool().num_free_accounts(), 1);
}

#[test]
fn should_not_rebind_or_refund_a_created_spl_withdrawal() {
    let request = setup_with_nonce();
    accept(100, 7, &request);
    record(spl_transaction_created(&[100]), 30);
    for event in [
        spl_transaction_created(&[100]),
        EventType::AcceptedSplWithdrawalRequest(accepted_request(100, 8, &request)),
        EventType::FailedSplWithdrawalRequest(SplWithdrawalFeeReimbursementRequest {
            cksol_burn_block_index: 100.into(),
            reimbursed_amount: request.fee,
            to: request.account,
        }),
    ] {
        assert!(catch_unwind(AssertUnwindSafe(|| record(event, 40))).is_err());
        assert_eq!(
            assert_replay_matches().created_spl_withdrawal_txs().len(),
            1
        );
    }
}

#[test]
fn should_reject_spl_binding_without_pending_request_or_available_nonce() {
    let request = setup_with_nonce();
    assert!(
        catch_unwind(AssertUnwindSafe(|| record(
            spl_transaction_created(&[100]),
            30
        )))
        .is_err()
    );
    assert_eq!(assert_replay_matches().nonce_pool().num_free_accounts(), 1);
    accept(100, 7, &request);
    accept(101, 8, &request);
    record(spl_transaction_created(&[100]), 30);
    assert!(
        catch_unwind(AssertUnwindSafe(|| record(
            spl_transaction_created(&[101]),
            40
        )))
        .is_err()
    );
    let replayed = assert_replay_matches();
    assert!(
        replayed
            .pending_spl_withdrawal_requests()
            .contains_key(&101.into())
    );
    assert_eq!(replayed.created_spl_withdrawal_txs().len(), 1);
}

#[test]
fn should_not_bind_a_sol_transaction_to_an_spl_nonce() {
    let request = setup_with_nonce();
    accept(100, 7, &request);
    record(spl_transaction_created(&[100]), 30);
    record(
        EventType::AcceptedWithdrawalRequest(WithdrawalRequest {
            account: request.account,
            solana_address: request.address.to_bytes(),
            burn_block_index: 200.into(),
            burned_amount: 20_000,
            amount_to_transfer: 10_000,
        }),
        31,
    );
    assert!(
        catch_unwind(AssertUnwindSafe(|| record(
            EventType::CreatedWithdrawalTransaction {
                burn_indices: vec![200.into()],
                nonce_account: NONCE_ACCOUNT,
                nonce_value: Hash::new_from_array([19; 32]),
            },
            40,
        )))
        .is_err()
    );
    let replayed = assert_replay_matches();
    assert!(
        replayed
            .pending_withdrawal_requests()
            .contains_key(&200.into())
    );
    assert_eq!(replayed.created_spl_withdrawal_txs().len(), 1);
    assert!(replayed.created_withdrawal_txs().is_empty());
}

#[test]
fn should_replay_an_accepted_withdrawal_into_pending() {
    let request = setup();
    accept(100, 7, &request);
    let replayed = assert_replay_matches();
    assert_eq!(
        replayed
            .pending_spl_withdrawal_requests()
            .get(&LedgerBurnIndex::new(100)),
        Some(&PendingSplWithdrawalRequest {
            request: accepted_request(100, 7, &request),
            created_at: 20,
        })
    );
    assert!(
        replayed
            .pending_spl_withdrawal_fee_reimbursements()
            .is_empty()
    );
}

#[test]
fn should_reject_a_zero_fee_reimbursement() {
    let request = setup();
    assert!(catch_unwind(AssertUnwindSafe(|| reimburse_fee(100, 0, request.account))).is_err());
    assert!(
        assert_replay_matches()
            .pending_spl_withdrawal_fee_reimbursements()
            .is_empty()
    );
}

#[test]
fn should_reject_invalid_terms_on_acceptance() {
    let request = setup();
    let changes: [fn(&mut SplWithdrawalRequestTerms); 4] = [
        |r| r.mint = [3; 32].into(),
        |r| r.ledger_id = Principal::from_slice(&[99]),
        |r| r.amount = 0,
        |r| r.fee = 0,
    ];
    for change in changes {
        let mut changed = request.clone();
        change(&mut changed);
        assert!(catch_unwind(AssertUnwindSafe(|| accept(100, 7, &changed))).is_err());
        assert!(
            assert_replay_matches()
                .pending_spl_withdrawal_requests()
                .is_empty()
        );
    }
}

#[test]
fn should_replay_an_older_log_without_spl_withdrawals() {
    setup();
    let replayed = assert_replay_matches();
    assert!(replayed.pending_spl_withdrawal_requests().is_empty());
    assert!(
        replayed
            .pending_spl_withdrawal_fee_reimbursements()
            .is_empty()
    );
}

#[test]
fn should_replay_a_full_or_partial_fee_reimbursement_to_the_original_subaccount() {
    let mut request = setup();
    request.account.subaccount = Some([42; 32]);
    for (index, refund) in [1, 999_990, 1_000_000].into_iter().enumerate() {
        let burn_index = 100 + index as u64;
        reimburse_fee(burn_index, refund, request.account);

        let replayed = assert_replay_matches();
        assert!(replayed.pending_spl_withdrawal_requests().is_empty());
        assert_eq!(
            replayed.pending_spl_withdrawal_fee_reimbursements().len(),
            index + 1
        );
        assert_eq!(
            replayed
                .pending_spl_withdrawal_fee_reimbursements()
                .get(&burn_index.into()),
            Some(&PendingSplWithdrawalFeeReimbursement {
                request: SplWithdrawalFeeReimbursementRequest {
                    cksol_burn_block_index: burn_index.into(),
                    reimbursed_amount: refund,
                    to: request.account,
                },
                created_at: 30,
            }),
        );
    }
}

#[test]
fn should_not_reimburse_an_accepted_withdrawal() {
    let request = setup();
    accept(100, 7, &request);
    assert!(
        catch_unwind(AssertUnwindSafe(|| reimburse_fee(
            100,
            request.fee,
            request.account
        )))
        .is_err()
    );
    let replayed = assert_replay_matches();
    assert_eq!(replayed.pending_spl_withdrawal_requests().len(), 1);
    assert!(
        replayed
            .pending_spl_withdrawal_fee_reimbursements()
            .is_empty()
    );
}

#[test]
fn should_not_accept_a_withdrawal_scheduled_for_fee_reimbursement() {
    let request = setup();
    reimburse_fee(100, request.fee, request.account);
    assert!(catch_unwind(AssertUnwindSafe(|| accept(100, 7, &request))).is_err());
    let replayed = assert_replay_matches();
    assert!(replayed.pending_spl_withdrawal_requests().is_empty());
    assert_eq!(
        replayed.pending_spl_withdrawal_fee_reimbursements().len(),
        1
    );
}

#[test]
fn should_not_replace_an_existing_fee_reimbursement() {
    let request = setup();
    reimburse_fee(100, request.fee, request.account);
    assert!(
        catch_unwind(AssertUnwindSafe(|| reimburse_fee(
            100,
            request.fee - 1,
            request.account
        )))
        .is_err()
    );
    let replayed = assert_replay_matches();
    assert_eq!(
        replayed.pending_spl_withdrawal_fee_reimbursements()[&100.into()]
            .request
            .reimbursed_amount,
        request.fee
    );
}

#[test]
fn should_only_move_the_failed_withdrawal_into_the_reimbursement_queue() {
    let request = setup();
    reimburse_fee(100, request.fee, request.account);
    accept(101, 7, &request);
    let replayed = assert_replay_matches();
    assert_eq!(replayed.pending_spl_withdrawal_requests().len(), 1);
    assert!(
        replayed
            .pending_spl_withdrawal_requests()
            .contains_key(&101.into())
    );
    assert_eq!(
        replayed.pending_spl_withdrawal_fee_reimbursements().len(),
        1
    );
    assert!(
        replayed
            .pending_spl_withdrawal_fee_reimbursements()
            .contains_key(&100.into())
    );
}

#[test]
fn should_preserve_the_existing_top_up_event_encoding() {
    let bytes = hex::decode("8207821818820b0d").unwrap();
    let event = Event::from_bytes(bytes.as_slice().into());
    assert_eq!(
        event,
        Event {
            timestamp: 7,
            payload: EventType::ToppedUpOperatingBalance {
                burn_block_index: LedgerBurnIndex::new(11),
                amount: 13,
            },
        }
    );
    assert_eq!(event.to_bytes().as_ref(), bytes.as_slice());
}

#[test]
#[should_panic(expected = "already accepted SPL withdrawal request")]
fn should_reject_accepting_a_withdrawal_twice() {
    let request = setup();
    accept(100, 7, &request);
    accept(100, 8, &request);
}

#[test]
#[should_panic(expected = "already belongs to a withdrawal")]
fn should_reject_reusing_a_token_burn_on_the_same_ledger() {
    let request = setup();
    accept(100, 7, &request);
    accept(101, 7, &request);
}

#[test]
fn should_keep_equal_token_burn_indexes_on_different_ledgers_separate() {
    let first = setup();
    let second = request(3);
    record(
        EventType::AddedSplToken(spl_token(second.mint, TokenProgram::Classic)),
        3,
    );
    accept(100, 7, &first);
    accept(101, 7, &second);
    let replayed = assert_replay_matches();
    assert_eq!(replayed.pending_spl_withdrawal_requests().len(), 2);
    assert_eq!(
        replayed.pending_spl_withdrawal_requests()[&LedgerBurnIndex::new(100)].request,
        accepted_request(100, 7, &first)
    );
    assert_eq!(
        replayed.pending_spl_withdrawal_requests()[&LedgerBurnIndex::new(101)].request,
        accepted_request(101, 7, &second)
    );
}
