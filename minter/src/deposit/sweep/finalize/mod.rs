use crate::{
    constants::MAX_CONCURRENT_RPC_CALLS,
    deposit::finalize::credit_finalized_sweep_round,
    runtime::CanisterRuntime,
    state::{Sweep, event::EventType},
};

#[cfg(test)]
mod tests;

/// Credits the deposits of the sweep transactions that have been finalized successfully.
///
/// Each sweep is settled against the plan it was submitted with, so that only an outcome
/// matching exactly what the minter built is credited. A sweep whose outcome cannot be read
/// is left finalized and retried on the next run, while a sweep whose outcome does not match
/// its plan is quarantined instead of credited.
///
/// Returns whether the finalization timer must run again immediately, which is the case
/// only when this round credited or quarantined a sweep and finalized sweeps are left. A
/// round whose fetches all failed is retried at the timer interval instead of in a hot loop.
pub async fn credit_finalized_sweeps<R: CanisterRuntime>(runtime: &R) -> bool {
    credit_finalized_sweep_round(
        runtime,
        |state| {
            let finalized = state.deposits().finalized();
            (
                finalized.len(),
                finalized
                    .iter()
                    .take(MAX_CONCURRENT_RPC_CALLS)
                    .map(|(signature, sweep)| (*signature, sweep.clone()))
                    .collect(),
            )
        },
        |signature, sweep: Sweep, outcome| {
            let settled = sweep.settle(outcome)?;
            Ok(EventType::CreditedSweep {
                signature,
                amount_received: settled.amount_received(),
                mints: settled.into_mints(),
            })
        },
        |signature| EventType::QuarantinedSweep { signature },
        "sweep",
    )
    .await
}
