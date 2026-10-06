use cksol_types::DepositSplId;
use icrc_ledger_types::icrc1::account::Account;
use solana_address::Address;
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

/// SPL deposits waiting for a token sweep. State is rebuilt from queued-deposit events.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SplDeposits {
    next_id: DepositSplId,
    queued: BTreeMap<DepositSplId, QueuedSplDeposit>,
    in_flight_ids: BTreeMap<(Account, Address), DepositSplId>,
}

impl SplDeposits {
    pub fn next_id(&self) -> DepositSplId {
        self.next_id
    }

    pub fn queued(&self) -> &BTreeMap<DepositSplId, QueuedSplDeposit> {
        &self.queued
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
