use crate::{
    address::{account_address, associated_token_address, minter_address},
    state::{
        QueuedSplDeposit, SchnorrPublicKey, SupportedSplToken, SweepMismatch, SweepSettlementError,
        TokenProgram, UnreadableOutcome,
        event::{CreditedSplDeposit, Signer, VersionedMessage},
        validate_sweep_transaction,
    },
};
use cksol_types::DepositSplId;
use icrc_ledger_types::icrc1::account::Account;
use sol_rpc_types::Lamport;
use solana_address::Address;
use solana_hash::Hash;
use solana_signature::Signature;
use solana_transaction::Message;
use solana_transaction_status_client_types::{
    EncodedConfirmedTransactionWithStatusMeta, UiTransactionStatusMeta, UiTransactionTokenBalance,
    option_serializer::OptionSerializer,
};
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

    /// Checks the executed message and balance changes against the sweep plan.
    /// Returns the lamport spend to debit and the token deposits to mint.
    pub fn settle(
        self,
        outcome: &EncodedConfirmedTransactionWithStatusMeta,
    ) -> Result<SettledSplSweep, SweepSettlementError> {
        let (message, meta) = validate_sweep_transaction(outcome, |hash| self.sweep_message(hash))?;
        let pre_lamports = meta.pre_balances[0];
        let post_lamports = meta.post_balances[0];
        let lamports_spent = pre_lamports
            .checked_sub(post_lamports)
            .filter(|spent| *spent >= meta.fee)
            .ok_or(SweepMismatch::UnexpectedLamportsSpent {
                pre: pre_lamports,
                post: post_lamports,
                fee: meta.fee,
            })?;
        self.validate_token_balances(&message, meta)?;
        let mints = self
            .deposits
            .into_iter()
            .map(|(deposit_id, deposit)| CreditedSplDeposit {
                deposit_id,
                amount_to_mint: deposit.balance,
            })
            .collect();
        Ok(SettledSplSweep {
            lamports_spent,
            mints,
        })
    }

    fn validate_token_balances(
        &self,
        message: &Message,
        meta: &UiTransactionStatusMeta,
    ) -> Result<(), SweepSettlementError> {
        let pre = token_balances(&meta.pre_token_balances, &message.account_keys)?;
        let post = token_balances(&meta.post_token_balances, &message.account_keys)?;
        let mut expected = BTreeMap::new();
        for transfer in &self.transfers {
            let deposit = &self.deposits[&transfer.deposit_id];
            let token_program = transfer.token_program.id();
            expected.insert(
                deposit.address,
                ExpectedTokenBalance {
                    mint: deposit.mint,
                    owner: transfer.owner,
                    token_program,
                    decimals: transfer.decimals,
                    change: -i128::from(deposit.balance),
                },
            );
            let destination =
                associated_token_address(&self.minter_address, &deposit.mint, &token_program);
            expected
                .entry(destination)
                .or_insert(ExpectedTokenBalance {
                    mint: deposit.mint,
                    owner: self.minter_address,
                    token_program,
                    decimals: transfer.decimals,
                    change: 0,
                })
                .change += i128::from(deposit.balance);
        }
        for (address, expected) in expected {
            let post = post
                .get(&address)
                .ok_or(UnreadableOutcome::IncompleteTokenBalances)?;
            let post = expected.amount(address, post)?;
            let pre = match pre.get(&address) {
                Some(balance) => expected.amount(address, balance)?,
                // A newly initialized ATA has no pre-token balance, even when prefunded
                // with lamports. Infer zero only when its post balance is the planned total.
                None if expected.change > 0 && i128::from(post) == expected.change => 0,
                None => return Err(UnreadableOutcome::IncompleteTokenBalances.into()),
            };
            if i128::from(post) - i128::from(pre) != expected.change {
                return Err(SweepMismatch::UnexpectedTokenBalanceChange {
                    address,
                    pre,
                    post,
                    expected_change: expected.change,
                }
                .into());
            }
        }
        Ok(())
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

/// A validated SPL sweep with the main account's spend and token deposits to mint.
#[derive(Debug, PartialEq, Eq)]
pub struct SettledSplSweep {
    lamports_spent: Lamport,
    mints: Vec<CreditedSplDeposit>,
}

impl SettledSplSweep {
    pub fn lamports_spent(&self) -> Lamport {
        self.lamports_spent
    }

    pub fn into_mints(self) -> Vec<CreditedSplDeposit> {
        self.mints
    }
}

struct ExpectedTokenBalance {
    mint: Address,
    owner: Address,
    token_program: Address,
    decimals: u8,
    change: i128,
}

impl ExpectedTokenBalance {
    fn amount(
        &self,
        address: Address,
        balance: &UiTransactionTokenBalance,
    ) -> Result<u64, SweepSettlementError> {
        let (OptionSerializer::Some(owner), OptionSerializer::Some(program)) =
            (&balance.owner, &balance.program_id)
        else {
            return Err(UnreadableOutcome::IncompleteTokenBalances.into());
        };
        if balance.mint != self.mint.to_string()
            || *owner != self.owner.to_string()
            || *program != self.token_program.to_string()
            || balance.ui_token_amount.decimals != self.decimals
        {
            return Err(SweepMismatch::UnexpectedTokenAccount { address }.into());
        }
        balance
            .ui_token_amount
            .amount
            .parse()
            .map_err(|error: std::num::ParseIntError| {
                UnreadableOutcome::InvalidTokenBalances {
                    reason: error.to_string(),
                }
                .into()
            })
    }
}

fn token_balances<'a>(
    balances: &'a OptionSerializer<Vec<UiTransactionTokenBalance>>,
    keys: &[Address],
) -> Result<BTreeMap<Address, &'a UiTransactionTokenBalance>, SweepSettlementError> {
    let OptionSerializer::Some(balances) = balances else {
        return Err(UnreadableOutcome::IncompleteTokenBalances.into());
    };
    let mut by_address = BTreeMap::new();
    for balance in balances {
        let address = *keys.get(balance.account_index as usize).ok_or_else(|| {
            UnreadableOutcome::InvalidTokenBalances {
                reason: "Token account index is out of bounds".to_string(),
            }
        })?;
        if by_address.insert(address, balance).is_some() {
            return Err(UnreadableOutcome::InvalidTokenBalances {
                reason: "Duplicate token account balance".to_string(),
            }
            .into());
        }
    }
    Ok(by_address)
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
