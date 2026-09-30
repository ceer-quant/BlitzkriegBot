//! mad_dog acceptance checks — the REAL package (`user_layer/strategies_lua/
//! mad_dog`) driven through the engine adapter, exactly the surface the kernel
//! runs.
//!
//! The spec's acceptance rows, mapped:
//!   positive  — dominance (hold + mean spellings) + fast wick + calm
//!               underlying → ONE resting-bid suggestion 0.02 under the wick
//!               low, direction from the token's side, `shares` absent (the
//!               kernel sizes);
//!   negative  — time floor (< 180s), slow bleed (no panic speed), no
//!               dominance, thin book, extreme OBI, underwater account,
//!               underlying waterfall (and the disclosed `spot_missing=pass`
//!               diagnostic knob that re-arms the same scenario);
//!   ratchet   — a deeper wick moves the bid to the new extreme − offset;
//!   recovery  — the panic leg ending above the broken level cancels the
//!               stale bid (a `break`, the strategy's only non-entry row).
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

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../user_layer/strategies_lua/mad_dog")
}

fn adapter() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("mad_dog package must load");
    LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
}

/// Adapter whose `bk.params()` start from a modified default bag (the
/// manifest-defaults path, overridden the way a param registry would).
fn adapter_with_params(override_key: &str, override_value: &str) -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("mad_dog package must load");
    let mut tunables = loaded.tunables;
    tunables.insert(override_key.to_string(), override_value.to_string());
    LuaEngineAdapter::new(loaded.strategy, tunables)
}

/// Adapter whose account view is pre-populated (the host does not push one
/// today; the test exercises the gate for the day one does).
fn adapter_with_account(available: &str) -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("mad_dog package must load");
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
        neg_risk: false,
        question: "BTC up or down".into(),
    }
}

fn book(mid_bid: Decimal, mid_ask: Decimal, depth: Decimal, ts: i64) -> OrderbookSnapshot {
    OrderbookSnapshot::from_levels(
        UP.to_string(),
        vec![(mid_bid, depth)],
        vec![(mid_ask, depth)],
        ts,
    )
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

/// One CLOSED sec1 bar per price, 1s apart, keyed by the asset — the shape
/// the E29 aggregator hands `bk_on_kline` for Binance spot prints.
fn feed_spot(adapter: &mut LuaEngineAdapter, start_open_ms: i64, closes: &[Decimal]) {
    for (i, price) in closes.iter().enumerate() {
        let open = start_open_ms + (i as i64) * 1000;
        let k = Kline {
            symbol: "BTC".into(),
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

/// A confirmed dominant phase: `n` one-second prints at `mid` (two-sided
/// book, deep enough, OBI-neutral), oldest at `start_ms`.
fn feed_dominance(
    adapter: &mut LuaEngineAdapter,
    token: &str,
    start_ms: i64,
    n: usize,
    mid: Decimal,
) {
    for i in 0..n {
        let ts = start_ms + (i as i64) * 1000;
        let (bid, ask) = (mid - dec!(0.01), mid + dec!(0.01));
        adapter.on_book(token, &token_book(token, bid, ask, dec!(600), ts), ts);
    }
}

fn flat_spot(_start_open_ms: i64, n: usize) -> Vec<Decimal> {
    vec![dec!(60000); n]
}

// ── positive ────────────────────────────────────────────────────────────────

/// Hold-dominance (22s of 0.56) + fast wick to 0.28 (50% inside 2s) + calm
/// underlying → one resting bid at 0.26, direction Up, no shares.
#[test]
fn dominant_then_fast_wick_suggests_one_maker_bid() {
    let mut a = adapter();
    feed_dominance(&mut a, UP, NOW, 22, dec!(0.56));
    feed_spot(&mut a, NOW + 12_000, &flat_spot(NOW + 12_000, 10));
    let wick_ts = NOW + 23_000;
    let wick = book(dec!(0.27), dec!(0.29), dec!(600), wick_ts);
    a.on_book(UP, &wick, wick_ts);

    let markets = vec![market()];
    let wick_for_fresh = wick.clone();
    let fresh = move |t: &str| (t == UP).then(|| wick_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 600, wick_ts, &fresh);

    assert_eq!(got.len(), 1, "one suggestion, got {got:?}");
    assert_eq!(got[0].token_id, UP);
    assert_eq!(got[0].price, dec!(0.26));
    assert_eq!(got[0].shares, None, "sizing belongs to the kernel");
    assert_eq!(got[0].asset, "BTC");
    assert!(
        got[0].reason.contains("panic wick"),
        "reason should name the wick: {}",
        got[0].reason
    );
}

/// The mean spelling of dominance (rolling mean >= 0.55 over a window with
/// real coverage, without a 20s continuous hold) arms the same suggestion.
#[test]
fn mean_dominance_also_arms() {
    let mut a = adapter();
    // Alternating 0.60 / 0.51 prints for 61s: mean 0.555 >= 0.55 with >20s of
    // window coverage, while every hold window contains a 0.51.
    for i in 0..61 {
        let ts = NOW + (i as i64) * 1000;
        let mid = if i % 2 == 0 { dec!(0.60) } else { dec!(0.51) };
        a.on_book(
            UP,
            &book(mid - dec!(0.01), mid + dec!(0.01), dec!(600), ts),
            ts,
        );
    }
    feed_spot(&mut a, NOW + 50_000, &flat_spot(NOW + 50_000, 10));
    let wick_ts = NOW + 63_000;
    let wick = book(dec!(0.27), dec!(0.29), dec!(600), wick_ts);
    a.on_book(UP, &wick, wick_ts);

    let markets = vec![market()];
    let wick_for_fresh = wick.clone();
    let fresh = move |t: &str| (t == UP).then(|| wick_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 600, wick_ts, &fresh);
    assert_eq!(got.len(), 1, "mean-dominance must arm, got {got:?}");
    assert_eq!(got[0].price, dec!(0.26));
}

/// A deeper wick ratchets the extreme: the bid follows to new_low − 0.02.
#[test]
fn deeper_wick_ratchets_the_bid() {
    let mut a = adapter();
    feed_dominance(&mut a, UP, NOW, 22, dec!(0.56));
    feed_spot(&mut a, NOW + 12_000, &flat_spot(NOW + 12_000, 10));
    let wick_ts = NOW + 23_000;
    a.on_book(
        UP,
        &book(dec!(0.27), dec!(0.29), dec!(600), wick_ts),
        wick_ts,
    );
    let deeper_ts = NOW + 24_000;
    let deeper = book(dec!(0.23), dec!(0.25), dec!(600), deeper_ts);
    a.on_book(UP, &deeper, deeper_ts);

    let markets = vec![market()];
    let deeper_for_fresh = deeper.clone();
    let fresh = move |t: &str| (t == UP).then(|| deeper_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 600, deeper_ts, &fresh);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].price, dec!(0.22));
}

/// Recovery above the broken level closes the panic leg and CANCELS the
/// stale resting bid (a break), emitting no further entries.
#[test]
fn recovery_cancels_the_stale_bid() {
    let mut a = adapter();
    feed_dominance(&mut a, UP, NOW, 22, dec!(0.56));
    let wick_ts = NOW + 23_000;
    a.on_book(
        UP,
        &book(dec!(0.27), dec!(0.29), dec!(600), wick_ts),
        wick_ts,
    );
    let deeper_ts = NOW + 24_000;
    a.on_book(
        UP,
        &book(dec!(0.23), dec!(0.25), dec!(600), deeper_ts),
        deeper_ts,
    );
    // Panic ends: the price recovers to 0.50.
    feed_spot(&mut a, NOW + 12_000, &flat_spot(NOW + 12_000, 10));
    let rec_ts = NOW + 30_000;
    let recovered = book(dec!(0.49), dec!(0.51), dec!(600), rec_ts);
    a.on_book(UP, &recovered, rec_ts);

    let markets = vec![market()];
    let recovered_for_fresh = recovered.clone();
    let fresh = move |t: &str| (t == UP).then(|| recovered_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 600, rec_ts, &fresh);
    assert!(got.is_empty(), "no entries after recovery: {got:?}");
    let breaks = a.take_breaks();
    assert_eq!(breaks.len(), 1, "one cancel for the stale bid");
    assert_eq!(breaks[0].0, UP);
    assert_eq!(
        breaks[0].1,
        dec!(0.24),
        "the ratcheted extreme is what rested"
    );
}

// ── negative (each row must refuse) ─────────────────────────────────────────

/// Time floor: same perfect setup (including calm spot data, so the time
/// gate is the ONLY thing that can refuse), but < 180s left → no suggestion.
#[test]
fn refuses_inside_the_time_floor() {
    let mut a = adapter();
    feed_dominance(&mut a, UP, NOW, 22, dec!(0.56));
    feed_spot(&mut a, NOW + 12_000, &flat_spot(NOW + 12_000, 10));
    let wick_ts = NOW + 23_000;
    let wick = book(dec!(0.27), dec!(0.29), dec!(600), wick_ts);
    a.on_book(UP, &wick, wick_ts);
    let markets = vec![market()];
    let wick_for_fresh = wick.clone();
    let fresh = move |t: &str| (t == UP).then(|| wick_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 179, wick_ts, &fresh);
    assert!(got.is_empty(), "time floor must refuse: {got:?}");
}

/// A slow bleed into the broken zone is not a panic: no speed, no arm.
#[test]
fn refuses_a_slow_bleed() {
    let mut a = adapter();
    feed_dominance(&mut a, UP, NOW, 22, dec!(0.56));
    // 1s steps of ~4% each: 0.54 down to 0.34 — every 3s move far under 15%.
    let steps = [
        dec!(0.54),
        dec!(0.52),
        dec!(0.50),
        dec!(0.48),
        dec!(0.46),
        dec!(0.44),
        dec!(0.42),
        dec!(0.40),
        dec!(0.38),
        dec!(0.36),
        dec!(0.34),
    ];
    let mut ts = NOW + 23_000;
    for mid in steps {
        a.on_book(
            UP,
            &book(mid - dec!(0.01), mid + dec!(0.01), dec!(600), ts),
            ts,
        );
        ts += 1000;
    }
    let markets = vec![market()];
    let last = book(dec!(0.33), dec!(0.35), dec!(600), ts);
    let fresh = move |t: &str| (t == UP).then(|| last.clone());
    let got = candidates_of(&mut a, &markets, 600, ts, &fresh);
    assert!(got.is_empty(), "slow bleed must never arm: {got:?}");
}

/// No dominance: the same fast wick without a held direction buys nothing.
#[test]
fn refuses_without_dominance() {
    let mut a = adapter();
    feed_dominance(&mut a, UP, NOW, 10, dec!(0.45)); // 10s at 0.45: neither spelling
    let wick_ts = NOW + 11_000;
    let wick = book(dec!(0.27), dec!(0.29), dec!(600), wick_ts);
    a.on_book(UP, &wick, wick_ts);
    let markets = vec![market()];
    let wick_for_fresh = wick.clone();
    let fresh = move |t: &str| (t == UP).then(|| wick_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 600, wick_ts, &fresh);
    assert!(got.is_empty(), "no direction confirmed, no knife: {got:?}");
}

/// Thin book: bid depth under the floor refuses (OBI stays neutral here, so
/// this isolates the depth gate).
#[test]
fn refuses_a_thin_book() {
    let mut a = adapter();
    feed_dominance(&mut a, UP, NOW, 22, dec!(0.56));
    let wick_ts = NOW + 23_000;
    let thin = OrderbookSnapshot::from_levels(
        UP.to_string(),
        vec![(dec!(0.27), dec!(400))],
        vec![(dec!(0.29), dec!(500))],
        wick_ts,
    );
    a.on_book(UP, &thin, wick_ts);
    let markets = vec![market()];
    let thin_for_fresh = thin.clone();
    let fresh = move |t: &str| (t == UP).then(|| thin_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 600, wick_ts, &fresh);
    assert!(got.is_empty(), "depth floor must refuse: {got:?}");
}

/// Extreme imbalance refuses even with plenty of depth.
#[test]
fn refuses_extreme_obi() {
    let mut a = adapter();
    feed_dominance(&mut a, UP, NOW, 22, dec!(0.56));
    let wick_ts = NOW + 23_000;
    let skewed = OrderbookSnapshot::from_levels(
        UP.to_string(),
        vec![(dec!(0.27), dec!(900))],
        vec![(dec!(0.29), dec!(100))],
        wick_ts,
    );
    a.on_book(UP, &skewed, wick_ts);
    let markets = vec![market()];
    let skewed_for_fresh = skewed.clone();
    let fresh = move |t: &str| (t == UP).then(|| skewed_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 600, wick_ts, &fresh);
    assert!(got.is_empty(), "OBI band must refuse: {got:?}");
}

/// An underwater account view refuses (when the host surfaces one at all —
/// no host path populates it today, and then the kernel owns funds).
#[test]
fn refuses_an_underwater_account() {
    let mut a = adapter_with_account("0.50");
    feed_dominance(&mut a, UP, NOW, 22, dec!(0.56));
    let wick_ts = NOW + 23_000;
    let wick = book(dec!(0.27), dec!(0.29), dec!(600), wick_ts);
    a.on_book(UP, &wick, wick_ts);
    let markets = vec![market()];
    let wick_for_fresh = wick.clone();
    let fresh = move |t: &str| (t == UP).then(|| wick_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 600, wick_ts, &fresh);
    assert!(got.is_empty(), "available < $1 must refuse: {got:?}");
}

/// Underlying waterfall: the sec1 spot series drops 0.6% over the window →
/// an UP buy is refused. The disclosed diagnostic knob (`spot_missing=pass`
/// only covers MISSING data, not present adverse data) must not re-arm this.
#[test]
fn refuses_an_underlying_waterfall() {
    let mut a = adapter();
    feed_dominance(&mut a, UP, NOW, 22, dec!(0.56));
    // 10 one-second closes from 60000 down to 59640 (-0.6%).
    let closes: Vec<Decimal> = (0..10)
        .map(|i| dec!(60000) - dec!(40) * Decimal::from(i))
        .collect();
    feed_spot(&mut a, NOW + 12_000, &closes);
    let wick_ts = NOW + 23_000;
    let wick = book(dec!(0.27), dec!(0.29), dec!(600), wick_ts);
    a.on_book(UP, &wick, wick_ts);
    let markets = vec![market()];
    let wick_for_fresh = wick.clone();
    let fresh = move |t: &str| (t == UP).then(|| wick_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 600, wick_ts, &fresh);
    assert!(got.is_empty(), "waterfall must refuse an UP buy: {got:?}");
}

/// Book-only diagnostics: with NO spot data at all the default ("block")
/// refuses; flipping the disclosed `spot_missing` knob to "pass" arms the
/// same setup — that is what the frozen-corpus diagnostic arm runs on.
#[test]
fn missing_spot_data_default_refuses_and_pass_knob_arms() {
    let mut blocked = adapter();
    feed_dominance(&mut blocked, UP, NOW, 22, dec!(0.56));
    let wick_ts = NOW + 23_000;
    let wick = book(dec!(0.27), dec!(0.29), dec!(600), wick_ts);
    blocked.on_book(UP, &wick, wick_ts);
    let markets = vec![market()];
    let wick_for_fresh = wick.clone();
    let fresh = move |t: &str| (t == UP).then(|| wick_for_fresh.clone());
    let got = candidates_of(&mut blocked, &markets, 600, wick_ts, &fresh);
    assert!(got.is_empty(), "missing spot data must refuse by default");

    let mut passing = adapter_with_params("spot_missing", "pass");
    feed_dominance(&mut passing, UP, NOW, 22, dec!(0.56));
    let wick2 = book(dec!(0.27), dec!(0.29), dec!(600), wick_ts);
    passing.on_book(UP, &wick2, wick_ts);
    let markets2 = vec![market()];
    let wick2_for_fresh = wick2.clone();
    let fresh2 = move |t: &str| (t == UP).then(|| wick2_for_fresh.clone());
    let got = candidates_of(&mut passing, &markets2, 600, wick_ts, &fresh2);
    assert_eq!(got.len(), 1, "pass knob arms the book-only setup: {got:?}");
    assert_eq!(got[0].price, dec!(0.26));
}

// ── packaging ───────────────────────────────────────────────────────────────

/// The declaration the E27 handshake reads: prediction / binary wheel, the
/// three capabilities the polymarket seam actually serves.
#[test]
fn modes_declare_what_the_seam_serves() {
    let loaded = load_lua_package(&package_dir()).expect("package loads");
    assert_eq!(loaded.name, "mad_dog");
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

/// The DOWN side is its own token: a wick on `down` suggests DOWN.
#[test]
fn down_side_wick_suggests_down() {
    let mut a = adapter();
    feed_dominance(&mut a, DOWN, NOW, 22, dec!(0.56));
    feed_spot(&mut a, NOW + 12_000, &flat_spot(NOW + 12_000, 10));
    let wick_ts = NOW + 23_000;
    let wick = token_book(DOWN, dec!(0.27), dec!(0.29), dec!(600), wick_ts);
    a.on_book(DOWN, &wick, wick_ts);

    let markets = vec![market()];
    let wick_for_fresh = wick.clone();
    let fresh = move |t: &str| (t == DOWN).then(|| wick_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 600, wick_ts, &fresh);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].token_id, DOWN);
    assert_eq!(got[0].price, dec!(0.26));
}
