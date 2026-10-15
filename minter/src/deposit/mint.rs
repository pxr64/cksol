use crate::{
    constants::{LEDGER_DEDUPLICATION_WINDOW, MAX_PENDING_MINTS_PER_ROUND},
    guard::TimerGuard,
    ledger::client::{LedgerClient, to_ledger_mint_index},
    numeric::LedgerMintIndex,
    queue_cursor::QueueCursor,
    runtime::CanisterRuntime,
    state::{State, TaskType, audit::process_event, event::EventType, mutate_state, read_state},
};
use canlog::log;
use cksol_types_internal::log::Priority;
use ic_canister_runtime::Runtime;
use icrc_ledger_types::icrc1::transfer::{TransferArg, TransferError};
use scopeguard::ScopeGuard;
use std::{cell::RefCell, collections::BTreeMap, time::Duration};

/// The queues that mint on a timer, each with its own rotation cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MintQueue {
    Sol,
    Spl,
    SplFee,
}

impl MintQueue {
    fn task(self) -> TaskType {
        match self {
            Self::Sol => TaskType::Mint,
            Self::Spl | Self::SplFee => TaskType::MintSpl,
        }
    }
}

thread_local! {
    /// Where the previous round of each queue stopped. The cursors reset on upgrade; the
    /// mint arguments live in the event log.
    static CURSORS: RefCell<BTreeMap<MintQueue, QueueCursor<u64>>> =
        const { RefCell::new(BTreeMap::new()) };
}

pub fn reset_cursors() {
    CURSORS.with_borrow_mut(BTreeMap::clear);
}

/// Runs one bounded round, continuing after the previous attempt and wrapping at the end.
/// Returns whether the round made progress with more pending mints than one round holds.
pub async fn process_pending_mints_round<R: CanisterRuntime, Pending: Clone>(
    runtime: &R,
    queue: MintQueue,
    pending_mints: impl Fn(&State) -> &BTreeMap<u64, Pending>,
    mint: impl AsyncFn(&R, u64, Pending),
) -> bool {
    let _guard = match TimerGuard::new(queue.task()) {
        Ok(guard) => guard,
        Err(_) => return false,
    };
    let cursor = CURSORS.with_borrow(|cursors| cursors.get(&queue).copied().unwrap_or_default());
    let (pending_mints_before_round, round) = read_state(|state| {
        let pending = pending_mints(state);
        (
            pending.len(),
            cursor
                .round(pending, MAX_PENDING_MINTS_PER_ROUND)
                .into_iter()
                .map(|(deposit_id, pending)| (*deposit_id, pending.clone()))
                .collect::<Vec<_>>(),
        )
    });
    if round.is_empty() {
        CURSORS.with_borrow_mut(|cursors| cursors.remove(&queue));
        return false;
    }
    for (deposit_id, pending) in round {
        // Advance before awaiting the ledger, including when its reply fails or traps.
        CURSORS.with_borrow_mut(|cursors| cursors.entry(queue).or_default().advance(deposit_id));
        mint(runtime, deposit_id, pending).await;
    }
    let pending_mints_after_round = read_state(|state| pending_mints(state).len());
    if pending_mints_after_round == 0 {
        CURSORS.with_borrow_mut(|cursors| cursors.remove(&queue));
    }
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
