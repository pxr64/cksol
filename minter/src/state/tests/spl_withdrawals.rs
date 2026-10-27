use crate::{
    lifecycle,
    numeric::LedgerBurnIndex,
    state::{
        FeeBurnedSplWithdrawalRequest, PendingSplWithdrawalFeeReimbursement,
        PendingSplWithdrawalRequest, State, TokenProgram,
        audit::{process_event, replay_events},
        event::{
            Event, EventType, SplWithdrawalFeeReimbursementRequest, SplWithdrawalRequest,
            SplWithdrawalRequestTerms,
        },
        mutate_state, read_state,
    },
    storage::with_event_iter,
    test_fixtures::{account, runtime::TestCanisterRuntime, spl_token, valid_init_args},
};
use candid::Principal;
use ic_stable_structures::Storable;
use std::panic::{AssertUnwindSafe, catch_unwind};

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
    mutate_state(|state| process_event(state, payload, &runtime));
}

fn setup() -> SplWithdrawalRequestTerms {
    lifecycle::init(valid_init_args(), TestCanisterRuntime::new().with_time(1));
    let request = request(2);
    record(
        EventType::AddedSplToken(spl_token(request.mint, TokenProgram::Classic)),
        2,
    );
    request
}

fn record_fee_burn(id: u64, request: &SplWithdrawalRequestTerms) {
    record(
        EventType::BurnedCkSolWithdrawalFee {
            cksol_burn_block_index: id.into(),
            request: request.clone(),
        },
        10,
    );
}

fn accepted_request(
    id: u64,
    token_burn: u64,
    terms: &SplWithdrawalRequestTerms,
) -> SplWithdrawalRequest {
    SplWithdrawalRequest {
        account: terms.account,
        mint: terms.mint,
        ledger_id: terms.ledger_id,
        address: terms.address,
        amount: terms.amount,
        fee: terms.fee,
        create_destination_account: terms.create_destination_account,
        cksol_burn_block_index: id.into(),
        ckspl_burn_block_index: token_burn.into(),
    }
}

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
fn should_replay_the_fee_checkpoint_then_accept_directly_into_pending() {
    let request = setup();
    let id = LedgerBurnIndex::new(100);
    record_fee_burn(100, &request);
    let replayed = assert_replay_matches();
    assert_eq!(
        replayed.fee_burned_spl_withdrawal_requests().get(&id),
        Some(&FeeBurnedSplWithdrawalRequest {
            request: request.clone(),
            created_at: 10,
        })
    );
    assert!(replayed.pending_spl_withdrawal_requests().is_empty());

    accept(100, 7, &request);
    let replayed = assert_replay_matches();
    assert!(replayed.fee_burned_spl_withdrawal_requests().is_empty());
    assert_eq!(
        replayed.pending_spl_withdrawal_requests().get(&id),
        Some(&PendingSplWithdrawalRequest {
            request: accepted_request(100, 7, &request),
            fee_burned_at: 10,
            created_at: 20,
        })
    );
}

#[test]
fn should_replay_an_older_log_without_spl_withdrawals() {
    setup();
    let replayed = assert_replay_matches();
    assert!(replayed.fee_burned_spl_withdrawal_requests().is_empty());
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
        record_fee_burn(burn_index, &request);
        reimburse_fee(burn_index, refund, request.account);

        let replayed = assert_replay_matches();
        assert!(replayed.fee_burned_spl_withdrawal_requests().is_empty());
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
                fee_burned_at: 10,
                created_at: 30,
            }),
        );
    }
}

#[test]
fn should_reject_invalid_reimbursements_without_consuming_the_fee_checkpoint() {
    let mut request = setup();
    request.account.subaccount = Some([42; 32]);
    record_fee_burn(100, &request);
    let mut changed_subaccount = request.account;
    changed_subaccount.subaccount = Some([43; 32]);
    for (amount, to) in [
        (0, request.account),
        (request.fee + 1, request.account),
        (request.fee, account(2)),
        (request.fee, changed_subaccount),
    ] {
        assert!(catch_unwind(AssertUnwindSafe(|| reimburse_fee(100, amount, to))).is_err());
        let replayed = assert_replay_matches();
        assert_eq!(
            replayed.fee_burned_spl_withdrawal_requests()[&100.into()].request,
            request
        );
        assert!(
            replayed
                .pending_spl_withdrawal_fee_reimbursements()
                .is_empty()
        );
    }
}

#[test]
fn should_reject_a_reimbursement_without_a_fee_burn() {
    let request = setup();
    assert!(
        catch_unwind(AssertUnwindSafe(|| reimburse_fee(
            100,
            request.fee,
            request.account
        )))
        .is_err()
    );
    let replayed = assert_replay_matches();
    assert!(
        replayed
            .pending_spl_withdrawal_fee_reimbursements()
            .is_empty()
    );
}

#[test]
fn should_not_reimburse_an_accepted_withdrawal() {
    let request = setup();
    record_fee_burn(100, &request);
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
    record_fee_burn(100, &request);
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
    record_fee_burn(100, &request);
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
fn should_not_reuse_a_fee_burn_scheduled_for_reimbursement() {
    let request = setup();
    record_fee_burn(100, &request);
    reimburse_fee(100, request.fee, request.account);
    assert!(catch_unwind(AssertUnwindSafe(|| record_fee_burn(100, &request))).is_err());
    let replayed = assert_replay_matches();
    assert!(replayed.fee_burned_spl_withdrawal_requests().is_empty());
    assert_eq!(
        replayed.pending_spl_withdrawal_fee_reimbursements().len(),
        1
    );
}

#[test]
fn should_only_move_the_failed_withdrawal_into_the_reimbursement_queue() {
    let request = setup();
    record_fee_burn(100, &request);
    record_fee_burn(101, &request);
    reimburse_fee(100, request.fee, request.account);
    accept(101, 7, &request);
    let replayed = assert_replay_matches();
    assert!(replayed.fee_burned_spl_withdrawal_requests().is_empty());
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
#[should_panic(expected = "already has a fee burn")]
fn should_reject_a_duplicate_fee_burn() {
    let request = setup();
    record_fee_burn(100, &request);
    record_fee_burn(100, &request);
}

#[test]
#[should_panic(expected = "already has a fee burn")]
fn should_reject_reusing_a_fee_burn_after_acceptance() {
    let request = setup();
    record_fee_burn(100, &request);
    accept(100, 7, &request);
    record_fee_burn(100, &request);
}

#[test]
#[should_panic(expected = "requires a recorded fee burn")]
fn should_reject_acceptance_without_a_fee_burn() {
    let request = setup();
    accept(100, 7, &request);
}

#[test]
#[should_panic(expected = "is already accepted")]
fn should_reject_accepting_a_withdrawal_twice() {
    let request = setup();
    record_fee_burn(100, &request);
    accept(100, 7, &request);
    accept(100, 8, &request);
}

#[test]
#[should_panic(expected = "already belongs to a withdrawal")]
fn should_reject_reusing_a_token_burn_on_the_same_ledger() {
    let request = setup();
    record_fee_burn(100, &request);
    record_fee_burn(101, &request);
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
    record_fee_burn(100, &first);
    record_fee_burn(101, &second);
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

#[test]
fn should_reject_changed_terms_without_consuming_the_fee_checkpoint() {
    let request = setup();
    record_fee_burn(100, &request);
    let changes: [fn(&mut SplWithdrawalRequestTerms); 7] = [
        |r| r.account = account(2),
        |r| r.mint = [3; 32].into(),
        |r| r.ledger_id = Principal::from_slice(&[99]),
        |r| r.address = [8; 32].into(),
        |r| r.amount += 1,
        |r| r.fee += 1,
        |r| r.create_destination_account = true,
    ];
    for change in changes {
        let mut changed = request.clone();
        change(&mut changed);
        assert!(catch_unwind(AssertUnwindSafe(|| accept(100, 7, &changed))).is_err());
        let replayed = assert_replay_matches();
        assert_eq!(
            replayed.fee_burned_spl_withdrawal_requests()[&LedgerBurnIndex::new(100)].request,
            request
        );
        assert!(replayed.pending_spl_withdrawal_requests().is_empty());
    }
}

#[test]
#[should_panic(expected = "mint's registered ledger")]
fn should_reject_a_fee_burn_record_for_the_wrong_token_ledger() {
    let mut request = setup();
    request.ledger_id = Principal::from_slice(&[99]);
    record_fee_burn(100, &request);
}
