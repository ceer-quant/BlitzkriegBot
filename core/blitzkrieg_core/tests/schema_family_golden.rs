//! #840 freeze wave — JSON-RPC schema per-family wire golden (§8.2 of the PR1
//! plan, comment 6558972782; approved for PR1 in 6560170852 ②, landed with the
//! freeze wave). One representative type per family, 22 families covering the
//! whole `core::schema` surface (118 pub types):
//!
//! Every family with a `Serialize` side is locked BYTE-EXACTLY
//! (`serde_json::to_string` must equal the literal — field order included);
//! every family with a `Deserialize` side is locked through a parse that must
//! land on the spelled-out Rust value (camelCase mapping, decimal-as-string,
//! `deny_unknown_fields` where declared). Types carrying both derives are
//! round-tripped in BOTH directions.
//!
//! This file is the freeze mechanism for the JSON-RPC domain: `core::schema`
//! stays the canonical owner (see the ownership ruling in `schema/mod.rs`),
//! and any wire drift here is a `[CONTRACT-BREAKING]` change by definition
//! once the freeze commit lands.
//!
//! Family map (§8.2): 1 transport `Request` · 2 blueprint `BlueprintCompileParams`
//! · 3 risk `RiskLimitsResult` · 4 kline `KlineSubscribeParams` · 5 order
//! `PlaceParams` · 6 position `PositionExitParams` · 7 balance `BalanceResult`
//! · 8 strategy-params `StrategyParamsGetParams` · 9 round-duration
//! `RoundDurationResult` · 10 unified-mapping `UnifiedConfirmParams` ·
//! 11 market-plugin `MarketPluginReadout` · 12 credentials `PluginCredentialRow`
//! · 13 reset `ResetTradingParams` · 14 fee-quote `FeeQuoteResult` · 15 book
//! `BookLevel` · 16 reconcile `ReconcileParams` · 17 execution-policy
//! `ExecutionPolicySetParams` · 18 account `AccountView` · 19 arb-config
//! `ArbConfigSetParams` · 20 journal `AccountJournalParams` · 21 event-stream
//! `Event` · 22 intent-audit `IntentAuditTailParams`.

use blitzkrieg_core::risk::limits::{AccountRiskLimits, Bound, LimitSource};
use blitzkrieg_core::schema::{
    AccountJournalParams, AccountView, ArbConfigSetParams, BalanceResult, BlueprintCompileParams,
    BookLevel, Event, ExecutionPolicySetParams, FeeQuoteResult, IntentAuditTailParams, JSONRPC,
    KlineSubscribeParams, MarketPluginOption, MarketPluginReadout, PROTOCOL_VERSION, PlaceParams,
    PluginCredentialRow, PositionExitParams, ReconcileParams, Request, ResetTradingParams,
    RiskAccountLimitsView, RiskExitView, RiskLimitsResult, RoundDurationOption,
    RoundDurationResult, StrategyParamsGetParams, UnifiedConfirmParams,
};
use blitzkrieg_market_api::unified::{ListingStatus, SettlementSource, Venue};
use blitzkrieg_market_api::{
    AccountId, CoreErrorCode, FillPolicy, KlineInterval, MarketType, Side,
};
use rust_decimal::Decimal;
use std::str::FromStr;

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).expect("golden decimal")
}

fn wire<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("golden serialize")
}

// ── 1. transport — Request (Serialize + Deserialize: both directions) ───────

#[test]
fn family_01_transport_request() {
    const FIXTURE: &str =
        r#"{"jsonrpc":"2.0","version":"1.1","id":7,"method":"engine.stats","params":{}}"#;
    let req: Request = serde_json::from_str(FIXTURE).expect("parse envelope");
    assert_eq!(req.jsonrpc, JSONRPC);
    assert_eq!(req.version, PROTOCOL_VERSION);
    assert_eq!(req.method, "engine.stats");
    // Byte-exact both ways: the envelope is the one type every message wears.
    assert_eq!(wire(&req), FIXTURE);
}

// ── 2. blueprint — BlueprintCompileParams (Deserialize) ─────────────────────

#[test]
fn family_02_blueprint_compile_params() {
    let p: BlueprintCompileParams =
        serde_json::from_str(r#"{"json":"strategy lua body"}"#).expect("parse");
    assert_eq!(p.json, "strategy lua body");
}

// ── 3. risk — RiskLimitsResult (Serialize) ──────────────────────────────────

#[test]
fn family_03_risk_limits_result() {
    let v = RiskLimitsResult {
        version: "1.1".into(),
        account: RiskAccountLimitsView {
            id: "default".into(),
            limits: AccountRiskLimits {
                max_single_loss_usd: Bound::new(d("35"), LimitSource::Env),
                max_daily_drawdown_usd: Bound::off(),
                max_position_size: Bound::new(d("500"), LimitSource::Flag),
                max_consecutive_losses: Bound::off(),
                cooldown_minutes: Bound::off(),
            },
        },
        exit: RiskExitView {
            stop_loss_pct: d("0.12"),
            take_profit_pct: d("0.24"),
            force_exit_sec: 300,
            trailing_enabled: true,
            holder_stop_pct: d("0.12"),
            strategy_exit_maker_first: true,
        },
    };
    // §4.4: a Bound crosses the wire as {"value":"<decimal string>","source":
    // "<provenance>"}; Decimal-as-string everywhere else (camelCase).
    assert_eq!(
        wire(&v),
        r#"{"version":"1.1","account":{"id":"default","limits":{"maxSingleLossUsd":{"value":"35","source":"env"},"maxDailyDrawdownUsd":{"value":"0","source":"default"},"maxPositionSize":{"value":"500","source":"flag"},"maxConsecutiveLosses":{"value":"0","source":"default"},"cooldownMinutes":{"value":"0","source":"default"}}},"exit":{"stopLossPct":0.12,"takeProfitPct":0.24,"forceExitSec":300,"trailingEnabled":true,"holderStopPct":0.12,"strategyExitMakerFirst":true}}"#
    );
}

// ── 4. kline — KlineSubscribeParams (Deserialize) ───────────────────────────

#[test]
fn family_04_kline_subscribe_params() {
    let p: KlineSubscribeParams =
        serde_json::from_str(r#"{"symbols":["BTC-USD"],"intervals":["min1","hour4"]}"#)
            .expect("parse");
    assert_eq!(p.symbols, vec!["BTC-USD".to_owned()]);
    assert_eq!(p.intervals, vec![KlineInterval::Min1, KlineInterval::Hour4]);
    // Defaults: both fields are `#[serde(default)]` — the empty subscribe.
    let empty: KlineSubscribeParams = serde_json::from_str(r#"{}"#).expect("parse");
    assert!(empty.symbols.is_empty() && empty.intervals.is_empty());
}

// ── 5. order — PlaceParams (Deserialize; `order` flattened) ─────────────────

#[test]
fn family_05_order_place_params() {
    // The flatten is the wire contract: an OrderRequest's camelCase fields AT
    // TOP LEVEL, plus makerTimeoutMs.
    let p: PlaceParams = serde_json::from_str(
        r#"{"tokenId":"tok-1","conditionId":"0xc","side":"buy","mode":"taker","price":"0.42","size":"10","internalKey":"k","strategy":"s","asset":"BTC","direction":"up","roundSlot":7,"accountId":"default","makerTimeoutMs":90000}"#,
    )
    .expect("parse");
    assert_eq!(p.maker_timeout_ms, 90_000);
    // OrderRequest carries no PartialEq — the field-by-field lock below IS
    // the camelCase/decimal-as-string mapping check.
    let o = &p.order;
    assert_eq!(o.token_id, "tok-1");
    assert_eq!(o.condition_id, "0xc");
    assert_eq!(o.side, Side::Buy);
    assert_eq!(o.mode, FillPolicy::Taker);
    assert_eq!(o.price, d("0.42"));
    assert_eq!(o.size, d("10"));
    assert_eq!(o.internal_key, "k");
    assert_eq!(o.strategy, "s");
    assert_eq!(o.asset, "BTC");
    assert_eq!(o.direction, "up");
    assert_eq!(o.round_slot, 7);
    assert_eq!(o.account_id, AccountId("default".into()));
}

// ── 6. position — PositionExitParams (Deserialize, deny_unknown_fields) ─────

#[test]
fn family_06_position_exit_params() {
    let p: PositionExitParams = serde_json::from_str(r#"{"positionId":"pos-1"}"#).expect("parse");
    assert_eq!(p.position_id, Some("pos-1".to_owned()));
    // Omitted field flattens every open position — the documented spelling.
    let all: PositionExitParams = serde_json::from_str(r#"{}"#).expect("parse");
    assert_eq!(all.position_id, None);
    // deny_unknown_fields: a typo'd key is refused, not silently ignored.
    assert!(serde_json::from_str::<PositionExitParams>(r#"{"positionId":"x","extra":1}"#).is_err());
}

// ── 7. balance — BalanceResult (Serialize) ──────────────────────────────────

#[test]
fn family_07_balance_result() {
    let v = BalanceResult {
        balance: d("100"),
        reserved: d("5"),
        available: d("95"),
        seed: None,
        opening_equity: None,
        cumulative_net_pnl: None,
    };
    // `seed` absent in LIVE (skip_serializing_if) — the reader treats it as
    // "unknown", not zero.
    assert_eq!(
        wire(&v),
        r#"{"balance":100.0,"reserved":5.0,"available":95.0}"#
    );
    // `seed` is a plain Option<Decimal> (no wire helper) — rust_decimal's own
    // serde spells it a string; the freeze pins that asymmetry as-is.
    let with_seed = BalanceResult {
        seed: Some(d("80")),
        ..v
    };
    assert_eq!(
        wire(&with_seed),
        r#"{"balance":100.0,"reserved":5.0,"available":95.0,"seed":"80"}"#
    );
    // issue 1046: cumulative net PnL = balance + reserved − opening_equity;
    // DRY opening equity == seed → 100 + 5 − 80 = 25.
    let with_cum = BalanceResult {
        opening_equity: Some(d("80")),
        cumulative_net_pnl: Some(d("25")),
        ..with_seed
    };
    assert_eq!(
        wire(&with_cum),
        r#"{"balance":100.0,"reserved":5.0,"available":95.0,"seed":"80","openingEquity":"80","cumulativeNetPnl":"25"}"#
    );
    // cumulative_net_pnl absent when opening_equity unknown → reader treats
    // as "unknown", never zero.
    let no_cum = BalanceResult { ..with_seed };
    assert_eq!(
        wire(&no_cum),
        r#"{"balance":100.0,"reserved":5.0,"available":95.0,"seed":"80"}"#
    );
}

// ── 8. strategy-params — StrategyParamsGetParams (Deserialize) ──────────────

#[test]
fn family_08_strategy_params_get_params() {
    let p: StrategyParamsGetParams = serde_json::from_str(r#"{"name":"mad_dog"}"#).expect("parse");
    assert_eq!(p.name, "mad_dog");
}

// ── 9. round-duration — RoundDurationResult (Serialize) ─────────────────────

#[test]
fn family_09_round_duration_result() {
    let v = RoundDurationResult {
        effective_sec: 900,
        source: "cli",
        allowed: vec![
            RoundDurationOption {
                sec: 300,
                label: Some("5m"),
            },
            RoundDurationOption {
                sec: 900,
                label: None,
            },
        ],
        next_session_sec: 900,
        persisted: false,
    };
    // `label` is skipped when None — a non-standard value renders unnamed.
    assert_eq!(
        wire(&v),
        r#"{"effectiveSec":900,"source":"cli","allowed":[{"sec":300,"label":"5m"},{"sec":900}],"nextSessionSec":900,"persisted":false}"#
    );
}

// ── 10. unified-mapping — UnifiedConfirmParams (Deserialize) ────────────────

#[test]
fn family_10_unified_confirm_params() {
    let p: UnifiedConfirmParams = serde_json::from_str(
        r#"{"title":"Will BTC close above 100k?","listings":[{"venue":"polymarket","conditionId":"0xc","upTokenId":"up","downTokenId":"down","asset":"bitcoin","title":"BTC up","settlementRules":"expiry","settlementSource":"chainlink","expiresAtMs":1760000000000,"status":"active"}],"actor":"panel"}"#,
    )
    .expect("parse");
    assert_eq!(p.title, "Will BTC close above 100k?");
    assert_eq!(p.actor, Some("panel".to_owned()));
    let l = &p.listings[0];
    assert_eq!(l.venue, Venue::Polymarket);
    assert_eq!(l.condition_id, "0xc");
    assert_eq!(l.settlement_source, SettlementSource::Chainlink);
    assert_eq!(l.status, ListingStatus::Active);
}

// ── 11. market-plugin — MarketPluginReadout (Serialize) ─────────────────────

#[test]
fn family_11_market_plugin_readout() {
    let v = MarketPluginReadout {
        active_plugin: "polymarket".into(),
        registered: vec![MarketPluginOption {
            name: "polymarket".into(),
            kind: "prediction".into(),
            active: true,
        }],
        next_session_plugin: "polymarket".into(),
        persisted: false,
        flag_pinned: true,
        source: "flag",
        derived_venues: vec![],
    };
    // `kind` rides the wire as `type` (rename).
    assert_eq!(
        wire(&v),
        r#"{"activePlugin":"polymarket","registered":[{"name":"polymarket","type":"prediction","active":true}],"nextSessionPlugin":"polymarket","persisted":false,"flagPinned":true,"source":"flag","derivedVenues":[]}"#
    );
}

// ── 12. credentials — PluginCredentialRow (Serialize) ───────────────────────

#[test]
fn family_12_credentials_plugin_credential_row() {
    let v = PluginCredentialRow {
        plugin: "polymarket".into(),
        required_vars: vec!["POLY_PK".into()],
        resolved_vars: vec!["POLY_PK".into()],
        ready: true,
    };
    // Names only — values never travel back over IPC (§9.4).
    assert_eq!(
        wire(&v),
        r#"{"plugin":"polymarket","requiredVars":["POLY_PK"],"resolvedVars":["POLY_PK"],"ready":true}"#
    );
}

// ── 13. reset — ResetTradingParams (Deserialize) ────────────────────────────

#[test]
fn family_13_reset_trading_params() {
    let p: ResetTradingParams =
        serde_json::from_str(r#"{"confirm":"RESET","reason":"ops","accountId":"default"}"#)
            .expect("parse");
    assert_eq!(p.confirm, "RESET");
    assert_eq!(p.reason, Some("ops".to_owned()));
    assert_eq!(p.account_id, Some("default".to_owned()));
    // All-optional except confirm: the session-default spelling.
    let bare: ResetTradingParams = serde_json::from_str(r#"{"confirm":"RESET"}"#).expect("parse");
    assert_eq!(bare.reason, None);
    assert_eq!(bare.account_id, None);
}

// ── 14. fee-quote — FeeQuoteResult (Serialize) ──────────────────────────────

#[test]
fn family_14_fee_quote_result() {
    let v = FeeQuoteResult {
        model: "legacy_quadratic".into(),
        rate: d("0.0004"),
        exponent: 2,
        price: d("0.42"),
        fee_per_share: d("0.00007"),
        fee_pct_of_price: d("0.0166"),
        maker_fee_per_share: d("0"),
        model_matches: true,
    };
    assert_eq!(
        wire(&v),
        r#"{"model":"legacy_quadratic","rate":0.0004,"exponent":2,"price":0.42,"feePerShare":0.00007,"feePctOfPrice":0.0166,"makerFeePerShare":0.0,"modelMatches":true}"#
    );
}

// ── 15. book — BookLevel (Deserialize) ──────────────────────────────────────

#[test]
fn family_15_book_level() {
    let l: BookLevel = serde_json::from_str(r#"{"price":"0.42","size":"100"}"#).expect("parse");
    assert_eq!(l.price, d("0.42"));
    assert_eq!(l.size, d("100"));
}

// ── 16. reconcile — ReconcileParams + ReconcileTrade (Deserialize) ──────────

#[test]
fn family_16_reconcile_params() {
    let p: ReconcileParams = serde_json::from_str(
        r#"{"openOrderIds":["ord-1"],"trades":[{"venueOrderId":"v1","tradeId":"t1","tokenId":"tok-1","side":"buy","size":"10","price":"0.42","tsMs":1760000000000}]}"#,
    )
    .expect("parse");
    assert_eq!(p.open_order_ids, vec!["ord-1".to_owned()]);
    let t = &p.trades[0];
    assert_eq!(t.venue_order_id, "v1");
    assert_eq!(t.trade_id, "t1");
    assert_eq!(t.token_id, "tok-1");
    assert_eq!(t.side, Side::Buy);
    assert_eq!(t.size, d("10"));
    assert_eq!(t.price, d("0.42"));
    assert_eq!(t.ts_ms, 1_760_000_000_000);
    assert_eq!(t.maker, None);
    // Defaults: both top-level fields — the quiet-sweep spelling.
    let bare: ReconcileParams = serde_json::from_str(r#"{}"#).expect("parse");
    assert!(bare.open_order_ids.is_empty() && bare.trades.is_empty());
}

// ── 17. execution-policy — ExecutionPolicySetParams (Deserialize) ───────────

#[test]
fn family_17_execution_policy_set_params() {
    let p: ExecutionPolicySetParams = serde_json::from_str(
        r#"{"accountId":"default","budgetRatio":"0.5","minBudgetUsd":"10","maxBudgetUsd":"50","minEquityUsd":"20","maxPositionsPerAsset":3,"rules":[{"op":"cap"}]}"#,
    )
    .expect("parse");
    assert_eq!(p.account_id, "default");
    assert_eq!(p.budget_ratio, Some("0.5".to_owned()));
    assert_eq!(p.min_budget_usd, Some("10".to_owned()));
    assert_eq!(p.max_budget_usd, Some("50".to_owned()));
    assert_eq!(p.min_equity_usd, Some("20".to_owned()));
    assert_eq!(p.max_positions_per_asset, Some(3));
    assert_eq!(p.rules.as_ref().map(Vec::len), Some(1));
    // deny_unknown_fields: a typo'd key is refused.
    assert!(
        serde_json::from_str::<ExecutionPolicySetParams>(r#"{"accountId":"a","typo":1}"#).is_err()
    );
}

// ── 18. account — AccountView (Serialize) ───────────────────────────────────

#[test]
fn family_18_account_view() {
    let v = AccountView {
        id: "acc-1".into(),
        name: "main".into(),
        market_type: MarketType::Prediction,
        status: "active".into(),
        status_reason: None,
        balance: d("100"),
        available: d("95"),
        reserved: d("5"),
        credentials_loaded: true,
        venue: "polymarket".into(),
        trading_enabled: true,
        strategies: vec!["mad_dog".into()],
        rounds: vec![900],
        open_positions: 2,
        day_realized_usd: d("1.25"),
        // 现金真值锚点（操作员裁决 2026-11-08）：累计 Seed 入金。
        seeded_total: d("100"),
        updated_at_ms: 1_760_000_000_000,
    };
    // `status` is a plain wire tag string (§12.1) and `statusReason` only
    // appears for suspensions — a mid-list map flip is what the plain string
    // exists to prevent.
    assert_eq!(
        wire(&v),
        r#"{"id":"acc-1","name":"main","marketType":"prediction","status":"active","balance":100.0,"available":95.0,"reserved":5.0,"credentialsLoaded":true,"venue":"polymarket","tradingEnabled":true,"strategies":["mad_dog"],"rounds":[900],"openPositions":2,"dayRealizedUsd":1.25,"seededTotal":100.0,"updatedAtMs":1760000000000}"#
    );
}

// ── 19. arb-config — ArbConfigSetParams (Deserialize) ───────────────────────

#[test]
fn family_19_arb_config_set_params() {
    let p: ArbConfigSetParams = serde_json::from_str(
        r#"{"version":1,"legAVenue":"polymarket","legAAccount":"a1","legBVenue":"kalshi","legBAccount":"a2","pairs":["BTC"],"actor":"panel"}"#,
    )
    .expect("parse");
    assert_eq!(p.version, 1);
    assert_eq!(p.leg_a_venue, "polymarket");
    assert_eq!(p.leg_a_account, "a1");
    assert_eq!(p.leg_b_venue, "kalshi");
    assert_eq!(p.leg_b_account, "a2");
    assert_eq!(p.pairs, vec!["BTC".to_owned()]);
    assert_eq!(p.actor, Some("panel".to_owned()));
}

// ── 20. journal — AccountJournalParams (Deserialize) ────────────────────────

#[test]
fn family_20_account_journal_params() {
    let p: AccountJournalParams =
        serde_json::from_str(r#"{"accountId":"default","limit":50}"#).expect("parse");
    assert_eq!(p.account_id, Some("default".to_owned()));
    assert_eq!(p.limit, Some(50));
}

// ── 21. event-stream — Event (Serialize; tag = kind, SCREAMING_SNAKE) ───────

#[test]
fn family_21_event_stream_risk_alert() {
    // The event stream's canonical enum; the small variant locks the tag
    // convention (`kind` + SCREAMING_SNAKE_CASE) and the error-code spelling
    // without duplicating the large variants already locked by behaviour
    // tests (ipc/server) and the Evolution golden surfaces.
    let v = Event::RiskAlert {
        code: CoreErrorCode::RiskRejected,
        message: "gate 2 refused".into(),
    };
    assert_eq!(
        wire(&v),
        r#"{"kind":"RISK_ALERT","code":"RISK_REJECTED","message":"gate 2 refused"}"#
    );
}

// ── 22. intent-audit — IntentAuditTailParams (Deserialize) ──────────────────

#[test]
fn family_22_intent_audit_tail_params() {
    let p: IntentAuditTailParams = serde_json::from_str(
        r#"{"limit":25,"accountId":"default","strategy":"mad_dog","decision":"approved"}"#,
    )
    .expect("parse");
    assert_eq!(p.limit, Some(25));
    assert_eq!(p.account_id, Some("default".to_owned()));
    assert_eq!(p.strategy, Some("mad_dog".to_owned()));
    assert_eq!(p.decision, Some("approved".to_owned()));
    // Defaults: a bare tail is the newest 50 across everything.
    let bare: IntentAuditTailParams = serde_json::from_str(r#"{}"#).expect("parse");
    assert!(bare.limit.is_none() && bare.decision.is_none());
}

// ── 23. unverified-exposure — UnverifiedExposureReport (Serialize; issue 991,
// additive) ───────────────────────────────────────────────────────────────────

#[test]
fn family_23_unverified_exposure_report() {
    use blitzkrieg_core::position::{UnverifiedExposureEntry, UnverifiedExposureReport};
    let v = UnverifiedExposureReport {
        entries: vec![UnverifiedExposureEntry {
            position_id: "hft-7".into(),
            condition_id: "cond".into(),
            token_id: "tok".into(),
            account_id: AccountId("default".into()),
            strategy: "spread_arb".into(),
            shares: d("5"),
            cost_usd: d("2"),
            fallback_price: d("0.40"),
            original_fallback_price: Some(d("0.40")),
            // Issue 991: "reconciled" = venue cash arrived, price still the
            // estimate. serde(default) keeps pre-991 UI payloads loadable.
            source: "reconciled".into(),
            net_pnl_usd: d("-2"),
            exited_at_ms: 1_758_888_010_000,
            percentage_of_total_cost: d("12.5"),
        }],
        total_positions: 1,
        total_shares: d("5"),
        total_cost_usd: d("2"),
        total_percentage_of_closed_cost: d("12.5"),
    };
    // The aggregates predate issue 991 and carry Decimal's default (string)
    // serde — their shape is FROZEN as-is (additive-only discipline). The
    // entry-level decimals ride crate::decimal (number-or-string). `source`
    // is issue 991's additive field, serde(default).
    assert_eq!(
        wire(&v),
        r#"{"entries":[{"positionId":"hft-7","conditionId":"cond","tokenId":"tok","accountId":"default","strategy":"spread_arb","shares":5.0,"costUsd":2.0,"fallbackPrice":0.4,"originalFallbackPrice":0.4,"source":"reconciled","netPnlUsd":-2.0,"exitedAtMs":1758888010000,"percentageOfTotalCost":12.5}],"totalPositions":1,"totalShares":"5","totalCostUsd":"2","totalPercentageOfClosedCost":"12.5"}"#
    );
}
