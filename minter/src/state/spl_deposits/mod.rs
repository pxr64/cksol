use cksol_types::DepositSplId;
use icrc_ledger_types::icrc1::account::Account;
use solana_address::Address;
use solana_signature::Signature;
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

/// SPL deposits grouped by their progress towards a token mint.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SplDeposits {
    next_id: DepositSplId,
    queued: BTreeMap<DepositSplId, QueuedSplDeposit>,
    swept: BTreeMap<Signature, SplSweep>,
    in_flight_ids: BTreeMap<(Account, Address), DepositSplId>,
}

impl SplDeposits {
    pub fn next_id(&self) -> DepositSplId {
        self.next_id
    }

    pub fn queued(&self) -> &BTreeMap<DepositSplId, QueuedSplDeposit> {
        &self.queued
    }

    pub fn swept(&self) -> &BTreeMap<Signature, SplSweep> {
        &self.swept
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
    pub(super) fn sweep(&mut self, deposit_ids: &[DepositSplId], signature: &Signature) {
        assert!(
            !deposit_ids.is_empty(),
            "Attempted to sweep no SPL deposits with transaction {signature}"
        );
        assert!(
            !self.swept.contains_key(signature),
            "Attempted to submit SPL sweep {signature} twice"
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
        self.swept.insert(*signature, SplSweep::plan(deposits));
    }
}

/// The deposits moved by one SPL sweep transaction, kept as they were queued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplSweep {
    deposits: BTreeMap<DepositSplId, QueuedSplDeposit>,
}

impl SplSweep {
    /// Groups the deposits of one sweep in deposit-id order.
    pub fn plan(deposits: impl IntoIterator<Item = (DepositSplId, QueuedSplDeposit)>) -> Self {
        let mut unique = BTreeMap::new();
        for (deposit_id, deposit) in deposits {
            assert!(
                unique.insert(deposit_id, deposit).is_none(),
                "Attempted to create an SPL sweep with deposit {deposit_id} twice"
            );
        }
        assert!(
            !unique.is_empty(),
            "Attempted to plan an SPL sweep without deposits"
        );
        Self { deposits: unique }
    }

    pub fn deposits(&self) -> &BTreeMap<DepositSplId, QueuedSplDeposit> {
        &self.deposits
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
