use super::*;
use crate::{
    lifecycle,
    state::{
        SupportedSplToken, TokenProgram,
        audit::{process_event, replay_events},
        event::{Event, EventType},
        mutate_state, read_state,
    },
    storage::with_event_iter,
    test_fixtures::{account, runtime::TestCanisterRuntime, valid_init_args},
};
use candid::Principal;
use ic_stable_structures::Storable;

fn deposit(account: Account, mint: Address) -> QueuedSplDeposit {
    QueuedSplDeposit {
        account,
        mint,
        address: [3; 32].into(),
        balance: 42,
    }
}

#[test]
#[should_panic(expected = "Attempted to plan an SPL sweep without deposits")]
fn should_reject_empty_spl_sweep_plan() {
    SplSweep::plan([]);
}

#[test]
#[should_panic(expected = "Attempted to create an SPL sweep with deposit 0 twice")]
fn should_reject_duplicate_deposit_id_in_sweep_plan() {
    let first = deposit(account(1), [1; 32].into());
    SplSweep::plan([(0, first.clone()), (0, first)]);
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

    deposits.sweep(&[0, 1, 2], &signature);

    assert_eq!(
        deposits.queued(),
        &BTreeMap::from([(3, still_queued.clone())])
    );
    assert_eq!(
        deposits.swept(),
        &BTreeMap::from([(
            signature,
            SplSweep {
                deposits: BTreeMap::from([
                    (0, first.clone()),
                    (1, other.clone()),
                    (2, other_account.clone()),
                ]),
            },
        )])
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

    deposits.sweep(&[0], &signature);

    assert!(deposits.queued().is_empty());
    assert_eq!(
        deposits.swept().get(&signature).unwrap().deposits(),
        &BTreeMap::from([(0, queued)])
    );
}

#[test]
#[should_panic(expected = "Attempted to sweep no SPL deposits")]
fn should_reject_empty_sweep() {
    SplDeposits::default().sweep(&[], &Signature::from([1; 64]));
}

#[test]
#[should_panic(expected = "unknown or already swept SPL deposit")]
fn should_reject_duplicate_deposit_id_in_sweep() {
    let mut deposits = SplDeposits::default();
    deposits.queue(0, deposit(account(1), [1; 32].into()));

    deposits.sweep(&[0, 0], &Signature::from([1; 64]));
}

#[test]
#[should_panic(expected = "unknown or already swept SPL deposit")]
fn should_reject_sweep_for_unknown_deposit() {
    SplDeposits::default().sweep(&[0], &Signature::from([1; 64]));
}

#[test]
#[should_panic(expected = "unknown or already swept SPL deposit")]
fn should_reject_sweeping_same_deposit_twice() {
    let mut deposits = SplDeposits::default();
    deposits.queue(0, deposit(account(1), [1; 32].into()));
    deposits.sweep(&[0], &Signature::from([1; 64]));

    deposits.sweep(&[0], &Signature::from([2; 64]));
}

#[test]
#[should_panic(expected = "Attempted to submit SPL sweep")]
fn should_reject_reusing_sweep_signature() {
    let mut deposits = SplDeposits::default();
    deposits.queue(0, deposit(account(1), [1; 32].into()));
    deposits.queue(1, deposit(account(2), [1; 32].into()));
    let signature = Signature::from([1; 64]);
    deposits.sweep(&[0], &signature);

    deposits.sweep(&[1], &signature);
}

#[test]
#[should_panic(expected = "already has one in flight")]
fn should_reject_new_deposit_for_swept_account_and_mint() {
    let mut deposits = SplDeposits::default();
    let first = deposit(account(1), [1; 32].into());
    deposits.queue(0, first.clone());
    deposits.sweep(&[0], &Signature::from([1; 64]));

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
    let token = SupportedSplToken {
        mint: [1; 32].into(),
        token_program: TokenProgram::Classic,
        decimals: 6,
        ledger_id: Principal::from_slice(&[3; 20]),
        minimum_deposit_amount: 1,
        paused: false,
    };
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
