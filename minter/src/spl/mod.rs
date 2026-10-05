use crate::{
    runtime::CanisterRuntime,
    state::{SupportedSplToken, audit::process_event, event::EventType, mutate_state, read_state},
};
use candid::Principal;
use cksol_types::{AddSplTokenArgs, AddSplTokenError};
use sol_rpc_types::{
    CommitmentLevel, GetAccountInfoEncoding, GetAccountInfoParams, MultiRpcResult,
};

use solana_program_pack::Pack;
use spl_token_2022_interface::{
    extension::{BaseStateWithExtensions, StateWithExtensions},
    state::Mint as Mint2022,
};
use spl_token_interface::state::Mint;

#[cfg(test)]
mod tests;

const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const CLASSIC_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Registers a token after checking its finalized Solana mint account.
/// The canister endpoint restricts this operation to the configured ledger suite
/// orchestrator before any RPC call.
pub async fn add_spl_token<R: CanisterRuntime>(
    runtime: &R,
    args: AddSplTokenArgs,
) -> Result<(), AddSplTokenError> {
    let token = SupportedSplToken::from(args);
    validate_config(&token, runtime.canister_self())?;
    read_state(|state| state.validate_spl_token_registration(&token))?;

    let data = get_finalized_mint_data(runtime, &token).await?;
    validate_mint_data(&data, &token)?;

    // Recheck after the RPC await: another registration may have claimed the mint
    // or ledger while this request was suspended. No await separates check and write.
    mutate_state(|state| {
        process_event(state, EventType::AddedSplToken(token), runtime);
        Ok(())
    })
}

async fn get_finalized_mint_data<R: CanisterRuntime>(
    runtime: &R,
    token: &SupportedSplToken,
) -> Result<Vec<u8>, AddSplTokenError> {
    let client = read_state(|state| state.sol_rpc_client(runtime.inter_canister_call_runtime()));
    let result = client
        .get_account_info(GetAccountInfoParams::from_pubkey(token.mint.clone()))
        .with_encoding(GetAccountInfoEncoding::Base64)
        .with_commitment(CommitmentLevel::Finalized)
        .with_response_size_estimate(2_048)
        .try_send()
        .await
        .map_err(|e| AddSplTokenError::TemporarilyUnavailable(e.to_string()))?;
    let account = match result {
        MultiRpcResult::Consistent(Ok(Some(account))) => Ok(account),
        MultiRpcResult::Consistent(Ok(None)) => Err(AddSplTokenError::InvalidToken(
            "Mint account does not exist".to_string(),
        )),
        MultiRpcResult::Consistent(Err(e)) => {
            Err(AddSplTokenError::TemporarilyUnavailable(e.to_string()))
        }
        MultiRpcResult::Inconsistent(_) => Err(AddSplTokenError::TemporarilyUnavailable(
            "Inconsistent mint account RPC results".to_string(),
        )),
    }?;
    if account.executable || account.owner != token.token_program.to_string() {
        return Err(AddSplTokenError::InvalidToken(
            "Mint must be a non-executable account owned by the configured token program"
                .to_string(),
        ));
    }
    let data = account.data.decode().ok_or_else(|| {
        AddSplTokenError::InvalidToken("Cannot decode mint account data".to_string())
    })?;
    Ok(data)
}

fn validate_config(
    token: &SupportedSplToken,
    minter_id: Principal,
) -> Result<(), AddSplTokenError> {
    if ![CLASSIC_TOKEN_PROGRAM, TOKEN_2022_PROGRAM]
        .contains(&token.token_program.to_string().as_str())
    {
        return Err(AddSplTokenError::InvalidToken(
            "Only the classic SPL Token and Token-2022 programs are supported".to_string(),
        ));
    }
    if token.minimum_deposit_amount == 0 {
        return Err(AddSplTokenError::InvalidToken(
            "Minimum deposit amount must be greater than zero".to_string(),
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
    let decimals = if token.token_program.to_string() == TOKEN_2022_PROGRAM {
        let mint = StateWithExtensions::<Mint2022>::unpack(data)
            .map_err(|error| AddSplTokenError::InvalidToken(error.to_string()))?;
        let extensions = mint
            .get_extension_types()
            .map_err(|error| AddSplTokenError::InvalidToken(error.to_string()))?;
        if !extensions.is_empty() {
            return Err(AddSplTokenError::InvalidToken(
                "Token-2022 mint extensions are not supported".to_string(),
            ));
        }
        mint.base.decimals
    } else {
        Mint::unpack(data)
            .map_err(|error| AddSplTokenError::InvalidToken(error.to_string()))?
            .decimals
    };
    if decimals != token.decimals {
        return Err(AddSplTokenError::InvalidToken(format!(
            "Mint decimals are {decimals}, expected {}",
            token.decimals
        )));
    }
    Ok(())
}
