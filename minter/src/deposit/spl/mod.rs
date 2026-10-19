use crate::{
    address::{account_address, associated_token_address, minter_address, minter_public_key},
    constants::{FEE_PER_SIGNATURE, GET_ACCOUNT_INFO_CYCLES, XRC_REQUEST_CYCLES},
    cycles::{RpcCallCharge, charge_rpc_call, check_caller_available_cycles},
    guard::deposit_spl_guard,
    rpc::{get_spl_token_account, get_spl_token_balance},
    runtime::CanisterRuntime,
    spl_fee::{FeeEstimator, XrcPriceOracle, estimate_sol_cost},
    state::{SupportedSplToken, audit::process_event, event::EventType, mutate_state, read_state},
    utils::{assert_valid_deposit_owner, assert_valid_deposit_token},
};
use canlog::log;
use cksol_types::{DepositSplError, DepositSplId, DepositSplStatus, Lamport};
use cksol_types_internal::log::Priority;
use icrc_ledger_types::icrc1::account::Account;
use solana_address::Address;

#[cfg(test)]
mod tests;

mod finalize;
mod mint;
mod timer;

pub use finalize::credit_finalized_spl_sweeps;
pub use mint::process_pending_spl_mints;
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
    let consolidation_fee = read_state(|state| state.deposit_sol_fee());
    check_caller_available_cycles(
        runtime,
        2 * GET_ACCOUNT_INFO_CYCLES + XRC_REQUEST_CYCLES + consolidation_fee,
    )?;
    let master_key =
        minter_public_key().map_err(|e| DepositSplError::TemporarilyUnavailable(e.to_string()))?;
    let owner = account_address(&master_key, &account);
    let address = associated_token_address(&owner, &token.mint, &token.token_program.id());
    let balance = read_deposit_balance(runtime, &token, address, owner).await?;
    let rent = read_destination_rent(runtime, &token, minter_address(&master_key)).await?;
    let fee = quote_deposit_fee(runtime, &token, balance, rent, consolidation_fee).await?;

    Ok(queue_deposit(runtime, account, mint, address, balance, fee))
}

pub fn deposit_spl_status(deposit_id: DepositSplId) -> DepositSplStatus {
    read_state(|state| state.spl_deposits().status(deposit_id))
}

/// Reads and validates the source account and minimum amount, charging its RPC cost.
async fn read_deposit_balance<R: CanisterRuntime>(
    runtime: &R,
    token: &SupportedSplToken,
    address: Address,
    owner: Address,
) -> Result<u64, DepositSplError> {
    let result = get_spl_token_balance(runtime, address, owner, token.mint, token.token_program)
        .await
        .map_err(DepositSplError::from)
        .and_then(|balance| balance_above_minimum(balance, token.minimum_deposit_amount, 0));
    charge_account_read(runtime, &result);
    result
}

/// Reads and validates the treasury account, charging the actual RPC cost.
/// Returns the account-creation rent budget, or zero if the account already exists.
async fn read_destination_rent<R: CanisterRuntime>(
    runtime: &R,
    token: &SupportedSplToken,
    destination_owner: Address,
) -> Result<Lamport, DepositSplError> {
    let destination =
        associated_token_address(&destination_owner, &token.mint, &token.token_program.id());
    let result = get_spl_token_account(runtime, destination, token.token_program)
        .await
        .and_then(|account| match account {
            Some(account) => account.validate(destination_owner, token.mint).map(|()| 0),
            None => Ok(token.token_program.account_rent_exemption()),
        })
        .map_err(DepositSplError::from);
    charge_account_read(runtime, &result);
    result
}

/// Charges the RPC cost of an account read, with no fee on success.
fn charge_account_read<R: CanisterRuntime, T>(runtime: &R, result: &Result<T, DepositSplError>) {
    charge_rpc_call(
        runtime,
        RpcCallCharge {
            attached_cycles: GET_ACCOUNT_INFO_CYCLES,
            fee_on_success: 0,
        },
        result,
    );
}

/// Quotes the fee and checks the net minimum, charging the actual XRC call cost.
/// Accepts the consolidation fee only when both steps succeed.
async fn quote_deposit_fee<R: CanisterRuntime>(
    runtime: &R,
    token: &SupportedSplToken,
    balance: u64,
    rent: Lamport,
    consolidation_fee: u128,
) -> Result<u64, DepositSplError> {
    // Quote a standalone sweep; batching can reduce its actual cost.
    let cost = estimate_sol_cost(2 * FEE_PER_SIGNATURE, rent);
    let oracle = XrcPriceOracle::new(
        runtime.clone(),
        token.mint,
        token.xrc_symbol.clone(),
        token.decimals,
    );
    let result = FeeEstimator::new(oracle)
        .estimate_fee(token.mint, cost)
        .await
        .map_err(DepositSplError::TemporarilyUnavailable)
        .and_then(|fee| {
            balance_above_minimum(balance, token.minimum_deposit_amount, fee)?;
            Ok(fee)
        });
    charge_rpc_call(
        runtime,
        RpcCallCharge {
            attached_cycles: XRC_REQUEST_CYCLES,
            fee_on_success: consolidation_fee,
        },
        &result,
    );
    result
}

/// Records the accepted balance and fixed fee quote before returning the deposit ID.
fn queue_deposit<R: CanisterRuntime>(
    runtime: &R,
    account: Account,
    mint: Address,
    address: Address,
    balance: u64,
    fee: u64,
) -> DepositSplId {
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
                fee,
            },
            runtime,
        );
        deposit_id
    });
    log!(
        Priority::Info,
        "Queued SPL deposit {deposit_id} for account {account:?}: {balance} units of mint {mint} sweepable from {address}, with a fee of {fee} units"
    );
    deposit_id
}

fn balance_above_minimum(
    balance: u64,
    minimum_deposit_amount: u64,
    fee: u64,
) -> Result<u64, DepositSplError> {
    let minimum_deposit_amount = minimum_deposit_amount.checked_add(fee).ok_or_else(|| {
        DepositSplError::TemporarilyUnavailable(
            "SPL deposit fee and minimum deposit amount exceed u64::MAX".to_string(),
        )
    })?;
    if balance < minimum_deposit_amount {
        return Err(DepositSplError::ValueTooSmall {
            balance,
            minimum_deposit_amount,
        });
    }
    Ok(balance)
}
