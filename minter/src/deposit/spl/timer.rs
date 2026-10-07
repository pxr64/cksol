use crate::{
    constants::MAX_CONCURRENT_RPC_CALLS,
    sol_transfer::{MAX_SIGNATURES, MAX_TX_SIZE, transaction_size},
    state::{SchnorrPublicKey, SplSweep, State},
};
use solana_hash::Hash;
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

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
