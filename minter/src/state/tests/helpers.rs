use crate::withdraw::round::can_create_withdrawal_transaction;
use crate::withdraw::spl::fees::{spl_withdrawal_reserved_cost, spl_withdrawal_transaction_cost};
use crate::{
    constants::FEE_PER_SIGNATURE,
    lifecycle,
    numeric::LedgerBurnIndex,
    state::{
        PendingSplWithdrawalRequest, State, TokenProgram,
        audit::{process_event, replay_events},
        event::{
            Event, EventType, Signer, SplWithdrawalFeeReimbursementRequest, SplWithdrawalRequest,
            SplWithdrawalRequestTerms, TransactionPurpose,
        },
        mutate_state, read_state, reset_state,
    },
    storage::{reset_events, with_event_iter},
    test_fixtures::{
        NONCE_ACCOUNT, account, events, init_args_with_nonce_account, init_balance_to,
        minter_public_key_fetched_event, minter_signature, runtime::TestCanisterRuntime, spl_token,
        valid_init_args,
    },
};
use cksol_types::{TxFinalizedStatus, WithdrawSplStatus};
use cksol_types_internal::InitArgs;
use ic_stable_structures::Storable;
use sol_rpc_types::Lamport;
use solana_hash::Hash;
use solana_signature::Signature;
use solana_transaction::Message;
use std::panic::{AssertUnwindSafe, catch_unwind};

pub(crate) const FUNDED_BALANCE: Lamport = 500_000_000;
pub(crate) const FUNDED_TOKENS: u64 = 10_000_000;

pub(crate) fn request(mint_byte: u8) -> SplWithdrawalRequestTerms {
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

pub(crate) fn record(payload: EventType, timestamp: u64) {
    let runtime = TestCanisterRuntime::new().with_time(timestamp);
    mutate_state(|state| {
        // Failed IC message execution rolls state changes back. Apply to a copy
        // so caught assertion failures in these tests model that behavior.
        let mut updated = state.clone();
        process_event(&mut updated, payload, &runtime);
        *state = updated;
    });
}

pub(crate) fn setup() -> SplWithdrawalRequestTerms {
    setup_with(valid_init_args())
}

pub(crate) fn setup_with(init_args: InitArgs) -> SplWithdrawalRequestTerms {
    lifecycle::init(init_args, TestCanisterRuntime::new().with_time(1));
    let request = request(2);
    record(
        EventType::AddedSplToken(spl_token(request.mint, TokenProgram::Classic)),
        2,
    );
    request
}

pub(crate) fn accepted_request(
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

pub(crate) fn spl_transaction_created(burn_indices: &[u64]) -> EventType {
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
pub(crate) fn bound_message(state: &State) -> Message {
    let created = &state.created_spl_withdrawal_txs()[&NONCE_ACCOUNT];
    state
        .bound_spl_withdrawal_message(&NONCE_ACCOUNT, created)
        .unwrap()
}

pub(crate) fn submitted_spl_withdrawal(id: u64) -> EventType {
    submitted_spl_withdrawals(&[id])
}

pub(crate) fn submitted_spl_withdrawals(burn_indices: &[u64]) -> EventType {
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

pub(crate) fn setup_created_spl_withdrawal(
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

pub(crate) fn setup_pending_spl_batch() -> [SplWithdrawalRequestTerms; 2] {
    let mut first = setup_with_nonce();
    first.create_destination_account = true;
    first.fee += TokenProgram::Classic.account_rent_exemption();
    accept(100, 7, &first);
    let mut token = spl_token([5; 32].into(), TokenProgram::Token2022);
    token.decimals = 9;
    record(EventType::AddedSplToken(token.clone()), 40);
    events::fund_spl_treasury(&token, 200_000_000);
    let second = SplWithdrawalRequestTerms {
        mint: token.mint,
        ledger_id: token.ledger_id,
        amount: 123_456_789,
        fee: request(5).fee + TokenProgram::Token2022.account_rent_exemption(),
        ..first.clone()
    };
    record(
        EventType::AcceptedSplWithdrawalRequest(accepted_request(101, 8, &second)),
        50,
    );

    [first, second]
}

pub(crate) fn setup_created_spl_batch() -> [SplWithdrawalRequestTerms; 2] {
    let requests = setup_pending_spl_batch();
    record(spl_transaction_created(&[100, 101]), 60);
    requests
}

/// Submits a batch and returns its withdrawal IDs (ckSOL fee-burn indices) and reserved SOL.
/// `shared_destination` selects five requests sharing one ATA; otherwise the batch contains
/// two requests for different mints, one classic SPL and one Token-2022.
pub(crate) fn setup_submitted_spl_batch(shared_destination: bool) -> (Vec<u64>, Lamport) {
    reset_state();
    reset_events();
    let (indices, reserved) = if shared_destination {
        let mut request = setup_with_nonce();
        // All five requests use the same mint and destination wallet, so they share an ATA.
        for id in 100..105 {
            // The first request needs no creation instruction. The other four request
            // creation of the same ATA, whose rent must be reserved only once.
            request.create_destination_account = id != 100;
            request.fee = 1_000_000
                + if request.create_destination_account {
                    TokenProgram::Classic.account_rent_exemption()
                } else {
                    0
                };
            // Use `id` for both ledger burn indices just to keep this fixture simple.
            accept(id, id, &request);
        }
        let indices = vec![100, 101, 102, 103, 104];
        record(spl_transaction_created(&indices), 60);
        (
            indices,
            FEE_PER_SIGNATURE + TokenProgram::Classic.account_rent_exemption(),
        )
    } else {
        // ID 100 withdraws classic SPL; ID 101 withdraws Token-2022.
        // Both request ATA creation for their distinct mints, so reserve both rents.
        setup_created_spl_batch();
        (
            vec![100, 101],
            FEE_PER_SIGNATURE
                + TokenProgram::Classic.account_rent_exemption()
                + TokenProgram::Token2022.account_rent_exemption(),
        )
    };
    // Both setups have bound their requests to a nonce. Submit them under one signature.
    record(submitted_spl_withdrawals(&indices), 70);
    (indices, reserved)
}

/// Fails the submitted batch and returns the original requests now awaiting individual retries.
pub(crate) fn setup_spl_withdrawal_retries(shared_destination: bool) -> Vec<SplWithdrawalRequest> {
    setup_submitted_spl_batch(shared_destination);
    record(
        EventType::SplitFailedSplWithdrawalBatch {
            signature: minter_signature(),
        },
        80,
    );
    let state = assert_replay_matches();
    assert!(can_create_withdrawal_transaction(&state));
    state
        .retrying_spl_withdrawal_requests()
        .values()
        .map(|pending| pending.request.clone())
        .collect()
}

/// Two unsubmitted requests with registered classic SPL and Token-2022 mints.
pub(crate) fn setup_spl_batch_budget_requests() -> Vec<SplWithdrawalRequest> {
    let classic = setup();
    let token = spl_token([5; 32].into(), TokenProgram::Token2022);
    record(EventType::AddedSplToken(token.clone()), 10);
    let token_2022 = SplWithdrawalRequestTerms {
        mint: token.mint,
        ledger_id: token.ledger_id,
        ..classic.clone()
    };
    vec![
        accepted_request(100, 7, &classic),
        accepted_request(101, 8, &token_2022),
    ]
}

/// Records the terminal event once, rejects a repeat, and checks the shared outcome:
/// the request left the sent map and the nonce account is free again.
pub(crate) fn finalize(terminal: EventType) -> State {
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
pub(crate) fn setup_with_nonce() -> SplWithdrawalRequestTerms {
    let request = setup_with(init_args_with_nonce_account());
    record(minter_public_key_fetched_event(), 3);
    init_balance_to(FUNDED_BALANCE);
    events::fund_spl_treasury(
        &spl_token(request.mint, TokenProgram::Classic),
        FUNDED_TOKENS,
    );
    request
}

pub(crate) fn assert_reserves(create_destination_account: bool, cost: Lamport) {
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
pub(crate) fn accept(id: u64, token_burn: u64, terms: &SplWithdrawalRequestTerms) {
    record(
        EventType::AcceptedSplWithdrawalRequest(accepted_request(id, token_burn, terms)),
        20,
    );
}

pub(crate) fn reimburse_fee(id: u64, amount: u64, to: icrc_ledger_types::icrc1::account::Account) {
    record(
        EventType::FailedSplWithdrawalRequest(SplWithdrawalFeeReimbursementRequest {
            cksol_burn_block_index: id.into(),
            reimbursed_amount: amount,
            to,
        }),
        30,
    );
}

pub(crate) fn assert_replay_matches() -> State {
    let events = with_event_iter(|events| {
        events
            .map(|event| Event::from_bytes(event.to_bytes()))
            .collect::<Vec<_>>()
    });
    let replayed = replay_events(events);
    read_state(|state| assert_eq!(state, &replayed));
    replayed
}

/// Takes one original request from the retry queue and reserves a transaction for it.
pub(crate) fn create_individual_spl_retry(
    id: u64,
    nonce_value: Hash,
    created_at: u64,
) -> PendingSplWithdrawalRequest {
    let before = read_state(Clone::clone);
    let pending = before.retrying_spl_withdrawal_requests()[&id.into()].clone();
    let terms = &pending.request.terms;
    record(
        EventType::CreatedSplWithdrawalTransaction {
            burn_indices: vec![id.into()],
            nonce_account: NONCE_ACCOUNT,
            nonce_value,
        },
        created_at,
    );
    // Replay the encoded history after each transition, as on an upgrade.
    let created = assert_replay_matches();
    assert_eq!(
        created.created_spl_withdrawal_requests()[&id.into()],
        pending
    );
    assert_eq!(
        created.created_spl_withdrawal_txs()[&NONCE_ACCOUNT].burn_indices,
        vec![id.into()]
    );
    assert_eq!(bound_message(&created).recent_blockhash, nonce_value);
    // Retry SOL is prepaid by the failed batch; binding does not charge free SOL again.
    assert_eq!(created.balance(), before.balance());
    assert_eq!(
        created.spl_treasury_balance(&terms.mint),
        before.spl_treasury_balance(&terms.mint) - terms.amount
    );
    assert_eq!(created.nonce_pool().num_free_accounts(), 0);
    assert!(
        !created
            .retrying_spl_withdrawal_requests()
            .contains_key(&id.into())
    );
    pending
}

/// Submits the retry with its own signature, preserving the original burns and acceptance time.
pub(crate) fn submit_individual_spl_retry(
    pending: &PendingSplWithdrawalRequest,
    signature: Signature,
    submitted_at: u64,
) {
    let id = *pending.request.cksol_burn_block_index.get();
    let mut submitted = submitted_spl_withdrawal(id);
    let EventType::SubmittedTransaction {
        signature: submitted_signature,
        ..
    } = &mut submitted
    else {
        unreachable!();
    };
    *submitted_signature = signature;
    record(submitted, submitted_at);
    let sent = assert_replay_matches();
    assert_eq!(
        sent.sent_spl_withdrawal_requests()[&id.into()].request,
        pending.request
    );
    assert_eq!(
        sent.sent_spl_withdrawal_requests()[&id.into()].created_at,
        pending.created_at
    );
    let reserved = spl_withdrawal_transaction_cost(&sent, std::iter::once(&pending.request.terms));
    assert_eq!(
        spl_withdrawal_reserved_cost(&sent, &signature),
        Some(reserved)
    );

    // An individual retry cannot be split again into further retries.
    assert!(
        catch_unwind(AssertUnwindSafe(|| record(
            EventType::SplitFailedSplWithdrawalBatch { signature },
            submitted_at + 5
        )))
        .is_err()
    );
    read_state(|state| assert_eq!(state, &sent));
}

/// Checks that success completes the request, while terminal failure queues its full reimbursement.
pub(crate) fn finalize_individual_spl_retry(
    pending: &PendingSplWithdrawalRequest,
    signature: Signature,
    succeeds: bool,
    finalized_at: u64,
) {
    let id = *pending.request.cksol_burn_block_index.get();
    let terms = &pending.request.terms;
    let before = read_state(Clone::clone);
    let reserved = spl_withdrawal_reserved_cost(&before, &signature).unwrap();
    record(
        if succeeds {
            // Model a success where the destination ATA already exists, so only the fee is spent.
            EventType::SucceededSplWithdrawalTransaction {
                signature,
                lamports_spent: FEE_PER_SIGNATURE,
            }
        } else {
            EventType::FailedTransaction { signature }
        },
        finalized_at,
    );
    let finalized = assert_replay_matches();
    // Both outcomes release unused rent and keep the transaction fee deducted.
    assert_eq!(
        finalized.balance(),
        before.balance() + reserved - FEE_PER_SIGNATURE
    );
    assert_eq!(finalized.nonce_pool().num_free_accounts(), 1);
    assert!(finalized.sent_spl_withdrawal_requests().is_empty());
    if succeeds {
        // Delivered tokens stay deducted from the treasury; no reimbursement is queued.
        assert_eq!(
            finalized.spl_withdrawal_status(id),
            WithdrawSplStatus::TxFinalized(TxFinalizedStatus::Success {
                transaction_id: signature.into()
            })
        );
        assert!(
            !finalized
                .pending_spl_withdrawal_reimbursements()
                .contains_key(&id.into())
        );
        assert_eq!(
            finalized.spl_treasury_balance(&terms.mint),
            before.spl_treasury_balance(&terms.mint)
        );
    } else {
        // Terminal failure restores the tokens and queues their full ckSPL reimbursement.
        let reimbursement = &finalized.pending_spl_withdrawal_reimbursements()[&id.into()];
        assert_eq!(reimbursement.request, pending.request);
        assert_eq!(reimbursement.created_at, finalized_at);
        assert_eq!(
            finalized.spl_withdrawal_status(id),
            WithdrawSplStatus::ReimbursementPending {
                transaction_id: signature.into(),
                reimbursed_amount: terms.amount
            }
        );
        assert_eq!(
            finalized.spl_treasury_balance(&terms.mint),
            before.spl_treasury_balance(&terms.mint) + terms.amount
        );
    }
}

pub(crate) fn set_balance(state: &mut State, balance: Lamport) {
    state.balance = balance;
}

pub(crate) fn set_spl_treasury_balance(
    state: &mut State,
    mint: solana_address::Address,
    amount: u64,
) {
    state.spl_treasury_balances.insert(mint, amount);
}

pub(crate) fn nonce_pool_mut(state: &mut State) -> &mut crate::state::DurableNoncePool {
    &mut state.nonce_pool
}

pub(crate) fn accept_sol_withdrawal(
    state: &mut State,
    request: &crate::state::event::WithdrawalRequest,
    created_at: u64,
) {
    state.process_accepted_withdrawal(request, created_at);
}

pub(crate) fn bind_nonce(state: &mut State, address: &solana_address::Address, nonce_value: Hash) {
    state.nonce_pool.bind(address, nonce_value);
}
