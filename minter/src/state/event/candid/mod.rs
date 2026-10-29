#[cfg(test)]
mod tests;

use crate::{
    numeric::{LedgerBurnIndex, LedgerMintIndex},
    rpc::BlockHeight,
    state::{
        DepositBalance, SupportedSplToken, TokenProgram,
        event::{
            CreditedDeposit, CreditedSplDeposit, Event, EventType, Signer,
            SplWithdrawalFeeReimbursementRequest, SplWithdrawalRequest, SplWithdrawalRequestTerms,
            TransactionPurpose, VersionedMessage, WithdrawalRequest,
        },
    },
};
use cksol_types_internal::event::{
    CreditedDeposit as CandidCreditedDeposit, CreditedSplDeposit as CandidCreditedSplDeposit,
    Event as CandidEvent, EventType as CandidEventType, Signer as CandidSigner,
    SplWithdrawalRequestTerms as CandidSplWithdrawalRequestTerms,
    TransactionPurpose as CandidTransactionPurpose, VersionedTransactionMessage,
};
use ic_ed25519::PublicKey;
use sol_rpc_types::{
    Hash as CandidHash, Lamport, Pubkey as CandidAddress, Signature as CandidSignature,
};
use solana_address::Address;
use solana_hash::Hash;
use solana_signature::Signature;
use thiserror::Error;

impl From<Event> for CandidEvent {
    fn from(event: Event) -> Self {
        CandidEvent {
            timestamp: event.timestamp,
            payload: CandidEventType::from(event.payload),
        }
    }
}

impl From<EventType> for CandidEventType {
    fn from(event_type: EventType) -> Self {
        match event_type {
            EventType::Init(args) => CandidEventType::Init(args),
            EventType::Upgrade(args) => CandidEventType::Upgrade(args),
            EventType::AcceptedWithdrawalRequest(request) => {
                CandidEventType::AcceptedWithdrawalRequest {
                    account: request.account,
                    solana_address: CandidAddress::from(Address::from(request.solana_address)),
                    burn_block_index: *request.burn_block_index.get(),
                    burned_amount: request.burned_amount,
                    amount_to_transfer: request.amount_to_transfer,
                }
            }
            EventType::SubmittedTransaction {
                signature,
                message,
                signers,
                purpose,
            } => CandidEventType::SubmittedTransaction {
                signature: CandidSignature::from(signature),
                transaction: VersionedTransactionMessage::from(message),
                signers: signers.into_iter().map(CandidSigner::from).collect(),
                purpose: CandidTransactionPurpose::from(purpose),
            },
            EventType::SucceededTransaction { signature } => {
                CandidEventType::SucceededTransaction {
                    signature: CandidSignature::from(signature),
                }
            }
            EventType::SucceededSplWithdrawalTransaction {
                signature,
                lamports_spent,
            } => CandidEventType::SucceededSplWithdrawalTransaction {
                signature: signature.into(),
                lamports_spent,
            },
            EventType::FailedTransaction { signature } => CandidEventType::FailedTransaction {
                signature: CandidSignature::from(signature),
            },
            EventType::ExpiredTransaction { signature } => CandidEventType::ExpiredTransaction {
                signature: CandidSignature::from(signature),
            },
            EventType::QueuedDeposit {
                deposit_id,
                account,
                address,
                balance,
            } => CandidEventType::QueuedDeposit {
                deposit_id,
                account,
                address: CandidAddress::from(address),
                balance: Lamport::from(balance),
            },
            EventType::CreditedSweep {
                signature,
                amount_received,
                mints,
            } => CandidEventType::CreditedSweep {
                signature: CandidSignature::from(signature),
                amount_received,
                mints: mints.into_iter().map(CandidCreditedDeposit::from).collect(),
            },
            EventType::QuarantinedSweep { signature } => CandidEventType::QuarantinedSweep {
                signature: CandidSignature::from(signature),
            },
            EventType::MinterPublicKeyFetched {
                public_key,
                chain_code,
            } => CandidEventType::MinterPublicKeyFetched {
                public_key: public_key.serialize_raw().to_vec(),
                chain_code: chain_code.to_vec(),
            },
            EventType::MintedSweptDeposit {
                deposit_id,
                mint_block_index,
            } => CandidEventType::MintedSweptDeposit {
                deposit_id,
                mint_block_index: *mint_block_index.get(),
            },
            EventType::QuarantinedPendingMint { deposit_id } => {
                CandidEventType::QuarantinedPendingMint { deposit_id }
            }
            EventType::CreatedWithdrawalTransaction {
                burn_indices,
                nonce_account,
                nonce_value,
            } => CandidEventType::CreatedWithdrawalTransaction {
                burn_indices: burn_indices.iter().map(|idx| *idx.get()).collect(),
                nonce_account: CandidAddress::from(nonce_account),
                nonce_value: CandidHash::from(nonce_value),
            },
            EventType::AddedSplToken(token) => CandidEventType::AddedSplToken {
                mint: CandidAddress::from(token.mint),
                token_program: CandidAddress::from(token.token_program.id()),
                decimals: token.decimals,
                ledger_id: token.ledger_id,
                minimum_deposit_amount: token.minimum_deposit_amount,
                paused: token.paused,
                xrc_symbol: token.xrc_symbol,
                token_symbol: token.token_symbol,
            },
            EventType::QueuedSplDeposit {
                deposit_id,
                account,
                mint,
                address,
                balance,
                fee,
            } => CandidEventType::QueuedSplDeposit {
                deposit_id,
                account,
                mint: CandidAddress::from(mint),
                address: CandidAddress::from(address),
                balance,
                fee,
            },
            EventType::CreditedSplSweep {
                signature,
                lamports_spent,
                mints,
            } => CandidEventType::CreditedSplSweep {
                signature: CandidSignature::from(signature),
                lamports_spent,
                mints: mints
                    .into_iter()
                    .map(CandidCreditedSplDeposit::from)
                    .collect(),
            },
            EventType::QuarantinedSplSweep { signature } => CandidEventType::QuarantinedSplSweep {
                signature: CandidSignature::from(signature),
            },
            EventType::SplitFailedSplSweep { signature } => CandidEventType::SplitFailedSplSweep {
                signature: CandidSignature::from(signature),
            },
            EventType::MintedSweptSplDeposit {
                deposit_id,
                mint_block_index,
            } => CandidEventType::MintedSweptSplDeposit {
                deposit_id,
                mint_block_index: *mint_block_index.get(),
            },
            EventType::QuarantinedPendingSplMint { deposit_id } => {
                CandidEventType::QuarantinedPendingSplMint { deposit_id }
            }
            EventType::MintedSplDepositFee {
                deposit_id,
                mint_block_index,
            } => CandidEventType::MintedSplDepositFee {
                deposit_id,
                mint_block_index: *mint_block_index.get(),
            },
            EventType::QuarantinedPendingSplFeeMint { deposit_id } => {
                CandidEventType::QuarantinedPendingSplFeeMint { deposit_id }
            }
            EventType::ToppedUpOperatingBalance {
                burn_block_index,
                amount,
            } => CandidEventType::ToppedUpOperatingBalance {
                burn_block_index: *burn_block_index.get(),
                amount,
            },
            EventType::AcceptedSplWithdrawalRequest(request) => {
                CandidEventType::AcceptedSplWithdrawalRequest {
                    terms: request.terms.into(),
                    cksol_burn_block_index: *request.cksol_burn_block_index.get(),
                    ckspl_burn_block_index: *request.ckspl_burn_block_index.get(),
                }
            }
            EventType::QuarantinedSplWithdrawalFeeReimbursement {
                cksol_burn_block_index,
            } => CandidEventType::QuarantinedSplWithdrawalFeeReimbursement {
                cksol_burn_block_index: *cksol_burn_block_index.get(),
            },
            EventType::CreatedSplWithdrawalTransaction {
                cksol_burn_block_index,
                nonce_account,
                nonce_value,
            } => CandidEventType::CreatedSplWithdrawalTransaction {
                cksol_burn_block_index: *cksol_burn_block_index.get(),
                nonce_account: nonce_account.into(),
                nonce_value: nonce_value.into(),
            },
            EventType::ReimbursedSplWithdrawalFee {
                cksol_burn_block_index,
                mint_block_index,
            } => CandidEventType::ReimbursedSplWithdrawalFee {
                cksol_burn_block_index: *cksol_burn_block_index.get(),
                mint_block_index: *mint_block_index.get(),
            },
            EventType::FailedSplWithdrawalRequest(request) => {
                CandidEventType::FailedSplWithdrawalRequest {
                    cksol_burn_block_index: *request.cksol_burn_block_index.get(),
                    reimbursed_amount: request.reimbursed_amount,
                    to: request.to,
                }
            }
        }
    }
}

impl From<VersionedMessage> for VersionedTransactionMessage {
    fn from(message: VersionedMessage) -> Self {
        match message {
            VersionedMessage::Legacy(message) => VersionedTransactionMessage::Legacy(
                bincode::serialize(&message).expect("serializing transaction should succeed"),
            ),
        }
    }
}

impl From<Signer> for CandidSigner {
    fn from(signer: Signer) -> Self {
        match signer {
            Signer::Minter => CandidSigner::Minter,
            Signer::Account(account) => CandidSigner::Account(account),
        }
    }
}

impl From<TransactionPurpose> for CandidTransactionPurpose {
    fn from(purpose: TransactionPurpose) -> Self {
        match purpose {
            TransactionPurpose::SweepDeposit {
                deposit_ids,
                block_height,
            } => CandidTransactionPurpose::SweepDeposit {
                deposit_ids,
                block_height: block_height.get(),
            },
            TransactionPurpose::Withdrawal { burn_indices } => {
                CandidTransactionPurpose::Withdrawal {
                    burn_indices: burn_indices.iter().map(|idx| *idx.get()).collect(),
                }
            }
            TransactionPurpose::SplWithdrawal {
                cksol_burn_block_index,
            } => CandidTransactionPurpose::SplWithdrawal {
                cksol_burn_block_index: *cksol_burn_block_index.get(),
            },
            TransactionPurpose::SweepSplDeposits {
                deposit_ids,
                block_height,
            } => CandidTransactionPurpose::SweepSplDeposits {
                deposit_ids,
                block_height: block_height.get(),
            },
            TransactionPurpose::RetrySplDeposit {
                deposit_id,
                block_height,
            } => CandidTransactionPurpose::RetrySplDeposit {
                deposit_id,
                block_height: block_height.get(),
            },
        }
    }
}

impl From<CreditedDeposit> for CandidCreditedDeposit {
    fn from(mint: CreditedDeposit) -> Self {
        CandidCreditedDeposit {
            deposit_id: mint.deposit_id,
            amount_to_mint: mint.amount_to_mint,
        }
    }
}

impl From<CreditedSplDeposit> for CandidCreditedSplDeposit {
    fn from(mint: CreditedSplDeposit) -> Self {
        CandidCreditedSplDeposit {
            deposit_id: mint.deposit_id,
            amount_to_mint: mint.amount_to_mint,
        }
    }
}

impl TryFrom<CandidEvent> for Event {
    type Error = InvalidCandidEvent;

    fn try_from(event: CandidEvent) -> Result<Self, Self::Error> {
        Ok(Event {
            timestamp: event.timestamp,
            payload: EventType::try_from(event.payload)?,
        })
    }
}

impl TryFrom<CandidEventType> for EventType {
    type Error = InvalidCandidEvent;

    fn try_from(event_type: CandidEventType) -> Result<Self, Self::Error> {
        Ok(match event_type {
            CandidEventType::Init(args) => EventType::Init(args),
            CandidEventType::Upgrade(args) => EventType::Upgrade(args),
            CandidEventType::AcceptedWithdrawalRequest {
                account,
                solana_address,
                burn_block_index,
                burned_amount,
                amount_to_transfer,
            } => EventType::AcceptedWithdrawalRequest(WithdrawalRequest {
                account,
                solana_address: Address::from(solana_address).to_bytes(),
                burn_block_index: LedgerBurnIndex::new(burn_block_index),
                burned_amount,
                amount_to_transfer,
            }),
            CandidEventType::SubmittedTransaction {
                signature,
                transaction,
                signers,
                purpose,
            } => EventType::SubmittedTransaction {
                signature: Signature::from(signature),
                message: VersionedMessage::try_from(transaction)?,
                signers: signers.into_iter().map(Signer::from).collect(),
                purpose: TransactionPurpose::from(purpose),
            },
            CandidEventType::SucceededTransaction { signature } => {
                EventType::SucceededTransaction {
                    signature: Signature::from(signature),
                }
            }
            CandidEventType::SucceededSplWithdrawalTransaction {
                signature,
                lamports_spent,
            } => EventType::SucceededSplWithdrawalTransaction {
                signature: signature.into(),
                lamports_spent,
            },
            CandidEventType::FailedTransaction { signature } => EventType::FailedTransaction {
                signature: Signature::from(signature),
            },
            CandidEventType::ExpiredTransaction { signature } => EventType::ExpiredTransaction {
                signature: Signature::from(signature),
            },
            CandidEventType::QueuedDeposit {
                deposit_id,
                account,
                address,
                balance,
            } => EventType::QueuedDeposit {
                deposit_id,
                account,
                address: Address::from(address),
                balance: DepositBalance::new(balance).ok_or(
                    InvalidCandidEvent::DepositBalanceBelowRentExemption(balance),
                )?,
            },
            CandidEventType::CreditedSweep {
                signature,
                amount_received,
                mints,
            } => EventType::CreditedSweep {
                signature: Signature::from(signature),
                amount_received,
                mints: mints.into_iter().map(CreditedDeposit::from).collect(),
            },
            CandidEventType::QuarantinedSweep { signature } => EventType::QuarantinedSweep {
                signature: Signature::from(signature),
            },
            CandidEventType::MinterPublicKeyFetched {
                public_key,
                chain_code,
            } => EventType::MinterPublicKeyFetched {
                public_key: PublicKey::deserialize_raw(&public_key)
                    .map_err(|e| InvalidCandidEvent::InvalidPublicKey(e.to_string()))?,
                chain_code: <[u8; 32]>::try_from(chain_code)
                    .map_err(|bytes| InvalidCandidEvent::InvalidChainCode(bytes.len()))?,
            },
            CandidEventType::MintedSweptDeposit {
                deposit_id,
                mint_block_index,
            } => EventType::MintedSweptDeposit {
                deposit_id,
                mint_block_index: LedgerMintIndex::new(mint_block_index),
            },
            CandidEventType::QuarantinedPendingMint { deposit_id } => {
                EventType::QuarantinedPendingMint { deposit_id }
            }
            CandidEventType::CreatedWithdrawalTransaction {
                burn_indices,
                nonce_account,
                nonce_value,
            } => EventType::CreatedWithdrawalTransaction {
                burn_indices: burn_indices.into_iter().map(LedgerBurnIndex::new).collect(),
                nonce_account: Address::from(nonce_account),
                nonce_value: Hash::from(nonce_value),
            },
            CandidEventType::AddedSplToken {
                mint,
                token_program,
                decimals,
                ledger_id,
                minimum_deposit_amount,
                paused,
                xrc_symbol,
                token_symbol,
            } => EventType::AddedSplToken(SupportedSplToken {
                mint: Address::from(mint),
                token_program: TokenProgram::try_from(Address::from(token_program))
                    .map_err(|e| InvalidCandidEvent::InvalidTokenProgram(e.to_string()))?,
                decimals,
                ledger_id,
                minimum_deposit_amount,
                paused,
                xrc_symbol,
                token_symbol,
            }),
            CandidEventType::QueuedSplDeposit {
                deposit_id,
                account,
                mint,
                address,
                balance,
                fee,
            } => EventType::QueuedSplDeposit {
                deposit_id,
                account,
                mint: Address::from(mint),
                address: Address::from(address),
                balance,
                fee,
            },
            CandidEventType::CreditedSplSweep {
                signature,
                lamports_spent,
                mints,
            } => EventType::CreditedSplSweep {
                signature: Signature::from(signature),
                lamports_spent,
                mints: mints.into_iter().map(CreditedSplDeposit::from).collect(),
            },
            CandidEventType::QuarantinedSplSweep { signature } => EventType::QuarantinedSplSweep {
                signature: Signature::from(signature),
            },
            CandidEventType::SplitFailedSplSweep { signature } => EventType::SplitFailedSplSweep {
                signature: Signature::from(signature),
            },
            CandidEventType::MintedSweptSplDeposit {
                deposit_id,
                mint_block_index,
            } => EventType::MintedSweptSplDeposit {
                deposit_id,
                mint_block_index: LedgerMintIndex::new(mint_block_index),
            },
            CandidEventType::QuarantinedPendingSplMint { deposit_id } => {
                EventType::QuarantinedPendingSplMint { deposit_id }
            }
            CandidEventType::MintedSplDepositFee {
                deposit_id,
                mint_block_index,
            } => EventType::MintedSplDepositFee {
                deposit_id,
                mint_block_index: LedgerMintIndex::new(mint_block_index),
            },
            CandidEventType::QuarantinedPendingSplFeeMint { deposit_id } => {
                EventType::QuarantinedPendingSplFeeMint { deposit_id }
            }
            CandidEventType::ToppedUpOperatingBalance {
                burn_block_index,
                amount,
            } => EventType::ToppedUpOperatingBalance {
                burn_block_index: LedgerBurnIndex::new(burn_block_index),
                amount,
            },
            CandidEventType::AcceptedSplWithdrawalRequest {
                terms,
                cksol_burn_block_index,
                ckspl_burn_block_index,
            } => EventType::AcceptedSplWithdrawalRequest(SplWithdrawalRequest {
                terms: terms.into(),
                cksol_burn_block_index: LedgerBurnIndex::new(cksol_burn_block_index),
                ckspl_burn_block_index: LedgerBurnIndex::new(ckspl_burn_block_index),
            }),
            CandidEventType::QuarantinedSplWithdrawalFeeReimbursement {
                cksol_burn_block_index,
            } => EventType::QuarantinedSplWithdrawalFeeReimbursement {
                cksol_burn_block_index: LedgerBurnIndex::new(cksol_burn_block_index),
            },
            CandidEventType::CreatedSplWithdrawalTransaction {
                cksol_burn_block_index,
                nonce_account,
                nonce_value,
            } => EventType::CreatedSplWithdrawalTransaction {
                cksol_burn_block_index: cksol_burn_block_index.into(),
                nonce_account: nonce_account.into(),
                nonce_value: nonce_value.into(),
            },
            CandidEventType::ReimbursedSplWithdrawalFee {
                cksol_burn_block_index,
                mint_block_index,
            } => EventType::ReimbursedSplWithdrawalFee {
                cksol_burn_block_index: LedgerBurnIndex::new(cksol_burn_block_index),
                mint_block_index: LedgerMintIndex::new(mint_block_index),
            },
            CandidEventType::FailedSplWithdrawalRequest {
                cksol_burn_block_index,
                reimbursed_amount,
                to,
            } => EventType::FailedSplWithdrawalRequest(SplWithdrawalFeeReimbursementRequest {
                cksol_burn_block_index: LedgerBurnIndex::new(cksol_burn_block_index),
                reimbursed_amount,
                to,
            }),
        })
    }
}

impl From<SplWithdrawalRequestTerms> for CandidSplWithdrawalRequestTerms {
    fn from(request: SplWithdrawalRequestTerms) -> Self {
        Self {
            account: request.account,
            mint: request.mint.into(),
            ledger_id: request.ledger_id,
            address: request.address.into(),
            amount: request.amount,
            fee: request.fee,
            create_destination_account: request.create_destination_account,
        }
    }
}

impl From<CandidSplWithdrawalRequestTerms> for SplWithdrawalRequestTerms {
    fn from(request: CandidSplWithdrawalRequestTerms) -> Self {
        Self {
            account: request.account,
            mint: request.mint.into(),
            ledger_id: request.ledger_id,
            address: request.address.into(),
            amount: request.amount,
            fee: request.fee,
            create_destination_account: request.create_destination_account,
        }
    }
}

impl TryFrom<VersionedTransactionMessage> for VersionedMessage {
    type Error = InvalidCandidEvent;

    fn try_from(message: VersionedTransactionMessage) -> Result<Self, Self::Error> {
        match message {
            VersionedTransactionMessage::Legacy(bytes) => bincode::deserialize(&bytes)
                .map(VersionedMessage::Legacy)
                .map_err(|e| InvalidCandidEvent::InvalidMessage(e.to_string())),
        }
    }
}

impl From<CandidSigner> for Signer {
    fn from(signer: CandidSigner) -> Self {
        match signer {
            CandidSigner::Minter => Signer::Minter,
            CandidSigner::Account(account) => Signer::Account(account),
        }
    }
}

impl From<CandidTransactionPurpose> for TransactionPurpose {
    fn from(purpose: CandidTransactionPurpose) -> Self {
        match purpose {
            CandidTransactionPurpose::SweepDeposit {
                deposit_ids,
                block_height,
            } => TransactionPurpose::SweepDeposit {
                deposit_ids,
                block_height: BlockHeight::new(block_height),
            },
            CandidTransactionPurpose::Withdrawal { burn_indices } => {
                TransactionPurpose::Withdrawal {
                    burn_indices: burn_indices.into_iter().map(LedgerBurnIndex::new).collect(),
                }
            }
            CandidTransactionPurpose::SplWithdrawal {
                cksol_burn_block_index,
            } => TransactionPurpose::SplWithdrawal {
                cksol_burn_block_index: cksol_burn_block_index.into(),
            },
            CandidTransactionPurpose::SweepSplDeposits {
                deposit_ids,
                block_height,
            } => TransactionPurpose::SweepSplDeposits {
                deposit_ids,
                block_height: BlockHeight::new(block_height),
            },
            CandidTransactionPurpose::RetrySplDeposit {
                deposit_id,
                block_height,
            } => TransactionPurpose::RetrySplDeposit {
                deposit_id,
                block_height: BlockHeight::new(block_height),
            },
        }
    }
}

impl From<CandidCreditedDeposit> for CreditedDeposit {
    fn from(mint: CandidCreditedDeposit) -> Self {
        CreditedDeposit {
            deposit_id: mint.deposit_id,
            amount_to_mint: mint.amount_to_mint,
        }
    }
}

impl From<CandidCreditedSplDeposit> for CreditedSplDeposit {
    fn from(mint: CandidCreditedSplDeposit) -> Self {
        CreditedSplDeposit {
            deposit_id: mint.deposit_id,
            amount_to_mint: mint.amount_to_mint,
        }
    }
}

/// A Candid event that cannot be converted back into a minter event.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum InvalidCandidEvent {
    #[error("invalid transaction message: {0}")]
    InvalidMessage(String),
    #[error("invalid public key: {0}")]
    InvalidPublicKey(String),
    #[error("invalid chain code: expected 32 bytes, got {0}")]
    InvalidChainCode(usize),
    #[error("deposit balance of {0} lamports is below the rent exemption threshold")]
    DepositBalanceBelowRentExemption(Lamport),
    #[error("invalid token program: {0}")]
    InvalidTokenProgram(String),
}
