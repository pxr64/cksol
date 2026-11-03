use cksol_int_tests::{
    Setup,
    fixtures::{
        FINALIZE_TRANSACTIONS_DELAY, MINTER_ADDRESS, PROCESS_PENDING_MINTS_DELAY,
        SWEEP_DEPOSITS_DELAY, WITHDRAWAL_PROCESSING_DELAY,
    },
    ledger_init_args::{FEE_COLLECTOR_SUBACCOUNT, LEDGER_TRANSFER_FEE},
    validator::{FEE_PER_SIGNATURE, SolanaTestValidator},
};
use cksol_types::{
    AddSplTokenArgs, DepositSplArgs, DepositSplStatus, GetSplDepositAddressArgs, SplFeeMintStatus,
    TxFinalizedStatus, WithdrawSplArgs, WithdrawSplOk, WithdrawSplStatus,
};
use cksol_types_internal::event::EventType;
use icrc_ledger_types::icrc1::account::Account;
use solana_address::Address;
use solana_client::rpc_config::CommitmentConfig;
use solana_keypair::{Keypair, Signer};
use solana_native_token::LAMPORTS_PER_SOL;
use solana_program_pack::Pack;
use spl_associated_token_account_interface::address::get_associated_token_address_with_program_id;
use spl_token_2022_interface::{extension::ExtensionType, state::Account as TokenAccount};

struct SplDeposit {
    funder: Keypair,
    mint: Address,
    token_program: Address,
    treasury: Address,
    account: Account,
    ledger_id: candid::Principal,
}

struct SplWithdrawal {
    deposit: SplDeposit,
    amount: u64,
    destination_ata: Address,
    destination_rent: u64,
    fee: u64,
    cksol_before: u64,
    ckspl_before: u64,
    treasury_before: u64,
    receipt: WithdrawSplOk,
}

/// Reuses the real deposit flow to establish treasury backing and the user's ckSPL balance.
async fn sweep_and_mint_spl_deposit(
    validator: &SolanaTestValidator,
    nonce_accounts: Vec<String>,
    token_program: Address,
) -> (Setup, SplDeposit) {
    let setup = validator
        .setup_builder()
        .with_pocket_ic_non_live_mode()
        .with_nonce_accounts(nonce_accounts)
        .with_ledger_suite_orchestrator(Setup::DEFAULT_CALLER)
        .build()
        .await;
    validator.fund_minter(&setup).await;
    setup.top_up_operating_balance().await;
    let deposit = sweep_and_mint_spl_deposit_on_setup(validator, &setup, token_program).await;
    (setup, deposit)
}

/// Funds another mint on the same minter through the existing sweep and mint flow.
async fn sweep_and_mint_spl_deposit_on_setup(
    validator: &SolanaTestValidator,
    setup: &Setup,
    token_program: Address,
) -> SplDeposit {
    const DECIMALS: u8 = 6;
    const DEPOSIT_AMOUNT: u64 = 10_000_000;
    // Two signatures plus the program's ATA rent, with a 20% margin,
    // converted at 150 SPL per SOL and rounded up to six decimal places.
    let (token_account_len, expected_fee) =
        if token_program == spl_token_interface::id().to_bytes().into() {
            (TokenAccount::LEN, 368_871)
        } else {
            (
                ExtensionType::try_calculate_account_len::<TokenAccount>(&[
                    ExtensionType::ImmutableOwner,
                ])
                .unwrap(),
                375_135,
            )
        };

    let http_mocks = || validator.mock_builder().with_xrc_prices(150.0, 1.0).build();
    let ledger = setup.install_spl_ledger(DECIMALS).await;
    let operating_before = setup.minter().get_operating_balance().await.balance;

    let funder = Keypair::new();
    validator
        .airdrop_and_confirm(funder.pubkey(), LAMPORTS_PER_SOL)
        .await;
    let mint = validator
        .create_mint_for_program(&funder, DECIMALS, token_program)
        .await;
    setup
        .minter()
        .with_http_mocks(http_mocks())
        .add_spl_token(AddSplTokenArgs {
            mint: mint.into(),
            token_program: token_program.into(),
            decimals: DECIMALS,
            ledger_id: ledger.canister_id(),
            minimum_deposit_amount: 1_000_000,
            paused: false,
            xrc_symbol: "SPL".to_string(),
            token_symbol: Some("ckSPL".to_string()),
        })
        .await
        .expect("add_spl_token should validate the mint on the validator");

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
            mint: mint.into(),
        })
        .await
        .expect("the registered mint should have a deposit address")
        .into();
    assert_eq!(
        validator
            .mint_to_for_program(&funder, &mint, &owner, DEPOSIT_AMOUNT, token_program)
            .await,
        source
    );
    assert_eq!(validator.get_token_balance(&source).await, DEPOSIT_AMOUNT);
    let treasury: Address = get_associated_token_address_with_program_id(
        &MINTER_ADDRESS.to_bytes().into(),
        &mint.to_bytes().into(),
        &token_program.to_bytes().into(),
    )
    .to_bytes()
    .into();
    let rpc = validator.rpc_client();
    assert!(
        rpc.get_account_with_commitment(&treasury, CommitmentConfig::finalized())
            .await
            .unwrap()
            .value
            .is_none()
    );
    let sol_before = validator.get_balance(&MINTER_ADDRESS).await;
    let rent = rpc
        .get_minimum_balance_for_rent_exemption(token_account_len)
        .await
        .unwrap();
    let deposit_id = setup
        .minter()
        .with_http_mocks(http_mocks())
        .deposit_spl(DepositSplArgs {
            owner: None,
            subaccount: account.subaccount,
            mint: mint.into(),
        })
        .await
        .expect("deposit_spl should read the real source balance and quote the fee");
    assert_eq!(
        setup.minter().deposit_spl_status(deposit_id).await,
        DepositSplStatus::Queued {
            balance: DEPOSIT_AMOUNT,
            fee: expected_fee,
        }
    );
    assert_eq!(ledger.balance_of(account).await, 0);

    // The minter signs and submits the sweep; the validator executes the real SPL instructions.
    setup.advance_time(SWEEP_DEPOSITS_DELAY).await;
    setup.execute_http_mocks(http_mocks()).await;
    validator
        .wait_for_finalized_token_balance(&treasury, DEPOSIT_AMOUNT)
        .await;
    assert_eq!(validator.get_token_balance(&source).await, 0);
    assert_eq!(ledger.balance_of(account).await, 0);
    let sol_after = sol_before - 2 * FEE_PER_SIGNATURE - rent;
    assert_eq!(validator.get_balance(&MINTER_ADDRESS).await, sol_after);

    // Settle the validator's transaction metadata, then mint the recorded net amount and fee.
    setup.advance_time(FINALIZE_TRANSACTIONS_DELAY).await;
    setup.execute_http_mocks(http_mocks()).await;
    setup.advance_time(PROCESS_PENDING_MINTS_DELAY).await;
    setup.execute_http_mocks(http_mocks()).await;
    assert_eq!(setup.minter().get_minter_info().await.balance, sol_after);
    assert_eq!(
        setup.minter().get_operating_balance().await.balance,
        operating_before - 2 * FEE_PER_SIGNATURE - rent
    );
    let user_balance = ledger.balance_of(account).await;
    let fee_balance = ledger
        .balance_of(Account {
            owner: setup.minter_canister_id(),
            subaccount: Some(FEE_COLLECTOR_SUBACCOUNT),
        })
        .await;
    assert_eq!(user_balance, DEPOSIT_AMOUNT - expected_fee);
    assert_eq!(fee_balance, expected_fee);
    assert_eq!(user_balance + fee_balance, DEPOSIT_AMOUNT);
    assert_eq!(
        setup.minter().deposit_spl_status(deposit_id).await,
        DepositSplStatus::Minted {
            block_index: 0,
            minted_amount: DEPOSIT_AMOUNT - expected_fee,
            fee: SplFeeMintStatus::Minted { block_index: 1 },
        }
    );
    SplDeposit {
        ledger_id: ledger.canister_id(),
        funder,
        mint,
        token_program,
        treasury,
        account,
    }
}

async fn withdraw_spl_on_the_validator(token_program: Address, create_destination_account: bool) {
    const AMOUNT: u64 = 500_000;

    let validator = SolanaTestValidator::start().await;
    let nonce_account = validator.create_nonce_accounts(1, &MINTER_ADDRESS).await[0];
    let (setup, deposit) =
        sweep_and_mint_spl_deposit(&validator, vec![nonce_account.to_string()], token_program)
            .await;
    let destination = Keypair::new().pubkey();
    let sol_before = validator.get_balance(&MINTER_ADDRESS).await;
    let tracked_sol_before = setup.minter().get_minter_info().await.balance;
    let nonce_before = validator.get_nonce_value(&nonce_account).await;
    let withdrawal = queue_spl_withdrawal_on_the_validator(
        &validator,
        &setup,
        deposit,
        &destination,
        AMOUNT,
        create_destination_account,
    )
    .await;

    // Forward RPC calls to the validator so it executes the actual nonce and SPL instructions.
    setup.advance_time(WITHDRAWAL_PROCESSING_DELAY).await;
    setup
        .execute_http_mocks(validator.mock_builder().build())
        .await;
    validator
        .wait_for_finalized_token_balance(&withdrawal.destination_ata, AMOUNT)
        .await;
    assert_eq!(
        validator
            .get_token_balance(&withdrawal.deposit.treasury)
            .await,
        withdrawal.treasury_before - AMOUNT
    );
    assert_ne!(
        validator.get_nonce_value(&nonce_account).await,
        nonce_before
    );
    assert_eq!(
        validator.get_balance(&MINTER_ADDRESS).await,
        sol_before - FEE_PER_SIGNATURE - withdrawal.destination_rent
    );
    if create_destination_account {
        assert_eq!(
            validator.get_balance(&withdrawal.destination_ata).await,
            withdrawal.destination_rent
        );
    }

    setup.advance_time(FINALIZE_TRANSACTIONS_DELAY).await;
    setup
        .execute_http_mocks(validator.mock_builder().build())
        .await;
    let signature = match setup
        .minter()
        .withdraw_spl_status(withdrawal.receipt.cksol_burn_block_index)
        .await
    {
        WithdrawSplStatus::TxFinalized(TxFinalizedStatus::Success { transaction_id }) => {
            transaction_id
        }
        status => panic!("expected a successful SPL withdrawal, got {status:?}"),
    };
    let transaction = setup.minter().signed_transaction(&signature).await;
    assert_eq!(transaction.signatures.len(), 1);
    assert_eq!(
        transaction.message.instructions.len(),
        if create_destination_account { 3 } else { 2 }
    );
    let transfer = transaction.message.instructions.last().unwrap();
    assert_eq!(
        transaction.message.account_keys[transfer.program_id_index as usize],
        token_program
    );
    assert_eq!(
        setup.minter().get_minter_info().await.balance,
        tracked_sol_before - FEE_PER_SIGNATURE - withdrawal.destination_rent
    );
    assert_eq!(
        setup
            .ledger_with_id(withdrawal.deposit.ledger_id)
            .balance_of(withdrawal.deposit.account)
            .await,
        withdrawal.ckspl_before - AMOUNT
    );
    assert_eq!(
        setup.ledger().balance_of(withdrawal.deposit.account).await,
        withdrawal.cksol_before - withdrawal.fee
    );
    setup.drop().await;
}

/// Queues a withdrawal without advancing time, so multiple requests can enter the same batch.
async fn queue_spl_withdrawal_on_the_validator(
    validator: &SolanaTestValidator,
    setup: &Setup,
    deposit: SplDeposit,
    destination: &Address,
    amount: u64,
    create_destination_account: bool,
) -> SplWithdrawal {
    let destination_ata: Address = get_associated_token_address_with_program_id(
        &destination.to_bytes().into(),
        &deposit.mint.to_bytes().into(),
        &deposit.token_program.to_bytes().into(),
    )
    .to_bytes()
    .into();
    let destination_rent = if create_destination_account {
        let rpc = validator.rpc_client();
        assert!(
            rpc.get_account_with_commitment(&destination_ata, CommitmentConfig::finalized())
                .await
                .unwrap()
                .value
                .is_none()
        );
        let account_len = rpc.get_account(&deposit.treasury).await.unwrap().data.len();
        rpc.get_minimum_balance_for_rent_exemption(account_len)
            .await
            .unwrap()
    } else {
        assert_eq!(
            validator
                .mint_to_for_program(
                    &deposit.funder,
                    &deposit.mint,
                    destination,
                    0,
                    deposit.token_program,
                )
                .await,
            destination_ata
        );
        assert_eq!(validator.get_token_balance(&destination_ata).await, 0);
        0
    };
    let fee = Setup::DEFAULT_WITHDRAWAL_FEE + destination_rent;
    setup
        .ledger()
        .transfer(deposit.account, fee + LEDGER_TRANSFER_FEE)
        .await;
    setup
        .ledger()
        .approve(deposit.account.subaccount, fee, setup.minter_account())
        .await;
    let ledger = setup.ledger_with_id(deposit.ledger_id);
    ledger
        .approve(deposit.account.subaccount, amount, setup.minter_account())
        .await;
    let cksol_before = setup.ledger().balance_of(deposit.account).await;
    let ckspl_before = ledger.balance_of(deposit.account).await;
    let treasury_before = validator.get_token_balance(&deposit.treasury).await;
    let receipt = setup
        .minter()
        .withdraw_spl(WithdrawSplArgs {
            from_subaccount: deposit.account.subaccount,
            mint: deposit.mint.into(),
            amount,
            address: destination.to_string(),
            create_destination_account,
        })
        .await
        .expect("withdraw_spl should burn the fee and tokens");
    assert_eq!(
        setup.ledger().balance_of(deposit.account).await,
        cksol_before - fee
    );
    assert_eq!(
        ledger.balance_of(deposit.account).await,
        ckspl_before - amount
    );
    assert_eq!(
        setup
            .minter()
            .withdraw_spl_status(receipt.cksol_burn_block_index)
            .await,
        WithdrawSplStatus::Pending
    );
    SplWithdrawal {
        deposit,
        amount,
        destination_ata,
        destination_rent,
        fee,
        cksol_before,
        ckspl_before,
        treasury_before,
        receipt,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_batch_classic_spl_and_token_2022_withdrawals_on_the_validator() {
    let validator = SolanaTestValidator::start().await;
    let nonce_account = validator.create_nonce_accounts(1, &MINTER_ADDRESS).await[0];
    let (setup, classic) = sweep_and_mint_spl_deposit(
        &validator,
        vec![nonce_account.to_string()],
        spl_token_interface::id().to_bytes().into(),
    )
    .await;
    let token_2022 = sweep_and_mint_spl_deposit_on_setup(
        &validator,
        &setup,
        spl_token_2022_interface::id().to_bytes().into(),
    )
    .await;
    let destination = Keypair::new().pubkey();
    let sol_before = validator.get_balance(&MINTER_ADDRESS).await;
    let tracked_sol_before = setup.minter().get_minter_info().await.balance;
    let nonce_before = validator.get_nonce_value(&nonce_account).await;

    // Both burns finish before the processing timer runs, so the one free nonce serves both.
    let withdrawals = [
        queue_spl_withdrawal_on_the_validator(
            &validator,
            &setup,
            classic,
            &destination,
            500_000,
            true,
        )
        .await,
        queue_spl_withdrawal_on_the_validator(
            &validator,
            &setup,
            token_2022,
            &destination,
            700_000,
            true,
        )
        .await,
    ];
    let burn_indices: Vec<_> = withdrawals
        .iter()
        .map(|withdrawal| withdrawal.receipt.cksol_burn_block_index)
        .collect();
    assert!(burn_indices[0] < burn_indices[1]);
    let rent: u64 = withdrawals.iter().map(|w| w.destination_rent).sum();

    setup.advance_time(WITHDRAWAL_PROCESSING_DELAY).await;
    setup
        .execute_http_mocks(validator.mock_builder().build())
        .await;
    for withdrawal in &withdrawals {
        validator
            .wait_for_finalized_token_balance(&withdrawal.destination_ata, withdrawal.amount)
            .await;
        assert_eq!(
            validator
                .get_token_balance(&withdrawal.deposit.treasury)
                .await,
            withdrawal.treasury_before - withdrawal.amount
        );
        assert_eq!(
            validator.get_balance(&withdrawal.destination_ata).await,
            withdrawal.destination_rent
        );
    }
    assert_ne!(
        validator.get_nonce_value(&nonce_account).await,
        nonce_before
    );
    // Both ATA rents are paid, but the two transfers share one signature fee.
    let spent = FEE_PER_SIGNATURE + rent;
    assert_eq!(
        validator.get_balance(&MINTER_ADDRESS).await,
        sol_before - spent
    );

    setup.advance_time(FINALIZE_TRANSACTIONS_DELAY).await;
    setup
        .execute_http_mocks(validator.mock_builder().build())
        .await;
    let mut signatures = Vec::new();
    for withdrawal in &withdrawals {
        let status = setup
            .minter()
            .withdraw_spl_status(withdrawal.receipt.cksol_burn_block_index)
            .await;
        let WithdrawSplStatus::TxFinalized(TxFinalizedStatus::Success { transaction_id }) = status
        else {
            panic!("expected a successful batched SPL withdrawal, got {status:?}");
        };
        signatures.push(transaction_id);
        assert_eq!(
            setup
                .ledger_with_id(withdrawal.deposit.ledger_id)
                .balance_of(withdrawal.deposit.account)
                .await,
            withdrawal.ckspl_before - withdrawal.amount
        );
    }
    assert_eq!(signatures[0], signatures[1]);
    let transaction = setup.minter().signed_transaction(&signatures[0]).await;
    assert_eq!(transaction.signatures.len(), 1);
    assert_eq!(transaction.message.header.num_required_signatures, 1);
    assert_eq!(transaction.message.recent_blockhash, nonce_before);
    assert_eq!(transaction.message.instructions.len(), 5); // Advance nonce, two ATA creates, two transfers.
    let nonce_instruction = &transaction.message.instructions[0];
    let advance_nonce = solana_system_interface::instruction::advance_nonce_account(
        &nonce_account,
        &MINTER_ADDRESS,
    );
    assert_eq!(
        transaction.message.account_keys[nonce_instruction.program_id_index as usize],
        advance_nonce.program_id
    );
    assert_eq!(nonce_instruction.data, advance_nonce.data);
    assert_eq!(
        transaction.message.account_keys[nonce_instruction.accounts[0] as usize],
        nonce_account
    );
    for (transfer, withdrawal) in transaction.message.instructions[3..]
        .iter()
        .zip(&withdrawals)
    {
        assert_eq!(
            transaction.message.account_keys[transfer.program_id_index as usize],
            withdrawal.deposit.token_program
        );
    }

    let events = setup.minter().get_all_events().await;
    let batches: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventType::CreatedSplWithdrawalTransaction {
                burn_indices,
                nonce_account,
                ..
            } => Some((burn_indices.clone(), Address::from(nonce_account.clone()))),
            _ => None,
        })
        .collect();
    assert_eq!(batches, vec![(burn_indices, nonce_account)]);
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventType::SucceededSplWithdrawalTransaction { signature, lamports_spent }
            if signature == &signatures[0] && *lamports_spent == spent
    )));
    // Finalization releases the unused individual-retry reservations back to the minter.
    assert_eq!(
        setup.minter().get_minter_info().await.balance,
        tracked_sol_before - spent
    );
    setup.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn should_sweep_and_mint_a_classic_spl_deposit_on_the_validator() {
    let validator = SolanaTestValidator::start().await;
    let (setup, _) = sweep_and_mint_spl_deposit(
        &validator,
        Vec::new(),
        spl_token_interface::id().to_bytes().into(),
    )
    .await;
    setup.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn should_withdraw_classic_spl_to_an_existing_ata_on_the_validator() {
    withdraw_spl_on_the_validator(spl_token_interface::id().to_bytes().into(), false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn should_withdraw_token_2022_to_an_existing_ata_on_the_validator() {
    withdraw_spl_on_the_validator(spl_token_2022_interface::id().to_bytes().into(), false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn should_create_a_classic_spl_destination_ata_on_the_validator() {
    withdraw_spl_on_the_validator(spl_token_interface::id().to_bytes().into(), true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn should_create_a_token_2022_destination_ata_on_the_validator() {
    withdraw_spl_on_the_validator(spl_token_2022_interface::id().to_bytes().into(), true).await;
}
