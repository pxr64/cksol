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
        address::{account_address, spl_deposit_address},
        constants::GET_ACCOUNT_INFO_CYCLES,
        deposit::spl::deposit_spl,
        guard::deposit_spl_guard,
        state::{
            SupportedSplToken, TokenProgram, audit::process_event, event::EventType, mutate_state,
            read_state,
        },
        storage::total_event_count,
        test_fixtures::{
            DEPOSIT_CONSOLIDATION_FEE, deposit::DEPOSITOR_ACCOUNT, init_schnorr_master_key,
            init_state, runtime::TestCanisterRuntime, schnorr_master_key_response,
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
    const RPC_REFUND: u128 = GET_ACCOUNT_INFO_CYCLES / 2;

    fn init_token(token_2022: bool) -> SupportedSplToken {
        init_state();
        init_schnorr_master_key();
        register_token(token_2022)
    }

    fn register_token(token_2022: bool) -> SupportedSplToken {
        register(token(token_2022))
    }

    fn token(token_2022: bool) -> SupportedSplToken {
        SupportedSplToken {
            mint: [2; 32].into(),
            token_program: if token_2022 {
                TokenProgram::Token2022
            } else {
                TokenProgram::Classic
            },
            decimals: 6,
            ledger_id: Principal::from_slice(&[3; 20]),
            minimum_deposit_amount: MINIMUM_DEPOSIT_AMOUNT,
            paused: false,
        }
    }

    fn register(token: SupportedSplToken) -> SupportedSplToken {
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
        token_account_info_in_state(token, amount, AccountState::Initialized)
    }

    fn token_account_info_in_state(
        token: &SupportedSplToken,
        amount: u64,
        state: AccountState,
    ) -> AccountInfo {
        let master_key = read_state(|state| state.minter_public_key().cloned()).unwrap();
        let owner = account_address(&master_key, &DEPOSITOR_ACCOUNT);
        let account = TokenAccount {
            owner: owner.to_bytes().into(),
            mint: token.mint.to_bytes().into(),
            amount,
            state,
            ..TokenAccount::default()
        };
        let mut data = vec![0; TokenAccount::LEN];
        TokenAccount::pack(account, &mut data).unwrap();
        AccountInfo {
            lamports: 2_000_000,
            data: AccountData::Binary(STANDARD.encode(&data), AccountEncoding::Base64),
            owner: token.token_program.id().to_string(),
            executable: false,
            rent_epoch: 0,
            space: data.len() as u64,
        }
    }

    fn runtime() -> TestCanisterRuntime {
        TestCanisterRuntime::new()
            .with_increasing_time()
            .add_msg_cycles_available(GET_ACCOUNT_INFO_CYCLES + DEPOSIT_CONSOLIDATION_FEE)
            .add_msg_cycles_refunded(RPC_REFUND)
            .expecting_charges()
    }

    fn assert_rpc_cost_charged_without_queueing(runtime: &TestCanisterRuntime) {
        assert_eq!(
            runtime.msg_cycles_accepted(),
            [GET_ACCOUNT_INFO_CYCLES - RPC_REFUND]
        );
        assert_eq!(runtime.sent_update_calls().len(), 1);
        assert_eq!(runtime.sent_update_calls()[0].method, "getAccountInfo");
        assert!(read_state(|state| state.deposits().queued().is_empty()));
        assert_eq!(total_event_count(), 1); // Token registration only.
    }

    fn assert_queued_deposit(
        runtime: &TestCanisterRuntime,
        token: &SupportedSplToken,
        balance: u64,
    ) {
        assert_eq!(
            runtime.msg_cycles_accepted(),
            [GET_ACCOUNT_INFO_CYCLES - RPC_REFUND + DEPOSIT_CONSOLIDATION_FEE]
        );
        assert_eq!(runtime.sent_update_calls().len(), 1);
        let master_key = read_state(|state| state.minter_public_key().cloned()).unwrap();
        let owner = account_address(&master_key, &DEPOSITOR_ACCOUNT);
        read_state(|state| {
            let queued = state.spl_deposits().queued().get(&0).unwrap();
            assert_eq!(queued.account, DEPOSITOR_ACCOUNT);
            assert_eq!(queued.mint, token.mint);
            assert_eq!(
                queued.address,
                spl_deposit_address(&owner, &token.mint, &token.token_program.id())
            );
            assert_eq!(queued.balance, balance);
            assert_eq!(
                state
                    .spl_deposits()
                    .in_flight_id(&DEPOSITOR_ACCOUNT, &token.mint),
                Some(0)
            );
            assert!(state.deposits().queued().is_empty());
        });
        assert_eq!(total_event_count(), 2); // Registration and queued deposit.
    }

    #[tokio::test]
    async fn should_reject_unknown_mints_without_side_effects() {
        init_state();
        let runtime = TestCanisterRuntime::new();
        for mint in [
            cksol_types::Address::from(solana_address::Address::from([2; 32])),
            cksol_types::Address::default(),
        ] {
            assert_eq!(
                deposit_spl(&runtime, DEPOSITOR_ACCOUNT, mint.clone()).await,
                Err(DepositSplError::UnsupportedToken { mint })
            );
        }
        assert!(runtime.sent_update_calls().is_empty());
        assert!(runtime.msg_cycles_accepted().is_empty());
        assert_eq!(total_event_count(), 0);
    }

    #[tokio::test]
    async fn should_fail_before_rpc_if_insufficient_cycles_are_attached() {
        let token = init_token(false);
        let runtime = TestCanisterRuntime::new()
            .add_msg_cycles_available(GET_ACCOUNT_INFO_CYCLES + DEPOSIT_CONSOLIDATION_FEE - 1);

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

        assert_eq!(
            result,
            Err(DepositSplError::InsufficientCycles(
                InsufficientCyclesError {
                    expected: GET_ACCOUNT_INFO_CYCLES + DEPOSIT_CONSOLIDATION_FEE,
                    received: GET_ACCOUNT_INFO_CYCLES + DEPOSIT_CONSOLIDATION_FEE - 1,
                }
            ))
        );
        assert!(runtime.sent_update_calls().is_empty());
        assert!(runtime.msg_cycles_accepted().is_empty());
        assert_eq!(runtime.schnorr_public_key_call_count(), 0);
    }

    #[tokio::test]
    async fn should_queue_balance_at_minimum_and_charge_rpc_cost_and_fee() {
        let token = init_token(false);
        let account = token_account_info(&token, MINIMUM_DEPOSIT_AMOUNT);
        let runtime = runtime().add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

        assert_eq!(result, Ok(0));
        assert_queued_deposit(&runtime, &token, MINIMUM_DEPOSIT_AMOUNT);
    }

    #[tokio::test]
    async fn should_queue_token_2022_balance_above_minimum() {
        let token = init_token(true);
        let account = token_account_info(&token, MINIMUM_DEPOSIT_AMOUNT + 1);
        let runtime = runtime().add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

        assert_eq!(result, Ok(0));
        assert_queued_deposit(&runtime, &token, MINIMUM_DEPOSIT_AMOUNT + 1);
    }

    #[tokio::test]
    async fn should_reject_balance_below_token_minimum_and_charge_rpc_cost() {
        let token = init_token(false);
        let balance = MINIMUM_DEPOSIT_AMOUNT - 1;
        let account = token_account_info(&token, balance);
        let runtime = runtime().add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

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

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

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

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

        assert_matches!(result, Err(DepositSplError::TemporarilyUnavailable(_)));
        assert_rpc_cost_charged_without_queueing(&runtime);
    }

    #[tokio::test]
    async fn should_reject_frozen_token_account_as_invalid_and_charge_rpc_cost() {
        let token = init_token(false);
        let account =
            token_account_info_in_state(&token, MINIMUM_DEPOSIT_AMOUNT, AccountState::Frozen);
        let runtime = runtime().add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

        assert_eq!(
            result,
            Err(DepositSplError::InvalidTokenAccount(
                "Token account is frozen".to_string()
            ))
        );
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
            token.mint.into(),
        ));
        assert!(futures::poll!(first.as_mut()).is_pending());
        let blocked_runtime = TestCanisterRuntime::new();

        let result = deposit_spl(&blocked_runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

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

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

        assert_matches!(result, Err(DepositSplError::InsufficientCycles(_)));
        let _guard = deposit_spl_guard(DEPOSITOR_ACCOUNT, token.mint).unwrap();
    }
    #[tokio::test]
    async fn should_return_existing_id_without_rpc_or_another_fee() {
        let token = init_token(false);
        let account = token_account_info(&token, MINIMUM_DEPOSIT_AMOUNT);
        let first_runtime =
            runtime().add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));
        assert_eq!(
            deposit_spl(&first_runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await,
            Ok(0)
        );
        let repeated_runtime = TestCanisterRuntime::new()
            .add_msg_cycles_available(GET_ACCOUNT_INFO_CYCLES + DEPOSIT_CONSOLIDATION_FEE);

        let result = deposit_spl(&repeated_runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

        assert_eq!(result, Ok(0));
        assert!(repeated_runtime.sent_update_calls().is_empty());
        assert!(repeated_runtime.msg_cycles_accepted().is_empty());
        assert_eq!(read_state(|state| state.spl_deposits().queued().len()), 1);
        assert_eq!(total_event_count(), 2);
    }

    #[tokio::test]
    async fn should_return_existing_id_for_paused_token_without_cycles() {
        init_state();
        let token = register(SupportedSplToken {
            paused: true,
            ..token(false)
        });
        mutate_state(|state| {
            process_event(
                state,
                EventType::QueuedSplDeposit {
                    deposit_id: 0,
                    account: DEPOSITOR_ACCOUNT,
                    mint: token.mint,
                    address: [4; 32].into(),
                    balance: MINIMUM_DEPOSIT_AMOUNT,
                },
                &TestCanisterRuntime::new().with_increasing_time(),
            )
        });
        let runtime = TestCanisterRuntime::new();

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

        assert_eq!(result, Ok(0));
        assert!(runtime.sent_update_calls().is_empty());
        assert!(runtime.msg_cycles_accepted().is_empty());
        assert_eq!(total_event_count(), 2);
    }
}
