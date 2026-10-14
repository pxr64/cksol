use assert2::check;
use cksol_int_tests::{
    Setup, SetupBuilder,
    fixtures::{
        FINALIZE_TRANSACTIONS_DELAY, MINTER_ADDRESS, MockBuilder, PROCESS_PENDING_MINTS_DELAY,
        SUBMISSION_BLOCK_HEIGHT, SWEEP_DEPOSITS_DELAY, get_memo,
    },
    validator::FEE_PER_SIGNATURE,
};
use cksol_types::{DepositSplArgs, GetSplDepositAddressArgs, Memo, MintMemo};
use cksol_types_internal::event::{CreditedSplDeposit, EventType, TransactionPurpose};
use icrc_ledger_types::icrc1::account::Account;
use solana_address::Address;
use spl_token_2022_interface::extension::ExtensionType;

mod fixtures;

use fixtures::spl::{
    BALANCE_ABOVE_MINIMUM, DECIMALS, MINT, RENT_EXEMPT_LAMPORTS, account_info,
    associated_token_address, queue_deposit_for_caller, register_token, sweep_transaction_response,
    token_account_data,
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
