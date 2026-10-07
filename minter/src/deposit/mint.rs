use crate::{
    constants::{LEDGER_DEDUPLICATION_WINDOW, MAX_PENDING_MINTS_PER_ROUND},
    guard::TimerGuard,
    ledger::client::{LedgerClient, to_ledger_mint_index},
    numeric::LedgerMintIndex,
    runtime::CanisterRuntime,
    state::{State, TaskType, audit::process_event, event::EventType, mutate_state, read_state},
};
use canlog::log;
use cksol_types_internal::log::Priority;
use ic_canister_runtime::Runtime;
use icrc_ledger_types::icrc1::transfer::{TransferArg, TransferError};
use scopeguard::ScopeGuard;
use std::{collections::BTreeMap, time::Duration};

/// Runs one round of pending mints under the given task. Returns whether the round made
/// progress with more pending mints than one round holds, so the caller runs again at once.
pub async fn process_pending_mints_round<R: CanisterRuntime, Id: Copy, Pending: Clone>(
    runtime: &R,
    task: TaskType,
    pending_mints: impl Fn(&State) -> &BTreeMap<Id, Pending>,
    mint: impl AsyncFn(&R, Id, Pending),
) -> bool {
    let _guard = match TimerGuard::new(task) {
        Ok(guard) => guard,
        Err(_) => return false,
    };
    let (pending_mints_before_round, round) = read_state(|state| {
        let pending = pending_mints(state);
        (
            pending.len(),
            pending
                .iter()
                .take(MAX_PENDING_MINTS_PER_ROUND)
                .map(|(deposit_id, pending)| (*deposit_id, pending.clone()))
                .collect::<Vec<_>>(),
        )
    });
    if round.is_empty() {
        return false;
    }
    for (deposit_id, pending) in round {
        mint(runtime, deposit_id, pending).await;
    }
    let pending_mints_after_round = read_state(|state| pending_mints(state).len());
    let round_made_progress = pending_mints_after_round < pending_mints_before_round;
    let more_pending_mints_than_one_round =
        pending_mints_before_round > MAX_PENDING_MINTS_PER_ROUND;
    round_made_progress && more_pending_mints_than_one_round
}

/// Mints with fixed transfer arguments, retrying transient errors and quarantining unsafe retries.
pub async fn mint_pending_deposit<R: CanisterRuntime, C: Runtime>(
    runtime: &R,
    client: LedgerClient<C>,
    args: TransferArg,
    minted_event: impl Fn(LedgerMintIndex) -> EventType,
    quarantine_event: EventType,
    label: &str,
) {
    let created_at_time = args
        .created_at_time
        .expect("BUG: a pending mint must have a timestamp");
    let age = Duration::from_nanos(runtime.time().saturating_sub(created_at_time));
    if age > LEDGER_DEDUPLICATION_WINDOW {
        log!(
            Priority::Error,
            "Quarantining {label}: its pending mint is older than the deduplication window of the ledger"
        );
        mutate_state(|state| process_event(state, quarantine_event, runtime));
        return;
    }
    let quarantine_unless_defused = scopeguard::guard(quarantine_event, |event| {
        mutate_state(|state| process_event(state, event, runtime));
    });
    let amount = args.amount.clone();
    match client.transfer(args).await {
        Ok(Ok(mint_block_index)) => {
            mutate_state(|state| process_event(state, minted_event(mint_block_index), runtime));
            log!(
                Priority::Info,
                "Minted {amount} units for {label} (ledger block index {})",
                mint_block_index.get()
            );
            ScopeGuard::into_inner(quarantine_unless_defused);
        }
        Ok(Err(TransferError::Duplicate { duplicate_of })) => {
            let mint_block_index = to_ledger_mint_index(duplicate_of);
            mutate_state(|state| process_event(state, minted_event(mint_block_index), runtime));
            log!(
                Priority::Info,
                "Mint of {label} was deduplicated by the ledger (ledger block index {})",
                mint_block_index.get()
            );
            ScopeGuard::into_inner(quarantine_unless_defused);
        }
        Ok(Err(TransferError::TooOld)) => {
            log!(
                Priority::Error,
                "Quarantining {label}: the ledger rejected its mint as outside the deduplication window"
            );
        }
        Ok(Err(
            rejection @ (TransferError::BadFee { .. }
            | TransferError::BadBurn { .. }
            | TransferError::InsufficientFunds { .. }),
        )) => {
            log!(
                Priority::Error,
                "Quarantining {label}: the ledger definitively rejected its mint from the minting account: {rejection:?}"
            );
        }
        Ok(Err(
            transient @ (TransferError::TemporarilyUnavailable
            | TransferError::GenericError { .. }
            | TransferError::CreatedInFuture { .. }),
        )) => {
            ScopeGuard::into_inner(quarantine_unless_defused);
            log!(
                Priority::Info,
                "Failed to mint {label}, retrying on the next round: {transient:?}"
            );
        }
        Err(ic_error) => {
            ScopeGuard::into_inner(quarantine_unless_defused);
            log!(
                Priority::Info,
                "Failed to mint {label}, retrying on the next round: {ic_error}"
            );
        }
    }
}
