use crate::{
    guard::{
        GuardError, MAX_CONCURRENT, TimerGuard, TimerGuardError, deposit_sol_guard,
        deposit_spl_guard, withdrawal_guard,
    },
    state::TaskType,
    test_fixtures::init_state,
};
use candid::Principal;
use icrc_ledger_types::icrc1::account::Account;

fn principal(id: u64) -> Principal {
    Principal::try_from_slice(&id.to_le_bytes()).unwrap()
}

fn account(id: u64, sub: Option<u8>) -> Account {
    Account {
        owner: principal(id),
        subaccount: sub.map(|i| [i; 32]),
    }
}

mod guard {
    use super::*;

    #[test]
    fn should_prevent_concurrent_access_to_same_account() {
        init_state();

        // Effectively the same Account
        let account1 = account(0, None);
        let account2 = account(0, Some(0));
        {
            let _guard = deposit_sol_guard(account1).unwrap();
            let res = deposit_sol_guard(account2).err();
            assert_eq!(res, Some(GuardError::AlreadyProcessing));
        }
        let _guard = deposit_sol_guard(account1).unwrap();
    }

    #[test]
    fn should_allow_access_after_guard_has_been_dropped() {
        init_state();

        let account = account(0, None);
        {
            let _guard = deposit_sol_guard(account).unwrap();
        }
        let _guard = deposit_sol_guard(account).unwrap();
    }

    #[test]
    fn should_guard_deposit_sol_independently_of_withdrawals() {
        init_state();

        let account = account(0, None);
        let _withdrawal_guard = withdrawal_guard(account).unwrap();
        let _deposit_sol_guard = deposit_sol_guard(account).unwrap();

        let res = deposit_sol_guard(account).err();

        assert_eq!(res, Some(GuardError::AlreadyProcessing));
    }

    #[test]
    fn should_prevent_more_than_max_concurrent_access() {
        init_state();

        let guards: Vec<_> = (0..MAX_CONCURRENT / 2)
            .map(|id| {
                deposit_sol_guard(account(0, Some(id as u8))).unwrap_or_else(|e| {
                    panic!("Could not create guard for subaccount {id}: {e:#?}")
                })
            })
            .chain((MAX_CONCURRENT / 2..MAX_CONCURRENT).map(|id| {
                deposit_sol_guard(account(id as u64, None))
                    .unwrap_or_else(|e| panic!("Could not create guard for principal {id}: {e:#?}"))
            }))
            .collect();
        assert_eq!(guards.len(), MAX_CONCURRENT);
        let account = account(MAX_CONCURRENT as u64 + 1, None);
        let res = deposit_sol_guard(account).err();
        assert_eq!(res, Some(GuardError::TooManyConcurrentRequests));
    }
}

mod spl_guard {
    use super::*;
    use solana_address::Address;

    fn mint(id: u8) -> Address {
        [id; 32].into()
    }

    #[test]
    fn should_prevent_concurrent_access_to_same_account_and_mint() {
        init_state();
        let account1 = account(0, None);
        let account2 = account(0, Some(0));
        {
            let _guard = deposit_spl_guard(account1, mint(1)).unwrap();
            let result = deposit_spl_guard(account2, mint(1)).err();

            assert_eq!(result, Some(GuardError::AlreadyProcessing));
        }
        let _guard = deposit_spl_guard(account1, mint(1)).unwrap();
    }

    #[test]
    fn should_allow_different_accounts_and_mints() {
        init_state();
        let _first = deposit_spl_guard(account(0, None), mint(1)).unwrap();
        let _other_mint = deposit_spl_guard(account(0, None), mint(2)).unwrap();
        let _other_owner = deposit_spl_guard(account(1, None), mint(1)).unwrap();
        let _other_subaccount = deposit_spl_guard(account(0, Some(1)), mint(1)).unwrap();
    }

    #[test]
    fn should_guard_spl_deposits_independently_of_sol_and_withdrawals() {
        init_state();
        let account = account(0, None);
        let _sol_guard = deposit_sol_guard(account).unwrap();
        let _withdrawal_guard = withdrawal_guard(account).unwrap();
        let _spl_guard = deposit_spl_guard(account, mint(1)).unwrap();

        assert_eq!(
            deposit_spl_guard(account, mint(1)).err(),
            Some(GuardError::AlreadyProcessing)
        );
    }

    #[test]
    fn should_prevent_more_than_max_concurrent_spl_requests() {
        init_state();
        let guards: Vec<_> = (0..MAX_CONCURRENT)
            .map(|id| deposit_spl_guard(account(0, None), mint(id as u8)).unwrap())
            .collect();
        assert_eq!(guards.len(), MAX_CONCURRENT);
        assert_eq!(
            deposit_spl_guard(account(1, None), mint(1)).err(),
            Some(GuardError::TooManyConcurrentRequests)
        );
        assert_eq!(
            deposit_spl_guard(account(0, None), mint(1)).err(),
            Some(GuardError::AlreadyProcessing)
        );

        drop(guards);
        let _guard = deposit_spl_guard(account(1, None), mint(1)).unwrap();
    }
}

mod timer_guard {
    use super::*;

    #[test]
    fn should_create_guard_successfully() {
        init_state();

        let guard = TimerGuard::new(TaskType::SweepDeposits);
        assert!(guard.is_ok());
    }

    #[test]
    fn should_prevent_concurrent_access_to_same_task() {
        init_state();

        let _guard = TimerGuard::new(TaskType::SweepDeposits).unwrap();
        let result = TimerGuard::new(TaskType::SweepDeposits);

        assert_eq!(result, Err(TimerGuardError::AlreadyProcessing));
    }

    #[test]
    fn should_allow_access_after_guard_has_been_dropped() {
        init_state();

        {
            let _guard = TimerGuard::new(TaskType::SweepDeposits).unwrap();
        }

        let guard = TimerGuard::new(TaskType::SweepDeposits);
        assert!(guard.is_ok());
    }

    #[test]
    fn should_allow_concurrent_access_to_different_tasks() {
        init_state();

        let _guard1 = TimerGuard::new(TaskType::SweepDeposits).unwrap();
        let guard2 = TimerGuard::new(TaskType::Mint);

        assert!(guard2.is_ok());
    }
}
