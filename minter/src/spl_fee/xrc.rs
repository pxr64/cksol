use crate::{constants::XRC_REQUEST_CYCLES, runtime::CanisterRuntime, spl_fee::PriceOracle};
use candid::{Nat, Principal};
use cksol_types::Lamport;
use ic_canister_runtime::Runtime;
use ic_xrc_types::{
    Asset, AssetClass, ExchangeRateError, GetExchangeRateRequest, GetExchangeRateResult,
};
use num_traits::ToPrimitive;
use solana_address::Address;
use thiserror::Error;

#[cfg(test)]
mod tests;

/// The exchange rate canister `uf6dk-hyaaa-aaaaq-qaaaq-cai`.
const XRC_CANISTER_ID: Principal = Principal::from_slice(&[0, 0, 0, 0, 2, 16, 0, 1, 1, 1]);
/// Rates expire after five minutes to limit pricing drift. The window also absorbs the clock
/// difference between subnets.
const MAX_RATE_AGE_SECONDS: u64 = 300;

/// Why the exchange rate canister cannot quote SOL in a symbol.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum XrcAssetError {
    #[error("the exchange rate canister has no SOL rate in that symbol: {0}")]
    UnknownAsset(String),
    #[error("{0}")]
    Unavailable(String),
}

/// Checks that the exchange rate canister quotes SOL in `symbol`, so that a token with a
/// wrong symbol is rejected at registration.
pub async fn check_xrc_asset<R: CanisterRuntime>(
    runtime: &R,
    symbol: &str,
) -> Result<(), XrcAssetError> {
    match get_exchange_rate(runtime, symbol)
        .await
        .map_err(XrcAssetError::Unavailable)?
    {
        Ok(_) => Ok(()),
        Err(
            error @ (ExchangeRateError::CryptoBaseAssetNotFound
            | ExchangeRateError::CryptoQuoteAssetNotFound
            | ExchangeRateError::StablecoinRateNotFound
            | ExchangeRateError::StablecoinRateTooFewRates
            | ExchangeRateError::StablecoinRateZeroRate
            | ExchangeRateError::ForexBaseAssetNotFound
            | ExchangeRateError::ForexQuoteAssetNotFound
            | ExchangeRateError::ForexAssetsNotFound),
        ) => Err(XrcAssetError::UnknownAsset(format!("{error:?}"))),
        Err(error) => Err(XrcAssetError::Unavailable(format!(
            "XRC returned an error: {error:?}"
        ))),
    }
}

/// Requests the current SOL rate in `symbol` from the exchange rate canister.
async fn get_exchange_rate<R: CanisterRuntime>(
    runtime: &R,
    symbol: &str,
) -> Result<GetExchangeRateResult, String> {
    runtime
        .inter_canister_call_runtime()
        .update_call(
            XRC_CANISTER_ID,
            "get_exchange_rate",
            (exchange_rate_request(symbol),),
            XRC_REQUEST_CYCLES,
        )
        .await
        .map_err(|error| format!("Failed to call XRC: {error}"))
}

fn exchange_rate_request(symbol: &str) -> GetExchangeRateRequest {
    GetExchangeRateRequest {
        base_asset: crypto_asset("SOL"),
        quote_asset: crypto_asset(symbol),
        timestamp: None,
    }
}

fn crypto_asset(symbol: &str) -> Asset {
    Asset {
        symbol: symbol.to_string(),
        class: AssetClass::Cryptocurrency,
    }
}

/// Converts SOL costs for one mint using its configured XRC symbol and token decimals.
pub struct XrcPriceOracle<R> {
    runtime: R,
    mint: Address,
    symbol: String,
    decimals: u8,
}

impl<R: CanisterRuntime> XrcPriceOracle<R> {
    pub fn new(runtime: R, mint: Address, symbol: String, decimals: u8) -> Self {
        Self {
            runtime,
            mint,
            symbol,
            decimals,
        }
    }
}

impl<R: CanisterRuntime> PriceOracle for XrcPriceOracle<R> {
    async fn sol_to_spl(&self, amount: Lamport, mint: Address) -> Result<u64, String> {
        if mint != self.mint {
            return Err("Mint does not match the configured XRC asset".to_string());
        }
        let request = exchange_rate_request(&self.symbol);
        let rate = get_exchange_rate(&self.runtime, &self.symbol)
            .await?
            .map_err(|error| format!("XRC returned an error: {error:?}"))?;
        if rate.base_asset != request.base_asset || rate.quote_asset != request.quote_asset {
            return Err("XRC returned a different asset pair".to_string());
        }
        if rate.rate == 0 {
            return Err("XRC returned an unusable exchange rate".to_string());
        }
        let now = self.runtime.time() / 1_000_000_000;
        if now.abs_diff(rate.timestamp) > MAX_RATE_AGE_SECONDS {
            return Err("XRC returned an outdated or future exchange rate".to_string());
        }
        // Bound the exponent before constructing the rate scale.
        if rate.metadata.decimals > u32::from(u8::MAX) {
            return Err("XRC rate decimals exceed 255".to_string());
        }
        // Use wide integers so intermediate scaling cannot overflow a valid token fee.
        let numerator =
            Nat::from(amount).0 * rate.rate * Nat::from(10u8).0.pow(u32::from(self.decimals));
        // Divide by 10^9 to convert lamports to SOL and by 10^rate_decimals to unscale XRC's rate.
        let denominator = Nat::from(10u8).0.pow(9 + rate.metadata.decimals);
        // Round up, so the fee never undercharges the cost.
        let fee = (numerator + &denominator - 1u8) / denominator;
        fee.to_u64()
            .ok_or_else(|| "Estimated SPL fee exceeds u64::MAX".to_string())
    }
}
