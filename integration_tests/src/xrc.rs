use crate::{CanisterHttpReply, CanisterHttpRequest, CanisterHttpResponse, canister_wasm};
use candid::{Encode, Principal};
use pocket_ic::nonblocking::PocketIc;
use serde_json::json;

pub const CANISTER_ID: Principal = Principal::from_slice(&[0, 0, 0, 0, 2, 16, 0, 1, 1, 1]);
/// XRC charges its base cost plus two uncached cryptocurrency lookups.
pub const REQUEST_COST: u128 = 20_000_000 + 2 * 240_000_000;

/// Installs the official XRC release used by the SPL integration tests.
pub async fn install(env: &PocketIc) {
    if env.canister_exists(CANISTER_ID).await {
        return;
    }
    env.create_canister_with_id(None, None, CANISTER_ID)
        .await
        .unwrap();
    env.add_cycles(CANISTER_ID, u64::MAX as u128).await;
    env.install_canister(
        CANISTER_ID,
        canister_wasm(
            "../wasms/xrc-2026.02.24.wasm.gz",
            "https://github.com/dfinity/exchange-rate-canister/releases/download/2026.02.24/xrc.wasm.gz",
        )
        .await,
        Encode!().unwrap(),
        None,
    )
    .await;
}

/// Mocks exchange HTTP responses, leaving parsing, rate calculation and refunds to XRC.
pub fn http_response(request: &CanisterHttpRequest) -> Option<CanisterHttpResponse> {
    let url = reqwest::Url::parse(&request.url).unwrap();
    let host = url.host_str()?;
    let is_exchange = matches!(
        host,
        "api.exchange.coinbase.com"
            | "api.kucoin.com"
            | "www.okx.com"
            | "api.gateio.ws"
            | "api.mexc.com"
            | "api.poloniex.com"
            | "api.crypto.com"
            | "api.bitget.com"
            | "openapi.digifinex.com"
    );
    if !is_exchange {
        // Fiat rates are not used by these crypto/crypto requests. Answer the
        // background forex requests so they do not block the HTTP mock queue.
        return matches!(
            host,
            "www.rba.gov.au"
                | "www.cbbh.ba"
                | "www.bankofcanada.ca"
                | "www.ecb.europa.eu"
                | "nbg.gov.ge"
                | "tassidicambio.bancaditalia.it"
                | "forex.cbm.gov.mm"
                | "www.nrb.org.np"
                | "www.backend-rates.bazg.admin.ch"
                | "www.tcmb.gov.tr"
                | "cbu.uz"
        )
        .then(|| reply(503, vec![]));
    }

    // A low positive SOL/SPL rate keeps the fee at one token unit in the
    // deposit lifecycle fixtures. These are synthetic prices, not live quotes.
    let (rate, float_rate) = if request.url.contains("SOL") {
        ("0.000000001", 0.000_000_001)
    } else {
        assert!(request.url.contains("SPL"), "Unexpected XRC pair: {url}");
        ("1.0", 1.0)
    };
    // Candle timestamps do not affect XRC's extraction; XRC supplies the
    // requested minute in its exchange-rate response.
    let body = match host {
        "api.exchange.coinbase.com" => json!([[0, 1.0, 1.0, float_rate, 1.0, 1.0]]),
        "api.kucoin.com" => json!({"data": [["0", rate, "1", "1", "1", "1", "1"]]}),
        "www.okx.com" => json!({"data": [["0", rate, "1", "1", "1", "1", "1", "1", "1"]]}),
        "api.gateio.ws" => json!([["0", "1", "1", rate, "1", "1", "0", "true"]]),
        "api.mexc.com" => json!({"data": [[0, rate, "1", "1", "1", "1", 0, "1"]]}),
        "api.poloniex.com" => json!([[
            "1", "1", rate, "1", "1", "1", "1", "1", 1, 0, "1", "MINUTE_1", 0, 0
        ]]),
        "api.crypto.com" => json!({"result": {"data": [{"o": rate}]}}),
        "api.bitget.com" => json!({"data": [["0", rate, "1", "1", "1", "1", "1", "1"]]}),
        "openapi.digifinex.com" => json!({"data": [[0, 1.0, 1.0, 1.0, 1.0, float_rate]]}),
        _ => unreachable!(),
    };
    Some(reply(200, serde_json::to_vec(&body).unwrap()))
}

fn reply(status: u16, body: Vec<u8>) -> CanisterHttpResponse {
    CanisterHttpResponse::CanisterHttpReply(CanisterHttpReply {
        status,
        headers: vec![],
        body,
    })
}
