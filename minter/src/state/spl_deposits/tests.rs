use super::*;
use crate::{
    constants::FEE_PER_SIGNATURE,
    lifecycle,
    state::{
        State, TokenProgram,
        audit::{process_event, replay_events},
        event::{Event, EventType, Signer, TransactionPurpose},
        mutate_state, read_state, reset_state,
    },
    storage::{reset_events, with_event_iter},
    test_fixtures::{
        DEFAULT_BLOCK_HEIGHT, account, classic_spl_tokens, planned_spl_sweep, planned_sweep,
        queued_deposit_of, queued_spl_deposit, runtime::TestCanisterRuntime, schnorr_master_key,
        signature, spl_token, valid_init_args,
    },
};
use ic_stable_structures::Storable;
use solana_hash::Hash;
use solana_transaction::Message;

fn deposit(account: Account, mint: Address) -> QueuedSplDeposit {
    queued_spl_deposit(account, mint, TokenProgram::Classic)
}

/// Placeholders for a sweep that the id asserts reject before recovery.
fn no_plan() -> (
    VersionedMessage,
    Vec<Signer>,
    BTreeMap<Address, SupportedSplToken>,
) {
    (Message::default().into(), vec![], BTreeMap::new())
}

/// Sweeps the queued deposits with the given ids under a plan recovered from their message.
fn sweep(deposits: &mut SplDeposits, deposit_ids: &[u64], signature: &Signature) {
    let selected: BTreeMap<_, _> = deposit_ids
        .iter()
        .filter_map(|id| Some((*id, deposits.queued().get(id)?.clone())))
        .collect();
    let tokens = classic_spl_tokens(selected.values());
    let plan = planned_spl_sweep(selected);
    let message = plan.sweep_message(Hash::default());
    let signers = plan.signers(&message);
    deposits.sweep(deposit_ids, &message.into(), &signers, &tokens, signature);
}

fn submitted_spl_sweep() -> (TestCanisterRuntime, Signature, SplSweep) {
    let runtime = TestCanisterRuntime::new().with_increasing_time();
    lifecycle::init(valid_init_args(), runtime.clone());
    let mut tokens = BTreeMap::new();
    let mut queued = Vec::new();
    for (deposit_id, token_program) in [TokenProgram::Classic, TokenProgram::Token2022]
        .into_iter()
        .enumerate()
    {
        let token = spl_token([deposit_id as u8 + 1; 32].into(), token_program);
        let deposit = queued_spl_deposit(account(1), token.mint, token_program);
        mutate_state(|state| {
            process_event(state, EventType::AddedSplToken(token.clone()), &runtime);
            process_event(
                state,
                EventType::QueuedSplDeposit {
                    deposit_id: deposit_id as u64,
                    account: deposit.account,
                    mint: deposit.mint,
                    address: deposit.address,
                    balance: deposit.balance,
                },
                &runtime,
            );
        });
        tokens.insert(token.mint, token);
        queued.push((deposit_id as u64, deposit));
    }
    let sweep = SplSweep::plan(queued, &tokens, &schnorr_master_key());
    let message = sweep.sweep_message(Hash::default());
    let signers = sweep.signers(&message);
    let signature = signature(100);
    mutate_state(|state| {
        process_event(
            state,
            EventType::SubmittedTransaction {
                signature,
                message: message.into(),
                signers,
                purpose: TransactionPurpose::SweepSplDeposits {
                    deposit_ids: sweep.deposits().keys().copied().collect(),
                },
                block_height: DEFAULT_BLOCK_HEIGHT,
            },
            &runtime,
        );
    });
    (runtime, signature, sweep)
}

fn assert_replay_matches_state() -> State {
    let events = with_event_iter(|events| {
        events
            .map(|event| Event::from_bytes(event.to_bytes()))
            .collect::<Vec<_>>()
    });
    let replayed = replay_events(events);
    read_state(|state| assert_eq!(state, &replayed));
    replayed
}

fn fund_minter() -> u64 {
    let runtime = TestCanisterRuntime::new().add_times([0; 8]);
    let deposit = queued_deposit_of(account(3), 10_000_000);
    let sweep = planned_sweep([(0, deposit)]);
    let signature = signature(101);
    let amount_received = sweep.expected_received();
    mutate_state(|state| {
        for event in [
            EventType::QueuedDeposit {
                deposit_id: 0,
                account: deposit.account,
                address: deposit.address,
                balance: deposit.balance,
            },
            EventType::SubmittedTransaction {
                signature,
                message: sweep.sweep_message(Hash::default()).into(),
                signers: vec![Signer::Account(deposit.account)],
                purpose: TransactionPurpose::SweepDeposits {
                    deposit_ids: vec![0],
                },
                block_height: DEFAULT_BLOCK_HEIGHT,
            },
            EventType::SucceededTransaction { signature },
            EventType::CreditedSweep {
                signature,
                amount_received,
                mints: sweep.mints(),
            },
        ] {
            process_event(state, event, &runtime);
        }
    });
    amount_received
}

mod credit_sweep {
    use super::*;

    const CREDIT_TIMESTAMP: u64 = 1_234_000_000;
    const LAMPORTS_SPENT: u64 = 20_000;

    fn finalized_sweep() -> (
        TestCanisterRuntime,
        Signature,
        SplSweep,
        Vec<CreditedSplDeposit>,
    ) {
        let (runtime, signature, sweep) = submitted_spl_sweep();
        mutate_state(|state| {
            process_event(
                state,
                EventType::SucceededTransaction { signature },
                &runtime,
            )
        });
        let mints = sweep
            .deposits()
            .iter()
            .map(|(id, deposit)| CreditedSplDeposit {
                deposit_id: *id,
                amount_to_mint: deposit.balance,
            })
            .collect();
        (runtime, signature, sweep, mints)
    }

    fn credit(signature: Signature, mints: Vec<CreditedSplDeposit>, spent: u64) {
        mutate_state(|state| {
            process_event(
                state,
                EventType::CreditedSplSweep {
                    signature,
                    lamports_spent: spent,
                    mints,
                },
                &TestCanisterRuntime::new().add_times([CREDIT_TIMESTAMP; 2]),
            )
        });
    }

    #[test]
    fn should_credit_and_replay_spl_sweep_with_spend_debited() {
        let (_, signature, sweep, mints) = finalized_sweep();
        let initial_balance = fund_minter();
        credit(signature, mints, LAMPORTS_SPENT);
        let replayed = assert_replay_matches_state();
        assert_eq!(replayed.balance(), initial_balance - LAMPORTS_SPENT);
        assert!(replayed.spl_deposits().finalized().is_empty());
        assert_eq!(
            replayed.spl_deposits().pending_mints().len(),
            sweep.deposits().len()
        );
        for (id, deposit) in sweep.deposits() {
            let pending = &replayed.spl_deposits().pending_mints()[id];
            assert_eq!(
                pending,
                &PendingSplMint {
                    deposit: SweptSplDeposit {
                        deposit: deposit.clone(),
                        signature
                    },
                    amount_to_mint: deposit.balance,
                    created_at_time: CREDIT_TIMESTAMP,
                }
            );
            assert_eq!(pending.account(), deposit.account);
            assert_eq!(pending.sweep_signature(), signature);
            assert_eq!(
                replayed
                    .spl_deposits()
                    .in_flight_id(&deposit.account, &deposit.mint),
                Some(*id)
            );
        }
    }

    #[test]
    #[should_panic(expected = "not finalized")]
    fn should_reject_crediting_spl_sweep_twice() {
        let (_, signature, _, mints) = finalized_sweep();
        fund_minter();
        credit(signature, mints.clone(), LAMPORTS_SPENT);
        credit(signature, mints, LAMPORTS_SPENT);
    }

    #[test]
    #[should_panic(expected = "not finalized")]
    fn should_reject_crediting_unfinalized_spl_sweep() {
        let (_, signature, _) = submitted_spl_sweep();
        fund_minter();
        credit(signature, vec![], LAMPORTS_SPENT);
    }

    #[test]
    fn should_credit_and_replay_spl_sweep_without_tracked_balance() {
        let (_, signature, _, mints) = finalized_sweep();
        credit(signature, mints, LAMPORTS_SPENT);
        let replayed = assert_replay_matches_state();
        assert_eq!(replayed.balance(), 0);
        assert!(replayed.spl_deposits().finalized().is_empty());
        assert_eq!(replayed.spl_deposits().pending_mints().len(), 2);
    }

    #[test]
    fn should_floor_tracked_balance_when_spl_sweep_spend_exceeds_it() {
        let (_, signature, _, mints) = finalized_sweep();
        let balance = fund_minter();
        credit(signature, mints, balance + LAMPORTS_SPENT);
        let replayed = assert_replay_matches_state();
        assert_eq!(replayed.balance(), 0);
        assert!(replayed.spl_deposits().finalized().is_empty());
        assert_eq!(replayed.spl_deposits().pending_mints().len(), 2);
    }

    #[test]
    #[should_panic(expected = "mints for 2 deposits")]
    fn should_reject_missing_spl_mint() {
        let (_, signature, _, mut mints) = finalized_sweep();
        fund_minter();
        mints.pop();
        credit(signature, mints, LAMPORTS_SPENT);
    }

    #[test]
    #[should_panic(expected = "not part of sweep")]
    fn should_reject_unknown_spl_deposit() {
        let (_, signature, _, mut mints) = finalized_sweep();
        fund_minter();
        mints[0].deposit_id = 100;
        credit(signature, mints, LAMPORTS_SPENT);
    }

    #[test]
    #[should_panic(expected = "twice in sweep")]
    fn should_reject_duplicate_spl_deposit_in_credit() {
        let (_, signature, _, mut mints) = finalized_sweep();
        fund_minter();
        mints[1] = mints[0].clone();
        credit(signature, mints, LAMPORTS_SPENT);
    }

    #[test]
    #[should_panic(expected = "different from its transferred balance")]
    fn should_reject_spl_credit_with_wrong_token_amount() {
        let (_, signature, _, mut mints) = finalized_sweep();
        fund_minter();
        mints[0].amount_to_mint += 1;
        credit(signature, mints, LAMPORTS_SPENT);
    }
}

#[test]
fn should_submit_and_replay_spl_sweep() {
    let (_, signature, sweep) = submitted_spl_sweep();
    let replayed = assert_replay_matches_state();

    assert!(replayed.spl_deposits().queued().is_empty());
    assert_eq!(
        replayed.spl_deposits().swept().get(&signature),
        Some(&sweep)
    );
    for (id, deposit) in sweep.deposits() {
        assert_eq!(
            replayed
                .spl_deposits()
                .in_flight_id(&deposit.account, &deposit.mint),
            Some(*id)
        );
    }
    let transaction = replayed.submitted_transactions().get(&signature).unwrap();
    assert_eq!(
        transaction.purpose,
        TransactionPurpose::SweepSplDeposits {
            deposit_ids: vec![0, 1]
        }
    );
    assert_eq!(
        transaction.signers,
        vec![Signer::Minter, Signer::Account(account(1))]
    );
    assert_eq!(transaction.block_height, DEFAULT_BLOCK_HEIGHT);
    assert_eq!(transaction.amount, 0);
    assert_eq!(replayed.balance(), 0);
    assert!(replayed.deposits().queued().is_empty());
    assert!(replayed.deposits().swept().is_empty());
}

#[test]
#[should_panic(expected = "the plan builds")]
fn should_reject_replayed_submission_with_wrong_token_amount() {
    let (_, _, sweep) = submitted_spl_sweep();
    let mut events = with_event_iter(|events| events.collect::<Vec<_>>());
    let EventType::SubmittedTransaction { message, .. } = &mut events.last_mut().unwrap().payload
    else {
        panic!("expected the submitted SPL sweep event");
    };
    let crate::state::event::VersionedMessage::Legacy(message) = message;
    message.instructions[1].data =
        spl_token_2022_interface::instruction::TokenInstruction::TransferChecked {
            amount: sweep.deposits()[&0].balance + 1,
            decimals: 6,
        }
        .pack();

    replay_events(events);
}

#[test]
fn should_finalize_and_replay_spl_sweep_without_releasing_accounts() {
    let (runtime, signature, sweep) = submitted_spl_sweep();
    mutate_state(|state| {
        process_event(
            state,
            EventType::SucceededTransaction { signature },
            &runtime,
        )
    });
    let replayed = assert_replay_matches_state();

    assert!(replayed.submitted_transactions().is_empty());
    assert!(replayed.succeeded_transactions().contains(&signature));
    assert!(replayed.spl_deposits().swept().is_empty());
    assert_eq!(
        replayed.spl_deposits().finalized().get(&signature),
        Some(&sweep)
    );
    for (id, deposit) in sweep.deposits() {
        assert_eq!(
            replayed
                .spl_deposits()
                .in_flight_id(&deposit.account, &deposit.mint),
            Some(*id)
        );
    }
}

#[test]
fn should_drop_and_replay_failed_spl_sweep() {
    let (runtime, signature, sweep) = submitted_spl_sweep();
    mutate_state(|state| {
        process_event(state, EventType::FailedTransaction { signature }, &runtime)
    });
    let replayed = assert_replay_matches_state();

    assert!(replayed.submitted_transactions().is_empty());
    assert!(replayed.failed_transactions().contains_key(&signature));
    assert!(replayed.transactions_to_resubmit().is_empty());
    assert!(replayed.spl_deposits().swept().is_empty());
    assert_eq!(
        replayed.spl_deposits().dropped().len(),
        sweep.deposits().len()
    );
    for (deposit_id, deposit) in sweep.deposits() {
        assert_eq!(
            replayed.spl_deposits().dropped().get(deposit_id),
            Some(&SweptSplDeposit {
                deposit: deposit.clone(),
                signature
            })
        );
        assert_eq!(
            replayed
                .spl_deposits()
                .in_flight_id(&deposit.account, &deposit.mint),
            None
        );
    }
}

#[test]
fn should_drop_and_replay_expired_spl_sweep_without_resubmission() {
    let (runtime, signature, sweep) = submitted_spl_sweep();
    mutate_state(|state| {
        process_event(state, EventType::ExpiredTransaction { signature }, &runtime)
    });
    let replayed = assert_replay_matches_state();

    assert!(replayed.submitted_transactions().is_empty());
    assert!(replayed.transactions_to_resubmit().is_empty());
    assert!(replayed.spl_deposits().swept().is_empty());
    assert_eq!(
        replayed.spl_deposits().dropped().len(),
        sweep.deposits().len()
    );
    for (deposit_id, deposit) in sweep.deposits() {
        assert_eq!(
            replayed.spl_deposits().dropped().get(deposit_id),
            Some(&SweptSplDeposit {
                deposit: deposit.clone(),
                signature,
            })
        );
        assert_eq!(
            replayed
                .spl_deposits()
                .in_flight_id(&deposit.account, &deposit.mint),
            None
        );
    }
}

#[test]
fn should_split_and_replay_failed_spl_batch_without_releasing_accounts() {
    let (runtime, signature, sweep) = submitted_spl_sweep();
    mutate_state(|state| {
        process_event(
            state,
            EventType::SplitFailedSplSweep { signature },
            &runtime,
        )
    });
    let replayed = assert_replay_matches_state();
    assert!(replayed.submitted_transactions().is_empty());
    assert!(replayed.failed_transactions().contains_key(&signature));
    assert!(replayed.transactions_to_resubmit().is_empty());
    assert_eq!(replayed.spl_deposits().queued(), sweep.deposits());
    assert!(replayed.spl_deposits().swept().is_empty());
    assert!(replayed.spl_deposits().dropped().is_empty());
    assert_eq!(replayed.spl_deposits().next_id(), 2);
    for (id, deposit) in sweep.deposits() {
        assert!(replayed.spl_deposits().requires_individual_sweep(*id));
        assert_eq!(
            replayed
                .spl_deposits()
                .in_flight_id(&deposit.account, &deposit.mint),
            Some(*id)
        );
    }
}

#[test]
fn should_debit_the_fee_of_a_failed_spl_sweep() {
    for split in [false, true] {
        reset_state();
        reset_events();
        let (runtime, signature, _) = submitted_spl_sweep();
        let initial_balance = fund_minter();
        let event = if split {
            EventType::SplitFailedSplSweep { signature }
        } else {
            EventType::FailedTransaction { signature }
        };

        mutate_state(|state| process_event(state, event, &runtime));

        let replayed = assert_replay_matches_state();
        assert_eq!(replayed.balance(), initial_balance - 2 * FEE_PER_SIGNATURE);
    }
}

#[test]
#[should_panic(expected = "not an SPL batch")]
fn should_reject_spl_split_event_for_single_deposit() {
    let (runtime, batch_signature, _) = submitted_spl_sweep();
    mutate_state(|state| {
        process_event(
            state,
            EventType::SplitFailedSplSweep {
                signature: batch_signature,
            },
            &runtime,
        )
    });
    let (deposit, token) = read_state(|state| {
        let deposit = state.spl_deposits().queued()[&0].clone();
        let token = state.supported_spl_token(&deposit.mint).cloned().unwrap();
        (deposit, token)
    });
    let plan = SplSweep::plan(
        [(0, deposit)],
        &BTreeMap::from([(token.mint, token)]),
        &schnorr_master_key(),
    );
    let message = plan.sweep_message(Hash::default());
    let signature = signature(101);
    mutate_state(|state| {
        process_event(
            state,
            EventType::SubmittedTransaction {
                signature,
                signers: plan.signers(&message),
                message: message.into(),
                purpose: TransactionPurpose::SweepSplDeposits {
                    deposit_ids: vec![0],
                },
                block_height: DEFAULT_BLOCK_HEIGHT,
            },
            &runtime,
        );
        process_event(
            state,
            EventType::SplitFailedSplSweep { signature },
            &runtime,
        );
    });
}

#[test]
#[should_panic(expected = "Attempted to plan an SPL sweep without deposits")]
fn should_reject_empty_spl_sweep_plan() {
    planned_spl_sweep([]);
}

#[test]
#[should_panic(expected = "Attempted to create an SPL sweep with deposit 0 twice")]
fn should_reject_duplicate_deposit_id_in_sweep_plan() {
    let first = deposit(account(1), [1; 32].into());
    planned_spl_sweep([(0, first.clone()), (0, first)]);
}

#[test]
fn should_queue_deposits_by_account_and_mint() {
    let mut deposits = SplDeposits::default();
    let first = deposit(account(1), [1; 32].into());
    let other_mint = deposit(account(1), [2; 32].into());
    let other_account = deposit(account(2), first.mint);

    deposits.queue(0, first.clone());
    deposits.queue(1, other_mint.clone());
    deposits.queue(2, other_account.clone());

    assert_eq!(deposits.next_id(), 3);
    for (id, queued) in [(0, first), (1, other_mint), (2, other_account)] {
        assert_eq!(deposits.queued().get(&id), Some(&queued));
        assert_eq!(
            deposits.in_flight_id(&queued.account, &queued.mint),
            Some(id)
        );
    }
    assert_eq!(deposits.in_flight_id(&account(3), &[1; 32].into()), None);
}

#[test]
fn should_move_queued_deposits_to_one_sweep_and_keep_them_in_flight() {
    let mut deposits = SplDeposits::default();
    let first = deposit(account(1), [1; 32].into());
    let other = deposit(account(1), [2; 32].into());
    let other_account = deposit(account(2), first.mint);
    let still_queued = deposit(account(3), first.mint);
    let signature = Signature::from([1; 64]);
    deposits.queue(0, first.clone());
    deposits.queue(1, other.clone());
    deposits.queue(2, other_account.clone());
    deposits.queue(3, still_queued.clone());

    sweep(&mut deposits, &[0, 1, 2], &signature);

    assert_eq!(
        deposits.queued(),
        &BTreeMap::from([(3, still_queued.clone())])
    );
    assert_eq!(deposits.swept().len(), 1);
    assert_eq!(
        deposits.swept().get(&signature).unwrap().deposits(),
        &BTreeMap::from([
            (0, first.clone()),
            (1, other.clone()),
            (2, other_account.clone())
        ])
    );
    for (id, deposit) in [
        (0, first),
        (1, other),
        (2, other_account),
        (3, still_queued),
    ] {
        assert_eq!(
            deposits.in_flight_id(&deposit.account, &deposit.mint),
            Some(id)
        );
    }
    assert_eq!(deposits.next_id(), 4);
    assert_eq!(
        deposits.swept().get(&signature).unwrap().deposits().len(),
        3
    );
}

#[test]
fn should_sweep_a_single_deposit() {
    let mut deposits = SplDeposits::default();
    let queued = deposit(account(1), [1; 32].into());
    let signature = Signature::from([1; 64]);
    deposits.queue(0, queued.clone());

    sweep(&mut deposits, &[0], &signature);

    assert!(deposits.queued().is_empty());
    assert_eq!(
        deposits.swept().get(&signature).unwrap().deposits(),
        &BTreeMap::from([(0, queued)])
    );
}

#[test]
#[should_panic(expected = "Attempted to sweep no SPL deposits")]
fn should_reject_empty_sweep() {
    let (message, signers, tokens) = no_plan();
    SplDeposits::default().sweep(&[], &message, &signers, &tokens, &Signature::from([1; 64]));
}

#[test]
#[should_panic(expected = "unknown or already swept SPL deposit")]
fn should_reject_duplicate_deposit_id_in_sweep() {
    let mut deposits = SplDeposits::default();
    deposits.queue(0, deposit(account(1), [1; 32].into()));

    sweep(&mut deposits, &[0, 0], &Signature::from([1; 64]));
}

#[test]
#[should_panic(expected = "unknown or already swept SPL deposit")]
fn should_reject_sweep_for_unknown_deposit() {
    let (message, signers, tokens) = no_plan();
    SplDeposits::default().sweep(&[0], &message, &signers, &tokens, &Signature::from([1; 64]));
}

#[test]
#[should_panic(expected = "unknown or already swept SPL deposit")]
fn should_reject_sweeping_same_deposit_twice() {
    let mut deposits = SplDeposits::default();
    deposits.queue(0, deposit(account(1), [1; 32].into()));
    sweep(&mut deposits, &[0], &Signature::from([1; 64]));

    let (message, signers, tokens) = no_plan();
    deposits.sweep(&[0], &message, &signers, &tokens, &Signature::from([2; 64]));
}

#[test]
#[should_panic(expected = "Attempted to record SPL sweep")]
fn should_reject_reusing_sweep_signature() {
    let mut deposits = SplDeposits::default();
    deposits.queue(0, deposit(account(1), [1; 32].into()));
    deposits.queue(1, deposit(account(2), [1; 32].into()));
    let signature = Signature::from([1; 64]);
    sweep(&mut deposits, &[0], &signature);

    sweep(&mut deposits, &[1], &signature);
}

#[test]
#[should_panic(expected = "already has one in flight")]
fn should_reject_new_deposit_for_swept_account_and_mint() {
    let mut deposits = SplDeposits::default();
    let first = deposit(account(1), [1; 32].into());
    deposits.queue(0, first.clone());
    sweep(&mut deposits, &[0], &Signature::from([1; 64]));

    deposits.queue(1, first);
}

#[test]
#[should_panic(expected = "already has one in flight")]
fn should_reject_duplicate_account_and_mint() {
    let mut deposits = SplDeposits::default();
    let first = deposit(account(1), [1; 32].into());
    let same_account = Account {
        subaccount: Some([0; 32]),
        ..first.account
    };
    deposits.queue(0, first.clone());

    deposits.queue(1, deposit(same_account, first.mint));
}

#[test]
#[should_panic(expected = "out of sequence")]
fn should_reject_out_of_sequence_deposit_id() {
    let mut deposits = SplDeposits::default();

    deposits.queue(1, deposit(account(1), [1; 32].into()));
}

#[test]
#[should_panic(expected = "zero balance")]
fn should_reject_zero_balance() {
    let mut deposits = SplDeposits::default();
    let empty = QueuedSplDeposit {
        balance: 0,
        ..deposit(account(1), [1; 32].into())
    };

    deposits.queue(0, empty);
}

#[test]
#[should_panic(expected = "unsupported SPL mint")]
fn should_reject_queued_deposit_for_unsupported_mint() {
    let runtime = TestCanisterRuntime::new().with_increasing_time();
    lifecycle::init(valid_init_args(), runtime.clone());
    let queued = deposit(account(1), [1; 32].into());

    mutate_state(|state| {
        process_event(
            state,
            EventType::QueuedSplDeposit {
                deposit_id: 0,
                account: queued.account,
                mint: queued.mint,
                address: queued.address,
                balance: queued.balance,
            },
            &runtime,
        )
    });
}

#[test]
fn should_replay_queued_spl_deposit_from_stable_events() {
    let runtime = TestCanisterRuntime::new().with_increasing_time();
    lifecycle::init(valid_init_args(), runtime.clone());
    let token = spl_token([1; 32].into(), TokenProgram::Classic);
    let queued = deposit(
        Account {
            subaccount: Some([7; 32]),
            ..account(1)
        },
        token.mint,
    );
    mutate_state(|state| {
        process_event(state, EventType::AddedSplToken(token), &runtime);
        process_event(
            state,
            EventType::QueuedSplDeposit {
                deposit_id: 0,
                account: queued.account,
                mint: queued.mint,
                address: queued.address,
                balance: queued.balance,
            },
            &runtime,
        );
    });
    let events = with_event_iter(|events| {
        events
            .map(|event| Event::from_bytes(event.to_bytes()))
            .collect::<Vec<_>>()
    });

    let replayed = replay_events(events);

    read_state(|state| assert_eq!(state, &replayed));
    assert_eq!(replayed.spl_deposits().queued().get(&0), Some(&queued));
    assert_eq!(
        replayed
            .spl_deposits()
            .in_flight_id(&queued.account, &queued.mint),
        Some(0)
    );
    assert_eq!(replayed.spl_deposits().next_id(), 1);
    assert_eq!(replayed.deposits().next_id(), 0);
    assert!(replayed.deposits().queued().is_empty());
}
