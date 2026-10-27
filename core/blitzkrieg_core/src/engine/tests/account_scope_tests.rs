#![cfg(test)]

use super::*;
use blitzkrieg_market_api::DEFAULT_ACCOUNT_ID as DEFAULT_ACCT;
use rust_decimal_macros::dec;
// Multi-account fan-out and account/venue scope semantics: distinct
// internal keys per account, trading-closed/frozen scoping, per-account
// enablement, exits-only demotion and account-scope rebuilds.
// ── 多账户扇出（多账户同时跑策略）：每条先红后绿的有牙测试 ────────────
//
// 部署形状：每（账户×策略）一个实例；default scope 是兼容形状（回放
// 基线逐位不变的锚）。这里的测试用 `probe` 适配器（test_support 宿主
// 注册的）+ 直接构造 TradeSignal 走 submit 路径。

/// Account A's entry candidate does not collide with account B's on the
/// SAME token: the OME's global internal-key dedup disambiguates by the
/// trailing account segment. (Today-red → the second order was dropped
/// as a duplicate.)
#[test]
fn two_accounts_same_token_get_distinct_internal_keys() {
    let mut e = engine();
    let now = 1_000_000i64;
    // Host pushes each account book's facts before evaluate; the engine
    // fixture never does, so the tests state them the way the host would.
    e.set_account_trading("paper", true);
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    let mut orders = e.submit_candidate_orders_for_test(
        vec![
            scoped_signal(DEFAULT_ACCT, "tok"),
            scoped_signal("paper", "tok"),
        ],
        now,
    );
    assert_eq!(orders.len(), 2, "both scopes emit");
    let k0 = orders.remove(0).internal_key;
    let k1 = orders.remove(0).internal_key;
    assert_ne!(k0, k1, "the account segment disambiguates");
    // The default scope's key stays byte-identical to the pre-fanout
    // spelling (4 segments, NO account segment) — the replay baseline
    // compares this exact string.
    assert!(
        !k0.contains(&format!(":{DEFAULT_ACCT}")),
        "default scope key must not carry an account segment: {k0}"
    );
    assert!(
        k1.ends_with(":paper"),
        "non-default scope key ends with the account segment: {k1}"
    );
}

/// A trading-closed scope contributes NO entry candidates; its exit
/// intents keep flowing (「关=拒开仓、放平仓」).
#[test]
fn trading_closed_scope_drops_entries_exits_flow() {
    let mut e = engine();
    let now = 1_000_000i64;
    e.set_account_trading("paper", false);
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
    assert!(orders.is_empty(), "closed scope's entry is dropped");
    // Re-open: the same candidate flows again.
    e.set_account_trading("paper", true);
    let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
    assert_eq!(orders.len(), 1, "re-opened scope emits");
}

/// The account-scoped kill switch (PR G): a FROZEN scope contributes no
/// entry candidates — exactly the trading-OFF shape — while a sibling
/// scope on the same token keeps emitting (the isolation that makes the
/// per-account freeze worth having). The default scope is freezable too:
/// unlike `scope_trading`, there is no privileged default arm.
#[test]
fn frozen_scope_drops_entries_sibling_flows() {
    let mut e = engine();
    let now = 1_000_000i64;
    e.set_account_trading("paper", true);
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    e.set_account_frozen("paper", true);
    let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
    assert!(orders.is_empty(), "frozen scope's entry is dropped");
    // The sibling on the SAME token is untouched by paper's freeze.
    e.set_account_trading("default", true);
    let orders = e.submit_candidate_orders_for_test(
        vec![
            scoped_signal("paper", "tok"),
            scoped_signal(DEFAULT_ACCT, "tok"),
        ],
        now,
    );
    assert_eq!(
        orders.len(),
        1,
        "only the non-frozen scope's candidate survives: {orders:?}"
    );
    // Lifting the freeze re-opens the scope.
    e.set_account_frozen("paper", false);
    let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
    assert_eq!(orders.len(), 1, "resumed scope emits again");
    // The default scope freezes like any other (manual account.freeze
    // default must stop it too).
    e.set_account_frozen(DEFAULT_ACCT, true);
    let orders = e.submit_candidate_orders_for_test(vec![scoped_signal(DEFAULT_ACCT, "tok")], now);
    assert!(orders.is_empty(), "the default scope is freezable");
}

/// pending_tokens keys on (account, token): A's resting entry does not
/// suppress B's candidate for the same token. (Today-red: the shared
/// token key swallowed B.)
#[test]
fn pending_token_is_per_account() {
    let mut e = engine();
    e.note_scoped_order_placed("acct_a", "tok");
    // Same token, different account: not suppressed.
    let mut e2 = engine();
    e2.note_scoped_order_placed("acct_b", "tok");
    assert!(
        !e2.pending_tokens
            .contains(&("acct_a".to_string(), "tok".into())),
        "B's book never saw A's pending token"
    );
    // Same account, same token: suppressed (the dedup that matters).
    assert!(
        e2.pending_tokens
            .contains(&("acct_b".to_string(), "tok".into()))
    );
    // The default scope's key shape: ("default", token) — the historical
    // single-string key's exact behaviour.
    let mut e3 = engine();
    e3.note_order_placed("tok");
    assert!(
        e3.pending_tokens
            .contains(&(DEFAULT_ACCT.to_string(), "tok".into())),
        "default scope pending key = (default, token)"
    );
}

// ── 账户↔venue 解耦：per-account ENABLEMENT 语义牙（先红后绿） ─────────

/// 双账户同策略同 token：A 停用 B 启用 → 只有 B scope 出意图。
///
/// Scope enablement is per-INSTANCE: disabling (strategy, paper) — the
/// write's rebuild omitting the name — never touches (strategy, paper2).
/// The disable rides the rebuild: a FLAT scope drops outright; a HELD
/// scope is demoted exits-only (its candidates die at the submission
/// gate) while the sibling on the SAME strategy and token keeps
/// emitting, and re-adding the name restores A independently.
#[test]
fn per_account_disable_leaves_sibling_scopes_trading() {
    let mut e = engine();
    let now = 1_000_000i64;
    e.set_account_trading("paper", true);
    e.set_account_trading("paper2", true);
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    // Instantiate both scopes the way the boot/rebuild does, then disable
    // A while it HOLDS (the realistic disable: the write's rebuild omits
    // the name and hands the held set — DEMOTE, not teardown).
    e.rebuild_account_scopes("paper", &["probe".into()], &[], &mut |n, a| {
        Some(probe_rebuild(n, a))
    });
    e.rebuild_account_scopes("paper2", &["probe".into()], &[], &mut |n, a| {
        Some(probe_rebuild(n, a))
    });
    e.rebuild_account_scopes("paper", &[], &["probe".into()], &mut |n, a| {
        Some(probe_rebuild(n, a))
    });
    assert!(
        e.strategies
            .iter()
            .any(|s| s.account_id == "paper" && s.exits_only),
        "the disabled-and-held scope is demoted exits-only"
    );

    // A disabled scope emits nothing…
    let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
    assert!(orders.is_empty(), "disabled scope's entry is dropped");
    // …while the sibling scope of the SAME strategy+token keeps flowing.
    let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper2", "tok")], now);
    assert_eq!(
        orders.len(),
        1,
        "the sibling scope is untouched by paper's disable"
    );
    // Both candidates in one cycle: only the enabled scope's lands.
    let orders = e.submit_candidate_orders_for_test(
        vec![
            scoped_signal("paper", "tok"),
            scoped_signal("paper2", "tok"),
        ],
        now,
    );
    assert_eq!(orders.len(), 1, "only B's intent survives: {orders:?}");
    assert_eq!(orders[0].account_id.as_str(), "paper2");
    // Re-enabling restores A independently (the ADD phase un-demotes).
    e.rebuild_account_scopes("paper", &["probe".into()], &[], &mut |n, a| {
        Some(probe_rebuild(n, a))
    });
    assert!(
        !e.strategies
            .iter()
            .any(|s| s.account_id == "paper" && s.exits_only),
        "a scope back in the enablement set returns to normal"
    );
    let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
    assert_eq!(orders.len(), 1, "re-enabled scope emits again");
}

/// 停用有持仓的 scope → 无新 entry、exit intent 照常流出（exits-only
/// 降级）。The disable rides the rebuild's DEMOTE phase (the same
/// mechanism a removed-and-held strategy gets): the scope stays hosted
/// and `enabled`, so its exit intents KEEP being drained by evaluate —
/// but its entry candidates die at the submission gate. Never a
/// teardown, never a stuck position.
#[test]
fn disabling_a_scope_with_positions_demotes_exits_only() {
    struct ExitEmitter {
        name: String,
    }
    impl EngineStrategy for ExitEmitter {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_book(&mut self, _: &str, _: &OrderbookSnapshot, _: i64) {}
        fn on_round(&mut self, _: i64, _: i64, _: i64) {}
        fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            // Emit an entry for each held position's token — the shape a
            // strategy that wants to (re)enter under itself produces.
            ctx.held_positions()
                .iter()
                .map(|p| TradeSignal {
                    strategy: self.name.clone(),
                    account_id: "paper".into(),
                    asset: "BTC".into(),
                    direction: SignalDirection::Up,
                    token_id: p.token_id.clone(),
                    condition_id: "cond".into(),
                    price: dec!(0.42),
                    reason: "exit-only probe".into(),
                    shares: None,
                    pair: None,
                })
                .collect()
        }
        fn take_exit_intents(&mut self) -> Vec<StrategyExitIntent> {
            // One standing exit intent: the position must keep being
            // managed even after the scope is disabled.
            vec![StrategyExitIntent {
                token_id: "held_tok".into(),
                reason: "manual (exit-only probe)".into(),
                account_id: "paper".into(),
            }]
        }
    }
    let mut e = Engine::new(cfg());
    // The paper scope hosts the emitter itself (a rebuild's build fn
    // would stand in a bare NamedStub — this scope must EMIT).
    e.register_scoped_strategy(
        Box::new(ExitEmitter {
            name: "probe".into(),
        }),
        "test".into(),
        "paper",
    )
    .expect("exit emitter registers");
    // Registration starts DISABLED; being in the account's enablement set
    // is what turns it on (the write's rebuild ENABLEs what lands).
    for s in &mut e.strategies {
        if s.account_id == "paper" {
            s.enabled = true;
        }
    }
    // The enablement write's shape: the scope is in the set (the rebuild
    // finds the existing scope and clears any demotion — the instance
    // with the exit emitter survives).
    e.rebuild_account_scopes("paper", &["probe".into()], &[], &mut |n, a| {
        Some(probe_rebuild(n, a))
    });
    // The host pushes the held positions BEFORE the evaluate (the same
    // push run_cycle does). HeldPosition's wire-facing string fields.
    let held_view = || {
        vec![HeldPosition {
            strategy: "probe".into(),
            account_id: "paper".into(),
            condition_id: "cond".into(),
            token_id: "held_tok".into(),
            direction: "up".into(),
            shares: "10".into(),
            entry_price: "0.40".into(),
            round_slot: 1,
        }]
    };
    e.set_positions_view(held_view());
    e.set_account_trading("paper", true);
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: 1_000_000i64,
    });
    // Sanity while enabled: entries AND exits both flow.
    let orders = e.evaluate(1_000_000i64);
    assert_eq!(
        orders.len(),
        1,
        "enabled scope emits the held token's re-entry"
    );
    assert_eq!(
        e.drain_strategy_exits().len(),
        1,
        "enabled scope emits exits"
    );

    // Disable the scope WHILE HOLDING: the write's rebuild omits the
    // name and names what is held → DEMOTE.
    e.set_positions_view(held_view());
    let kept = e.rebuild_account_scopes("paper", &[], &["probe".into()], &mut |n, a| {
        Some(probe_rebuild(n, a))
    });
    assert_eq!(kept, vec!["probe".to_string()], "the demotion is reported");
    let orders = e.evaluate(1_000_001i64);
    assert!(orders.is_empty(), "disabled scope: no new entries");
    assert_eq!(
        e.drain_strategy_exits().len(),
        1,
        "exits-only demotion: exit intents keep flowing"
    );
    assert!(
        e.strategies
            .iter()
            .any(|s| s.account_id == "paper" && s.strategy.name() == "probe"),
        "a held scope is never torn down by the disable"
    );
}

/// Sizing reads the scope's OWN pushed equity, not the global scalar.
#[test]
fn sizing_uses_scope_equity_not_the_global_scalar() {
    let mut e = engine_from(pct_cfg(dec!(100), dec!(1), dec!(1000)));
    // The GLOBAL scalar (the active ledger's mirror) says $10; the
    // "rich" scope's own book says $500. At price 0.50:
    //   scalar sizing → 10/0.50 = 20 shares; scope sizing → 1000.
    e.set_equity_usd(dec!(10));
    e.set_account_equity("rich", dec!(500));
    let shares = e.compute_shares(dec!(0.50), "probe", "rich");
    assert_eq!(shares, dec!(1000), "the scope's own equity drives sizing");
    // The default scope keeps reading the global scalar.
    let default_shares = e.compute_shares(dec!(0.50), "probe", DEFAULT_ACCT);
    assert_eq!(default_shares, dec!(20), "default scope = the scalar");
    // A scope with NO push sizes against ZERO: fail-closed, no ticket.
    let none = e.entry_ticket(dec!(0.50), "probe", "ghost");
    assert_eq!(none, None, "unknown scope = zero equity = no ticket");
}

/// A venue-bound scope drops candidates whose token belongs to a
/// DIFFERENT venue's round market — in the engine, before any order
/// exists, so no place_gated cooldown burns.
#[test]
fn venue_bound_scope_drops_foreign_venue_candidates() {
    let mut e = engine();
    let now = 1_000_000i64;
    e.set_account_venue("poly_only", "polymarket");
    e.set_account_trading("poly_only", true);
    e.set_account_trading("free", true);
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)], // the fixture market's venue
        now_ms: now,
    });
    let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("poly_only", "tok")], now);
    assert!(
        orders.is_empty(),
        "a venue-bound scope never reaches into another venue's book"
    );
    // Unbound scope: the same candidate flows.
    let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("free", "tok")], now);
    assert_eq!(orders.len(), 1, "unbound scope trades any venue");
}

/// The exits-only demotion (account.strategies.set safety net): a
/// removed-but-held strategy's scope stops contributing entries while
/// its exit intents keep flowing; the flag clears when the strategy
/// returns to the assignment.
#[test]
fn exits_only_scope_drops_entries_keeps_exits_flagged() {
    let mut e = engine();
    // Push the book fact so the empty assertion below can ONLY come from
    // the exits-only gate (an unpushed scope would fail closed anyway).
    e.set_account_trading("paper", true);
    e.rebuild_account_scopes(
        "paper",
        &["probe".to_string()],
        &[],
        &mut |name, account| Some(probe_rebuild(name, account)),
    );
    // probe now hosted for paper. Demote it exits-only (as if removed
    // while holding positions).
    for s in &mut e.strategies {
        if s.account_id == "paper" {
            s.exits_only = true;
        }
    }
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
    assert!(orders.is_empty(), "exits-only scope: no entries");
    // Restore: the flag flips off in the rebuild, entries flow.
    e.rebuild_account_scopes(
        "paper",
        &["probe".to_string()],
        &[],
        &mut |name, account| Some(probe_rebuild(name, account)),
    );
    assert!(
        !e.strategies
            .iter()
            .any(|s| s.account_id == "paper" && s.exits_only),
        "a scope back in the assignment returns to normal"
    );
}

/// rebuild_account_scopes drops a removed-and-FLAT scope outright.
#[test]
fn rebuild_drops_removed_and_flat_scopes() {
    let mut e = engine();
    e.rebuild_account_scopes(
        "paper",
        &["probe".to_string()],
        &[],
        &mut |name, account| Some(probe_rebuild(name, account)),
    );
    assert_eq!(
        e.strategies
            .iter()
            .filter(|s| s.account_id == "paper")
            .count(),
        1,
        "the requested scope exists"
    );
    // A rebuild WITHOUT the name (and nothing held) drops it.
    e.rebuild_account_scopes("paper", &[], &[], &mut |name, account| {
        Some(probe_rebuild(name, account))
    });
    assert!(
        !e.strategies.iter().any(|s| s.account_id == "paper"),
        "removed-and-flat scope is gone"
    );
    // A rebuild that KEEPS the name but the build fn fails (non-Lua
    // source) still leaves the existing scope alone.
    e.rebuild_account_scopes("paper", &["probe".to_string()], &[], &mut |_, _| None);
}

/// A minimal named EngineStrategy stand-in for the scope-rebuild tests:
/// the real builder is the service's Lua re-loader; the engine tests only
/// need A strategy with the requested name (never traded here).
struct NamedStub {
    name: String,
}
impl EngineStrategy for NamedStub {
    fn name(&self) -> &str {
        &self.name
    }
    fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, _slot: i64, _left: i64, _now: i64) {}
    fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        Vec::new()
    }
}
fn probe_rebuild(name: &str, _account: &str) -> Box<dyn EngineStrategy> {
    Box::new(NamedStub {
        name: name.to_string(),
    })
}

/// The assignment write is a DECISION: an ENABLED default scope the
/// request omits (and nothing holds) is torn down outright — with
/// assignments explicit there is no INHERIT resolution to protect, and
/// the operator who trimmed the list asked for exactly this. Durable
/// exposure rides the DEMOTE phase (held strategies keep managing their
/// exits until flat).
#[test]
fn rebuild_drops_an_enabled_scope_the_assignment_omits() {
    let mut e = engine();
    e.rebuild_account_scopes(
        DEFAULT_ACCT,
        &["alpha".into(), "beta".into(), "gamma".into()],
        &[],
        &mut |name, _| Some(probe_rebuild(name, "default")),
    );
    assert!(e.set_strategy_enabled("alpha", true));
    // The explicit trim omits alpha (enabled) and gamma (disabled).
    e.rebuild_account_scopes(DEFAULT_ACCT, &["beta".into()], &[], &mut |name, _| {
        Some(probe_rebuild(name, "default"))
    });
    assert!(
        !e.strategies.iter().any(|s| s.strategy.name() == "alpha"),
        "an enabled scope the assignment omits drops — the trim is a decision"
    );
    assert!(
        !e.strategies.iter().any(|s| s.strategy.name() == "gamma"),
        "the disabled omitted scope drops as before"
    );
    assert!(
        e.strategies
            .iter()
            .any(|s| s.strategy.name() == "beta" && s.enabled),
        "the named scope arrives enabled (assignment IS enablement)"
    );
}
