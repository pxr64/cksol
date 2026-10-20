use crate::{
    Setup,
    fixtures::{
        FINALIZE_TRANSACTIONS_DELAY, MINTER_ADDRESS, MockBuilder, RENT_EXEMPTION_THRESHOLD,
        SWEEP_DEPOSITS_DELAY,
    },
    validator::{FEE_PER_SIGNATURE, SolanaTestValidator},
};
use icrc_ledger_types::icrc1::account::Account;
use solana_address::Address;
use solana_client::rpc_config::CommitmentConfig;
use solana_keypair::{Keypair, Signer};
use solana_native_token::LAMPORTS_PER_SOL;
use solana_program_pack::Pack;
use solana_system_interface::instruction::create_account;
use solana_transaction::Transaction;
use spl_associated_token_account_interface::{
    address::get_associated_token_address_with_program_id,
    instruction::create_associated_token_account,
};
use spl_token_interface::{
    instruction::{initialize_mint2, mint_to},
    state::Mint,
};
use std::time::Duration;

impl SolanaTestValidator {
    /// A mock builder that forwards every Solana RPC call to this validator.
    pub fn mock_builder(&self) -> MockBuilder {
        MockBuilder::new().with_solana_rpc_url(self.rpc_url())
    }

    /// Funds the minter through its native deposit flow so the SOL is also tracked in state.
    pub async fn fund_minter(&self, setup: &Setup) {
        let account = Account {
            owner: setup.proxy_canister_id(),
            subaccount: None,
        };
        self.fund_deposit_address(setup, account, LAMPORTS_PER_SOL)
            .await;
        setup
            .minter()
            .with_http_mocks(self.mock_builder().build())
            .deposit_sol(account)
            .await
            .expect("deposit_sol should queue the funding deposit");
        setup.advance_time(SWEEP_DEPOSITS_DELAY).await;
        setup.execute_http_mocks(self.mock_builder().build()).await;
        let balance = LAMPORTS_PER_SOL - RENT_EXEMPTION_THRESHOLD - FEE_PER_SIGNATURE;
        self.wait_for_finalized_balance(&MINTER_ADDRESS, balance)
            .await;
        setup.advance_time(FINALIZE_TRANSACTIONS_DELAY).await;
        setup.execute_http_mocks(self.mock_builder().build()).await;
        assert_eq!(setup.minter().get_minter_info().await.balance, balance);
    }

    /// Creates a classic SPL mint on the validator, with `funder` as its mint authority.
    pub async fn create_mint(&self, funder: &Keypair, decimals: u8) -> Address {
        let mint = Keypair::new();
        let rpc = self.rpc_client();
        let rent = rpc
            .get_minimum_balance_for_rent_exemption(Mint::LEN)
            .await
            .unwrap();
        let instructions = [
            create_account(
                &funder.pubkey(),
                &mint.pubkey(),
                rent,
                Mint::LEN as u64,
                &spl_token_interface::id().to_bytes().into(),
            ),
            initialize_mint2(
                &spl_token_interface::id(),
                &mint.pubkey().to_bytes().into(),
                &funder.pubkey().to_bytes().into(),
                None,
                decimals,
            )
            .unwrap(),
        ];
        let blockhash = rpc.get_latest_blockhash().await.unwrap();
        let transaction = Transaction::new_signed_with_payer(
            &instructions,
            Some(&funder.pubkey()),
            &[funder, &mint],
            blockhash,
        );
        let signature = rpc.send_transaction(&transaction).await.unwrap();
        self.confirm_transaction(&signature, CommitmentConfig::finalized())
            .await;
        mint.pubkey()
    }

    /// Creates the ATA of `owner` for `mint`, mints `amount` to it, and returns its address.
    pub async fn mint_to(
        &self,
        funder: &Keypair,
        mint: &Address,
        owner: &Address,
        amount: u64,
    ) -> Address {
        let token_program = spl_token_interface::id();
        let account = get_associated_token_address_with_program_id(
            &owner.to_bytes().into(),
            &mint.to_bytes().into(),
            &token_program,
        );
        let instructions = [
            create_associated_token_account(
                &funder.pubkey().to_bytes().into(),
                &owner.to_bytes().into(),
                &mint.to_bytes().into(),
                &token_program,
            ),
            mint_to(
                &token_program,
                &mint.to_bytes().into(),
                &account,
                &funder.pubkey().to_bytes().into(),
                &[],
                amount,
            )
            .unwrap(),
        ];
        let rpc = self.rpc_client();
        let blockhash = rpc.get_latest_blockhash().await.unwrap();
        let transaction = Transaction::new_signed_with_payer(
            &instructions,
            Some(&funder.pubkey()),
            &[funder],
            blockhash,
        );
        let signature = rpc.send_transaction(&transaction).await.unwrap();
        self.confirm_transaction(&signature, CommitmentConfig::finalized())
            .await;
        account.to_bytes().into()
    }

    pub async fn get_token_balance(&self, address: &Address) -> u64 {
        self.rpc_client()
            .get_token_account_balance_with_commitment(address, CommitmentConfig::finalized())
            .await
            .expect("Failed to read the SPL token balance")
            .value
            .amount
            .parse()
            .unwrap()
    }

    /// Waits for the real token transfer to be finalized before the minter settles it.
    pub async fn wait_for_finalized_token_balance(&self, address: &Address, expected_balance: u64) {
        let rpc = self.rpc_client();
        let mut last_balance = None;
        for _ in 0..60 {
            last_balance = rpc
                .get_token_account_balance_with_commitment(address, CommitmentConfig::finalized())
                .await
                .ok()
                .and_then(|response| response.value.amount.parse::<u64>().ok());
            if last_balance == Some(expected_balance) {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        panic!(
            "Token balance of {address} at finalized commitment is {last_balance:?}, expected {expected_balance}"
        );
    }
}
