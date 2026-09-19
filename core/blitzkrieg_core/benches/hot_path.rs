//! E14 hot-path benches (criterion) — the per-tick trading loop.
//!
//! These measure the exact chain production traffic walks every `tick_ms`
//! (default 50ms): market data in (`engine_on_data` → books mirror + resting
//! maker + strategy books), evaluation out (`engine_evaluate` → fresh-book
//! snapshots → candidate dispatch → gates), and the periodic `tick()` sweep
//! (exit checks + escalation) with an open position.
//!
//! The hosted strategy is the same test adapter the unit tests use
//! (`strategies::test_support`, enabled by default features), so bench numbers
//! reflect real strategy work, not an empty engine.
//!
//! Run: cargo bench -p blitzkrieg-core --bench hot_path

use blitzkrieg_core::engine::{DataEvent, Engine, EngineConfig};
use blitzkrieg_core::model::{CryptoMarket, OrderRequest, Side};
use blitzkrieg_core::risk::RiskConfig;
use blitzkrieg_core::service::{Core, CoreConfig};
use blitzkrieg_core::strategies::test_support::TestSpreadArb;
use blitzkrieg_core::signal::TrendConfig;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const TOKENS: [&str; 4] = ["tok-btc-up", "tok-eth-up", "tok-sol-up", "tok-xrp-up"];
const DEPTH: usize = 20;

/// Fixed fake wall clock, 60s into its 900s round: the round-age gate
/// (>=30s) and time-left gate (>=180s) both pass, books are always fresh,
/// and — unlike a ticking clock — the round can never expire mid-benchmark,
/// which would silently short-circuit the very paths being measured.
const NOW: i64 = 1_700_000_160_000;

fn now() -> i64 {
    NOW
}

/// A book with `DEPTH` levels per side around the given mid, size 100,
/// spread 2 ticks (best bid = mid-0.01, best ask = mid+0.01).
fn book(mid: Decimal, depth: usize) -> (Vec<(Decimal, Decimal)>, Vec<(Decimal, Decimal)>) {
    let tick = dec!(0.01);
    let bids = (0..depth)
        .map(|i| (mid - tick * Decimal::from((i + 1) as u64), dec!(100)))
        .collect();
    let asks = (0..depth)
        .map(|i| (mid + tick * Decimal::from((i + 1) as u64), dec!(100)))
        .collect();
    (bids, asks)
}

/// A Core with the spread_arb test strategy registered and ENABLED, a live
/// round for the four bench tokens, and fresh books — the steady state the
/// 50ms production tick loop lives in.
fn bench_core() -> Core {
    let cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        round_duration_sec: 900,
        ..Default::default()
    };
    let mut c = Core::new(cfg);
    let mut engine = Engine::new(EngineConfig::default());
    engine
        .register_user_strategy(
            Box::new(TestSpreadArb::new(TrendConfig::default(), Default::default())),
            "bench".into(),
        )
        .expect("bench strategy registers");
    engine.set_strategy_enabled("spread_arb", true);
    c.enable_engine(engine);

    let n = now();
    let slot = n / 1000 / 900;
    // Round horizon pinned far beyond any bench run so the round-age and
    // time-left gates stay open for the whole measurement.
    let end = n + 100 * 3600 * 1000;
    let markets: Vec<CryptoMarket> = TOKENS
        .iter()
        .map(|t| CryptoMarket {
            asset: t.split('-').next().unwrap_or("BTC").to_uppercase(),
            condition_id: format!("cond-{t}"),
            question_id: format!("q-{t}"),
            up_token_id: t.to_string(),
            down_token_id: format!("down-{t}"),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: end,
            round_slot: slot,
            neg_risk: true,
            question: "?".into(),
        })
        .collect();
    c.engine_on_data(DataEvent::RoundMarkets { markets, now_ms: n }, n);
    for (i, t) in TOKENS.iter().enumerate() {
        let (bids, asks) = book(dec!(0.45) - dec!(0.02) * Decimal::from(i as u64), DEPTH);
        c.engine_on_data(
            DataEvent::Book {
                token_id: t.to_string(),
                bids,
                asks,
                now_ms: n,
            },
            n,
        );
    }
    c
}

/// One open taker position on the first token, entered through the real
/// `place()` path so positions/OME/ledger all carry real state. The entry
/// price is the book's best ask; the book never moves afterwards, so the
/// position is slightly underwater and no TP/SL fires — it stays open for
/// the whole measurement (asserted below).
fn open_position(c: &mut Core) {
    let n = now();
    let slot = n / 1000 / 900;
    c.place(
        OrderRequest {
            token_id: TOKENS[0].to_string(),
            condition_id: "cond-tok-btc-up".into(),
            side: Side::Buy,
            mode: blitzkrieg_core::model::FillPolicy::Taker,
            price: dec!(0.46),
            size: dec!(10),
            internal_key: "bench-k".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: slot,
        },
        0,
        n,
    )
    .expect("bench position opens");
    feed_books(c);
    c.tick(n).expect("bench tick runs");
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "bench fixture must hold ONE open position — a fixture that instantly \
         exits would measure an empty book sweep, not the position loop"
    );
}

/// Feed every token a fresh Book event (the feed-pump shape: one book per
/// token per cadence tick).
fn feed_books(c: &mut Core) {
    let n = now();
    for (i, t) in TOKENS.iter().enumerate() {
        let mid = dec!(0.45) - dec!(0.02) * Decimal::from(i as u64);
        let (bids, asks) = book(mid, DEPTH);
        c.engine_on_data(
            DataEvent::Book {
                token_id: t.to_string(),
                bids,
                asks,
                now_ms: n,
            },
            n,
        );
    }
}

/// Strategy book application: LocalBook apply + the per-strategy snapshot
/// copy, per book event. This is where depth-sized Vec allocations live.
fn bench_book_apply(c: &mut Criterion) {
    let mut g = c.benchmark_group("hot_path.book_apply");
    for depth in [5usize, 20] {
        g.bench_with_input(
            BenchmarkId::new("depth", depth),
            &depth,
            |b, &depth| {
                let mut c = bench_core();
                b.iter_batched(
                    || {
                        let n = now();
                        TOKENS
                            .iter()
                            .map(|t| {
                                let (bids, asks) = book(dec!(0.45), depth);
                                DataEvent::Book {
                                    token_id: t.to_string(),
                                    bids,
                                    asks,
                                    now_ms: n,
                                }
                            })
                            .collect::<Vec<_>>()
                    },
                    |events| {
                        for ev in events {
                            c.engine_on_data(ev, now());
                        }
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    g.finish();
}

/// The evaluate half of the 50ms tick loop: fresh-book closure, strategy
/// candidate dispatch, kernel gates.
fn bench_evaluate(c: &mut Criterion) {
    let mut g = c.benchmark_group("hot_path.evaluate");
    for tokens in [1usize, 4] {
        let mut c0 = bench_core();
        // Drop to the requested token count by overwriting the round.
        if tokens == 1 {
            let n = now();
            let slot = n / 1000 / 900;
            let end = n + 100 * 3600 * 1000;
            c0.engine_on_data(
                DataEvent::RoundMarkets {
                    markets: vec![CryptoMarket {
                        asset: "BTC".into(),
                        condition_id: "cond-tok-btc-up".into(),
                        question_id: "q".into(),
                        up_token_id: TOKENS[0].into(),
                        down_token_id: "down".into(),
                        up_price: dec!(0.6),
                        down_price: dec!(0.4),
                        expires_at_ms: end,
                        round_slot: slot,
                        neg_risk: true,
                        question: "?".into(),
                    }],
                    now_ms: n,
                },
                n,
            );
        }
        g.bench_function(BenchmarkId::new("tokens", tokens), |b| {
            b.iter(|| {
                std::hint::black_box(c0.engine_evaluate(now()));
            });
        });
    }
    g.finish();
}

/// One book-event sweep across all four tokens through the FULL core path
/// (books mirror + resting maker scan + engine apply + per-strategy work).
fn bench_engine_on_data(c: &mut Criterion) {
    let mut g = c.benchmark_group("hot_path.engine_on_data");
    g.bench_function("4tok", |b| {
        let mut c = bench_core();
        b.iter(|| {
            feed_books(&mut c);
            std::hint::black_box(&c);
        });
    });
    g.finish();
}

/// The `tick()` sweep with one open position: exit checks (the books mirror
/// value path), escalation scans, accounting.
fn bench_tick_with_position(c: &mut Criterion) {
    let mut g = c.benchmark_group("hot_path.tick");
    g.bench_function("1pos", |b| {
        let mut c = bench_core();
        open_position(&mut c);
        b.iter(|| {
            let _ = std::hint::black_box(c.tick(now()));
        });
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_book_apply,
    bench_evaluate,
    bench_engine_on_data,
    bench_tick_with_position
);
criterion_main!(benches);
