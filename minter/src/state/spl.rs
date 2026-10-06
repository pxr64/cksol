use candid::Principal;
use cksol_types::Address;
use minicbor::{Decode, Encode};

/// Configuration of a supported SPL token.
///
/// Amounts are in the token's smallest units, not lamports.
#[derive(Clone, Debug, PartialEq, Eq, Decode, Encode)]
pub struct SupportedSplToken {
    #[cbor(n(0), with = "crate::state::event::cbor::rpc_address")]
    pub mint: Address,
    #[cbor(n(1), with = "crate::state::event::cbor::rpc_address")]
    pub token_program: Address,
    #[n(2)]
    pub decimals: u8,
    #[cbor(n(3), with = "icrc_cbor::principal")]
    pub ledger_id: Principal,
    #[n(4)]
    pub minimum_deposit_amount: u64,
    #[n(5)]
    pub paused: bool,
}

impl From<cksol_types::AddSplTokenArgs> for SupportedSplToken {
    fn from(args: cksol_types::AddSplTokenArgs) -> Self {
        Self {
            mint: args.mint,
            token_program: args.token_program,
            decimals: args.decimals,
            ledger_id: args.ledger_id,
            minimum_deposit_amount: args.minimum_deposit_amount,
            paused: args.paused,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        deposit::spl::deposit_spl,
        state::{mutate_state, read_state},
        storage::total_event_count,
        test_fixtures::{
            deposit::DEPOSITOR_ACCOUNT, init_state, ledger_canister_id,
            runtime::TestCanisterRuntime,
        },
    };
    use cksol_types::DepositSplError;
    use std::str::FromStr;

    fn token(paused: bool) -> SupportedSplToken {
        SupportedSplToken {
            mint: Address::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").unwrap(),
            token_program: Address::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
                .unwrap(),
            decimals: 6,
            ledger_id: ledger_canister_id(),
            minimum_deposit_amount: 1_000_000,
            paused,
        }
    }

    #[tokio::test]
    async fn should_reject_unknown_mints_without_side_effects() {
        init_state();
        let runtime = TestCanisterRuntime::new();
        for mint in [token(false).mint, Address::default()] {
            assert_eq!(
                deposit_spl(&runtime, DEPOSITOR_ACCOUNT, mint.clone()).await,
                Err(DepositSplError::UnsupportedToken { mint })
            );
        }
        assert!(runtime.sent_update_calls().is_empty());
        assert!(runtime.msg_cycles_accepted().is_empty());
        assert_eq!(total_event_count(), 0);
    }

    #[tokio::test]
    async fn should_reject_paused_tokens_without_side_effects() {
        init_state();
        let token = token(true);
        let mint = token.mint.clone();
        // Test-only setup: production registration must go through the event log.
        mutate_state(|state| state.supported_spl_tokens.insert(token.mint.clone(), token));
        let runtime = TestCanisterRuntime::new();
        assert_eq!(
            deposit_spl(&runtime, DEPOSITOR_ACCOUNT, mint.clone()).await,
            Err(DepositSplError::TokenPaused { mint })
        );
        assert!(runtime.sent_update_calls().is_empty());
        assert!(runtime.msg_cycles_accepted().is_empty());
        assert_eq!(total_event_count(), 0);
    }

    #[tokio::test]
    async fn should_find_supported_token_but_require_cycles_before_reading_balance() {
        init_state();
        let token = token(false);
        mutate_state(|state| {
            state
                .supported_spl_tokens
                .insert(token.mint.clone(), token.clone())
        });
        assert_eq!(
            read_state(|state| state.supported_spl_token(&token.mint).cloned()),
            Some(token.clone())
        );
        let runtime = TestCanisterRuntime::new().add_msg_cycles_available(0);
        assert_eq!(
            deposit_spl(&runtime, DEPOSITOR_ACCOUNT, token.mint.clone()).await,
            Err(DepositSplError::InsufficientCycles(
                cksol_types::InsufficientCyclesError {
                    expected: crate::constants::GET_SPL_TOKEN_BALANCE_CYCLES,
                    received: 0,
                }
            ))
        );
        assert!(runtime.sent_update_calls().is_empty());
        assert!(runtime.msg_cycles_accepted().is_empty());
        assert_eq!(total_event_count(), 0);
    }
}
