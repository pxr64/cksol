use crate::state::{
    SupportedSplToken,
    event::{Signer, VersionedMessage},
};
use cksol_types::DepositSplId;
use icrc_ledger_types::icrc1::account::Account;
use solana_address::Address;
use solana_signature::Signature;
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

mod sweeps;
pub use sweeps::{SplSweep, SplSweepRecoveryError, SplSweeps, SplTransfer};

/// SPL deposits grouped by their progress towards a token mint.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SplDeposits {
    next_id: DepositSplId,
    queued: BTreeMap<DepositSplId, QueuedSplDeposit>,
    swept: SplSweeps,
    finalized: SplSweeps,
    dropped: BTreeMap<DepositSplId, SweptSplDeposit>,
    in_flight_ids: BTreeMap<(Account, Address), DepositSplId>,
}

impl SplDeposits {
    pub fn next_id(&self) -> DepositSplId {
        self.next_id
    }

    pub fn queued(&self) -> &BTreeMap<DepositSplId, QueuedSplDeposit> {
        &self.queued
    }

    pub fn swept(&self) -> &SplSweeps {
        &self.swept
    }

    pub fn finalized(&self) -> &SplSweeps {
        &self.finalized
    }

    pub fn dropped(&self) -> &BTreeMap<DepositSplId, SweptSplDeposit> {
        &self.dropped
    }

    pub fn in_flight_id(&self, account: &Account, mint: &Address) -> Option<DepositSplId> {
        self.in_flight_ids.get(&(*account, *mint)).copied()
    }

    pub(super) fn queue(&mut self, deposit_id: DepositSplId, deposit: QueuedSplDeposit) {
        assert_eq!(
            deposit_id, self.next_id,
            "Attempted to queue SPL deposit {deposit_id} out of sequence, expected {}",
            self.next_id
        );
        let key = (deposit.account, deposit.mint);
        assert!(
            !self.in_flight_ids.contains_key(&key),
            "Attempted to queue an SPL deposit for account {:?} and mint {} that already has one in flight",
            deposit.account,
            deposit.mint
        );
        assert!(
            deposit.balance > 0,
            "Attempted to queue SPL deposit {deposit_id} with a zero balance"
        );
        self.in_flight_ids.insert(key, deposit_id);
        self.queued.insert(deposit_id, deposit);
        self.next_id += 1;
    }

    /// Moves the given queued deposits to their submitted sweep. Each account and mint
    /// stays in flight until the sweep and the ledger mint are resolved.
    pub(super) fn sweep(
        &mut self,
        deposit_ids: &[DepositSplId],
        message: &VersionedMessage,
        signers: &[Signer],
        tokens: &BTreeMap<Address, SupportedSplToken>,
        signature: &Signature,
    ) {
        assert!(
            !deposit_ids.is_empty(),
            "Attempted to sweep no SPL deposits with transaction {signature}"
        );
        let deposits = deposit_ids
            .iter()
            .map(|deposit_id| {
                let deposit = self.queued.remove(deposit_id).unwrap_or_else(|| {
                    panic!("Attempted to sweep unknown or already swept SPL deposit {deposit_id}")
                });
                (*deposit_id, deposit)
            })
            .collect::<Vec<_>>();
        let sweep = SplSweep::recover(deposits, tokens, message, signers).unwrap_or_else(|error| {
            panic!("Attempted to sweep with transaction {signature}: {error}")
        });
        self.swept.insert(*signature, sweep);
    }

    pub(super) fn finalize_swept(&mut self, signature: &Signature) {
        let sweep = self.swept.remove(signature).unwrap_or_else(|| {
            panic!("Attempted to finalize SPL sweep {signature} that is not swept")
        });
        self.finalized.insert(*signature, sweep);
    }

    /// Drops a failed or expired sweep and releases its account and mint pairs.
    pub(super) fn drop_swept(&mut self, signature: &Signature) {
        let sweep = self
            .swept
            .remove(signature)
            .unwrap_or_else(|| panic!("Attempted to drop SPL sweep {signature} that is not swept"));
        for (deposit_id, deposit) in sweep.deposits() {
            assert_eq!(
                self.in_flight_ids.remove(&(deposit.account, deposit.mint)),
                Some(*deposit_id),
                "BUG: SPL deposit {deposit_id} is not in flight"
            );
            self.dropped.insert(
                *deposit_id,
                SweptSplDeposit {
                    deposit: deposit.clone(),
                    signature: *signature,
                },
            );
        }
    }
}

/// A finalized token-account balance accepted for a future sweep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedSplDeposit {
    pub account: Account,
    pub mint: Address,
    pub address: Address,
    /// The token-account balance when the deposit was queued, in the mint's smallest units.
    pub balance: u64,
}

/// A deposit together with the sweep transaction submitted to move its tokens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweptSplDeposit {
    pub deposit: QueuedSplDeposit,
    pub signature: Signature,
}
