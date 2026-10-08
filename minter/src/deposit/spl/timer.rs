use crate::{
    address::lazy_get_schnorr_master_key,
    constants::MAX_CONCURRENT_RPC_CALLS,
    guard::TimerGuard,
    rpc::{Block, SubmitTransactionError, get_recent_block, submit_transaction},
    runtime::CanisterRuntime,
    sol_transfer::{CreateTransferError, MAX_SIGNATURES, MAX_TX_SIZE, transaction_size},
    spl_transfer::sign_spl_sweep_transaction,
    state::{
        QueuedSplDeposit, SchnorrPublicKey, SplSweep, State, SupportedSplToken, TaskType,
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
    if read_state(|state| {
        state.spl_deposits().queued().is_empty() && state.spl_deposits().retrying().is_empty()
    }) {
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
    let SweepRound {
        retries,
        batches,
        leaves_deposits_queued,
    } = read_state(|state| SweepRound::take_from_queue(state, &master_key));
    if retries.is_empty() && batches.is_empty() {
        return;
    }

    let reschedule = scopeguard::guard(runtime.clone(), |runtime| {
        runtime.set_timer(Duration::ZERO, sweep_queued_spl_deposits);
    });
    let retries = retries
        .into_iter()
        .map(|sweep| (retry_purpose(&sweep), sweep));
    let batches = batches
        .into_iter()
        .map(|sweep| (batch_purpose(&sweep), sweep));
    futures::future::join_all(retries.chain(batches).map(async |(purpose, sweep)| {
        match submit_spl_sweep_transaction(&runtime, purpose, sweep, block).await {
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
    if !leaves_deposits_queued {
        scopeguard::ScopeGuard::into_inner(reschedule);
    }
}

fn retry_purpose(sweep: &SplSweep) -> TransactionPurpose {
    let [deposit_id] = sweep.deposits().keys().copied().collect::<Vec<_>>()[..] else {
        panic!("BUG: a retry sweeps exactly one deposit");
    };
    TransactionPurpose::RetrySplDeposit { deposit_id }
}

fn batch_purpose(sweep: &SplSweep) -> TransactionPurpose {
    TransactionPurpose::SweepSplDeposits {
        deposit_ids: sweep.deposits().keys().copied().collect(),
    }
}

struct SweepRound {
    retries: Vec<SplSweep>,
    batches: Vec<SplSweep>,
    leaves_deposits_queued: bool,
}

impl SweepRound {
    /// Sweeps each retried deposit alone first, then fills the remaining slots of the round
    /// with batches of queued deposits.
    fn take_from_queue(state: &State, master_key: &SchnorrPublicKey) -> Self {
        let retries = Self::take_retries(state, master_key, MAX_CONCURRENT_RPC_CALLS);
        let batches =
            Self::take_batches(state, master_key, MAX_CONCURRENT_RPC_CALLS - retries.len());
        let swept = retries
            .iter()
            .chain(&batches)
            .map(|sweep| sweep.deposits().len())
            .sum::<usize>();
        Self {
            retries,
            batches,
            leaves_deposits_queued: swept
                < (state.spl_deposits().queued().len() + state.spl_deposits().retrying().len()),
        }
    }

    /// One sweep per retried deposit, in id order, up to the given number of sweeps.
    fn take_retries(state: &State, master_key: &SchnorrPublicKey, slots: usize) -> Vec<SplSweep> {
        let mut retries = Vec::new();
        for (deposit_id, deposit) in state.spl_deposits().retrying().iter().take(slots) {
            let tokens = BTreeMap::from([(deposit.mint, registered_token(state, deposit))]);
            let retry = SplSweep::plan([(*deposit_id, deposit.clone())], &tokens, master_key);
            assert!(
                fits_in_one_transaction(&retry),
                "BUG: SPL deposit {deposit_id} does not fit in a sweep of its own"
            );
            retries.push(retry);
        }
        retries
    }

    /// Fills sweeps with queued deposits in id order up to the transaction limits, up to the
    /// given number of sweeps.
    fn take_batches(state: &State, master_key: &SchnorrPublicKey, slots: usize) -> Vec<SplSweep> {
        let mut batches = Vec::new();
        let mut queued = state.spl_deposits().queued().iter().peekable();
        while batches.len() < slots {
            let mut selected = Vec::new();
            let mut tokens = BTreeMap::new();
            let mut batch = None;
            while let Some((deposit_id, deposit)) = queued.peek() {
                tokens
                    .entry(deposit.mint)
                    .or_insert_with(|| registered_token(state, deposit));
                let candidate = SplSweep::plan(
                    selected
                        .iter()
                        .cloned()
                        .chain(std::iter::once((**deposit_id, (**deposit).clone()))),
                    &tokens,
                    master_key,
                );
                if !fits_in_one_transaction(&candidate) {
                    // Every lone deposit builds the same message, so one that does not fit
                    // alone means the layout changed, not the deposit.
                    assert!(
                        batch.is_some(),
                        "BUG: SPL deposit {deposit_id} does not fit in a sweep of its own"
                    );
                    break;
                }
                selected.push((**deposit_id, (**deposit).clone()));
                queued.next();
                batch = Some(candidate);
            }
            match batch {
                Some(batch) => batches.push(batch),
                None => break,
            }
        }
        batches
    }
}

fn registered_token(state: &State, deposit: &QueuedSplDeposit) -> SupportedSplToken {
    state
        .supported_spl_token(&deposit.mint)
        .cloned()
        .expect("BUG: queued SPL deposit mint must be registered")
}

/// The blockhash has a fixed length, so a placeholder gives the final size.
fn fits_in_one_transaction(sweep: &SplSweep) -> bool {
    let message = sweep.sweep_message(Hash::default());
    transaction_size(&message) <= MAX_TX_SIZE
        && u64::from(message.header.num_required_signatures) <= MAX_SIGNATURES
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
    purpose: TransactionPurpose,
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
                purpose,
                block_height: block.block_height,
            },
            runtime,
        )
    });
    submit_transaction(runtime, transaction).await?;
    Ok(signature)
}
