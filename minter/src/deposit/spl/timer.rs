use crate::{
    address::lazy_get_schnorr_master_key,
    constants::MAX_CONCURRENT_RPC_CALLS,
    guard::TimerGuard,
    rpc::{Block, SubmitTransactionError, get_recent_block, submit_transaction},
    runtime::CanisterRuntime,
    sol_transfer::{CreateTransferError, MAX_SIGNATURES, MAX_TX_SIZE, transaction_size},
    spl_transfer::sign_spl_sweep_transaction,
    state::{
        SchnorrPublicKey, SplSweep, State, TaskType,
        audit::process_event,
        event::{EventType, TransactionPurpose},
        mutate_state, read_state,
    },
};
use canlog::log;
use cksol_types_internal::log::Priority;
use solana_hash::Hash;
use solana_signature::Signature;
use std::collections::BTreeMap;
use std::time::Duration;
use thiserror::Error;

#[cfg(test)]
mod tests;

pub async fn sweep_queued_spl_deposits<R: CanisterRuntime>(runtime: R) {
    let _guard = match TimerGuard::new(TaskType::SweepSplDeposits) {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if read_state(|state| state.spl_deposits().queued().is_empty()) {
        return;
    }
    let master_key = lazy_get_schnorr_master_key(&runtime).await;
    let block = match get_recent_block(&runtime).await {
        Ok(block) => block,
        Err(error) => {
            log!(
                Priority::Info,
                "Failed to fetch recent blockhash for SPL sweep: {error}"
            );
            return;
        }
    };
    let sweep = read_state(|state| SweepRound::take_from_queue(state, &master_key));
    if sweep.batches.is_empty() {
        return;
    }

    let reschedule = scopeguard::guard(runtime.clone(), |runtime| {
        runtime.set_timer(Duration::ZERO, sweep_queued_spl_deposits);
    });
    futures::future::join_all(sweep.batches.into_iter().map(async |batch| {
        match submit_spl_sweep_transaction(&runtime, batch, block).await {
            Ok(signature) => log!(
                Priority::Info,
                "Submitted SPL sweep transaction {signature}"
            ),
            Err(SweepError::CreateTransactionFailed(error)) => {
                log!(
                    Priority::Error,
                    "Failed to create SPL sweep transaction: {error}"
                )
            }
            Err(SweepError::SubmitTransactionFailed(error)) => log!(
                Priority::Info,
                "Failed to submit SPL sweep transaction (awaiting finalization): {error}"
            ),
        }
    }))
    .await;
    if !sweep.leaves_deposits_queued {
        scopeguard::ScopeGuard::into_inner(reschedule);
    }
}

struct SweepRound {
    batches: Vec<SplSweep>,
    leaves_deposits_queued: bool,
}

impl SweepRound {
    /// Fills each sweep with queued deposits in id order up to the transaction limits, and
    /// stops at the number of sweeps that one round submits.
    fn take_from_queue(state: &State, master_key: &SchnorrPublicKey) -> Self {
        let mut queued = state.spl_deposits().queued().iter().peekable();
        let mut batches = Vec::new();
        while batches.len() < MAX_CONCURRENT_RPC_CALLS {
            let mut deposits = Vec::new();
            let mut tokens = BTreeMap::new();
            let mut batch = None;
            while let Some((deposit_id, deposit)) = queued.peek() {
                let individual = state.spl_deposits().requires_individual_sweep(**deposit_id);
                if individual && !deposits.is_empty() {
                    break;
                }
                tokens.entry(deposit.mint).or_insert_with(|| {
                    state
                        .supported_spl_token(&deposit.mint)
                        .cloned()
                        .expect("BUG: queued SPL deposit mint must be registered")
                });
                let candidate = SplSweep::plan(
                    deposits
                        .iter()
                        .cloned()
                        .chain(std::iter::once((**deposit_id, (**deposit).clone()))),
                    &tokens,
                    master_key,
                );
                // The blockhash has a fixed length, so a placeholder gives the final size.
                let message = candidate.sweep_message(Hash::default());
                let signatures = message.header.num_required_signatures as u64;
                if transaction_size(&message) > MAX_TX_SIZE || signatures > MAX_SIGNATURES {
                    // Every lone deposit builds the same message, so one that does not fit
                    // alone means the layout changed, not the deposit.
                    assert!(
                        batch.is_some(),
                        "BUG: SPL deposit {deposit_id} does not fit in a sweep of its own"
                    );
                    break;
                }
                deposits.push((**deposit_id, (**deposit).clone()));
                queued.next();
                batch = Some(candidate);
                if individual {
                    break;
                }
            }
            match batch {
                Some(batch) => batches.push(batch),
                None => break,
            }
        }
        let selected = batches
            .iter()
            .map(|sweep| sweep.deposits().len())
            .sum::<usize>();
        Self {
            batches,
            leaves_deposits_queued: selected < state.spl_deposits().queued().len(),
        }
    }
}

#[derive(Debug, Error)]
enum SweepError {
    #[error("failed to create transaction: {0}")]
    CreateTransactionFailed(#[from] CreateTransferError),
    #[error("failed to submit transaction: {0}")]
    SubmitTransactionFailed(#[from] SubmitTransactionError),
}

async fn submit_spl_sweep_transaction<R: CanisterRuntime>(
    runtime: &R,
    sweep: SplSweep,
    block: Block,
) -> Result<Signature, SweepError> {
    let (transaction, signers) =
        sign_spl_sweep_transaction(runtime, &sweep, block.blockhash).await?;
    let signature = transaction.signatures[0];
    mutate_state(|state| {
        process_event(
            state,
            EventType::SubmittedTransaction {
                signature,
                message: transaction.message.clone().into(),
                signers,
                purpose: TransactionPurpose::SweepSplDeposits {
                    deposit_ids: sweep.deposits().keys().copied().collect(),
                },
                block_height: block.block_height,
            },
            runtime,
        )
    });
    submit_transaction(runtime, transaction).await?;
    Ok(signature)
}
