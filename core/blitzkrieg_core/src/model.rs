//! Core domain model: orders, fills, order states, structured error codes.
//!
//! The market-agnostic primitives (`Side`, `FillPolicy`, `OrderStatus`, …) now
//! live in `blitzkrieg-market-api` and are re-exported here, so the core and
//! every market plugin share ONE definition (P0.6). This module keeps only the
//! core-owned structs. Nothing here performs I/O.

use crate::decimal;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

// Shared market-agnostic primitives (single source of truth = market_api).
pub use blitzkrieg_market_api::{
    CoreError, CoreErrorCode, CoreResult, FillPolicy, FillStatus, OrderId, OrderStatus, OrderType,
    Side, TokenId, TradeId,
};
// E28 (§9.1): account identity is a market_api type for the same reason
// `Kline` is — the strategy API names the account a round targets and may not
// depend on the core. Re-exported so the kernel's import paths stay stable.
pub use blitzkrieg_market_api::{AccountId, default_account_id};
// Shared strategy-layer data shapes (single source of truth = strategy_logic,
// PR-B): the orderbook snapshot and signal direction the shared evaluators
// accept. Re-exported so the kernel's import paths stay stable.
pub use strategy_logic::model::{OrderbookSnapshot, SignalDirection};

/// Runtime mode. Core-owned (the market contract does not have a notion of dry
/// vs live; each plugin implements both).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Simulated: no exchange calls; fills synthesised from market books.
    Dry,
    /// Real orders via the CLOB.
    Live,
    /// Observation only: the venue's order egress is never constructed.
    ///
    /// This is a THIRD variant rather than a `readonly: bool` flag beside `mode`
    /// for one reason: a flag is opt-in at every site that reads it, and a site
    /// that forgets to read it fails open — it places the real order. A variant
    /// makes every `match` on `Mode` a compile error until it is revisited, so
    /// the guarantee cannot be lost by omission (E12-e / #94).
    ///
    /// Behaviourally read-only settles like [`Mode::Dry`] — because with no venue
    /// there is nothing to report a fill, so a live ledger would be a fiction —
    /// while the *intent* stays visible: `readonly()` answers false for `Dry`
    /// (a simulation, not a promise) and true here.
    ReadOnly,
}

impl Mode {
    /// Whether this mode is a promise never to send an order to a venue.
    ///
    /// Deliberately NOT `mode != Mode::Live`: `Dry` is a simulation, and a
    /// simulation that quietly becomes live is a different thing from an operator
    /// asking for a hard guarantee. Only `ReadOnly` promises.
    pub fn readonly(self) -> bool {
        matches!(self, Mode::ReadOnly)
    }

    /// Whether the venue order executor may be started at all.
    ///
    /// The single predicate every egress site must consult. `Live` is the only
    /// yes: `Dry` has no credentials story and `ReadOnly` exists to refuse.
    pub fn may_trade(self) -> bool {
        matches!(self, Mode::Live)
    }

    /// Whether orders settle as simulated fills rather than waiting on a venue.
    ///
    /// True for both non-trading modes: read-only cannot wait for a venue that
    /// was never started, so it settles the same way a simulation does.
    pub fn settles_locally(self) -> bool {
        !matches!(self, Mode::Live)
    }
}

/// The role an order actually played, RESOLVED FROM ITS FILLS rather than
/// assumed from the requested policy (E17).
///
/// The distinction decides money: a maker fill pays no fee, a taker fill does.
/// Deriving the fee basis from the *request* let a `MakerThenTaker` order be
/// charged as a taker while its trade record claimed maker, and the cash ledger
/// drifted away from `seed + Σ netPnl`.
///
/// State machine, one instance per order, advanced by [`OrderRole::after_fill`]:
///
/// ```text
///                  fill(Maker)                    fill(Taker)
///   Pending ───────────────────────► Maker ─────────────────────► MakerThenTaker
///      │                               ▲                                  │
///      │ fill(Taker)                   │ fill(Maker)                      │ fill(any)
///      ▼                               │                                  ▼
///    Taker ────────────────────────────┘                            MakerThenTaker
/// ```
///
/// `Pending` and `MakerThenTaker` describe an ORDER. A single FILL is always
/// either `Maker` or `Taker` — that is what [`OrderRole::is_maker`] decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderRole {
    /// Submitted, no fill yet: the role is not knowable in advance.
    #[default]
    Pending,
    /// Rested on the book and was hit — post-only, so it never crossed.
    Maker,
    /// Crossed the spread.
    Taker,
    /// Rested first and crossed later: this order's fills are mixed.
    MakerThenTaker,
}

impl OrderRole {
    /// Whether a fill with this role pays no taker fee. Meaningful for a single
    /// fill (`Maker`/`Taker`); `Pending` is treated as "not maker", i.e. the
    /// conservative side that charges a fee.
    pub fn is_maker(self) -> bool {
        matches!(self, Self::Maker)
    }

    /// Fold one fill's role into the order-level role.
    pub fn after_fill(self, fill: OrderRole) -> OrderRole {
        match (self, fill) {
            (Self::Pending, r) => r,
            (Self::Maker, Self::Taker) | (Self::Taker, Self::Maker) => Self::MakerThenTaker,
            (Self::MakerThenTaker, _) => Self::MakerThenTaker,
            (r, _) => r,
        }
    }

    /// The role a fill of an order with this filling behaviour must have had: a
    /// post-only order can only ever be hit as maker, and a `MakerThenTaker`
    /// order rests first — its taker leg is submitted as its own `Taker` order
    /// on escalation — so it fills as maker too. Only a `Taker` order crosses.
    pub fn from_fill_policy(mode: FillPolicy) -> Self {
        match mode {
            FillPolicy::Taker => Self::Taker,
            FillPolicy::Maker | FillPolicy::MakerThenTaker => Self::Maker,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderRequest {
    pub token_id: TokenId,
    pub condition_id: String,
    pub side: Side,
    pub mode: FillPolicy,
    #[serde(with = "decimal")]
    pub price: Decimal,
    #[serde(with = "decimal")]
    pub size: Decimal,
    /// Strategy/asset/direction/slot provenance, echoed back on events.
    pub internal_key: String,
    pub strategy: String,
    pub asset: String,
    #[serde(rename = "direction")]
    pub direction: String,
    pub round_slot: i64,
    /// E28 (§9.2): which account the order spends. `serde(default)` makes the
    /// wire default equal the old single-account behaviour — a 0.2 client (or
    /// a 0.2 line in a persisted log) reads as `default` without a migration.
    /// An account that is not configured is refused later (the ledger lookup
    /// is explicit; §9.3), so a default here can never silently spend a
    /// non-default book.
    #[serde(default = "default_account_id")]
    pub account_id: AccountId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fill {
    pub order_id: OrderId,
    pub trade_id: Option<TradeId>,
    pub token_id: TokenId,
    pub side: Side,
    #[serde(with = "decimal")]
    pub price: Decimal,
    /// Cumulative filled size reported by this event (the OME derives deltas).
    #[serde(with = "decimal")]
    pub size: Decimal,
    pub status: FillStatus,
    /// Exchange-reported timestamp (ms).
    pub ts_ms: i64,
    pub tx_hash: Option<String>,
    /// The venue's own maker/taker report for this execution (E17-b). When
    /// present it is the AUTHORITY for the fee and for the position's role; when
    /// absent (dry matcher, reconciliation synthesis) the OME falls back to the
    /// order's fill policy. Never inferred from the cumulative size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maker: Option<bool>,
    /// E28 (§9.2): the account the money moved in. Defaulted on read so a
    /// 0.2 line (or a dry/reconciliation synthesis that predates the field)
    /// lands in the `default` book without a migration; written explicitly
    /// on every new fill (read old, write new).
    #[serde(default = "default_account_id")]
    pub account_id: AccountId,
}

/// A core-side tracked order. Mirrors the Node TrackedOrder but is authoritative.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackedOrder {
    pub order_id: OrderId,
    pub internal_key: String,
    pub strategy: String,
    pub asset: String,
    pub direction: String,
    pub token_id: TokenId,
    pub condition_id: String,
    pub side: Side,
    pub mode: FillPolicy,
    /// Role this order has actually played, resolved from its fills (E17). Starts
    /// `Pending`; persisted so the audit trail survives a restart.
    #[serde(default)]
    pub role: OrderRole,
    #[serde(with = "decimal")]
    pub price: Decimal,
    #[serde(with = "decimal")]
    pub size: Decimal,
    #[serde(with = "decimal")]
    pub filled_size: Decimal,
    #[serde(with = "decimal::opt", default)]
    pub avg_fill_price: Option<Decimal>,
    pub status: OrderStatus,
    pub round_slot: i64,
    pub submitted_at_ms: i64,
    pub updated_at_ms: i64,
    /// #351 order lifecycle: when the strategy made the decision this order
    /// carries. Equals `submitted_at_ms` everywhere the kernel itself submits
    /// (the decision is the submission); a caller that stamps a later or
    /// earlier decision records the difference here. Defaulted on read so a
    /// 0.2/0.3.0 order log line lands as "decided at submission" without a
    /// migration.
    #[serde(default)]
    pub decision_at_ms: i64,
    /// #351: when the order arrives at the venue (`decision_at + taker
    /// latency`). A dry taker with latency > 0 fills against the book as it
    /// stands AT this instant, never the decision-time quote. `0`/absent =
    /// the historic immediate-fill behaviour.
    #[serde(default)]
    pub execute_at_ms: i64,
    /// #351: when the fill report reaches the local book
    /// (`execute_at + network rtt`). Ledger/position effects are stamped with
    /// this instant. `0`/absent = effects apply at execution.
    #[serde(default)]
    pub report_at_ms: i64,
    /// Exchange order id once the venue acknowledges a LIVE order (None in dry,
    /// or before the POST resolves). Reconciliation matches WS/REST on this.
    #[serde(default)]
    pub venue_order_id: Option<String>,
    /// Pending simulated maker→taker escalation deadline (ms), dry mode only.
    #[serde(default)]
    pub escalate_at_ms: Option<i64>,
    /// Maker→taker escalation window the order was submitted with. The LIVE
    /// path cannot arm the clock at submit (the venue acks asynchronously), so
    /// `confirm_live` starts it from this value once the venue accepts.
    #[serde(default)]
    pub maker_timeout_ms: i64,
    /// E28 (§9.2): the account the order belongs to. Defaulted on read so a
    /// 0.2 log line lands in the `default` book without a migration; written
    /// explicitly on every new order (read old, write new).
    #[serde(default = "default_account_id")]
    pub account_id: AccountId,
    /// The order's venue tag for `orders.list` — the venue binding of the
    /// account the order was placed from (the same fact `account.list`
    /// reports; #505 routes the execution by it). A DERIVED read: written
    /// empty on every new/restored order (so the persisted log lines and the
    /// backtest/replay wire stay byte-identical) and filled in when the view
    /// is read. Empty = the account is unbound (or unknown to the
    /// book) — the row renders untagged and the panel's venue filter keeps
    /// it visible. Never inferred from the token or market (#427's mapping
    /// is a different system; the order path does not reach into it).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub venue: String,
    /// B1a: the round-length family the order was sized/timed against, in
    /// seconds. `0` = pre-multi-family legacy archive (serde default keeps
    /// old order-log lines loading unchanged and byte-identical on rewrite);
    /// B1b stamps it at the consumption points. The authoritative source is
    /// the market's `round_duration_sec`; [`TrackedOrder::effective_round_duration_sec`]
    /// resolves the three-state fallback (own > market declaration > global).
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub round_duration_sec: i64,
}

impl TrackedOrder {
    /// B1a: the order's effective round length, resolved three-state (read
    /// only — no write here): the order's OWN stamp when it carries one
    /// (B1b's write path), else the market's declared duration when the
    /// market spoke (#354's `round_duration_sec`), else the configured
    /// global grid. `0` from every source = the caller has no clock fact at
    /// all; B1b's consumers decide what a 0 means at their own gates.
    pub fn effective_round_duration_sec(&self, market_decl: Option<i64>, global_sec: i64) -> i64 {
        if self.round_duration_sec > 0 {
            return self.round_duration_sec;
        }
        if let Some(d) = market_decl.filter(|d| *d > 0) {
            return d;
        }
        global_sec
    }

    /// B1a: idempotent lazy backfill — write `sec` only when the field is
    /// still the legacy `0`. An existing non-0 stamp is never overwritten
    /// (the first family a record was admitted under stays authoritative),
    /// so calling this on every tick / sweep pass is free and stable.
    pub fn backfill_round_duration(&mut self, sec: i64) {
        if self.round_duration_sec == 0 {
            self.round_duration_sec = sec;
        }
    }
}

/// B1a serde companion: `0` (= undeclared / legacy archive) never reaches the
/// wire, so a persisted log line written after this PR stays byte-identical
/// with the pre-B1a shape whenever the field still carries the default.
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

// ── Exit reasons & orderbook ─────────────────────────────────────────────────
// `SignalDirection` and `OrderbookSnapshot` are re-exported from
// `strategy_logic::model` at the top of this file (PR-B): one definition shared
// with the external-strategy logic crate.

/// Why a position was closed. Mirrors the TS `ExitReason` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    TakeProfit,
    StopLoss,
    RatchetFloor,
    TrailingStop,
    BreakevenLock,
    DepthCollapse,
    StaleProfit,
    StagnantProfit,
    TimeExit,
    ForceExit,
    Manual,
    /// Explicit close requested by a hosted (in-tree or external) strategy via
    /// an exit *intent*. The kernel still prices, sizes, risk-checks and
    /// submits it — the strategy never places anything itself.
    StrategySignal,
    /// The market resolved and the position was closed at its redemption value
    /// (issue #175). The position did not exit — it reached the end of its
    /// market, and the winning shares are redeemed on-chain.
    Settlement,
    /// A complete UP+DOWN share-pair on one condition was merged on-chain for
    /// its collateral ($1 per share-pair). The collection channel of
    /// pair-discount arbitrage (@almach: 12,570 merges, zero sells) — not a
    /// market exit: each leg leaves at $0.50/share so the pair's exit notional
    /// is exactly the $1.00 the ledger credits per pair. Unpaired remainders
    /// stay open for settlement.
    Merge,
    /// #684: a LIVE position held past its market's expiry whose settlement
    /// query was NEVER answered by the venue (the updown round list only
    /// carries future rounds, so an already-expired condition can never appear
    /// in an answer). Closed at a bookkeeping price — last known mid, else
    /// entry price (cost recovery) — with NO redemption claim: no verdict was
    /// received, so none is invented and no "winner" is redeemed. The venue's
    /// own cash truth folds in later through the reconcile/realign sweep; the
    /// wire label `unverifiable_expiry` on the trade record is the audit trail
    /// (`settlement` would read as a verified verdict, which never happened).
    UnverifiableExpiry,
}

/// A binary UP/DOWN market for one asset in the current round.
///
/// #648 C: the DEFINITION moved verbatim to `blitzkrieg_data_model` (the leaf
/// the offline `tools/onchain_pull` crate also compiles against); this
/// re-export keeps every `crate::model::CryptoMarket` spelling compiling.
pub use blitzkrieg_data_model::CryptoMarket;

#[cfg(test)]
mod mode_tests {
    use super::Mode;

    /// The whole point of the third variant: `readonly()` must answer for the
    /// promise, not for "is it dry". A `mode != Live` implementation would pass a
    /// read-only test and also claim a simulation had promised something it never
    /// did — so Dry is asserted false here, not just ReadOnly true.
    #[test]
    fn readonly_is_a_promise_dry_does_not_make() {
        assert!(Mode::ReadOnly.readonly());
        assert!(!Mode::Dry.readonly());
        assert!(!Mode::Live.readonly());
    }

    /// Egress admission. This predicate is what guards construction of the venue
    /// executor, so a regression that widened it would reopen real trading.
    #[test]
    fn only_live_may_reach_a_venue() {
        assert!(Mode::Live.may_trade());
        assert!(!Mode::Dry.may_trade());
        assert!(!Mode::ReadOnly.may_trade());
    }

    /// ReadOnly settles locally: with no venue constructed, nothing would ever
    /// report a fill, so a ledger that waited would simply never move.
    #[test]
    fn everything_but_live_settles_locally() {
        assert!(Mode::Dry.settles_locally());
        assert!(Mode::ReadOnly.settles_locally());
        assert!(!Mode::Live.settles_locally());
    }

    /// The IPC contract carries mode as a lowercase string; a UI gating on
    /// "readonly" needs the exact wire spelling.
    #[test]
    fn wire_spelling_is_stable() {
        let s = |m: Mode| serde_json::to_string(&m).unwrap();
        assert_eq!(s(Mode::Dry), "\"dry\"");
        assert_eq!(s(Mode::Live), "\"live\"");
        assert_eq!(s(Mode::ReadOnly), "\"readonly\"");
    }
}

#[cfg(test)]
mod round_duration_tests {
    use super::{SignalDirection, TrackedOrder};
    use blitzkrieg_market_api::{FillPolicy, OrderStatus, Side, default_account_id};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    /// A legacy archive order: every field the pre-B1a shape carried, and
    /// nothing else. Constructed (not deserialized) so the struct literal
    /// itself proves the field is at the tail and defaulted.
    fn legacy_order() -> TrackedOrder {
        TrackedOrder {
            order_id: "o1".into(),
            internal_key: "k1".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            token_id: "tok".into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode: FillPolicy::Maker,
            price: dec!(0.43),
            size: dec!(10),
            filled_size: Decimal::ZERO,
            avg_fill_price: None,
            status: OrderStatus::Live,
            round_slot: 1,
            submitted_at_ms: 1,
            updated_at_ms: 1,
            decision_at_ms: 1,
            execute_at_ms: 0,
            report_at_ms: 0,
            venue_order_id: None,
            escalate_at_ms: None,
            maker_timeout_ms: 0,
            role: super::OrderRole::Pending,
            account_id: default_account_id(),
            venue: String::new(),
            round_duration_sec: 0,
        }
    }

    fn direction() -> SignalDirection {
        SignalDirection::Up
    }

    #[test]
    fn effective_duration_resolves_the_three_states() {
        // State 1: the record's own stamp wins over everything.
        let mut o = legacy_order();
        o.round_duration_sec = 900;
        assert_eq!(o.effective_round_duration_sec(Some(3600), 1800), 900);

        // State 2: own 0 + market declaration > 0 → the declaration.
        let o = legacy_order();
        assert_eq!(o.effective_round_duration_sec(Some(3600), 1800), 3600);

        // State 2b: a present-but-nonpositive declaration is not a fact.
        assert_eq!(o.effective_round_duration_sec(Some(0), 1800), 1800);
        assert_eq!(o.effective_round_duration_sec(Some(-5), 1800), 1800);

        // State 3: no own stamp, no declaration → the global grid (also
        // when the market never spoke at all).
        assert_eq!(o.effective_round_duration_sec(None, 1800), 1800);

        // No clock fact anywhere → 0; B1b's consumers gate on that.
        assert_eq!(o.effective_round_duration_sec(None, 0), 0);
    }

    #[test]
    fn backfill_is_idempotent_and_never_overwrites() {
        let mut o = legacy_order();
        // First backfill writes.
        o.backfill_round_duration(3600);
        assert_eq!(o.round_duration_sec, 3600);
        // Second backfill is a no-op — two calls, one write.
        o.backfill_round_duration(900);
        assert_eq!(
            o.round_duration_sec, 3600,
            "a non-0 stamp is never overwritten"
        );
        // 0 is not a fact: backfilling 0 onto 0 stays 0.
        let mut o2 = legacy_order();
        o2.backfill_round_duration(0);
        assert_eq!(o2.round_duration_sec, 0);
    }

    #[test]
    fn serde_default_and_skip_keep_old_lines_byte_identical() {
        // The serde contract: a pre-B1a JSON line (no field) deserializes to
        // 0; a 0 value serializes back WITHOUT the field, so re-persisted
        // legacy log lines stay byte-identical; a stamped value round-trips
        // under the camelCase wire name.
        let o = legacy_order();
        let line = serde_json::to_string(&o).unwrap();
        assert!(
            !line.contains("roundDurationSec"),
            "0 never reaches the wire: {line}"
        );
        let back: TrackedOrder = serde_json::from_str(&line).unwrap();
        assert_eq!(
            back.round_duration_sec, 0,
            "absent field defaults to the legacy 0"
        );

        let mut o = legacy_order();
        o.backfill_round_duration(900);
        let line = serde_json::to_string(&o).unwrap();
        assert!(
            line.contains("\"roundDurationSec\":900"),
            "stamped value rides the camelCase wire"
        );
        let back: TrackedOrder = serde_json::from_str(&line).unwrap();
        assert_eq!(back.round_duration_sec, 900);
    }

    #[test]
    fn position_carries_the_same_tri_state_and_backfill() {
        // The position mirrors the order contract (B1b's expiry sweep and
        // force-close consume it): the same three states, the same
        // idempotence — `direction()` referenced so the helper is load-bearing.
        let _ = direction();
        let mut p = crate::position::OpenPosition {
            id: "hft-1".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: direction(),
            token_id: "tok".into(),
            condition_id: "cond".into(),
            entry_price: dec!(0.40),
            current_price: dec!(0.40),
            prev_price: dec!(0.40),
            shares: dec!(10),
            cost_usd: dec!(4),
            was_maker_entry: true,
            entry_fee_pct: Decimal::ZERO,
            entry_role: super::OrderRole::Maker,
            exit_role: super::OrderRole::Pending,
            target_exit_price: None,
            entered_at_ms: 0,
            expires_at_ms: 0,
            account_id: default_account_id(),
            venue: String::new(),
            partner_token_id: String::new(),
            last_book_ts: 0,
            round_duration_sec: 0,
            state: crate::exit_policy::ExitState::new(dec!(0.40), 0),
            flows: crate::position::CashFlows::default(),
        };
        assert_eq!(p.effective_round_duration_sec(Some(900), 1800), 900);
        assert_eq!(p.effective_round_duration_sec(None, 1800), 1800);
        p.round_duration_sec = 3600;
        assert_eq!(p.effective_round_duration_sec(Some(900), 1800), 3600);
        // Wire contract, mirrored: the bare (0) snapshot skips the field
        // (serialize BEFORE the backfill — a stamped value must ride).
        p.round_duration_sec = 0;
        let bare = serde_json::to_string(&p).unwrap();
        assert!(!bare.contains("roundDurationSec"));
        // Backfill idempotence, mirrored.
        p.backfill_round_duration(900);
        p.backfill_round_duration(3600);
        assert_eq!(p.round_duration_sec, 900);
        // A stamped value rides the camelCase wire.
        assert!(
            serde_json::to_string(&p)
                .unwrap()
                .contains("\"roundDurationSec\":900")
        );
    }
}
