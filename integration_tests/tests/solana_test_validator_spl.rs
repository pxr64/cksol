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
    TxFinalizedStatus, WithdrawSplArgs, WithdrawSplStatus,
};
use icrc_ledger_types::icrc1::account::Account;
use solana_address::Address;
use solana_client::rpc_config::CommitmentConfig;
use solana_keypair::{Keypair, Signer};
use solana_native_token::LAMPORTS_PER_SOL;
use solana_program_pack::Pack;
use spl_associated_token_account_interface::address::get_associated_token_address_with_program_id;
use spl_token_2022_interface::{extension::ExtensionType, state::Account as TokenAccount};

struct SplDeposit {
    setup: Setup,
    funder: Keypair,
    mint: Address,
    treasury: Address,
    account: Account,
    ledger_id: candid::Principal,
}

/// Reuses the real deposit flow to establish treasury backing and the user's ckSPL balance.
async fn sweep_and_mint_spl_deposit(
    validator: &SolanaTestValidator,
    nonce_accounts: Vec<String>,
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
    let setup = validator
        .setup_builder()
        .with_pocket_ic_non_live_mode()
        .with_nonce_accounts(nonce_accounts)
        .with_ledger_suite_orchestrator(Setup::DEFAULT_CALLER)
        .build()
        .await;
    let ledger = setup.install_spl_ledger(DECIMALS).await;
    validator.fund_minter(&setup).await;
    setup.top_up_operating_balance().await;

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
        Setup::OPERATING_TOP_UP - 2 * FEE_PER_SIGNATURE - rent
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
        setup,
        funder,
        mint,
        treasury,
        account,
    }
}

async fn withdraw_spl_to_an_existing_ata_on_the_validator(token_program: Address) {
    const AMOUNT: u64 = 500_000;

    let validator = SolanaTestValidator::start().await;
    let nonce_account = validator.create_nonce_accounts(1, &MINTER_ADDRESS).await[0];
    let SplDeposit {
        setup,
        funder,
        mint,
        treasury,
        account,
        ledger_id,
    } = sweep_and_mint_spl_deposit(&validator, vec![nonce_account.to_string()], token_program)
        .await;
    let ledger = setup.ledger_with_id(ledger_id);
    let destination = Keypair::new();
    let destination_ata = validator
        .mint_to_for_program(&funder, &mint, &destination.pubkey(), 0, token_program)
        .await;
    assert_eq!(validator.get_token_balance(&destination_ata).await, 0);

    // The native funding helper mints ckSOL to the default account; fund the token subaccount's fee.
    let fee = Setup::DEFAULT_WITHDRAWAL_FEE;
    setup
        .ledger()
        .transfer(account, fee + LEDGER_TRANSFER_FEE)
        .await;
    setup
        .ledger()
        .approve(account.subaccount, fee, setup.minter_account())
        .await;
    ledger
        .approve(account.subaccount, AMOUNT, setup.minter_account())
        .await;
    let cksol_before = setup.ledger().balance_of(account).await;
    let ckspl_before = ledger.balance_of(account).await;
    let treasury_before = validator.get_token_balance(&treasury).await;
    let sol_before = validator.get_balance(&MINTER_ADDRESS).await;
    let tracked_sol_before = setup.minter().get_minter_info().await.balance;
    let nonce_before = validator.get_nonce_value(&nonce_account).await;
    let receipt = setup
        .minter()
        .withdraw_spl(WithdrawSplArgs {
            from_subaccount: account.subaccount,
            mint: mint.into(),
            amount: AMOUNT,
            address: destination.pubkey().to_string(),
            create_destination_account: false,
        })
        .await
        .expect("withdraw_spl should burn the fee and tokens");
    assert_eq!(setup.ledger().balance_of(account).await, cksol_before - fee);
    assert_eq!(ledger.balance_of(account).await, ckspl_before - AMOUNT);
    assert_eq!(
        setup
            .minter()
            .withdraw_spl_status(receipt.cksol_burn_block_index)
            .await,
        WithdrawSplStatus::Pending
    );

    // Forward RPC calls to the validator so it executes the actual nonce and SPL instructions.
    setup.advance_time(WITHDRAWAL_PROCESSING_DELAY).await;
    setup
        .execute_http_mocks(validator.mock_builder().build())
        .await;
    validator
        .wait_for_finalized_token_balance(&destination_ata, AMOUNT)
        .await;
    assert_eq!(
        validator.get_token_balance(&treasury).await,
        treasury_before - AMOUNT
    );
    assert_ne!(
        validator.get_nonce_value(&nonce_account).await,
        nonce_before
    );
    assert_eq!(
        validator.get_balance(&MINTER_ADDRESS).await,
        sol_before - FEE_PER_SIGNATURE
    );

    setup.advance_time(FINALIZE_TRANSACTIONS_DELAY).await;
    setup
        .execute_http_mocks(validator.mock_builder().build())
        .await;
    let signature = match setup
        .minter()
        .withdraw_spl_status(receipt.cksol_burn_block_index)
        .await
    {
        WithdrawSplStatus::TxFinalized(TxFinalizedStatus::Success { transaction_id }) => {
            transaction_id
        }
        status => panic!("expected a successful SPL withdrawal, got {status:?}"),
    };
    let transaction = setup.minter().signed_transaction(&signature).await;
    assert_eq!(transaction.signatures.len(), 1);
    assert_eq!(transaction.message.instructions.len(), 2);
    let transfer = transaction.message.instructions.last().unwrap();
    assert_eq!(
        transaction.message.account_keys[transfer.program_id_index as usize],
        token_program
    );
    assert_eq!(
        setup.minter().get_minter_info().await.balance,
        tracked_sol_before - FEE_PER_SIGNATURE
    );
    assert_eq!(ledger.balance_of(account).await, ckspl_before - AMOUNT);
    assert_eq!(setup.ledger().balance_of(account).await, cksol_before - fee);
    setup.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn should_sweep_and_mint_a_classic_spl_deposit_on_the_validator() {
    let validator = SolanaTestValidator::start().await;
    let deposit = sweep_and_mint_spl_deposit(
        &validator,
        Vec::new(),
        spl_token_interface::id().to_bytes().into(),
    )
    .await;
    deposit.setup.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn should_withdraw_classic_spl_to_an_existing_ata_on_the_validator() {
    withdraw_spl_to_an_existing_ata_on_the_validator(spl_token_interface::id().to_bytes().into())
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn should_withdraw_token_2022_to_an_existing_ata_on_the_validator() {
    withdraw_spl_to_an_existing_ata_on_the_validator(
        spl_token_2022_interface::id().to_bytes().into(),
    )
    .await;
}
