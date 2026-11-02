use base64::{Engine, engine::general_purpose::STANDARD};
use candid::Principal;
use cksol_int_tests::{
    Setup, SetupBuilder,
    fixtures::{
        FINALIZE_TRANSACTIONS_DELAY, MINTER_ADDRESS, MockBuilder, WITHDRAWAL_PROCESSING_DELAY,
        get_memo,
    },
    validator::FEE_PER_SIGNATURE,
};
use cksol_types::{
    Memo, MintMemo, Signature, TxFinalizedStatus, WithdrawSplArgs, WithdrawSplError,
    WithdrawSplLedgerError, WithdrawSplStatus,
};
use cksol_types_internal::{
    UpgradeArgs,
    event::{EventType, TransactionPurpose},
};
use ic_pocket_canister_runtime::JsonRpcResponse;
use icrc_ledger_types::icrc1::account::Account;
use serde_json::json;
use solana_address::{Address, address};
use solana_transaction::Transaction;

pub mod fixtures;
use fixtures::spl::{
    BALANCE_ABOVE_MINIMUM, DECIMALS, DEPOSIT_FEE, MINT, associated_token_address, finalize_sweep,
    queue_deposit, register_token, run_mint_timer, sweep_queued_deposit,
};

const DESTINATION: Address = address!("E4MpwNnMWs2XtW5gVrxZvyS7fMq31QD5HvbxmwP45Tz3");
const AMOUNT: u64 = 500_000;

async fn funded_setup(token_program: Address) -> (Setup, Principal) {
    let setup = SetupBuilder::new()
        .with_proxy_canister()
        .with_ledger_suite_orchestrator(Setup::DEFAULT_CALLER)
        .build()
        .await;
    setup.fund_minter().await;
    setup.top_up_operating_balance().await;
    let ledger_id = setup.install_spl_ledger(DECIMALS).await.canister_id();
    register_token(&setup, ledger_id, token_program, 32).await;
    let account = Account::from(setup.proxy_canister_id());
    let owner: Address = setup.minter().get_deposit_address(account).await.into();
    let deposit = queue_deposit(&setup, account, token_program, &[], 36).await;
    let (signature, transaction) = sweep_queued_deposit(&setup, deposit, 44).await;
    finalize_sweep(&setup, &signature, transaction, &owner, &token_program, 56).await;
    run_mint_timer(&setup).await;
    assert_eq!(
        setup.ledger_with_id(ledger_id).balance_of(account).await,
        BALANCE_ABOVE_MINIMUM - DEPOSIT_FEE
    );
    (setup, ledger_id)
}

fn withdrawal_args(create_destination_account: bool) -> WithdrawSplArgs {
    WithdrawSplArgs {
        from_subaccount: None,
        mint: MINT.into(),
        amount: AMOUNT,
        address: DESTINATION.to_string(),
        create_destination_account,
    }
}

#[derive(Clone, Copy)]
enum WithdrawalOutcome {
    Success { rent_spent: u64 },
    Failure,
}

async fn withdraw_and_finalize(
    token_program: Address,
    create_destination: bool,
    outcome: WithdrawalOutcome,
) {
    let rent_spent = match outcome {
        WithdrawalOutcome::Success { rent_spent } => rent_spent,
        WithdrawalOutcome::Failure => 0,
    };
    let failed = matches!(outcome, WithdrawalOutcome::Failure);
    let (setup, ledger_id) = funded_setup(token_program).await;
    let account = Account::from(setup.proxy_canister_id());
    let spender = setup.minter_account();
    setup.ledger().approve(None, 10_000_000, spender).await;
    setup
        .ledger_with_id(ledger_id)
        .approve(None, AMOUNT, spender)
        .await;
    let sol_before = setup.ledger().balance_of(account).await;
    let spl_before = setup.ledger_with_id(ledger_id).balance_of(account).await;
    let reserved_rent = if !create_destination {
        0
    } else if token_program == spl_token_interface::id().to_bytes().into() {
        2_039_280
    } else {
        2_074_080
    };
    let fee = Setup::DEFAULT_WITHDRAWAL_FEE + reserved_rent;
    let receipt = setup
        .minter()
        .withdraw_spl(withdrawal_args(create_destination))
        .await
        .unwrap();
    let id = receipt.cksol_burn_block_index;
    assert_eq!(
        minicbor::decode::<Memo>(&get_memo(setup.ledger().get_block(id).await)).unwrap(),
        Memo::spl_withdrawal_fee(MINT, AMOUNT, DESTINATION)
    );
    assert_eq!(
        minicbor::decode::<Memo>(&get_memo(
            setup
                .ledger_with_id(ledger_id)
                .get_block(receipt.ckspl_burn_block_index)
                .await
        ))
        .unwrap(),
        Memo::spl_convert(id, DESTINATION)
    );
    assert_eq!(setup.ledger().balance_of(account).await, sol_before - fee);
    assert_eq!(
        setup.ledger_with_id(ledger_id).balance_of(account).await,
        spl_before - AMOUNT
    );
    assert_eq!(
        setup.minter().withdraw_spl_status(id).await,
        WithdrawSplStatus::Pending
    );
    assert_eq!(
        setup.minter().withdraw_spl_status(u64::MAX).await,
        WithdrawSplStatus::NotFound
    );
    let balance_before = setup.minter().get_minter_info().await.balance;

    setup.advance_time(WITHDRAWAL_PROCESSING_DELAY).await;
    setup
        .execute_http_mocks(
            MockBuilder::with_start_id(72)
                .submit_withdrawal_transaction()
                .build(),
        )
        .await;
    let signature = match setup.minter().withdraw_spl_status(id).await {
        WithdrawSplStatus::TxSent { transaction_id } => transaction_id,
        other => panic!("expected TxSent, got {other:?}"),
    };
    let transaction = setup.minter().signed_transaction(&signature).await;
    assert_eq!(transaction.message.signer_keys(), vec![&MINTER_ADDRESS]);
    assert_eq!(transaction.signatures.len(), 1);
    assert_eq!(
        transaction.message.instructions.len(),
        if create_destination { 3 } else { 2 }
    );
    let transfer = transaction.message.instructions.last().unwrap();
    assert_eq!(
        transaction.message.account_keys[transfer.program_id_index as usize].to_bytes(),
        token_program.to_bytes()
    );
    assert_eq!(
        spl_token_2022_interface::instruction::TokenInstruction::unpack(&transfer.data).unwrap(),
        spl_token_2022_interface::instruction::TokenInstruction::TransferChecked {
            amount: AMOUNT,
            decimals: DECIMALS
        }
    );
    assert_eq!(
        transaction.message.account_keys[transfer.accounts[0] as usize].to_bytes(),
        associated_token_address(&MINTER_ADDRESS, &token_program).to_bytes()
    );
    assert_eq!(
        transaction.message.account_keys[transfer.accounts[2] as usize].to_bytes(),
        associated_token_address(&DESTINATION, &token_program).to_bytes()
    );

    setup
        .minter()
        .upgrade(UpgradeArgs::default())
        .await
        .unwrap();
    assert_eq!(
        setup.minter().withdraw_spl_status(id).await,
        WithdrawSplStatus::TxSent {
            transaction_id: signature.clone()
        }
    );
    setup.advance_time(FINALIZE_TRANSACTIONS_DELAY).await;
    setup
        .execute_http_mocks(
            MockBuilder::with_start_id(80)
                .get_advanced_nonce_account()
                .get_transaction(
                    &signature,
                    withdrawal_response(&transaction, rent_spent, failed),
                )
                .build(),
        )
        .await;
    match outcome {
        WithdrawalOutcome::Success { rent_spent } => {
            assert_successful_withdrawal(
                &setup,
                ledger_id,
                id,
                &signature,
                balance_before - FEE_PER_SIGNATURE - rent_spent,
                spl_before - AMOUNT,
                rent_spent,
            )
            .await;
        }
        WithdrawalOutcome::Failure => {
            assert_eq!(
                setup.minter().get_minter_info().await.balance,
                balance_before - FEE_PER_SIGNATURE
            );
            assert_reimbursed_withdrawal(
                &setup,
                ledger_id,
                id,
                &signature,
                sol_before - fee,
                spl_before,
            )
            .await;
        }
    }
    setup.drop().await;
}

async fn assert_reimbursed_withdrawal(
    setup: &Setup,
    ledger_id: Principal,
    id: u64,
    signature: &Signature,
    expected_sol_balance: u64,
    spl_before: u64,
) {
    let account = Account::from(setup.proxy_canister_id());
    let pending = WithdrawSplStatus::ReimbursementPending {
        transaction_id: signature.clone(),
        reimbursed_amount: AMOUNT,
    };
    assert_eq!(setup.minter().withdraw_spl_status(id).await, pending);
    assert_eq!(
        setup.ledger_with_id(ledger_id).balance_of(account).await,
        spl_before - AMOUNT
    );
    setup
        .minter()
        .upgrade(UpgradeArgs::default())
        .await
        .unwrap();
    assert_eq!(setup.minter().withdraw_spl_status(id).await, pending);
    run_mint_timer(setup).await;
    let reimbursed = setup.minter().withdraw_spl_status(id).await;
    let mint_block_index = match &reimbursed {
        WithdrawSplStatus::Reimbursed {
            reimbursed_amount,
            transaction_id,
            mint_block_index,
        } => {
            assert_eq!(*reimbursed_amount, AMOUNT);
            assert_eq!(transaction_id, signature);
            *mint_block_index
        }
        other => panic!("expected token reimbursement, got {other:?}"),
    };
    assert_eq!(
        setup.ledger_with_id(ledger_id).balance_of(account).await,
        spl_before
    );
    assert_eq!(
        setup.ledger().balance_of(account).await,
        expected_sol_balance
    );
    assert_eq!(
        minicbor::decode::<Memo>(&get_memo(
            setup
                .ledger_with_id(ledger_id)
                .get_block(mint_block_index)
                .await
        ))
        .unwrap(),
        Memo::from(MintMemo::spl_withdrawal_reimbursement(id))
    );
    setup.minter().assert_that_events().await.satisfy(|events| {
        assert_eq!(
            events
                .iter()
                .filter(|event| **event
                    == EventType::ReimbursedSplWithdrawal {
                        cksol_burn_block_index: id,
                        mint_block_index,
                    })
                .count(),
            1
        );
    });
    run_mint_timer(setup).await;
    setup
        .minter()
        .upgrade(UpgradeArgs::default())
        .await
        .unwrap();
    assert_eq!(
        setup.ledger_with_id(ledger_id).balance_of(account).await,
        spl_before
    );
    assert_eq!(setup.minter().withdraw_spl_status(id).await, reimbursed);
}

async fn assert_successful_withdrawal(
    setup: &Setup,
    ledger_id: Principal,
    id: u64,
    signature: &Signature,
    expected_minter_balance: u64,
    expected_spl_balance: u64,
    rent_spent: u64,
) {
    let account = Account::from(setup.proxy_canister_id());
    assert_eq!(
        setup.minter().withdraw_spl_status(id).await,
        WithdrawSplStatus::TxFinalized(TxFinalizedStatus::Success {
            transaction_id: signature.clone()
        })
    );
    assert_eq!(
        setup.minter().get_minter_info().await.balance,
        expected_minter_balance
    );
    setup.minter().assert_that_events().await.satisfy(|events| {
        assert!(events.iter().any(|event| matches!(event,
            EventType::SubmittedTransaction { purpose: TransactionPurpose::SplWithdrawal { cksol_burn_block_index }, .. } if *cksol_burn_block_index == id)));
        assert!(events.contains(&EventType::SucceededSplWithdrawalTransaction { signature: signature.clone(), lamports_spent: FEE_PER_SIGNATURE + rent_spent }));
    });
    setup
        .minter()
        .upgrade(UpgradeArgs::default())
        .await
        .unwrap();
    assert_eq!(
        setup.minter().get_minter_info().await.balance,
        expected_minter_balance
    );
    assert_eq!(
        setup.ledger_with_id(ledger_id).balance_of(account).await,
        expected_spl_balance
    );
}

fn withdrawal_response(
    transaction: &Transaction,
    rent_spent: u64,
    failed: bool,
) -> JsonRpcResponse {
    let count = transaction.message.account_keys.len();
    let pre = vec![10_000_000; count];
    let mut post = pre.clone();
    post[0] -= FEE_PER_SIGNATURE + rent_spent;
    let error = if failed {
        json!({"InstructionError": [transaction.message.instructions.len() - 1, {"Custom": 1}]})
    } else {
        serde_json::Value::Null
    };
    let status = if failed {
        json!({"Err": error.clone()})
    } else {
        json!({"Ok": null})
    };
    JsonRpcResponse::from(json!({
        "jsonrpc": "2.0", "id": 1,
        "result": {
            "slot": 350_000_000_u64,
            "transaction": [STANDARD.encode(bincode::serialize(transaction).unwrap()), "base64"],
            "meta": { "err": error, "status": status, "fee": FEE_PER_SIGNATURE,
                "preBalances": pre, "postBalances": post,
                "preTokenBalances": [], "postTokenBalances": [],
                "innerInstructions": [], "logMessages": [], "rewards": [] }
        }
    }))
}

#[tokio::test]
async fn should_withdraw_classic_spl_to_an_existing_ata() {
    withdraw_and_finalize(
        spl_token_interface::id().to_bytes().into(),
        false,
        WithdrawalOutcome::Success { rent_spent: 0 },
    )
    .await;
}

#[tokio::test]
async fn should_reconcile_classic_spl_ata_rent_after_upgrade() {
    for rent_spent in [0, 2_039_280] {
        withdraw_and_finalize(
            spl_token_interface::id().to_bytes().into(),
            true,
            WithdrawalOutcome::Success { rent_spent },
        )
        .await;
    }
}

#[tokio::test]
async fn should_reconcile_token_2022_ata_rent_after_upgrade() {
    for rent_spent in [0, 2_074_080] {
        withdraw_and_finalize(
            spl_token_2022_interface::id().to_bytes().into(),
            true,
            WithdrawalOutcome::Success { rent_spent },
        )
        .await;
    }
}

#[tokio::test]
async fn should_reimburse_tokens_after_definitive_on_chain_failure_and_replay() {
    for token_program in [
        spl_token_interface::id().to_bytes().into(),
        spl_token_2022_interface::id().to_bytes().into(),
    ] {
        withdraw_and_finalize(token_program, true, WithdrawalOutcome::Failure).await;
    }
}

#[tokio::test]
async fn should_refund_the_fee_when_the_token_allowance_is_missing() {
    let (setup, ledger_id) = funded_setup(spl_token_interface::id().to_bytes().into()).await;
    let account = Account::from(setup.proxy_canister_id());
    setup
        .ledger()
        .approve(None, 10_000_000, setup.minter_account())
        .await;
    let sol_before = setup.ledger().balance_of(account).await;
    let spl_before = setup.ledger_with_id(ledger_id).balance_of(account).await;
    let id = match setup.minter().withdraw_spl(withdrawal_args(false)).await {
        Err(WithdrawSplError::CkSplLedgerError {
            cksol_burn_block_index,
            error: WithdrawSplLedgerError::InsufficientAllowance { .. },
        }) => cksol_burn_block_index,
        other => panic!("expected missing token allowance, got {other:?}"),
    };
    let reimbursed_amount = Setup::DEFAULT_WITHDRAWAL_FEE - 500;
    assert_eq!(
        setup.minter().withdraw_spl_status(id).await,
        WithdrawSplStatus::FeeReimbursementPending { reimbursed_amount }
    );
    assert_eq!(
        setup.ledger_with_id(ledger_id).balance_of(account).await,
        spl_before
    );
    setup
        .minter()
        .upgrade(UpgradeArgs::default())
        .await
        .unwrap();
    run_mint_timer(&setup).await;
    assert!(matches!(setup.minter().withdraw_spl_status(id).await,
        WithdrawSplStatus::FeeReimbursed { reimbursed_amount: amount, .. } if amount == reimbursed_amount));
    let sol_after = setup.ledger().balance_of(account).await;
    assert_eq!(sol_after, sol_before - 500);
    run_mint_timer(&setup).await;
    assert_eq!(setup.ledger().balance_of(account).await, sol_after);
    setup.drop().await;
}
