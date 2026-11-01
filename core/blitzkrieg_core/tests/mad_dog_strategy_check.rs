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
use blitzkrieg_market_api::{MarketStructure, MarketType};
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
        round_duration_sec: 900,
        archive_verdict: false,
        venue: String::new(),
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

/// A book whose two sides carry DIFFERENT depths: the adapter's obi wire
/// string is then a rust_decimal division that does not terminate — the
/// 28-significant-digit shape the frozen archive's full-depth books emit.
fn depth_book(
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

/// The declaration the E27 handshake reads: prediction / binary wheel, and
/// NOTHING else — a strategy declares WHERE it works, never which capability
/// bits it demands (that surface is removed; it once refused every
/// binary-market strategy on any venue whose plugin declared fewer bits than
/// the copied template asked for).
#[test]
fn modes_declare_type_and_structure_only() {
    let loaded = load_lua_package(&package_dir()).expect("package loads");
    assert_eq!(loaded.name, "mad_dog");
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

/// The DOWN side is its own token: a wick on `down` suggests DOWN.
#[test]
#[ignore = "probe: needs a local spot corpus (BK_PROBE_CORPUS, default \
/tmp/bk-spot-corpus/range-20260919T1600Z.jsonl) that CI does not carry — \
run explicitly where the corpus exists"]
fn probe_spot_corpus_with_evals() {
    use blitzkrieg_core::kline::KlineAggregator;
    use blitzkrieg_core::strategies::EngineStrategy as _;
    use std::collections::HashMap;
    use std::io::BufRead;

    let corpus = std::env::var("BK_PROBE_CORPUS")
        .unwrap_or_else(|_| "/tmp/bk-spot-corpus/range-20260919T1600Z.jsonl".into());
    let file = std::fs::File::open(&corpus).expect("corpus file");

    let loaded = load_lua_package(&package_dir()).expect("package loads");
    let mut a = LuaEngineAdapter::new(loaded.strategy, loaded.tunables);
    let mut agg = KlineAggregator::with_default_intervals();
    let mut last_books: HashMap<String, OrderbookSnapshot> = HashMap::new();
    let mut cur_markets: Vec<CryptoMarket> = Vec::new();
    let mut last_eval_ms: i64 = 0;
    let mut evals = 0usize;
    let mut first_err: Option<String> = None;

    for line in std::io::BufReader::new(file).lines() {
        let line = line.expect("line");
        let v: serde_json::Value = serde_json::from_str(&line).expect("json");
        let at = v["at"].as_i64().expect("at");
        match v["k"].as_str().expect("k") {
            "book" => {
                let token = v["t"].as_str().expect("t").to_string();
                let parse_side = |key: &str| -> Vec<(Decimal, Decimal)> {
                    v[key]
                        .as_array()
                        .map(|ls| {
                            ls.iter()
                                .filter_map(|l| {
                                    let arr = l.as_array()?;
                                    Some((
                                        Decimal::from_str_exact(arr[0].as_str()?).ok()?,
                                        Decimal::from_str_exact(arr[1].as_str()?).ok()?,
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let (bids, asks) = (parse_side("b"), parse_side("a"));
                let snap = OrderbookSnapshot::from_levels(token.clone(), bids, asks, at);
                a.on_book(&token, &snap, at);
                last_books.insert(token.clone(), snap);
                agg.on_trade(&token, last_books[&token].mid_price, Decimal::ONE, at);
            }
            "spot" => {
                let asset = v["s"].as_str().expect("s").to_string();
                let price = Decimal::from_str_exact(v["p"].as_str().expect("p")).expect("price");
                agg.on_trade(&asset, price, Decimal::ONE, at);
            }
            "round" => {
                let ms: Vec<CryptoMarket> =
                    serde_json::from_value(v["m"].clone()).expect("markets");
                if let Some(m) = ms.first() {
                    let tl = ((m.expires_at_ms - at) / 1000).max(0);
                    a.on_round(m.round_slot, tl, at);
                    cur_markets = ms;
                }
            }
            _ => {}
        }
        for bar in agg.take_closed() {
            a.on_kline(&bar);
        }
        if at - last_eval_ms >= 50 && !cur_markets.is_empty() {
            last_eval_ms = at;
            evals += 1;
            let lb = last_books.clone();
            let fresh = move |t: &str| lb.get(t).cloned();
            let ctx = StrategyCtx::new(
                &cur_markets,
                cur_markets[0].round_slot,
                ((cur_markets[0].expires_at_ms - at) / 1000).max(0),
                at,
                &fresh,
            );
            let got = a.find_candidates(&ctx);
            if first_err.is_none() {
                let markets2 = cur_markets.clone();
                let fresh2 = |_t: &str| None;
                let ctx2 = StrategyCtx::new(&markets2, cur_markets[0].round_slot, 600, at, &fresh2);
                let diag = a.diagnostics(&ctx2);
                let errs = diag
                    .first()
                    .and_then(|d| d.get("errors"))
                    .and_then(|e| e.as_i64())
                    .unwrap_or(0);
                if errs > 0 {
                    first_err = diag
                        .first()
                        .and_then(|d| d.get("lastError"))
                        .and_then(|e| e.as_str().map(str::to_string));
                }
            }
            let _ = got;
        }
    }

    let markets = vec![market()];
    let fresh = |_t: &str| None;
    let ctx = StrategyCtx::new(&markets, NOW / 900_000, 600, NOW, &fresh);
    let diag = a.diagnostics(&ctx);
    println!("EVALS: {evals}  DIAG: {diag:?}");
    let errs = diag
        .first()
        .and_then(|d| d.get("errors"))
        .and_then(|e| e.as_i64())
        .unwrap_or(-1);
    let last = diag
        .first()
        .and_then(|d| d.get("lastError"))
        .and_then(|e| e.as_str().map(str::to_string))
        .or(first_err);
    assert!(
        errs == 0,
        "lua callback errors: {errs}, lastError: {last:?}"
    );
}

// ── regression: the archive's real books break the decimal helpers ─────────
//
// The frozen-archive books are FULL depth, and the adapter's `obi` /
// `spread_pct` are rust_decimal DIVISION results — up to 28 significant
// digits ("-0.4090909090909090909090909091"). The first cut of the decimal
// helpers rescaled to the max scale against an 18-entry POW10 table, so
// every evaluation touching such a book died mid-body and its suggestion
// was silently lost (signals=0 in the spot-corpus replay while the same
// strategy fired in the tests, whose synthetic books had equal depths and
// therefore obi = 0). These cases pin the exact shapes that broke it.

/// A wick book whose obi string is a 28-digit rust_decimal division output.
/// The strategy must still suggest — and the diagnostics must stay clean.
#[test]
fn fires_on_full_depth_books_with_28_digit_obi() {
    let mut a = adapter();
    feed_dominance(&mut a, UP, NOW, 22, dec!(0.56));
    feed_spot(&mut a, NOW + 12_000, &flat_spot(NOW + 12_000, 10));
    let wick_ts = NOW + 23_000;
    // Full-depth book: bid_depth 15234.5 vs ask_depth 22078.66 — the ratio
    // does not terminate, so the adapter's obi wire string is a recurring
    // rust_decimal division (28 significant digits).
    let wick = depth_book(
        UP,
        dec!(0.27),
        dec!(0.29),
        dec!(15234.5),
        dec!(22078.66),
        wick_ts,
    );
    a.on_book(UP, &wick, wick_ts);

    let markets = vec![market()];
    let wick_for_fresh = wick.clone();
    let fresh = move |t: &str| (t == UP).then(|| wick_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 600, wick_ts, &fresh);
    assert_eq!(
        got.len(),
        1,
        "28-digit obi must not eat the suggestion: {got:?}"
    );
    assert_eq!(got[0].price, dec!(0.26));

    let ctx = StrategyCtx::new(&markets, NOW / 900_000, 600, wick_ts, &fresh);
    let diag = a.diagnostics(&ctx);
    let errs = diag
        .first()
        .and_then(|d| d.get("errors"))
        .and_then(|e| e.as_i64())
        .unwrap_or(-1);
    assert_eq!(errs, 0, "zero Lua callback errors, diagnostics: {diag:?}");
}

/// Extreme OBI in the 28-digit wire form must still be REFUSED by the band
/// (a refused-by-gate book and a crashed evaluation must stay
/// distinguishable: the crash lost suggestions, the gate refuses them).
#[test]
fn refuses_28_digit_obi_inside_the_band_edge() {
    let mut a = adapter();
    feed_dominance(&mut a, UP, NOW, 22, dec!(0.56));
    let wick_ts = NOW + 23_000;
    // |obi| = 0.85… > 0.5 with full depth either way.
    let skewed = depth_book(
        UP,
        dec!(0.27),
        dec!(0.29),
        dec!(85234.5),
        dec!(15078.66),
        wick_ts,
    );
    a.on_book(UP, &skewed, wick_ts);
    let markets = vec![market()];
    let skewed_for_fresh = skewed.clone();
    let fresh = move |t: &str| (t == UP).then(|| skewed_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 600, wick_ts, &fresh);
    assert!(got.is_empty(), "OBI band must refuse: {got:?}");
    let ctx = StrategyCtx::new(&markets, NOW / 900_000, 600, wick_ts, &fresh);
    let diag = a.diagnostics(&ctx);
    let errs = diag
        .first()
        .and_then(|d| d.get("errors"))
        .and_then(|e| e.as_i64())
        .unwrap_or(-1);
    assert_eq!(
        errs, 0,
        "refusal must come from the gate, not a crash: {diag:?}"
    );
}

/// `obi`/`spread_pct` scale explosion also flows through `bid_depth` sums.
/// A depth string with 12 fractional digits must compare cleanly.
#[test]
fn fires_with_long_fractional_depth() {
    let mut a = adapter();
    feed_dominance(&mut a, UP, NOW, 22, dec!(0.56));
    feed_spot(&mut a, NOW + 12_000, &flat_spot(NOW + 12_000, 10));
    let wick_ts = NOW + 23_000;
    let wick = depth_book(
        UP,
        dec!(0.27),
        dec!(0.29),
        Decimal::from_str_exact("15234.523423423423").expect("depth"),
        Decimal::from_str_exact("22078.661234123412").expect("depth"),
        wick_ts,
    );
    a.on_book(UP, &wick, wick_ts);
    let markets = vec![market()];
    let wick_for_fresh = wick.clone();
    let fresh = move |t: &str| (t == UP).then(|| wick_for_fresh.clone());
    let got = candidates_of(&mut a, &markets, 600, wick_ts, &fresh);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].price, dec!(0.26));
}

// ── regression: spot_ok across the five acceptance domains (issue #847) ────
//
// ff311c8 replaced the float move ratio `diff.m / (POW10[diff.s] * oldest.m)
// * 100` — which reads POW10[oldest.s]x too small whenever the OLDEST close
// carries a scale (a -5% waterfall on a scale-2 close read -0.05%, so the
// veto never fired) and whose denominator product itself wrapped i64 at the
// top of the declared domain — with the exact rational cross-multiplication
// `|diff| * 100 >= thr * oldest`, threshold included. These rows pin the
// fixed verdicts on the REAL package (loader + sandbox + adapter): every row
// isolates the spot gate (dominance latched, fast wick armed, deep neutral
// book, inside the time window, no account view), so the only thing that can
// flip the verdict is spot_ok.
//
// Rows marked [old-red] fail on ff311c8^ by construction: the old float form
// reads the move POW10[oldest.s]x small there. The window geometry is shared
// and self-checking: a slip that drops the oldest bar turns any two_step
// pair into a flat diff (diff.m == 0 → pass), which fails every veto row
// loudly.

/// Spot-window geometry shared by every row below: ten sec1 bars opening at
/// `SPOT_OPEN`, evaluated at `SPOT_NOW`. Bar 0 closes 499ms INSIDE the
/// window cutoff (`SPOT_NOW - spot_window_sec·1s`), bar 9 closes 501ms
/// before now, so the gate's window pair is EXACTLY closes[0] → closes[9]
/// and each row can state the move the gate sees.
const SPOT_OPEN: i64 = NOW + 12_500;
const SPOT_NOW: i64 = NOW + 23_000;

/// Exact decimal from a string literal: unlike the `dec!` macro (numeric
/// literals only), this keeps EXPLICIT trailing scales ("950.00" stays
/// scale 2, "9.5000" stays scale 4) — the (mantissa, scale) pairs are the
/// point of the scale rows below.
fn d(s: &str) -> Decimal {
    Decimal::from_str_exact(s).expect("valid decimal literal")
}

/// Ten closes whose window pair is exactly (`first` → `rest`): `first` at
/// the oldest in-window bar, `rest` for the other nine.
fn window_pair(first: Decimal, rest: Decimal) -> Vec<Decimal> {
    let mut v = vec![first];
    v.resize(10, rest);
    v
}

/// Arm `token` (22s hold-dominance at 0.56, then a fast wick to 0.28) with
/// `closes` as the spot window, and evaluate at SPOT_NOW with every non-spot
/// gate satisfied. The suggestions ARE the spot gate's verdict: one row =
/// pass (resting bid at 0.26), empty = veto.
fn spot_verdict(
    adapter: &mut LuaEngineAdapter,
    token: &str,
    closes: &[Decimal],
) -> Vec<blitzkrieg_core::signal::TradeSignal> {
    feed_dominance(adapter, token, NOW, 22, dec!(0.56));
    feed_spot(adapter, SPOT_OPEN, closes);
    let wick = book(dec!(0.27), dec!(0.29), dec!(600), SPOT_NOW);
    adapter.on_book(token, &wick, SPOT_NOW);
    let markets = vec![market()];
    let wick_for_fresh = wick.clone();
    let fresh = move |t: &str| (t == token).then(|| wick_for_fresh.clone());
    candidates_of(adapter, &markets, 600, SPOT_NOW, &fresh)
}

/// Lua callback errors the adapter recorded (0 = no crash mid-evaluation;
/// the adapter swallows evaluation errors into diagnostics).
fn lua_errors(adapter: &mut LuaEngineAdapter) -> i64 {
    let markets = vec![market()];
    let fresh = |_t: &str| None;
    let ctx = StrategyCtx::new(&markets, NOW / 900_000, 600, SPOT_NOW, &fresh);
    let diag = adapter.diagnostics(&ctx);
    diag.first()
        .and_then(|d| d.get("errors"))
        .and_then(|e| e.as_i64())
        .unwrap_or(-1)
}

// ── domain 1: forward AND reverse move, each vetoing only its own side ──────

/// [old-red] A -1% waterfall on scale-2 closes vetoes the UP buy, and a +1%
/// pump on scale-2 closes vetoes the DOWN buy. The old float form read both
/// moves as ±0.01% (POW10[2] too small) and bought through either one.
#[test]
fn spot_waterfall_vetoes_up_and_pump_vetoes_down() {
    let mut a = adapter();
    // Window pair 100.00 → 99.00: move = -1% ≤ -thr → waterfall → UP veto.
    let closes = window_pair(d("100.00"), d("99.00"));
    assert!(
        spot_verdict(&mut a, UP, &closes).is_empty(),
        "a waterfall must veto the UP buy"
    );

    let mut b = adapter();
    // Window pair 100.00 → 101.00: move = +1% ≥ +thr → pump → DOWN veto.
    let closes = window_pair(d("100.00"), d("101.00"));
    assert!(
        spot_verdict(&mut b, DOWN, &closes).is_empty(),
        "a pump must veto the DOWN buy"
    );
}

/// Direction symmetry: the SAME adverse move does not veto the OPPOSITE
/// side — a waterfall is confirmation for the DOWN buy, a pump for the UP
/// buy (normal spot data present → pass, the other half of the
/// spot_missing domain).
#[test]
fn spot_vetoes_are_direction_symmetric() {
    let mut a = adapter();
    let closes = window_pair(d("100.00"), d("99.00"));
    assert_eq!(
        spot_verdict(&mut a, DOWN, &closes).len(),
        1,
        "the same waterfall must NOT veto the DOWN buy"
    );

    let mut b = adapter();
    let closes = window_pair(d("100.00"), d("101.00"));
    assert_eq!(
        spot_verdict(&mut b, UP, &closes).len(),
        1,
        "the same pump must NOT veto the UP buy"
    );
}

// ── domain 2: threshold EQUALITY boundary (the contract is >=, not >) ───────

/// [old-red] A move of EXACTLY thr must veto (|diff|·100 == thr·oldest).
/// Scale-2 closes also make the old float form read -0.05% here.
#[test]
fn spot_move_exactly_at_threshold_vetoes() {
    let mut a = adapter();
    // 100.00 → 99.50 is EXACTLY -0.5% = spot_max_move_pct (manifest default).
    let closes = window_pair(d("100.00"), d("99.50"));
    assert!(
        spot_verdict(&mut a, UP, &closes).is_empty(),
        "move == thr sits inside the veto (threshold included)"
    );
}

/// The mirror edge: just UNDER the threshold passes. Proves the equality
/// veto above is the boundary, not an over-broad gate.
#[test]
fn spot_move_just_under_threshold_passes() {
    let mut a = adapter();
    // 100.00 → 99.51 = -0.49% < 0.5%: |diff|·100 < thr·oldest, exactly.
    let closes = window_pair(d("100.00"), d("99.51"));
    let got = spot_verdict(&mut a, UP, &closes);
    assert_eq!(got.len(), 1, "-0.49% is calm: {got:?}");
    assert_eq!(got[0].price, dec!(0.26));
}

// ── domain 3: the SAME move in three (mantissa, scale) spellings ────────────

/// [old-red] THE scale-bug lesion: one -5% move written as 95000/90250 at
/// s=0, 950.00→902.50 at s=2 and 9.5000→9.0250 at s=4 — identical mantissa
/// pairs, POW10-shifted scales — must produce the byte-identical verdict
/// (VETO) at every scale. The old form read s=2 as -0.05% and s=4 as
/// -0.0005% and bought every knife at scale ≥ 1.
#[test]
fn spot_same_move_across_scales_is_bit_identical_veto() {
    for (oldest, newest) in [
        (dec!(95000), dec!(90250)), // s=0
        (d("950.00"), d("902.50")), // s=2, same mantissas
        (d("9.5000"), d("9.0250")), // s=4, same mantissas
    ] {
        let mut a = adapter();
        let closes = window_pair(oldest, newest);
        assert!(
            spot_verdict(&mut a, UP, &closes).is_empty(),
            "-5% spelled at scale {oldest} must veto the UP buy"
        );
    }
}

/// The pass direction must not drift either: the same calm +0.0105% move in
/// the same three spellings lets the entry through at the IDENTICAL price —
/// equivalent fractions stay equivalent across scales.
#[test]
fn spot_calm_move_across_scales_is_bit_identical_pass() {
    for (oldest, newest) in [
        (dec!(95000), dec!(95010)), // s=0
        (d("950.00"), d("950.10")), // s=2, same mantissas
        (d("9.5000"), d("9.5010")), // s=4, same mantissas
    ] {
        let mut a = adapter();
        let closes = window_pair(oldest, newest);
        let got = spot_verdict(&mut a, UP, &closes);
        assert_eq!(got.len(), 1, "calm {oldest} → {newest} must pass: {got:?}");
        assert_eq!(got[0].price, dec!(0.26), "the row must be byte-identical");
    }
}

// ── domain 4: spot_missing both ways (fail-closed default, disclosed knob) ──

/// NO spot bars at all: the default `spot_missing=block` refuses (cannot
/// confirm the underlying is calm → no knife), the disclosed
/// `spot_missing=pass` diagnostic knob arms the identical book-only shape.
/// With normal data present → pass is pinned by the symmetry rows above.
#[test]
fn spot_missing_block_refuses_and_pass_knob_arms() {
    let mut blocked = adapter();
    assert!(
        spot_verdict(&mut blocked, UP, &[]).is_empty(),
        "no spot data + default block must refuse"
    );

    let mut passing = adapter_with_params("spot_missing", "pass");
    let got = spot_verdict(&mut passing, UP, &[]);
    assert_eq!(got.len(), 1, "the pass knob arms the same shape: {got:?}");
    assert_eq!(got[0].price, dec!(0.26));
}

// ── domain 5: overflow upper bound — 1e5 prices at 10 decimals ──────────────

/// [old-red] The top of the declared domain at its most violent legal move:
/// 99999.9999999999 → 0.0000000001 (scale 10). The new products stay far
/// inside i64 (|diff|·100 ≈ 1e17, the aligned comparison ≈ 1e18 < 2^63) and
/// the verdict is a veto; the old form's denominator POW10[10]·oldest ≈ 1e25
/// wrapped i64 before the comparison. No Lua error may be swallowed either
/// way (a crashed evaluation must not masquerade as a pass).
#[test]
fn spot_max_magnitude_waterfall_vetoes_without_overflow() {
    let mut a = adapter();
    let closes = window_pair(d("99999.9999999999"), d("0.0000000001"));
    assert!(
        spot_verdict(&mut a, UP, &closes).is_empty(),
        "a ~-100% waterfall at max magnitude must veto the UP buy"
    );
    assert_eq!(
        lua_errors(&mut a),
        0,
        "no Lua callback error may be swallowed"
    );
}

/// The pump mirror at the same magnitude vetoes the DOWN buy, also without
/// overflow and without a swallowed error.
#[test]
fn spot_max_magnitude_pump_vetoes_down_without_overflow() {
    let mut a = adapter();
    let closes = window_pair(d("0.0000000001"), d("99999.9999999999"));
    assert!(
        spot_verdict(&mut a, DOWN, &closes).is_empty(),
        "an extreme pump on a near-zero oldest close must veto the DOWN buy"
    );
    assert_eq!(
        lua_errors(&mut a),
        0,
        "no Lua callback error may be swallowed"
    );
}

/// No MISjudgement at the top of the domain: a 1e-10-wide move on max
/// scale-10 closes is calm — the gate must still pass it and bid 0.26 (max
/// magnitude must not scare the veto).
#[test]
fn spot_max_scale_tiny_move_still_passes() {
    let mut a = adapter();
    let closes = window_pair(d("99999.9999999999"), d("99999.9999999998"));
    let got = spot_verdict(&mut a, UP, &closes);
    assert_eq!(got.len(), 1, "a 1e-10 move must not veto: {got:?}");
    assert_eq!(got[0].price, dec!(0.26));
    assert_eq!(
        lua_errors(&mut a),
        0,
        "no Lua callback error may be swallowed"
    );
}
