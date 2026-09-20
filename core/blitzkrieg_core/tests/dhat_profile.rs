//! E14 allocation accounting (dhat) — the hot-path load the production
//! traffic walks every tick, measured in allocation bytes/blocks rather than
//! process RSS.
//!
//! Issue #96 acceptance names dhat as the memory tool; RSS probes
//! (`scripts/e14-memory-baseline.mjs`) stay as the system-level view, but
//! allocator page-return behaviour makes RSS a noisy acceptance metric —
//! allocation volume is what the code actually controls.
//!
//! Run (release so the numbers reflect the shipped build):
//!   cargo test -p blitzkrieg-core --release --test dhat_profile
//! dhat writes `dhat-heap.json` to the workspace root at profiler exit.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use blitzkrieg_core::engine::{DataEvent, Engine, EngineConfig};
use blitzkrieg_core::model::{CryptoMarket, FillPolicy, OrderRequest, Side};
use blitzkrieg_core::risk::RiskConfig;
use blitzkrieg_core::service::{Core, CoreConfig};
use blitzkrieg_core::signal::TrendConfig;
use blitzkrieg_core::strategies::test_support::TestSpreadArb;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const TOKENS: [&str; 4] = ["tok-btc-up", "tok-eth-up", "tok-sol-up", "tok-xrp-up"];
const DEPTH: usize = 20;
const BOOK_EVENTS: usize = 1000;
const EVALUATE_CYCLES: usize = 1000;
const TICKS: usize = 200;

const NOW: i64 = 1_700_000_160_000;

type BookSide = Vec<(Decimal, Decimal)>;
fn book(mid: Decimal, depth: usize) -> (BookSide, BookSide) {
    let tick = dec!(0.01);
    let bids = (0..depth)
        .map(|i| (mid - tick * Decimal::from((i + 1) as u64), dec!(100)))
        .collect();
    let asks = (0..depth)
        .map(|i| (mid + tick * Decimal::from((i + 1) as u64), dec!(100)))
        .collect();
    (bids, asks)
}

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
            Box::new(TestSpreadArb::new(
                TrendConfig::default(),
                Default::default(),
            )),
            "bench".into(),
        )
        .expect("bench strategy registers");
    engine.set_strategy_enabled("spread_arb", true);
    c.enable_engine(engine);

    let n = NOW;
    let slot = n / 1000 / 900;
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

fn feed_books(c: &mut Core) {
    let n = NOW;
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

#[test]
fn hot_path_allocation_profile() {
    // Absolute output path: the workspace root's `docs/perf/` archive dir, so
    // the profile survives whatever cwd the test harness runs under.
    let out =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/perf/dhat-heap.json");
    // Default (at-exit) output mode: `testing()` suppressed the write on this
    // toolchain build, and one profile per test binary is all we need.
    let profiler = dhat::Profiler::builder().file_name(out.clone()).build();
    println!("dhat cwd: {:?}", std::env::current_dir());
    println!("dhat output: {}", out.display());
    {
        let mut c = bench_core();
        // Book ingest: the feed-pump shape (4 tokens refreshed per cadence tick).
        for _ in 0..BOOK_EVENTS {
            feed_books(&mut c);
        }
        // Evaluation: the strategy half of the tick loop.
        for _ in 0..EVALUATE_CYCLES {
            std::hint::black_box(c.engine_evaluate(NOW));
        }
        // Position sweep with one open position, entered via the real place path.
        c.place(
            OrderRequest {
                token_id: TOKENS[0].to_string(),
                condition_id: "cond-tok-btc-up".into(),
                side: Side::Buy,
                mode: FillPolicy::Taker,
                price: dec!(0.46),
                size: dec!(10),
                internal_key: "dhat-k".into(),
                strategy: "spread_arb".into(),
                asset: "BTC".into(),
                direction: "up".into(),
                round_slot: NOW / 1000 / 900,
            },
            0,
            NOW,
        )
        .expect("profile position opens");
        for _ in 0..TICKS {
            let _ = c.tick(NOW);
        }
        assert_eq!(
            c.positions().open_positions().len(),
            1,
            "fixture must hold the open position"
        );
    }
    drop(profiler); // dhat-heap.json is emitted here
}
