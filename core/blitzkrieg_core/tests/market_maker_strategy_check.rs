//! market_maker acceptance checks — the REAL package (`user_layer/
//! strategies_lua/market_maker`) driven through the engine adapter, exactly
//! the surface the kernel runs.
//!
//! The spec's acceptance rows, mapped (v1.1 — the dual-side rework):
//!   positive  — band (0.20–0.42) + time floor + two-sided depth + OBI band
//!               + calm chip + calm underlying → maker bids for EACH
//!               qualifying side of the first qualifying market (both UP and
//!               DOWN when both books qualify — the engine takes Buy-side
//!               limits only, so the dual quote is the UP+DOWN pair of bids);
//!   holdings  — `bk.holdings()` is the ONE fill-blind position observation:
//!               one held leg retires its own bid, keeps the opposite bid
//!               only while the pair keeps its merge discount (the skew),
//!               closes above the fee-cleared band, and a completed pair
//!               emits reason="merge" intents for both tokens;
//!   negative  — below/above the band, inside the time floor, thin bid side,
//!               thin ask side, extreme OBI, one-sided chip decline, spot
//!               waterfall (UP veto) / pump (DOWN veto), missing spot data
//!               (default block, disclosed `spot_missing=pass` diagnostic),
//!               underwater account, second thesis while one is in flight;
//!   teeth     — a milder decline (under the threshold) arms: proves the
//!               trend gate, not a neighbour gate, refused in the negative.
//!
//! Sandbox refusal (os/require nil) and manifest tamper refusals are the
//! lua-sandbox-check's fixtures; this file loads the untampered package
//! through the same loader, so a manifest/modes/sha256 regression fails every
//! test here at setup time.

use std::path::PathBuf;

use blitzkrieg_core::strategies::{EngineStrategy, HeldPosition, StrategyCtx};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use blitzkrieg_core::{kline::Kline, model::CryptoMarket, model::OrderbookSnapshot};
use blitzkrieg_lua_runtime::bk_api::{AccountView, FeeScheduleView};
use blitzkrieg_market_api::kline::KlineInterval;
use blitzkrieg_market_api::{MarketStructure, MarketType};
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

/// Adapter with the kernel's shipped taker-fee schedule pushed (the shape the
/// live adapter installs; the fee-band exit reads it through `bk.fees()`).
fn adapter_with_fees() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("market_maker package must load");
    {
        let state = loaded.strategy.state();
        let mut st = state.lock().expect("lock");
        st.fee_schedule = Some(FeeScheduleView {
            name: "legacy_quadratic".into(),
            rate: "0.125".into(),
            exponent: 2,
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
        venue: String::new(),
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

fn flat_spot(_start_open_ms: i64, n: usize) -> Vec<Decimal> {
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

/// The DUAL passing setup: BOTH chips of one market qualify (32s calm at
/// 0.30 each side, calm spot, deep balanced books).
fn dual_armed_setup(
    adapter: &mut LuaEngineAdapter,
    up_spot: &[Decimal],
) -> (OrderbookSnapshot, OrderbookSnapshot) {
    feed_flat(adapter, UP, NOW, 32, dec!(0.30));
    feed_flat(adapter, DOWN, NOW, 32, dec!(0.30));
    feed_spot(adapter, NOW, up_spot);
    let up_book = token_book(
        UP,
        dec!(0.29),
        dec!(0.31),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    adapter.on_book(UP, &up_book, NOW + 32_000);
    let down_book = token_book(
        DOWN,
        dec!(0.29),
        dec!(0.31),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    adapter.on_book(DOWN, &down_book, NOW + 32_000);
    (up_book, down_book)
}

/// A held UP leg of THIS slot, filled at `entry` — the `bk.holdings()` row.
fn held_up(entry: Decimal) -> Vec<HeldPosition> {
    vec![HeldPosition {
        account_id: "default".into(),
        strategy: "market_maker".into(),
        condition_id: "cond".into(),
        token_id: UP.into(),
        direction: "up".into(),
        shares: "10".into(),
        entry_price: entry.to_string(),
        round_slot: NOW / 900_000,
    }]
}

/// Both legs of THIS slot held — the completed pair the merge burns.
fn held_pair(up_entry: Decimal, down_entry: Decimal) -> Vec<HeldPosition> {
    vec![
        HeldPosition {
            account_id: "default".into(),
            strategy: "market_maker".into(),
            condition_id: "cond".into(),
            token_id: UP.into(),
            direction: "up".into(),
            shares: "10".into(),
            entry_price: up_entry.to_string(),
            round_slot: NOW / 900_000,
        },
        HeldPosition {
            account_id: "default".into(),
            strategy: "market_maker".into(),
            condition_id: "cond".into(),
            token_id: DOWN.into(),
            direction: "down".into(),
            shares: "10".into(),
            entry_price: down_entry.to_string(),
            round_slot: NOW / 900_000,
        },
    ]
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

/// The DUAL row: both chips qualify → both sides of the ONE market arm
/// (UP and DOWN maker bids at 0.28 each). The engine takes Buy-side limits
/// only, so this pair of bids IS the dual quote.
#[test]
fn both_chips_qualifying_arms_both_sides() {
    let mut a = adapter();
    let (up_book, down_book) = dual_armed_setup(&mut a, &flat_spot(NOW, 32));

    let markets = vec![market()];
    let up_b = up_book.clone();
    let down_b = down_book.clone();
    let fresh = move |t: &str| {
        if t == UP {
            Some(up_b.clone())
        } else {
            (t == DOWN).then(|| down_b.clone())
        }
    };
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);

    assert_eq!(got.len(), 2, "both sides arm: {got:?}");
    assert_eq!(got[0].token_id, UP);
    assert_eq!(got[0].price, dec!(0.28));
    assert_eq!(got[1].token_id, DOWN);
    assert_eq!(got[1].price, dec!(0.28));
}

/// The fill-blind upkeep, flat world: the armed bid's mid clears
/// bid + spread → the thesis ran away — the resting bids retire (breaks),
/// NO exit is suggested (there is no position to close), and the SAME
/// evaluate re-arms at the new mid (「价差到手就平，立刻开下一笔」).
#[test]
fn flat_target_hit_breaks_then_re_arms_without_a_fake_exit() {
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
    assert!(
        exits.is_empty(),
        "flat world: nothing to close, no exit faked: {exits:?}"
    );
    let breaks = a.take_breaks();
    assert_eq!(breaks.len(), 1, "the resting bid is retired");
    assert_eq!(breaks[0].0, UP);
    assert_eq!(breaks[0].1, dec!(0.28));

    // 「价差到手就平，立刻开下一笔」— the SAME evaluate re-arms at the new mid.
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

/// The declaration the E27 handshake reads: prediction / binary wheel, and
/// NOTHING else — a strategy declares WHERE it works, never which capability
/// bits it demands (that surface is removed; it once refused every
/// binary-market strategy on any venue whose plugin declared fewer bits than
/// the copied template asked for).
#[test]
fn modes_declare_type_and_structure_only() {
    let loaded = load_lua_package(&package_dir()).expect("package loads");
    assert_eq!(loaded.name, "market_maker");
    assert_eq!(loaded.declared_modes.len(), 1);
    let m = &loaded.declared_modes[0];
    assert_eq!(m.market_type, MarketType::Prediction);
    assert_eq!(m.structure, Some(MarketStructure::BinaryOutcomeWheel));
    // The validator itself refuses capabilities in a strategy payload; what
    // this asserts is the DECLARATION the package ships carries none.
    // And the shipped manifest genuinely carries no capabilities key: the
    // loader would refuse the package outright if it did (CapabilityForbidden).
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(package_dir().join("manifest.json")).expect("manifest readable"),
    )
    .expect("manifest json");
    assert!(
        manifest["modes"][0].get("capabilities").is_none(),
        "a strategy manifest must not declare capabilities: {manifest}"
    );
}

// ── holdings: the skew, the merge, the fee band ─────────────────────────────

/// One held leg: the held side's bid retires (the skew), the OPPOSITE bid is
/// NOT armed again (a resting entry cannot be re-priced and no second entry
/// fires under inventory), and NO exit fires — the mid is far below the
/// fee band.
#[test]
fn one_held_leg_breaks_its_own_bid_and_arms_nothing() {
    let mut a = adapter_with_fees();
    let book = armed_setup(&mut a, UP, &flat_spot(NOW, 32));
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());

    let held = held_up(dec!(0.28));
    let got = {
        let c = ctx(&markets, 600, NOW + 32_000, &fresh).with_positions(&held);
        a.find_candidates(&c)
    };
    assert!(
        got.is_empty(),
        "inventory open: no new entry may arm: {got:?}"
    );
    let breaks = a.take_breaks();
    assert_eq!(breaks.len(), 1, "the held side's resting bid retires");
    assert_eq!(breaks[0].0, UP);
    let exits = a.take_exit_intents();
    assert!(
        exits.is_empty(),
        "mid 0.30 is far under the fee band: no close: {exits:?}"
    );
}

/// The merge row: BOTH legs of one condition held → reason="merge" intents
/// for both tokens (the kernel burns complete pairs at 1.00/pair fee-free),
/// everything breaks, nothing arms.
#[test]
fn both_legs_held_emit_merge_intents() {
    let mut a = adapter();
    let book = armed_setup(&mut a, UP, &flat_spot(NOW, 32));
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());

    let held = held_pair(dec!(0.28), dec!(0.30));
    let got = {
        let c = ctx(&markets, 600, NOW + 32_000, &fresh).with_positions(&held);
        a.find_candidates(&c)
    };
    assert!(
        got.is_empty(),
        "no entries while a merge is pending: {got:?}"
    );
    let exits = a.take_exit_intents();
    assert_eq!(exits.len(), 2, "both tokens go to the merge: {exits:?}");
    for e in &exits {
        assert_eq!(e.reason, "merge");
    }
    let _ = a.take_breaks();
}

/// TEETH for the merge row: a single leg does NOT merge (a one-sided exit
/// intent would be a taker sell) — covered by the one-held-leg test above;
/// this one proves the dual-row test did not pass by accident: rename the
/// DOWN token to a foreign condition and the merge disappears (the kernel
/// merges ONE condition's legs only).
#[test]
fn legs_of_different_conditions_do_not_merge() {
    let mut a = adapter();
    let book = armed_setup(&mut a, UP, &flat_spot(NOW, 32));
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());

    let mut held = held_pair(dec!(0.28), dec!(0.30));
    held[1].condition_id = "other-cond".into();
    held[1].token_id = "down-x".into();
    let got = {
        let c = ctx(&markets, 600, NOW + 32_000, &fresh).with_positions(&held);
        a.find_candidates(&c)
    };
    // The foreign row is invisible (a different condition), so the state
    // machine sees ONE held leg of THIS market: skew upkeep, no merge.
    assert!(got.is_empty(), "still no re-arm under inventory: {got:?}");
    let exits = a.take_exit_intents();
    assert!(
        !exits.iter().any(|e| e.reason == "merge"),
        "one condition's leg alone never merges: {exits:?}"
    );
    let _ = a.take_breaks();
}

/// The skew's merge-discount cap: one leg held at 0.28 with the opposite bid
/// armed at 0.98 — entry + bid = 1.26 > 1.00 - 0.02, so the opposite bid is
/// RETIRED (its fill would complete a pair that LOSES 0.26/pair).
#[test]
fn an_unprofitable_opposite_bid_is_retired() {
    let mut a = adapter_with_params("max_mid_price", "0.98");
    // Arm a two-side thesis on a market whose DOWN chip sits at 0.98
    // (max_mid_price raised so both qualify; UP stays at 0.30).
    feed_flat(&mut a, UP, NOW, 32, dec!(0.30));
    feed_flat(&mut a, DOWN, NOW, 32, dec!(0.98));
    feed_spot(&mut a, NOW, &flat_spot(NOW, 32));
    let up_book = token_book(
        UP,
        dec!(0.29),
        dec!(0.31),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    a.on_book(UP, &up_book, NOW + 32_000);
    let down_book = token_book(
        DOWN,
        dec!(0.97),
        dec!(0.99),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    a.on_book(DOWN, &down_book, NOW + 32_000);
    let markets = vec![market()];
    let up_b = up_book.clone();
    let down_b = down_book.clone();
    let fresh = move |t: &str| {
        if t == UP {
            Some(up_b.clone())
        } else {
            (t == DOWN).then(|| down_b.clone())
        }
    };
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert_eq!(got.len(), 2, "both sides arm: {got:?}");
    assert_eq!(got[0].price, dec!(0.28));
    assert_eq!(got[1].price, dec!(0.96));

    // The UP leg fills. 0.28 + 0.96 = 1.24 > 0.98 cap → the DOWN bid retires.
    let held = held_up(dec!(0.28));
    let held_ctx = ctx(&markets, 600, NOW + 32_000, &fresh).with_positions(&held);
    let _ = a.find_candidates(&held_ctx);
    let mut breaks = a.take_breaks();
    breaks.sort();
    assert_eq!(
        breaks.len(),
        2,
        "held bid AND the unprofitable opposite: {breaks:?}"
    );
    assert_eq!(breaks[0], (DOWN.to_string(), dec!(0.96)));
    assert_eq!(breaks[1], (UP.to_string(), dec!(0.28)));
}

/// The skew keeps a PROFITABLE opposite bid resting: entries 0.28 + 0.30 =
/// 0.58 <= 0.98 cap → only the held side's bid breaks; the DOWN bid survives
/// (no break for DOWN) — it IS the exit leg.
#[test]
fn a_profitable_opposite_bid_survives_the_held_leg() {
    let mut a = adapter();
    let (up_book, down_book) = dual_armed_setup(&mut a, &flat_spot(NOW, 32));
    let markets = vec![market()];
    let up_b = up_book.clone();
    let down_b = down_book.clone();
    let fresh = move |t: &str| {
        if t == UP {
            Some(up_b.clone())
        } else {
            (t == DOWN).then(|| down_b.clone())
        }
    };
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert_eq!(got.len(), 2, "both sides arm: {got:?}");

    let held = held_up(dec!(0.28));
    let _ = {
        let c = ctx(&markets, 600, NOW + 32_000, &fresh).with_positions(&held);
        a.find_candidates(&c)
    };
    let breaks = a.take_breaks();
    assert_eq!(breaks.len(), 1, "only the held side's bid: {breaks:?}");
    assert_eq!(breaks[0], (UP.to_string(), dec!(0.28)));
}

/// The fee-band close: entry 0.28, the shipped legacy_quadratic curve
/// (0.125*(p(1-p))^2). fee(0.28) = 0.00508, fee(0.31) = 0.00572,
/// fee(0.34) = 0.00629; with the default 100% margin the band is
/// 0.28 + 0.03 + (fee(0.28) + fee(mid)): ~0.3208 at mid 0.31,
/// ~0.3214 at mid 0.34. At mid 0.31: no exit. At mid 0.34: one taker
/// exit, reason naming the band.
#[test]
fn the_taker_close_respects_the_fee_band() {
    // FEE BAND ARM: mid 0.31 < ~0.3208 → no close.
    let mut a = adapter_with_fees();
    let book = armed_setup(&mut a, UP, &flat_spot(NOW, 32));
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let held = held_up(dec!(0.28));
    let below = token_book(
        UP,
        dec!(0.30),
        dec!(0.32),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    a.on_book(UP, &below, NOW + 32_000);
    {
        let c = ctx(&markets, 600, NOW + 32_000, &fresh).with_positions(&held);
        let _ = a.find_candidates(&c);
    }
    let exits = a.take_exit_intents();
    assert!(
        exits.is_empty(),
        "mid 0.31 does not clear the fee band: {exits:?}"
    );
    let _ = a.take_breaks();

    // FEE BAND CLEAR: mid 0.34 >= ~0.3214 → exactly one exit.
    let mut b = adapter_with_fees();
    armed_setup(&mut b, UP, &flat_spot(NOW, 32));
    let above = token_book(
        UP,
        dec!(0.33),
        dec!(0.35),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    b.on_book(UP, &above, NOW + 32_000);
    let above_fresh = move |t: &str| (t == UP).then(|| above.clone());
    let held_above = held_up(dec!(0.28));
    let above_ctx = ctx(&markets, 600, NOW + 32_000, &above_fresh).with_positions(&held_above);
    let _ = b.find_candidates(&above_ctx);
    let exits = b.take_exit_intents();
    assert_eq!(exits.len(), 1, "mid 0.34 clears the band: {exits:?}");
    assert_eq!(exits[0].token_id, UP);
    assert!(
        exits[0].reason.contains("fee band"),
        "reason should name the fee band: {}",
        exits[0].reason
    );
}

/// No fee schedule in force (`bk.fees()` = nil): the band cannot be computed
/// → fail closed, no exit ever fires (a strategy that prices fees must
/// never assume zero).
#[test]
fn no_fee_schedule_fails_closed_no_exit() {
    let mut a = adapter(); // no set_fee_schedule
    let book = armed_setup(&mut a, UP, &flat_spot(NOW, 32));
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let moon = token_book(
        UP,
        dec!(0.69),
        dec!(0.71),
        dec!(400),
        dec!(400),
        NOW + 32_000,
    );
    a.on_book(UP, &moon, NOW + 32_000);
    let moon_fresh = move |t: &str| (t == UP).then(|| moon.clone());
    let held = held_up(dec!(0.28));
    {
        let c = ctx(&markets, 600, NOW + 32_000, &moon_fresh).with_positions(&held);
        let _ = a.find_candidates(&c);
    }
    let _ = fresh; // the pre-arm book; the moon book replaces it for the close
    let exits = a.take_exit_intents();
    assert!(
        exits.is_empty(),
        "unknown fee curve = no band = no taker exit: {exits:?}"
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

/// One thesis in flight: while a thesis is armed (target not yet run away,
/// chip still calm, holdings flat) the next evaluate arms nothing new.
#[test]
fn a_second_thesis_waits_while_one_is_in_flight() {
    let mut a = adapter();
    let book = armed_setup(&mut a, UP, &flat_spot(NOW, 32));
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert_eq!(got.len(), 1, "setup must arm: {got:?}");
    let got2 = candidates_of(&mut a, &markets, 600, NOW + 32_000, &fresh);
    assert!(got2.is_empty(), "one in flight, no second thesis: {got2:?}");
    assert!(
        a.take_exit_intents().is_empty(),
        "no close before the target"
    );
}
