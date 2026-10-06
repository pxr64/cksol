use crate::{
    address::{account_address, lazy_get_schnorr_master_key, spl_deposit_address},
    constants::GET_ACCOUNT_INFO_CYCLES,
    cycles::{RpcCallCharge, charge_rpc_call, check_caller_available_cycles},
    guard::deposit_spl_guard,
    rpc::get_spl_token_balance,
    runtime::CanisterRuntime,
    utils::{assert_valid_deposit_owner, assert_valid_deposit_token},
};
use cksol_types::{DepositSplError, DepositSplId};
use icrc_ledger_types::icrc1::account::Account;
use solana_address::Address;

#[cfg(test)]
mod tests;

/// Entry point for balance-based SPL deposits.
pub async fn deposit_spl<R: CanisterRuntime>(
    runtime: &R,
    account: Account,
    mint: cksol_types::Address,
) -> Result<DepositSplId, DepositSplError> {
    assert_valid_deposit_owner(&account, runtime.canister_self());
    let mint = Address::from(mint);
    let _guard = deposit_spl_guard(account, mint)?;
    let token = assert_valid_deposit_token(&mint)?;
    check_caller_available_cycles(runtime, GET_ACCOUNT_INFO_CYCLES)?;

    let master_key = lazy_get_schnorr_master_key(runtime).await;
    let owner = account_address(&master_key, &account);
    let address = spl_deposit_address(&owner, &token.mint, &token.token_program.id());
    let result = get_spl_token_balance(runtime, address, owner, token.mint, token.token_program)
        .await
        .map_err(DepositSplError::from)
        .and_then(|balance| balance_above_minimum(balance, token.minimum_deposit_amount));
    charge_rpc_call(
        runtime,
        RpcCallCharge {
            attached_cycles: GET_ACCOUNT_INFO_CYCLES,
            // No sweep fee is charged until SPL deposits can be queued.
            fee_on_success: 0,
        },
        &result,
    );
    result?;
    Err(DepositSplError::TemporarilyUnavailable(
        "SPL deposits are not implemented yet".to_string(),
    ))
}

fn balance_above_minimum(
    balance: u64,
    minimum_deposit_amount: u64,
) -> Result<u64, DepositSplError> {
    if balance < minimum_deposit_amount {
        return Err(DepositSplError::ValueTooSmall {
            balance,
            minimum_deposit_amount,
        });
    }
    Ok(balance)
}
