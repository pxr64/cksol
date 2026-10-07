use crate::{
    address::{account_address, associated_token_address, lazy_get_schnorr_master_key},
    constants::GET_ACCOUNT_INFO_CYCLES,
    cycles::{RpcCallCharge, charge_rpc_call, check_caller_available_cycles},
    guard::deposit_spl_guard,
    rpc::get_spl_token_balance,
    runtime::CanisterRuntime,
    state::{audit::process_event, event::EventType, mutate_state, read_state},
    utils::{assert_valid_deposit_owner, assert_valid_deposit_token},
};
use canlog::log;
use cksol_types::{DepositSplError, DepositSplId};
use cksol_types_internal::log::Priority;
use icrc_ledger_types::icrc1::account::Account;
use solana_address::Address;

#[cfg(test)]
mod tests;

mod timer;

pub use timer::sweep_queued_spl_deposits;

/// Entry point for balance-based SPL deposits.
pub async fn deposit_spl<R: CanisterRuntime>(
    runtime: &R,
    account: Account,
    mint: cksol_types::Address,
) -> Result<DepositSplId, DepositSplError> {
    assert_valid_deposit_owner(&account, runtime.canister_self());
    let mint = Address::from(mint);
    let _guard = deposit_spl_guard(account, mint)?;
    // A pause stops new deposits only, so an in-flight deposit stays reachable.
    if let Some(deposit_id) = read_state(|state| state.spl_deposits().in_flight_id(&account, &mint))
    {
        return Ok(deposit_id);
    }
    let token = assert_valid_deposit_token(&mint)?;
    let deposit_consolidation_fee = read_state(|state| state.deposit_consolidation_fee());
    check_caller_available_cycles(runtime, GET_ACCOUNT_INFO_CYCLES + deposit_consolidation_fee)?;

    let master_key = lazy_get_schnorr_master_key(runtime).await;
    let owner = account_address(&master_key, &account);
    let address = associated_token_address(&owner, &token.mint, &token.token_program.id());
    let result = get_spl_token_balance(runtime, address, owner, token.mint, token.token_program)
        .await
        .map_err(DepositSplError::from)
        .and_then(|balance| balance_above_minimum(balance, token.minimum_deposit_amount));
    charge_rpc_call(
        runtime,
        RpcCallCharge {
            attached_cycles: GET_ACCOUNT_INFO_CYCLES,
            fee_on_success: deposit_consolidation_fee,
        },
        &result,
    );
    let balance = result?;
    let deposit_id = mutate_state(|state| {
        let deposit_id = state.spl_deposits().next_id();
        process_event(
            state,
            EventType::QueuedSplDeposit {
                deposit_id,
                account,
                mint,
                address,
                balance,
            },
            runtime,
        );
        deposit_id
    });
    log!(
        Priority::Info,
        "Queued SPL deposit {deposit_id} for account {account:?}: {balance} units of mint {mint} sweepable from {address}"
    );
    Ok(deposit_id)
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
