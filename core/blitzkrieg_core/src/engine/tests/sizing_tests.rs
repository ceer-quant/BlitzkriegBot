#![cfg(test)]

use super::*;
use rust_decimal_macros::dec;
// Position sizing: fixed-lot clamps (#?), equity-relative sizing
// (#202), E2-a per-strategy sizing and the #202 x declared-shares
// equity cap on depth-sized legs.

// ── #202 equity-relative sizing ─────────────────────────────────────────

/// A percentage engine. The absolute `size_usd` stays populated (2.5) even
/// though the relative mode must ignore it, so any leak of the old budget
/// into the ticket shows up as a wrong number here.

#[test]
fn equity_sizing_replaces_the_fixed_lot_and_follows_the_account() {
    let mut e = engine_from(pct_cfg(dec!(20), dec!(10), dec!(1000)));
    e.set_equity_usd(dec!(4.8));
    // 4.8 × 20% = 0.96 USD → 0.96/0.40 = 2.4 → 2 whole shares (floor: a
    // percentage budget is an upper bound). The absolute path sends 10
    // shares = 4.00 USD = 83% of this book.
    assert_eq!(
        e.compute_shares(dec!(0.40), "spread_arb", "default"),
        dec!(2)
    );
    assert_eq!(e.entry_budget_usd("spread_arb"), dec!(0.96));
    // The band's own floor (min_shares = 10) must NOT lift it back over the
    // budget — that floor is the #202 bug itself.
    assert_eq!(e.effective_sizing("spread_arb").min_shares, dec!(10));
    assert_eq!(
        e.compute_shares(dec!(0.40), "spread_arb", "default"),
        dec!(2)
    );
    // The same percentage on a 100× account is 100× the budget, and the lot
    // follows (the point of the knob: 4.8 → 480 must move the order size).
    e.set_equity_usd(dec!(480));
    assert_eq!(e.entry_budget_usd("spread_arb"), dec!(96));
    assert_eq!(
        e.compute_shares(dec!(0.40), "spread_arb", "default"),
        dec!(240)
    );
    // The old ceiling still wins when it is the tighter one: the shipped
    // [10, 10] band saturates even a 480 USD account at 10 shares (4.00 USD,
    // 0.8% of it), so arming `size_pct` means revisiting `max_shares`.
    let mut shipped = engine_from(pct_cfg(dec!(20), dec!(10), dec!(10)));
    shipped.set_equity_usd(dec!(480));
    assert_eq!(
        shipped.compute_shares(dec!(0.40), "spread_arb", "default"),
        dec!(10)
    );
}

#[test]
fn equity_sizing_needs_a_budget_it_can_actually_spend() {
    // No equity pushed yet: sizing against an unknown account has no safe
    // reading, and falling back to the absolute lot would silently restore
    // the fixed 10 shares this knob exists to remove.
    let mut unknown = engine_from(pct_cfg(dec!(20), dec!(10), dec!(1000)));
    assert_eq!(
        unknown.compute_shares(dec!(0.40), "spread_arb", "default"),
        Decimal::ZERO
    );
    unknown.set_equity_usd(dec!(4.8));
    assert_eq!(
        unknown.compute_shares(dec!(0.40), "spread_arb", "default"),
        dec!(2)
    );
    // A budget that cannot buy one whole share (1% of 4.8 = 0.048 at 0.40)
    // emits NO order rather than a 0-size one that becomes a per-tick
    // rejection; the caller counts the skip.
    let mut tiny = engine_from(pct_cfg(dec!(1), dec!(10), dec!(1000)));
    tiny.set_equity_usd(dec!(4.8));
    assert_eq!(
        tiny.compute_shares(dec!(0.40), "spread_arb", "default"),
        Decimal::ZERO
    );
    assert_eq!(tiny.entry_ticket(dec!(0.40), "spread_arb", "default"), None);
    // A non-positive price and a zero ceiling are dead ends too, never a
    // panic or an unbounded ticket.
    assert_eq!(
        tiny.equity_shares(Decimal::ZERO, &tiny.global_sizing(), "default"),
        Decimal::ZERO
    );
    let mut closed = engine_from(pct_cfg(dec!(20), dec!(10), Decimal::ZERO));
    closed.set_equity_usd(dec!(4.8));
    assert_eq!(
        closed.compute_shares(dec!(0.40), "spread_arb", "default"),
        Decimal::ZERO
    );
}

#[test]
fn per_strategy_equity_pct_is_clamped_by_the_global() {
    let over = |pct: Decimal, weight: Option<Decimal>| StrategySize {
        size_usd: None,
        min_shares: None,
        max_shares: None,
        size_weight: weight,
        size_pct: Some(pct),
    };
    // Global 20%: a 50% leg is clamped down to it, a 5% leg is kept.
    let mut c = pct_cfg(dec!(20), dec!(10), dec!(1000));
    c.strategy_sizes = [
        ("greedy".to_string(), over(dec!(50), None)),
        ("timid".to_string(), over(dec!(5), None)),
    ]
    .into();
    let mut e = engine_from(c);
    e.set_equity_usd(dec!(100));
    assert_eq!(e.effective_sizing("greedy").size_pct, dec!(20));
    assert!(e.effective_sizing("greedy").strategy_scoped);
    assert_eq!(e.entry_budget_usd("greedy"), dec!(20));
    assert_eq!(e.compute_shares(dec!(0.40), "greedy", "default"), dec!(50));
    assert_eq!(e.compute_shares(dec!(0.40), "timid", "default"), dec!(12)); // 5/0.4 = 12.5
    // A weight scales the leg's budget inside its own percentage.
    let mut c2 = pct_cfg(dec!(20), dec!(10), dec!(1000));
    c2.strategy_sizes = [("half".to_string(), over(dec!(20), Some(dec!(0.5))))].into();
    let mut e2 = engine_from(c2);
    e2.set_equity_usd(dec!(100));
    assert_eq!(e2.entry_budget_usd("half"), dec!(10));
    assert_eq!(e2.compute_shares(dec!(0.40), "half", "default"), dec!(25));
    // Global OFF is "no opinion", not a zero ceiling: a leg configured with
    // its own percentage is then the only one in force, and a leg without
    // one stays on the absolute path.
    let mut c3 = pct_cfg(Decimal::ZERO, dec!(10), dec!(1000));
    c3.strategy_sizes = [("only".to_string(), over(dec!(10), None))].into();
    let mut e3 = engine_from(c3);
    e3.set_equity_usd(dec!(100));
    assert_eq!(e3.entry_budget_usd("only"), dec!(10));
    assert_eq!(e3.compute_shares(dec!(0.40), "only", "default"), dec!(25));
    assert_eq!(e3.effective_sizing("unset").size_pct, Decimal::ZERO);
}

/// The shipped default must be untouched by this PR: with `size_pct = 0` the
/// engine ignores the equity entirely, so the service pushing a balance once
/// per cycle (as it now always does) cannot move one share of an
/// unconfigured deployment's order flow.
#[test]
fn unconfigured_sizing_ignores_the_equity_entirely() {
    let mut e = engine_from(cfg()); // size_pct = 0, size_usd 2.5, band [10, 10]
    let before = e.compute_shares(dec!(0.25), "spread_arb", "default");
    assert_eq!(before, dec!(10));
    e.set_equity_usd(dec!(4.8));
    assert_eq!(
        e.compute_shares(dec!(0.25), "spread_arb", "default"),
        before
    );
    assert_eq!(
        e.entry_ticket(dec!(0.25), "spread_arb", "default"),
        Some(before)
    );
    assert_eq!(e.entry_budget_usd("spread_arb"), dec!(2.5));
    // Even a zero absolute budget keeps the historical zero-size order (that
    // path's own off switch) instead of turning into a silent skip.
    let mut off = cfg();
    off.size_usd = Decimal::ZERO;
    let off = engine_from(off);
    assert_eq!(
        off.compute_shares(dec!(0.25), "spread_arb", "default"),
        Decimal::ZERO
    );
    assert_eq!(
        off.entry_ticket(dec!(0.25), "spread_arb", "default"),
        Some(Decimal::ZERO)
    );
}

// ── E2-a per-strategy sizing ────────────────────────────────────────────

fn sizing_cfg(overrides: &[(&str, StrategySize)]) -> EngineConfig {
    let mut c = cfg();
    c.size_usd = dec!(10);
    c.min_shares = dec!(2);
    c.max_shares = dec!(20);
    c.strategy_sizes = overrides
        .iter()
        .map(|(n, s)| ((*n).to_string(), s.clone()))
        .collect();
    c
}

#[test]
fn strategy_sizing_falls_back_to_the_globals_when_absent() {
    let e = engine_from(sizing_cfg(&[]));
    // No entry at all → the global band, and reported as not strategy-scoped.
    let s = e.effective_sizing("whatever");
    assert_eq!(s.size_usd, dec!(10));
    assert_eq!(s.min_shares, dec!(2));
    assert_eq!(s.max_shares, dec!(20));
    assert!(!s.strategy_scoped);
    assert_eq!(
        e.compute_shares(dec!(0.50), "whatever", "default"),
        dec!(20)
    ); // 10/0.5=20

    // A caps-only entry (no sizing fields) must behave exactly like absent.
    let e2 = engine_from(sizing_cfg(&[("capped", StrategySize::default())]));
    assert_eq!(e2.effective_sizing("capped").size_usd, dec!(10));
    assert!(!e2.effective_sizing("capped").strategy_scoped);
}

#[test]
fn strategy_sizing_overrides_take_effect_inside_the_global_band() {
    // Global band here is [2,20] on a 10u budget (see `sizing_cfg`).
    let e = engine_from(sizing_cfg(&[(
        "small",
        StrategySize {
            size_usd: Some(dec!(1)),
            min_shares: Some(dec!(1)),
            max_shares: Some(dec!(2)),
            size_weight: None,
            size_pct: None,
        },
    )]));
    let s = e.effective_sizing("small");
    // The 1-share floor is raised to the global floor (2): the global band
    // bounds the strategy from BELOW as well, so a lot can never fall under
    // the venue/risk minimum.
    assert_eq!(
        (s.size_usd, s.min_shares, s.max_shares),
        (dec!(1), dec!(2), dec!(2))
    );
    assert!(s.strategy_scoped);
    // 1 / 0.45 ≈ 2.2 → rounds to 2, inside [2,2].
    assert_eq!(e.compute_shares(dec!(0.45), "small", "default"), dec!(2));
    // 1 / 0.05 = 20 → clamped to the strategy's own 2-share cap.
    assert_eq!(e.compute_shares(dec!(0.05), "small", "default"), dec!(2));
    // 1 / 2.00 = 0.5 → rounds to 1, then raised to the global 2-share floor.
    assert_eq!(e.compute_shares(dec!(2.00), "small", "default"), dec!(2));
    // The un-configured sibling keeps using the globals in the same engine.
    assert_eq!(e.compute_shares(dec!(0.50), "other", "default"), dec!(20));
}

/// E16/#98 weighted allocation: a weight re-weights the leg's share of the
/// per-entry budget and can never widen the global cap; a non-positive
/// weight zeroes the leg out.
#[test]
fn strategy_size_weight_reweights_but_never_widens() {
    let e = engine_from(sizing_cfg(&[
        (
            "half",
            StrategySize {
                size_weight: Some(dec!(0.5)),
                ..Default::default()
            },
        ),
        (
            "greedy",
            StrategySize {
                size_weight: Some(dec!(4)),
                ..Default::default()
            },
        ),
        (
            "off",
            StrategySize {
                size_weight: Some(Decimal::ZERO),
                ..Default::default()
            },
        ),
    ]));
    // half: 10 × 0.5 = 5 → 5/0.5 = 10 shares (global band [2,20]).
    let h = e.effective_sizing("half");
    assert_eq!(h.size_usd, dec!(5));
    assert_eq!(h.size_weight, Some(dec!(0.5)));
    assert!(h.strategy_scoped);
    assert_eq!(e.compute_shares(dec!(0.50), "half", "default"), dec!(10));
    // greedy: 10 × 4 clamps back to the global 10 — the weight cannot
    // widen the budget.
    let g = e.effective_sizing("greedy");
    assert_eq!(g.size_usd, dec!(10));
    // off: weight 0 → no budget at all.
    assert_eq!(e.effective_sizing("off").size_usd, Decimal::ZERO);
    assert_eq!(e.compute_shares(dec!(0.50), "off", "default"), dec!(0));
}

#[test]
fn strategy_sizing_can_never_exceed_the_global_risk_ceiling() {
    // An override that asks for MORE than the global band is clamped: more
    // notional, a higher share cap and a lower floor are all ignored.
    let e = engine_from(sizing_cfg(&[(
        "greedy",
        StrategySize {
            size_usd: Some(dec!(100)),
            min_shares: Some(dec!(0)),
            max_shares: Some(dec!(999)),
            size_weight: None,
            size_pct: None,
        },
    )]));
    let s = e.effective_sizing("greedy");
    assert_eq!(
        s.size_usd,
        dec!(10),
        "notional must be capped at the global budget"
    );
    assert_eq!(
        s.max_shares,
        dec!(20),
        "share ceiling must clamp to the global max"
    );
    assert_eq!(
        s.min_shares,
        dec!(2),
        "floor must not drop below the global min"
    );
    // 100/0.05 would be 2000 shares; the global ceiling still binds.
    assert_eq!(e.compute_shares(dec!(0.05), "greedy", "default"), dec!(20));
}

#[test]
fn strategy_floor_above_the_global_ceiling_still_yields_the_ceiling() {
    // Degenerate config: floor 50 with a global ceiling of 20. The ceiling
    // wins (a floor is never allowed to defeat the global risk cap).
    let e = engine_from(sizing_cfg(&[(
        "weird",
        StrategySize {
            size_usd: None,
            min_shares: Some(dec!(50)),
            max_shares: None,
            size_weight: None,
            size_pct: None,
        },
    )]));
    let s = e.effective_sizing("weird");
    assert_eq!((s.min_shares, s.max_shares), (dec!(20), (dec!(20))));
    assert_eq!(e.compute_shares(dec!(0.10), "weird", "default"), dec!(20));
}

// ── #202 × declared shares: the equity cap reaches depth-sized legs ────

/// Two strategies that DECLARE shares (the single_leg_pair shape), sized by
/// `--size-pct` per strategy: the equity ticket caps the declaration, and
/// the global cap still wins when it is tighter.
#[test]
fn an_armed_size_pct_caps_declared_shares_to_the_equity_ticket() {
    // Equity 100: `pct20` gets 20% = $20; at the dip's mid 0.44 the
    // whole-share ticket is floor(20 / 0.44) = 45 shares; `pct1` gets $1
    // → floor(1 / 0.44) = 2 shares.
    let mut e = engine_from(sizing_cfg(&[
        (
            "pct20",
            StrategySize {
                size_pct: Some(dec!(20)),
                ..Default::default()
            },
        ),
        (
            "pct1",
            StrategySize {
                size_pct: Some(dec!(1)),
                ..Default::default()
            },
        ),
    ]));
    e.set_equity_usd(dec!(100));
    e.register_user_strategy(
        Box::new(SharesBuyer::on_assets(
            "pct20",
            dec!(0.45),
            dec!(200),
            &["BTC"],
        )),
        "test".into(),
    )
    .unwrap();
    e.register_user_strategy(
        Box::new(SharesBuyer::on_assets(
            "pct1",
            dec!(0.45),
            dec!(200),
            &["ETH"],
        )),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("pct20", true));
    assert!(e.set_strategy_enabled("pct1", true));
    assert!(
        e.set_strategy_enabled("spread_arb", false),
        "isolate the two declared dips"
    );

    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market_of("BTC", 1_800_000), market_of("ETH", 1_800_000)],
        now_ms: now,
    });
    for asset in ["BTC", "ETH"] {
        e.on_data(DataEvent::Book {
            token_id: format!("{}_up", asset.to_lowercase()),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 1_000,
        });
    }
    let orders = e.evaluate(now + 2_000);
    assert_eq!(orders.len(), 2, "{orders:?}");
    let big = orders
        .iter()
        .find(|o| o.strategy == "pct20")
        .expect("pct20 entry");
    let small = orders
        .iter()
        .find(|o| o.strategy == "pct1")
        .expect("pct1 entry");
    // Declared 200 is capped by the equity ticket (45), then by the
    // strategy's global-clamped share ceiling (20).
    assert_eq!(big.size, dec!(20), "{big:?}");
    assert_eq!(small.size, dec!(2), "{small:?}");
    assert_eq!(big.price, dec!(0.44));
    assert_eq!(small.price, dec!(0.44));
    assert_eq!(e.size_pct_skip_count(), 0);
}

/// With the mode ARMED, a declared count whose price class sits above the
/// equity budget (ticket 0: the budget cannot buy one whole share) buys
/// NOTHING and is counted — the same statement `entry_ticket` makes for
/// the notional path (#202). A PAIR leg must never fall back to notional
/// sizing: that would open the naked leg.
#[test]
fn a_declared_count_above_the_armed_budget_buys_nothing() {
    let mut c = sizing_cfg(&[(
        "tiny",
        StrategySize {
            size_pct: Some(dec!(0.1)),
            ..Default::default()
        },
    )]);
    // Global ceiling 20 must not resurrect the leg either.
    c.max_shares = dec!(20);
    let mut e = engine_from(c);
    e.set_equity_usd(dec!(1)); // 0.1% = $0.001 → floor(0.001/0.44) = 0
    e.register_user_strategy(
        Box::new(SharesBuyer::on_assets(
            "tiny",
            dec!(0.45),
            dec!(5),
            &["BTC"],
        )),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("tiny", true));
    assert!(e.set_strategy_enabled("spread_arb", false));

    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market_of("BTC", 1_800_000)],
        now_ms: now,
    });
    e.on_data(DataEvent::Book {
        token_id: "btc_up".into(),
        bids: vec![(dec!(0.43), dec!(100))],
        asks: vec![(dec!(0.45), dec!(100))],
        now_ms: now + 1_000,
    });
    let orders = e.evaluate(now + 2_000);
    assert!(
        orders.iter().all(|o| o.strategy != "tiny"),
        "a zero-ticket declaration must not emit: {orders:?}"
    );
    assert_eq!(e.size_pct_skip_count(), 1, "the skip is counted");
}

/// The cap is INERT while `size_pct` is off (the default): a declared
/// count on the absolute path is still only bounded by `max_shares`, so
/// every existing replay is untouched (bit-for-bit discipline).
#[test]
fn size_pct_off_leaves_declared_shares_clamped_only_by_max_shares() {
    let mut e = engine_from(sizing_cfg(&[]));
    e.set_equity_usd(dec!(100));
    e.register_user_strategy(
        Box::new(SharesBuyer::on_assets("BTC", dec!(0.45), dec!(7), &["BTC"])),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("BTC", true));
    assert!(e.set_strategy_enabled("spread_arb", false));

    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market_of("BTC", 1_800_000)],
        now_ms: now,
    });
    e.on_data(DataEvent::Book {
        token_id: "btc_up".into(),
        bids: vec![(dec!(0.43), dec!(100))],
        asks: vec![(dec!(0.45), dec!(100))],
        now_ms: now + 1_000,
    });
    let orders = e.evaluate(now + 2_000);
    assert_eq!(orders.len(), 1, "{orders:?}");
    assert_eq!(
        orders[0].size,
        dec!(7),
        "declared 7 ≤ global cap 20: honoured"
    );
}

/// Per-asset market helper (the shared `market()` only makes BTC).

/// A hosted strategy that DECLARES its share count on every signal (the
/// `single_leg_pair` / `pair_discount_arb` shape): it sizes from the
/// visible ask depth, which has nothing to do with the account. The
/// declared amount is configurable so a test can place it above or below
/// the equity ticket.
struct SharesBuyer {
    name: String,
    account: String,
    buy_below: Decimal,
    declared: Decimal,
    assets: Vec<String>,
}
impl SharesBuyer {
    fn on_assets(name: &str, buy_below: Decimal, declared: Decimal, assets: &[&str]) -> Self {
        Self {
            name: name.to_string(),
            account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            buy_below,
            declared,
            assets: assets.iter().map(|a| (*a).to_string()).collect(),
        }
    }
}
impl EngineStrategy for SharesBuyer {
    fn name(&self) -> &str {
        &self.name
    }
    fn on_book(&mut self, _token_id: &str, _snap: &crate::model::OrderbookSnapshot, _now_ms: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    fn gate_exemptions(&self) -> GateExemptions {
        GateExemptions::none()
    }
    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        let mut out = Vec::new();
        for market in ctx.markets() {
            if !self.assets.is_empty() && !self.assets.contains(&market.asset) {
                continue;
            }
            let Some(book) = ctx.fresh_book(&market.up_token_id) else {
                continue;
            };
            if book.mid_price <= self.buy_below {
                out.push(TradeSignal {
                    strategy: self.name.clone(),
                    account_id: self.account.clone(),
                    asset: market.asset.clone(),
                    direction: SignalDirection::Up,
                    token_id: market.up_token_id.clone(),
                    condition_id: market.condition_id.clone(),
                    price: book.mid_price,
                    reason: format!("{} depth dip", self.name),
                    shares: Some(self.declared),
                    pair: None,
                });
            }
        }
        out
    }
}

#[test]
fn two_strategies_size_independently_in_one_evaluation() {
    // One cycle, two strategies, one dip each on its OWN asset: every entry
    // carries the strategy's own lot instead of a single global size.
    let mut e = engine_from(sizing_cfg(&[
        (
            "small",
            StrategySize {
                size_usd: Some(dec!(1)),
                min_shares: Some(dec!(1)),
                max_shares: Some(dec!(1)),
                size_weight: None,
                size_pct: None,
            },
        ),
        (
            "big",
            StrategySize {
                size_usd: Some(dec!(10)),
                min_shares: Some(dec!(20)),
                max_shares: Some(dec!(20)),
                size_weight: None,
                size_pct: None,
            },
        ),
    ]));
    e.register_user_strategy(
        Box::new(DipBuyer::on_assets("small", dec!(0.45), &["BTC"])),
        "test".into(),
    )
    .unwrap();
    e.register_user_strategy(
        Box::new(DipBuyer::on_assets("big", dec!(0.45), &["ETH"])),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("small", true));
    assert!(e.set_strategy_enabled("big", true));
    assert!(
        e.set_strategy_enabled("spread_arb", false),
        "isolate the two sized dips"
    );

    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market_of("BTC", 1_800_000), market_of("ETH", 1_800_000)],
        now_ms: now,
    });
    for asset in ["BTC", "ETH"] {
        e.on_data(DataEvent::Book {
            token_id: format!("{}_up", asset.to_lowercase()),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 1_000,
        });
    }
    let orders = e.evaluate(now + 2_000);
    assert_eq!(orders.len(), 2, "{orders:?}");
    let small = orders
        .iter()
        .find(|o| o.strategy == "small")
        .expect("small entry");
    let big = orders
        .iter()
        .find(|o| o.strategy == "big")
        .expect("big entry");
    assert_eq!(small.asset, "BTC");
    assert_eq!(big.asset, "ETH");
    assert_eq!(
        small.size,
        dec!(1),
        "1u / 0.44 rounds to 2, capped by its own 1-share lot"
    );
    assert_eq!(big.size, dec!(20));
    assert_eq!(small.price, dec!(0.44), "same dip price for both");
    assert_eq!(big.price, dec!(0.44));
}
