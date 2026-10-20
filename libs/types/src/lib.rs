//! Candid types used by the Candid interface of the ckSOL minter.

#![forbid(unsafe_code)]
#![forbid(missing_docs)]

use candid::{CandidType, Principal};
use icrc_ledger_types::icrc1::account::{Account, Subaccount};
pub use memo::{BurnMemo, MAX_SERIALIZED_MEMO_BYTES, Memo, MintMemo};
use serde::{Deserialize, Serialize};
pub use sol_rpc_types::{Lamport, Pubkey as Address, Signature};
use thiserror::Error;

mod memo;

/// Index of a mint transaction on the ckSOL ledger.
pub type LedgerMintIndex = u64;

/// Index of a burn transaction on the ckSOL ledger.
pub type LedgerBurnIndex = u64;

/// Arguments for a request to the `get_deposit_address` ckSOL minter endpoint.
#[derive(Clone, Eq, PartialEq, Debug, Default, CandidType, Deserialize, Serialize)]
pub struct GetDepositAddressArgs {
    /// The principal to deposit funds to.
    ///
    /// If not set, defaults to the caller's principal.
    /// The resolved owner must be a non-anonymous principal.
    pub owner: Option<Principal>,
    /// The subaccount to deposit funds to.
    pub subaccount: Option<Subaccount>,
}

impl From<Account> for GetDepositAddressArgs {
    fn from(account: Account) -> Self {
        Self {
            owner: Some(account.owner),
            subaccount: account.subaccount,
        }
    }
}

/// Arguments for a request to the `deposit_sol` ckSOL minter endpoint.
#[derive(Clone, Eq, PartialEq, Debug, Default, CandidType, Deserialize, Serialize)]
pub struct DepositSolArgs {
    /// The principal to credit with the deposit.
    ///
    /// If not set, defaults to the caller's principal.
    /// The resolved owner must be a non-anonymous principal.
    pub owner: Option<Principal>,
    /// The subaccount to credit with the deposit.
    pub subaccount: Option<Subaccount>,
}

impl From<Account> for DepositSolArgs {
    fn from(account: Account) -> Self {
        Self {
            owner: Some(account.owner),
            subaccount: account.subaccount,
        }
    }
}

/// Identifies a deposit queued by the `deposit_sol` ckSOL minter endpoint.
///
/// A sequence number assigned when the deposit is queued.
pub type DepositSolId = u64;

/// The status of a deposit queued by the `deposit_sol` ckSOL minter endpoint.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize, Serialize)]
pub enum DepositSolStatus {
    /// No deposit with this identifier was queued.
    NotFound,
    /// The deposit address is queued for a sweep to the minter's main account.
    Queued {
        /// The amount that will be swept from the deposit address.
        sweepable_amount: Lamport,
    },
    /// A Solana transaction sweeping the deposit address to the minter's main account
    /// has been submitted but is not yet finalized.
    Swept {
        /// The signature of the sweep transaction.
        signature: Signature,
    },
    /// The sweep transaction was finalized successfully, so the deposited SOL reached
    /// the minter's main account, but the ckSOL mint has not landed yet.
    Finalized {
        /// The signature of the sweep transaction.
        signature: Signature,
    },
    /// The minter minted ckSOL for the deposit on the ledger.
    Minted {
        /// The mint transaction index on the ckSOL ledger.
        block_index: LedgerMintIndex,
        /// The minted amount: the swept amount minus the deposit's share of the
        /// transaction fee of the sweep.
        minted_amount: Lamport,
    },
    /// The sweep transaction failed, or expired without ever being seen on chain, so no
    /// ckSOL is owed. Calling `deposit_sol` again queues a new sweep of whatever balance
    /// the deposit address still holds.
    Dropped {
        /// The signature of the sweep transaction.
        signature: Signature,
    },
    /// The minter stopped processing the deposit: either the finalized sweep's outcome
    /// did not match the plan the minter submitted it with, so the amount to credit
    /// cannot be determined safely and no ckSOL was minted, or the ckSOL mint of the
    /// credited deposit could not be completed, in which case the mint may nevertheless
    /// have landed on the ledger. This is not expected to happen and resolving it
    /// requires a minter upgrade: before minting by hand, search the ledger for a mint
    /// whose memo carries the sweep signature and the deposit id, otherwise a double
    /// mint results. Meanwhile the account stays in flight, so `deposit_sol` keeps
    /// rejecting it and a new deposit has to use a different subaccount.
    Quarantined {
        /// The signature of the sweep transaction.
        signature: Signature,
    },
}

/// An error from the `deposit_sol` ckSOL minter endpoint.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize, Error)]
pub enum DepositSolError {
    /// Insufficient cycles attached by the caller to complete the `deposit_sol` call.
    #[error(transparent)]
    InsufficientCycles(#[from] InsufficientCyclesError),
    /// The minter experiences temporary issues, try the call again later.
    #[error("Transient error, try the call again later: {0}")]
    TemporarilyUnavailable(String),
    /// There is already a concurrent `deposit_sol` invocation for the same account.
    #[error("There is already a concurrent `deposit_sol` invocation for the same account")]
    AlreadyProcessing,
    /// The balance of the deposit address is below the minimum deposit amount.
    ///
    /// The minimum deposit amount applies to the balance of the deposit address and includes
    /// the rent exemption threshold, so a deposit of exactly the minimum is accepted.
    #[error(
        "Insufficient deposit address balance: expected at least {minimum_deposit_amount} lamports, but got {balance} lamports"
    )]
    ValueTooSmall {
        /// The balance of the deposit address.
        balance: Lamport,
        /// The minimum deposit amount for the deposit to be queued.
        minimum_deposit_amount: Lamport,
    },
    /// The latest deposit of the account is quarantined: its sweep was finalized, but the
    /// outcome did not match the plan the minter submitted, so the deposit could not be
    /// credited safely. The account stays rejected until a minter upgrade resolves the
    /// quarantined deposit; a new deposit has to use a different subaccount.
    #[error("The latest deposit {deposit_id} of this account is quarantined")]
    Quarantined {
        /// The identifier of the quarantined deposit.
        deposit_id: DepositSolId,
    },
}

/// Arguments for a balance-based SPL deposit request.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize, Serialize)]
pub struct DepositSplArgs {
    /// The principal to credit. Defaults to the caller when omitted.
    /// The resolved owner must be a non-anonymous principal.
    pub owner: Option<Principal>,
    /// The subaccount to credit with the deposit.
    pub subaccount: Option<Subaccount>,
    /// The Solana mint address identifying the SPL token to deposit.
    pub mint: Address,
}

/// Identifies a deposit queued by the `deposit_spl` endpoint.
pub type DepositSplId = u64;

/// Arguments for a request to the `get_spl_deposit_address` ckSOL minter endpoint.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize, Serialize)]
pub struct GetSplDepositAddressArgs {
    /// The principal to deposit tokens to. Defaults to the caller when omitted.
    /// The resolved owner must be a non-anonymous principal.
    pub owner: Option<Principal>,
    /// The subaccount to deposit tokens to.
    pub subaccount: Option<Subaccount>,
    /// The Solana mint address identifying the SPL token to deposit.
    pub mint: Address,
}

/// An error from the `get_spl_deposit_address` endpoint.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize, Error)]
pub enum GetSplDepositAddressError {
    /// The requested mint is not registered as a supported SPL token.
    #[error("Unsupported SPL token: {mint}")]
    UnsupportedToken {
        /// The mint that is not registered.
        mint: Address,
    },
}

/// The status of a deposit queued by the `deposit_spl` ckSOL minter endpoint.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize, Serialize)]
pub enum DepositSplStatus {
    /// No deposit with this identifier was queued.
    NotFound,
    /// The token account is queued for a sweep to the minter's treasury token account.
    Queued {
        /// The recorded balance that will be swept, in the mint's smallest units.
        balance: u64,
        /// The fee quoted at acceptance, in the mint's smallest units.
        fee: u64,
    },
    /// A Solana transaction sweeping the token account to the treasury has been submitted
    /// but is not yet finalized.
    Swept {
        /// The signature of the sweep transaction.
        signature: Signature,
    },
    /// The sweep transaction was finalized successfully, so the tokens reached the
    /// treasury, but the ckSPL mint has not landed yet.
    Finalized {
        /// The signature of the sweep transaction.
        signature: Signature,
    },
    /// The minter minted ckSPL for the deposit on its token ledger.
    Minted {
        /// The mint transaction index on the token ledger.
        block_index: LedgerMintIndex,
        /// The minted amount: the swept balance minus the fee, in the mint's smallest units.
        minted_amount: u64,
        /// The outcome of the fee mint into the minter's fee subaccount.
        fee: SplFeeMintStatus,
    },
    /// The sweep transaction failed, or expired without ever being seen on chain, so no
    /// ckSPL is owed. Calling `deposit_spl` again queues a new sweep of whatever balance
    /// the token account holds.
    Dropped {
        /// The signature of the sweep transaction.
        signature: Signature,
    },
    /// Either the sweep was finalized but its outcome did not match the plan, so no ckSPL
    /// was minted, or the mint could not be retried safely, so the mint outcome requires
    /// manual verification. Releasing or crediting the deposit requires a minter upgrade.
    /// Meanwhile the account and mint pair stays in flight, so `deposit_spl` keeps
    /// returning this identifier.
    Quarantined {
        /// The signature of the sweep transaction.
        signature: Signature,
    },
}

/// The outcome of the fee mint of a minted SPL deposit.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize, Serialize)]
pub enum SplFeeMintStatus {
    /// The deposit was quoted a zero fee, so there is no fee mint.
    None,
    /// The fee mint is queued on the token ledger.
    Pending,
    /// The fee was minted into the minter's fee subaccount.
    Minted {
        /// The fee mint transaction index on the token ledger.
        block_index: LedgerMintIndex,
    },
    /// The fee mint cannot be retried safely and requires manual resolution.
    Quarantined,
}

/// Configuration supplied when registering an SPL token.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize, Serialize)]
pub struct AddSplTokenArgs {
    /// The Solana mint address.
    pub mint: Address,
    /// The program owning the mint: classic SPL Token or Token-2022.
    /// Token-2022 mints with extensions are not supported.
    pub token_program: Address,
    /// Token decimals, checked against the finalized mint account.
    pub decimals: u8,
    /// The IC ledger to credit for this token.
    pub ledger_id: Principal,
    /// The minimum deposit in the token's smallest units.
    pub minimum_deposit_amount: u64,
    /// Whether deposits should initially be paused.
    pub paused: bool,
    /// The symbol of the underlying asset on the exchange rate canister, such as `USDC`.
    /// Registration fails when the canister has no SOL rate in that symbol.
    pub xrc_symbol: String,
}

/// An error while registering a supported SPL token.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize, Error)]
pub enum AddSplTokenError {
    /// The mint is already registered. Existing configurations are never overwritten.
    #[error("SPL token is already registered: {mint}")]
    AlreadySupported {
        /// The requested mint address.
        mint: Address,
    },
    /// The ledger is already assigned to SOL or another SPL mint.
    #[error("Ledger is already assigned to a token: {ledger_id}")]
    LedgerAlreadyUsed {
        /// The requested ledger canister ID.
        ledger_id: Principal,
    },
    /// The configuration or finalized mint account is invalid.
    #[error("Invalid SPL token: {0}")]
    InvalidToken(String),
    /// The mint account could not be read consistently. Registration can be retried.
    #[error("Token registration is temporarily unavailable: {0}")]
    TemporarilyUnavailable(String),
}

/// An error from the `deposit_spl` endpoint.
/// Additional variants will be introduced as SPL deposits are implemented.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize, Error)]
pub enum DepositSplError {
    /// A concurrent SPL deposit request is already processing for this account and mint.
    #[error("There is already a concurrent SPL deposit request for this account and mint")]
    AlreadyProcessing,
    /// Insufficient cycles attached to read the token balance.
    #[error(transparent)]
    InsufficientCycles(#[from] InsufficientCyclesError),
    /// The token balance is below the configured minimum plus the deposit fee, in the mint's
    /// smallest units.
    #[error("Balance {balance} is below the minimum deposit amount {minimum_deposit_amount}")]
    ValueTooSmall {
        /// The token balance in the mint's smallest units.
        balance: u64,
        /// The required balance, including the deposit fee, in the mint's smallest units.
        minimum_deposit_amount: u64,
    },
    /// The requested mint is not registered as a supported SPL token.
    #[error("Unsupported SPL token: {mint}")]
    UnsupportedToken {
        /// The requested Solana mint address.
        mint: Address,
    },
    /// Deposits for this supported token are paused.
    #[error("SPL deposits are paused for token: {mint}")]
    TokenPaused {
        /// The requested Solana mint address.
        mint: Address,
    },
    /// The token account at the deposit address is frozen, has an unsupported extension,
    /// or does not match the mint or the owner. A retry fails the same way.
    #[error("Invalid SPL token account: {0}")]
    InvalidTokenAccount(String),
    /// SPL deposit processing is currently unavailable.
    #[error("SPL deposit processing is unavailable: {0}")]
    TemporarilyUnavailable(String),
}

/// Insufficient cycles attached by the caller to complete the call.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize, Error)]
#[error("Insufficient cycles attached, expected {expected} but got {received}")]
pub struct InsufficientCyclesError {
    /// The amount of cycles the call requires.
    pub expected: u128,
    /// The amount of cycles received by the minter (attached by the caller).
    pub received: u128,
}

/// Arguments for a withdrawal request to the ckSOL minter endpoint.
#[derive(Clone, Eq, PartialEq, Debug, Default, CandidType, Deserialize, Serialize)]
pub struct WithdrawSolArgs {
    /// The subaccount to burn ckSOL from.
    pub from_subaccount: Option<Subaccount>,
    /// Amount to withdraw in Lamports.
    pub amount: Lamport,
    /// Address where to send Solana tokens.
    pub address: String,
}

/// The successful result of a withdrawal request.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize)]
pub struct WithdrawSolOk {
    /// The index of the burn block on the ckSOL ledger.
    pub block_index: LedgerBurnIndex,
}

/// Arguments for a request to the `withdraw_sol_status` ckSOL minter endpoint.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize)]
pub struct WithdrawSolStatusArgs {
    /// The burn block index returned by the `withdraw_sol` endpoint.
    pub block_index: LedgerBurnIndex,
}

/// The result of a `get_operating_balance` call.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize)]
pub struct OperatingBalance {
    /// The part of the minter balance that no ckSOL backs and that the minter may spend on
    /// its own costs, in lamports.
    pub balance: Lamport,
    /// The ckSOL ledger account that receives operating top-ups.
    pub top_up_account: Account,
}

/// The successful result of a `process_operating_top_up` call.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize)]
pub struct OperatingTopUp {
    /// The index of the burn block on the ckSOL ledger.
    pub block_index: LedgerBurnIndex,
    /// The burned amount added to the operating balance, in lamports.
    pub amount: Lamport,
}

/// The error result of a `process_operating_top_up` call.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize)]
pub enum ProcessOperatingTopUpError {
    /// Another call is processing the top-up account.
    AlreadyProcessing,
    /// The top-up account holds no ckSOL.
    NoFunds,
    /// The ledger cannot process the request now, retry later.
    TemporarilyUnavailable(String),
}

/// The error result of a withdrawal request.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize)]
pub enum WithdrawSolError {
    /// There is another request for this principal.
    AlreadyProcessing,
    /// The withdrawal amount is too low.
    ValueTooSmall {
        /// The minimum withdrawal amount.
        minimum_withdrawal_amount: Lamport,
        /// The requested withdrawal amount.
        withdrawal_amount: Lamport,
    },
    /// The Solana address is not valid.
    MalformedAddress(String),
    /// The destination address is not a valid withdrawal destination.
    /// The payload contains a human-readable message explaining why the destination was rejected.
    InvalidDestination(String),
    /// The withdrawal account does not hold the requested ckSOL amount.
    InsufficientFunds {
        /// The current balance of the withdrawal account.
        balance: Lamport,
    },
    /// The minter is not approved to transfer the requested amount.
    InsufficientAllowance {
        /// The current allowance for the minter.
        allowance: Lamport,
    },
    /// There are too many concurrent requests, retry later.
    TemporarilyUnavailable(String),
}

/// Status of a finalized transaction.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize)]
pub enum TxFinalizedStatus {
    /// The transaction was successful.
    Success {
        /// The unique identifier (signature) of the Solana transaction.
        transaction_id: Signature,
    },
    /// The transaction failed.
    Failure {
        /// The unique identifier (signature) of the Solana transaction.
        transaction_id: Signature,
    },
}

/// Status of a withdrawal request.
#[derive(Clone, Eq, PartialEq, Debug, CandidType, Deserialize)]
pub enum WithdrawSolStatus {
    /// Withdrawal request is not found.
    NotFound,
    /// Withdrawal request is waiting to be processed.
    Pending,
    /// Solana transaction was signed and is sent to the network.
    TxSent {
        /// The unique identifier (signature) of the Solana transaction.
        transaction_id: Signature,
    },
    /// Solana transaction is finalized.
    TxFinalized(TxFinalizedStatus),
}

/// Information about the ckSOL minter canister.
#[derive(Clone, Debug, Eq, PartialEq, CandidType, Deserialize, Serialize)]
pub struct MinterInfo {
    /// Extra cycles charged per `deposit_sol` call to offset the cost of the sweep.
    pub deposit_sol_fee: u128,
    /// Minimum withdrawal amount in lamports.
    pub minimum_withdrawal_amount: Lamport,
    /// Minimum deposit amount in lamports.
    pub minimum_deposit_amount: Lamport,
    /// Fee deducted from each withdrawal (ckSOL -> SOL).
    pub withdrawal_fee: Lamport,
    /// Minimum cycles the caller must attach when calling `deposit_sol`.
    pub deposit_sol_required_cycles: u128,
    /// The minter's tracked SOL balance in lamports.
    pub balance: Lamport,
    /// The minter's main Solana address, available once its Schnorr public key
    /// has been fetched for the first time, shortly after installation.
    pub minter_address: Option<String>,
    /// The base58 addresses of the durable nonce accounts reserved for withdrawal transactions.
    pub nonce_accounts: Vec<String>,
}
