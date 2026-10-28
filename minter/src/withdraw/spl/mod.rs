use crate::{
    address::{associated_token_address, minter_address, minter_public_key},
    constants::CKSOL_LEDGER_TRANSACTION_FEE,
    deposit::mint::{MintQueue, mint_pending, process_pending_mints_round},
    guard::{GuardError, withdrawal_guard},
    ledger::{self, BurnError, spl::SplBurnError},
    numeric::LedgerBurnIndex,
    queue_cursor::QueueCursors,
    runtime::CanisterRuntime,
    state::{
        PendingSplWithdrawalFeeReimbursement, State, SupportedSplToken, TokenProgram,
        audit::process_event,
        event::{
            EventType, SplWithdrawalFeeReimbursementRequest, SplWithdrawalRequest,
            SplWithdrawalRequestTerms,
        },
        mutate_state, read_state,
    },
};
use candid::Nat;
use canlog::log;
use cksol_types::{Memo, MintMemo, WithdrawSplOk};
use cksol_types_internal::log::Priority;
use icrc_ledger_types::{
    icrc1::{account::Account, transfer::TransferArg},
    icrc2::transfer_from::TransferFromError,
};
use sol_rpc_types::Lamport;
use solana_address::Address;
use std::{str::FromStr, time::Duration};

#[cfg(test)]
mod tests;

/// Errors returned while preparing or accepting an SPL withdrawal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WithdrawSplError {
    TokenNotSupported {
        mint: Address,
    },
    ValueTooSmall {
        minimum_withdrawal_amount: u64,
        withdrawal_amount: u64,
    },
    AlreadyProcessing,
    MalformedAddress(String),
    InvalidDestination(String),
    TemporarilyUnavailable(String),
    /// Burning the ckSOL withdrawal fee failed.
    CkSolLedgerError {
        error: LedgerError,
    },
    /// Burning ckSPL failed after the ckSOL fee was burned.
    CkSplLedgerError {
        cksol_burn_block_index: LedgerBurnIndex,
        error: LedgerError,
    },
}

/// A withdrawal burn failure on either the ckSOL or ckSPL ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerError {
    InsufficientFunds { balance: Nat },
    InsufficientAllowance { allowance: Nat },
    AmountTooLow { minimum_burn_amount: Nat },
    TemporarilyUnavailable(String),
}

impl From<BurnError> for WithdrawSplError {
    fn from(error: BurnError) -> Self {
        Self::CkSolLedgerError {
            error: error.into(),
        }
    }
}

impl From<BurnError> for LedgerError {
    fn from(error: BurnError) -> Self {
        match error {
            BurnError::TemporarilyUnavailable(message) => Self::TemporarilyUnavailable(message),
            BurnError::InsufficientFunds { balance } => Self::InsufficientFunds {
                balance: balance.into(),
            },
            BurnError::InsufficientAllowance { allowance } => Self::InsufficientAllowance {
                allowance: allowance.into(),
            },
        }
    }
}

impl From<SplBurnError> for LedgerError {
    fn from(error: SplBurnError) -> Self {
        match error {
            SplBurnError::TransferFrom(TransferFromError::InsufficientFunds { balance }) => {
                Self::InsufficientFunds { balance }
            }
            SplBurnError::TransferFrom(TransferFromError::InsufficientAllowance { allowance }) => {
                Self::InsufficientAllowance { allowance }
            }
            SplBurnError::TransferFrom(TransferFromError::BadBurn { min_burn_amount }) => {
                Self::AmountTooLow {
                    minimum_burn_amount: min_burn_amount,
                }
            }
            error => Self::TemporarilyUnavailable(error.to_string()),
        }
    }
}

impl From<super::DestinationError> for WithdrawSplError {
    fn from(error: super::DestinationError) -> Self {
        match error {
            super::DestinationError::InvalidDestination(message) => {
                Self::InvalidDestination(message)
            }
            super::DestinationError::TemporarilyUnavailable(message) => {
                Self::TemporarilyUnavailable(message)
            }
        }
    }
}

impl From<GuardError> for WithdrawSplError {
    fn from(error: GuardError) -> Self {
        match error {
            GuardError::AlreadyProcessing => Self::AlreadyProcessing,
            GuardError::TooManyConcurrentRequests => {
                Self::TemporarilyUnavailable("too many concurrent requests".to_string())
            }
        }
    }
}

/// Selects the token ledger and checks the amount in the token's smallest units.
/// This is only the token and amount check; destination and availability checks
/// must also pass before either ledger burn is attempted.
pub fn validate_token(mint: Address, amount: u64) -> Result<SupportedSplToken, WithdrawSplError> {
    if amount == 0 {
        return Err(WithdrawSplError::ValueTooSmall {
            minimum_withdrawal_amount: 1,
            withdrawal_amount: amount,
        });
    }
    read_state(|state| state.supported_spl_token(&mint).cloned())
        .ok_or(WithdrawSplError::TokenNotSupported { mint })
}

/// Validates a destination wallet locally, before either ledger burn.
/// Checking its Solana account type and token account state happens later.
pub fn validate_destination(
    token: &SupportedSplToken,
    address: &str,
) -> Result<Address, WithdrawSplError> {
    let destination = Address::from_str(address)
        .map_err(|error| WithdrawSplError::MalformedAddress(error.to_string()))?;
    super::validate_destination(&destination)?;

    let master_key = minter_public_key()
        .map_err(|error| WithdrawSplError::TemporarilyUnavailable(error.to_string()))?;
    let treasury = associated_token_address(
        &minter_address(&master_key),
        &token.mint,
        &token.token_program.id(),
    );
    if destination == treasury {
        return Err(WithdrawSplError::InvalidDestination(format!(
            "{destination} is the ckSOL minter's treasury token account for {}",
            token.mint,
        )));
    }
    Ok(destination)
}

/// Quotes the ckSOL fee in lamports using the configured withdrawal fee.
/// Allowing destination ATA creation includes its rent even if it already exists.
pub fn quote_withdrawal_fee(
    withdrawal_fee: Lamport,
    token_program: TokenProgram,
    create_destination_account: bool,
) -> Result<Lamport, WithdrawSplError> {
    let rent = if create_destination_account {
        token_program.account_rent_exemption()
    } else {
        0
    };
    withdrawal_fee.checked_add(rent).ok_or_else(|| {
        WithdrawSplError::TemporarilyUnavailable(
            "SPL withdrawal fee exceeds the maximum lamport amount".to_string(),
        )
    })
}

/// Validates a withdrawal, burns its fee and tokens, then records acceptance.
/// The account guard is held across both ledger calls.
pub async fn withdraw<R: CanisterRuntime>(
    runtime: &R,
    account: Account,
    mint: Address,
    amount: u64,
    address: &str,
    create_destination_account: bool,
) -> Result<WithdrawSplOk, WithdrawSplError> {
    let _guard = withdrawal_guard(account)?;
    let token = validate_token(mint, amount)?;
    let address = validate_destination(&token, address)?;
    let fee = quote_withdrawal_fee(
        read_state(|state| state.withdrawal_fee()),
        token.token_program,
        create_destination_account,
    )?;
    let request = SplWithdrawalRequestTerms {
        account,
        mint: token.mint,
        ledger_id: token.ledger_id,
        address,
        amount,
        fee,
        create_destination_account,
    };
    let cksol_burn_block_index = burn_withdrawal_fee(runtime, &request).await?;
    match burn_withdrawal_amount(runtime, cksol_burn_block_index).await {
        Ok(receipt) => Ok(receipt),
        Err(error) => {
            schedule_withdrawal_fee_reimbursement(
                runtime,
                cksol_burn_block_index,
                &error,
                CKSOL_LEDGER_TRANSACTION_FEE,
            );
            Err(WithdrawSplError::CkSplLedgerError {
                cksol_burn_block_index,
                error: error.into(),
            })
        }
    }
}

/// Burns the ckSOL fee and saves the terms before the token burn is attempted.
/// The caller must validate the terms and hold the account withdrawal guard
/// across both burns.
pub async fn burn_withdrawal_fee<R: CanisterRuntime>(
    runtime: &R,
    request: &SplWithdrawalRequestTerms,
) -> Result<LedgerBurnIndex, BurnError> {
    let burn_index = ledger::burn(
        runtime,
        runtime.canister_self().into(),
        request.account,
        request.fee,
        Memo::spl_withdrawal_fee(request.mint, request.amount, request.address),
    )
    .await?;
    let cksol_burn_block_index = LedgerBurnIndex::new(burn_index);
    mutate_state(|state| {
        process_event(
            state,
            EventType::BurnedCkSolWithdrawalFee {
                cksol_burn_block_index,
                request: request.clone(),
            },
            runtime,
        );
    });
    Ok(cksol_burn_block_index)
}

/// Burns the token amount from its fee checkpoint, then accepts the withdrawal.
/// The caller holds the account withdrawal guard across both burns, and an error
/// leaves the checkpoint intact.
pub async fn burn_withdrawal_amount<R: CanisterRuntime>(
    runtime: &R,
    cksol_burn_block_index: LedgerBurnIndex,
) -> Result<WithdrawSplOk, SplBurnError> {
    let fee_burn = read_state(|state| {
        state
            .fee_burned_spl_withdrawal_request(&cksol_burn_block_index)
            .clone()
    });
    let request = fee_burn.request;
    let ckspl_burn_block_index = ledger::spl::burn(
        runtime,
        request.ledger_id,
        request.account,
        request.amount,
        Memo::spl_convert(*cksol_burn_block_index.get(), request.address),
        // Keep all ledger arguments stable if the token burn is retried.
        fee_burn.created_at,
    )
    .await?;
    let accepted = SplWithdrawalRequest {
        terms: request,
        cksol_burn_block_index,
        ckspl_burn_block_index: LedgerBurnIndex::new(ckspl_burn_block_index),
    };
    mutate_state(|state| {
        process_event(
            state,
            EventType::AcceptedSplWithdrawalRequest(accepted.clone()),
            runtime,
        );
    });
    log!(
        Priority::Info,
        "Accepted SPL withdrawal request from {:?}: burned {} units of mint {}, queued withdrawal to {} (ckSOL burn block index {cksol_burn_block_index}, ckSPL burn block index {ckspl_burn_block_index})",
        accepted.terms.account,
        accepted.terms.amount,
        accepted.terms.mint,
        accepted.terms.address,
    );
    Ok(WithdrawSplOk {
        cksol_burn_block_index: *cksol_burn_block_index.get(),
        ckspl_burn_block_index,
    })
}

/// Queues a positive ckSOL refund after a definitive token-burn failure.
/// The caller holds the withdrawal guard and first reconciles any earlier attempt
/// whose outcome is unknown.
pub fn schedule_withdrawal_fee_reimbursement<R: CanisterRuntime>(
    runtime: &R,
    cksol_burn_block_index: LedgerBurnIndex,
    error: &SplBurnError,
    cksol_ledger_fee: Lamport,
) {
    let request = read_state(|state| {
        state
            .fee_burned_spl_withdrawal_request(&cksol_burn_block_index)
            .request
            .clone()
    });
    let reimbursed_amount = match error {
        SplBurnError::TransferFrom(TransferFromError::TemporarilyUnavailable) => request.fee,
        SplBurnError::TransferFrom(
            TransferFromError::InsufficientFunds { .. }
            | TransferFromError::InsufficientAllowance { .. }
            | TransferFromError::BadBurn { .. },
        ) => request.fee.saturating_sub(cksol_ledger_fee),
        SplBurnError::Call(_)
        | SplBurnError::TransferFrom(
            TransferFromError::BadFee { .. }
            | TransferFromError::TooOld
            | TransferFromError::CreatedInFuture { .. }
            | TransferFromError::Duplicate { .. }
            | TransferFromError::GenericError { .. },
        ) => return,
    };
    if reimbursed_amount == 0 {
        return;
    }
    mutate_state(|state| {
        process_event(
            state,
            EventType::FailedSplWithdrawalRequest(SplWithdrawalFeeReimbursementRequest {
                cksol_burn_block_index,
                reimbursed_amount,
                to: request.account,
            }),
            runtime,
        );
    });
    log!(
        Priority::Info,
        "Scheduled reimbursement of {reimbursed_amount} lamports of ckSOL to {:?} for SPL withdrawal {cksol_burn_block_index} after its token burn failed: {error}",
        request.account,
    );
}

/// Builds the fixed mint arguments of a fee reimbursement, so a retry is deduplicated.
pub fn fee_reimbursement_transfer_arg(
    pending: &PendingSplWithdrawalFeeReimbursement,
) -> TransferArg {
    TransferArg {
        from_subaccount: None,
        to: pending.request.to,
        fee: None,
        created_at_time: Some(pending.created_at),
        memo: Some(
            Memo::from(MintMemo::spl_withdrawal_fee_reimbursement(
                *pending.request.cksol_burn_block_index.get(),
            ))
            .into(),
        ),
        amount: pending.request.reimbursed_amount.into(),
    }
}

thread_local! {
    /// Where the previous reimbursement round stopped. The cursor resets on upgrade;
    /// the mint arguments live in the event log.
    static FEE_REIMBURSEMENT_CURSOR: QueueCursors<MintQueue, LedgerBurnIndex> =
        const { QueueCursors::new() };
}

pub fn reset_fee_reimbursement_cursor() {
    FEE_REIMBURSEMENT_CURSOR.with(QueueCursors::reset);
}

/// Mints the queued fee reimbursements in bounded rounds, like the deposit mints.
pub async fn process_pending_fee_reimbursements<R: CanisterRuntime>(runtime: R) {
    let run_again = process_pending_mints_round(
        &runtime,
        MintQueue::SplFeeReimbursement,
        &FEE_REIMBURSEMENT_CURSOR,
        |state: &State| state.pending_spl_withdrawal_fee_reimbursements(),
        process_pending_fee_reimbursement,
    )
    .await;
    if run_again {
        runtime.set_timer(Duration::ZERO, process_pending_fee_reimbursements);
    }
}

async fn process_pending_fee_reimbursement<R: CanisterRuntime>(
    runtime: &R,
    cksol_burn_block_index: LedgerBurnIndex,
    pending: PendingSplWithdrawalFeeReimbursement,
) {
    let client = read_state(|state| state.ledger_client(runtime.inter_canister_call_runtime()));
    mint_pending(
        runtime,
        MintQueue::SplFeeReimbursement,
        client,
        fee_reimbursement_transfer_arg(&pending),
        |mint_block_index| EventType::ReimbursedSplWithdrawalFee {
            cksol_burn_block_index,
            mint_block_index,
        },
        EventType::QuarantinedSplWithdrawalFeeReimbursement {
            cksol_burn_block_index,
        },
        &format!("fee reimbursement of SPL withdrawal {cksol_burn_block_index}"),
    )
    .await;
}
