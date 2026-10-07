use crate::{
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

/// Credits matching sweep outcomes and quarantines mismatches, retrying unreadable outcomes later.
/// Returns whether progress was made with more finalized sweeps left to process.
pub async fn credit_finalized_sweep_round<R: CanisterRuntime, S>(
    runtime: &R,
    select: impl FnOnce(&State) -> (usize, Vec<(Signature, S)>),
    settle: impl Fn(
        Signature,
        S,
        &EncodedConfirmedTransactionWithStatusMeta,
    ) -> Result<EventType, SweepSettlementError>,
    quarantine: impl Fn(Signature) -> EventType,
    label: &str,
) -> bool {
    let (finalized_count, round) = read_state(select);
    if round.is_empty() {
        return false;
    }

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

    settled_count > 0 && settled_count < finalized_count
}
