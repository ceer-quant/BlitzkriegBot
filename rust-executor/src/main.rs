use std::env;
use std::io::{self, BufRead, Write};
use std::str::FromStr;

use alloy::signers::local::LocalSigner;
use alloy::signers::Signer;
use polymarket_client_sdk_v2::clob::types::{AssetType, OrderType, Side, SignatureType};
use polymarket_client_sdk_v2::clob::{Client, Config};
use polymarket_client_sdk_v2::types::{Address, Decimal, U256};
use polymarket_client_sdk_v2::POLYGON;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
struct Request {
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct Response {
    id: Value,
    ok: bool,
    result: Option<Value>,
    error: Option<String>,
}

fn response_ok(id: Value, result: Value) -> Response {
    Response { id, ok: true, result: Some(result), error: None }
}

fn response_err(id: Value, error: impl ToString) -> Response {
    Response { id, ok: false, result: None, error: Some(error.to_string()) }
}

fn decimal_param(params: &Value, name: &str) -> anyhow::Result<Decimal> {
    let value = &params[name];
    if let Some(text) = value.as_str() {
        return Decimal::from_str(text).map_err(Into::into);
    }
    if let Some(number) = value.as_f64() {
        return Decimal::from_str(&number.to_string()).map_err(Into::into);
    }
    Err(anyhow::anyhow!("missing or invalid numeric parameter: {name}"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let private_key = env::var("POLYMARKET_PRIVATE_KEY")?;
    let funder = Address::from_str(&env::var("POLYMARKET_FUNDER_ADDRESS")?)?;
    let signer = LocalSigner::from_str(&private_key)?.with_chain_id(Some(POLYGON));

    let client = Client::new(
        &env::var("CLOB_API_URL").unwrap_or_else(|_| "https://clob.polymarket.com".into()),
        Config::builder().use_server_time(true).build(),
    )?
    .authentication_builder(&signer)
    .funder(funder)
    .signature_type(SignatureType::Poly1271)
    .authenticate()
    .await?;

    eprintln!("rust executor ready signer={} funder={} signature_type=Poly1271", signer.address(), funder);

    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout());
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        let request: Request = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(err) => {
                writeln!(stdout, "{}", serde_json::to_string(&response_err(Value::Null, err))?)?;
                stdout.flush()?;
                continue;
            }
        };

        let id = request.id.clone();
        let result = match request.method.as_str() {
            "auth_check" => Ok(json!({ "signer": signer.address().to_string(), "funder": funder.to_string(), "signatureType": 3 })),
            "open_orders" => client.orders(&Default::default(), None).await.map(|orders| json!({ "count": orders.data.len() })),
            "balance" => {
                let req = polymarket_client_sdk_v2::clob::types::request::BalanceAllowanceRequest::builder().asset_type(AssetType::Collateral).build();
                client.balance_allowance(req).await.map(|balance| json!({ "balance": format!("{}", balance.balance), "allowances": balance.allowances }))
            }
            "place_limit" => {
                let token_id = U256::from_str(request.params["tokenId"].as_str().unwrap_or(""))?;
                let price = decimal_param(&request.params, "price")?;
                let size = decimal_param(&request.params, "size")?;
                let side = if request.params["side"].as_str().unwrap_or("BUY") == "SELL" { Side::Sell } else { Side::Buy };
                client.limit_order()
                    .token_id(token_id)
                    .side(side)
                    .price(price)
                    .size(size)
                    .order_type(OrderType::GTC)
                    .post_only(request.params["postOnly"].as_bool().unwrap_or(true))
                    .build_sign_and_post(&signer)
                    .await
                    .map(|order| json!({ "orderId": order.order_id, "status": order.status, "success": order.success, "tradeIds": order.trade_ids }))
            }
            "cancel" => {
                let order_id = request.params["orderId"].as_str().unwrap_or("");
                client.cancel_order(order_id).await.map(|result| json!({ "success": true, "result": format!("{result:?}") }))
            }
            method => Err(polymarket_client_sdk_v2::error::Error::validation(format!("unknown method: {method}"))),
        };

        let output = match result {
            Ok(value) => response_ok(id, value),
            Err(err) => response_err(id, err),
        };
        writeln!(stdout, "{}", serde_json::to_string(&output)?)?;
        stdout.flush()?;
    }
    Ok(())
}
