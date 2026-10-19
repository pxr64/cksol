use super::*;
use crate::{
    spl_fee::FeeEstimator,
    test_fixtures::{runtime::TestCanisterRuntime, xrc_rate_at},
};
use ic_canister_runtime::IcError;
use ic_xrc_types::{ExchangeRate, ExchangeRateError};

const NOW_SECONDS: u64 = 1_800_000_000;
const MINT: Address = Address::new_from_array([1; 32]);

#[tokio::test]
async fn should_accept_a_symbol_the_xrc_quotes() {
    let runtime = runtime_with_rate(exchange_rate());

    assert_eq!(check_xrc_asset(&runtime, "USDC").await, Ok(()));

    let calls = runtime.sent_update_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].canister_id, XRC_CANISTER_ID);
    let request: GetExchangeRateRequest = calls[0].single_arg();
    assert_eq!(request.quote_asset.symbol, "USDC");
}

#[tokio::test]
async fn should_tell_an_unknown_asset_from_an_unavailable_xrc() {
    for (result, expected) in [
        (
            GetExchangeRateResult::Err(ExchangeRateError::CryptoQuoteAssetNotFound),
            XrcAssetError::UnknownAsset("CryptoQuoteAssetNotFound".to_string()),
        ),
        (
            GetExchangeRateResult::Err(ExchangeRateError::Pending),
            XrcAssetError::Unavailable("XRC returned an error: Pending".to_string()),
        ),
    ] {
        let runtime = TestCanisterRuntime::new().add_stub_response(result);

        assert_eq!(check_xrc_asset(&runtime, "USDC").await, Err(expected));
    }
    let runtime = TestCanisterRuntime::new().add_stub_error(IcError::CallPerformFailed);
    assert_eq!(
        check_xrc_asset(&runtime, "USDC").await,
        Err(XrcAssetError::Unavailable(format!(
            "Failed to call XRC: {}",
            IcError::CallPerformFailed
        )))
    );
}

fn exchange_rate() -> ExchangeRate {
    xrc_rate_at("USDC", 150_000_000_000, NOW_SECONDS - 60)
}

fn runtime_with_rate(rate: ExchangeRate) -> TestCanisterRuntime {
    TestCanisterRuntime::new()
        .with_time(NOW_SECONDS * 1_000_000_000)
        .add_stub_response(GetExchangeRateResult::Ok(rate))
}

#[tokio::test]
async fn should_request_the_sol_to_token_rate_and_estimate_the_fee() {
    let runtime = runtime_with_rate(exchange_rate());
    let oracle = XrcPriceOracle::new(runtime.clone(), MINT, "USDC".to_string(), 6);
    let estimator = FeeEstimator::new(oracle);

    assert_eq!(estimator.estimate_fee(MINT, 5_000).await, Ok(750));

    let calls = runtime.sent_update_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].canister_id, XRC_CANISTER_ID);
    assert_eq!(XRC_CANISTER_ID.to_text(), "uf6dk-hyaaa-aaaaq-qaaaq-cai");
    assert_eq!(calls[0].method, "get_exchange_rate");
    let request: GetExchangeRateRequest = calls[0].single_arg();
    assert_eq!(request.base_asset, exchange_rate().base_asset);
    assert_eq!(request.quote_asset, exchange_rate().quote_asset);
    assert_eq!(request.timestamp, None);
}

#[tokio::test]
async fn should_scale_the_rate_and_token_decimals_and_round_up() {
    for (amount, decimals, rate, rate_decimals, expected) in [
        (0, 6, 150_000_000_000, 9, 0),
        (1, 6, 150_000_000_000, 9, 1),
        (5_000, 6, 150_000_000_000, 9, 750),
        (5_000, 9, 150_000_000_000, 9, 750_000),
        (5_000, 18, 150_000_000_000, 9, 750_000_000_000_000),
        (1_000_000_000, 6, 12_345_678_901, 9, 12_345_679),
        (5_000, 6, 150_000_000, 6, 750),
        (u64::MAX, 9, 1_000_000_000_000_000_000, 18, u64::MAX),
    ] {
        let mut response = exchange_rate();
        response.rate = rate;
        response.metadata.decimals = rate_decimals;
        let oracle = XrcPriceOracle::new(
            runtime_with_rate(response),
            MINT,
            "USDC".to_string(),
            decimals,
        );

        assert_eq!(oracle.sol_to_spl(amount, MINT).await, Ok(expected));
    }
}

#[tokio::test]
async fn should_accept_a_rate_slightly_ahead_of_the_local_clock() {
    let mut ahead = exchange_rate();
    ahead.timestamp = NOW_SECONDS + 5;
    let oracle = XrcPriceOracle::new(runtime_with_rate(ahead), MINT, "USDC".to_string(), 6);

    assert_eq!(oracle.sol_to_spl(5_000, MINT).await, Ok(750));
}

#[tokio::test]
async fn should_reject_an_unconfigured_mint_without_calling_xrc() {
    let runtime = TestCanisterRuntime::new();
    let oracle = XrcPriceOracle::new(runtime.clone(), MINT, "USDC".to_string(), 6);

    assert_eq!(
        oracle
            .sol_to_spl(5_000, Address::new_from_array([2; 32]))
            .await,
        Err("Mint does not match the configured XRC asset".to_string())
    );
    assert!(runtime.sent_update_calls().is_empty());
}

#[tokio::test]
async fn should_return_xrc_and_inter_canister_call_errors() {
    for (runtime, expected) in [
        (
            TestCanisterRuntime::new()
                .add_stub_response(GetExchangeRateResult::Err(ExchangeRateError::Pending)),
            "XRC returned an error: Pending".to_string(),
        ),
        (
            TestCanisterRuntime::new().add_stub_response(GetExchangeRateResult::Err(
                ExchangeRateError::CryptoQuoteAssetNotFound,
            )),
            "XRC returned an error: CryptoQuoteAssetNotFound".to_string(),
        ),
        (
            TestCanisterRuntime::new().add_stub_error(IcError::CallPerformFailed),
            format!("Failed to call XRC: {}", IcError::CallPerformFailed),
        ),
    ] {
        let oracle = XrcPriceOracle::new(runtime, MINT, "USDC".to_string(), 6);

        assert_eq!(oracle.sol_to_spl(5_000, MINT).await, Err(expected));
    }
}

#[tokio::test]
async fn should_reject_unusable_exchange_rates() {
    let mut wrong_base = exchange_rate();
    wrong_base.base_asset.symbol = "BTC".to_string();
    let mut wrong_quote = exchange_rate();
    wrong_quote.quote_asset.class = AssetClass::FiatCurrency;
    let mut zero = exchange_rate();
    zero.rate = 0;
    let mut outdated = exchange_rate();
    outdated.timestamp = NOW_SECONDS - MAX_RATE_AGE_SECONDS - 1;
    let mut future = exchange_rate();
    future.timestamp = NOW_SECONDS + MAX_RATE_AGE_SECONDS + 1;
    let mut excessive_scale = exchange_rate();
    excessive_scale.metadata.decimals = u32::MAX;

    for (response, expected) in [
        (wrong_base, "XRC returned a different asset pair"),
        (wrong_quote, "XRC returned a different asset pair"),
        (zero, "XRC returned an unusable exchange rate"),
        (outdated, "XRC returned an outdated or future exchange rate"),
        (future, "XRC returned an outdated or future exchange rate"),
        (excessive_scale, "XRC rate decimals exceed 255"),
    ] {
        let oracle = XrcPriceOracle::new(runtime_with_rate(response), MINT, "USDC".to_string(), 6);

        assert_eq!(
            oracle.sol_to_spl(5_000, MINT).await,
            Err(expected.to_string())
        );
    }
}

#[tokio::test]
async fn should_reject_a_fee_that_exceeds_u64() {
    let oracle = XrcPriceOracle::new(
        runtime_with_rate(exchange_rate()),
        MINT,
        "USDC".to_string(),
        18,
    );

    assert_eq!(
        oracle.sol_to_spl(u64::MAX, MINT).await,
        Err("Estimated SPL fee exceeds u64::MAX".to_string())
    );
}
