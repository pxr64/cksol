use super::balance_above_minimum;
use cksol_types::DepositSplError;

#[test]
fn should_reject_balance_below_minimum() {
    for balance in [0, 99] {
        assert_eq!(
            balance_above_minimum(balance, 100),
            Err(DepositSplError::ValueTooSmall {
                balance,
                minimum_deposit_amount: 100,
            })
        );
    }
}

#[test]
fn should_accept_balance_at_minimum() {
    for minimum in [1, 100, u64::MAX] {
        assert_eq!(balance_above_minimum(minimum, minimum), Ok(minimum));
    }
}

#[test]
fn should_accept_balance_above_minimum() {
    for balance in [101, u64::MAX] {
        assert_eq!(balance_above_minimum(balance, 100), Ok(balance));
    }
}

mod balance_reading {
    use crate::{
        address::account_address,
        constants::GET_SPL_TOKEN_BALANCE_CYCLES,
        deposit::spl::deposit_spl,
        guard::deposit_spl_guard,
        state::{
            SupportedSplToken, audit::process_event, event::EventType, mutate_state, read_state,
        },
        storage::total_event_count,
        test_fixtures::{
            deposit::DEPOSITOR_ACCOUNT, init_schnorr_master_key, init_state,
            runtime::TestCanisterRuntime, schnorr_master_key_response,
        },
    };
    use assert_matches::assert_matches;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use candid::Principal;
    use cksol_types::{DepositSplError, InsufficientCyclesError};
    use ic_canister_runtime::IcError;
    use sol_rpc_types::{AccountData, AccountEncoding, AccountInfo, MultiRpcResult};
    use solana_program_pack::Pack;
    use spl_token_interface::state::{Account as TokenAccount, AccountState};

    const MINIMUM_DEPOSIT_AMOUNT: u64 = 100;
    const RPC_REFUND: u128 = GET_SPL_TOKEN_BALANCE_CYCLES / 2;

    fn init_token(token_2022: bool) -> SupportedSplToken {
        init_state();
        init_schnorr_master_key();
        register_token(token_2022)
    }

    fn register_token(token_2022: bool) -> SupportedSplToken {
        let token = SupportedSplToken {
            mint: solana_address::Address::from([2; 32]).into(),
            token_program: if token_2022 {
                solana_address::Address::from(spl_token_2022_interface::id().to_bytes()).into()
            } else {
                solana_address::Address::from(spl_token_interface::id().to_bytes()).into()
            },
            decimals: 6,
            ledger_id: Principal::from_slice(&[3; 20]),
            minimum_deposit_amount: MINIMUM_DEPOSIT_AMOUNT,
            paused: false,
        };
        mutate_state(|state| {
            process_event(
                state,
                EventType::AddedSplToken(token.clone()),
                &TestCanisterRuntime::new().with_increasing_time(),
            )
        });
        token
    }

    fn token_account_info(token: &SupportedSplToken, amount: u64) -> AccountInfo {
        let master_key = read_state(|state| state.minter_public_key().cloned()).unwrap();
        let owner = account_address(&master_key, &DEPOSITOR_ACCOUNT);
        let mint: solana_address::Address = token.mint.clone().into();
        let account = TokenAccount {
            owner: owner.to_bytes().into(),
            mint: mint.to_bytes().into(),
            amount,
            state: AccountState::Initialized,
            ..TokenAccount::default()
        };
        let mut data = vec![0; TokenAccount::LEN];
        TokenAccount::pack(account, &mut data).unwrap();
        AccountInfo {
            lamports: 2_000_000,
            data: AccountData::Binary(STANDARD.encode(&data), AccountEncoding::Base64),
            owner: token.token_program.to_string(),
            executable: false,
            rent_epoch: 0,
            space: data.len() as u64,
        }
    }

    fn runtime() -> TestCanisterRuntime {
        TestCanisterRuntime::new()
            .add_msg_cycles_available(GET_SPL_TOKEN_BALANCE_CYCLES)
            .add_msg_cycles_refunded(RPC_REFUND)
            .expecting_charges()
    }

    fn assert_rpc_cost_charged_without_queueing(runtime: &TestCanisterRuntime) {
        assert_eq!(
            runtime.msg_cycles_accepted(),
            [GET_SPL_TOKEN_BALANCE_CYCLES - RPC_REFUND]
        );
        assert_eq!(runtime.sent_update_calls().len(), 1);
        assert_eq!(runtime.sent_update_calls()[0].method, "getAccountInfo");
        assert!(read_state(|state| state.deposits().queued().is_empty()));
        assert_eq!(total_event_count(), 1); // Token registration only.
    }

    #[tokio::test]
    async fn should_fail_before_rpc_if_insufficient_cycles_are_attached() {
        let token = init_token(false);
        let runtime =
            TestCanisterRuntime::new().add_msg_cycles_available(GET_SPL_TOKEN_BALANCE_CYCLES - 1);

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint).await;

        assert_eq!(
            result,
            Err(DepositSplError::InsufficientCycles(
                InsufficientCyclesError {
                    expected: GET_SPL_TOKEN_BALANCE_CYCLES,
                    received: GET_SPL_TOKEN_BALANCE_CYCLES - 1,
                }
            ))
        );
        assert!(runtime.sent_update_calls().is_empty());
        assert!(runtime.msg_cycles_accepted().is_empty());
        assert_eq!(runtime.schnorr_public_key_call_count(), 0);
    }

    #[tokio::test]
    async fn should_read_balance_at_minimum_and_charge_only_rpc_cost() {
        let token = init_token(false);
        let account = token_account_info(&token, MINIMUM_DEPOSIT_AMOUNT);
        let runtime = runtime().add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint).await;

        assert_eq!(
            result,
            Err(DepositSplError::TemporarilyUnavailable(
                "SPL deposits are not implemented yet".to_string(),
            ))
        );
        assert_rpc_cost_charged_without_queueing(&runtime);
    }

    #[tokio::test]
    async fn should_read_token_2022_balance_above_minimum() {
        let token = init_token(true);
        let account = token_account_info(&token, MINIMUM_DEPOSIT_AMOUNT + 1);
        let runtime = runtime().add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint).await;

        assert_matches!(result, Err(DepositSplError::TemporarilyUnavailable(_)));
        assert_rpc_cost_charged_without_queueing(&runtime);
    }

    #[tokio::test]
    async fn should_reject_balance_below_token_minimum_and_charge_rpc_cost() {
        let token = init_token(false);
        let balance = MINIMUM_DEPOSIT_AMOUNT - 1;
        let account = token_account_info(&token, balance);
        let runtime = runtime().add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint).await;

        assert_eq!(
            result,
            Err(DepositSplError::ValueTooSmall {
                balance,
                minimum_deposit_amount: MINIMUM_DEPOSIT_AMOUNT,
            })
        );
        assert_rpc_cost_charged_without_queueing(&runtime);
    }

    #[tokio::test]
    async fn should_reject_missing_token_account_as_zero_balance() {
        let token = init_token(false);
        let runtime = runtime()
            .add_stub_response(MultiRpcResult::<Option<AccountInfo>>::Consistent(Ok(None)));

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint).await;

        assert_eq!(
            result,
            Err(DepositSplError::ValueTooSmall {
                balance: 0,
                minimum_deposit_amount: MINIMUM_DEPOSIT_AMOUNT,
            })
        );
        assert_rpc_cost_charged_without_queueing(&runtime);
    }

    #[tokio::test]
    async fn should_charge_rpc_cost_if_balance_read_fails() {
        let token = init_token(false);
        let runtime = runtime().add_stub_error(IcError::CallPerformFailed);

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint).await;

        assert_matches!(result, Err(DepositSplError::TemporarilyUnavailable(_)));
        assert_rpc_cost_charged_without_queueing(&runtime);
    }
    #[tokio::test]
    async fn should_hold_guard_across_await_and_release_it_after_error() {
        init_state();
        let token = register_token(false);
        let first_runtime = runtime()
            .with_schnorr_public_key(schnorr_master_key_response())
            .add_stub_response(MultiRpcResult::<Option<AccountInfo>>::Consistent(Ok(None)));
        let mut first = Box::pin(deposit_spl(
            &first_runtime,
            DEPOSITOR_ACCOUNT,
            token.mint.clone(),
        ));
        assert!(futures::poll!(first.as_mut()).is_pending());
        let blocked_runtime = TestCanisterRuntime::new();

        let result = deposit_spl(&blocked_runtime, DEPOSITOR_ACCOUNT, token.mint.clone()).await;

        assert_eq!(result, Err(DepositSplError::AlreadyProcessing));
        assert!(blocked_runtime.sent_update_calls().is_empty());
        assert!(blocked_runtime.msg_cycles_accepted().is_empty());
        assert_eq!(blocked_runtime.schnorr_public_key_call_count(), 0);
        assert_matches!(first.await, Err(DepositSplError::ValueTooSmall { .. }));
        let _guard = deposit_spl_guard(DEPOSITOR_ACCOUNT, token.mint).unwrap();
    }

    #[tokio::test]
    async fn should_release_guard_after_insufficient_cycles() {
        let token = init_token(false);
        let runtime = TestCanisterRuntime::new().add_msg_cycles_available(0);

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.clone()).await;

        assert_matches!(result, Err(DepositSplError::InsufficientCycles(_)));
        let _guard = deposit_spl_guard(DEPOSITOR_ACCOUNT, token.mint).unwrap();
    }
}
