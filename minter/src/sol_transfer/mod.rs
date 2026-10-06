use crate::{
    address::{
        DerivationPath, account_address, associated_token_address, lazy_get_schnorr_master_key,
        minter_address,
    },
    constants::FEE_PER_SIGNATURE,
    runtime::CanisterRuntime,
    signer::{SchnorrSigner, sign_bytes},
    state::{
        QueuedSplDeposit, SchnorrPublicKey, SplSweep, SupportedSplToken, Sweep, event::Signer,
    },
};
use derive_more::From;
use ic_cdk_management_canister::SignCallError;
use icrc_ledger_types::icrc1::account::Account;
use sol_rpc_types::Lamport;
use solana_address::Address;
use solana_hash::Hash;
use solana_system_interface::instruction;
use solana_transaction::{Instruction, Message, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

#[cfg(test)]
mod tests;

pub const MAX_SIGNATURES: u64 = 10;
pub const MAX_TX_SIZE: usize = 1_232;
const BYTES_PER_SIGNATURE: usize = 64;

/// Upper bound on the number of withdrawal transfers that fit in a single
/// Solana transaction when the fee-payer is the only signer.
pub const MAX_WITHDRAWALS_PER_TX: usize = 20;

/// Fee charged for a batch withdrawal transaction, which is signed by the fee payer only.
pub const BATCH_WITHDRAWAL_TX_FEE: Lamport = FEE_PER_SIGNATURE;

#[derive(Debug, Error, From)]
pub enum CreateTransferError {
    #[error("transaction size {got} exceeds maximum of {max} bytes")]
    TransactionTooLarge { max: usize, got: usize },
    #[error("signing failed: {0}")]
    SigningFailed(SignCallError),
}

/// Builds a checked transfer of the queued token balance to the minter's
/// associated token account. The destination account must already exist.
fn create_spl_sweep_instruction(
    deposit: &QueuedSplDeposit,
    token: &SupportedSplToken,
    deposit_owner: &Address,
    minter: &Address,
) -> Instruction {
    assert_eq!(deposit.mint, token.mint, "BUG: SPL sweep mint mismatch");
    let token_program = token.token_program.id();
    assert_eq!(
        deposit.address,
        associated_token_address(deposit_owner, &token.mint, &token_program),
        "BUG: SPL sweep source mismatch"
    );
    let destination = associated_token_address(minter, &token.mint, &token_program);
    spl_token_2022_interface::instruction::transfer_checked(
        &token_program.to_bytes().into(),
        &deposit.address.to_bytes().into(),
        &token.mint.to_bytes().into(),
        &destination.to_bytes().into(),
        &deposit_owner.to_bytes().into(),
        &[],
        deposit.balance,
        token.decimals,
    )
    .expect("BUG: supported SPL token program must accept transfer_checked")
}

/// Builds the message of an SPL sweep, with one creation of each destination token account.
/// The minter pays the creations and the fees, and each deposit owner authorizes its transfers.
pub fn create_spl_sweep_message(
    sweep: &SplSweep,
    tokens: &BTreeMap<Address, SupportedSplToken>,
    master_key: &SchnorrPublicKey,
    recent_blockhash: Hash,
) -> Message {
    let minter = minter_address(master_key);
    let mut destinations = BTreeSet::new();
    let mut instructions = Vec::new();
    for deposit in sweep.deposits().values() {
        let token = tokens
            .get(&deposit.mint)
            .unwrap_or_else(|| panic!("BUG: SPL sweep mint {} must be registered", deposit.mint));
        let owner = account_address(master_key, &deposit.account);
        let token_program = token.token_program.id();
        let destination = associated_token_address(&minter, &token.mint, &token_program);
        if destinations.insert(destination) {
            instructions.push(
                spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
                    &minter.to_bytes().into(),
                    &minter.to_bytes().into(),
                    &token.mint.to_bytes().into(),
                    &token_program.to_bytes().into(),
                ),
            );
        }
        instructions.push(create_spl_sweep_instruction(
            deposit, token, &owner, &minter,
        ));
    }
    Message::new_with_blockhash(&instructions, Some(&minter), &recent_blockhash)
}

/// Signs an SPL sweep with the minter and each distinct deposit owner.
/// Returns the signed transaction and its signers in signature order.
pub async fn sign_spl_sweep_transaction<R: CanisterRuntime>(
    runtime: &R,
    sweep: &SplSweep,
    tokens: &BTreeMap<Address, SupportedSplToken>,
    recent_blockhash: Hash,
) -> Result<(Transaction, Vec<Signer>), CreateTransferError> {
    let master_key = lazy_get_schnorr_master_key(runtime).await;
    let minter = minter_address(&master_key);
    let accounts_by_address: BTreeMap<Address, Account> = sweep
        .deposits()
        .values()
        .map(|deposit| {
            (
                account_address(&master_key, &deposit.account),
                deposit.account,
            )
        })
        .collect();
    let mut transaction = Transaction::new_unsigned(create_spl_sweep_message(
        sweep,
        tokens,
        &master_key,
        recent_blockhash,
    ));
    let signers: Vec<Signer> = transaction
        .message
        .signer_keys()
        .iter()
        .map(|key| {
            if **key == minter {
                Signer::Minter
            } else {
                let account = accounts_by_address
                    .get(key)
                    .copied()
                    .unwrap_or_else(|| panic!("BUG: unexpected SPL sweep signer {key}"));
                Signer::Account(account)
            }
        })
        .collect();
    sign_transaction(
        &mut transaction,
        signers.iter().map(Signer::derivation_path),
        &runtime.signer(),
    )
    .await?;
    Ok((transaction, signers))
}

/// Signs the transaction of a planned sweep with the deposit addresses it transfers from.
///
/// Returns the signed transaction and the signer accounts in the order of the signatures.
pub async fn sign_sweep_transaction<R: CanisterRuntime>(
    runtime: &R,
    sweep: &Sweep,
    recent_blockhash: Hash,
) -> Result<(Transaction, Vec<Signer>), CreateTransferError> {
    let mut transaction = Transaction::new_unsigned(sweep.sweep_message(recent_blockhash));
    let accounts_by_address: BTreeMap<Address, Account> = sweep
        .deposits()
        .values()
        .map(|deposit| (deposit.address, deposit.account))
        .collect();
    let signers: Vec<Signer> = transaction
        .message
        .signer_keys()
        .iter()
        .map(|key| {
            let account = accounts_by_address.get(key).copied().unwrap_or_else(|| {
                panic!("BUG: signer {key} is not a deposit address of the sweep")
            });
            Signer::Account(account)
        })
        .collect();

    sign_transaction(
        &mut transaction,
        signers.iter().map(Signer::derivation_path),
        &runtime.signer(),
    )
    .await?;

    Ok((transaction, signers))
}

/// Creates a signed Solana transaction that transfers lamports from a single
/// minter-controlled address (the fee payer) to multiple target addresses.
///
/// Returns the signed transaction and its signers:
/// only [`Signer::Minter`], the fee payer.
///
/// # Panics
///
/// Panics if the IC returns a signature that is not exactly 64 bytes.
pub async fn create_signed_batch_withdrawal_transaction<R: CanisterRuntime>(
    runtime: &R,
    targets: &[(Address, Lamport)],
    recent_blockhash: Hash,
) -> Result<(Transaction, Vec<Signer>), CreateTransferError> {
    let master_public_key = lazy_get_schnorr_master_key(runtime).await;
    let fee_payer_address = minter_address(&master_public_key);

    let instructions: Vec<Instruction> = targets
        .iter()
        .map(|(target, amount)| instruction::transfer(&fee_payer_address, target, *amount))
        .collect();

    let message =
        Message::new_with_blockhash(&instructions, Some(&fee_payer_address), &recent_blockhash);
    let mut transaction = Transaction::new_unsigned(message);

    let signers = vec![Signer::Minter];
    let derivation_paths: Vec<DerivationPath> =
        signers.iter().map(Signer::derivation_path).collect();
    sign_transaction(&mut transaction, derivation_paths, &runtime.signer()).await?;

    Ok((transaction, signers))
}

// Sign transaction, return error if it exceeds the maximum transaction size.
async fn sign_transaction(
    transaction: &mut Transaction,
    signer_derivation_paths: impl IntoIterator<Item = DerivationPath>,
    signer: &impl SchnorrSigner,
) -> Result<(), CreateTransferError> {
    let message_bytes = transaction.message_data();
    let message_len = message_bytes.len();
    transaction.signatures = sign_bytes(signer_derivation_paths, signer, message_bytes).await?;

    let tx_size = 1 + message_len + transaction.signatures.len() * BYTES_PER_SIGNATURE;
    if tx_size > MAX_TX_SIZE {
        return Err(CreateTransferError::TransactionTooLarge {
            max: MAX_TX_SIZE,
            got: tx_size,
        });
    }

    Ok(())
}
