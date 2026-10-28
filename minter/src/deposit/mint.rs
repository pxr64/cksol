use crate::{
    constants::{LEDGER_DEDUPLICATION_WINDOW, MAX_PENDING_MINTS_PER_ROUND},
    guard::TimerGuard,
    ledger::client::{LedgerClient, to_ledger_mint_index},
    numeric::LedgerMintIndex,
    queue_cursor::QueueCursors,
    runtime::CanisterRuntime,
    state::{State, TaskType, audit::process_event, event::EventType, mutate_state, read_state},
    storage::{FailedMintReason, record_failed_mint_attempt},
};
use canlog::log;
use cksol_types_internal::log::Priority;
use ic_canister_runtime::Runtime;
use icrc_ledger_types::icrc1::transfer::{TransferArg, TransferError};
use scopeguard::ScopeGuard;
use std::{collections::BTreeMap, thread::LocalKey, time::Duration};

/// The queues that mint on a timer, each with its own rotation cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MintQueue {
    Sol,
    Spl,
    SplFee,
    SplFeeReimbursement,
}

impl MintQueue {
    fn task(self) -> TaskType {
        match self {
            Self::Sol => TaskType::Mint,
            Self::Spl | Self::SplFee => TaskType::MintSpl,
            Self::SplFeeReimbursement => TaskType::ReimburseSplWithdrawalFees,
        }
    }

    /// The metrics count the ckSOL mints only.
    fn record_failed_mint(self, reason: FailedMintReason) {
        match self {
            Self::Sol => record_failed_mint_attempt(reason),
            Self::Spl | Self::SplFee | Self::SplFeeReimbursement => {}
        }
    }
}

thread_local! {
    /// Where the previous round of each deposit queue stopped. The cursors reset on
    /// upgrade; the mint arguments live in the event log.
    pub static CURSORS: QueueCursors<MintQueue, u64> = const { QueueCursors::new() };
}

pub fn reset_cursors() {
    CURSORS.with(QueueCursors::reset);
}

/// Runs one bounded round, continuing after the previous attempt and wrapping at the end.
/// Returns whether the round made progress with more pending mints than one round holds.
pub async fn process_pending_mints_round<R: CanisterRuntime, K: Copy + Ord, Pending: Clone>(
    runtime: &R,
    queue: MintQueue,
    cursors: &'static LocalKey<QueueCursors<MintQueue, K>>,
    pending_mints: impl Fn(&State) -> &BTreeMap<K, Pending>,
    mint: impl AsyncFn(&R, K, Pending),
) -> bool {
    let _guard = match TimerGuard::new(queue.task()) {
        Ok(guard) => guard,
        Err(_) => return false,
    };
    let (more_pending_mints_than_one_round, round) = read_state(|state| {
        let pending = pending_mints(state);
        (
            pending.len() > MAX_PENDING_MINTS_PER_ROUND,
            cursors
                .with(|cursors| cursors.round(queue, pending, MAX_PENDING_MINTS_PER_ROUND))
                .into_iter()
                .map(|(key, pending)| (*key, pending.clone()))
                .collect::<Vec<_>>(),
        )
    });
    if round.is_empty() {
        cursors.with(|cursors| cursors.clear(queue));
        return false;
    }
    let selected_keys: Vec<K> = round.iter().map(|(key, _)| *key).collect();
    for (key, pending) in round {
        // Advance before awaiting the ledger, including when its reply fails or traps.
        cursors.with(|cursors| cursors.advance(queue, key));
        mint(runtime, key, pending).await;
    }
    let round_made_progress = read_state(|state| {
        let pending = pending_mints(state);
        if pending.is_empty() {
            cursors.with(|cursors| cursors.clear(queue));
        }
        selected_keys.iter().any(|key| !pending.contains_key(key))
    });
    round_made_progress && more_pending_mints_than_one_round
}

/// Mints with fixed transfer arguments, retrying transient errors and quarantining unsafe retries.
pub async fn mint_pending<R: CanisterRuntime, C: Runtime>(
    runtime: &R,
    queue: MintQueue,
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
        queue.record_failed_mint(FailedMintReason::Expired);
        mutate_state(|state| process_event(state, quarantine_event, runtime));
        return;
    }
    let quarantine_unless_defused = scopeguard::guard(quarantine_event, |event| {
        queue.record_failed_mint(FailedMintReason::UnknownOutcome);
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
            let quarantine_event = ScopeGuard::into_inner(quarantine_unless_defused);
            queue.record_failed_mint(FailedMintReason::Expired);
            log!(
                Priority::Error,
                "Quarantining {label}: the ledger rejected its mint as outside the deduplication window"
            );
            mutate_state(|state| process_event(state, quarantine_event, runtime));
        }
        Ok(Err(
            rejection @ (TransferError::BadFee { .. }
            | TransferError::BadBurn { .. }
            | TransferError::InsufficientFunds { .. }),
        )) => {
            let quarantine_event = ScopeGuard::into_inner(quarantine_unless_defused);
            queue.record_failed_mint(FailedMintReason::Rejected);
            log!(
                Priority::Error,
                "Quarantining {label}: the ledger definitively rejected its mint from the minting account: {rejection:?}"
            );
            mutate_state(|state| process_event(state, quarantine_event, runtime));
        }
        Ok(Err(
            transient
            @ (TransferError::TemporarilyUnavailable | TransferError::GenericError { .. }),
        )) => {
            ScopeGuard::into_inner(quarantine_unless_defused);
            queue.record_failed_mint(FailedMintReason::LedgerError);
            log!(
                Priority::Info,
                "Failed to mint {label}, retrying on the next round: {transient:?}"
            );
        }
        Ok(Err(clock_skew @ TransferError::CreatedInFuture { .. })) => {
            ScopeGuard::into_inner(quarantine_unless_defused);
            queue.record_failed_mint(FailedMintReason::CreatedInFuture);
            log!(
                Priority::Info,
                "Failed to mint {label}, retrying on the next round: {clock_skew:?}"
            );
        }
        Err(ic_error) => {
            ScopeGuard::into_inner(quarantine_unless_defused);
            queue.record_failed_mint(FailedMintReason::CallError);
            log!(
                Priority::Info,
                "Failed to mint {label}, retrying on the next round: {ic_error}"
            );
        }
    }
}
