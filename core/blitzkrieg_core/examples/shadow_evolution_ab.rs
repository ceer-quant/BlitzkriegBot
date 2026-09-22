//! Deterministic A/B harness for Shadow Evolution (Task 1).
//!
//! Drives TWO full `Core`s (engine + OME + positions + shadow) through the SAME
//! scripted event timeline: identical ticks, identical timestamps, identical
//! initial parameters, identical starting balance. In-process so the harness owns
//! the clock — a 30-minute evaluation window replays in milliseconds, and the run
//! is fully deterministic (no RNG, no wall-clock, no network).
//!
//!   group A  = shadow evolution OFF (control)
//!   group B  = shadow evolution ON  (experiment)
//!
//! Scripted market: 70 synthetic UP tokens. A marginal minority (`i % 7 == 0`,
//! 10 tokens) takes a dip whose entry sits exactly at the default 0.45 cap and
//! then LOSES; the rest take a "deep" dip well under the cap and WIN. A variant
//! that tightens the cap (a directed single-knob mutation) skips the marginal
//! losers, so evolution has a real counterfactual to find — and, once it applies,
//! the live engine's own cap rises out of the losers' reach, so group B's
//! realised trades diverge from group A's. This mirrors the production symptom
//! the feature targets: entering at the cap is where the losses live.
//!
//! Fidelity fixes baked into the run (D-2/D-3): the shadow sees the SAME tick the
//! live engine just consumed (no one-tick lag); variants mutate ONE declared knob
//! at a time (not four in lockstep, which cancelled out); variants replay the
//! live `ExitConfig` (SL 12), not a fabricated 50% hard stop; and variant trade
//! history accumulates across round boundaries.
//!
//! E2-c (#28): a variant is no longer a kernel-side copy of spread_arb's entry
//! rule. It is a TWIN of the strategy itself (`ShadowFactory::make`), driven with
//! a counterfactual parameter bag. The manager owns one evolution unit per
//! strategy, each with its own parameter cell and its own audit file
//! (`data/evolution/<strategy>.jsonl`), so two strategies cannot cross-talk.
//!
//! Raw output under `docs/reports/data/`: trade CSV, evolution audit JSONL, a
//! machine-readable summary, and a safety-lock probe result. Run:
//!   cargo run --release -p blitzkrieg-core --example shadow_evolution_ab -- [out_dir]

use blitzkrieg_core::engine::{DataEvent, Engine, EngineConfig};
use blitzkrieg_core::model::CryptoMarket;
use blitzkrieg_core::position::PositionConfig;
use blitzkrieg_core::risk::RiskConfig;
use blitzkrieg_core::scanner::ScannerConfig;
use blitzkrieg_core::service::{Core, CoreConfig, ShadowEvolutionTuning};
use blitzkrieg_core::shadow_evolution::{MutableParams, StrategyParams};
use blitzkrieg_core::signal::{SpreadArbConfig, TrendConfig};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

const STEP_MS: i64 = 4_000; // 4s per scripted tick
const ROUND_SEC: i64 = 86_400; // ~1 day: far longer than the run so time-based exits never fire
const N_ASSETS: usize = 70;
/// The strategy under evolution, loaded from its own cdylib through the SAME
/// C ABI v2 dlopen path a third-party strategy uses. The kernel ships no
/// strategy and knows this name only as a string the harness passes in.
const STRATEGY: &str = "spread_arb";

/// Locate the shipped `spread_arb` cdylib. Release first (what CI builds), then
/// debug. Overridable with `BK_SPREAD_ARB_LIB` for an out-of-tree build.
fn spread_arb_lib() -> PathBuf {
    if let Some(p) = std::env::var_os("BK_SPREAD_ARB_LIB") {
        return PathBuf::from(p);
    }
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("user_layer")
        .join("strategies")
        .join("target");
    for profile in ["release", "debug"] {
        for name in [
            "libspread_arb_strategy.dylib",
            "libspread_arb_strategy.so",
            "spread_arb_strategy.dll",
        ] {
            let p = base.join(profile).join(name);
            if p.exists() {
                return p;
            }
        }
    }
    base.join("release").join("libspread_arb_strategy.dylib")
}

fn up_token(i: usize) -> String {
    format!("A{i:02}_UP")
}
fn down_token(i: usize) -> String {
    format!("A{i:02}_DOWN")
}

/// Build a full Core with the shared experiment configuration. `se_enabled`
/// toggles ONLY shadow evolution; every other knob is identical.
fn build_core(se_enabled: bool) -> Core {
    let assets: Vec<String> = (0..N_ASSETS).map(|i| format!("A{i:02}")).collect();
    // Position config: raise capacity and ZERO the cooldowns so a sequential
    // 40-token script is not throttled by the per-run position caps (which are
    // production defaults, not the property under test here). Exit policy itself
    // stays at the production default (SL 12 / trail 8 / TP 100).
    let positions = PositionConfig {
        max_positions: 64,
        max_daily_loss_usd: dec!(1_000_000),
        stop_loss_cooldown_sec: 0,
        exit_cooldown_sec: 0,
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        ..Default::default()
    };
    let mut cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(8),
            ..Default::default()
        },
        dry_seed_balance: dec!(100_000),
        engine_enabled: true,
        auto_exits_enabled: true,
        round_duration_sec: ROUND_SEC,
        assets: assets.clone(),
        min_round_age_sec: 0,
        trend_confirm_sec: 60,
        trend_window_floor_ms: 1_000,
        size_usd: dec!(2.5),
        min_shares: dec!(10),
        max_shares: dec!(10),
        discovery_enabled: false,
        positions,
        // Keep the breaker at its production default (3 consecutive losses):
        // the scripted outcomes alternate, so it is never tripped and no
        // safety behaviour is altered for the experiment.
        ..Default::default()
    };
    // Isolated logs: a synthetic experiment must never touch production data.
    cfg.trade_log_path = None;
    cfg.order_log_path = None;
    cfg.position_log_path = None;
    cfg.near_miss_path = None;
    cfg.shadow_evolution_enabled = se_enabled;
    // Task-mandated values: 30-min evaluation window, >=30 samples, >=5-min
    // observation, >=10-min cooldown, 5% gradient (all code defaults; pinned here
    // so the report can state them as facts rather than assumptions).
    cfg.shadow_evolution_tuning = Some(ShadowEvolutionTuning {
        min_sample_count: Some(30),
        min_observation_secs: Some(300),
        cooldown_secs: Some(600),
        // Nine variants = the baseline plus eight directed single-knob moves,
        // which is exactly two directions for each of spread_arb's four declared
        // knobs. Fewer variants would leave the cap knob unreachable in this run.
        variant_count: Some(9),
        ..Default::default()
    });
    let mut core = Core::new(cfg);
    let eng = Engine::new(EngineConfig {
        scanner: ScannerConfig {
            assets: assets.clone(),
            round_duration_sec: ROUND_SEC,
            min_round_age_sec: 0,
            min_time_left_sec: 0,
        },
        trend: TrendConfig {
            confirm_sec: 60,
            min_price: dec!(0.55),
            broken_price: dec!(0.35),
            ratio: dec!(0.8),
            window_floor_ms: 1_000,
        },
        spread_arb: SpreadArbConfig::default(),
        size_usd: dec!(2.5),
        min_shares: dec!(10),
        max_shares: dec!(10),
        ..Default::default()
    });
    core.enable_engine(eng);
    // PR-B: the kernel registers nothing, so the strategy under test enters the
    // way every strategy does — dlopen a C ABI v2 library. Build it first:
    //   (cd user_layer/strategies && cargo build --release)
    let lib = spread_arb_lib();
    let receipt = core.load_strategy_lib(&lib.to_string_lossy());
    assert!(
        receipt.contains("registered into the engine dispatch"),
        "loading {} failed: {receipt}",
        lib.display()
    );
    assert!(core.set_strategy_enabled(STRATEGY, true));
    if se_enabled {
        core.shadow_evolution_enable(0);
    }
    core
}

fn markets(slot: i64) -> Vec<CryptoMarket> {
    (0..N_ASSETS)
        .map(|i| CryptoMarket {
            asset: format!("A{i:02}"),
            condition_id: format!("cond{i}"),
            question_id: format!("q{i}"),
            up_token_id: up_token(i),
            down_token_id: down_token(i),
            up_price: dec!(0.5),
            down_price: dec!(0.5),
            expires_at_ms: (slot + 1) * ROUND_SEC * 1000,
            round_slot: slot,
            neg_risk: true,
            question: format!("A{i:02} up/down"),
        })
        .collect()
}

/// Feed one token's book to the full stack (engine+shadow, OME, exits, entries).
fn tick_token(core: &mut Core, token: &str, bid: Decimal, ask: Decimal, now: i64) {
    let bids = vec![(bid, dec!(500))];
    let asks = vec![(ask, dec!(500))];
    core.engine_on_data(
        DataEvent::Book {
            token_id: token.into(),
            bids: bids.clone(),
            asks: asks.clone(),
            now_ms: now,
        },
        now,
    );
    core.book_snapshot(token, bids, asks, now);
    let _ = core.tick(now);
    let _ = core.engine_evaluate(now);
}

/// Knobs whose value differs between two parameter bags, as `name:from->to`.
/// Addresses ONE strategy's namespace, so a record can only ever show that
/// strategy's own knobs.
fn changed_knobs(from: &MutableParams, to: &MutableParams, strategy: &str) -> Vec<String> {
    let empty = StrategyParams::new();
    let f = from.for_strategy(strategy).unwrap_or(&empty);
    let t = to.for_strategy(strategy).unwrap_or(&empty);
    let mut names: Vec<&str> = f.names();
    for n in t.names() {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    names
        .into_iter()
        .filter_map(|n| {
            let (fv, tv) = (f.get(n), t.get(n));
            if fv == tv {
                None
            } else {
                let show =
                    |v: Option<Decimal>| v.map(|v| v.to_string()).unwrap_or_else(|| "-".into());
                Some(format!("{n}:{}->{}", show(fv), show(tv)))
            }
        })
        .collect()
}

#[derive(Debug, Default)]
struct ManagerExp {
    baseline_wr: Decimal,
    baseline_pf: Decimal,
    baseline_n: u32,
    base_pnl: Decimal,
    best_variant_wr: Decimal,
    best_variant_pf: Decimal,
    best_variant_n: u32,
    best_variant_pnl: Decimal,
    applied: u64,
    rejected: u64,
    final_cap: Decimal,
    final_min_price: Decimal,
    trajectory: Vec<String>,
}

/// Drive the REAL `ShadowEvolution` component through a scenario where a
/// cap-tightened variant genuinely beats the baseline:
///   * "winner" tokens offer a dip at mid 0.44 (entry 0.42, under cap) then rally;
///   * "cap" tokens offer a dip at mid 0.47 clamped to the 0.45 cap, then collapse.
///     The baseline takes both; a variant whose cap is 3% lower skips the cap losers.
fn run_manager_experiment() -> ManagerExp {
    use blitzkrieg_core::exit_policy::ExitConfig;
    use blitzkrieg_core::model::OrderbookSnapshot;
    use blitzkrieg_core::shadow_evolution::ShadowEvolution;
    use blitzkrieg_core::shadow_evolution::config::{ImmutableConfig, ShadowEvolutionConfig};
    use blitzkrieg_core::strategies::EngineStrategy;

    let cfg = ShadowEvolutionConfig {
        enabled: true,
        evaluation_window_secs: 3600, // wide: the script spans ~900s
        min_sample_count: 30,
        min_win_rate_improvement: dec!(0.05),
        min_profit_factor_improvement: dec!(0.10),
        min_observation_secs: 300,
        cooldown_secs: 0,
        max_gradient: dec!(0.05),
        // Baseline + 8 directed moves = both directions of all four knobs.
        variant_count: 9,
        audit_dir: std::env::temp_dir()
            .join("shadow_ab_manager_audit")
            .to_string_lossy()
            .into_owned(),
        risk: ImmutableConfig::default(),
        exit_cfg: ExitConfig::default(),
        ..Default::default()
    };
    // The manager builds one unit per EVOLVABLE strategy, and a strategy is the
    // only thing that can declare its own knobs — so it needs a live instance.
    // PR-B: that instance comes from the shipped cdylib, exactly as it would in
    // production; the kernel has no builtin to hand it here.
    let loaded = blitzkrieg_core::strategy_engine::loader::load_foreign(&spread_arb_lib())
        .unwrap_or_else(|e| panic!("load spread_arb cdylib failed: {e:?}"));
    let strat = loaded.strategy;
    let mut se = ShadowEvolution::new(cfg, &[&strat as &dyn EngineStrategy]);
    se.enable(0);

    let n_tokens = 48usize;
    let mut now: i64 = 1_700_000_000_000;
    let market: Vec<CryptoMarket> = (0..n_tokens)
        .map(|i| CryptoMarket {
            asset: format!("M{i:02}"),
            condition_id: format!("mc{i}"),
            question_id: format!("mq{i}"),
            up_token_id: format!("M{i:02}_UP"),
            down_token_id: format!("M{i:02}_DOWN"),
            up_price: dec!(0.5),
            down_price: dec!(0.5),
            expires_at_ms: now + 86_400_000,
            round_slot: 1,
            neg_risk: true,
            question: "m".into(),
        })
        .collect();
    se.on_round(&market, &[], now);

    let book = |bid: Decimal, ask: Decimal, ts: i64| {
        OrderbookSnapshot::from_levels("t", vec![(bid, dec!(500))], vec![(ask, dec!(500))], ts)
    };

    for i in 0..n_tokens {
        let token = format!("M{i:02}_UP");
        let cap_loser = i % 4 == 0; // 12 losers, 36 winners
        if cap_loser {
            // Dip at mid 0.47 → entry clamped to 0.45 == cap (baseline takes it;
            // tighter variant skips), then collapse (both SL levels trip).
            se.on_tick(&token, &book(dec!(0.45), dec!(0.49), now), now);
            now += 10_000;
            se.on_tick(&token, &book(dec!(0.20), dec!(0.22), now), now);
            now += 10_000;
        } else {
            // Dip at mid 0.44 → entry 0.42 for EVERY variant, then rally high and
            // retrace so the shared trailing stop closes it as a WIN.
            se.on_tick(&token, &book(dec!(0.42), dec!(0.46), now), now);
            now += 10_000;
            se.on_tick(&token, &book(dec!(0.80), dec!(0.82), now), now);
            now += 10_000;
            se.on_tick(&token, &book(dec!(0.58), dec!(0.60), now), now);
            now += 10_000;
        }
    }

    // Capture variant metrics BEFORE evaluate(): applying an evolution rebuilds
    // the variant set, which resets the accumulated samples.
    let evals_now = now + 1_000;
    let pre_views = se.variant_views(evals_now);
    let outcomes = se.evaluate(evals_now);
    let applied = se.evolution_count(STRATEGY);
    let rejected = se.rejected_count(STRATEGY);

    let mut out = ManagerExp {
        applied,
        rejected,
        ..Default::default()
    };
    for v in &pre_views {
        if v.is_baseline {
            out.baseline_wr = v.win_rate;
            out.baseline_pf = v.profit_factor;
            out.baseline_n = v.sample_count;
            out.base_pnl = v.total_pnl_usd;
        } else if v.win_rate > out.best_variant_wr {
            out.best_variant_wr = v.win_rate;
            out.best_variant_pf = v.profit_factor;
            out.best_variant_n = v.sample_count;
            out.best_variant_pnl = v.total_pnl_usd;
        }
    }
    let cur = se.current_params();
    out.final_cap = cur
        .get(STRATEGY, "trend_max_entry_price")
        .unwrap_or_default();
    out.final_min_price = cur.get(STRATEGY, "trend_min_price").unwrap_or_default();
    for rec in se.history(Some(STRATEGY), 50) {
        out.trajectory.push(format!(
            "applied={} reason={:?} {}",
            rec.applied,
            rec.reason,
            changed_knobs(&rec.from_params, &rec.to_params, STRATEGY).join(", ")
        ));
    }
    let _ = outcomes;
    out
}

#[derive(Debug, Clone)]
struct TradeRow {
    group: char,
    idx: usize,
    asset: String,
    entry: Decimal,
    exit: Decimal,
    net: Decimal,
    reason: String,
    hold_sec: i64,
}

#[derive(Default, Debug)]
struct Metrics {
    n: usize,
    wins: usize,
    gross_profit: Decimal,
    gross_loss: Decimal,
    net: Decimal,
    max_dd: Decimal,
}

impl Metrics {
    fn win_rate(&self) -> Decimal {
        if self.n == 0 {
            Decimal::ZERO
        } else {
            Decimal::from(self.wins) / Decimal::from(self.n)
        }
    }
    fn profit_factor(&self) -> Decimal {
        if self.gross_loss <= Decimal::ZERO {
            if self.gross_profit > Decimal::ZERO {
                Decimal::from(100)
            } else {
                Decimal::ZERO
            }
        } else {
            self.gross_profit / self.gross_loss
        }
    }
}

fn metrics(rows: &[TradeRow]) -> Metrics {
    let mut m = Metrics::default();
    let mut equity = Decimal::ZERO;
    let mut peak = Decimal::ZERO;
    for r in rows {
        m.n += 1;
        m.net += r.net;
        if r.net > Decimal::ZERO {
            m.wins += 1;
            m.gross_profit += r.net;
        } else {
            m.gross_loss += -r.net;
        }
        equity += r.net;
        if equity > peak {
            peak = equity;
        }
        let dd = peak - equity;
        if dd > m.max_dd {
            m.max_dd = dd;
        }
    }
    m
}

/// Run one group through the shared scripted stream. Returns the realised trade
/// rows and the final `Core` (so the caller can read evolution counters/audit).
fn run_group(group: char, se_enabled: bool) -> (Vec<TradeRow>, Core) {
    let mut core = build_core(se_enabled);
    let mut rows = Vec::new();
    let mut recorded = 0usize;
    let mut now: i64 = 1_700_000_000_000; // fixed epoch → deterministic
    // The scanner derives the current slot from the SAME simulated clock, so the
    // market's round_slot/expiry must be built from it (otherwise the timing gate
    // sees a stale expiry and blocks every entry).
    let slot = now / 1000 / ROUND_SEC;

    core.engine_on_data(
        DataEvent::RoundMarkets {
            markets: markets(slot),
            now_ms: now,
        },
        now,
    );

    // 1) Warm-up: hold every token at mid 0.61 so all trends confirm. The trend
    //    window is 60s and confirmation needs >=90% of it spanned, so 20 ticks at
    //    4s (80s) clears the bar with margin.
    for _ in 0..20 {
        for i in 0..N_ASSETS {
            tick_token(&mut core, &up_token(i), dec!(0.60), dec!(0.62), now);
        }
        now += STEP_MS;
    }

    // 2) One trade per token, sequential in time. Every 7th token takes a
    //    "marginal" entry at the 0.45 cap that stops out; the rest take a "deep"
    //    entry under the cap that wins on a trailing exit. Keeping the losing
    //    minority small (~15%) matters: a cap-tightening variant must still see
    //    >= `min_sample_count` (30) trades to be considered, so the winners have
    //    to dominate. Winner exits are maker SELLs, so the LAST tick of each
    //    winner block crosses the resting SELL to actually fill it.
    for i in 0..N_ASSETS {
        let token = up_token(i);
        let marginal = i % 7 == 0;
        if marginal {
            // Dip to the cap, fill, then collapse. Two ticks are needed to FILL:
            // the engine's entry is a resting maker BUY at the cap, which only
            // executes when the ask crosses it. Crucial detail — the fill tick
            // (bid 0.44 / ask 0.45, mid 0.445) still computes an entry of 0.44 for
            // a variant whose factor is 0.98, so a mere 2% cap tightening would
            // re-enter here. The directed cap variant tightens 3% (cap 0.4365 <
            // 0.44), so it skips BOTH ticks and never takes this loser. The trend
            // stays above `broken_price` (0.35) through the fill tick, so the
            // resting bid is NOT cancelled before it fills; only the collapse
            // (mid 0.21) breaks the trend, tripping the live 12% stop as a taker
            // sell that fills immediately.
            tick_token(&mut core, &token, dec!(0.45), dec!(0.49), now);
            now += STEP_MS;
            tick_token(&mut core, &token, dec!(0.44), dec!(0.45), now); // ask crosses → fill @0.45
            now += STEP_MS;
            tick_token(&mut core, &token, dec!(0.20), dec!(0.22), now); // collapse → SL taker sell fills
            now += STEP_MS;
            tick_token(&mut core, &token, dec!(0.20), dec!(0.21), now); // settle (mid < broken → no re-entry)
            now += STEP_MS;
        } else {
            // Dip mid 0.44 → resting bid 0.42 (under cap) → fills.
            tick_token(&mut core, &token, dec!(0.42), dec!(0.46), now);
            now += STEP_MS;
            tick_token(&mut core, &token, dec!(0.41), dec!(0.42), now); // cross → fill
            now += STEP_MS;
            tick_token(&mut core, &token, dec!(0.75), dec!(0.77), now); // high → HWM
            now += STEP_MS;
            tick_token(&mut core, &token, dec!(0.60), dec!(0.62), now); // retrace → trailing exit
            now += STEP_MS;
            tick_token(&mut core, &token, dec!(0.66), dec!(0.68), now); // cross the exit SELL
            now += STEP_MS;
            tick_token(&mut core, &token, dec!(0.60), dec!(0.62), now); // settle
            now += STEP_MS;
        }
        let closed = core.positions().closed_positions();
        while recorded < closed.len() {
            let p = &closed[recorded];
            rows.push(TradeRow {
                group,
                idx: rows.len(),
                asset: p.asset.clone(),
                entry: p.entry_price,
                exit: p.exit_price,
                net: p.net_pnl_usd,
                reason: format!("{:?}", p.exit_reason),
                hold_sec: p.hold_time_sec,
            });
            recorded += 1;
        }
        // Probe: how many virtual trades have the variants accumulated so far?
        if std::env::var("AB_PROBE").is_ok() && i % 10 == 0 {
            let vv = core.shadow_evolution_variants(now);
            let s: Vec<String> = vv
                .iter()
                .map(|v| format!("{}:{}", v.id, v.sample_count))
                .collect();
            eprintln!("[{group}] after token {i} (t={}) samples {s:?}", now / 1000);
        }
    }

    // Final evaluator view — computed at the run's OWN clock so the 30-minute
    // window is not accidentally exceeded (which would prune every sample).
    if std::env::var("AB_PROBE").is_ok() {
        let cur = core.shadow_evolution().current_params();
        let g = |k: &str| {
            cur.get(STRATEGY, k)
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        };
        eprintln!(
            "[{group}] PARAMS cap={} factor={} min={} broken={}",
            g("trend_max_entry_price"),
            g("trend_entry_factor"),
            g("trend_min_price"),
            g("trend_broken_price"),
        );
        let vv = core.shadow_evolution_variants(now + 1_000);
        for v in &vv {
            eprintln!(
                "[{group}] FINAL {} baseline={} samples={} wr={} pf={} pnl={} age={}",
                v.id,
                v.is_baseline,
                v.sample_count,
                v.win_rate,
                v.profit_factor,
                v.total_pnl_usd,
                v.age_sec
            );
        }
    }

    (rows, core)
}

fn debug_dump(tag: char, core: &mut Core, rows: &[TradeRow]) {
    if std::env::var("AB_DEBUG").is_err() {
        return;
    }
    let now = 1_700_000_000_000 + 10_000_000;
    let status = core.shadow_evolution_status(now);
    let variants = core.shadow_evolution_variants(now);
    eprintln!(
        "[{tag}] trades={} status={status:?} variants={} applied={} rejected={}",
        rows.len(),
        variants.len(),
        core.shadow_evolution().evolution_count(STRATEGY),
        core.shadow_evolution().rejected_count(STRATEGY)
    );
    for v in &variants {
        eprintln!(
            "[{tag}]   variant {} strategy={} baseline={} samples={} wr={} pf={} pnl={} age={}",
            v.id,
            v.strategy,
            v.is_baseline,
            v.sample_count,
            v.win_rate,
            v.profit_factor,
            v.total_pnl_usd,
            v.age_sec
        );
    }
}

fn main() {
    // ── EXP-C: manager-level qualification run. Drives the REAL ShadowEvolution
    //    component (the production one) directly, with no engine observation lag,
    //    through a scenario where a cap-tightened variant genuinely wins. This
    //    isolates "does the evolution mechanism fire when a variant truly
    //    outperforms?" from "does a variant get generated that outperforms in a
    //    single static regime?" (the full-engine A/B answers the latter).
    let exp_c = run_manager_experiment();

    // ── Model self-check: confirm a cap-tightened variant really skips the
    //    marginal entry while the baseline takes it. Validates the premise above.
    //    This drives a real spread_arb TWIN (the strategy's own code path).
    if std::env::var("AB_PROBE").is_ok() {
        model_self_check();
    }

    let out_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("docs/reports/data"));
    let _ = fs::create_dir_all(&out_dir);

    let (rows_a, mut core_a) = run_group('A', false);
    let (rows_b, mut core_b) = run_group('B', true);
    debug_dump('A', &mut core_a, &rows_a);
    debug_dump('B', &mut core_b, &rows_b);

    let ma = metrics(&rows_a);
    let mb = metrics(&rows_b);

    // ── Safety-lock probe (group B's manager): a +20% manual apply must be
    //    REJECTED by Lock 2 (gradient ±5%). This is the demonstrable proof that
    //    the lock is live regardless of whether natural proposals trip it.
    let before_cap = core_b
        .shadow_evolution()
        .current_params()
        .get(STRATEGY, "trend_max_entry_price")
        .unwrap_or(dec!(0.45));
    let mut far_p = StrategyParams::new();
    far_p.set("trend_max_entry_price", before_cap * dec!(1.20));
    let mut far = MutableParams::new();
    far.set_strategy(STRATEGY, far_p);
    let lock_probe = match core_b.shadow_evolution_apply(far, 1_700_000_999_999) {
        Ok(()) => "ACCEPTED (UNEXPECTED — lock failed)".to_string(),
        Err(e) => format!("REJECTED by Lock 2: {e}"),
    };

    // ── Raw trades CSV.
    let mut csv = String::from("group,idx,asset,entry,exit,net,reason,hold_sec\n");
    for r in rows_a.iter().chain(rows_b.iter()) {
        let _ = writeln!(
            csv,
            "{},{},{},{},{},{},{},{}",
            r.group, r.idx, r.asset, r.entry, r.exit, r.net, r.reason, r.hold_sec
        );
    }
    let _ = fs::write(out_dir.join("shadow_ab_trades.csv"), &csv);

    // ── Group B audit (applied + rejected) → JSONL on disk. Read from the
    //    strategy's OWN file, so the artefact is exactly that strategy's history.
    let history = core_b.shadow_evolution_history(Some(STRATEGY), 500);
    let mut audit_jsonl = String::new();
    for rec in &history {
        if let Ok(line) = serde_json::to_string(rec) {
            audit_jsonl.push_str(&line);
            audit_jsonl.push('\n');
        }
    }
    let _ = fs::write(out_dir.join("shadow_ab_evolution_B.jsonl"), &audit_jsonl);

    // ── Summary (markdown + machine-readable numbers).
    let mut s = String::new();
    let _ = writeln!(s, "# Shadow Evolution A/B — raw summary");
    let _ = writeln!(
        s,
        "groups: A=evolution OFF, B=evolution ON (identical tick stream, params, balance)"
    );
    let _ = writeln!(
        s,
        "assets_per_round: {N_ASSETS}, round_sec: {ROUND_SEC}, tick_ms: {STEP_MS}, scripted_rounds: 1"
    );
    let _ = writeln!(s);
    let _ = writeln!(s, "## Realised (dry) metrics");
    let _ = writeln!(s, "group,trades,wins,win_rate,pf_ratio,net_pnl,max_dd");
    let _ = writeln!(
        s,
        "A,{},{},{:.4},{:.4},{:.4},{:.4}",
        ma.n,
        ma.wins,
        ma.win_rate(),
        ma.profit_factor(),
        ma.net,
        ma.max_dd
    );
    let _ = writeln!(
        s,
        "B,{},{},{:.4},{:.4},{:.4},{:.4}",
        mb.n,
        mb.wins,
        mb.win_rate(),
        mb.profit_factor(),
        mb.net,
        mb.max_dd
    );
    let _ = writeln!(s);
    let _ = writeln!(s, "## Group B evolution ({STRATEGY})");
    let _ = writeln!(
        s,
        "audited_strategies: {:?}",
        core_b.shadow_evolution().audited_strategies()
    );
    let _ = writeln!(
        s,
        "evolutions_applied: {}",
        core_b.shadow_evolution().evolution_count(STRATEGY)
    );
    let _ = writeln!(
        s,
        "evolutions_rejected_natural: {}",
        core_b.shadow_evolution().rejected_count(STRATEGY)
    );
    let _ = writeln!(s, "audit_records: {}", history.len());
    let _ = writeln!(s, "safety_lock_probe: {lock_probe}");
    let final_cap = core_b
        .shadow_evolution()
        .current_params()
        .get(STRATEGY, "trend_max_entry_price")
        .unwrap_or(before_cap);
    let _ = writeln!(
        s,
        "final_params_vs_initial_max_entry_cap: {before_cap} -> {final_cap}"
    );
    let _ = writeln!(s);
    let _ = writeln!(s, "## Group B parameter trajectory (changed knobs only)");
    let _ = writeln!(s, "ts,knob,from,to,reason,applied,rejection");
    for rec in &history {
        for change in changed_knobs(&rec.from_params, &rec.to_params, STRATEGY) {
            let (knob, rest) = change.split_once(':').unwrap_or((change.as_str(), ""));
            let (from, to) = rest.split_once("->").unwrap_or((rest, ""));
            let _ = writeln!(
                s,
                "{},{},{},{},{:?},{},{}",
                rec.timestamp,
                knob,
                from,
                to,
                rec.reason,
                rec.applied,
                rec.rejection.clone().unwrap_or_default()
            );
        }
    }
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "## EXP-C: manager-level qualification (real ShadowEvolution, no engine lag)"
    );
    let _ = writeln!(
        s,
        "baseline: n={} wr={:.4} pf={:.4} pnl={:.4}",
        exp_c.baseline_n, exp_c.baseline_wr, exp_c.baseline_pf, exp_c.base_pnl
    );
    let _ = writeln!(
        s,
        "best_variant: n={} wr={:.4} pf={:.4} pnl={:.4}",
        exp_c.best_variant_n, exp_c.best_variant_wr, exp_c.best_variant_pf, exp_c.best_variant_pnl
    );
    let _ = writeln!(s, "evolutions_applied: {}", exp_c.applied);
    let _ = writeln!(s, "evolutions_rejected: {}", exp_c.rejected);
    let _ = writeln!(
        s,
        "final_cap: {}  final_min_price: {}",
        exp_c.final_cap, exp_c.final_min_price
    );
    let _ = writeln!(s, "trajectory:");
    for t in &exp_c.trajectory {
        let _ = writeln!(s, "  {t}");
    }
    let _ = fs::write(out_dir.join("shadow_ab_summary.md"), &s);

    println!("{s}");
    println!("raw files written to: {}", out_dir.display());
    println!("  - shadow_ab_trades.csv  - shadow_ab_summary.md  - shadow_ab_evolution_B.jsonl");
    let _ = core_a;
}

/// Drive a real spread_arb twin at two cap values through the same dip and
/// report whether it would open a position. Proves the counterfactual the
/// evolution search depends on is produced by the strategy's OWN logic.
fn model_self_check() {
    use blitzkrieg_core::exit_policy::ExitConfig;
    use blitzkrieg_core::strategies::EngineStrategy;
    use blitzkrieg_core::strategies::shadow_twin::{TwinReplay, tick_ctx};

    let m = CryptoMarket {
        asset: "T".into(),
        condition_id: "c".into(),
        question_id: "q".into(),
        up_token_id: "T".into(),
        down_token_id: "T_DOWN".into(),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: 900_000,
        round_slot: 1,
        neg_risk: false,
        question: "?".into(),
    };
    let loaded = blitzkrieg_core::strategy_engine::loader::load_foreign(&spread_arb_lib())
        .unwrap_or_else(|e| panic!("load spread_arb cdylib failed: {e:?}"));
    let strat = loaded.strategy;
    let factory = strat
        .shadow_factory()
        .expect("spread_arb declares itself evolvable");
    for (label, cap) in [("baseline", dec!(0.45)), ("tight-3pct", dec!(0.4365))] {
        let mut p = StrategyParams::from_knobs(&factory.knobs());
        p.set("trend_max_entry_price", cap);
        let Some(twin) = factory.make(&p) else {
            continue;
        };
        let mut replay = TwinReplay::new(twin, &ExitConfig::default());
        replay.on_round(std::slice::from_ref(&m), &[], 0);
        let mut now = 0i64;
        for _ in 0..70 {
            now += 1_000;
            let b = book(dec!(0.60), dec!(0.62));
            replay.on_tick(&tick_ctx(std::slice::from_ref(&m), "T", &b, 1, 880, now));
        }
        now += 1_000;
        let dip = book(dec!(0.45), dec!(0.49)); // mid 0.47 → entry clamps to 0.45
        replay.on_tick(&tick_ctx(std::slice::from_ref(&m), "T", &dip, 1, 870, now));
        eprintln!(
            "[probe] {label}: cap={cap} open={}",
            replay.open_positions()
        );
    }
}

fn book(bid: Decimal, ask: Decimal) -> blitzkrieg_core::model::OrderbookSnapshot {
    blitzkrieg_core::model::OrderbookSnapshot::from_levels(
        "T",
        vec![(bid, dec!(500))],
        vec![(ask, dec!(500))],
        0,
    )
}
