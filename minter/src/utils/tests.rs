use crate::{
    lifecycle,
    state::{
        audit::{process_event, replay_events},
        event::EventType,
        mutate_state,
    },
    storage::with_event_iter,
    test_fixtures::{
        init_state, init_state_with_args, runtime::TestCanisterRuntime, valid_init_args,
    },
    utils::{
        assert_ledger_suite_orchestrator, assert_non_anonymous_account, assert_valid_deposit_owner,
    },
};
use candid::Principal;
use cksol_types_internal::{InitArgs, UpgradeArgs};
use icrc_ledger_types::icrc1::account::Account;

const MINTER_ID: Principal = Principal::from_slice(&[0xCA; 10]);

fn account_of(owner: Principal) -> Account {
    Account {
        owner,
        subaccount: None,
    }
}

#[test]
fn should_accept_regular_deposit_owner() {
    assert_valid_deposit_owner(&account_of(Principal::from_slice(&[1, 2, 3])), MINTER_ID);
}

#[test]
#[should_panic(expected = "the owner must be non-anonymous")]
fn should_reject_anonymous_deposit_owner() {
    assert_valid_deposit_owner(&account_of(Principal::anonymous()), MINTER_ID);
}

#[test]
#[should_panic(expected = "is not a valid deposit owner")]
fn should_reject_the_minter_as_deposit_owner() {
    assert_valid_deposit_owner(&account_of(MINTER_ID), MINTER_ID);
}

#[test]
#[should_panic(expected = "the owner must be non-anonymous")]
fn should_reject_anonymous_account() {
    assert_non_anonymous_account(&account_of(Principal::anonymous()));
}

#[test]
fn should_disable_token_registration_without_an_orchestrator() {
    init_state();
    assert!(assert_ledger_suite_orchestrator(Principal::from_slice(&[1])).is_err());
    assert!(assert_ledger_suite_orchestrator(Principal::anonymous()).is_err());
}

#[test]
fn should_allow_only_the_configured_orchestrator() {
    let orchestrator = Principal::from_slice(&[1, 2, 3]);
    init_state_with_args(InitArgs {
        ledger_suite_orchestrator_id: Some(orchestrator),
        ..valid_init_args()
    });
    assert_eq!(assert_ledger_suite_orchestrator(orchestrator), Ok(()));
    assert!(assert_ledger_suite_orchestrator(Principal::from_slice(&[4, 5, 6])).is_err());
    assert!(assert_ledger_suite_orchestrator(Principal::anonymous()).is_err());
}

#[test]
fn should_revoke_old_orchestrator_after_upgrade_and_preserve_configuration_on_empty_upgrade() {
    let old = Principal::from_slice(&[1, 2, 3]);
    let new = Principal::from_slice(&[4, 5, 6]);
    let runtime = TestCanisterRuntime::new().with_increasing_time();
    lifecycle::init(
        InitArgs {
            ledger_suite_orchestrator_id: Some(old),
            ..valid_init_args()
        },
        runtime.clone(),
    );
    mutate_state(|state| {
        process_event(
            state,
            EventType::Upgrade(UpgradeArgs {
                ledger_suite_orchestrator_id: Some(new),
                ..UpgradeArgs::default()
            }),
            &runtime,
        )
    });
    assert!(assert_ledger_suite_orchestrator(old).is_err());
    assert_eq!(assert_ledger_suite_orchestrator(new), Ok(()));
    mutate_state(|state| {
        process_event(state, EventType::Upgrade(UpgradeArgs::default()), &runtime)
    });
    assert_eq!(assert_ledger_suite_orchestrator(new), Ok(()));
    let replayed = with_event_iter(|events| replay_events(events));
    assert_eq!(replayed.ledger_suite_orchestrator_id(), Some(new));
}
