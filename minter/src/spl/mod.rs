use crate::{
    rpc::{decode_program_account, get_account_info},
    runtime::CanisterRuntime,
    spl_fee::{XrcAssetError, check_xrc_asset},
    state::{
        SupportedSplToken, TokenProgram, audit::process_event, event::EventType, mutate_state,
        read_state,
    },
};
use candid::Principal;
use cksol_types::{AddSplTokenArgs, AddSplTokenError};
use solana_program_pack::Pack;
use spl_token_interface::state::Mint;
use token_2022::{TOKEN_2022_ACCOUNT_TYPE_MINT, split_token_2022_account, token_2022_extensions};

pub mod token_2022;

const MAX_CK_TOKEN_SYMBOL_NUM_BYTES: usize = 20;

#[cfg(test)]
mod tests;

/// Registers a token after checking its finalized Solana mint account.
/// The canister endpoint restricts this operation to the configured ledger suite
/// orchestrator before any RPC call.
pub async fn add_spl_token<R: CanisterRuntime>(
    runtime: &R,
    args: AddSplTokenArgs,
) -> Result<(), AddSplTokenError> {
    let token = SupportedSplToken::try_from(args)?;
    validate_config(&token, runtime.canister_self())?;
    read_state(|state| state.validate_spl_token_registration(&token))?;

    let data = get_finalized_mint_data(runtime, &token).await?;
    validate_mint_data(&data, &token)?;
    check_xrc_asset(runtime, &token.xrc_symbol)
        .await
        .map_err(|error| match error {
            XrcAssetError::UnknownAsset(_) => AddSplTokenError::InvalidToken(error.to_string()),
            XrcAssetError::Unavailable(message) => {
                AddSplTokenError::TemporarilyUnavailable(message)
            }
        })?;

    // Recheck after the external calls: another registration may have claimed the mint
    // or ledger while this request was suspended. No await separates check and write.
    mutate_state(|state| {
        state.validate_spl_token_registration(&token)?;
        process_event(state, EventType::AddedSplToken(token), runtime);
        Ok(())
    })
}

async fn get_finalized_mint_data<R: CanisterRuntime>(
    runtime: &R,
    token: &SupportedSplToken,
) -> Result<Vec<u8>, AddSplTokenError> {
    let account = get_account_info(runtime, token.mint)
        .await
        .map_err(|error| AddSplTokenError::TemporarilyUnavailable(error.to_string()))?
        .ok_or_else(|| AddSplTokenError::InvalidToken("Mint account does not exist".to_string()))?;
    decode_program_account(account, &token.token_program.id())
        .map_err(AddSplTokenError::InvalidToken)
}

fn validate_config(
    token: &SupportedSplToken,
    minter_id: Principal,
) -> Result<(), AddSplTokenError> {
    if token.minimum_deposit_amount == 0 {
        return Err(AddSplTokenError::InvalidToken(
            "Minimum deposit amount must be greater than zero".to_string(),
        ));
    }
    if token.token_symbol.as_ref().is_some_and(|symbol| {
        symbol.trim().is_empty()
            || symbol.len() > MAX_CK_TOKEN_SYMBOL_NUM_BYTES
            || !symbol.is_ascii()
    }) {
        return Err(AddSplTokenError::InvalidToken(
            "The ledger token symbol must be non-empty ASCII and at most 20 bytes".to_string(),
        ));
    }
    if token.xrc_symbol.trim().is_empty() {
        return Err(AddSplTokenError::InvalidToken(
            "The XRC symbol must not be empty".to_string(),
        ));
    }
    if [
        Principal::anonymous(),
        Principal::management_canister(),
        minter_id,
    ]
    .contains(&token.ledger_id)
    {
        return Err(AddSplTokenError::InvalidToken(
            "Invalid ledger canister ID".to_string(),
        ));
    }
    Ok(())
}

fn validate_mint_data(data: &[u8], token: &SupportedSplToken) -> Result<(), AddSplTokenError> {
    let decimals = match token.token_program {
        TokenProgram::Token2022 => {
            let (base, tlv) =
                split_token_2022_account(data, Mint::LEN, TOKEN_2022_ACCOUNT_TYPE_MINT)
                    .map_err(AddSplTokenError::InvalidToken)?;
            let mint = Mint::unpack(base)
                .map_err(|error| AddSplTokenError::InvalidToken(error.to_string()))?;
            if !token_2022_extensions(tlv)
                .map_err(AddSplTokenError::InvalidToken)?
                .is_empty()
            {
                return Err(AddSplTokenError::InvalidToken(
                    "Token-2022 mint extensions are not supported".to_string(),
                ));
            }
            mint.decimals
        }
        TokenProgram::Classic => {
            Mint::unpack(data)
                .map_err(|error| AddSplTokenError::InvalidToken(error.to_string()))?
                .decimals
        }
    };
    if decimals != token.decimals {
        return Err(AddSplTokenError::InvalidToken(format!(
            "Mint decimals are {decimals}, expected {}",
            token.decimals
        )));
    }
    Ok(())
}
