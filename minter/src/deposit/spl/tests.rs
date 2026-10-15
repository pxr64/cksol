use super::balance_above_minimum;
use cksol_types::DepositSplError;

#[test]
fn should_reject_balance_below_minimum() {
    for balance in [0, 99] {
        assert_eq!(
            balance_above_minimum(balance, 100, 0),
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
        assert_eq!(balance_above_minimum(minimum, minimum, 0), Ok(minimum));
    }
}

#[test]
fn should_accept_balance_above_minimum() {
    for balance in [101, u64::MAX] {
        assert_eq!(balance_above_minimum(balance, 100, 0), Ok(balance));
    }
}

#[test]
fn should_require_the_minimum_deposit_after_deducting_the_fee() {
    for balance in [24, 25, 124] {
        assert_eq!(
            balance_above_minimum(balance, 100, 25),
            Err(DepositSplError::ValueTooSmall {
                balance,
                minimum_deposit_amount: 125,
            })
        );
    }
    for balance in [125, u64::MAX] {
        assert_eq!(balance_above_minimum(balance, 100, 25), Ok(balance));
    }
}

#[test]
fn should_reject_unrepresentable_minimum_with_fee() {
    assert!(matches!(
        balance_above_minimum(u64::MAX, u64::MAX, 1),
        Err(DepositSplError::TemporarilyUnavailable(_))
    ));
}

mod balance_reading {
    use crate::{
        address::{
            account_address, associated_token_address, get_spl_deposit_address, minter_address,
        },
        constants::GET_ACCOUNT_INFO_CYCLES,
        deposit::spl::deposit_spl,
        guard::deposit_spl_guard,
        state::{
            SupportedSplToken, TokenProgram, audit::process_event, event::EventType, mutate_state,
            read_state,
        },
        storage::total_event_count,
        test_fixtures::{
            DEPOSIT_SOL_FEE, deposit::DEPOSITOR_ACCOUNT, init_schnorr_master_key, init_state,
            runtime::TestCanisterRuntime, spl_token,
        },
    };
    use assert_matches::assert_matches;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use cksol_types::{DepositSplError, GetSplDepositAddressError, InsufficientCyclesError};
    use ic_canister_runtime::IcError;
    use sol_rpc_types::{AccountData, AccountEncoding, AccountInfo, MultiRpcResult};
    use solana_program_pack::Pack;
    use spl_token_interface::state::{Account as TokenAccount, AccountState};

    pub(super) const MINIMUM_DEPOSIT_AMOUNT: u64 = 100;
    const RPC_REFUND: u128 = GET_ACCOUNT_INFO_CYCLES / 2;

    fn init_token(token_2022: bool) -> SupportedSplToken {
        init_state();
        init_schnorr_master_key();
        register_token(token_2022)
    }

    pub(super) fn register_token(token_2022: bool) -> SupportedSplToken {
        register(token(token_2022))
    }

    fn token(token_2022: bool) -> SupportedSplToken {
        let token_program = if token_2022 {
            TokenProgram::Token2022
        } else {
            TokenProgram::Classic
        };
        SupportedSplToken {
            minimum_deposit_amount: MINIMUM_DEPOSIT_AMOUNT,
            ..spl_token([2; 32].into(), token_program)
        }
    }

    pub(super) fn register(token: SupportedSplToken) -> SupportedSplToken {
        mutate_state(|state| {
            process_event(
                state,
                EventType::AddedSplToken(token.clone()),
                &TestCanisterRuntime::new().with_increasing_time(),
            )
        });
        token
    }

    pub(super) fn token_account_info(token: &SupportedSplToken, amount: u64) -> AccountInfo {
        token_account_info_in_state(token, amount, AccountState::Initialized)
    }

    fn token_account_info_in_state(
        token: &SupportedSplToken,
        amount: u64,
        state: AccountState,
    ) -> AccountInfo {
        let master_key = read_state(|state| state.minter_public_key().cloned()).unwrap();
        let owner = account_address(&master_key, &DEPOSITOR_ACCOUNT);
        token_account_info_for_owner(token, amount, state, owner)
    }

    fn destination_account_info(token: &SupportedSplToken, state: AccountState) -> AccountInfo {
        let master_key = read_state(|state| state.minter_public_key().cloned()).unwrap();
        token_account_info_for_owner(token, 0, state, minter_address(&master_key))
    }

    fn token_account_info_for_owner(
        token: &SupportedSplToken,
        amount: u64,
        state: AccountState,
        owner: solana_address::Address,
    ) -> AccountInfo {
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

    pub(super) fn runtime() -> TestCanisterRuntime {
        TestCanisterRuntime::new()
            .with_increasing_time()
            .add_msg_cycles_available(2 * GET_ACCOUNT_INFO_CYCLES + DEPOSIT_SOL_FEE)
            .add_msg_cycles_refunded(RPC_REFUND)
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
            [
                GET_ACCOUNT_INFO_CYCLES - RPC_REFUND,
                GET_ACCOUNT_INFO_CYCLES - RPC_REFUND + DEPOSIT_SOL_FEE
            ]
        );
        assert_eq!(runtime.sent_update_calls().len(), 2);
        let master_key = read_state(|state| state.minter_public_key().cloned()).unwrap();
        let owner = account_address(&master_key, &DEPOSITOR_ACCOUNT);
        read_state(|state| {
            let queued = state.spl_deposits().queued().get(&0).unwrap();
            assert_eq!(queued.account, DEPOSITOR_ACCOUNT);
            assert_eq!(queued.mint, token.mint);
            assert_eq!(
                queued.address,
                associated_token_address(&owner, &token.mint, &token.token_program.id())
            );
            assert_eq!(queued.balance, balance);
            assert_eq!(queued.fee, 0);
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
            .add_msg_cycles_available(2 * GET_ACCOUNT_INFO_CYCLES + DEPOSIT_SOL_FEE - 1);

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

        assert_eq!(
            result,
            Err(DepositSplError::InsufficientCycles(
                InsufficientCyclesError {
                    expected: 2 * GET_ACCOUNT_INFO_CYCLES + DEPOSIT_SOL_FEE,
                    received: 2 * GET_ACCOUNT_INFO_CYCLES + DEPOSIT_SOL_FEE - 1,
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
        let runtime = runtime()
            .add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))))
            .add_stub_response(MultiRpcResult::<Option<AccountInfo>>::Consistent(Ok(None)));

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

        assert_eq!(result, Ok(0));
        assert_queued_deposit(&runtime, &token, MINIMUM_DEPOSIT_AMOUNT);
    }

    #[tokio::test]
    async fn should_queue_token_2022_balance_above_minimum() {
        let token = init_token(true);
        let account = token_account_info(&token, MINIMUM_DEPOSIT_AMOUNT + 1);
        let runtime = runtime()
            .add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))))
            .add_stub_response(MultiRpcResult::<Option<AccountInfo>>::Consistent(Ok(None)));

        let result = deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await;

        assert_eq!(result, Ok(0));
        assert_queued_deposit(&runtime, &token, MINIMUM_DEPOSIT_AMOUNT + 1);
    }

    #[tokio::test]
    async fn should_accept_existing_unfrozen_destinations_for_both_token_programs() {
        for token_2022 in [false, true] {
            crate::state::reset_state();
            crate::storage::reset_events();
            let token = init_token(token_2022);
            let runtime = runtime()
                .add_stub_response(MultiRpcResult::Consistent(Ok(Some(token_account_info(
                    &token,
                    MINIMUM_DEPOSIT_AMOUNT,
                )))))
                .add_stub_response(MultiRpcResult::Consistent(Ok(Some(
                    destination_account_info(&token, AccountState::Initialized),
                ))));
            assert_eq!(
                deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await,
                Ok(0)
            );
            assert_queued_deposit(&runtime, &token, MINIMUM_DEPOSIT_AMOUNT);
            let calls = runtime.sent_update_calls();
            let master_key = read_state(|state| state.minter_public_key().cloned()).unwrap();
            let expected_destination = associated_token_address(
                &minter_address(&master_key),
                &token.mint,
                &token.token_program.id(),
            );
            let args: (
                sol_rpc_types::RpcSources,
                Option<sol_rpc_types::RpcConfig>,
                sol_rpc_types::GetAccountInfoParams,
            ) = calls[1].args();
            assert_eq!(args.2.pubkey.to_string(), expected_destination.to_string());
        }
    }

    #[tokio::test]
    async fn should_reject_frozen_destinations_without_charging_consolidation_fee() {
        for token_2022 in [false, true] {
            crate::state::reset_state();
            crate::storage::reset_events();
            let token = init_token(token_2022);
            let runtime = runtime()
                .add_stub_response(MultiRpcResult::Consistent(Ok(Some(token_account_info(
                    &token,
                    MINIMUM_DEPOSIT_AMOUNT,
                )))))
                .add_stub_response(MultiRpcResult::Consistent(Ok(Some(
                    destination_account_info(&token, AccountState::Frozen),
                ))));
            assert_eq!(
                deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await,
                Err(DepositSplError::InvalidTokenAccount(
                    "Token account is frozen".to_string()
                ))
            );
            assert_eq!(
                runtime.msg_cycles_accepted(),
                [GET_ACCOUNT_INFO_CYCLES - RPC_REFUND; 2]
            );
            assert_eq!(runtime.sent_update_calls().len(), 2);
            assert!(read_state(|state| state.spl_deposits().queued().is_empty()));
            assert_eq!(total_event_count(), 1);
            let _guard = deposit_spl_guard(DEPOSITOR_ACCOUNT, token.mint).unwrap();
        }
    }

    #[tokio::test]
    async fn should_not_queue_if_destination_read_fails() {
        let token = init_token(false);
        let runtime = runtime()
            .add_stub_response(MultiRpcResult::Consistent(Ok(Some(token_account_info(
                &token,
                MINIMUM_DEPOSIT_AMOUNT,
            )))))
            .add_stub_error(IcError::CallPerformFailed);
        assert_matches!(
            deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await,
            Err(DepositSplError::TemporarilyUnavailable(_))
        );
        assert_eq!(
            runtime.msg_cycles_accepted(),
            [GET_ACCOUNT_INFO_CYCLES - RPC_REFUND; 2]
        );
        assert!(read_state(|state| state.spl_deposits().queued().is_empty()));
        assert_eq!(total_event_count(), 1);
    }

    #[tokio::test]
    async fn should_allow_prefunded_unallocated_destination() {
        let token = init_token(false);
        let destination = AccountInfo {
            lamports: 1,
            data: AccountData::Binary(STANDARD.encode([]), AccountEncoding::Base64),
            owner: solana_system_interface::program::id().to_string(),
            executable: false,
            rent_epoch: 0,
            space: 0,
        };
        let runtime = runtime()
            .add_stub_response(MultiRpcResult::Consistent(Ok(Some(token_account_info(
                &token,
                MINIMUM_DEPOSIT_AMOUNT,
            )))))
            .add_stub_response(MultiRpcResult::Consistent(Ok(Some(destination))));
        assert_eq!(
            deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await,
            Ok(0)
        );
        assert_queued_deposit(&runtime, &token, MINIMUM_DEPOSIT_AMOUNT);
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
        init_schnorr_master_key();
        let token = register_token(false);
        let first_runtime = runtime()
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
        let first_runtime = runtime()
            .add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))))
            .add_stub_response(MultiRpcResult::<Option<AccountInfo>>::Consistent(Ok(None)));
        assert_eq!(
            deposit_spl(&first_runtime, DEPOSITOR_ACCOUNT, token.mint.into()).await,
            Ok(0)
        );
        let repeated_runtime = TestCanisterRuntime::new()
            .add_msg_cycles_available(2 * GET_ACCOUNT_INFO_CYCLES + DEPOSIT_SOL_FEE);

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
                    fee: 0,
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

    #[test]
    fn should_return_the_token_account_of_the_deposit_address() {
        let classic = init_token(false);
        let token_2022 = register(SupportedSplToken {
            minimum_deposit_amount: MINIMUM_DEPOSIT_AMOUNT,
            ..spl_token([3; 32].into(), TokenProgram::Token2022)
        });
        let master_key = read_state(|state| state.minter_public_key().cloned()).unwrap();
        let owner = account_address(&master_key, &DEPOSITOR_ACCOUNT);
        for token in [classic, token_2022] {
            assert_eq!(
                get_spl_deposit_address(&DEPOSITOR_ACCOUNT, &token.mint),
                Ok(associated_token_address(
                    &owner,
                    &token.mint,
                    &token.token_program.id()
                ))
            );
        }
    }

    #[test]
    fn should_reject_token_account_of_unsupported_mint() {
        let token = init_token(false);
        let mint = [9; 32].into();
        assert_ne!(mint, token.mint);

        assert_eq!(
            get_spl_deposit_address(&DEPOSITOR_ACCOUNT, &mint),
            Err(GetSplDepositAddressError::UnsupportedToken { mint: mint.into() })
        );
    }
}

mod lifecycle {
    use super::balance_reading::{MINIMUM_DEPOSIT_AMOUNT, register, register_token, runtime};
    use crate::{
        address::{account_address, associated_token_address},
        constants::FEE_PER_SIGNATURE,
        deposit::spl::{
            deposit_spl, deposit_spl_status, process_pending_spl_mints, sweep_queued_spl_deposits,
        },
        lifecycle,
        monitor::finalize_transactions,
        state::{
            SplSweep, SupportedSplToken, TokenProgram,
            audit::{process_event, replay_events},
            event::{CreditedSplDeposit, Event, EventType, TransactionPurpose},
            mutate_state, read_state, reset_state,
        },
        storage::{reset_events, total_event_count, with_event_iter},
        test_fixtures::{
            BLOCK_INDEX, DEFAULT_BLOCK_HEIGHT, EventsAssert, GetTransactionResult, account,
            confirmed_block, confirmed_block_with,
            deposit::DEPOSITOR_ACCOUNT,
            devnet_sweep,
            runtime::{SentUpdateCall, TestCanisterRuntime},
            signer::{sign_as_devnet_minter, sign_for},
            spl_fee_mint_arg,
            spl_sweep::balanced_outcome,
            spl_token, valid_init_args,
        },
    };
    use base64::{Engine, engine::general_purpose::STANDARD};
    use cksol_types::{DepositSplId, DepositSplStatus, Memo, MintMemo, SplFeeMintStatus};
    use ic_stable_structures::Storable;
    use icrc_ledger_types::icrc1::{
        account::Account,
        transfer::{BlockIndex, NumTokens, TransferArg, TransferError},
    };
    use sol_rpc_types::{AccountData, AccountEncoding, AccountInfo, MultiRpcResult};
    use solana_hash::Hash;
    use solana_program_pack::Pack;
    use solana_signature::Signature;
    use solana_transaction_status_client_types::{
        EncodedConfirmedTransactionWithStatusMeta, EncodedTransaction, TransactionBinaryEncoding,
        option_serializer::OptionSerializer,
    };
    use spl_token_interface::state::{Account as TokenAccount, AccountState};
    use std::collections::BTreeSet;

    const INITIAL_BALANCE: u64 = 10_000_000;
    const DESTINATION_LAMPORTS: u64 = 2_039_280;
    const SLOT: u64 = 300_000_000;
    const CREDITED_AT: u64 = 1_000;
    const MINTED_AT: u64 = CREDITED_AT + 1;

    fn setup() {
        reset_state();
        reset_events();
        let runtime = TestCanisterRuntime::new().with_increasing_time();
        lifecycle::init(valid_init_args(), runtime.clone());
        devnet_sweep::record_master_key(&runtime);
        devnet_sweep::init_balance_to(INITIAL_BALANCE);
    }

    fn source_account_info(
        token: &SupportedSplToken,
        account: Account,
        amount: u64,
    ) -> AccountInfo {
        let token_account = TokenAccount {
            owner: account_address(&devnet_sweep::master_key(), &account)
                .to_bytes()
                .into(),
            mint: token.mint.to_bytes().into(),
            amount,
            state: AccountState::Initialized,
            ..TokenAccount::default()
        };
        let mut data = vec![0; TokenAccount::LEN];
        TokenAccount::pack(token_account, &mut data).unwrap();
        AccountInfo {
            lamports: 2_000_000,
            data: AccountData::Binary(STANDARD.encode(&data), AccountEncoding::Base64),
            owner: token.token_program.id().to_string(),
            executable: false,
            rent_epoch: 0,
            space: data.len() as u64,
        }
    }

    async fn deposit(token: &SupportedSplToken, account: Account, amount: u64) -> DepositSplId {
        let runtime = runtime()
            .add_stub_response(MultiRpcResult::Consistent(Ok(Some(source_account_info(
                token, account, amount,
            )))))
            .add_stub_response(MultiRpcResult::<Option<AccountInfo>>::Consistent(Ok(None)));
        deposit_spl(&runtime, account, token.mint.into())
            .await
            .unwrap()
    }

    struct SubmittedSweep {
        signature: Signature,
        plan: SplSweep,
        blockhash: Hash,
        sent_transaction: String,
    }

    /// Runs one sweep round that submits one transaction signed by the minter and the owners.
    /// The minter signs for real, so the finalizer accepts the transaction it sent. Every
    /// sweep builds on a fresh blockhash, so a repeated sweep never repeats a signature.
    async fn sweep(owners: &[Account]) -> SubmittedSweep {
        static SWEEPS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
        let blockhash = Hash::from([SWEEPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed); 32]);
        let mut runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_stub_response(MultiRpcResult::Consistent(Ok(SLOT)))
            .add_stub_response(MultiRpcResult::Consistent(Ok(confirmed_block_with(
                blockhash,
                DEFAULT_BLOCK_HEIGHT,
            ))))
            .add_signer(sign_as_devnet_minter());
        for owner in owners {
            runtime = runtime.add_signer(sign_for(owner));
        }
        let runtime = runtime.add_stub_response(
            MultiRpcResult::<sol_rpc_types::Signature>::Consistent(Ok(Signature::default().into())),
        );

        sweep_queued_spl_deposits(runtime.clone()).await;

        let sends: Vec<SentUpdateCall> = runtime
            .sent_update_calls()
            .into_iter()
            .filter(|call| call.method == "sendTransaction")
            .collect();
        assert_eq!(sends.len(), 1);
        let (_, _, params): (
            sol_rpc_types::RpcSources,
            Option<sol_rpc_types::RpcConfig>,
            sol_rpc_types::SendTransactionParams,
        ) = sends[0].args();
        let sent_transaction = params.get_transaction().to_string();
        let transaction: solana_transaction::Transaction =
            bincode::deserialize(&STANDARD.decode(&sent_transaction).unwrap()).unwrap();
        let signature = transaction.signatures[0];
        let plan = read_state(|state| {
            state
                .spl_deposits()
                .swept()
                .get(&signature)
                .expect("the round submitted one sweep")
                .clone()
        });
        SubmittedSweep {
            signature,
            plan,
            blockhash,
            sent_transaction,
        }
    }

    fn blockhash() -> Hash {
        confirmed_block().blockhash.to_string().parse().unwrap()
    }

    /// The outcome of the sweep's own signed transaction: every planned transfer landed, and
    /// the minter paid the fee plus the rent of one new account per mint.
    fn outcome_of(sweep: &SubmittedSweep) -> (EncodedConfirmedTransactionWithStatusMeta, u64) {
        let message = sweep.plan.sweep_message(sweep.blockhash);
        let fee = u64::from(message.header.num_required_signatures) * FEE_PER_SIGNATURE;
        let destinations: BTreeSet<_> = sweep
            .plan
            .transfers()
            .iter()
            .map(|transfer| {
                let deposit = &sweep.plan.deposits()[&transfer.deposit_id];
                associated_token_address(
                    &sweep.plan.minter_address(),
                    &deposit.mint,
                    &transfer.token_program.id(),
                )
            })
            .collect();
        let spent = fee + destinations.len() as u64 * DESTINATION_LAMPORTS;
        let balance_before = read_state(|state| state.balance());
        let mut outcome = balanced_outcome(&sweep.plan, sweep.blockhash, true);
        outcome.transaction.transaction = EncodedTransaction::Binary(
            sweep.sent_transaction.clone(),
            TransactionBinaryEncoding::Base64,
        );
        let meta = outcome.transaction.meta.as_mut().unwrap();
        meta.fee = fee;
        meta.pre_balances[0] = balance_before;
        meta.post_balances[0] = balance_before - spent;
        for destination in destinations {
            let index = message
                .account_keys
                .iter()
                .position(|key| *key == destination)
                .unwrap();
            meta.post_balances[index] = DESTINATION_LAMPORTS;
        }
        (outcome, spent)
    }

    /// Runs the finalizer with a finalized status and the given outcome of the sweep.
    async fn finalize_with(outcome: EncodedConfirmedTransactionWithStatusMeta) {
        let runtime = TestCanisterRuntime::new()
            .with_time(CREDITED_AT)
            .add_recent_block(Ok(SLOT))
            .add_stub_response(MultiRpcResult::Consistent(Ok(vec![Some(
                sol_rpc_types::TransactionStatus {
                    slot: SLOT,
                    status: Ok(()),
                    err: None,
                    confirmation_status: Some(
                        sol_rpc_types::TransactionConfirmationStatus::Finalized,
                    ),
                },
            )])))
            .add_stub_response(GetTransactionResult::Consistent(Ok(Some(
                outcome.try_into().unwrap(),
            ))));

        finalize_transactions(runtime).await;
    }

    /// Settles the sweep with its expected outcome and returns the credit timestamp.
    async fn settle(sweep: &SubmittedSweep) -> u64 {
        let balance_before = read_state(|state| state.balance());
        let (outcome, spent) = outcome_of(sweep);

        finalize_with(outcome).await;

        let created_at = read_state(|state| {
            assert!(state.submitted_transactions().is_empty());
            assert!(state.spl_deposits().swept().is_empty());
            assert!(state.spl_deposits().finalized().is_empty());
            assert_eq!(
                state.balance(),
                balance_before + sweep.plan.max_lamports_spent() - spent
            );
            assert!(state.spl_sweep_reservations().is_empty());
            let (first_id, _) = sweep.plan.deposits().first_key_value().unwrap();
            state.spl_deposits().pending_mints()[first_id].created_at_time
        });
        assert_eq!(created_at, CREDITED_AT);
        assert_replay_matches();
        created_at
    }

    /// Runs one mint round with a ledger block index per pending mint, in id order.
    async fn mint(count: usize) -> Vec<SentUpdateCall> {
        let mut runtime = TestCanisterRuntime::new().with_time(MINTED_AT);
        for index in 0..count {
            runtime =
                runtime.add_stub_response::<Result<BlockIndex, TransferError>>(Ok((BLOCK_INDEX
                    + index as u64)
                    .into()));
        }

        process_pending_spl_mints(runtime.clone()).await;

        runtime.sent_update_calls()
    }

    fn mint_arg(
        to: Account,
        created_at: u64,
        signature: Signature,
        deposit_id: DepositSplId,
        amount: u64,
    ) -> TransferArg {
        TransferArg {
            from_subaccount: None,
            to,
            fee: None,
            created_at_time: Some(created_at),
            memo: Some(Memo::from(MintMemo::sweep(signature, deposit_id)).into()),
            amount: NumTokens::from(amount),
        }
    }

    /// Finalizes the sweep as failed on chain with the given error.
    async fn fail(sweep: &SubmittedSweep, error: sol_rpc_types::TransactionError) {
        let runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_recent_block(Ok(SLOT))
            .add_stub_response(MultiRpcResult::Consistent(Ok(vec![Some(
                sol_rpc_types::TransactionStatus {
                    slot: SLOT,
                    status: Err(error.clone()),
                    err: Some(error),
                    confirmation_status: Some(
                        sol_rpc_types::TransactionConfirmationStatus::Finalized,
                    ),
                },
            )])));

        finalize_transactions(runtime).await;

        read_state(|state| {
            assert!(state.failed_transactions().contains_key(&sweep.signature));
            assert!(state.spl_deposits().swept().get(&sweep.signature).is_none());
        });
    }

    /// The token balance the outcome leaves on the source account of the deposit.
    fn source_balance_after(
        sweep: &SubmittedSweep,
        outcome: &EncodedConfirmedTransactionWithStatusMeta,
        deposit_id: DepositSplId,
    ) -> u64 {
        let source = sweep.plan.deposits()[&deposit_id].address;
        let source_index = sweep
            .plan
            .sweep_message(blockhash())
            .account_keys
            .iter()
            .position(|key| *key == source)
            .unwrap() as u8;
        let OptionSerializer::Some(balances) = &outcome
            .transaction
            .meta
            .as_ref()
            .unwrap()
            .post_token_balances
        else {
            unreachable!()
        };
        balances
            .iter()
            .find(|balance| balance.account_index == source_index)
            .unwrap()
            .ui_token_amount
            .amount
            .parse()
            .unwrap()
    }

    fn assert_replay_matches() {
        let mut replayed = replay_events(with_event_iter(|events| {
            events
                .map(|event| Event::from_bytes(event.to_bytes()))
                .collect::<Vec<_>>()
        }));
        replayed.cache_minter_public_key(
            read_state(|state| state.minter_public_key().cloned()).unwrap(),
        );
        read_state(|state| assert_eq!(state, &replayed));
    }

    #[tokio::test]
    async fn should_sweep_full_balance_and_deduct_recorded_fee_once() {
        const BALANCE: u64 = 1_000;
        const FEE: u64 = 25;
        for token_2022 in [false, true] {
            setup();
            let token = register_token(token_2022);
            let owner = account_address(&devnet_sweep::master_key(), &DEPOSITOR_ACCOUNT);
            // The production estimator currently quotes zero. Record a nonzero quote to
            // exercise the sweep, settlement and mint paths before enabling an oracle.
            mutate_state(|state| {
                process_event(
                    state,
                    EventType::QueuedSplDeposit {
                        deposit_id: 0,
                        account: DEPOSITOR_ACCOUNT,
                        mint: token.mint,
                        address: associated_token_address(
                            &owner,
                            &token.mint,
                            &token.token_program.id(),
                        ),
                        balance: BALANCE,
                        fee: FEE,
                    },
                    &TestCanisterRuntime::new().with_increasing_time(),
                )
            });
            assert_replay_matches();

            let sweep = sweep(&[DEPOSITOR_ACCOUNT]).await;
            assert_eq!(sweep.plan.deposits()[&0].balance, BALANCE);
            let created_at = settle(&sweep).await;
            read_state(|state| {
                let pending = &state.spl_deposits().pending_mints()[&0];
                assert_eq!(pending.deposit.deposit.fee, FEE);
                assert_eq!(pending.amount_to_mint, BALANCE - FEE);
            });

            let calls = mint(2).await;
            assert_eq!(calls.len(), 2);
            assert_eq!(
                calls[0].single_arg::<TransferArg>(),
                mint_arg(
                    DEPOSITOR_ACCOUNT,
                    created_at,
                    sweep.signature,
                    0,
                    BALANCE - FEE
                )
            );
            assert_eq!(
                calls[1].single_arg::<TransferArg>(),
                spl_fee_mint_arg(0, sweep.signature, MINTED_AT, FEE)
            );
            assert_eq!(calls[1].canister_id, token.ledger_id);
            read_state(|state| {
                assert!(state.spl_deposits().pending_fee_mints().is_empty());
                assert_eq!(
                    *state.spl_deposits().minted_fees()[&0].get(),
                    BLOCK_INDEX + 1
                );
                assert_eq!(
                    state.spl_deposits().minted()[&0].minted_amount,
                    BALANCE - FEE
                );
            });
            assert_replay_matches();
        }
    }

    #[tokio::test]
    async fn should_report_the_status_of_each_stage() {
        setup();
        let token = register_token(false);
        assert_eq!(deposit_spl_status(0), DepositSplStatus::NotFound);

        let deposit_id = deposit(&token, DEPOSITOR_ACCOUNT, MINIMUM_DEPOSIT_AMOUNT).await;
        assert_eq!(
            deposit_spl_status(deposit_id),
            DepositSplStatus::Queued {
                balance: MINIMUM_DEPOSIT_AMOUNT,
                fee: 0,
            }
        );

        let sweep = sweep(&[DEPOSITOR_ACCOUNT]).await;
        assert_eq!(
            deposit_spl_status(deposit_id),
            DepositSplStatus::Swept {
                signature: sweep.signature.into(),
            }
        );

        settle(&sweep).await;
        assert_eq!(
            deposit_spl_status(deposit_id),
            DepositSplStatus::Finalized {
                signature: sweep.signature.into(),
            }
        );

        mint(1).await;
        assert_eq!(
            deposit_spl_status(deposit_id),
            DepositSplStatus::Minted {
                block_index: BLOCK_INDEX,
                minted_amount: MINIMUM_DEPOSIT_AMOUNT,
                fee: SplFeeMintStatus::None,
            }
        );
        assert_eq!(
            deposit_spl_status(deposit_id + 1),
            DepositSplStatus::NotFound
        );
    }

    #[tokio::test]
    async fn should_process_spl_deposit_through_sweep_credit_and_mint() {
        for token_2022 in [false, true] {
            setup();
            let token = register_token(token_2022);
            let deposit_id = deposit(&token, DEPOSITOR_ACCOUNT, MINIMUM_DEPOSIT_AMOUNT).await;
            assert_eq!(deposit_id, 0);
            read_state(|state| {
                assert_eq!(
                    state.spl_deposits().queued()[&deposit_id].balance,
                    MINIMUM_DEPOSIT_AMOUNT
                )
            });

            let sweep = sweep(&[DEPOSITOR_ACCOUNT]).await;

            let signature = sweep.signature;
            read_state(|state| {
                assert!(state.spl_deposits().queued().is_empty());
                assert!(state.submitted_transactions().contains_key(&signature));
            });
            let created_at = settle(&sweep).await;
            let lamports_spent = 2 * FEE_PER_SIGNATURE + DESTINATION_LAMPORTS;
            read_state(|state| {
                let pending = &state.spl_deposits().pending_mints()[&deposit_id];
                assert_eq!(pending.amount_to_mint, MINIMUM_DEPOSIT_AMOUNT);
                assert_eq!(
                    state
                        .spl_deposits()
                        .in_flight_id(&DEPOSITOR_ACCOUNT, &token.mint),
                    Some(deposit_id)
                );
                assert_eq!(state.balance(), INITIAL_BALANCE - lamports_spent);
            });

            let calls = mint(1).await;

            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].canister_id, token.ledger_id);
            assert_eq!(calls[0].method, "icrc1_transfer");
            assert_eq!(
                calls[0].single_arg::<TransferArg>(),
                mint_arg(
                    DEPOSITOR_ACCOUNT,
                    created_at,
                    signature,
                    deposit_id,
                    MINIMUM_DEPOSIT_AMOUNT
                )
            );
            read_state(|state| {
                assert!(state.spl_deposits().pending_mints().is_empty());
                let minted = &state.spl_deposits().minted()[&deposit_id];
                assert_eq!(minted.minted_amount, MINIMUM_DEPOSIT_AMOUNT);
                assert_eq!(*minted.mint_block_index.get(), BLOCK_INDEX);
                assert!(
                    state
                        .spl_deposits()
                        .in_flight_id(&DEPOSITOR_ACCOUNT, &token.mint)
                        .is_none()
                );
                assert_eq!(state.balance(), INITIAL_BALANCE - lamports_spent);
            });
            let deposit = sweep.plan.deposits()[&deposit_id].clone();
            let message = sweep.plan.sweep_message(sweep.blockhash);
            let payloads: Vec<EventType> =
                with_event_iter(|events| events.map(|event| event.payload).collect());
            assert_eq!(
                payloads[payloads.len() - 5..],
                [
                    EventType::QueuedSplDeposit {
                        deposit_id,
                        account: deposit.account,
                        mint: deposit.mint,
                        address: deposit.address,
                        balance: deposit.balance,
                        fee: deposit.fee,
                    },
                    EventType::SubmittedTransaction {
                        signature,
                        message: message.clone().into(),
                        signers: sweep.plan.signers(&message),
                        purpose: TransactionPurpose::SweepSplDeposits {
                            deposit_ids: vec![deposit_id],
                            block_height: DEFAULT_BLOCK_HEIGHT,
                        },
                    },
                    EventType::SucceededTransaction { signature },
                    EventType::CreditedSplSweep {
                        signature,
                        lamports_spent,
                        mints: vec![CreditedSplDeposit {
                            deposit_id,
                            amount_to_mint: MINIMUM_DEPOSIT_AMOUNT,
                        }],
                    },
                    EventType::MintedSweptSplDeposit {
                        deposit_id,
                        mint_block_index: BLOCK_INDEX.into(),
                    },
                ]
            );
            assert_replay_matches();
            let events_before = total_event_count();
            let repeated_runtime = TestCanisterRuntime::new();

            process_pending_spl_mints(repeated_runtime.clone()).await;

            assert!(repeated_runtime.sent_update_calls().is_empty());
            assert_eq!(total_event_count(), events_before);
        }
    }

    #[tokio::test]
    async fn should_sweep_a_batch_across_two_mints_and_owners_into_two_ledgers() {
        setup();
        let classic = register_token(false);
        let token_2022 = register(SupportedSplToken {
            minimum_deposit_amount: MINIMUM_DEPOSIT_AMOUNT,
            ..spl_token([3; 32].into(), TokenProgram::Token2022)
        });
        let alice = DEPOSITOR_ACCOUNT;
        let bob = account(2);
        let expected = [
            (deposit(&classic, alice, 100).await, &classic, alice, 100),
            (deposit(&classic, bob, 200).await, &classic, bob, 200),
            (
                deposit(&token_2022, alice, 300).await,
                &token_2022,
                alice,
                300,
            ),
        ];
        assert_eq!(expected.map(|(id, ..)| id), [0, 1, 2]);

        let sweep = sweep(&[alice, bob]).await;

        let message = sweep.plan.sweep_message(sweep.blockhash);
        assert_eq!(
            sweep.plan.deposits().keys().copied().collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(message.header.num_required_signatures, 3);
        assert_eq!(message.instructions.len(), 5);
        let created_at = settle(&sweep).await;
        let lamports_spent = 3 * FEE_PER_SIGNATURE + 2 * DESTINATION_LAMPORTS;
        read_state(|state| {
            assert_eq!(state.spl_deposits().pending_mints().len(), 3);
            assert_eq!(state.balance(), INITIAL_BALANCE - lamports_spent);
        });
        EventsAssert::from_recorded().expect_contains_event_eq(EventType::CreditedSplSweep {
            signature: sweep.signature,
            lamports_spent,
            mints: expected
                .iter()
                .map(|(deposit_id, _, _, amount)| CreditedSplDeposit {
                    deposit_id: *deposit_id,
                    amount_to_mint: *amount,
                })
                .collect(),
        });

        let calls = mint(3).await;

        assert_eq!(calls.len(), 3);
        for (call, (deposit_id, token, owner, amount)) in calls.iter().zip(expected) {
            assert_eq!(call.canister_id, token.ledger_id);
            assert_eq!(call.method, "icrc1_transfer");
            assert_eq!(
                call.single_arg::<TransferArg>(),
                mint_arg(owner, created_at, sweep.signature, deposit_id, amount)
            );
        }
        read_state(|state| {
            assert!(state.spl_deposits().pending_mints().is_empty());
            assert_eq!(state.spl_deposits().minted().len(), 3);
            for (deposit_id, token, owner, amount) in expected {
                assert_eq!(
                    state.spl_deposits().minted()[&deposit_id].minted_amount,
                    amount
                );
                assert!(
                    state
                        .spl_deposits()
                        .in_flight_id(&owner, &token.mint)
                        .is_none()
                );
            }
        });
        assert_replay_matches();
    }

    #[tokio::test]
    async fn should_release_a_dropped_deposit_and_mint_its_replacement() {
        setup();
        let token = register_token(false);
        let first = deposit(&token, DEPOSITOR_ACCOUNT, MINIMUM_DEPOSIT_AMOUNT).await;
        let failed = sweep(&[DEPOSITOR_ACCOUNT]).await;
        assert_eq!(
            deposit_spl(
                &TestCanisterRuntime::new(),
                DEPOSITOR_ACCOUNT,
                token.mint.into()
            )
            .await,
            Ok(first)
        );

        fail(
            &failed,
            sol_rpc_types::TransactionError::InstructionError(
                1,
                sol_rpc_types::InstructionError::Custom(1),
            ),
        )
        .await;
        assert_eq!(
            deposit_spl_status(first),
            DepositSplStatus::Dropped {
                signature: failed.signature.into(),
            }
        );

        let failed_fee = 2 * FEE_PER_SIGNATURE;
        read_state(|state| {
            assert_eq!(
                state.spl_deposits().dropped()[&first].signature,
                failed.signature
            );
            assert!(
                state
                    .spl_deposits()
                    .in_flight_id(&DEPOSITOR_ACCOUNT, &token.mint)
                    .is_none()
            );
            assert_eq!(state.balance(), INITIAL_BALANCE - failed_fee);
        });

        let second = deposit(&token, DEPOSITOR_ACCOUNT, MINIMUM_DEPOSIT_AMOUNT).await;

        assert_eq!(second, first + 1);
        let retry = sweep(&[DEPOSITOR_ACCOUNT]).await;
        assert_ne!(retry.signature, failed.signature);
        let created_at = settle(&retry).await;
        let calls = mint(1).await;
        assert_eq!(
            calls[0].single_arg::<TransferArg>(),
            mint_arg(
                DEPOSITOR_ACCOUNT,
                created_at,
                retry.signature,
                second,
                MINIMUM_DEPOSIT_AMOUNT
            )
        );
        read_state(|state| {
            assert_eq!(
                state
                    .spl_deposits()
                    .minted()
                    .keys()
                    .copied()
                    .collect::<Vec<_>>(),
                vec![second]
            );
            assert!(state.spl_deposits().dropped().contains_key(&first));
            assert!(
                state
                    .spl_deposits()
                    .in_flight_id(&DEPOSITOR_ACCOUNT, &token.mint)
                    .is_none()
            );
            assert_eq!(
                state.balance(),
                INITIAL_BALANCE - failed_fee - (2 * FEE_PER_SIGNATURE + DESTINATION_LAMPORTS)
            );
        });
        assert_replay_matches();
    }

    #[tokio::test]
    async fn should_report_a_quarantined_pair_by_its_deposit_id() {
        setup();
        let token = register_token(false);
        let deposit_id = deposit(&token, DEPOSITOR_ACCOUNT, MINIMUM_DEPOSIT_AMOUNT).await;
        let sweep = sweep(&[DEPOSITOR_ACCOUNT]).await;
        let (mut outcome, _) = outcome_of(&sweep);
        let OptionSerializer::Some(balances) = &mut outcome
            .transaction
            .meta
            .as_mut()
            .unwrap()
            .post_token_balances
        else {
            unreachable!()
        };
        balances[0].ui_token_amount.amount = "1".to_string();

        finalize_with(outcome).await;

        EventsAssert::from_recorded().expect_contains_event_eq(EventType::QuarantinedSplSweep {
            signature: sweep.signature,
        });
        assert_eq!(
            deposit_spl_status(deposit_id),
            DepositSplStatus::Quarantined {
                signature: sweep.signature.into(),
            }
        );
        read_state(|state| {
            assert_eq!(
                state.spl_deposits().quarantined()[&deposit_id].signature,
                sweep.signature
            );
            // A quarantined sweep keeps its reservation: its spend is unknown.
            assert_eq!(
                state.balance(),
                INITIAL_BALANCE - sweep.plan.max_lamports_spent()
            );
            assert!(state.spl_sweep_reservations().is_empty());
            assert!(state.spl_deposits().pending_mints().is_empty());
            assert_eq!(
                state
                    .spl_deposits()
                    .in_flight_id(&DEPOSITOR_ACCOUNT, &token.mint),
                Some(deposit_id)
            );
        });
        assert_eq!(
            deposit_spl(
                &TestCanisterRuntime::new(),
                DEPOSITOR_ACCOUNT,
                token.mint.into()
            )
            .await,
            Ok(deposit_id)
        );
        let other_subaccount = Account {
            subaccount: Some([1; 32]),
            ..DEPOSITOR_ACCOUNT
        };

        let next = deposit(&token, other_subaccount, MINIMUM_DEPOSIT_AMOUNT).await;

        assert_eq!(next, deposit_id + 1);
        assert_replay_matches();
    }

    #[tokio::test]
    async fn should_mint_the_recorded_balance_and_leave_a_top_up_for_the_next_deposit() {
        setup();
        let token = register_token(false);
        let first = deposit(&token, DEPOSITOR_ACCOUNT, MINIMUM_DEPOSIT_AMOUNT).await;
        let first_sweep = sweep(&[DEPOSITOR_ACCOUNT]).await;
        let (outcome, _) = outcome_of(&first_sweep);
        let leftover = source_balance_after(&first_sweep, &outcome, first);
        assert!(leftover >= MINIMUM_DEPOSIT_AMOUNT);
        let created_at = settle(&first_sweep).await;

        let calls = mint(1).await;

        assert_eq!(
            calls[0].single_arg::<TransferArg>(),
            mint_arg(
                DEPOSITOR_ACCOUNT,
                created_at,
                first_sweep.signature,
                first,
                MINIMUM_DEPOSIT_AMOUNT
            )
        );
        read_state(|state| {
            assert_eq!(
                state.spl_deposits().minted()[&first].minted_amount,
                MINIMUM_DEPOSIT_AMOUNT
            );
            assert!(
                state
                    .spl_deposits()
                    .in_flight_id(&DEPOSITOR_ACCOUNT, &token.mint)
                    .is_none()
            );
        });

        let second = deposit(&token, DEPOSITOR_ACCOUNT, leftover).await;

        assert_eq!(second, first + 1);
        read_state(|state| assert_eq!(state.spl_deposits().queued()[&second].balance, leftover));
        let second_sweep = sweep(&[DEPOSITOR_ACCOUNT]).await;
        let created_at = settle(&second_sweep).await;
        let calls = mint(1).await;
        assert_eq!(
            calls[0].single_arg::<TransferArg>(),
            mint_arg(
                DEPOSITOR_ACCOUNT,
                created_at,
                second_sweep.signature,
                second,
                leftover
            )
        );
        read_state(|state| {
            let minted = state.spl_deposits().minted();
            assert_eq!(minted.len(), 2);
            assert_eq!(
                minted.values().map(|mint| mint.minted_amount).sum::<u64>(),
                MINIMUM_DEPOSIT_AMOUNT + leftover
            );
            assert!(
                state
                    .spl_deposits()
                    .in_flight_id(&DEPOSITOR_ACCOUNT, &token.mint)
                    .is_none()
            );
        });
        assert_replay_matches();
    }
}
