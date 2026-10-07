use crate::state::{SweepMismatch, SweepSettlementError, UnreadableOutcome};
use solana_hash::Hash;
use solana_transaction::Message;
use solana_transaction_status_client_types::{
    EncodedConfirmedTransactionWithStatusMeta, UiTransactionStatusMeta,
};

/// Checks that a transaction succeeded with the planned message and complete lamport balances.
pub fn validate_sweep_transaction(
    outcome: &EncodedConfirmedTransactionWithStatusMeta,
    planned_message: impl FnOnce(Hash) -> Message,
) -> Result<(Message, &UiTransactionStatusMeta), SweepSettlementError> {
    let transaction = outcome
        .transaction
        .transaction
        .decode()
        .ok_or(UnreadableOutcome::TransactionDecodingFailed)?;
    let meta = outcome
        .transaction
        .meta
        .as_ref()
        .ok_or(UnreadableOutcome::NoMetaField)?;
    if let Some(error) = &meta.err {
        return Err(SweepMismatch::TransactionFailed {
            error: error.to_string(),
        }
        .into());
    }
    let solana_message::VersionedMessage::Legacy(message) = transaction.message else {
        return Err(SweepMismatch::UnexpectedMessage.into());
    };
    if message != planned_message(message.recent_blockhash) {
        return Err(SweepMismatch::UnexpectedMessage.into());
    }
    if meta.pre_balances.len() != message.account_keys.len()
        || meta.post_balances.len() != message.account_keys.len()
    {
        return Err(UnreadableOutcome::IncompleteBalances.into());
    }
    Ok((message, meta))
}
