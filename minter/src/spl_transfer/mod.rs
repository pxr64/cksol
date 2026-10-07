use crate::{
    runtime::CanisterRuntime,
    sol_transfer::{CreateTransferError, sign_transaction},
    state::{SplSweep, event::Signer},
};
use solana_hash::Hash;
use solana_transaction::Transaction;

#[cfg(test)]
mod tests;

/// Signs an SPL sweep with the minter and each distinct deposit owner.
/// Returns the signed transaction and its signers in signature order.
pub async fn sign_spl_sweep_transaction<R: CanisterRuntime>(
    runtime: &R,
    sweep: &SplSweep,
    recent_blockhash: Hash,
) -> Result<(Transaction, Vec<Signer>), CreateTransferError> {
    let mut transaction = Transaction::new_unsigned(sweep.sweep_message(recent_blockhash));
    let signers = sweep.signers(&transaction.message);
    sign_transaction(
        &mut transaction,
        signers.iter().map(Signer::derivation_path),
        &runtime.signer(),
    )
    .await?;
    Ok((transaction, signers))
}
