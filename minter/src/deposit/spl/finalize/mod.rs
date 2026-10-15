use crate::{
    deposit::finalize::{FinalizedQueue, credit_finalized_sweep_round},
    runtime::CanisterRuntime,
    state::{SplSweep, event::EventType},
};

#[cfg(test)]
mod tests;

/// Settles finalized SPL sweeps, crediting matching outcomes and quarantining mismatches.
/// Returns whether progress was made with more finalized sweeps left to process.
pub async fn credit_finalized_spl_sweeps<R: CanisterRuntime>(runtime: &R) -> bool {
    credit_finalized_sweep_round(
        runtime,
        FinalizedQueue::Spl,
        |state| state.spl_deposits().finalized().by_signature(),
        |signature, sweep: SplSweep, outcome| {
            let settled = sweep.settle(&outcome.transaction, outcome.meta.as_ref())?;
            Ok(EventType::CreditedSplSweep {
                signature,
                lamports_spent: settled.lamports_spent(),
                mints: settled.into_mints(),
            })
        },
        |signature| EventType::QuarantinedSplSweep { signature },
        "SPL sweep",
    )
    .await
}
