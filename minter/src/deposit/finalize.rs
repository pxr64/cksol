use crate::{
    constants::MAX_CONCURRENT_RPC_CALLS,
    queue_cursor::QueueCursors,
    rpc::get_transaction,
    runtime::CanisterRuntime,
    state::{
        State, SweepSettlementError, audit::process_event, event::EventType, mutate_state,
        read_state,
    },
};
use canlog::log;
use cksol_types_internal::log::Priority;
use solana_signature::Signature;
use solana_transaction_status_client_types::EncodedConfirmedTransactionWithStatusMeta;
use std::collections::BTreeMap;

/// The queues of finalized sweeps that settle on a timer, each with its own rotation cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FinalizedQueue {
    Sol,
    Spl,
}

thread_local! {
    /// Where the previous round of each queue stopped. The cursors reset on upgrade; the
    /// sweep plans live in the event log.
    static CURSORS: QueueCursors<FinalizedQueue, Signature> = const { QueueCursors::new() };
}

pub fn reset_cursors() {
    CURSORS.with(QueueCursors::reset);
}

/// Credits matching sweep outcomes and quarantines mismatches, retrying unreadable outcomes later.
/// Returns whether progress was made with more finalized sweeps left to process.
pub async fn credit_finalized_sweep_round<R: CanisterRuntime, S: Clone>(
    runtime: &R,
    queue: FinalizedQueue,
    finalized_sweeps: impl Fn(&State) -> &BTreeMap<Signature, S>,
    settle: impl Fn(
        Signature,
        S,
        &EncodedConfirmedTransactionWithStatusMeta,
    ) -> Result<EventType, SweepSettlementError>,
    quarantine: impl Fn(Signature) -> EventType,
    label: &str,
) -> bool {
    let (finalized_count, round) = read_state(|state| {
        let finalized = finalized_sweeps(state);
        (
            finalized.len(),
            CURSORS
                .with(|cursors| cursors.round(queue, finalized, MAX_CONCURRENT_RPC_CALLS))
                .into_iter()
                .map(|(signature, sweep)| (*signature, sweep.clone()))
                .collect::<Vec<_>>(),
        )
    });
    let Some((last_signature, _)) = round.last() else {
        CURSORS.with(|cursors| cursors.clear(queue));
        return false;
    };
    // Advance before the parallel RPC calls, even when every outcome is unavailable.
    CURSORS.with(|cursors| cursors.advance(queue, *last_signature));

    let outcomes = futures::future::join_all(
        round
            .iter()
            .map(|(signature, _)| get_transaction(runtime, *signature)),
    )
    .await;

    let mut settled_count = 0;
    for ((signature, sweep), outcome) in round.into_iter().zip(outcomes) {
        let outcome = match outcome {
            Ok(Some(outcome)) => outcome,
            Ok(None) => {
                log!(
                    Priority::Info,
                    "Finalized {label} {signature} was not returned by getTransaction, retrying later"
                );
                continue;
            }
            Err(e) => {
                log!(
                    Priority::Info,
                    "Failed to fetch finalized {label} {signature}: {e}, retrying later"
                );
                continue;
            }
        };
        let event = match settle(signature, sweep, &outcome) {
            Ok(event) => event,
            Err(SweepSettlementError::Unreadable(e)) => {
                log!(
                    Priority::Info,
                    "Could not read the outcome of {label} {signature}: {e}, retrying later"
                );
                continue;
            }
            Err(SweepSettlementError::Mismatch(e)) => {
                log!(
                    Priority::Error,
                    "Quarantining the deposits of {label} {signature}: {e}"
                );
                quarantine(signature)
            }
        };
        mutate_state(|state| process_event(state, event, runtime));
        settled_count += 1;
    }

    if read_state(|state| finalized_sweeps(state).is_empty()) {
        CURSORS.with(|cursors| cursors.clear(queue));
    }
    settled_count > 0 && settled_count < finalized_count
}
