use crate::constants::{CLASSIC_TOKEN_ACCOUNT_RENT_EXEMPTION, TOKEN_ACCOUNT_RENT_EXEMPTION};
use candid::Principal;
use cksol_types::{AddSplTokenArgs, AddSplTokenError};
use minicbor::{Decode, Encode};
use solana_address::{Address, address};

/// The Solana program that owns a mint and its token accounts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Decode, Encode)]
pub enum TokenProgram {
    #[n(0)]
    Classic,
    #[n(1)]
    Token2022,
}

const TOKEN_2022_PROGRAM_ID: Address = address!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

impl TokenProgram {
    pub fn id(self) -> Address {
        match self {
            Self::Classic => spl_token_interface::id().to_bytes().into(),
            Self::Token2022 => TOKEN_2022_PROGRAM_ID,
        }
    }

    /// The rent exemption of an associated token account of this program.
    pub fn account_rent_exemption(self) -> u64 {
        match self {
            Self::Classic => CLASSIC_TOKEN_ACCOUNT_RENT_EXEMPTION,
            Self::Token2022 => TOKEN_ACCOUNT_RENT_EXEMPTION,
        }
    }
}

impl TryFrom<Address> for TokenProgram {
    type Error = AddSplTokenError;

    fn try_from(program: Address) -> Result<Self, Self::Error> {
        [Self::Classic, Self::Token2022]
            .into_iter()
            .find(|candidate| candidate.id() == program)
            .ok_or_else(|| {
                AddSplTokenError::InvalidToken(
                    "Only the classic SPL Token and Token-2022 programs are supported".to_string(),
                )
            })
    }
}

/// Configuration of a supported SPL token.
///
/// Amounts are in the token's smallest units, not lamports.
#[derive(Clone, Debug, PartialEq, Eq, Decode, Encode)]
pub struct SupportedSplToken {
    #[cbor(n(0), with = "crate::state::event::cbor::address")]
    pub mint: Address,
    #[n(1)]
    pub token_program: TokenProgram,
    #[n(2)]
    pub decimals: u8,
    #[cbor(n(3), with = "icrc_cbor::principal")]
    pub ledger_id: Principal,
    #[n(4)]
    pub minimum_deposit_amount: u64,
    #[n(5)]
    pub paused: bool,
    /// The underlying asset symbol used for XRC requests.
    #[n(6)]
    pub xrc_symbol: String,
    /// The ckSPL ledger symbol; absent in legacy deposit-only registrations.
    #[n(7)]
    pub token_symbol: Option<String>,
}

impl TryFrom<AddSplTokenArgs> for SupportedSplToken {
    type Error = AddSplTokenError;

    fn try_from(args: AddSplTokenArgs) -> Result<Self, Self::Error> {
        Ok(Self {
            mint: args.mint.into(),
            token_program: TokenProgram::try_from(Address::from(args.token_program))?,
            decimals: args.decimals,
            ledger_id: args.ledger_id,
            minimum_deposit_amount: args.minimum_deposit_amount,
            paused: args.paused,
            xrc_symbol: args.xrc_symbol,
            token_symbol: args.token_symbol,
        })
    }
}
