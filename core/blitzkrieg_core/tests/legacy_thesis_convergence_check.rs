//! #961 legacy convergence acceptance — the pre-#961 packages that declare
//! `BK_THESIS_DEATH_ENABLE = true` (form B of docs/thesis_death.md).
//!
//! #1003 (2026-11-08) moved two of the original five to a reasoned per-package
//! exemption (`scripts/thesis-death-check.mjs` EXEMPT map, docs/thesis_death.md
//! §6): the measured death arm was net WORSE than the no-death arm for
//! spread_arb (Δ −$73.10 official fee, misfire 40.45%) and not better for
//! single_leg_pair (misfire 50.84%) — the stdlib default's per-leg reversal
//! band contradicts those packages' theses (the spread / the odds band with
//! kernel-owned survival). They now declare NOTHING, so they are asserted the
//! other way: the runtime must NOT force any exit on them (the forced hook was
//! these packages' only exit emitter on this path, so an exit row here would
//! be a residual forced verdict, not a strategy row).
//!
//! The remaining form-B packages — flash_arb, hot_side_momentum, whale_follow
//! — keep the full #961 contract, driven as the REAL shipped directory through
//! the engine adapter (the lua_momentum thesis check is the harness
//! precedent). The declaration is form B, so the stdlib default predicate
//! (`bk.thesis_death_default`) judges every held position with the package's
//! own manifest tunables (all three ship the documented defaults: a 25% flat
//! reversal band, tightening off, a 30s quoting timeout):
//!
//!   alive — a sub-band drift (8.3% off entry) emits NO exit intent: the
//!           death never fires on inventory noise, and these packages'
//!           own evaluates never emit exits (the「止损不归你管」seal),
//!           so ANY exit row here is the runtime's forced verdict;
//!   death — the held token's own bid reversed past the band (33.3% off
//!           the 0.60 entry) makes the SAME evaluate emit ONE
//!           runtime-injected `thesis_death_exit` intent: full taker
//!           (use_maker false), unpriced (the kernel prices it off the
//!           live bid), for the held token, by name.
//!
//! The entries side of every package is untouched — these scenarios run
//! the packages' own evaluates as-is; none of them proposes an entry on a
//! bare single-book ctx, and none of them may: the convergence contract
//! is death-conditions only, entry signals stay frozen.

use std::path::PathBuf;

use blitzkrieg_core::model::{CryptoMarket, OrderbookSnapshot};
use blitzkrieg_core::strategies::{EngineStrategy, HeldPosition, StrategyCtx, StrategyExitIntent};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;
const TOK: &str = "up";

/// The packages the #961 legacy convergence moved to form B that still carry
/// the declaration (alphabetical, the order the CI gate reports them).
/// #1003 exempted `single_leg_pair` and `spread_arb` — see
/// [`exempted_packages_declare_nothing_and_force_no_exit`].
const FORM_B: &[&str] = &["flash_arb", "hot_side_momentum", "whale_follow"];

/// The packages #1003 closed per-package (the reasoned exemption, NOT a bare
/// grandfather): the measured death arm misfires on their thesis shapes.
const EXEMPTED_BY_1003: &[&str] = &["single_leg_pair", "spread_arb"];

fn package_dir(pkg: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua")
        .join(pkg)
}

fn adapter(pkg: &str) -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir(pkg))
        .unwrap_or_else(|e| panic!("{pkg} package must load: {e}"));
    LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
}

fn market() -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond".into(),
        question_id: "q".into(),
        up_token_id: TOK.into(),
        down_token_id: "down".into(),
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

fn held(pkg: &str) -> Vec<HeldPosition> {
    vec![HeldPosition {
        strategy: pkg.into(),
        account_id: blitzkrieg_core::model::default_account_id().to_string(),
        condition_id: "cond".into(),
        token_id: TOK.into(),
        direction: "up".into(),
        shares: "1.00".into(),
        entry_price: "0.60".into(),
        round_slot: NOW / 900_000,
    }]
}

fn ctx<'a>(
    markets: &'a [CryptoMarket],
    time_left_sec: i64,
    now_ms: i64,
    fresh: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
) -> StrategyCtx<'a> {
    StrategyCtx::new(markets, NOW / 900_000, time_left_sec, now_ms, fresh)
}

/// Evaluate one package with one held position under one fresh book.
fn evaluate_held(a: &mut LuaEngineAdapter, pkg: &str, bid: Decimal, ask: Decimal, ts: i64) {
    let book = token_book(TOK, bid, ask, ts);
    a.on_book(TOK, &book, ts);
    let fresh = move |t: &str| (t == TOK).then(|| book.clone());
    let _ =
        a.find_candidates(&ctx(&[market()], 600, ts + 1_000, &fresh).with_positions(&held(pkg)));
}

/// ALIVE: a sub-band drift is not a thesis death — every form-B package
/// holds through an 8.3% adverse drift with NO exit intent whatsoever.
#[test]
fn sub_band_drift_is_alive_in_every_form_b_package() {
    for pkg in FORM_B {
        let mut a = adapter(pkg);
        evaluate_held(&mut a, pkg, dec!(0.55), dec!(0.57), NOW);
        let exits: Vec<StrategyExitIntent> = a.take_exit_intents();
        assert!(
            exits.is_empty(),
            "{pkg}: an 8.3% drift is inside the 25% stdlib band — no death, no exit: {exits:?}"
        );
    }
}

/// DEATH: the held token's own bid reversed past the shipped 25% band
/// (0.40 bid = 33.3% off the 0.60 entry) — the SAME evaluate emits ONE
/// runtime-injected forced close, tagged thesis_death_exit, full taker,
/// unpriced, for the held token, by name. This is the Exit-First contract
/// these packages adopted: a deep reversal can no longer ride bare.
#[test]
fn reversal_past_the_stdlib_band_closes_same_tick_in_every_form_b_package() {
    for pkg in FORM_B {
        let mut a = adapter(pkg);
        // Sub-band first: the death must be the REVERSAL's verdict, not a
        // stale or a first-tick artifact.
        evaluate_held(&mut a, pkg, dec!(0.55), dec!(0.57), NOW);
        assert!(
            a.take_exit_intents().is_empty(),
            "{pkg}: pre-death drift must stay silent"
        );
        // The reversal: past the band.
        let death_ts = NOW + 2_000;
        evaluate_held(&mut a, pkg, dec!(0.40), dec!(0.42), death_ts);
        let exits: Vec<StrategyExitIntent> = a.take_exit_intents();
        assert_eq!(
            exits.len(),
            1,
            "{pkg}: the reversed thesis must be ONE forced verdict: {exits:?}"
        );
        assert_eq!(exits[0].token_id, TOK, "{pkg}: the held leg is what dies");
        assert_eq!(
            exits[0].reason, "thesis_death_exit",
            "{pkg}: the runtime's forced tag, not a strategy row"
        );
        assert!(
            !exits[0].use_maker,
            "{pkg}: a forced death is a full TAKER close"
        );
        assert_eq!(
            exits[0].exit_price.as_deref(),
            None,
            "{pkg}: the kernel prices it off the live bid"
        );
    }
}

/// The manifest tunables flowed: every form-B package narrows or loosens
/// the stdlib bands through its own manifest (these three ship the
/// documented defaults), so a 30% reversal with `thesis_death_stale_ms`
/// judged fresh (1s-old quote) dies by the REVERSAL condition, not the
/// stale one — and the 30s timeout with `0` off is provably not what
/// fired here (the quote is 1s old).
#[test]
fn the_death_is_the_reversal_condition_not_the_stale_timeout() {
    for pkg in FORM_B {
        let mut a = adapter(pkg);
        // A 1s-old quote, a 30% drawdown: reversal fires (>= 25%), stale
        // cannot (age 1000ms << 30000ms). One verdict, one cause.
        evaluate_held(&mut a, pkg, dec!(0.42), dec!(0.44), NOW);
        let exits: Vec<StrategyExitIntent> = a.take_exit_intents();
        assert_eq!(
            exits.len(),
            1,
            "{pkg}: a 30% reversal on a fresh quote must die by the band: {exits:?}"
        );
        assert_eq!(exits[0].reason, "thesis_death_exit", "{pkg}");
    }
}

/// #1003: the per-package closure is real — an exempted package declares
/// NEITHER form, so the runtime's forced scan is a no-op for it even on a
/// past-the-band reversal (the exact scenario the pre-#1003 declaration
/// closed on). The forced hook was these packages' only exit emitter on
/// this path, so ANY exit row would be a residual forced verdict — the
/// exemption must leave the package honestly exit-silent (its survival
/// surface is the kernel's: settlement / force-exit / trailing ladder).
#[test]
fn exempted_packages_declare_nothing_and_force_no_exit() {
    for pkg in EXEMPTED_BY_1003 {
        // The shipped directory must not carry either declaration form.
        let src = std::fs::read_to_string(package_dir(pkg).join("strategy.lua"))
            .unwrap_or_else(|e| panic!("{pkg} strategy.lua must read: {e}"));
        let declares_bool = src.contains("BK_THESIS_DEATH_ENABLE = true");
        let declares_fn = {
            // A declaration predicate is `function bk_thesis_death(` at a
            // statement position; comments about the revocation must not
            // count. Lua line comments start with `--`.
            src.lines().any(|l| {
                let t = l.trim_start();
                !t.starts_with("--") && t.contains("function bk_thesis_death(")
            })
        };
        assert!(
            !declares_bool && !declares_fn,
            "{pkg}: the #1003 exemption must remove the declaration (bool={declares_bool}, fn={declares_fn})"
        );
        // And the runtime must not force anything on a past-the-band book.
        let mut a = adapter(pkg);
        evaluate_held(&mut a, pkg, dec!(0.40), dec!(0.42), NOW + 2_000);
        let exits: Vec<StrategyExitIntent> = a.take_exit_intents();
        assert!(
            exits.is_empty(),
            "{pkg}: an exempted package gets NO forced death — even a 33.3% reversal emits nothing: {exits:?}"
        );
    }
}
