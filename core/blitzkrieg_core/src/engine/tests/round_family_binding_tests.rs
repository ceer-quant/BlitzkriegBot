#![cfg(test)]

use super::*;
use std::sync::{Arc, Mutex};
// #1170 (方案 A): family-bound strategy instances. Split out of
// round_rollover_tests (the #780 file-size ratchet: the binding fixture block
// pushed that file past 1500 lines) — the subject is the strategy-instance
// binding surface, not the rollover merge logic, so it reads as its own file.

use super::round_rollover_tests::family_market;

/// A probe that records (slot, time_left) for every `on_round` fire and the
/// (slot, time_left, market count) of every evaluation ctx — the two surfaces
/// a family binding must scope.
struct FamilyProbe {
    rounds: Arc<Mutex<Vec<(i64, i64)>>>,
    ctxs: Arc<Mutex<Vec<(i64, i64, usize)>>>,
}
impl EngineStrategy for FamilyProbe {
    fn name(&self) -> &str {
        "family_probe"
    }
    fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, slot: i64, left: i64, _now: i64) {
        self.rounds.lock().unwrap().push((slot, left));
    }
    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<crate::signal::TradeSignal> {
        self.ctxs.lock().unwrap().push((
            ctx.round_slot(),
            ctx.time_left_sec(),
            ctx.markets().len(),
        ));
        Vec::new()
    }
}

fn host_bound(e: &mut Engine, probe: FamilyProbe, families: &[(&str, i64)]) {
    e.register_user_strategy(Box::new(probe), "test".into())
        .unwrap();
    e.set_strategy_round_families(
        families
            .iter()
            .map(|(n, s)| (crate::engine::strategy_env_fold(n), *s))
            .collect(),
    );
    assert!(e.set_strategy_enabled("family_probe", true));
}

#[test]
fn a_family_bound_strategy_only_hears_its_familys_rollover() {
    // The #1170 dispatch bug: before the binding, EVERY family's rollover
    // fired `on_round` into EVERY hosted strategy with THAT family's (slot,
    // time_left) — a 1h transition zeroed/shifted a 15m package's
    // round-duration cache mid-round. A bound instance hears only its own.
    let rounds = Arc::new(Mutex::new(Vec::new()));
    let mut e = Engine::new(cfg());
    host_bound(
        &mut e,
        FamilyProbe {
            rounds: rounds.clone(),
            ctxs: Arc::new(Mutex::new(Vec::new())),
        },
        &[("family_probe", 900)],
    );
    // The 15m family feeds first (slot 2), then the 1h family joins.
    let end15 = 1_800_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end15, "polymarket", 900)],
        now_ms: 1_000,
    });
    let end1h = 2 * 3600 * 1000;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end1h, "polymarket", 3600)],
        now_ms: 1_000,
    });
    assert_eq!(
        e.scanner.markets().len(),
        2,
        "the multi-family table holds both rows"
    );
    // Exactly ONE fire, from the 15m family's first feed — the 1h family's
    // rollover (the second RoundMarkets) never reached the bound instance.
    assert_eq!(
        rounds.lock().unwrap().len(),
        1,
        "the bound instance hears ONLY its family's rollover: {:?}",
        rounds.lock().unwrap()
    );

    // The unbound control: the same feeds fire on EVERY transition.
    let rounds2 = Arc::new(Mutex::new(Vec::new()));
    let mut e2 = Engine::new(cfg());
    host_bound(
        &mut e2,
        FamilyProbe {
            rounds: rounds2.clone(),
            ctxs: Arc::new(Mutex::new(Vec::new())),
        },
        &[],
    );
    e2.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end15, "polymarket", 900)],
        now_ms: 1_000,
    });
    e2.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end1h, "polymarket", 3600)],
        now_ms: 1_000,
    });
    assert_eq!(
        rounds2.lock().unwrap().len(),
        2,
        "an unbound instance keeps the historic every-rollover dispatch"
    );
}

#[test]
fn a_family_bound_strategy_hears_the_binding_set_after_registration() {
    // The binding lands AFTER the scopes exist (install_engine pushes the
    // parsed table before packages load, but the setter also re-stamps live
    // scopes) — the re-stamp must reach every scope of the name.
    let rounds = Arc::new(Mutex::new(Vec::new()));
    let mut e = Engine::new(cfg());
    let probe = FamilyProbe {
        rounds: rounds.clone(),
        ctxs: Arc::new(Mutex::new(Vec::new())),
    };
    e.register_user_strategy(Box::new(probe), "test".into())
        .unwrap();
    assert!(e.set_strategy_enabled("family_probe", true));
    e.set_strategy_round_families([(crate::engine::strategy_env_fold("family_probe"), 900)].into());

    let end15 = 1_800_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end15, "polymarket", 900)],
        now_ms: 1_000,
    });
    let end1h = 2 * 3600 * 1000;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end1h, "polymarket", 3600)],
        now_ms: 1_000,
    });
    assert_eq!(
        rounds.lock().unwrap().len(),
        1,
        "a binding set after registration re-stamps the live scope: {:?}",
        rounds.lock().unwrap()
    );
}

#[test]
fn a_family_bound_strategy_sees_only_its_familys_markets_and_clock() {
    // The ctx surface: a bound instance's `bk.round()` must read ITS family's
    // clock (slot AND time_left) and its market list must hold ONLY its
    // family's rows — the first-row-family read the unbound table clock gives
    // is exactly the #1170 poisoning for a package priced off the wrong grid.
    // Discriminating setup: the 1h family feeds FIRST, so the table clock
    // (first row's family) reads the 1h numbers; a 900-bound instance must
    // still see 900-family numbers.
    let ctxs = Arc::new(Mutex::new(Vec::new()));
    let mut e = Engine::new(cfg());
    host_bound(
        &mut e,
        FamilyProbe {
            rounds: Arc::new(Mutex::new(Vec::new())),
            ctxs: ctxs.clone(),
        },
        &[("family_probe", 900)],
    );
    let end15 = 1_800_000i64;
    let end1h = 2 * 3600 * 1000;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end1h, "polymarket", 3600)],
        now_ms: 1_000,
    });
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end15, "polymarket", 900)],
        now_ms: 1_000,
    });
    assert_eq!(e.scanner.markets().len(), 2);
    e.evaluate(1_500_000);
    let seen = ctxs.lock().unwrap();
    let (slot, tleft, n) = seen[0];
    // The same numbers the ctx contract reads: the bound family's legacy fold.
    let own900 = e.scanner.round_timing_legacy_at(900, 1_500_000);
    let table = e.scanner.round_timing_legacy_at(3600, 1_500_000);
    assert_ne!(
        own900.slot, table.slot,
        "setup: the two families' grid clocks must disagree at this instant"
    );
    assert_eq!(
        slot, own900.slot,
        "the ctx slot reads the 900s family's clock"
    );
    assert_eq!(
        tleft, own900.time_left_sec,
        "the ctx time_left reads the 900s family's clock"
    );
    assert_eq!(
        n, 1,
        "the ctx market list holds ONLY the bound family's rows"
    );
}
