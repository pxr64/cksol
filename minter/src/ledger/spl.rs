use super::client::LedgerClient;
use crate::runtime::CanisterRuntime;
use candid::Principal;
use canlog::log;
use cksol_types::Memo;
use cksol_types_internal::log::Priority;
use ic_canister_runtime::IcError;
use icrc_ledger_types::{
    icrc1::{account::Account, transfer::NumTokens},
    icrc2::transfer_from::{TransferFromArgs, TransferFromError},
};
use num_traits::ToPrimitive;
use thiserror::Error;

#[cfg(test)]
mod tests;

/// Keeps ledger replies separate from call failures for withdrawal recovery.
#[derive(Debug, PartialEq, Error)]
pub enum SplBurnError {
    #[error("Failed to call ckSPL ledger: {0}")]
    Call(IcError),
    #[error("Failed to burn ckSPL: {0}")]
    TransferFrom(TransferFromError),
}

/// Burns ckSPL on its registered ledger, whose minting account is this minter's
/// default account, as on the ckSOL ledger.
///
/// Any retry must reuse the original burn arguments. In particular,
/// a call error can leave the burn outcome unknown. A later `TooOld` response does
/// not prove that an earlier attempt failed; it requires reconciliation.
pub async fn burn<R: CanisterRuntime>(
    runtime: &R,
    ledger_id: Principal,
    from: Account,
    burn_amount: u64,
    burn_memo: Memo,
    created_at_time: u64,
) -> Result<u64, SplBurnError> {
    let result = LedgerClient::new(runtime.inter_canister_call_runtime(), ledger_id)
        .transfer_from(TransferFromArgs {
            spender_subaccount: None,
            from,
            to: runtime.canister_self().into(),
            fee: None,
            created_at_time: Some(created_at_time),
            memo: Some(burn_memo.into()),
            amount: NumTokens::from(burn_amount),
        })
        .await;

    let block_index = match result {
        Ok(Ok(block_index))
        | Ok(Err(TransferFromError::Duplicate {
            duplicate_of: block_index,
        })) => block_index,
        Ok(Err(error)) => return Err(SplBurnError::TransferFrom(error)),
        Err(error) => return Err(SplBurnError::Call(error)),
    };

    let block_index = block_index
        .0
        .to_u64()
        .expect("ledger block index does not fit into u64");

    log!(
        Priority::Info,
        "Burned {burn_amount} ckSPL units from {from:?} on ledger {ledger_id} (ledger block index {block_index})"
    );

    Ok(block_index)
}
