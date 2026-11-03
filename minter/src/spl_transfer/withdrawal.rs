use super::{create_associated_token_account_instruction, transfer_checked_instruction};
use crate::{
    address::associated_token_address,
    sol_transfer::{CreateTransferError, build_nonce_message},
    state::SupportedSplToken,
};
use solana_address::Address;
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_message::Message;
use std::collections::BTreeSet;

/// One transfer from a treasury ATA to a destination wallet's ATA.
pub struct SplWithdrawalTransfer<'a> {
    pub token: &'a SupportedSplToken,
    pub destination: &'a Address,
    pub amount: u64,
    pub create_destination_account: bool,
}

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

/// Creates each requested destination ATA once, then transfers tokens in request order.
/// Different mints and token programs can share the minter's signature and durable nonce.
pub fn build_batch_spl_withdrawal_instructions(
    minter_address: &Address,
    transfers: &[SplWithdrawalTransfer<'_>],
) -> Vec<Instruction> {
    let mut instructions = Vec::new();
    let mut destinations_to_create = BTreeSet::new();
    for transfer in transfers {
        let token_program = transfer.token.token_program.id();
        if transfer.create_destination_account
            && destinations_to_create.insert((
                *transfer.destination,
                transfer.token.mint,
                token_program,
            ))
        {
            instructions.push(create_associated_token_account_instruction(
                minter_address,
                transfer.destination,
                &transfer.token.mint,
                &token_program,
            ));
        }
    }
    for transfer in transfers {
        let token_program = transfer.token.token_program.id();
        let source = associated_token_address(minter_address, &transfer.token.mint, &token_program);
        let destination =
            associated_token_address(transfer.destination, &transfer.token.mint, &token_program);
        instructions.push(transfer_checked_instruction(
            &token_program,
            &source,
            &transfer.token.mint,
            &destination,
            minter_address,
            transfer.amount,
            transfer.token.decimals,
        ));
    }
    instructions
}
