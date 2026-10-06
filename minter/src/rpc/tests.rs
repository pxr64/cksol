use crate::{
    constants::GET_RECENT_BLOCK_MAX_TRIES,
    rpc::{
        Block, BlockHeight, GetBalanceError, GetRecentBlockError, GetTransactionError,
        SubmitTransactionError, get_balance, get_recent_block, get_transaction, submit_transaction,
    },
    test_fixtures::{
        confirmed_block, confirmed_block_at_height,
        deposit::{
            DEPOSIT_ADDRESS, legacy_deposit_transaction, legacy_deposit_transaction_signature,
        },
        init_state,
        runtime::TestCanisterRuntime,
    },
};
use assert_matches::assert_matches;
use ic_canister_runtime::IcError;
use sol_rpc_types::{HttpOutcallError, RpcError, RpcSource, SupportedRpcProviderId};
use solana_transaction::{Message, Transaction};

mod get_balance_tests {
    use super::*;
    use sol_rpc_types::Lamport;

    type MultiRpcResult = sol_rpc_types::MultiRpcResult<Lamport>;

    #[tokio::test]
    async fn should_return_balance() {
        init_state();
        let runtime =
            TestCanisterRuntime::new().add_stub_response(MultiRpcResult::Consistent(Ok(42)));

        let result = get_balance(&runtime, DEPOSIT_ADDRESS).await;

        assert_eq!(result, Ok(42));
    }

    #[tokio::test]
    async fn should_fail_if_call_fails_or_results_are_wrong() {
        init_state();
        let rpc_error = RpcError::ValidationError("Error 1".to_string());
        let inconsistent = vec![(
            RpcSource::Supported(SupportedRpcProviderId::AnkrMainnet),
            Err(rpc_error.clone()),
        )];

        for (runtime, expected) in [
            (
                TestCanisterRuntime::new().add_stub_error(IcError::CallPerformFailed),
                GetBalanceError::IcError(IcError::CallPerformFailed),
            ),
            (
                TestCanisterRuntime::new()
                    .add_stub_response(MultiRpcResult::Consistent(Err(rpc_error.clone()))),
                GetBalanceError::RpcError(rpc_error.clone()),
            ),
            (
                TestCanisterRuntime::new()
                    .add_stub_response(MultiRpcResult::Inconsistent(inconsistent.clone())),
                GetBalanceError::InconsistentRpcResults,
            ),
        ] {
            let result = get_balance(&runtime, DEPOSIT_ADDRESS).await;

            assert_eq!(result, Err(expected));
        }
    }
}

// TODO DEFI-2643: Test behavior with cycles
mod get_transaction_tests {
    use super::*;

    type MultiRpcResult = sol_rpc_types::MultiRpcResult<
        Option<sol_rpc_types::EncodedConfirmedTransactionWithStatusMeta>,
    >;

    #[tokio::test]
    async fn should_fail_if_get_transaction_fails() {
        init_state();

        let runtime = TestCanisterRuntime::new().add_stub_error(IcError::CallPerformFailed);

        let result = get_transaction(&runtime, legacy_deposit_transaction_signature()).await;

        assert_eq!(
            result,
            Err(GetTransactionError::IcError(IcError::CallPerformFailed))
        );
    }

    #[tokio::test]
    async fn should_fail_if_get_transaction_returns_rpc_error() {
        init_state();

        let rpc_error = RpcError::HttpOutcallError(HttpOutcallError::InvalidHttpJsonRpcResponse {
            status: 500,
            body: "{}}".to_string(),
            parsing_error: None,
        });

        let runtime = TestCanisterRuntime::new()
            .add_stub_response(MultiRpcResult::Consistent(Err(rpc_error.clone())));

        let result = get_transaction(&runtime, legacy_deposit_transaction_signature()).await;

        assert_eq!(result, Err(GetTransactionError::RpcError(rpc_error)));
    }

    #[tokio::test]
    async fn should_fail_if_get_transaction_result_inconsistent() {
        init_state();

        let results = vec![
            (
                RpcSource::Supported(SupportedRpcProviderId::AnkrMainnet),
                Err(RpcError::ValidationError("Error 1".to_string())),
            ),
            (
                RpcSource::Supported(SupportedRpcProviderId::DrpcMainnet),
                Err(RpcError::ValidationError("Error 2".to_string())),
            ),
        ];

        let runtime =
            TestCanisterRuntime::new().add_stub_response(MultiRpcResult::Inconsistent(results));

        let result = get_transaction(&runtime, legacy_deposit_transaction_signature()).await;

        assert_eq!(result, Err(GetTransactionError::InconsistentRpcResults));
    }

    #[tokio::test]
    async fn should_return_empty_if_transaction_not_found() {
        init_state();

        let runtime =
            TestCanisterRuntime::new().add_stub_response(MultiRpcResult::Consistent(Ok(None)));

        let result = get_transaction(&runtime, legacy_deposit_transaction_signature()).await;

        assert_eq!(result, Ok(None))
    }

    #[tokio::test]
    async fn should_return_transaction() {
        init_state();

        let runtime = TestCanisterRuntime::new().add_stub_response(MultiRpcResult::Consistent(Ok(
            Some(legacy_deposit_transaction().try_into().unwrap()),
        )));

        let result = get_transaction(&runtime, legacy_deposit_transaction_signature()).await;

        assert_eq!(result, Ok(Some(legacy_deposit_transaction())))
    }
}

mod submit_transaction_tests {
    use super::*;

    type SendTransactionResult = sol_rpc_types::MultiRpcResult<sol_rpc_types::Signature>;

    #[tokio::test]
    async fn should_return_signature_on_success() {
        init_state();

        let expected_signature = signature();
        let runtime = TestCanisterRuntime::new().add_stub_response(
            SendTransactionResult::Consistent(Ok(expected_signature.clone())),
        );

        let result = submit_transaction(&runtime, transaction()).await;

        assert_eq!(result, Ok(expected_signature.into()));
    }

    #[tokio::test]
    async fn should_fail_on_ic_error() {
        init_state();

        let runtime = TestCanisterRuntime::new().add_stub_error(IcError::CallPerformFailed);

        let result = submit_transaction(&runtime, transaction()).await;

        assert_eq!(
            result,
            Err(SubmitTransactionError::IcError(IcError::CallPerformFailed))
        );
    }

    #[tokio::test]
    async fn should_fail_on_rpc_error() {
        init_state();

        let rpc_error = RpcError::HttpOutcallError(HttpOutcallError::InvalidHttpJsonRpcResponse {
            status: 500,
            body: "Internal server error".to_string(),
            parsing_error: None,
        });

        let runtime = TestCanisterRuntime::new()
            .add_stub_response(SendTransactionResult::Consistent(Err(rpc_error.clone())));

        let result = submit_transaction(&runtime, transaction()).await;

        assert_eq!(result, Err(SubmitTransactionError::RpcError(rpc_error)));
    }

    #[tokio::test]
    async fn should_fail_on_inconsistent_results() {
        init_state();

        let results = vec![
            (
                RpcSource::Supported(SupportedRpcProviderId::AnkrMainnet),
                Ok(solana_signature::Signature::from([0x11; 64]).into()),
            ),
            (
                RpcSource::Supported(SupportedRpcProviderId::DrpcMainnet),
                Ok(solana_signature::Signature::from([0x22; 64]).into()),
            ),
        ];

        let runtime = TestCanisterRuntime::new()
            .add_stub_response(SendTransactionResult::Inconsistent(results));

        let result = submit_transaction(&runtime, transaction()).await;

        assert_eq!(result, Err(SubmitTransactionError::InconsistentRpcResults));
    }

    fn transaction() -> Transaction {
        let message = Message::new(&[], None);
        Transaction {
            signatures: vec![signature().into()],
            message,
        }
    }

    fn signature() -> sol_rpc_types::Signature {
        solana_signature::Signature::from([0x42; 64]).into()
    }
}

mod get_recent_block_tests {
    use super::*;

    type GetSlotResult = sol_rpc_types::MultiRpcResult<sol_rpc_types::Slot>;
    type GetBlockResult = sol_rpc_types::MultiRpcResult<Option<sol_rpc_types::ConfirmedBlock>>;

    const SLOT: sol_rpc_types::Slot = 978458723;

    #[tokio::test]
    async fn should_return_slot_blockhash_and_block_height_on_success() {
        init_state();
        let block_height = BlockHeight::new(SLOT - 10);
        let runtime = TestCanisterRuntime::new()
            .add_stub_response(GetSlotResult::Consistent(Ok(SLOT)))
            .add_stub_response(GetBlockResult::Consistent(Ok(Some(
                confirmed_block_at_height(block_height),
            ))));

        let result = get_recent_block(&runtime).await;

        assert_eq!(
            result,
            Ok(Block {
                slot: SLOT,
                blockhash: blockhash().into(),
                block_height,
            })
        );
    }

    #[tokio::test]
    async fn should_fail_when_block_has_no_block_height() {
        init_state();
        let runtime = TestCanisterRuntime::new()
            .add_stub_response(GetSlotResult::Consistent(Ok(SLOT)))
            .add_stub_response(GetBlockResult::Consistent(Ok(Some(
                sol_rpc_types::ConfirmedBlock {
                    block_height: None,
                    ..confirmed_block()
                },
            ))));

        let result = get_recent_block(&runtime).await;

        assert_eq!(
            result,
            Err(GetRecentBlockError::MissingBlockHeight { slot: SLOT })
        );
    }

    #[tokio::test]
    async fn should_fail_after_retrying() {
        init_state();
        let runtime = TestCanisterRuntime::new()
            .add_recent_block(Err(RpcError::ValidationError("Error".to_string())));

        let result = get_recent_block(&runtime).await;

        assert_matches!(
            result,
            Err(GetRecentBlockError::Failed(errors))
                if errors.len() == GET_RECENT_BLOCK_MAX_TRIES.get()
        );
    }

    fn blockhash() -> sol_rpc_types::Hash {
        solana_hash::Hash::from([0x42; 32]).into()
    }
}

mod get_spl_token_balance_tests {
    use super::*;
    use crate::rpc::{GetSplTokenBalanceError, get_spl_token_balance};
    use crate::state::TokenProgram;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sol_rpc_types::{AccountData, AccountEncoding, AccountInfo};
    use solana_address::Address;
    use solana_program_pack::Pack;
    use spl_token_2022_interface::{
        extension::{
            BaseStateWithExtensionsMut, ExtensionType, StateWithExtensionsMut,
            immutable_owner::ImmutableOwner, memo_transfer::MemoTransfer,
            transfer_fee::TransferFeeAmount,
        },
        state::Account as Token2022Account,
    };
    use spl_token_interface::state::{Account as TokenAccount, AccountState};

    type MultiRpcResult = sol_rpc_types::MultiRpcResult<Option<AccountInfo>>;

    fn owner() -> Address {
        [1; 32].into()
    }

    fn mint() -> Address {
        [2; 32].into()
    }

    fn token_account(amount: u64) -> TokenAccount {
        TokenAccount {
            mint: mint().to_bytes().into(),
            owner: owner().to_bytes().into(),
            amount,
            state: AccountState::Initialized,
            ..TokenAccount::default()
        }
    }

    fn account_info(data: &[u8], program: Address) -> AccountInfo {
        AccountInfo {
            lamports: 2_000_000,
            data: AccountData::Binary(STANDARD.encode(data), AccountEncoding::Base64),
            owner: program.to_string(),
            executable: false,
            rent_epoch: 0,
            space: data.len() as u64,
        }
    }

    fn packed_account(account: TokenAccount, program: Address) -> AccountInfo {
        let mut data = vec![0; TokenAccount::LEN];
        TokenAccount::pack(account, &mut data).unwrap();
        account_info(&data, program)
    }

    fn account_with_extensions(extensions: &[ExtensionType], memo_required: bool) -> AccountInfo {
        let mut data = vec![
            0;
            ExtensionType::try_calculate_account_len::<Token2022Account>(extensions)
                .unwrap()
        ];
        let mut account =
            StateWithExtensionsMut::<Token2022Account>::unpack_uninitialized(&mut data).unwrap();
        for extension in extensions {
            match extension {
                ExtensionType::ImmutableOwner => {
                    account.init_extension::<ImmutableOwner>(false).unwrap();
                }
                ExtensionType::MemoTransfer => {
                    account
                        .init_extension::<MemoTransfer>(false)
                        .unwrap()
                        .require_incoming_transfer_memos = memo_required.into();
                }
                ExtensionType::TransferFeeAmount => {
                    account.init_extension::<TransferFeeAmount>(false).unwrap();
                }
                _ => panic!("unsupported test extension"),
            }
        }
        account.base = Token2022Account {
            mint: mint().to_bytes().into(),
            owner: owner().to_bytes().into(),
            amount: 42,
            state: spl_token_2022_interface::state::AccountState::Initialized,
            ..Token2022Account::default()
        };
        account.pack_base();
        account.init_account_type().unwrap();
        account_info(&data, TokenProgram::Token2022.id())
    }

    async fn balance(
        runtime: &TestCanisterRuntime,
        program: TokenProgram,
    ) -> Result<u64, GetSplTokenBalanceError> {
        get_spl_token_balance(runtime, DEPOSIT_ADDRESS, owner(), mint(), program).await
    }

    #[tokio::test]
    async fn should_return_raw_balance_for_both_token_programs() {
        init_state();
        for program in [TokenProgram::Classic, TokenProgram::Token2022] {
            for amount in [0, 42, u64::MAX] {
                let account = packed_account(token_account(amount), program.id());
                let runtime = TestCanisterRuntime::new()
                    .add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

                let result = balance(&runtime, program).await;

                assert_eq!(result, Ok(amount));
                assert_eq!(runtime.sent_update_calls()[0].method, "getAccountInfo");
            }
        }
    }

    #[tokio::test]
    async fn should_return_balance_from_token_2022_associated_account() {
        init_state();
        let account = account_with_extensions(&[ExtensionType::ImmutableOwner], false);
        let runtime = TestCanisterRuntime::new()
            .add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

        let result = balance(&runtime, TokenProgram::Token2022).await;

        assert_eq!(result, Ok(42));
    }

    #[tokio::test]
    async fn should_return_balance_from_memo_transfer_account() {
        init_state();
        for memo_required in [false, true] {
            for extensions in [
                vec![ExtensionType::MemoTransfer],
                vec![ExtensionType::ImmutableOwner, ExtensionType::MemoTransfer],
            ] {
                let account = account_with_extensions(&extensions, memo_required);
                let runtime = TestCanisterRuntime::new()
                    .add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

                let result = balance(&runtime, TokenProgram::Token2022).await;

                assert_eq!(result, Ok(42));
            }
        }
    }

    #[tokio::test]
    async fn should_return_zero_if_account_does_not_exist() {
        init_state();
        for program in [TokenProgram::Classic, TokenProgram::Token2022] {
            let runtime =
                TestCanisterRuntime::new().add_stub_response(MultiRpcResult::Consistent(Ok(None)));

            let result = balance(&runtime, program).await;

            assert_eq!(result, Ok(0));
        }
    }

    #[tokio::test]
    async fn should_fail_if_call_fails_or_results_are_wrong() {
        init_state();
        let rpc_error = RpcError::ValidationError("Error 1".to_string());
        for (runtime, expected) in [
            (
                TestCanisterRuntime::new().add_stub_error(IcError::CallPerformFailed),
                GetSplTokenBalanceError::IcError(IcError::CallPerformFailed),
            ),
            (
                TestCanisterRuntime::new()
                    .add_stub_response(MultiRpcResult::Consistent(Err(rpc_error.clone()))),
                GetSplTokenBalanceError::RpcError(rpc_error),
            ),
            (
                TestCanisterRuntime::new().add_stub_response(MultiRpcResult::Inconsistent(vec![])),
                GetSplTokenBalanceError::InconsistentRpcResults,
            ),
        ] {
            let result = balance(&runtime, TokenProgram::Classic).await;

            assert_eq!(result, Err(expected));
        }
    }

    #[tokio::test]
    async fn should_reject_wrong_program_owner_mint_and_frozen_accounts() {
        init_state();
        for program in [TokenProgram::Classic, TokenProgram::Token2022] {
            let mut executable = packed_account(token_account(42), program.id());
            executable.executable = true;
            let cases = [
                packed_account(token_account(42), Address::default()),
                executable,
                packed_account(
                    TokenAccount {
                        owner: [3; 32].into(),
                        ..token_account(42)
                    },
                    program.id(),
                ),
                packed_account(
                    TokenAccount {
                        mint: [3; 32].into(),
                        ..token_account(42)
                    },
                    program.id(),
                ),
                packed_account(
                    TokenAccount {
                        state: AccountState::Frozen,
                        ..token_account(42)
                    },
                    program.id(),
                ),
            ];
            for account in cases {
                let runtime = TestCanisterRuntime::new()
                    .add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

                let result = balance(&runtime, program).await;

                assert_matches!(result, Err(GetSplTokenBalanceError::InvalidTokenAccount(_)));
            }
        }
    }

    #[tokio::test]
    async fn should_reject_invalid_or_uninitialized_account_data() {
        init_state();
        for program in [TokenProgram::Classic, TokenProgram::Token2022] {
            let mut invalid_encoding = packed_account(token_account(42), program.id());
            invalid_encoding.data =
                AccountData::Binary("invalid base64".to_string(), AccountEncoding::Base64);
            let cases = [
                invalid_encoding,
                account_info(&[], program.id()),
                account_info(&[0; 82], program.id()),
                packed_account(TokenAccount::default(), program.id()),
            ];
            for account in cases {
                let runtime = TestCanisterRuntime::new()
                    .add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

                let result = balance(&runtime, program).await;

                assert_matches!(result, Err(GetSplTokenBalanceError::InvalidTokenAccount(_)));
            }
        }
    }

    #[tokio::test]
    async fn should_reject_allowed_extensions_with_invalid_payload_size() {
        init_state();
        for extension in [ExtensionType::ImmutableOwner, ExtensionType::MemoTransfer] {
            let account = account_with_extensions(&[extension], false);
            let AccountData::Binary(encoded, AccountEncoding::Base64) = account.data else {
                panic!("expected a base64 test account");
            };
            let mut data = STANDARD.decode(encoded).unwrap();
            // After the base account and account-type byte, each TLV entry has
            // a two-byte type and a two-byte length. Enlarge this allowed entry
            // by one byte while keeping the TLV buffer structurally readable.
            let length_offset = Token2022Account::LEN + 1 + 2;
            let length =
                u16::from_le_bytes(data[length_offset..length_offset + 2].try_into().unwrap());
            data[length_offset..length_offset + 2].copy_from_slice(&(length + 1).to_le_bytes());
            data.push(0);
            let account = account_info(&data, TokenProgram::Token2022.id());
            let runtime = TestCanisterRuntime::new()
                .add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

            let result = balance(&runtime, TokenProgram::Token2022).await;

            assert_matches!(result, Err(GetSplTokenBalanceError::InvalidTokenAccount(_)));
        }
    }

    #[tokio::test]
    async fn should_reject_other_token_2022_account_extensions() {
        init_state();
        let account = account_with_extensions(&[ExtensionType::TransferFeeAmount], false);
        let runtime = TestCanisterRuntime::new()
            .add_stub_response(MultiRpcResult::Consistent(Ok(Some(account))));

        let result = balance(&runtime, TokenProgram::Token2022).await;

        assert_eq!(
            result,
            Err(GetSplTokenBalanceError::InvalidTokenAccount(
                "Unsupported Token-2022 token account extension".to_string(),
            ))
        );
    }
}
