use crate::{
    constants::{
        GET_ACCOUNT_INFO_CYCLES, GET_BALANCE_CYCLES, GET_RECENT_BLOCK_MAX_TRIES,
        GET_SIGNATURE_STATUSES_CYCLES, GET_TRANSACTION_CYCLES,
    },
    runtime::CanisterRuntime,
    spl::token_2022::{
        TOKEN_2022_ACCOUNT_TYPE_ACCOUNT, TOKEN_2022_EXTENSION_IMMUTABLE_OWNER,
        TOKEN_2022_EXTENSION_MEMO_TRANSFER, split_token_2022_account, token_2022_extensions,
    },
    state::{TokenProgram, read_state},
};
use cksol_types::{DepositSolError, DepositSplError};
use derive_more::From;
use ic_canister_runtime::IcError;
use minicbor::{Decode, Encode};
use sol_rpc_types::{
    CommitmentLevel, GetAccountInfoEncoding, GetAccountInfoParams, GetTransactionEncoding, Lamport,
    MultiRpcResult, RpcError, Slot,
};
use solana_account_decoder_client_types::UiAccount;
use solana_address::Address;
use solana_hash::Hash;
use solana_nonce::{state::State as NonceState, versions::Versions as NonceVersions};
use solana_program_pack::Pack;
use solana_sdk_ids::system_program;
use solana_signature::Signature;
use solana_transaction::{Transaction, versioned::VersionedTransaction};
use solana_transaction_status_client_types::{
    EncodedConfirmedTransactionWithStatusMeta, UiTransactionStatusMeta,
};
use spl_token_interface::state::Account as TokenAccount;
use thiserror::Error;

#[cfg(test)]
mod tests;

/// A `getTransaction` response attributed to the queried signature: the transaction
/// decoded by [`get_transaction`], whose first signature is the queried one and signs
/// the message.
#[derive(Debug, PartialEq)]
pub struct FetchedTransaction {
    pub transaction: VersionedTransaction,
    pub meta: Option<UiTransactionStatusMeta>,
}

pub async fn get_transaction<R: CanisterRuntime>(
    runtime: &R,
    signature: Signature,
) -> Result<Option<FetchedTransaction>, GetTransactionError> {
    let result = read_state(|state| state.sol_rpc_client(runtime.inter_canister_call_runtime()))
        .get_transaction(signature)
        .with_encoding(GetTransactionEncoding::Base64)
        .with_commitment(CommitmentLevel::Finalized)
        .with_max_supported_transaction_version(0)
        .with_cycles(GET_TRANSACTION_CYCLES)
        .try_send()
        .await;
    match result? {
        MultiRpcResult::Consistent(Ok(Some(outcome))) => {
            let transaction = ensure_signed_with(&outcome, signature)?;
            Ok(Some(FetchedTransaction {
                transaction,
                meta: outcome.transaction.meta,
            }))
        }
        MultiRpcResult::Consistent(Ok(None)) => Ok(None),
        MultiRpcResult::Consistent(Err(e)) => Err(GetTransactionError::RpcError(e)),
        MultiRpcResult::Inconsistent(_) => Err(GetTransactionError::InconsistentRpcResults),
    }
}

fn ensure_signed_with(
    outcome: &EncodedConfirmedTransactionWithStatusMeta,
    queried: Signature,
) -> Result<VersionedTransaction, GetTransactionError> {
    let decoded = outcome
        .transaction
        .transaction
        .decode()
        .ok_or(GetTransactionError::UndecodableTransaction { queried })?;
    match decoded.signatures.first() {
        Some(first) if *first == queried => {}
        returned => {
            return Err(GetTransactionError::SignatureMismatch {
                queried,
                returned: returned.copied().map(Box::new),
            });
        }
    }
    if decoded.verify_with_results().first() != Some(&true) {
        return Err(GetTransactionError::InvalidSignature { queried });
    }
    Ok(decoded)
}

#[derive(Debug, PartialEq, Error, From)]
pub enum GetTransactionError {
    #[error("Error while calling SOL RPC canister: {0}")]
    IcError(IcError),
    #[error("RPC error while fetching transaction: {0}")]
    RpcError(RpcError),
    #[error("Inconsistent RPC results for transaction")]
    InconsistentRpcResults,
    #[error("Transaction returned for {queried} cannot be decoded")]
    #[from(ignore)]
    UndecodableTransaction { queried: Signature },
    #[error("Transaction returned for {queried} has first signature {returned:?}")]
    SignatureMismatch {
        queried: Signature,
        returned: Option<Box<Signature>>,
    },
    #[error("Transaction returned for {queried} is not signed by its fee payer")]
    #[from(ignore)]
    InvalidSignature { queried: Signature },
}

impl GetTransactionError {
    /// A consistent response that cannot be attributed to the queried signature points
    /// to a misbehaving provider or SOL RPC canister, not to a transient error.
    pub fn is_response_untrustworthy(&self) -> bool {
        match self {
            GetTransactionError::UndecodableTransaction { .. }
            | GetTransactionError::SignatureMismatch { .. }
            | GetTransactionError::InvalidSignature { .. } => true,
            GetTransactionError::IcError(_)
            | GetTransactionError::RpcError(_)
            | GetTransactionError::InconsistentRpcResults => false,
        }
    }
}

#[derive(Debug, PartialEq, Error, From)]
pub enum GetBalanceError {
    #[error("Error while calling SOL RPC canister: {0}")]
    IcError(IcError),
    #[error("RPC error while fetching balance: {0}")]
    RpcError(RpcError),
    #[error("Inconsistent RPC results for balance")]
    InconsistentRpcResults,
}

impl From<GetBalanceError> for DepositSolError {
    fn from(error: GetBalanceError) -> Self {
        DepositSolError::TemporarilyUnavailable(error.to_string())
    }
}

#[derive(Debug, PartialEq, Error, From)]
pub enum GetSplTokenBalanceError {
    #[error("Error while calling SOL RPC canister: {0}")]
    IcError(IcError),
    #[error("RPC error while fetching SPL token balance: {0}")]
    RpcError(RpcError),
    #[error("Inconsistent RPC results for SPL token balance")]
    InconsistentRpcResults,
    #[error("Invalid SPL token account: {0}")]
    InvalidTokenAccount(String),
}

impl From<GetAccountInfoError> for GetSplTokenBalanceError {
    fn from(error: GetAccountInfoError) -> Self {
        match error {
            GetAccountInfoError::IcError(error) => Self::IcError(error),
            GetAccountInfoError::RpcError(error) => Self::RpcError(error),
            GetAccountInfoError::InconsistentRpcResults => Self::InconsistentRpcResults,
        }
    }
}

#[derive(Debug, PartialEq, Error, From)]
pub enum GetAccountInfoError {
    #[error("Error while calling SOL RPC canister: {0}")]
    IcError(IcError),
    #[error("RPC error while fetching account info: {0}")]
    RpcError(RpcError),
    #[error("Inconsistent RPC results for account info")]
    InconsistentRpcResults,
}

impl From<GetSplTokenBalanceError> for DepositSplError {
    fn from(error: GetSplTokenBalanceError) -> Self {
        match error {
            // The account itself is the problem, so a retry fails the same way.
            GetSplTokenBalanceError::InvalidTokenAccount(reason) => {
                DepositSplError::InvalidTokenAccount(reason)
            }
            GetSplTokenBalanceError::IcError(_)
            | GetSplTokenBalanceError::RpcError(_)
            | GetSplTokenBalanceError::InconsistentRpcResults => {
                DepositSplError::TemporarilyUnavailable(error.to_string())
            }
        }
    }
}

pub async fn get_balance<R: CanisterRuntime>(
    runtime: &R,
    address: Address,
) -> Result<Lamport, GetBalanceError> {
    let result = read_state(|state| state.sol_rpc_client(runtime.inter_canister_call_runtime()))
        .get_balance(address)
        .with_commitment(CommitmentLevel::Finalized)
        .with_cycles(GET_BALANCE_CYCLES)
        .try_send()
        .await;
    match result? {
        MultiRpcResult::Consistent(Ok(balance)) => Ok(balance),
        MultiRpcResult::Consistent(Err(e)) => Err(GetBalanceError::RpcError(e)),
        MultiRpcResult::Inconsistent(_) => Err(GetBalanceError::InconsistentRpcResults),
    }
}

/// Reads the finalized amount of a token account in the mint's smallest units, where a
/// missing account holds zero. The account must belong to the expected program, mint and
/// owner, and must not be frozen.
pub async fn get_spl_token_balance<R: CanisterRuntime>(
    runtime: &R,
    address: Address,
    owner: Address,
    mint: Address,
    token_program: TokenProgram,
) -> Result<u64, GetSplTokenBalanceError> {
    let Some(account) = get_spl_token_account(runtime, address, token_program).await? else {
        return Ok(0);
    };
    account.validate(owner, mint)?;
    Ok(account.amount)
}

/// Reads the finalized state of a token account of the given program. A missing account is `None`.
pub async fn get_spl_token_account<R: CanisterRuntime>(
    runtime: &R,
    address: Address,
    token_program: TokenProgram,
) -> Result<Option<SplTokenAccount>, GetSplTokenBalanceError> {
    let Some(account) = get_account_info(runtime, address).await? else {
        return Ok(None);
    };
    // A prefunded, unallocated ATA can still be created by the sweep.
    if !account.executable
        && account.owner == solana_system_interface::program::id().to_string()
        && account.data.decode().is_some_and(|data| data.is_empty())
    {
        return Ok(None);
    }
    let data = decode_program_account(account, &token_program.id())
        .map_err(GetSplTokenBalanceError::InvalidTokenAccount)?;
    let account = match token_program {
        TokenProgram::Token2022 => parse_token_2022_account(&data)?,
        TokenProgram::Classic => TokenAccount::unpack(&data)
            .map_err(|error| GetSplTokenBalanceError::InvalidTokenAccount(error.to_string()))?,
    };
    Ok(Some(account.into()))
}

/// The fields of a token account that the minter checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SplTokenAccount {
    pub owner: Address,
    pub mint: Address,
    /// The amount in the mint's smallest units.
    pub amount: u64,
    pub frozen: bool,
}

impl From<TokenAccount> for SplTokenAccount {
    fn from(account: TokenAccount) -> Self {
        Self {
            owner: account.owner.to_bytes().into(),
            mint: account.mint.to_bytes().into(),
            amount: account.amount,
            frozen: account.is_frozen(),
        }
    }
}

impl SplTokenAccount {
    /// Checks the expected owner and mint and rejects frozen accounts.
    pub fn validate(&self, owner: Address, mint: Address) -> Result<(), GetSplTokenBalanceError> {
        if self.owner != owner || self.mint != mint {
            return Err(GetSplTokenBalanceError::InvalidTokenAccount(
                "Token account mint or owner does not match the expected deposit".to_string(),
            ));
        }
        if self.frozen {
            return Err(GetSplTokenBalanceError::InvalidTokenAccount(
                "Token account is frozen".to_string(),
            ));
        }
        Ok(())
    }
}

fn parse_token_2022_account(data: &[u8]) -> Result<TokenAccount, GetSplTokenBalanceError> {
    let (base, tlv) =
        split_token_2022_account(data, TokenAccount::LEN, TOKEN_2022_ACCOUNT_TYPE_ACCOUNT)
            .map_err(GetSplTokenBalanceError::InvalidTokenAccount)?;
    let account = TokenAccount::unpack(base)
        .map_err(|error| GetSplTokenBalanceError::InvalidTokenAccount(error.to_string()))?;
    for (extension, value) in
        token_2022_extensions(tlv).map_err(GetSplTokenBalanceError::InvalidTokenAccount)?
    {
        let value_len = match extension {
            TOKEN_2022_EXTENSION_IMMUTABLE_OWNER => 0,
            TOKEN_2022_EXTENSION_MEMO_TRANSFER => 1,
            _ => {
                return Err(GetSplTokenBalanceError::InvalidTokenAccount(
                    "Unsupported Token-2022 token account extension".to_string(),
                ));
            }
        };
        if value.len() != value_len {
            return Err(GetSplTokenBalanceError::InvalidTokenAccount(
                "Invalid Token-2022 extension length".to_string(),
            ));
        }
    }
    Ok(account)
}

/// Reads the finalized state of an account. A missing account is `None`.
pub async fn get_account_info<R: CanisterRuntime>(
    runtime: &R,
    address: Address,
) -> Result<Option<UiAccount>, GetAccountInfoError> {
    let result = read_state(|state| state.sol_rpc_client(runtime.inter_canister_call_runtime()))
        .get_account_info(GetAccountInfoParams::from_pubkey(address))
        .with_encoding(GetAccountInfoEncoding::Base64)
        .with_commitment(CommitmentLevel::Finalized)
        .with_response_size_estimate(2_048)
        .with_cycles(GET_ACCOUNT_INFO_CYCLES)
        .try_send()
        .await;
    match result? {
        MultiRpcResult::Consistent(Ok(account)) => Ok(account),
        MultiRpcResult::Consistent(Err(error)) => Err(GetAccountInfoError::RpcError(error)),
        MultiRpcResult::Inconsistent(_) => Err(GetAccountInfoError::InconsistentRpcResults),
    }
}

/// Returns the data of a non-executable account owned by the given program.
/// The error carries the reason the account is rejected.
pub fn decode_program_account(account: UiAccount, program: &Address) -> Result<Vec<u8>, String> {
    if account.executable || account.owner != program.to_string() {
        return Err("Account must be non-executable and owned by the expected program".to_string());
    }
    account
        .data
        .decode()
        .ok_or_else(|| "Cannot decode account data".to_string())
}

pub async fn submit_transaction<R: CanisterRuntime>(
    runtime: &R,
    transaction: Transaction,
    preflight_commitment: CommitmentLevel,
) -> Result<Signature, SubmitTransactionError> {
    send_transaction(
        runtime,
        transaction,
        Preflight::Simulate(preflight_commitment),
    )
    .await
}

/// Submits a withdrawal transaction without the providers' preflight
/// simulation, so that a doomed transaction lands and fails on-chain,
/// advancing its nonce and freeing its nonce account, instead of staying
/// unbroadcast and occupying the account until an operator intervenes.
pub async fn submit_transaction_skipping_preflight<R: CanisterRuntime>(
    runtime: &R,
    transaction: Transaction,
) -> Result<Signature, SubmitTransactionError> {
    send_transaction(runtime, transaction, Preflight::Skip).await
}

enum Preflight {
    Simulate(CommitmentLevel),
    Skip,
}

async fn send_transaction<R: CanisterRuntime>(
    runtime: &R,
    transaction: Transaction,
    preflight: Preflight,
) -> Result<Signature, SubmitTransactionError> {
    let client = read_state(|state| state.sol_rpc_client(runtime.inter_canister_call_runtime()));
    let request = match preflight {
        Preflight::Simulate(commitment) => client
            .send_transaction(transaction)
            .with_preflight_commitment(commitment),
        Preflight::Skip => client
            .send_transaction(transaction)
            .with_skip_preflight(true),
    };
    match request.try_send().await {
        Ok(MultiRpcResult::Consistent(Ok(signature))) => Ok(signature),
        Ok(MultiRpcResult::Consistent(Err(e))) => Err(SubmitTransactionError::RpcError(e)),
        Ok(MultiRpcResult::Inconsistent(_)) => Err(SubmitTransactionError::InconsistentRpcResults),
        Err(e) => Err(SubmitTransactionError::IcError(e)),
    }
}

#[derive(Debug, PartialEq, Error, From)]
pub enum SubmitTransactionError {
    #[error("Error while calling SOL RPC canister: {0}")]
    IcError(IcError),
    #[error("RPC error while sending transaction: {0}")]
    RpcError(RpcError),
    #[error("Inconsistent RPC results for sendTransaction")]
    InconsistentRpcResults,
}

pub async fn get_nonce_account<R: CanisterRuntime>(
    runtime: &R,
    address: Address,
) -> Result<NonceAccount, GetNonceAccountError> {
    let result = read_state(|state| state.sol_rpc_client(runtime.inter_canister_call_runtime()))
        .get_account_info(address)
        .with_encoding(GetAccountInfoEncoding::Base64)
        .with_commitment(CommitmentLevel::Finalized)
        .with_cycles(GET_ACCOUNT_INFO_CYCLES)
        .try_send()
        .await;
    match result? {
        MultiRpcResult::Consistent(Ok(Some(account))) => NonceAccount::try_from(account),
        MultiRpcResult::Consistent(Ok(None)) => Err(GetNonceAccountError::AccountNotFound),
        MultiRpcResult::Consistent(Err(e)) => Err(GetNonceAccountError::RpcError(e)),
        MultiRpcResult::Inconsistent(_) => Err(GetNonceAccountError::InconsistentRpcResults),
    }
}

/// The on-chain state of an initialized durable nonce account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NonceAccount {
    pub authority: Address,
    pub nonce: Hash,
}

impl TryFrom<UiAccount> for NonceAccount {
    type Error = GetNonceAccountError;

    fn try_from(account: UiAccount) -> Result<Self, Self::Error> {
        if account.owner != system_program::ID.to_string() || account.executable {
            return Err(GetNonceAccountError::UnexpectedAccountMetadata {
                owner: account.owner,
                executable: account.executable,
            });
        }
        let data = account.data.decode().ok_or_else(|| {
            GetNonceAccountError::NotAnInitializedNonceAccount(
                "undecodable account data".to_string(),
            )
        })?;
        let versions: NonceVersions = bincode::deserialize(&data)
            .map_err(|e| GetNonceAccountError::NotAnInitializedNonceAccount(e.to_string()))?;
        let state = match versions {
            NonceVersions::Legacy(_) => return Err(GetNonceAccountError::LegacyNonceAccount),
            NonceVersions::Current(state) => state,
        };
        match *state {
            NonceState::Uninitialized => Err(GetNonceAccountError::NotAnInitializedNonceAccount(
                "the nonce account is uninitialized".to_string(),
            )),
            NonceState::Initialized(data) => Ok(Self {
                authority: data.authority,
                nonce: data.blockhash(),
            }),
        }
    }
}

#[derive(Debug, PartialEq, Error)]
pub enum GetNonceAccountError {
    #[error("Error while calling SOL RPC canister: {0}")]
    IcError(#[from] IcError),
    #[error("RPC error while fetching nonce account: {0}")]
    RpcError(RpcError),
    #[error("Inconsistent RPC results for getAccountInfo")]
    InconsistentRpcResults,
    #[error("Nonce account not found")]
    AccountNotFound,
    #[error(
        "Expected a non-executable account owned by the system program, got owner {owner} (executable: {executable})"
    )]
    UnexpectedAccountMetadata { owner: String, executable: bool },
    #[error("Not an initialized nonce account: {0}")]
    NotAnInitializedNonceAccount(String),
    #[error("Legacy nonce account, which cannot back a durable transaction")]
    LegacyNonceAccount,
}

pub async fn get_recent_block<R: CanisterRuntime>(
    runtime: &R,
    commitment: CommitmentLevel,
) -> Result<Block, GetRecentBlockError> {
    let client =
        read_state(|state| state.sol_rpc_client_builder(runtime.inter_canister_call_runtime()))
            .with_default_commitment_level(commitment)
            .build();
    match client
        .get_recent_block()
        .with_num_tries(GET_RECENT_BLOCK_MAX_TRIES)
        .try_send()
        .await
    {
        Ok((slot, block)) => {
            let blockhash: Hash =
                block
                    .blockhash
                    .parse()
                    .map_err(|e: solana_hash::ParseHashError| {
                        GetRecentBlockError::Failed(vec![e.to_string()])
                    })?;
            let block_height = block
                .block_height
                .map(BlockHeight::from)
                .ok_or(GetRecentBlockError::MissingBlockHeight { slot })?;
            Ok(Block {
                slot,
                blockhash,
                block_height,
            })
        }
        Err(errors) => Err(GetRecentBlockError::Failed(
            errors.into_iter().map(|e| e.to_string()).collect(),
        )),
    }
}

/// A block whose blockhash a new transaction can use.
///
/// The blockhash stays valid for 150 blocks after `block_height`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    pub slot: Slot,
    pub blockhash: Hash,
    pub block_height: BlockHeight,
}

/// The height of a block in the Solana ledger, i.e. the number of blocks
/// beneath it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Decode, Encode, From)]
#[cbor(transparent)]
pub struct BlockHeight(#[n(0)] u64);

impl BlockHeight {
    pub const fn new(height: u64) -> Self {
        Self(height)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

#[derive(Debug, PartialEq, Error)]
pub enum GetRecentBlockError {
    #[error("Failed to get recent block: {0:?}")]
    Failed(Vec<String>),
    #[error("Block at slot {slot} has no block height")]
    MissingBlockHeight { slot: Slot },
}

pub async fn get_signature_statuses<R: CanisterRuntime>(
    runtime: &R,
    signatures: &[Signature],
) -> Result<
    Vec<Option<solana_transaction_status_client_types::TransactionStatus>>,
    GetSignatureStatusesError,
> {
    let client = read_state(|state| state.sol_rpc_client(runtime.inter_canister_call_runtime()));
    let result = client
        .get_signature_statuses(signatures)
        .map_err(GetSignatureStatusesError::RpcError)?
        .with_search_transaction_history(true)
        .with_cycles(GET_SIGNATURE_STATUSES_CYCLES)
        .try_send()
        .await;
    match result? {
        MultiRpcResult::Consistent(Ok(statuses)) => Ok(statuses),
        MultiRpcResult::Consistent(Err(e)) => Err(GetSignatureStatusesError::RpcError(e)),
        MultiRpcResult::Inconsistent(_) => Err(GetSignatureStatusesError::InconsistentRpcResults),
    }
}

#[derive(Debug, PartialEq, Error)]
pub enum GetSignatureStatusesError {
    #[error("Error while calling SOL RPC canister: {0}")]
    IcError(#[from] IcError),
    #[error("RPC error while fetching signature statuses: {0}")]
    RpcError(RpcError),
    #[error("Inconsistent RPC results for getSignatureStatuses")]
    InconsistentRpcResults,
}
