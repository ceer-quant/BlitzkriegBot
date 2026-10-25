//! Wire types for the PolySimulator REST surface (#454).
//!
//! Modeled from the venue's published OpenAPI (llms-full.txt +
//! openapi.json, fetched 2026-10-14). Only the fields the client consumes are
//! modeled; unknown fields are kept (`deny_unknown_fields` deliberately NOT
//! used) so venue-side additions never break parsing. Money is parsed into
//! `rust_decimal::Decimal` from the venue's dollar-string form.

use rust_decimal::Decimal;
use serde::Deserialize;

/// One up/down interval round as `GET /v1/markets/updown` reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct UpDownRow {
    pub event_id: String,
    pub condition_id: String,
    pub slug: Option<String>,
    pub title: Option<String>,
    pub asset: String,
    pub interval: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub active: Option<bool>,
    pub closed: Option<bool>,
    pub resolved: Option<bool>,
    /// Nested per-market rows; `outcomes`/`outcome_prices`/`token_ids` are
    /// JSON-encoded STRINGS needing a second parse (PM Gamma convention).
    pub markets: Option<Vec<UpDownInnerMarket>>,
}

/// The nested market row of an updown entry.
#[derive(Debug, Clone, Deserialize)]
pub struct UpDownInnerMarket {
    pub condition_id: Option<String>,
    pub question: Option<String>,
    /// JSON-encoded string: `"[\"Up\", \"Down\"]"`.
    pub outcomes: Option<String>,
    /// JSON-encoded string: `"[\"0.55\", \"0.45\"]"`.
    pub outcome_prices: Option<String>,
    /// JSON-encoded string: `"[\"71321…\", \"82432…\"]"`.
    pub token_ids: Option<String>,
}

impl UpDownRow {
    /// Parse the nested `token_ids` JSON string. `token_ids[i]` is the CLOB
    /// token of `outcomes[i]`; on this venue `[0]` is the UP token, `[1]` the
    /// DOWN token (same `[yes, no]` convention as the market-list endpoint's
    /// `clob_token_ids`).
    pub fn parse_token_ids(&self) -> Option<(String, String)> {
        let raw = self.markets.as_ref()?.first()?.token_ids.as_deref()?;
        let ids: Vec<String> = serde_json::from_str(raw).ok()?;
        if ids.len() < 2 {
            return None;
        }
        Some((ids[0].clone(), ids[1].clone()))
    }

    /// Parse the nested `outcome_prices` JSON string into (up, down) decimals.
    pub fn parse_outcome_prices(&self) -> Option<(Decimal, Decimal)> {
        let raw = self.markets.as_ref()?.first()?.outcome_prices.as_deref()?;
        let prices: Vec<Decimal> = serde_json::from_str(raw).ok()?;
        if prices.len() < 2 {
            return None;
        }
        Some((prices[0], prices[1]))
    }

    /// Parse the ISO-8601 `end_date` into epoch millis — the round's expiry.
    pub fn end_ms(&self) -> Option<i64> {
        super::discovery::iso_to_ms(self.end_date.as_deref()?)
    }

    /// Parse `start_date` into epoch millis.
    pub fn start_ms(&self) -> Option<i64> {
        super::discovery::iso_to_ms(self.start_date.as_deref()?)
    }

    /// True when the venue reports the round open and unresolved.
    pub fn is_open(&self) -> bool {
        self.active.unwrap_or(false)
            && !self.closed.unwrap_or(false)
            && !self.resolved.unwrap_or(false)
    }
}

/// One price level as the CLOB book endpoint quotes it (strings).
#[derive(Debug, Clone, Deserialize)]
pub struct BookLevel {
    pub price: Decimal,
    pub size: Decimal,
}

/// L2 book snapshot from `GET /v1/clob/book/{token_id}`.
///
/// The venue's arrays are ordered ascending bids / descending asks (best =
/// LAST element on each side). The kernel's `apply_snapshot` replaces B-tree
/// levels, so ordering is irrelevant downstream; nothing here reorders.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderBookSnapshot {
    pub token_id: Option<String>,
    pub bids: Option<Vec<BookLevel>>,
    pub asks: Option<Vec<BookLevel>>,
    /// Venue timestamp, milliseconds as a STRING.
    pub timestamp: Option<String>,
    pub tick_size: Option<Decimal>,
    pub neg_risk: Option<bool>,
    pub stale: Option<bool>,
}

/// Account balance from `GET /v1/account/balance` (strings).
#[derive(Debug, Clone, Deserialize)]
pub struct BalanceBody {
    pub balance: Decimal,
    pub currency: Option<String>,
    pub starting_balance: Option<Decimal>,
    pub unrealized_pnl: Option<Decimal>,
    pub total_value: Option<Decimal>,
}

/// A resting or filled order from `GET /v1/orders` (native envelope).
///
/// `order_id` is an INTEGER on this venue — stringified at the seam so the
/// core's venue-order-id contract (strings) holds everywhere.
#[derive(Debug, Clone, Deserialize)]
pub struct NativeOrder {
    pub order_id: i64,
    pub market_id: String,
    pub side: Option<String>,
    pub outcome: Option<String>,
    pub order_type: Option<String>,
    pub limit_price: Option<Decimal>,
    pub quantity: Option<Decimal>,
    pub time_in_force: Option<String>,
    pub status: Option<String>,
    pub client_order_id: Option<String>,
    pub created_at: Option<String>,
    pub filled_at: Option<String>,
    pub fill_price: Option<Decimal>,
}

impl NativeOrder {
    /// Stringified venue id for the seam.
    pub fn venue_id(&self) -> String {
        self.order_id.to_string()
    }
}

/// One executed trade row from `GET /v1/data/trades` (PM-shape).
///
/// A trade's legs are split across `taker_order_id` (the marketable side) and
/// `maker_orders` (the resting order ids it consumed) — a fill on OUR resting
/// maker order appears here with `taker_order_id` naming someone else. Both
/// sides must be checked to book maker fills.
#[derive(Debug, Clone, Deserialize)]
pub struct PmTradeRow {
    pub id: String,
    pub market: String,
    pub asset_id: Option<String>,
    pub side: Option<String>,
    pub outcome: Option<String>,
    pub price: Option<Decimal>,
    pub size: Option<Decimal>,
    pub fee_rate_bps: Option<i64>,
    pub maker_orders: Option<Vec<String>>,
    pub taker_order_id: Option<String>,
    pub transaction_hash: Option<String>,
    pub bucket_index: Option<i64>,
    /// Last update, epoch millis as a bare integer.
    pub last_update: Option<i64>,
    /// Match time, epoch millis as a bare integer.
    pub match_time: Option<i64>,
    pub owner: Option<String>,
    pub status: Option<String>,
    pub trader_side: Option<String>,
}

/// Envelope of `GET /v1/data/trades` (PM-shape `{limit,count,next_cursor,data}`).
#[derive(Debug, Clone, Deserialize)]
pub struct PmTradesEnvelope {
    pub data: Option<Vec<PmTradeRow>>,
}

/// Open-order envelope of `GET /v1/orders` (native form).
#[derive(Debug, Clone, Deserialize)]
pub struct PendingOrdersEnvelope {
    pub orders: Option<Vec<NativeOrder>>,
}

/// An open position from `GET /v1/account/positions` (required: id,
/// market_id, outcome, quantity, avg_entry_price, status).
#[derive(Debug, Clone, Deserialize)]
pub struct PositionRow {
    pub id: i64,
    pub market_id: String,
    pub outcome: Option<String>,
    pub quantity: Decimal,
    pub avg_entry_price: Decimal,
    pub status: Option<String>,
}

/// Ack of `POST /v1/order` (live shape 2026-10-25:
/// `{"orderID":1404591,"status":"live"|"matched",…}` — camelCase, and a
/// `matched` ack already carries the fill). The DELETE ack is a DIFFERENT
/// envelope (`{"canceled":[…],"not_canceled":{…}}`, see `CancelAck`); this
/// struct only parses the POST side.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderResponse {
    #[serde(rename = "orderID")]
    pub order_id: i64,
    pub status: Option<String>,
    #[serde(rename = "errorMsg", default)]
    pub error_msg: Option<String>,
    #[serde(rename = "tradeIDs", default)]
    pub trade_ids: Option<Vec<String>>,
}

impl OrderResponse {
    pub fn venue_id(&self) -> String {
        self.order_id.to_string()
    }
}

/// Ack of `DELETE /v1/order` (live shape 2026-10-25: a batch envelope —
/// `{"canceled":["1404785"],"not_canceled":{"1404525":"Order not found …"}}`.
/// A single cancel succeeds when its id lands in `canceled`; `not_canceled`
/// carries per-id reasons for the ones the venue refused or had already
/// settled).
#[derive(Debug, Clone, Deserialize)]
pub struct CancelAck {
    #[serde(default)]
    pub canceled: Vec<String>,
    #[serde(default)]
    pub not_canceled: std::collections::HashMap<String, String>,
}

impl CancelAck {
    /// The cancelled id when OUR id is inside `canceled`; `None` otherwise —
    /// the caller turns the remaining case into an UnknownOrder-style error
    /// with the venue's own reason.
    pub fn cancelled_id(&self, order_id: &str) -> Option<&str> {
        self.canceled
            .iter()
            .find(|id| id.as_str() == order_id)
            .map(|id| id.as_str())
    }

    /// The venue's per-id reason when our id landed in `not_canceled`.
    pub fn rejection(&self, order_id: &str) -> Option<&str> {
        self.not_canceled.get(order_id).map(|s| s.as_str())
    }
}

/// Body of `DELETE /v1/order` (single cancel; note the venue's odd `orderID`
/// capitalization).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CancelOneBody {
    #[serde(rename = "orderID")]
    pub order_id: String,
}

/// Body of `POST /v1/order` — the venue's NATIVE order form (verified against
/// the live venue 2026-10-25; the flat form this struct replaced has never
/// been accepted: the validator answers `VALIDATION_FAILED: missing order`).
/// The order rides an `{"order": {…}}` envelope, prices/amounts are integer
/// 1e6-scaled strings — `price = makerAmount/takerAmount` for BUY and
/// `takerAmount/makerAmount` for SELL — and the venue derives outcome,
/// quantity and time-in-force itself (no `outcome`/`quantity`/`order_type`
/// fields exist; `time_in_force`/`client_order_id` are accepted and ignored,
/// so dedupe stays the caller's job via the order's own idempotency).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PlaceOrderBody {
    /// CLOB token id (the same id the descriptors minted).
    #[serde(rename = "tokenId")]
    pub token_id: String,
    /// `BUY` prices as `makerAmount/takerAmount`; `SELL` inverts.
    pub side: String,
    /// Cost side in 1e6-scaled integer-string form (BUY: cash out).
    #[serde(rename = "makerAmount")]
    pub maker_amount: String,
    /// Share side in 1e6-scaled integer-string form.
    #[serde(rename = "takerAmount")]
    pub taker_amount: String,
}

/// Query results from `GET /v1/markets/updown`.
#[derive(Debug, Clone, Deserialize)]
pub struct UpDownEnvelope {
    pub markets: Option<Vec<UpDownRow>>,
}

/// The venue's structured error body: `{"error": "CODE", "message": "…"}`.
/// Parsed by the client so the kernel sees the venue's own code. `error` is
/// tolerated as absent (some error bodies carry only a message or neither);
/// the caller falls back to the raw text when both are missing.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorBody {
    #[serde(default)]
    pub error: String,
    pub message: Option<String>,
}

/// Answer of `GET /v1/markets-by-token/{token_id}`: the (market, outcome)
/// pair the native order form needs. `outcome` is the venue's own label
/// ("Yes"/"Up"/custom) — the executor never guesses it.
#[derive(Debug, Clone, Deserialize)]
pub struct MarketByToken {
    pub condition_id: String,
    pub outcome: Option<String>,
    pub primary_token_id: Option<String>,
    pub secondary_token_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn updown_nested_json_strings_parse() {
        let row: UpDownRow = serde_json::from_str(
            r#"{
                "event_id": "evt_abc",
                "condition_id": "0xabc",
                "asset": "BTC",
                "interval": "1H",
                "start_date": "2026-03-15T13:00:00+00:00",
                "end_date": "2026-03-15T14:00:00Z",
                "active": true,
                "closed": false,
                "resolved": false,
                "markets": [{
                    "condition_id": "0xabc",
                    "question": "Will BTC go up?",
                    "outcomes": "[\"Up\", \"Down\"]",
                    "outcome_prices": "[\"0.55\", \"0.45\"]",
                    "token_ids": "[\"7132104567890123456789012345678901234567890123456789012345678901234\", \"8243215678901234567890123456789012345678901234567890123456789012345\"]"
                }]
            }"#,
        )
        .expect("updown row parses");
        assert_eq!(row.asset, "BTC");
        let (up, down) = row.parse_token_ids().expect("token ids parse");
        assert!(up.starts_with('7'));
        assert!(down.starts_with('8'));
        let (up_price, down_price) = row.parse_outcome_prices().expect("prices parse");
        assert_eq!(up_price, Decimal::from_str("0.55").unwrap());
        assert_eq!(down_price, Decimal::from_str("0.45").unwrap());
        assert!(row.is_open());
    }

    #[test]
    fn book_snapshot_keeps_venue_ordering_verbatim() {
        // best bid = LAST element of bids (ascending); best ask = LAST of asks
        // (descending). The kernel replaces B-tree levels so ordering does not
        // matter; this pin documents the venue's documented quirk.
        let book: OrderBookSnapshot = serde_json::from_str(
            r#"{
                "token_id": "71321",
                "bids": [{"price": "0.54", "size": "100"}, {"price": "0.55", "size": "80"}],
                "asks": [{"price": "0.57", "size": "90"}, {"price": "0.56", "size": "70"}],
                "timestamp": "1770706800000",
                "tick_size": "0.01",
                "neg_risk": false,
                "stale": false
            }"#,
        )
        .expect("book parses");
        let bids = book.bids.expect("bids");
        let asks = book.asks.expect("asks");
        assert_eq!(bids[0].price, Decimal::from_str("0.54").unwrap());
        assert_eq!(bids[1].price, Decimal::from_str("0.55").unwrap());
        assert_eq!(asks[0].price, Decimal::from_str("0.57").unwrap());
        assert_eq!(asks[1].price, Decimal::from_str("0.56").unwrap());
        assert_eq!(book.timestamp.as_deref(), Some("1770706800000"));
    }

    #[test]
    fn native_order_id_is_an_integer_stringified_at_the_seam() {
        let o: NativeOrder = serde_json::from_str(
            r#"{
                "order_id": 6391,
                "market_id": "0xabc",
                "side": "BUY",
                "outcome": "Yes",
                "order_type": "limit",
                "limit_price": "0.55",
                "quantity": "10",
                "time_in_force": "GTC",
                "status": "PENDING",
                "client_order_id": "core-42"
            }"#,
        )
        .expect("native order parses");
        assert_eq!(o.venue_id(), "6391");
        assert_eq!(o.status.as_deref(), Some("PENDING"));
    }

    #[test]
    fn pm_trade_row_carries_both_leg_sides() {
        let row: PmTradeRow = serde_json::from_str(
            r#"{
                "id": "tx-1",
                "market": "0xabc",
                "asset_id": "71321",
                "side": "SELL",
                "outcome": "Yes",
                "price": "0.55",
                "size": "10",
                "fee_rate_bps": 0,
                "maker_orders": ["41"],
                "taker_order_id": "9999",
                "bucket_index": 3,
                "last_update": 1770706800000,
                "match_time": 1770706800000,
                "owner": "0xabc",
                "status": "MATCHED",
                "trader_side": "TAKER"
            }"#,
        )
        .expect("trade row parses");
        assert_eq!(row.maker_orders.as_deref(), Some(&["41".to_string()][..]));
        assert_eq!(row.taker_order_id.as_deref(), Some("9999"));
        assert_eq!(row.price, Some(Decimal::from_str("0.55").unwrap()));
        assert_eq!(row.match_time, Some(1_770_706_800_000));
    }

    #[test]
    fn order_response_parses_the_live_camel_ack() {
        // Live shape 2026-10-25: camelCase orderID, a matched ack already
        // carries the fill ids.
        let ack: OrderResponse = serde_json::from_str(
            r#"{"orderID":1404591,"status":"matched","makingAmount":"4950000","takingAmount":"5000000","transactionsHashes":null,"tradeIDs":["1404591"],"errorMsg":""}"#,
        )
        .expect("ack parses");
        assert_eq!(ack.venue_id(), "1404591");
        assert_eq!(ack.status.as_deref(), Some("matched"));
        assert_eq!(ack.trade_ids.as_deref(), Some(&["1404591".to_string()][..]));
    }

    #[test]
    fn cancel_ack_reports_our_id_out_of_the_batch_envelope() {
        // Live shape 2026-10-25: {"canceled":[…],"not_canceled":{id: reason}}.
        let ack: CancelAck = serde_json::from_str(
            r#"{"canceled":["1404785"],"not_canceled":{"1404525":"Order not found or already cancelled/filled"}}"#,
        )
        .expect("cancel ack parses");
        assert_eq!(ack.cancelled_id("1404785"), Some("1404785"));
        assert_eq!(ack.cancelled_id("1404525"), None);
        assert_eq!(
            ack.rejection("1404525"),
            Some("Order not found or already cancelled/filled")
        );
        assert_eq!(ack.rejection("1404785"), None);
    }

    #[test]
    fn place_body_serializes_the_native_envelope() {
        let body = PlaceOrderBody {
            token_id: "71321".to_string(),
            side: "BUY".to_string(),
            maker_amount: "2175000".to_string(),
            taker_amount: "5000000".to_string(),
        };
        let wire = serde_json::to_value(&body).expect("serializes");
        let wrapped = serde_json::json!({ "order": wire });
        assert_eq!(
            wrapped,
            serde_json::json!({"order": {
                "tokenId": "71321", "side": "BUY",
                "makerAmount": "2175000", "takerAmount": "5000000"
            }})
        );
    }
}
