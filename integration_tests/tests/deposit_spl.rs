use assert_matches::assert_matches;
use assert2::check;
use cksol_int_tests::{
    Setup, SetupBuilder,
    fixtures::{
        DEFAULT_CALLER_DEPOSIT_ADDRESS, FINALIZE_TRANSACTIONS_DELAY, MINTER_ADDRESS, MockBuilder,
        PROCESS_PENDING_MINTS_DELAY, SUBMISSION_BLOCK_HEIGHT, SWEEP_DEPOSITS_DELAY, get_memo,
    },
    validator::FEE_PER_SIGNATURE,
};
use cksol_types::{DepositSplArgs, DepositSplError, GetSplDepositAddressArgs, Memo, MintMemo};
use cksol_types_internal::event::{CreditedSplDeposit, EventType, TransactionPurpose};
use icrc_ledger_types::icrc1::account::Account;
use sol_rpc_types::{
    CommitmentLevel, ConsensusStrategy, GetAccountInfoEncoding, GetAccountInfoParams, RpcConfig,
};
use solana_address::Address;
use spl_token_2022_interface::extension::ExtensionType;

mod fixtures;

use fixtures::spl::{
    BALANCE_ABOVE_MINIMUM, DECIMALS, MINIMUM_DEPOSIT_AMOUNT, MINT, RENT_EXEMPT_LAMPORTS,
    account_info, associated_token_address, frozen_token_account_data, queue_deposit_for_caller,
    register_token, setup_with_registered_mint, sweep_transaction_response, token_account_data,
    token_account_data_with_amount,
};

#[tokio::test]
async fn should_sweep_finalize_and_mint_queued_deposit_after_timers() {
    let token_program = spl_token_interface::id().to_bytes().into();
    let setup = SetupBuilder::new()
        .with_proxy_canister()
        .with_ledger_suite_orchestrator(Setup::DEFAULT_CALLER)
        .build()
        .await;
    let ledger = setup.install_spl_ledger(DECIMALS).await;
    // The mint account read uses RPC IDs 0 to 3.
    register_token(&setup, ledger.canister_id(), token_program).await;
    let account = Account {
        owner: setup.proxy_canister_id(),
        subaccount: Some([1; 32]),
    };
    let owner: Address = setup.minter().get_deposit_address(account).await.into();
    let source: Address = setup
        .minter()
        .get_spl_deposit_address(GetSplDepositAddressArgs {
            owner: None,
            subaccount: account.subaccount,
            mint: MINT.into(),
        })
        .await
        .expect("get_spl_deposit_address should return the source token address")
        .into();
    let deposit_id = setup
        .minter()
        .with_http_mocks(
            // Each RPC uses four providers. The source read uses IDs 4 to 7.
            // The destination read uses IDs 8 to 11.
            MockBuilder::with_start_id(4)
                .get_account_info(
                    &source,
                    Some(account_info(token_account_data(&owner, &[]), token_program)),
                )
                .get_account_info(
                    &associated_token_address(&MINTER_ADDRESS, &token_program),
                    None,
                )
                .build(),
        )
        .deposit_spl(DepositSplArgs {
            owner: None,
            subaccount: account.subaccount,
            mint: MINT.into(),
        })
        .await
        .expect("deposit_spl should queue a sweep");
    assert_eq!(ledger.balance_of(account).await, 0);

    setup.advance_time(SWEEP_DEPOSITS_DELAY).await;
    setup
        .execute_http_mocks(
            // getSlot, getBlock and sendTransaction use RPC IDs 12 to 23.
            MockBuilder::with_start_id(12)
                .submit_transaction(SUBMISSION_BLOCK_HEIGHT)
                .build(),
        )
        .await;
    let (signature, transaction) = setup
        .minter()
        .get_all_events()
        .await
        .into_iter()
        .find_map(|event| match event.payload {
            EventType::SubmittedTransaction {
                signature,
                transaction,
                purpose: TransactionPurpose::SweepSplDeposits { deposit_ids },
                ..
            } if deposit_ids == [deposit_id] => Some((signature, transaction)),
            _ => None,
        })
        .expect("the SPL sweep timer should submit the queued deposit");
    assert_eq!(ledger.balance_of(account).await, 0);

    setup.advance_time(FINALIZE_TRANSACTIONS_DELAY).await;
    setup
        .execute_http_mocks(
            // The finalization calls use RPC IDs 24 to 39.
            MockBuilder::with_start_id(24)
                .finalize_transaction(SUBMISSION_BLOCK_HEIGHT)
                .get_transaction(
                    &signature,
                    sweep_transaction_response(&signature, transaction, &owner, &token_program),
                )
                .build(),
        )
        .await;
    setup.minter().assert_that_events().await.satisfy(|events| {
        check!(events.contains(&EventType::CreditedSplSweep {
            signature: signature.clone(),
            // Main pays for two signatures and the rent of the new treasury ATA.
            lamports_spent: 2 * FEE_PER_SIGNATURE + RENT_EXEMPT_LAMPORTS,
            mints: vec![CreditedSplDeposit {
                deposit_id,
                // The fee is zero. Queue the full accepted balance for the user.
                amount_to_mint: BALANCE_ABOVE_MINIMUM
            }],
        }));
    });

    setup.advance_time(PROCESS_PENDING_MINTS_DELAY).await;
    // Run ten PocketIC rounds to complete the ledger call and its reply.
    for _ in 0..10 {
        setup.tick().await;
    }
    assert_eq!(ledger.balance_of(account).await, BALANCE_ABOVE_MINIMUM);
    assert_eq!(
        ledger
            .balance_of(Account {
                subaccount: None,
                ..account
            })
            .await,
        0
    );
    setup.minter().assert_that_events().await.satisfy(|events| {
        // The first mint on this ledger has block index zero.
        check!(events.contains(&EventType::MintedSweptSplDeposit {
            deposit_id,
            mint_block_index: 0
        }));
        check!(!events.iter().any(|event| matches!(
            event,
            EventType::MintedSplDepositFee { .. }
                | EventType::QuarantinedSplSweep { .. }
                | EventType::QuarantinedPendingSplMint { .. }
        )));
    });
    let memo: Memo = minicbor::decode(&get_memo(ledger.get_block(0).await)).unwrap();
    assert_eq!(
        memo,
        Memo::from(MintMemo::sweep(signature.clone(), deposit_id))
    );
    setup.drop().await;
}

#[tokio::test]
async fn should_queue_deposit_for_caller_if_owner_is_omitted() {
    queue_deposit_for_caller(spl_token_interface::id().to_bytes().into(), &[]).await;
}

#[tokio::test]
async fn should_queue_token_2022_deposit_with_immutable_owner() {
    queue_deposit_for_caller(
        spl_token_2022_interface::id().to_bytes().into(),
        &[ExtensionType::ImmutableOwner],
    )
    .await;
}

mod cycles {
    use super::*;

    fn deposit_args() -> DepositSplArgs {
        DepositSplArgs {
            owner: None,
            subaccount: None,
            mint: MINT.into(),
        }
    }

    /// The cycles that the SOL RPC canister charges for one account read as the minter sends it.
    async fn get_account_info_cycles_cost(setup: &Setup, address: Address) -> u128 {
        setup
            .sol_rpc()
            .get_account_info(GetAccountInfoParams::from_pubkey(address))
            .with_encoding(GetAccountInfoEncoding::Base64)
            .with_commitment(CommitmentLevel::Finalized)
            .with_rpc_config(RpcConfig {
                response_size_estimate: Some(2_048),
                response_consensus: Some(ConsensusStrategy::Threshold {
                    min: 3,
                    total: Some(4),
                }),
            })
            .request_cost()
            .send()
            .await
            .expect("Failed to get cycles cost for `getAccountInfo` request")
    }

    /// The cycle balances of the caller and of the minter.
    async fn cycle_balances(setup: &Setup) -> (u128, u128) {
        (
            setup.proxy().cycle_balance().await,
            setup.minter().cycle_balance().await,
        )
    }

    #[tokio::test]
    async fn should_charge_both_reads_and_the_consolidation_fee() {
        let token_program: Address = spl_token_interface::id().to_bytes().into();
        let setup = setup_with_registered_mint(token_program).await;
        let owner: Address = DEFAULT_CALLER_DEPOSIT_ADDRESS.parse().unwrap();
        let source = associated_token_address(&owner, &token_program);
        let destination = associated_token_address(&MINTER_ADDRESS, &token_program);
        let reads_cost = get_account_info_cycles_cost(&setup, source).await
            + get_account_info_cycles_cost(&setup, destination).await;
        assert!(reads_cost > 0);
        let (caller_before, minter_before) = cycle_balances(&setup).await;

        let result = setup
            .minter()
            .with_http_mocks(
                MockBuilder::with_start_id(4)
                    .get_account_info(
                        &source,
                        Some(account_info(token_account_data(&owner, &[]), token_program)),
                    )
                    .get_account_info(&destination, None)
                    .build(),
            )
            .deposit_spl(deposit_args())
            .await;

        assert_eq!(result, Ok(0));
        let (caller_after, minter_after) = cycle_balances(&setup).await;
        assert_eq!(
            caller_before - caller_after,
            reads_cost + Setup::DEFAULT_DEPOSIT_CONSOLIDATION_FEE
        );
        assert_eq!(
            minter_after - minter_before,
            Setup::DEFAULT_DEPOSIT_CONSOLIDATION_FEE
        );
        setup.drop().await;
    }

    #[tokio::test]
    async fn should_charge_both_reads_without_fee_when_the_destination_is_rejected() {
        let token_program: Address = spl_token_interface::id().to_bytes().into();
        let setup = setup_with_registered_mint(token_program).await;
        let owner: Address = DEFAULT_CALLER_DEPOSIT_ADDRESS.parse().unwrap();
        let source = associated_token_address(&owner, &token_program);
        let destination = associated_token_address(&MINTER_ADDRESS, &token_program);
        let reads_cost = get_account_info_cycles_cost(&setup, source).await
            + get_account_info_cycles_cost(&setup, destination).await;
        let (caller_before, minter_before) = cycle_balances(&setup).await;

        let result = setup
            .minter()
            .with_http_mocks(
                MockBuilder::with_start_id(4)
                    .get_account_info(
                        &source,
                        Some(account_info(token_account_data(&owner, &[]), token_program)),
                    )
                    .get_account_info(
                        &destination,
                        Some(account_info(
                            frozen_token_account_data(&MINTER_ADDRESS),
                            token_program,
                        )),
                    )
                    .build(),
            )
            .deposit_spl(deposit_args())
            .await;

        assert_matches!(result, Err(DepositSplError::InvalidTokenAccount(_)));
        let (caller_after, minter_after) = cycle_balances(&setup).await;
        assert_eq!(caller_before - caller_after, reads_cost);
        assert_eq!(minter_after, minter_before);
        setup.drop().await;
    }

    #[tokio::test]
    async fn should_charge_one_read_without_fee_when_the_balance_is_too_small() {
        let token_program: Address = spl_token_interface::id().to_bytes().into();
        let setup = setup_with_registered_mint(token_program).await;
        let owner: Address = DEFAULT_CALLER_DEPOSIT_ADDRESS.parse().unwrap();
        let source = associated_token_address(&owner, &token_program);
        let read_cost = get_account_info_cycles_cost(&setup, source).await;
        let (caller_before, minter_before) = cycle_balances(&setup).await;

        let result = setup
            .minter()
            .with_http_mocks(
                MockBuilder::with_start_id(4)
                    .get_account_info(
                        &source,
                        Some(account_info(
                            token_account_data_with_amount(&owner, MINIMUM_DEPOSIT_AMOUNT - 1),
                            token_program,
                        )),
                    )
                    .build(),
            )
            .deposit_spl(deposit_args())
            .await;

        assert_eq!(
            result,
            Err(DepositSplError::ValueTooSmall {
                balance: MINIMUM_DEPOSIT_AMOUNT - 1,
                minimum_deposit_amount: MINIMUM_DEPOSIT_AMOUNT,
            })
        );
        let (caller_after, minter_after) = cycle_balances(&setup).await;
        assert_eq!(caller_before - caller_after, read_cost);
        assert_eq!(minter_after, minter_before);
        setup.drop().await;
    }

    #[tokio::test]
    async fn should_refund_all_cycles_when_too_few_are_attached() {
        let token_program: Address = spl_token_interface::id().to_bytes().into();
        let setup = setup_with_registered_mint(token_program).await;
        let (caller_before, minter_before) = cycle_balances(&setup).await;

        let result = setup
            .minter()
            .deposit_spl_with_cycles(deposit_args(), Setup::DEFAULT_DEPOSIT_CONSOLIDATION_FEE)
            .await;

        assert_matches!(result, Err(DepositSplError::InsufficientCycles(_)));
        assert_eq!(cycle_balances(&setup).await, (caller_before, minter_before));
        setup.drop().await;
    }
}
