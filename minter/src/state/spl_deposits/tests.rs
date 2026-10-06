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
