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
//!   cargo test -p blitzkrieg-core --release --test dhat_profile -- --nocapture
//!
//! WHERE THE PROFILE GOES (#206): `target/dhat/dhat-heap.json`, i.e. under the
//! build directory, plus a one-line summary on stdout so the number reaches the
//! CI log. It used to be written to `docs/perf/dhat-heap.json` — a TRACKED path
//! — so running this test dirtied the worktree and the only way to keep `git
//! status` clean was to `git checkout` the file afterwards. Worse, that made a
//! test artifact look like a curated baseline: the two archived profiles E14
//! actually cites (`dhat-heap-baseline.json`, `dhat-heap-round2.json`) are
//! hand-kept, and nothing in `scripts/` or `.github/` ever read any of them
//! (#206). The profile this test writes is therefore scratch; the archived
//! baseline is updated by a human who reads the summary and commits it.
//!
//! The totals are printed, not asserted: a dhat byte total is a function of the
//! toolchain, the profile and the target, so a threshold here would be a
//! platform-specific number pretending to be an invariant. The gate on memory
//! growth is `scripts/e14-memory-baseline.mjs` (peak RSS ceiling + ratio
//! against the archived baseline).

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

/// One book side: `depth` levels of `(price, size)`.
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

/// Where the profile is written: `<target>/dhat/dhat-heap.json`.
///
/// Derived from `current_exe()` rather than from `CARGO_MANIFEST_DIR` so it
/// follows whatever target directory the build actually used (`CARGO_TARGET_DIR`,
/// a `--target <triple>` layout, or the CI cache) and always lands on an
/// ignored path. `current_exe()` is `<target>/<profile>/deps/dhat_profile-<hash>`,
/// so three `parent()` hops reach the target directory root.
fn profile_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("test binary path");
    let target = exe
        .parent() // deps/
        .and_then(|p| p.parent()) // <profile>/
        .and_then(|p| p.parent()) // <target>/
        .expect("test binary lives at <target>/<profile>/deps/<name>");
    let dir = target.join("dhat");
    std::fs::create_dir_all(&dir).expect("create the dhat output dir");
    dir.join("dhat-heap.json")
}

/// Total allocated bytes/blocks, summed over dhat's program points — the same
/// aggregate the archived `docs/perf/*.json` numbers in E14 §8 were read from.
/// Returns `None` (with the reason printed) rather than panicking: a profile
/// that cannot be summarised must not turn into a mysterious test failure.
fn profile_totals(path: &std::path::Path) -> Option<(u64, u64, u64, u64)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            println!("dhat: cannot re-read {}: {e}", path.display());
            return None;
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            println!("dhat: cannot parse {}: {e}", path.display());
            return None;
        }
    };
    let mut total = (0u64, 0u64); // bytes, blocks
    let mut at_exit = (0u64, 0u64); // still-live (leaked) bytes, blocks
    for pp in v["pps"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let n = |k: &str| pp[k].as_u64().unwrap_or(0);
        total.0 += n("tb");
        total.1 += n("tbk");
        at_exit.0 += n("eb");
        at_exit.1 += n("ebk");
    }
    Some((total.0, total.1, at_exit.0, at_exit.1))
}

#[test]
fn hot_path_allocation_profile() {
    let out = profile_path();
    // Default (at-exit) output mode: `testing()` suppressed the write on this
    // toolchain build, and one profile per test binary is all we need.
    let profiler = dhat::Profiler::builder().file_name(out.clone()).build();
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
    drop(profiler); // the profile is emitted here

    // The summary line is the point of keeping this in CI: the profile file is
    // scratch under target/, so the numbers have to reach the log to be read by
    // anyone. Printed with `--nocapture` (CI runs `cargo test --workspace`, whose
    // captured output a failing run surfaces anyway).
    match profile_totals(&out) {
        Some((bytes, blocks, exit_bytes, exit_blocks)) => {
            println!(
                "dhat: total allocated {bytes} bytes ({:.2} MiB) in {blocks} blocks; \
                 live at exit {exit_bytes} bytes ({:.2} MiB) in {exit_blocks} blocks",
                bytes as f64 / (1024.0 * 1024.0),
                exit_bytes as f64 / (1024.0 * 1024.0),
            );
            // The file must exist and carry a real measurement — this much IS an
            // invariant: a profiler that silently stopped writing would leave the
            // memory evidence gone while the test stayed green.
            assert!(bytes > 0 && blocks > 0, "dhat profile is empty");
        }
        None => panic!("dhat profile at {} is unreadable", out.display()),
    }
    println!("dhat profile → {}", out.display());
}
