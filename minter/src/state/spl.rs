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
