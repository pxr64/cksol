use crate::{
    address::{account_address, associated_token_address, minter_address},
    state::{
        QueuedSplDeposit, SchnorrPublicKey, SupportedSplToken, TokenProgram,
        event::{Signer, VersionedMessage},
    },
};
use cksol_types::DepositSplId;
use icrc_ledger_types::icrc1::account::Account;
use solana_address::Address;
use solana_hash::Hash;
use solana_signature::Signature;
use solana_transaction::Message;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

#[cfg(test)]
mod tests;

/// The SPL sweep transactions at one stage of the deposit lifecycle, keyed by signature.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SplSweeps {
    by_signature: BTreeMap<Signature, SplSweep>,
}

impl SplSweeps {
    pub fn get(&self, signature: &Signature) -> Option<&SplSweep> {
        self.by_signature.get(signature)
    }

    pub fn deposit(&self, deposit_id: DepositSplId) -> Option<(&Signature, &QueuedSplDeposit)> {
        self.by_signature
            .iter()
            .find_map(|(signature, sweep)| Some((signature, sweep.deposits.get(&deposit_id)?)))
    }

    pub fn signatures(&self) -> impl Iterator<Item = &Signature> {
        self.by_signature.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Signature, &SplSweep)> {
        self.by_signature.iter()
    }

    pub fn len(&self) -> usize {
        self.by_signature.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_signature.is_empty()
    }

    pub fn deposit_count(&self) -> usize {
        self.by_signature
            .values()
            .map(|sweep| sweep.deposits.len())
            .sum()
    }

    pub(super) fn insert(&mut self, signature: Signature, sweep: SplSweep) {
        for deposit_id in sweep.deposits().keys() {
            assert!(
                self.deposit(*deposit_id).is_none(),
                "Attempted to record SPL deposit {deposit_id} in sweep {signature} while another sweep holds it"
            );
        }
        assert!(
            self.by_signature.insert(signature, sweep).is_none(),
            "Attempted to record SPL sweep {signature} twice"
        );
    }

    pub(super) fn remove(&mut self, signature: &Signature) -> Option<SplSweep> {
        self.by_signature.remove(signature)
    }
}

/// The plan of one SPL sweep, including its token transfers and minter fee payer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplSweep {
    deposits: BTreeMap<DepositSplId, QueuedSplDeposit>,
    minter_address: Address,
    transfers: Vec<SplTransfer>,
}

impl SplSweep {
    pub fn plan(
        deposits: impl IntoIterator<Item = (DepositSplId, QueuedSplDeposit)>,
        tokens: &BTreeMap<Address, SupportedSplToken>,
        master_key: &SchnorrPublicKey,
    ) -> Self {
        let deposits: Vec<_> = deposits.into_iter().collect();
        let owners = deposits
            .iter()
            .map(|(_, deposit)| {
                (
                    deposit.account,
                    account_address(master_key, &deposit.account),
                )
            })
            .collect();
        Self::plan_with_owners(deposits, tokens, minter_address(master_key), &owners)
            .unwrap_or_else(|error| panic!("Attempted to plan an SPL sweep: {error}"))
    }

    fn plan_with_owners(
        deposits: impl IntoIterator<Item = (DepositSplId, QueuedSplDeposit)>,
        tokens: &BTreeMap<Address, SupportedSplToken>,
        minter_address: Address,
        owners: &BTreeMap<Account, Address>,
    ) -> Result<Self, SplSweepRecoveryError> {
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
        let transfers = unique
            .iter()
            .map(|(deposit_id, deposit)| {
                let token = tokens
                    .get(&deposit.mint)
                    .ok_or(SplSweepRecoveryError::UnregisteredMint { mint: deposit.mint })?;
                if token.mint != deposit.mint {
                    return Err(SplSweepRecoveryError::MintMismatch);
                }
                let owner = *owners
                    .get(&deposit.account)
                    .ok_or(SplSweepRecoveryError::InvalidSigners)?;
                if deposit.address
                    != associated_token_address(&owner, &token.mint, &token.token_program.id())
                {
                    return Err(SplSweepRecoveryError::SourceMismatch {
                        deposit_id: *deposit_id,
                    });
                }
                Ok(SplTransfer {
                    deposit_id: *deposit_id,
                    owner,
                    token_program: token.token_program,
                    decimals: token.decimals,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            deposits: unique,
            minter_address,
            transfers,
        })
    }

    /// Rebuilds the plan from the recorded signer keys and checks the complete submitted message.
    /// Recovery does not depend on the transient master-key cache.
    pub fn recover(
        deposits: impl IntoIterator<Item = (DepositSplId, QueuedSplDeposit)>,
        tokens: &BTreeMap<Address, SupportedSplToken>,
        submitted: &VersionedMessage,
        signers: &[Signer],
    ) -> Result<Self, SplSweepRecoveryError> {
        let VersionedMessage::Legacy(message) = submitted;
        let required = message.header.num_required_signatures as usize;
        if required == 0
            || message.account_keys.len() < required
            || signers.len() != required
            || signers.first() != Some(&Signer::Minter)
        {
            return Err(SplSweepRecoveryError::InvalidSigners);
        }
        let mut owners = BTreeMap::new();
        for (signer, key) in signers
            .iter()
            .skip(1)
            .zip(message.account_keys.iter().skip(1))
        {
            let Signer::Account(account) = signer else {
                return Err(SplSweepRecoveryError::InvalidSigners);
            };
            if owners.insert(*account, *key).is_some() {
                return Err(SplSweepRecoveryError::InvalidSigners);
            }
        }
        let sweep = Self::plan_with_owners(deposits, tokens, message.account_keys[0], &owners)?;
        let planned = VersionedMessage::Legacy(sweep.sweep_message(message.recent_blockhash));
        if planned != *submitted {
            return Err(SplSweepRecoveryError::UnexpectedMessage {
                planned: Box::new(planned),
                submitted: Box::new(submitted.clone()),
            });
        }
        Ok(sweep)
    }

    pub fn deposits(&self) -> &BTreeMap<DepositSplId, QueuedSplDeposit> {
        &self.deposits
    }

    pub fn minter_address(&self) -> Address {
        self.minter_address
    }

    pub fn transfers(&self) -> &[SplTransfer] {
        &self.transfers
    }

    /// The signers of the given message of this sweep, in signature order.
    pub fn signers(&self, message: &Message) -> Vec<Signer> {
        let accounts_by_owner: BTreeMap<Address, Account> = self
            .transfers
            .iter()
            .map(|transfer| (transfer.owner, self.deposits[&transfer.deposit_id].account))
            .collect();
        message
            .signer_keys()
            .iter()
            .map(|key| {
                if **key == self.minter_address {
                    Signer::Minter
                } else {
                    let account = accounts_by_owner
                        .get(key)
                        .copied()
                        .unwrap_or_else(|| panic!("BUG: unexpected SPL sweep signer {key}"));
                    Signer::Account(account)
                }
            })
            .collect()
    }

    /// One destination account creation per token, followed by each planned transfer.
    pub fn sweep_message(&self, recent_blockhash: Hash) -> Message {
        let mut destinations = BTreeSet::new();
        let mut instructions = Vec::new();
        for transfer in &self.transfers {
            let deposit = &self.deposits[&transfer.deposit_id];
            let token_program = transfer.token_program.id();
            let destination =
                associated_token_address(&self.minter_address, &deposit.mint, &token_program);
            if destinations.insert(destination) {
                instructions.push(
                    spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
                        &self.minter_address.to_bytes().into(),
                        &self.minter_address.to_bytes().into(),
                        &deposit.mint.to_bytes().into(),
                        &token_program.to_bytes().into(),
                    ),
                );
            }
            instructions.push(
                spl_token_2022_interface::instruction::transfer_checked(
                    &token_program.to_bytes().into(),
                    &deposit.address.to_bytes().into(),
                    &deposit.mint.to_bytes().into(),
                    &destination.to_bytes().into(),
                    &transfer.owner.to_bytes().into(),
                    &[],
                    deposit.balance,
                    transfer.decimals,
                )
                .expect("BUG: supported SPL token program must accept transfer_checked"),
            );
        }
        Message::new_with_blockhash(&instructions, Some(&self.minter_address), &recent_blockhash)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplTransfer {
    pub deposit_id: DepositSplId,
    pub owner: Address,
    pub token_program: TokenProgram,
    pub decimals: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum SplSweepRecoveryError {
    #[error("the recorded signers do not match the SPL sweep")]
    InvalidSigners,
    #[error("SPL sweep mint {mint} must be registered")]
    UnregisteredMint { mint: Address },
    #[error("BUG: SPL sweep mint mismatch")]
    MintMismatch,
    #[error("BUG: SPL sweep source mismatch for deposit {deposit_id}")]
    SourceMismatch { deposit_id: DepositSplId },
    #[error("the plan builds {planned:?} but the submitted message is {submitted:?}")]
    UnexpectedMessage {
        planned: Box<VersionedMessage>,
        submitted: Box<VersionedMessage>,
    },
}
