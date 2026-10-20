use crate::{
    constants::OPERATING_TOP_UP_SUBACCOUNT,
    guard::operating_top_up_guard,
    ledger::burn_from_subaccount,
    runtime::CanisterRuntime,
    state::{audit::process_event, event::EventType, mutate_state, read_state},
};
use candid::Principal;
use canlog::log;
use cksol_types::{BurnMemo, OperatingTopUp, ProcessOperatingTopUpError};
use cksol_types_internal::log::Priority;
use icrc_ledger_types::icrc1::account::Account;
use num_traits::ToPrimitive;

#[cfg(test)]
mod tests;

/// The ckSOL ledger account that receives operating top-ups.
pub fn operating_top_up_account(minter: Principal) -> Account {
    Account {
        owner: minter,
        subaccount: Some(OPERATING_TOP_UP_SUBACCOUNT),
    }
}

/// Burns all ckSOL of the top-up account and adds the amount to the operating balance.
pub async fn process_operating_top_up<R: CanisterRuntime>(
    runtime: &R,
) -> Result<OperatingTopUp, ProcessOperatingTopUpError> {
    let minter = runtime.canister_self();
    let top_up_account = operating_top_up_account(minter);
    let _guard = operating_top_up_guard(top_up_account)?;
    let balance = read_state(|state| state.ledger_client(runtime.inter_canister_call_runtime()))
        .balance_of(top_up_account)
        .await
        .map_err(|error| {
            ProcessOperatingTopUpError::TemporarilyUnavailable(format!(
                "Failed to read the top-up balance: {error}"
            ))
        })?
        .0
        .to_u64()
        .expect("BUG: the ckSOL balance does not fit into u64");
    if balance == 0 {
        return Err(ProcessOperatingTopUpError::NoFunds);
    }

    let block_index = burn_from_subaccount(
        runtime,
        minter.into(),
        OPERATING_TOP_UP_SUBACCOUNT,
        balance,
        BurnMemo::TopUp,
    )
    .await
    .map_err(|error| ProcessOperatingTopUpError::TemporarilyUnavailable(error.to_string()))?;

    mutate_state(|state| {
        process_event(
            state,
            EventType::ToppedUpOperatingBalance {
                burn_block_index: block_index.into(),
                amount: balance,
            },
            runtime,
        )
    });
    log!(
        Priority::Info,
        "Added {balance} lamports to the operating balance (burn block index {block_index})"
    );
    Ok(OperatingTopUp {
        block_index,
        amount: balance,
    })
}
