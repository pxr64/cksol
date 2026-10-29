use crate::state::{SweepMismatch, SweepSettlementError, UnreadableOutcome};
use sol_rpc_types::Lamport;
use solana_hash::Hash;
use solana_transaction::{Message, versioned::VersionedTransaction};
use solana_transaction_status_client_types::UiTransactionStatusMeta;

/// Checks that a transaction succeeded with the planned message and complete lamport balances.
pub fn validate_sweep_transaction<'a>(
    transaction: &'a VersionedTransaction,
    meta: Option<&'a UiTransactionStatusMeta>,
    planned_message: impl FnOnce(Hash) -> Message,
) -> Result<(&'a Message, &'a UiTransactionStatusMeta), SweepSettlementError> {
    let meta = meta.ok_or(UnreadableOutcome::NoMetaField)?;
    if let Some(error) = &meta.err {
        return Err(SweepMismatch::TransactionFailed {
            error: error.to_string(),
        }
        .into());
    }
    let solana_message::VersionedMessage::Legacy(message) = &transaction.message else {
        return Err(SweepMismatch::UnexpectedMessage.into());
    };
    if message != &planned_message(message.recent_blockhash) {
        return Err(SweepMismatch::UnexpectedMessage.into());
    }
    if meta.pre_balances.len() != message.account_keys.len()
        || meta.post_balances.len() != message.account_keys.len()
    {
        return Err(UnreadableOutcome::IncompleteBalances.into());
    }
    Ok((message, meta))
}

/// The lamports the fee payer spent, when the balances cover every account of the message
/// and the spend covers the transaction fee.
pub fn fee_payer_lamports_spent(
    message: &Message,
    meta: &UiTransactionStatusMeta,
) -> Option<Lamport> {
    if meta.pre_balances.len() != message.account_keys.len()
        || meta.post_balances.len() != message.account_keys.len()
    {
        return None;
    }
    meta.pre_balances[0]
        .checked_sub(meta.post_balances[0])
        .filter(|spent| *spent >= meta.fee)
}
