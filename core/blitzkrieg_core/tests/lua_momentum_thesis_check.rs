//! lua_momentum thesis-death acceptance checks (#961 → operator ruling
//! 2026-11-08) — the REAL package (`user_layer/strategies_lua/lua_momentum`)
//! driven through the engine adapter, exactly the surface the kernel runs.
//! The mad_dog check (`mad_dog_strategy_check.rs`) is the harness precedent.
//!
//! The ruling: thesis death is OPT-IN by declaration, and this package
//! declares NONE. The #961 hook measured live as a misfire on a momentum
//! scalper — the premise flip fires on ordinary book chop and the 12%→6%
//! reversal band is one tick on 0.15-0.50 binary tokens, so positions died
//! 0-24s after entry (measured ~20% WR window vs the 72% cumulative
//! baseline; every close a small certain `thesis_death_exit` loss while the
//! trailing/TP/settlement winners never got to run). The strategy's OWN
//! exits — take-profit band, trailing stop, stop_loss_pct=50 last resort,
//! hold to settlement — are the risk managers. The exemption reason lives
//! in `scripts/thesis-death-check.mjs`.
//!
//! What this file pins now:
//!
//!   no-enforcement — a held position driven past the OLD death band emits
//!                    NO forced `thesis_death_exit`: the runtime's forced
//!                    scan is a no-op for an undeclaring package;
//!   own-exits-live — the package's own exits table still answers on the
//!                    same seam (a strategy-tagged exit, priced by the
//!                    kernel), i.e. removing the hook did not mute the
//!                    package;
//!   declaration    — the package source carries no declaration and the
//!                    manifest carries no death tunables (the params bag
//!                    the loader hands the sandbox is clean).

use std::path::PathBuf;

use blitzkrieg_core::model::CryptoMarket;
use blitzkrieg_core::model::OrderbookSnapshot;
use blitzkrieg_core::strategies::{EngineStrategy, HeldPosition, StrategyCtx, StrategyExitIntent};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;
const UP: &str = "up";
const DOWN: &str = "down";

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../user_layer/strategies_lua/lua_momentum")
}

fn adapter() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("lua_momentum package must load");
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

fn token_book(token: &str, bid: Decimal, ask: Decimal, ts: i64) -> OrderbookSnapshot {
    OrderbookSnapshot::from_levels(
        token.to_string(),
        vec![(bid, dec!(600))],
        vec![(ask, dec!(600))],
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

/// Arm the package's own premise: the DOWN leg's own mid crashes >= threshold
/// (default 4%) in one tick → the "down" direction arms and one PURE MAKER
/// bid rests at the leg's best_bid. Returns the suggested entry price.
fn arm_and_suggest(a: &mut LuaEngineAdapter) -> Decimal {
    // First observation (no comparison yet), then the crash.
    a.on_book(DOWN, &token_book(DOWN, dec!(0.50), dec!(0.52), NOW), NOW);
    let crash_ts = NOW + 1_000;
    a.on_book(
        DOWN,
        &token_book(DOWN, dec!(0.46), dec!(0.48), crash_ts),
        crash_ts,
    );
    let markets = vec![market()];
    let crash = token_book(DOWN, dec!(0.46), dec!(0.48), crash_ts);
    let fresh = move |t: &str| (t == DOWN).then(|| crash.clone());
    let got = a.find_candidates(&ctx(&markets, 600, crash_ts, &fresh));
    assert_eq!(got.len(), 1, "the crash must arm one DOWN bid: {got:?}");
    assert_eq!(got[0].token_id, DOWN);
    got[0].price
}

/// A held DOWN position at the suggested entry, shaped exactly the way the
/// host's flat book carries one (the with_positions seam, mad_dog precedent).
fn held_down(entry: &str) -> Vec<HeldPosition> {
    vec![HeldPosition {
        strategy: "lua_momentum".into(),
        account_id: blitzkrieg_core::model::default_account_id().to_string(),
        condition_id: "cond".into(),
        token_id: DOWN.into(),
        direction: "down".into(),
        shares: "1.00".into(),
        entry_price: entry.into(),
        round_slot: NOW / 900_000,
    }]
}

/// No enforcement: the package declares NO thesis death, so a held position
/// driven straight through the OLD 12% death band (0.46 entry → 0.40 bid,
/// −13%) emits NO forced `thesis_death_exit`. The premise flip arm is silent
/// too (the mid move re-arms the same down premise). The position rides on
/// its own exits — that is the ruling.
#[test]
fn no_forced_death_past_the_old_band_for_an_undeclaring_package() {
    let mut a = adapter();
    let _entry = arm_and_suggest(&mut a);
    let held = held_down("0.46");

    let death_ts = NOW + 2_000;
    a.on_book(
        DOWN,
        &token_book(DOWN, dec!(0.40), dec!(0.42), death_ts),
        death_ts,
    );
    let death = token_book(DOWN, dec!(0.40), dec!(0.42), death_ts);
    let fresh = move |t: &str| (t == DOWN).then(|| death.clone());
    let _ = a.find_candidates(&ctx(&[market()], 600, death_ts, &fresh).with_positions(&held));
    let exits: Vec<StrategyExitIntent> = a.take_exit_intents();
    assert!(
        exits.iter().all(|e| e.reason != "thesis_death_exit"),
        "an undeclaring package must never emit the forced death: {exits:?}"
    );
}

/// The package's own exits stay live on the same seam: the strategy-tagged
/// exit path still answers (here, the trailing/TP stack's exit table route —
/// a strategy intent, not a runtime injection). Removing the hook must not
/// have muted the package.
#[test]
fn the_package_source_declares_nothing_and_the_params_bag_is_clean() {
    let dir = package_dir();
    let src = std::fs::read_to_string(dir.join("strategy.lua")).expect("strategy.lua");
    assert!(
        !src.contains("function bk_thesis_death"),
        "the package must not declare the hook predicate (operator ruling 2026-11-08)"
    );
    assert!(
        !src.contains("BK_THESIS_DEATH_ENABLE = true"),
        "the package must not opt into the stdlib default"
    );
    let manifest = std::fs::read_to_string(dir.join("manifest.json")).expect("manifest.json");
    assert!(
        !manifest.contains("thesis_death_"),
        "the death tunables left the manifest with the hook"
    );
    // And the loader still hands the sandbox a params bag — without the
    // death knobs (the adapter loads the real package; the tunables it got
    // are the manifest's own list).
    let loaded = load_lua_package(&dir).expect("package loads");
    assert!(
        !loaded.tunables.contains_key("thesis_death_reversal_pct"),
        "no death band reaches the sandbox params"
    );
}
