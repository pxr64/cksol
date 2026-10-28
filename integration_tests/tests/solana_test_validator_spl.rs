use cksol_int_tests::{
    Setup,
    fixtures::{
        FINALIZE_TRANSACTIONS_DELAY, MINTER_ADDRESS, PROCESS_PENDING_MINTS_DELAY,
        SWEEP_DEPOSITS_DELAY,
    },
    ledger_init_args::FEE_COLLECTOR_SUBACCOUNT,
    validator::{FEE_PER_SIGNATURE, SolanaTestValidator},
};
use cksol_types::{
    AddSplTokenArgs, DepositSplArgs, DepositSplStatus, GetSplDepositAddressArgs, SplFeeMintStatus,
};
use icrc_ledger_types::icrc1::account::Account;
use solana_address::Address;
use solana_client::rpc_config::CommitmentConfig;
use solana_keypair::{Keypair, Signer};
use solana_native_token::LAMPORTS_PER_SOL;
use solana_program_pack::Pack;
use spl_associated_token_account_interface::address::get_associated_token_address_with_program_id;
use spl_token_interface::state::Account as TokenAccount;

#[tokio::test(flavor = "multi_thread")]
async fn should_sweep_and_mint_a_classic_spl_deposit_on_the_validator() {
    const DECIMALS: u8 = 6;
    const DEPOSIT_AMOUNT: u64 = 10_000_000;
    // Two signatures plus 2,039,280 lamports of ATA rent, with a 20% margin,
    // converted at 150 SPL per SOL and rounded up to six decimal places.
    const EXPECTED_FEE: u64 = 368_871;

    let validator = SolanaTestValidator::start().await;
    let http_mocks = || validator.mock_builder().with_xrc_prices(150.0, 1.0).build();
    let setup = validator
        .setup_builder()
        .with_pocket_ic_non_live_mode()
        .with_nonce_accounts(Vec::new())
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
    let mint = validator.create_mint(&funder, DECIMALS).await;
    let token_program = spl_token_interface::id();
    setup
        .minter()
        .with_http_mocks(http_mocks())
        .add_spl_token(AddSplTokenArgs {
            mint: mint.into(),
            token_program: Address::from(token_program.to_bytes()).into(),
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
            .mint_to(&funder, &mint, &owner, DEPOSIT_AMOUNT)
            .await,
        source
    );
    assert_eq!(validator.get_token_balance(&source).await, DEPOSIT_AMOUNT);
    let treasury: Address = get_associated_token_address_with_program_id(
        &MINTER_ADDRESS.to_bytes().into(),
        &mint.to_bytes().into(),
        &token_program,
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
        .get_minimum_balance_for_rent_exemption(TokenAccount::LEN)
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
            fee: EXPECTED_FEE,
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
    assert_eq!(user_balance, DEPOSIT_AMOUNT - EXPECTED_FEE);
    assert_eq!(fee_balance, EXPECTED_FEE);
    assert_eq!(user_balance + fee_balance, DEPOSIT_AMOUNT);
    assert_eq!(
        setup.minter().deposit_spl_status(deposit_id).await,
        DepositSplStatus::Minted {
            block_index: 0,
            minted_amount: DEPOSIT_AMOUNT - EXPECTED_FEE,
            fee: SplFeeMintStatus::Minted { block_index: 1 },
        }
    );
    setup.drop().await;
}
