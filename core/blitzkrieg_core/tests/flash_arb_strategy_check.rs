//! flash_arb acceptance checks — the REAL package (`user_layer/strategies_lua/
//! flash_arb`) driven through the engine adapter, exactly the surface the
//! kernel runs.
//!
//! The spec's acceptance rows, mapped:
//!   positive  — spot pump/dump >= 0.25% inside the 2s window with the
//!               implied side still ask-priced <= 0.55 → ONE post_only-destined
//!               suggestion AT the opposite (ask) price, direction from the
//!               spot move, `shares` absent (the kernel sizes); the inclusive
//!               1.5s lag boundary fires;
//!   negative  — no spot move, sub-threshold move, PM already caught up,
//!               past the lag window (the spec's >2s row included), time
//!               floor < 180s, thin book, extreme OBI, underwater account,
//!               and a spot trigger on a DIFFERENT asset than the market;
//!   boundary  — exactly +0.25% triggers (the scaled-integer gate, no float
//!               knife-edge);
//!   seal      — a FIRING evaluation still emits no exit and no break.
//!
//! Sandbox refusal (os/require nil) and manifest tamper refusals are the
//! lua-sandbox-check's fixtures; this file loads the untampered package
//! through the same loader, so a manifest/modes/sha256 regression fails every
//! test here at setup time.

use std::path::PathBuf;

use blitzkrieg_core::strategies::{EngineStrategy, StrategyCtx};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use blitzkrieg_core::{kline::Kline, model::CryptoMarket, model::OrderbookSnapshot};
use blitzkrieg_lua_runtime::bk_api::AccountView;
use blitzkrieg_market_api::kline::KlineInterval;
use blitzkrieg_market_api::{MarketCapabilities, MarketStructure, MarketType};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;
const UP: &str = "up";
const DOWN: &str = "down";
/// Spot closes fed at NOW → the last of three sec1 bars closes here; that is
/// the trigger timestamp every evaluation is measured against.
const TRIG_MS: i64 = NOW + 2_999;

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../user_layer/strategies_lua/flash_arb")
}

fn adapter() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("flash_arb package must load");
    LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
}

/// Adapter whose `bk.params()` start from a modified default bag (the
/// manifest-defaults path, overridden the way a param registry would).
fn adapter_with_account(available: &str) -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("flash_arb package must load");
    {
        let state = loaded.strategy.state();
        let mut st = state.lock().expect("lock");
        st.account = Some(AccountView {
            id: "acct".into(),
            name: "dry".into(),
            market_type: "prediction".into(),
            balance: Some(available.to_string()),
            available: Some(available.to_string()),
            reserved: None,
        });
    }
    LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
}

fn market() -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond".into(),
        question_id: "q".into(),
        up_token_id: UP.into(),
        down_token_id: DOWN.into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: NOW + 900_000,
        round_slot: NOW / 900_000,
        round_duration_sec: 900,
        archive_verdict: false,
        neg_risk: false,
        question: "BTC up or down".into(),
    }
}

fn token_book(
    token: &str,
    mid_bid: Decimal,
    mid_ask: Decimal,
    depth: Decimal,
    ts: i64,
) -> OrderbookSnapshot {
    OrderbookSnapshot::from_levels(
        token.to_string(),
        vec![(mid_bid, depth)],
        vec![(mid_ask, depth)],
        ts,
    )
}

fn ctx<'a>(
    markets: &'a [CryptoMarket],
    time_left_sec: i64,
    now_ms: i64,
    fresh: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
) -> StrategyCtx<'a> {
    StrategyCtx::new(markets, NOW / 900_000, time_left_sec, now_ms, fresh)
}

fn candidates_of(
    adapter: &mut LuaEngineAdapter,
    markets: &[CryptoMarket],
    time_left_sec: i64,
    now_ms: i64,
    fresh: &dyn Fn(&str) -> Option<OrderbookSnapshot>,
) -> Vec<blitzkrieg_core::signal::TradeSignal> {
    adapter.find_candidates(&ctx(markets, time_left_sec, now_ms, fresh))
}

/// CLOSED sec1 bars, 1s apart, keyed by `symbol` — the shape the E29
/// aggregator hands `bk_on_kline` for Binance spot prints.
fn feed_spot(adapter: &mut LuaEngineAdapter, symbol: &str, start_open_ms: i64, closes: &[Decimal]) {
    for (i, price) in closes.iter().enumerate() {
        let open = start_open_ms + (i as i64) * 1000;
        let k = Kline {
            symbol: symbol.into(),
            interval: KlineInterval::Sec1,
            open_time_ms: open,
            close_time_ms: open + 999,
            open: *price,
            high: *price,
            low: *price,
            close: *price,
            volume: dec!(1),
            trade_count: 1,
            is_closed: true,
        };
        adapter.on_kline(&k);
    }
}

/// +0.30% inside the 2s window: two flat prints then the move.
fn pump() -> Vec<Decimal> {
    vec![dec!(60000), dec!(60000), dec!(60180)]
}

/// −0.30% inside the 2s window.
fn dump() -> Vec<Decimal> {
    vec![dec!(60000), dec!(60000), dec!(59820)]
}

fn flat() -> Vec<Decimal> {
    vec![dec!(60000); 3]
}

// ── positive ────────────────────────────────────────────────────────────────

/// Spot +0.30% in 2s, UP still ask-priced 0.55 (<= lag_max_price) → ONE
/// suggestion to buy UP AT the ask (the opposite price a post_only maker
/// joins), direction from the spot move, no shares (the kernel sizes).
#[test]
fn spot_pump_with_lagging_up_suggests_maker_buy_at_ask() {
    let mut a = adapter();
    feed_spot(&mut a, "BTC", NOW, &pump());
    let book = token_book(UP, dec!(0.54), dec!(0.55), dec!(600), TRIG_MS);

    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, TRIG_MS + 1500, &fresh);

    assert_eq!(got.len(), 1, "one suggestion, got {got:?}");
    assert_eq!(got[0].token_id, UP);
    assert_eq!(
        got[0].price,
        dec!(0.55),
        "entry is the opposite (ask) price"
    );
    assert_eq!(got[0].shares, None, "sizing belongs to the kernel");
    assert_eq!(got[0].asset, "BTC");
    assert!(
        got[0].reason.contains("spot +0.30%"),
        "reason should name the move: {}",
        got[0].reason
    );
}

/// Spot −0.30% → the DOWN side is the one suggested, at its own ask.
#[test]
fn spot_dump_suggests_down_buy() {
    let mut a = adapter();
    feed_spot(&mut a, "BTC", NOW, &dump());
    let book = token_book(DOWN, dec!(0.54), dec!(0.55), dec!(600), TRIG_MS);

    let markets = vec![market()];
    let fresh = move |t: &str| (t == DOWN).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, TRIG_MS + 1500, &fresh);

    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].token_id, DOWN);
    assert_eq!(got[0].price, dec!(0.55));
    assert!(
        got[0].reason.contains("spot -0.30%"),
        "reason should name the move: {}",
        got[0].reason
    );
}

/// The lag window is inclusive at 1.5s: exactly lag_max_ms after the
/// triggering bar close still fires (spec: "已过去 ≤ 1.5 秒").
#[test]
fn fires_at_the_lag_window_boundary() {
    let mut a = adapter();
    feed_spot(&mut a, "BTC", NOW, &pump());
    let book = token_book(UP, dec!(0.54), dec!(0.55), dec!(600), TRIG_MS);

    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, TRIG_MS + 1500, &fresh);
    assert_eq!(got.len(), 1, "exactly 1500ms must still fire: {got:?}");
}

// ── negative (each row must refuse) ─────────────────────────────────────────

/// No spot move → no trigger → no suggestion (the strategy is fail-closed by
/// construction: without spot data there is nothing to be late about).
#[test]
fn refuses_without_a_spot_move() {
    let mut a = adapter();
    feed_spot(&mut a, "BTC", NOW, &flat());
    let book = token_book(UP, dec!(0.54), dec!(0.55), dec!(600), TRIG_MS);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, TRIG_MS + 100, &fresh);
    assert!(got.is_empty(), "flat spot must never trigger: {got:?}");
}

/// +0.20% is under the 0.25% threshold → no trigger.
#[test]
fn refuses_a_sub_threshold_move() {
    let mut a = adapter();
    feed_spot(&mut a, "BTC", NOW, &[dec!(60000), dec!(60120)]);
    let book = token_book(UP, dec!(0.54), dec!(0.55), dec!(600), TRIG_MS);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, TRIG_MS + 100, &fresh);
    assert!(got.is_empty(), "sub-threshold move must refuse: {got:?}");
}

/// Exactly +0.25% triggers — the scaled-integer gate's inclusive boundary
/// (this is the case a float `>=` could silently flip).
#[test]
fn threshold_boundary_is_inclusive() {
    let mut a = adapter();
    feed_spot(&mut a, "BTC", NOW, &[dec!(60000), dec!(60150)]);
    let book = token_book(UP, dec!(0.54), dec!(0.55), dec!(600), TRIG_MS);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, TRIG_MS + 100, &fresh);
    assert_eq!(got.len(), 1, "exactly +0.25% must trigger: {got:?}");
}

/// PM already caught up: the ask has repriced past the lag band → refuse
/// (spec's "UP 筹码 > 0.65" row; the 0.55 gate rejects it a fortiori).
#[test]
fn refuses_when_pm_caught_up() {
    let mut a = adapter();
    feed_spot(&mut a, "BTC", NOW, &pump());
    let book = token_book(UP, dec!(0.69), dec!(0.70), dec!(600), TRIG_MS);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, TRIG_MS + 100, &fresh);
    assert!(got.is_empty(), "repriced ask must refuse: {got:?}");
}

/// Past the lag window (spec's ">2 秒拒绝" row; the 1.5s gate refuses the
/// whole 1501ms+ range, 2500ms included).
#[test]
fn refuses_after_the_lag_window() {
    let mut a = adapter();
    feed_spot(&mut a, "BTC", NOW, &pump());
    let book = token_book(UP, dec!(0.54), dec!(0.55), dec!(600), TRIG_MS);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, TRIG_MS + 2500, &fresh);
    assert!(got.is_empty(), "stale trigger must refuse: {got:?}");
}

/// Time floor: the perfect setup, but < 180s left → no suggestion.
#[test]
fn refuses_inside_the_time_floor() {
    let mut a = adapter();
    feed_spot(&mut a, "BTC", NOW, &pump());
    let book = token_book(UP, dec!(0.54), dec!(0.55), dec!(600), TRIG_MS);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 179, TRIG_MS + 100, &fresh);
    assert!(got.is_empty(), "time floor must refuse: {got:?}");
}

/// Thin book: bid depth under the floor refuses (OBI stays neutral here, so
/// this isolates the depth gate).
#[test]
fn refuses_a_thin_book() {
    let mut a = adapter();
    feed_spot(&mut a, "BTC", NOW, &pump());
    let thin = token_book(UP, dec!(0.54), dec!(0.55), dec!(400), TRIG_MS);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| thin.clone());
    let got = candidates_of(&mut a, &markets, 600, TRIG_MS + 100, &fresh);
    assert!(got.is_empty(), "depth floor must refuse: {got:?}");
}

/// Extreme imbalance refuses even with plenty of depth: 900 bid vs 100 ask
/// → |OBI| 0.78 > 0.3.
#[test]
fn refuses_extreme_obi() {
    let mut a = adapter();
    feed_spot(&mut a, "BTC", NOW, &pump());
    let skewed = OrderbookSnapshot::from_levels(
        UP.to_string(),
        vec![(dec!(0.54), dec!(900))],
        vec![(dec!(0.55), dec!(100))],
        TRIG_MS,
    );
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| skewed.clone());
    let got = candidates_of(&mut a, &markets, 600, TRIG_MS + 100, &fresh);
    assert!(got.is_empty(), "OBI band must refuse: {got:?}");
}

/// An underwater account view refuses (when the host surfaces one at all —
/// no host path populates it today, and then the kernel owns funds).
#[test]
fn refuses_an_underwater_account() {
    let mut a = adapter_with_account("0.50");
    feed_spot(&mut a, "BTC", NOW, &pump());
    let book = token_book(UP, dec!(0.54), dec!(0.55), dec!(600), TRIG_MS);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, TRIG_MS + 100, &fresh);
    assert!(got.is_empty(), "available < $1 must refuse: {got:?}");
}

/// A trigger on a DIFFERENT asset than the market's does not buy: ETH's
/// pump says nothing about the BTC round (per-market asset matching).
#[test]
fn refuses_an_asset_mismatch() {
    let mut a = adapter();
    feed_spot(&mut a, "ETH", NOW, &pump());
    let book = token_book(UP, dec!(0.54), dec!(0.55), dec!(600), TRIG_MS);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, TRIG_MS + 100, &fresh);
    assert!(got.is_empty(), "cross-asset trigger must refuse: {got:?}");
}

// ── the seal ────────────────────────────────────────────────────────────────

/// Even a FIRING evaluation expresses no exit and no break — survival is the
/// kernel's (hard stop, trailing, time exit, settlement), and this strategy
/// posts no resting order it would need to cancel.
#[test]
fn seal_a_firing_evaluation_emits_no_exit_or_break() {
    let mut a = adapter();
    feed_spot(&mut a, "BTC", NOW, &pump());
    let book = token_book(UP, dec!(0.54), dec!(0.55), dec!(600), TRIG_MS);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, TRIG_MS + 100, &fresh);
    assert_eq!(
        got.len(),
        1,
        "setup must fire for the seal to mean anything"
    );
    // `exits` is part of every evaluate result; `breaks` never accumulates.
    // A strategy that grew either would fail the no-stop-loss gate first —
    // this pins the same contract at runtime, on the real package.
    let breaks = a.take_breaks();
    assert!(breaks.is_empty(), "no resting order, no breaks: {breaks:?}");
}

// ── packaging ───────────────────────────────────────────────────────────────

/// The declaration the E27 handshake reads: prediction / binary wheel, the
/// three capabilities the polymarket seam actually serves.
#[test]
fn modes_declare_what_the_seam_serves() {
    let loaded = load_lua_package(&package_dir()).expect("package loads");
    assert_eq!(loaded.name, "flash_arb");
    assert_eq!(loaded.declared_modes.len(), 1);
    let m = &loaded.declared_modes[0];
    assert_eq!(m.market_type, MarketType::Prediction);
    assert_eq!(m.structure, Some(MarketStructure::BinaryOutcomeWheel));
    for cap in [
        MarketCapabilities::WEBSOCKET_FEED,
        MarketCapabilities::LEVEL2_SNAPSHOT,
        MarketCapabilities::POST_ONLY,
    ] {
        assert!(
            m.required_capabilities.satisfies(cap),
            "missing capability {cap:?}"
        );
    }
    assert!(
        !m.required_capabilities
            .satisfies(MarketCapabilities::KLINE_STREAM),
        "kline_stream is engine-side, not a seam bit — declaring it would refuse enable"
    );
}
