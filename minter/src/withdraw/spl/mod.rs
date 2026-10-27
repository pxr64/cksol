use crate::{
    address::{associated_token_address, minter_address, minter_public_key},
    guard::GuardError,
    state::{SupportedSplToken, read_state},
};
use solana_address::Address;
use std::str::FromStr;

#[cfg(test)]
mod tests;

/// Errors checked locally before attempting an SPL withdrawal burn.
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
