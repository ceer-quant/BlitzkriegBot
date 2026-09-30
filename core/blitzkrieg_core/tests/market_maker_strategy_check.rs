//! market_maker acceptance checks — the REAL package (`user_layer/
//! strategies_lua/market_maker`) driven through the engine adapter, exactly
//! the surface the kernel runs.
//!
//! The spec's acceptance rows, mapped:
//!   positive  — band (0.20–0.42) + time floor + two-sided depth + OBI band
//!               + calm chip + calm underlying → ONE resting maker bid
//!               mid − 0.02, no shares (the kernel sizes);
//!               target hit (mid >= bid + 0.03) → one exit suggestion AND
//!               one bid retirement, then the thesis re-arms at once;
//!   negative  — below/above the band, inside the time floor, thin bid side,
//!               thin ask side, extreme OBI, one-sided chip decline, spot
//!               waterfall (UP veto) / pump (DOWN veto), missing spot data
//!               (default block, disclosed `spot_missing=pass` diagnostic),
//!               underwater account, second bid while one is in flight;
//!   teeth     — a milder decline (under the threshold) arms: proves the
//!               trend gate, not a neighbour gate, refused in the negative.
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../user_layer/strategies_lua/market_maker")
}

fn adapter() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("market_maker package must load");
    LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
}

/// Adapter whose `bk.params()` start from a modified default bag (the
/// manifest-defaults path, overridden the way a param registry would).
fn adapter_with_params(override_key: &str, override_value: &str) -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("market_maker package must load");
    let mut tunables = loaded.tunables;
    tunables.insert(override_key.to_string(), override_value.to_string());
    LuaEngineAdapter::new(loaded.strategy, tunables)
}

/// Adapter whose account view is pre-populated (the host does not push one
/// today; the test exercises the gate for the day one does).
fn adapter_with_account(available: &str) -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("market_maker package must load");
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

fn token_book(
    token: &str,
    mid_bid: Decimal,
    mid_ask: Decimal,
    bid_depth: Decimal,
    ask_depth: Decimal,
    ts: i64,
) -> OrderbookSnapshot {
    OrderbookSnapshot::from_levels(
        token.to_string(),
        vec![(mid_bid, bid_depth)],
        vec![(mid_ask, ask_depth)],
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

/// `n` one-second prints of `mid` on `token` (two-sided, deep enough, OBI
/// neutral), oldest at `start_ms` — the 30s calm proof the trend gate wants.
fn feed_flat(adapter: &mut LuaEngineAdapter, token: &str, start_ms: i64, n: usize, mid: Decimal) {
    for i in 0..n {
        let ts = start_ms + (i as i64) * 1000;
        adapter.on_book(
            token,
            &token_book(
                token,
                mid - dec!(0.01),
                mid + dec!(0.01),
                dec!(400),
                dec!(400),
                ts,
            ),
            ts,
        );
    }
}

/// `n` one-second prints stepping from `from` down/up to `to` (inclusive).
fn feed_ramp(
    adapter: &mut LuaEngineAdapter,
    token: &str,
    start_ms: i64,
    n: usize,
    from: Decimal,
    to: Decimal,
) {
    let step = (to - from) / Decimal::from(n as i64 - 1);
    for i in 0..n {
        let ts = start_ms + (i as i64) * 1000;
        let mid = from + step * Decimal::from(i as i64);
        adapter.on_book(
            token,
            &token_book(
                token,
                mid - dec!(0.01),
                mid + dec!(0.01),
                dec!(400),
                dec!(400),
                ts,
            ),
            ts,
        );
    }
}

fn flat_spot(start_open_ms: i64, n: usize) -> Vec<Decimal> {
    vec![dec!(60000); n]
}

/// The full passing setup: 32s of calm 0.30 chip, 32 calm spot closes, deep
/// balanced book. `mid` and the token decide the variant; returns the final
/// book so the caller can hand it back as the fresh view.
fn armed_setup(adapter: &mut LuaEngineAdapter, token: &str, spot: &[Decimal]) -> OrderbookSnapshot {
    feed_flat(adapter, token, NOW, 32, dec!(0.30));
    feed_spot(adapter, NOW, spot);
    let book = token_book(
        token,
        dec!(0.29),
        dec!(0.31),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    adapter.on_book(token, &book, NOW + 32_000);
    book
}

// ── positive ────────────────────────────────────────────────────────────────

/// The spec's row 1: price 0.30, > 120s left, depth on both sides, calm
/// chip and calm underlying → ONE resting maker bid at 0.28, no shares.
#[test]
fn band_depth_and_calm_suggest_one_maker_bid() {
    let mut a = adapter();
    let book = armed_setup(&mut a, UP, &flat_spot(NOW, 32));

    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);

    assert_eq!(got.len(), 1, "one suggestion, got {got:?}");
    assert_eq!(got[0].token_id, UP);
    assert_eq!(got[0].price, dec!(0.28));
    assert_eq!(got[0].shares, None, "sizing belongs to the kernel");
    assert_eq!(got[0].asset, "BTC");
    assert!(
        got[0].reason.contains("resting maker bid"),
        "reason should name the resting bid: {}",
        got[0].reason
    );
}

/// The spec's row 2: after the bid armed, the chip's mid climbs to bid + 0.03
/// → one close suggestion and one bid retirement, then the thesis re-arms at
/// once (「价差到手就平，立刻开下一笔」).
#[test]
fn target_hit_suggests_the_close_then_re_arms() {
    let mut a = adapter();
    let book = armed_setup(&mut a, UP, &flat_spot(NOW, 32));
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert_eq!(got.len(), 1, "setup must arm: {got:?}");

    // Mid 0.32 >= 0.28 + 0.03.
    let bounced_ts = NOW + 33_000;
    let bounced = token_book(UP, dec!(0.31), dec!(0.33), dec!(400), dec!(400), bounced_ts);
    a.on_book(UP, &bounced, bounced_ts);
    let bounced_for_fresh = bounced.clone();
    let fresh2 = move |t: &str| (t == UP).then(|| bounced_for_fresh.clone());
    let rearmed = candidates_of(&mut a, &markets, 600, bounced_ts, &fresh2);

    let exits = a.take_exit_intents();
    assert_eq!(exits.len(), 1, "one close suggestion: {exits:?}");
    assert_eq!(exits[0].token_id, UP);
    assert!(
        exits[0].reason.contains("target reached"),
        "reason should name the target: {}",
        exits[0].reason
    );
    let breaks = a.take_breaks();
    assert_eq!(breaks.len(), 1, "the resting bid is retired too");
    assert_eq!(breaks[0].0, UP);
    assert_eq!(breaks[0].1, dec!(0.28));

    // 「价差到手就平，立刻开下一笔」— the SAME evaluate that banks the spread
    // already arms the next bid at the new mid.
    assert_eq!(rearmed.len(), 1, "re-armed at once: {rearmed:?}");
    assert_eq!(rearmed[0].price, dec!(0.30), "new bid follows the new mid");

    // One in flight again: the next evaluate adds nothing.
    let got3 = candidates_of(&mut a, &markets, 600, bounced_ts, &fresh2);
    assert!(got3.is_empty(), "one thesis in flight: {got3:?}");
}

/// The DOWN side is its own chip: a cheap DOWN token with a calm underlying
/// suggests DOWN.
#[test]
fn down_chip_suggests_down() {
    let mut a = adapter();
    let book = armed_setup(&mut a, DOWN, &flat_spot(NOW, 32));
    let markets = vec![market()];
    let fresh = move |t: &str| (t == DOWN).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].token_id, DOWN);
    assert_eq!(got[0].price, dec!(0.28));
}

/// The declaration the E27 handshake reads: prediction / binary wheel, the
/// three capabilities the polymarket seam actually serves.
#[test]
fn modes_declare_what_the_seam_serves() {
    let loaded = load_lua_package(&package_dir()).expect("package loads");
    assert_eq!(loaded.name, "market_maker");
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

// ── negative (each row must refuse) ─────────────────────────────────────────

/// The spec's row: price 0.15 → refused (below the band).
#[test]
fn refuses_below_the_band() {
    let mut a = adapter();
    feed_flat(&mut a, UP, NOW, 32, dec!(0.15));
    feed_spot(&mut a, NOW, &flat_spot(NOW, 32));
    let book = token_book(
        UP,
        dec!(0.14),
        dec!(0.16),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    a.on_book(UP, &book, NOW + 32_000);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert!(got.is_empty(), "below the band must refuse: {got:?}");
}

/// The spec's row: price 0.50 → refused (above the band).
#[test]
fn refuses_above_the_band() {
    let mut a = adapter();
    feed_flat(&mut a, UP, NOW, 32, dec!(0.50));
    feed_spot(&mut a, NOW, &flat_spot(NOW, 32));
    let book = token_book(
        UP,
        dec!(0.49),
        dec!(0.51),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    a.on_book(UP, &book, NOW + 32_000);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert!(got.is_empty(), "above the band must refuse: {got:?}");
}

/// The spec's row: < 120s left → refused (everything else is perfect, so the
/// time gate is the only thing that can refuse).
#[test]
fn refuses_inside_the_time_floor() {
    let mut a = adapter();
    let book = armed_setup(&mut a, UP, &flat_spot(NOW, 32));
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 119, NOW + 32_000, &fresh);
    assert!(got.is_empty(), "time floor must refuse: {got:?}");
}

/// A thin side refuses on each side in turn (OBI stays inside the band, so
/// the depth gates are isolated).
#[test]
fn refuses_a_thin_side() {
    let markets = vec![market()];
    for (bid_depth, ask_depth) in [(dec!(250), dec!(400)), (dec!(400), dec!(250))] {
        let mut a = adapter();
        feed_flat(&mut a, UP, NOW, 32, dec!(0.30));
        feed_spot(&mut a, NOW, &flat_spot(NOW, 32));
        let book = token_book(
            UP,
            dec!(0.29),
            dec!(0.31),
            bid_depth,
            ask_depth,
            NOW + 32_000,
        );
        a.on_book(UP, &book, NOW + 32_000);
        let fresh = move |t: &str| (t == UP).then(|| book.clone());
        let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
        assert!(
            got.is_empty(),
            "thin side {bid_depth}/{ask_depth} must refuse: {got:?}"
        );
    }
}

/// Extreme imbalance refuses even with both sides deep (9000 vs 300 keeps
/// both depth gates open so the OBI band is isolated).
#[test]
fn refuses_extreme_obi() {
    let mut a = adapter();
    feed_flat(&mut a, UP, NOW, 32, dec!(0.30));
    feed_spot(&mut a, NOW, &flat_spot(NOW, 32));
    let book = token_book(
        UP,
        dec!(0.29),
        dec!(0.31),
        dec!(9000),
        dec!(300),
        NOW + 32_000,
    );
    a.on_book(UP, &book, NOW + 32_000);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert!(got.is_empty(), "OBI band must refuse: {got:?}");
}

/// A one-sided chip decline refuses: the same 0.30 endpoint as the passing
/// setup, but reached by falling 12% inside the window — that is the knife.
#[test]
fn refuses_a_one_sided_chip_decline() {
    let mut a = adapter();
    feed_ramp(&mut a, UP, NOW, 32, dec!(0.34), dec!(0.30));
    feed_spot(&mut a, NOW, &flat_spot(NOW, 32));
    let book = token_book(
        UP,
        dec!(0.29),
        dec!(0.31),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    a.on_book(UP, &book, NOW + 32_000);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert!(got.is_empty(), "one-sided decline must refuse: {got:?}");
}

/// TEETH for the trend gate: the same ramp ending at the same 0.30, but only
/// −3.2% net (under the 5% threshold) — the setup ARMS. Proves the refusal in
/// the test above came from the trend gate, not from a neighbour.
#[test]
fn a_mild_decline_arms_trend_gate_is_the_refuser() {
    let mut a = adapter();
    feed_ramp(&mut a, UP, NOW, 32, dec!(0.31), dec!(0.30));
    feed_spot(&mut a, NOW, &flat_spot(NOW, 32));
    let book = token_book(
        UP,
        dec!(0.29),
        dec!(0.31),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    a.on_book(UP, &book, NOW + 32_000);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert_eq!(
        got.len(),
        1,
        "−3.2% is calm; the trend gate must open: {got:?}"
    );
    assert_eq!(got[0].price, dec!(0.28));
}

/// Underlying waterfall: the sec1 spot series drops ~1.65% over the window →
/// an UP buy is refused. The disclosed `spot_missing=pass` knob covers MISSING
/// data only; present adverse data still refuses.
#[test]
fn refuses_an_underlying_waterfall() {
    let mut a = adapter();
    feed_flat(&mut a, UP, NOW, 32, dec!(0.30));
    let closes: Vec<Decimal> = (0..32)
        .map(|i| dec!(60000) - dec!(32) * Decimal::from(i))
        .collect();
    feed_spot(&mut a, NOW, &closes);
    let book = token_book(
        UP,
        dec!(0.29),
        dec!(0.31),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    a.on_book(UP, &book, NOW + 32_000);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert!(got.is_empty(), "waterfall must refuse an UP buy: {got:?}");
}

/// Underlying pump: the mirror veto — a ~1.65% pump refuses a DOWN buy.
#[test]
fn refuses_an_underlying_pump_for_the_down_chip() {
    let mut a = adapter();
    feed_flat(&mut a, DOWN, NOW, 32, dec!(0.30));
    let closes: Vec<Decimal> = (0..32)
        .map(|i| dec!(60000) + dec!(32) * Decimal::from(i))
        .collect();
    feed_spot(&mut a, NOW, &closes);
    let book = token_book(
        DOWN,
        dec!(0.29),
        dec!(0.31),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    a.on_book(DOWN, &book, NOW + 32_000);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == DOWN).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert!(got.is_empty(), "pump must refuse a DOWN buy: {got:?}");
}

/// Book-only diagnostics: with NO spot data at all the default ("block")
/// refuses; flipping the disclosed `spot_missing` knob to "pass" arms the
/// same setup — that is what the frozen-corpus diagnostic arm runs on.
#[test]
fn missing_spot_data_default_refuses_and_pass_knob_arms() {
    let mut blocked = adapter();
    feed_flat(&mut blocked, UP, NOW, 32, dec!(0.30));
    let book = token_book(
        UP,
        dec!(0.29),
        dec!(0.31),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    blocked.on_book(UP, &book, NOW + 32_000);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut blocked, &markets, 600, NOW + 32_000, &fresh);
    assert!(got.is_empty(), "missing spot data must refuse by default");

    let mut passing = adapter_with_params("spot_missing", "pass");
    feed_flat(&mut passing, UP, NOW, 32, dec!(0.30));
    let book2 = token_book(
        UP,
        dec!(0.29),
        dec!(0.31),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    passing.on_book(UP, &book2, NOW + 32_000);
    let fresh2 = move |t: &str| (t == UP).then(|| book2.clone());
    let got2 = candidates_of(&mut passing, &markets, 600, NOW + 32_000, &fresh2);
    assert_eq!(
        got2.len(),
        1,
        "pass knob arms the book-only setup: {got2:?}"
    );
    assert_eq!(got2[0].price, dec!(0.28));
}

/// An underwater account view refuses (when the host surfaces one at all —
/// no host path populates it today, and then the kernel owns funds).
#[test]
fn refuses_an_underwater_account() {
    let mut a = adapter_with_account("0.50");
    feed_flat(&mut a, UP, NOW, 32, dec!(0.30));
    feed_spot(&mut a, NOW, &flat_spot(NOW, 32));
    let book = token_book(
        UP,
        dec!(0.29),
        dec!(0.31),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    a.on_book(UP, &book, NOW + 32_000);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert!(got.is_empty(), "available < $1 must refuse: {got:?}");
}

/// One thesis in flight: while a bid is armed (target not yet hit, chip
/// still calm) the next evaluate arms nothing new.
#[test]
fn a_second_bid_waits_while_one_is_in_flight() {
    let mut a = adapter();
    let book = armed_setup(&mut a, UP, &flat_spot(NOW, 32));
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert_eq!(got.len(), 1, "setup must arm: {got:?}");
    let got2 = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert!(got2.is_empty(), "one in flight, no second bid: {got2:?}");
    assert!(
        a.take_exit_intents().is_empty(),
        "no close before the target"
    );
}
