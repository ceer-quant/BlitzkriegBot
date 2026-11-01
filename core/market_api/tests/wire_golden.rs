//! Wire goldens for the market-agnostic contract DTOs — issue #840 PR0.
//!
//! These tests are migration guardrails, not feature tests: before any type
//! moves into `bk_contract` (Wave 1+), each golden below pins the EXACT wire
//! bytes the boundary speaks today. After the move the same goldens must pass
//! against the re-exported types — byte-for-byte, key order included.
//!
//! What is pinned here (§"0.4.2 freeze surface — order intent/fill,
//! feed/discovery, execution, account/risk view, JSON-RPC schema"):
//!
//! * every contract enum's wire spelling (`lowercase`/`UPPERCASE`/
//!   `snake_case`/`SCREAMING_SNAKE_CASE`) — adding a variant or changing a
//!   spelling is `[CONTRACT-BREAKING]`;
//! * the two Decimal wire styles: `#[serde(with = "decimal")]` fields ride as
//!   JSON numbers (f64 shortest-repr), while default-serde Decimal fields
//!   (e.g. `BookUpdate::bids` levels, `MarketResolution::payouts`,
//!   `RedemptionRequest::outcome_shares`) ride as JSON strings;
//! * full-struct camelCase shapes including the presence/absence rules
//!   (`skip_serializing_if` keys vanish, plain `Option`s serialize as `null`,
//!   `#[serde(default)]` vecs/strings serialize as present);
//! * `Kline`'s derived `close_time_ms` and the interval wire names;
//! * the `MarketCapabilities` bitmap: bit values, transparent (bare number)
//!   wire, superset semantics and the bit-order name table.
//!
//! Golden style follows the repo precedent
//! (`blitzkrieg_core/src/strategies/test_support.rs`: inline `concat!(r#"…"#)`
//! compared against `serde_json::to_string` — byte-for-byte, no snapshot
//! library). Every golden is also re-deserialized and re-serialized to prove
//! the canonical form is stable under a round-trip.

use blitzkrieg_market_api::{
    AccountId, BookUpdate, CoreError, CoreErrorCode, FillPolicy, FillStatus, Kline, KlineInterval,
    MarketCapabilities, MarketDescriptor, MarketFill, MarketMode, MarketResolution,
    MarketStructure, MarketType, MergeRequest, MergeResult, NetCheckItem, NetCheckReport,
    OrderIntent, OrderKind, OrderStatus, OrderType, PendingOrder, PluginInfo, ReconcileSnapshot,
    RedemptionFailure, RedemptionRequest, RedemptionResult, SelfCheckItem, SelfCheckReport,
    SettlementQuery, Side, SpotUpdate, TimeInForce, TopOfBookUpdate, VenueTradeInfo,
};
use rust_decimal::Decimal;
use std::str::FromStr;

/// Fixture timestamp — fixed, never `now` (same rule as `engine_stats_view_seam`).
const TS: i64 = 1_000_000_000_000;

fn wire<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("golden instance must serialize")
}

/// Serialize → golden, then deserialize → re-serialize → golden. Proves the
/// canonical form is stable and the deserializer accepts exactly these bytes.
fn assert_round_trip<T>(v: &T, golden: &str)
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let direct = wire(v);
    assert_eq!(direct, golden, "direct serialization drifted from golden");
    let back: T = serde_json::from_str(golden).expect("golden bytes must deserialize");
    assert_eq!(wire(&back), golden, "round-trip re-serialization drifted");
}

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).expect("fixture decimal")
}

// ── enum wire spellings ─────────────────────────────────────────────────────

#[test]
fn enum_wire_spellings_are_frozen() {
    // lowercase
    assert_eq!(wire(&Side::Buy), r#""buy""#);
    assert_eq!(wire(&Side::Sell), r#""sell""#);
    // UPPERCASE
    assert_eq!(wire(&OrderType::Gtc), r#""GTC""#);
    assert_eq!(wire(&OrderType::Fok), r#""FOK""#);
    assert_eq!(wire(&OrderType::Fak), r#""FAK""#);
    assert_eq!(wire(&OrderType::Gtd), r#""GTD""#);
    assert_eq!(wire(&TimeInForce::Gtd), r#""GTD""#);
    // snake_case
    assert_eq!(wire(&FillPolicy::Taker), r#""taker""#);
    assert_eq!(wire(&FillPolicy::Maker), r#""maker""#);
    assert_eq!(wire(&FillPolicy::MakerThenTaker), r#""maker_then_taker""#);
    assert_eq!(wire(&MarketType::Prediction), r#""prediction""#);
    assert_eq!(wire(&MarketType::Spot), r#""spot""#);
    assert_eq!(wire(&MarketType::Margin), r#""margin""#);
    assert_eq!(wire(&MarketType::Futures), r#""futures""#);
    assert_eq!(wire(&MarketType::Options), r#""options""#);
    assert_eq!(wire(&MarketType::Earn), r#""earn""#);
    assert_eq!(wire(&MarketType::Bot), r#""bot""#);
    // SCREAMING_SNAKE_CASE
    assert_eq!(wire(&OrderStatus::Pending), r#""PENDING""#);
    assert_eq!(wire(&OrderStatus::Live), r#""LIVE""#);
    assert_eq!(wire(&OrderStatus::PartiallyFilled), r#""PARTIALLY_FILLED""#);
    assert_eq!(wire(&OrderStatus::Filled), r#""FILLED""#);
    assert_eq!(wire(&OrderStatus::Cancelled), r#""CANCELLED""#);
    assert_eq!(wire(&OrderStatus::Rejected), r#""REJECTED""#);
    assert_eq!(wire(&OrderStatus::Failed), r#""FAILED""#);
    // #457: ForceClosed is deliberately NOT Cancelled on the wire.
    assert_eq!(wire(&OrderStatus::ForceClosed), r#""FORCE_CLOSED""#);
    assert_eq!(wire(&FillStatus::Matched), r#""MATCHED""#);
    assert_eq!(wire(&FillStatus::Mined), r#""MINED""#);
    assert_eq!(wire(&FillStatus::Confirmed), r#""CONFIRMED""#);
    assert_eq!(wire(&FillStatus::Failed), r#""FAILED""#);
    assert_eq!(wire(&CoreErrorCode::InvalidParams), r#""INVALID_PARAMS""#);
    assert_eq!(wire(&CoreErrorCode::UnknownOrder), r#""UNKNOWN_ORDER""#);
    assert_eq!(
        wire(&CoreErrorCode::InvalidOrderState),
        r#""INVALID_ORDER_STATE""#
    );
    assert_eq!(wire(&CoreErrorCode::WouldCross), r#""WOULD_CROSS""#);
    assert_eq!(
        wire(&CoreErrorCode::InvalidTickSize),
        r#""INVALID_TICK_SIZE""#
    );
    assert_eq!(wire(&CoreErrorCode::InvalidSize), r#""INVALID_SIZE""#);
    assert_eq!(
        wire(&CoreErrorCode::InsufficientFunds),
        r#""INSUFFICIENT_FUNDS""#
    );
    assert_eq!(wire(&CoreErrorCode::RiskRejected), r#""RISK_REJECTED""#);
    assert_eq!(wire(&CoreErrorCode::AccountLimit), r#""ACCOUNT_LIMIT""#);
    assert_eq!(
        wire(&CoreErrorCode::KillSwitchActive),
        r#""KILL_SWITCH_ACTIVE""#
    );
    assert_eq!(wire(&CoreErrorCode::MarketHalted), r#""MARKET_HALTED""#);
    assert_eq!(
        wire(&CoreErrorCode::NotAuthenticated),
        r#""NOT_AUTHENTICATED""#
    );
    assert_eq!(wire(&CoreErrorCode::VenueError), r#""VENUE_ERROR""#);
    assert_eq!(wire(&CoreErrorCode::Timeout), r#""TIMEOUT""#);
    assert_eq!(wire(&CoreErrorCode::Internal), r#""INTERNAL""#);
    // market structure (snake_case)
    assert_eq!(
        wire(&MarketStructure::CentralLimitOrderBook),
        r#""central_limit_order_book""#
    );
    assert_eq!(
        wire(&MarketStructure::ContinuousAuction),
        r#""continuous_auction""#
    );
    assert_eq!(wire(&MarketStructure::CallAuction), r#""call_auction""#);
    assert_eq!(
        wire(&MarketStructure::AutomatedMarketMaker),
        r#""automated_market_maker""#
    );
    assert_eq!(
        wire(&MarketStructure::BinaryOutcomeWheel),
        r#""binary_outcome_wheel""#
    );
    assert_eq!(
        wire(&MarketStructure::RequestForQuote),
        r#""request_for_quote""#
    );
    // kline intervals (snake_case, = wire_name())
    for (iv, name) in [
        (KlineInterval::Sec1, "sec1"),
        (KlineInterval::Sec5, "sec5"),
        (KlineInterval::Sec15, "sec15"),
        (KlineInterval::Min1, "min1"),
        (KlineInterval::Min5, "min5"),
        (KlineInterval::Min15, "min15"),
        (KlineInterval::Hour1, "hour1"),
        (KlineInterval::Hour4, "hour4"),
        (KlineInterval::Day1, "day1"),
    ] {
        assert_eq!(wire(&iv), format!(r#""{name}""#), "{name} spelling");
        assert_eq!(iv.wire_name(), name);
        assert_eq!(KlineInterval::from_wire(name), Some(iv));
    }
    assert_eq!(
        KlineInterval::from_wire("week1"),
        None,
        "no guessed default"
    );
}

// ── the two Decimal wire styles ─────────────────────────────────────────────

#[test]
fn decimal_wire_styles_are_frozen() {
    // `#[serde(with = "decimal")]` fields ride as JSON numbers.
    #[derive(serde::Serialize)]
    struct Numbered {
        #[serde(with = "blitzkrieg_market_api::decimal")]
        price: Decimal,
    }
    assert_eq!(wire(&Numbered { price: d("0.55") }), r#"{"price":0.55}"#);
    assert_eq!(wire(&Numbered { price: d("1") }), r#"{"price":1.0}"#);

    // Plain Decimal fields (no helper) ride as JSON strings.
    #[derive(serde::Serialize)]
    struct Stringed {
        price: Decimal,
    }
    assert_eq!(wire(&Stringed { price: d("0.55") }), r#"{"price":"0.55"}"#);
}

// ── order intent / fill family ──────────────────────────────────────────────

#[test]
fn order_intent_golden() {
    let intent = OrderIntent {
        market: MarketType::Prediction,
        symbol: "BTC-UP".into(),
        side: Side::Buy,
        order_kind: OrderKind::PostOnly,
        price: Some(d("0.55")),
        size: d("10.5"),
        time_in_force: TimeInForce::Gtd,
        metadata: Default::default(),
    };
    assert_round_trip(
        &intent,
        concat!(
            r#"{"market":"prediction","symbol":"BTC-UP","side":"buy","#,
            r#""orderKind":"POST_ONLY","price":0.55,"size":10.5,"#,
            r#""timeInForce":"GTD","metadata":{}}"#
        ),
    );
}

#[test]
fn market_fill_golden() {
    let fill = MarketFill {
        order_id: "ord-1".into(),
        trade_id: Some("t-1".into()),
        token_id: "tok-up".into(),
        side: Side::Buy,
        price: d("0.55"),
        size: d("10.5"),
        status: FillStatus::Confirmed,
        ts_ms: TS,
        tx_hash: Some("0xabc".into()),
        maker: Some(true),
    };
    assert_round_trip(
        &fill,
        concat!(
            r#"{"orderId":"ord-1","tradeId":"t-1","tokenId":"tok-up","side":"buy","#,
            r#""price":0.55,"size":10.5,"status":"CONFIRMED","tsMs":1000000000000,"#,
            r#""txHash":"0xabc","maker":true}"#
        ),
    );

    // Plain Options WITHOUT skip_serializing_if stay on the wire as null;
    // only `maker` (which has skip_serializing_if) vanishes.
    let dry = MarketFill {
        order_id: "ord-2".into(),
        trade_id: None,
        token_id: "tok-down".into(),
        side: Side::Sell,
        price: d("0.5"),
        size: d("1"),
        status: FillStatus::Matched,
        ts_ms: TS,
        tx_hash: None,
        maker: None,
    };
    assert_round_trip(
        &dry,
        concat!(
            r#"{"orderId":"ord-2","tradeId":null,"tokenId":"tok-down","side":"sell","#,
            r#""price":0.5,"size":1.0,"status":"MATCHED","tsMs":1000000000000,"txHash":null}"#
        ),
    );
}

#[test]
fn pending_order_golden() {
    let po = PendingOrder {
        core_order_id: "core-1".into(),
        token_id: "tok-up".into(),
        condition_id: "cond-1".into(),
        side: Side::Sell,
        fill_policy: FillPolicy::MakerThenTaker,
        price: d("0.6"),
        size: d("5"),
        internal_key: "key-1".into(),
        strategy: "lua_momentum".into(),
        asset: "BTC".into(),
        direction: "UP".into(),
        round_slot: 42,
    };
    assert_round_trip(
        &po,
        concat!(
            r#"{"coreOrderId":"core-1","tokenId":"tok-up","conditionId":"cond-1","side":"sell","#,
            r#""fillPolicy":"maker_then_taker","price":0.6,"size":5.0,"internalKey":"key-1","#,
            r#""strategy":"lua_momentum","asset":"BTC","direction":"UP","roundSlot":42}"#
        ),
    );
}

// ── feed / discovery family ─────────────────────────────────────────────────

#[test]
fn book_update_golden() {
    let book = BookUpdate {
        token_id: "tok-up".into(),
        bids: vec![(d("0.55"), d("10.5")), (d("0.54"), d("20"))],
        asks: vec![(d("0.56"), d("30.25"))],
        ts_ms: TS,
    };
    // Depth levels are plain Decimals → JSON STRINGS, not numbers.
    assert_round_trip(
        &book,
        concat!(
            r#"{"tokenId":"tok-up","bids":[["0.55","10.5"],["0.54","20"]],"#,
            r#""asks":[["0.56","30.25"]],"tsMs":1000000000000}"#
        ),
    );
}

#[test]
fn top_of_book_golden() {
    let top = TopOfBookUpdate {
        token_id: "tok-up".into(),
        best_bid: Some(d("0.55")),
        best_ask: None,
        ts_ms: TS,
    };
    // Plain Options serialize as null (no skip_serializing_if here).
    assert_round_trip(
        &top,
        r#"{"tokenId":"tok-up","bestBid":0.55,"bestAsk":null,"tsMs":1000000000000}"#,
    );
}

#[test]
fn spot_update_golden() {
    let spot = SpotUpdate {
        asset: "BTC".into(),
        price: d("65000.5"),
        ts_ms: TS,
    };
    assert_round_trip(
        &spot,
        r#"{"asset":"BTC","price":65000.5,"tsMs":1000000000000}"#,
    );
}

#[test]
fn market_descriptor_golden() {
    let md = MarketDescriptor {
        asset: "BTC".into(),
        condition_id: "cond-1".into(),
        question_id: "q-1".into(),
        up_token_id: "tok-up".into(),
        down_token_id: "tok-down".into(),
        up_price: d("0.55"),
        down_price: d("0.45"),
        expires_at_ms: TS,
        round_slot: 7,
        venue: "polymarket".into(),
        neg_risk: false,
        question: "BTC up or down?".into(),
    };
    assert_round_trip(
        &md,
        concat!(
            r#"{"asset":"BTC","conditionId":"cond-1","questionId":"q-1","#,
            r#""upTokenId":"tok-up","downTokenId":"tok-down","upPrice":0.55,"downPrice":0.45,"#,
            r#""expiresAtMs":1000000000000,"roundSlot":7,"venue":"polymarket","#,
            r#""negRisk":false,"question":"BTC up or down?"}"#
        ),
    );
}

// ── reconciliation / self-check / network family ────────────────────────────

#[test]
fn reconcile_and_self_check_golden() {
    let snap = ReconcileSnapshot {
        open_order_ids: vec!["ord-1".into()],
        trades: vec![VenueTradeInfo {
            venue_order_id: "v-1".into(),
            trade_id: "t-1".into(),
            token_id: "tok-up".into(),
            side: Side::Buy,
            size: d("10.5"),
            price: d("0.55"),
            ts_ms: TS,
            tx_hash: None,
            maker: Some(true),
        }],
        now_ms: TS,
    };
    // VenueTradeInfo's size/price carry #[serde(with = "decimal")] → numbers.
    assert_round_trip(
        &snap,
        concat!(
            r#"{"openOrderIds":["ord-1"],"trades":[{"venueOrderId":"v-1","tradeId":"t-1","#,
            r#""tokenId":"tok-up","side":"buy","size":10.5,"price":0.55,"#,
            r#""tsMs":1000000000000,"txHash":null,"maker":true}],"nowMs":1000000000000}"#
        ),
    );

    let self_check = SelfCheckReport {
        ok: true,
        ts_ms: TS,
        items: vec![SelfCheckItem {
            name: "auth_balance".into(),
            ok: true,
            detail: "balance ok".into(),
        }],
    };
    assert_round_trip(
        &self_check,
        concat!(
            r#"{"ok":true,"tsMs":1000000000000,"items":[{"name":"auth_balance","#,
            r#""ok":true,"detail":"balance ok"}]}"#
        ),
    );
}

#[test]
fn net_check_golden() {
    let report = NetCheckReport {
        ok: false,
        ts_ms: TS,
        hint_code: "tls_blocked".into(),
        hint: "TCP connects but TLS fails".into(),
        proxy_env: vec!["HTTPS_PROXY".into()],
        items: vec![NetCheckItem {
            name: "venue-rest".into(),
            target: "clob.example.com".into(),
            ok: false,
            status: "tls_error".into(),
            addrs: vec!["1.2.3.4".into()],
            fake_ip: false,
            ms: 42,
            detail: "TLS alert at handshake".into(),
        }],
    };
    assert_round_trip(
        &report,
        concat!(
            r#"{"ok":false,"tsMs":1000000000000,"hintCode":"tls_blocked","#,
            r#""hint":"TCP connects but TLS fails","proxyEnv":["HTTPS_PROXY"],"items":["#,
            r#"{"name":"venue-rest","target":"clob.example.com","ok":false,"status":"tls_error","#,
            r#""addrs":["1.2.3.4"],"fakeIp":false,"ms":42,"detail":"TLS alert at handshake"}]}"#
        ),
    );
}

// ── settlement / redemption / merge family ──────────────────────────────────

#[test]
fn settlement_query_and_resolution_golden() {
    let q = SettlementQuery {
        condition_id: "cond-1".into(),
        token_ids: vec!["tok-up".into()],
        expires_at_ms: TS,
    };
    assert_round_trip(
        &q,
        r#"{"conditionId":"cond-1","tokenIds":["tok-up"],"expiresAtMs":1000000000000}"#,
    );

    let r = MarketResolution {
        condition_id: "cond-1".into(),
        resolved: true,
        payouts: vec![("tok-up".into(), d("1")), ("tok-down".into(), d("0"))],
        neg_risk: false,
        source: "gamma".into(),
    };
    // payouts is a plain Vec<(String, Decimal)> → the decimal is a STRING.
    assert_round_trip(
        &r,
        concat!(
            r#"{"conditionId":"cond-1","resolved":true,"payouts":[["tok-up","1"],["tok-down","0"]],"#,
            r#""negRisk":false,"source":"gamma"}"#
        ),
    );
}

#[test]
fn redemption_family_golden() {
    let req = RedemptionRequest {
        id: "cond-1".into(),
        condition_id: "cond-1".into(),
        neg_risk: false,
        outcome_shares: vec![d("10.5"), d("0")],
        expected_payout_usd: d("5.25"),
        winning_token_id: "tok-up".into(),
        venue: "polymarket".into(),
    };
    // outcome_shares (plain Vec<Decimal>) = strings; expected_payout_usd
    // (#[serde(with = "decimal")]) = number.
    assert_round_trip(
        &req,
        concat!(
            r#"{"id":"cond-1","conditionId":"cond-1","negRisk":false,"outcomeShares":["10.5","0"],"#,
            r#""expectedPayoutUsd":5.25,"winningTokenId":"tok-up","venue":"polymarket"}"#
        ),
    );

    let failure = RedemptionResult {
        id: "cond-1".into(),
        condition_id: "cond-1".into(),
        tx_hash: None,
        block_number: None,
        failure: Some(RedemptionFailure {
            message: "not resolved".into(),
            manual: true,
        }),
        at_ms: TS,
    };
    assert_round_trip(
        &failure,
        concat!(
            r#"{"id":"cond-1","conditionId":"cond-1","failure":{"message":"not resolved","#,
            r#""manual":true},"atMs":1000000000000}"#
        ),
    );
}

#[test]
fn merge_family_golden() {
    let req = MergeRequest {
        id: "cond-1#merge".into(),
        condition_id: "cond-1".into(),
        neg_risk: true,
        pairs_usd: d("10"),
        venue: "".into(),
    };
    assert_round_trip(
        &req,
        concat!(
            r#"{"id":"cond-1#merge","conditionId":"cond-1","negRisk":true,"pairsUsd":10.0,"#,
            r#""venue":""}"#
        ),
    );

    let res = MergeResult::success(&req, "0xdef", Some(7), TS);
    assert_round_trip(
        &res,
        r#"{"id":"cond-1#merge","conditionId":"cond-1","txHash":"0xdef","blockNumber":7,"atMs":1000000000000}"#,
    );
}

// ── error / account / introspection family ──────────────────────────────────

#[test]
fn core_error_and_account_id_golden() {
    let err = CoreError::new(CoreErrorCode::InsufficientFunds, "short by 5 USDC");
    assert_round_trip(
        &err,
        r#"{"code":"INSUFFICIENT_FUNDS","message":"short by 5 USDC","raw":null}"#,
    );
    let with_raw = err.clone().with_raw("venue said no");
    assert_round_trip(
        &with_raw,
        r#"{"code":"INSUFFICIENT_FUNDS","message":"short by 5 USDC","raw":"venue said no"}"#,
    );

    // AccountId is serde-transparent: the wire sees a bare string.
    let acct = AccountId("default".to_string());
    assert_eq!(wire(&acct), r#""default""#);
}

#[test]
fn plugin_info_and_market_mode_golden() {
    let info = PluginInfo {
        name: "polymarket".into(),
        supported_round_secs: None,
        feed_source: None,
        market_type: MarketType::Prediction,
        has_data_feed: true,
        has_discovery: true,
        has_executor: true,
        enabled: true,
        active: true,
        structure: Some(MarketStructure::BinaryOutcomeWheel),
        capabilities: MarketCapabilities::WEBSOCKET_FEED | MarketCapabilities::LEVEL2_SNAPSHOT,
    };
    // supportedRoundSecs/feedSource are skip_serializing_if Options → vanish.
    assert_round_trip(
        &info,
        concat!(
            r#"{"name":"polymarket","marketType":"prediction","hasDataFeed":true,"hasDiscovery":true,"hasExecutor":true,"enabled":true,"active":true,"#,
            r#""structure":"binary_outcome_wheel","capabilities":3}"#
        ),
    );

    let mode = MarketMode {
        market_type: MarketType::Prediction,
        structure: Some(MarketStructure::BinaryOutcomeWheel),
        capabilities: MarketCapabilities::WEBSOCKET_FEED | MarketCapabilities::LEVEL2_SNAPSHOT,
    };
    assert_round_trip(
        &mode,
        r#"{"marketType":"prediction","structure":"binary_outcome_wheel","capabilities":3}"#,
    );
}

// ── kline ────────────────────────────────────────────────────────────────────

#[test]
fn kline_golden_and_derivation() {
    let mut k = Kline::new(
        "BTC",
        KlineInterval::Min1,
        1_000_000_000_000,
        d("0.5"),
        d("10"),
    );
    assert_eq!(k.close_time_ms, 1_000_000_000_000 + 60_000 - 1);
    k.apply_trade(d("0.55"), d("0.5"));
    k.apply_trade(d("0.45"), d("0.5"));
    k.is_closed = true;
    assert_round_trip(
        &k,
        concat!(
            r#"{"symbol":"BTC","interval":"min1","openTimeMs":1000000000000,"closeTimeMs":1000000059999,"#,
            r#""open":0.5,"high":0.55,"low":0.45,"close":0.45,"volume":11.0,"tradeCount":3,"isClosed":true}"#
        ),
    );
    assert_eq!(
        KlineInterval::Min1.bucket_open_ms(1_000_005_999),
        999_960_000
    );
}

// ── capability bitmap ───────────────────────────────────────────────────────

#[test]
fn capability_bitmap_is_frozen() {
    use blitzkrieg_market_api::modes::{CAPABILITY_NAMES, capabilities_to_names, capability_bit};
    let expected: [(u64, &str); 11] = [
        (1 << 0, "websocket_feed"),
        (1 << 1, "level2_snapshot"),
        (1 << 2, "kline_stream"),
        (1 << 3, "trade_stream"),
        (1 << 4, "leverage"),
        (1 << 5, "short_selling"),
        (1 << 6, "batch_orders"),
        (1 << 7, "post_only"),
        (1 << 8, "cancel_on_disconnect"),
        (1 << 9, "maker_rebate"),
        (1 << 10, "simulated"),
    ];
    assert_eq!(CAPABILITY_NAMES, expected, "bit order / name table frozen");
    for (bit, name) in expected {
        assert_eq!(capability_bit(name), Some(MarketCapabilities(bit)));
    }
    assert_eq!(capability_bit("time_travel"), None);

    // Transparent serde: the wire sees a bare number.
    assert_eq!(wire(&MarketCapabilities::NONE), "0");
    assert_eq!(wire(&MarketCapabilities::SIMULATED), "1024");
    let both = MarketCapabilities::WEBSOCKET_FEED | MarketCapabilities::SIMULATED;
    assert_eq!(wire(&both), "1025");

    // Superset semantics, one direction only.
    assert!(both.satisfies(MarketCapabilities::WEBSOCKET_FEED));
    assert!(both.satisfies(MarketCapabilities::NONE));
    assert!(!MarketCapabilities::WEBSOCKET_FEED.satisfies(both));

    // Bit-order names (the §7.2 one-table rule).
    assert_eq!(
        capabilities_to_names(both),
        vec!["websocket_feed", "simulated"]
    );
    assert_eq!(
        capabilities_to_names(MarketCapabilities::NONE),
        Vec::<&str>::new()
    );
}
