use crate::numeric::LedgerMintIndex;
use crate::state::{
    SupportedSplToken,
    event::{CreditedSplDeposit, Signer, VersionedMessage},
};
use cksol_types::{DepositSplId, DepositSplStatus, SplFeeMintStatus};
use icrc_ledger_types::icrc1::account::Account;
use solana_address::Address;
use solana_signature::Signature;
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

mod sweeps;
pub use sweeps::{SettledSplSweep, SplSweep, SplSweepRecoveryError, SplSweeps, SplTransfer};

/// SPL deposits grouped by their progress towards a token mint.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SplDeposits {
    next_id: DepositSplId,
    queued: BTreeMap<DepositSplId, QueuedSplDeposit>,
    retrying: BTreeMap<DepositSplId, QueuedSplDeposit>,
    swept: SplSweeps,
    finalized: SplSweeps,
    pending_mints: BTreeMap<DepositSplId, PendingSplMint>,
    minted: BTreeMap<DepositSplId, MintedSplSweep>,
    dropped: BTreeMap<DepositSplId, SweptSplDeposit>,
    quarantined: BTreeMap<DepositSplId, SweptSplDeposit>,
    in_flight_ids: BTreeMap<(Account, Address), DepositSplId>,
    pending_fee_mints: BTreeMap<DepositSplId, PendingSplFeeMint>,
    minted_fees: BTreeMap<DepositSplId, LedgerMintIndex>,
    quarantined_fee_mints: BTreeMap<DepositSplId, PendingSplFeeMint>,
}

impl SplDeposits {
    pub fn next_id(&self) -> DepositSplId {
        self.next_id
    }

    pub fn queued(&self) -> &BTreeMap<DepositSplId, QueuedSplDeposit> {
        &self.queued
    }

    pub fn retrying(&self) -> &BTreeMap<DepositSplId, QueuedSplDeposit> {
        &self.retrying
    }

    pub fn swept(&self) -> &SplSweeps {
        &self.swept
    }

    /// Whether the submitted sweep carries more than one deposit, so a failure splits it.
    pub fn is_batch(&self, signature: &Signature) -> bool {
        self.swept
            .get(signature)
            .is_some_and(|sweep| sweep.deposits().len() > 1)
    }

    pub fn finalized(&self) -> &SplSweeps {
        &self.finalized
    }

    pub fn dropped(&self) -> &BTreeMap<DepositSplId, SweptSplDeposit> {
        &self.dropped
    }

    pub fn pending_mints(&self) -> &BTreeMap<DepositSplId, PendingSplMint> {
        &self.pending_mints
    }

    pub fn quarantined(&self) -> &BTreeMap<DepositSplId, SweptSplDeposit> {
        &self.quarantined
    }

    pub fn minted(&self) -> &BTreeMap<DepositSplId, MintedSplSweep> {
        &self.minted
    }

    pub fn pending_fee_mints(&self) -> &BTreeMap<DepositSplId, PendingSplFeeMint> {
        &self.pending_fee_mints
    }

    pub fn minted_fees(&self) -> &BTreeMap<DepositSplId, LedgerMintIndex> {
        &self.minted_fees
    }

    pub fn quarantined_fee_mints(&self) -> &BTreeMap<DepositSplId, PendingSplFeeMint> {
        &self.quarantined_fee_mints
    }

    pub fn in_flight_id(&self, account: &Account, mint: &Address) -> Option<DepositSplId> {
        self.in_flight_ids.get(&(*account, *mint)).copied()
    }

    pub fn status(&self, deposit_id: DepositSplId) -> DepositSplStatus {
        if let Some(deposit) = self
            .queued
            .get(&deposit_id)
            .or_else(|| self.retrying.get(&deposit_id))
        {
            return DepositSplStatus::Queued {
                balance: deposit.balance,
                fee: deposit.fee,
            };
        }
        if let Some((signature, _)) = self.swept.deposit(deposit_id) {
            return DepositSplStatus::Swept {
                signature: (*signature).into(),
            };
        }
        if let Some((signature, _)) = self.finalized.deposit(deposit_id) {
            return DepositSplStatus::Finalized {
                signature: (*signature).into(),
            };
        }
        if let Some(pending) = self.pending_mints.get(&deposit_id) {
            return DepositSplStatus::Finalized {
                signature: pending.sweep_signature().into(),
            };
        }
        if let Some(minted) = self.minted.get(&deposit_id) {
            return DepositSplStatus::Minted {
                block_index: *minted.mint_block_index.get(),
                minted_amount: minted.minted_amount,
                fee: self.fee_status(deposit_id),
            };
        }
        if let Some(dropped) = self.dropped.get(&deposit_id) {
            return DepositSplStatus::Dropped {
                signature: dropped.signature.into(),
            };
        }
        if let Some(quarantined) = self.quarantined.get(&deposit_id) {
            return DepositSplStatus::Quarantined {
                signature: quarantined.signature.into(),
            };
        }
        DepositSplStatus::NotFound
    }

    fn fee_status(&self, deposit_id: DepositSplId) -> SplFeeMintStatus {
        if self.pending_fee_mints.contains_key(&deposit_id) {
            return SplFeeMintStatus::Pending;
        }
        if let Some(block_index) = self.minted_fees.get(&deposit_id) {
            return SplFeeMintStatus::Minted {
                block_index: *block_index.get(),
            };
        }
        if self.quarantined_fee_mints.contains_key(&deposit_id) {
            return SplFeeMintStatus::Quarantined;
        }
        SplFeeMintStatus::None
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
        assert!(
            deposit.fee < deposit.balance,
            "Attempted to queue SPL deposit {deposit_id} with a fee at or above its balance"
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
            .collect();
        self.record_sweep(deposits, message, signers, tokens, signature);
    }

    /// Moves one retried deposit to its submitted sweep.
    pub(super) fn retry(
        &mut self,
        deposit_id: DepositSplId,
        message: &VersionedMessage,
        signers: &[Signer],
        tokens: &BTreeMap<Address, SupportedSplToken>,
        signature: &Signature,
    ) {
        let deposit = self.retrying.remove(&deposit_id).unwrap_or_else(|| {
            panic!("Attempted to retry SPL deposit {deposit_id} that is not retrying")
        });
        self.record_sweep(
            vec![(deposit_id, deposit)],
            message,
            signers,
            tokens,
            signature,
        );
    }

    fn record_sweep(
        &mut self,
        deposits: Vec<(DepositSplId, QueuedSplDeposit)>,
        message: &VersionedMessage,
        signers: &[Signer],
        tokens: &BTreeMap<Address, SupportedSplToken>,
        signature: &Signature,
    ) {
        let sweep = SplSweep::recover(deposits, tokens, message, signers).unwrap_or_else(|error| {
            panic!("Attempted to sweep with transaction {signature}: {error}")
        });
        self.swept.insert(*signature, sweep);
    }

    /// Moves the deposits of a failed batch to the retrying stage, where each sweeps alone.
    pub(super) fn retry_individually(&mut self, signature: &Signature) {
        let sweep = self.swept.remove(signature).unwrap_or_else(|| {
            panic!("Attempted to split SPL sweep {signature} that is not swept")
        });
        assert!(
            sweep.deposits().len() > 1,
            "Attempted to split a single-deposit SPL sweep"
        );
        for (id, deposit) in sweep.deposits() {
            assert!(
                self.retrying.insert(*id, deposit.clone()).is_none(),
                "Attempted to retry already retrying SPL deposit {id}"
            );
        }
    }

    pub(super) fn finalize_swept(&mut self, signature: &Signature) {
        let sweep = self.swept.remove(signature).unwrap_or_else(|| {
            panic!("Attempted to finalize SPL sweep {signature} that is not swept")
        });
        self.finalized.insert(*signature, sweep);
    }

    /// Moves every deposit of a finalized sweep to its pending token mint.
    pub(super) fn credit_sweep(
        &mut self,
        signature: &Signature,
        mints: &[CreditedSplDeposit],
        timestamp: u64,
    ) {
        let sweep = self.finalized.remove(signature).unwrap_or_else(|| {
            panic!("Attempted to credit SPL sweep {signature} that is not finalized")
        });
        assert_eq!(
            mints.len(),
            sweep.deposits().len(),
            "Attempted to credit SPL sweep {signature} with {} mints for {} deposits",
            mints.len(),
            sweep.deposits().len()
        );
        for mint in mints {
            let deposit = sweep.deposits().get(&mint.deposit_id).unwrap_or_else(|| {
                panic!(
                    "Attempted to credit SPL deposit {} that is not part of sweep {signature}",
                    mint.deposit_id
                )
            });
            assert_eq!(
                mint.amount_to_mint,
                deposit.amount_to_mint(),
                "Attempted to credit SPL deposit {} with an amount different from its transferred balance minus the fee",
                mint.deposit_id
            );
            let pending = PendingSplMint {
                deposit: SweptSplDeposit {
                    deposit: deposit.clone(),
                    signature: *signature,
                },
                amount_to_mint: mint.amount_to_mint,
                created_at_time: timestamp,
            };
            assert!(
                self.pending_mints
                    .insert(mint.deposit_id, pending)
                    .is_none(),
                "Attempted to credit SPL deposit {} twice in sweep {signature}",
                mint.deposit_id
            );
        }
    }

    /// Quarantines every deposit of a finalized sweep, keeping its account and mint in flight.
    pub(super) fn quarantine_sweep(&mut self, signature: &Signature) {
        let sweep = self.finalized.remove(signature).unwrap_or_else(|| {
            panic!("Attempted to quarantine SPL sweep {signature} that is not finalized")
        });
        self.quarantined
            .extend(sweep.deposits().iter().map(|(deposit_id, deposit)| {
                (
                    *deposit_id,
                    SweptSplDeposit {
                        deposit: deposit.clone(),
                        signature: *signature,
                    },
                )
            }));
    }

    pub(super) fn mint(
        &mut self,
        deposit_id: DepositSplId,
        mint_block_index: LedgerMintIndex,
        timestamp: u64,
    ) {
        let pending = self.pending_mints.remove(&deposit_id).unwrap_or_else(|| {
            panic!("Attempted to mint SPL deposit {deposit_id} that has no pending mint")
        });
        let deposit = &pending.deposit.deposit;
        assert_eq!(
            self.in_flight_ids.remove(&(deposit.account, deposit.mint)),
            Some(deposit_id),
            "BUG: SPL deposit {deposit_id} is not in flight"
        );
        if deposit.fee > 0 {
            assert!(
                self.pending_fee_mints
                    .insert(
                        deposit_id,
                        PendingSplFeeMint {
                            mint: deposit.mint,
                            amount: deposit.fee,
                            signature: pending.deposit.signature,
                            created_at_time: timestamp,
                        },
                    )
                    .is_none(),
                "Attempted to queue the fee mint of SPL deposit {deposit_id} twice"
            );
        }
        self.minted.insert(
            deposit_id,
            MintedSplSweep {
                deposit: pending.deposit,
                minted_amount: pending.amount_to_mint,
                mint_block_index,
            },
        );
    }

    pub(super) fn mint_fee(&mut self, deposit_id: DepositSplId, mint_block_index: LedgerMintIndex) {
        self.pending_fee_mints.remove(&deposit_id).unwrap_or_else(|| {
            panic!("Attempted to mint the fee of SPL deposit {deposit_id} that has no pending fee mint")
        });
        assert!(
            self.minted_fees
                .insert(deposit_id, mint_block_index)
                .is_none(),
            "Attempted to mint the fee of SPL deposit {deposit_id} twice"
        );
    }

    pub(super) fn quarantine_fee_mint(&mut self, deposit_id: DepositSplId) {
        let pending = self.pending_fee_mints.remove(&deposit_id).unwrap_or_else(|| {
            panic!("Attempted to quarantine the fee of SPL deposit {deposit_id} that has no pending fee mint")
        });
        self.quarantined_fee_mints.insert(deposit_id, pending);
    }

    pub(super) fn quarantine_pending_mint(&mut self, deposit_id: DepositSplId) {
        let pending = self.pending_mints.remove(&deposit_id).unwrap_or_else(|| {
            panic!("Attempted to quarantine SPL deposit {deposit_id} that has no pending mint")
        });
        self.quarantined.insert(deposit_id, pending.deposit);
    }

    /// Drops a failed or expired sweep and releases its account and mint pairs.
    pub(super) fn drop_swept(&mut self, signature: &Signature) {
        let sweep = self
            .swept
            .remove(signature)
            .unwrap_or_else(|| panic!("Attempted to drop SPL sweep {signature} that is not swept"));
        for (deposit_id, deposit) in sweep.deposits() {
            self.drop_deposit(*deposit_id, deposit, signature);
        }
    }

    fn drop_deposit(
        &mut self,
        deposit_id: DepositSplId,
        deposit: &QueuedSplDeposit,
        signature: &Signature,
    ) {
        assert_eq!(
            self.in_flight_ids.remove(&(deposit.account, deposit.mint)),
            Some(deposit_id),
            "BUG: SPL deposit {deposit_id} is not in flight"
        );
        self.dropped.insert(
            deposit_id,
            SweptSplDeposit {
                deposit: deposit.clone(),
                signature: *signature,
            },
        );
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
    /// The fee quoted at acceptance, deducted from the ledger mint in the mint's smallest units.
    pub fee: u64,
}

impl QueuedSplDeposit {
    pub fn amount_to_mint(&self) -> u64 {
        self.balance
            .checked_sub(self.fee)
            .expect("BUG: a queued SPL deposit fee must not exceed its balance")
    }
}

/// A deposit together with the sweep transaction submitted to move its tokens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweptSplDeposit {
    pub deposit: QueuedSplDeposit,
    pub signature: Signature,
}

/// A swept SPL deposit waiting for its token ledger mint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSplMint {
    pub deposit: SweptSplDeposit,
    pub amount_to_mint: u64,
    /// The credit event's timestamp, reused by ledger retries for deduplication.
    pub created_at_time: u64,
}

/// A deposit fee waiting to be minted after the user's mint completed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSplFeeMint {
    pub mint: Address,
    pub amount: u64,
    pub signature: Signature,
    /// The user mint event's timestamp, reused by fee retries for deduplication.
    pub created_at_time: u64,
}

/// A swept SPL deposit whose token ledger mint completed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MintedSplSweep {
    pub deposit: SweptSplDeposit,
    pub minted_amount: u64,
    pub mint_block_index: LedgerMintIndex,
}

impl PendingSplMint {
    pub fn account(&self) -> Account {
        self.deposit.deposit.account
    }

    pub fn sweep_signature(&self) -> Signature {
        self.deposit.signature
    }
}
