use crate::{
    address::associated_token_address,
    runtime::CanisterRuntime,
    sol_transfer::{CreateTransferError, build_nonce_message, sign_transaction},
    state::{SplSweep, SupportedSplToken, event::Signer},
};
use solana_address::Address;
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_transaction::{Message, Transaction};

#[cfg(test)]
mod tests;

/// Builds an unsigned SPL withdrawal message: nonce advancement first, then the
/// withdrawal instructions, signed by the minter alone.
pub fn build_spl_withdrawal_message(
    minter_address: &Address,
    nonce_account: &Address,
    nonce_value: Hash,
    withdrawal_instructions: &[Instruction],
) -> Result<Message, CreateTransferError> {
    build_nonce_message(
        minter_address,
        nonce_account,
        nonce_value,
        withdrawal_instructions.iter().cloned(),
    )
}

/// Creates the associated token account of `owner` for `mint` unless it exists.
pub fn create_associated_token_account_instruction(
    payer: &Address,
    owner: &Address,
    mint: &Address,
    token_program: &Address,
) -> Instruction {
    spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
        &payer.to_bytes().into(),
        &owner.to_bytes().into(),
        &mint.to_bytes().into(),
        &token_program.to_bytes().into(),
    )
}

/// Builds a `TransferChecked` for either token program; both share the instruction layout.
pub fn transfer_checked_instruction(
    token_program: &Address,
    source: &Address,
    mint: &Address,
    destination: &Address,
    authority: &Address,
    amount: u64,
    decimals: u8,
) -> Instruction {
    let mut transfer = spl_token_interface::instruction::transfer_checked(
        &spl_token_interface::id(),
        &source.to_bytes().into(),
        &mint.to_bytes().into(),
        &destination.to_bytes().into(),
        &authority.to_bytes().into(),
        &[],
        amount,
        decimals,
    )
    .expect("BUG: transfer_checked only rejects a foreign token program id");
    transfer.program_id = token_program.to_bytes().into();
    transfer
}

/// Transfers tokens from the minter's treasury ATA to the destination owner's ATA,
/// optionally creating the destination ATA at the minter's expense first.
pub fn build_spl_withdrawal_instructions(
    minter_address: &Address,
    token: &SupportedSplToken,
    destination: &Address,
    amount: u64,
    create_destination_account: bool,
) -> Vec<Instruction> {
    let token_program = token.token_program.id();
    let source = associated_token_address(minter_address, &token.mint, &token_program);
    let destination_account = associated_token_address(destination, &token.mint, &token_program);
    let mut instructions = Vec::new();
    if create_destination_account {
        instructions.push(create_associated_token_account_instruction(
            minter_address,
            destination,
            &token.mint,
            &token_program,
        ));
    }
    instructions.push(transfer_checked_instruction(
        &token_program,
        &source,
        &token.mint,
        &destination_account,
        minter_address,
        amount,
        token.decimals,
    ));
    instructions
}

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
