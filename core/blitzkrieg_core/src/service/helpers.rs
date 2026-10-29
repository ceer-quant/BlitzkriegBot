//! Free helper functions, small state records and tuning constants that
//! belong to the service kernel's order/placement/extension machinery,
//! moved verbatim from `service.rs` (pure code motion, no logic edits).
//! Visible to the service module only; `service.rs` re-exports the ones the
//! child test files reach with `pub use helpers::*;`.

use super::Core;
use crate::ipc::schema::Event;
use rust_decimal::Decimal;
use std::collections::HashMap;
use tokio::sync::mpsc;

/// The narrowed surface handed to an extension when it is enabled. It carries
/// only an event sink clone and a snapshot of strategy names — deliberately NOT
/// the signer, venue client, UDS socket, order manager or any internal state.
pub(super) struct CoreExtensionContext {
    pub(super) tx: Option<mpsc::UnboundedSender<Event>>,
    pub(super) strategies: Vec<String>,
}

impl crate::extension::ExtensionContext for CoreExtensionContext {
    fn emit(&self, event: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }
    fn strategy_names(&self) -> Vec<String> {
        self.strategies.clone()
    }
    fn log(&self, message: &str) {
        tracing::info!(target: "extension", "{message}");
    }
}

/// Decimal → JSON number for the diagnostics payloads, matching the `crate::decimal`
/// wire convention (Node sees a plain number, not a string).
pub(super) fn dec_json(d: Decimal) -> serde_json::Value {
    use rust_decimal::prelude::ToPrimitive;
    serde_json::Value::from(d.to_f64().unwrap_or_default())
}

/// Feed/decision counters for P4 observability.
#[derive(Debug, Default, Clone)]
pub(super) struct CoreStats {
    pub(super) books: u64,
    pub(super) tops: u64,
    pub(super) spots: u64,
    pub(super) rounds: u64,
    /// #377 (治 #2): archive verdict events replayed into the core — the
    /// count of on-chain resolutions that came from the stream instead of
    /// the dry ladder.
    pub(super) resolutions: u64,
    /// #388: wallet-level cash rows credited from the stream (MAKER_REBATE /
    /// REWARD ledger rows), and the exact USD total each kind paid. The row
    /// count and the totals travel together so a report can reconcile the
    /// credit against the ledger it built.
    pub(super) cashflow_rebates: u64,
    pub(super) cashflow_rewards: u64,
    pub(super) cashflow_rebates_usd: Decimal,
    pub(super) cashflow_rewards_usd: Decimal,
    /// Modeled maker rebate credited on MAKER fills (`model_maker_rebate`;
    /// almach REPORT §5 失真点 5). Separate from `cashflow_rebates_usd` — that
    /// counts the corpus's REAL MAKER_REBATE rows, this counts the fill-level
    /// pool-share approximation — so a report can show both without either
    /// implying the other.
    pub(super) modeled_maker_rebate_usd: Decimal,
    /// #427: confirmed cross-venue mapping rows replayed from the stream —
    /// the corpus-completeness anchor for the cross-venue strategies (a
    /// corpus that lost its `unified` rows reports zero instead of replaying
    /// "successfully" blind).
    pub(super) unified_mappings: u64,
    pub(super) evaluations: u64,
    pub(super) signals: u64,
    pub(super) place_rejected: u64,
    /// Entries rejected by a configured per-strategy cap (P-1.1).
    pub(super) strategy_limit_rejected: u64,
    /// Orders the LIVE venue refused (place path), session-scoped.
    pub(super) venue_rejected: u64,
    /// #406: entry intents refused fail-closed for a non-positive price
    /// (a prediction market has no legal quote <= 0; the ticket's sizing,
    /// policy and physics paths all divide by the price). Session-scoped.
    pub(super) illegal_price_rejected: u64,
}

/// Backoff bookkeeping for one placement intent. Backoff is exponential from
/// [`REJECT_BACKOFF_BASE_MS`], capped at [`REJECT_BACKOFF_MAX_MS`]; after
/// [`MAX_PLACE_ATTEMPTS`] consecutive venue refusals the intent is no longer
/// retried at all, and if the refusals are consecutive engine-wide the trading
/// freeze trips (see `note_venue_rejection`).
#[derive(Debug, Default, Clone)]
pub(super) struct RejectCooldown {
    pub(super) attempts: u32,
    pub(super) next_ok_ms: i64,
    pub(super) last_error: String,
    /// The "abandoned after N rejections" alert for this intent was emitted.
    pub(super) alerted: bool,
}

/// Base spacing between retries of a venue-rejected intent.
pub(super) const REJECT_BACKOFF_BASE_MS: i64 = 2_000;
/// Retry spacing ceiling.
pub(super) const REJECT_BACKOFF_MAX_MS: i64 = 30_000;
/// Attempts of ONE intent after which it stops being retried until a
/// successful placement or a restart clears the cooldown. A closing intent is
/// exempt from the cap (its backoff still applies): the position it closes is
/// unhedged exposure, and walking away leaves naked residual on the venue —
/// the "position never closes" failure class.
pub(super) const MAX_PLACE_ATTEMPTS: u32 = 8;
/// Consecutive venue refusals that freeze trading (kill switch).
pub(super) const FREEZE_ON_REJECTS: u32 = 5;
/// Consecutive failed reconciliation sweeps that freeze trading (E31-b): a
/// blind sweep is the failure mode that lets ghosts and orphans accumulate.
pub(super) const SWEEP_FAILURE_FREEZE: u32 = 3;
/// 冻结自愈防抖：an AUTO-frozen account is only lifted by a reconcile sweep
/// after THIS long, so the flash outage that caused the freeze has to stay
/// quiet for ten minutes before the kernel trusts it again (真线 2026-10-28:
/// 断网恢复后 sweep 曾长期无处落地，冻结白挂数小时).
pub(super) const FREEZE_AUTO_LIFT_MIN_MS: i64 = 600_000;
/// A benign close-intent rejection (Insufficient position) backoff-retried
/// THIS many times in a row (≈15–20 min under the exponential curve) before
/// the kernel shouts once: the venue's book and ours have diverged on one
/// position, and only a reconcile sweep (or a human) can close the gap.
pub(super) const BENIGN_CLOSE_DIVERGENCE_ATTEMPTS: u32 = 40;

/// #462: how long a place-timeout order may sit pending with no venue evidence
/// (no id bound, no fold, nothing) before the reconciliation sweep retires it
/// and releases the reservation. Clears the bridge's 5 s sweep cadence plus a
/// slow POST's place timeout, so a merely-slow placement is never raced.
pub(super) const PLACE_TIMEOUT_VERDICT_MS: i64 = 30_000;
/// A passing accounting audit is written to the audit log this often, so the log
/// proves the check RAN and not merely that it never failed (issue #189).
pub(super) const AUDIT_HEARTBEAT_MS: i64 = 600_000;

/// The trade summary keeps realized PnL as `f64` (Node-compatible on disk); the
/// audit compares it against `Decimal` cash, so the conversion is explicit here
/// rather than an `as` cast in the comparison.
pub(super) fn f64_to_decimal(v: f64) -> Decimal {
    use std::str::FromStr;
    if !v.is_finite() {
        return Decimal::ZERO;
    }
    Decimal::from_str(&format!("{v:.9}")).unwrap_or(Decimal::ZERO)
}

/// A placement intent that CLOSES exposure (an automated exit, a strategy
/// close, a flatten or a residual backstop) rather than opening one.
///
/// The predicate itself lives in [`crate::risk`]: the risk gate has to grant a
/// closing intent the same exemption the retry policy does, and two copies of
/// "what counts as closing" is how the escape hatch gets locked again (P0 #174).
pub(super) fn is_close_intent(internal_key: &str) -> bool {
    crate::risk::is_close_intent(internal_key)
}

/// The internal key of an automated exit job for one open position.
///
/// 账户↔venue 解耦: the key carries the position's ACCOUNT segment so two
/// accounts' exit ladders on the SAME token never collide in the
/// place-cooldown/dedup map (a default leg's close backoff must never pause
/// paper's). An empty `suffix` (the residual backstop) keeps the trailing
/// colon off; a `default` position's key gains no account segment at all —
/// the historical single-book spelling is byte-identical, which the replay
/// baseline compares.
pub(super) fn exit_intent_key(
    prefix: &str,
    pos: &crate::position::OpenPosition,
    suffix: &str,
) -> String {
    let account = pos.account_id.as_str();
    if account == crate::account::default_account_id().as_str() {
        if suffix.is_empty() {
            format!("{prefix}:{}", pos.token_id)
        } else {
            format!("{prefix}:{}:{suffix}", pos.token_id)
        }
    } else if suffix.is_empty() {
        format!("{prefix}:{}:{account}", pos.token_id)
    } else {
        format!("{prefix}:{}:{suffix}:{account}", pos.token_id)
    }
}
pub(super) fn reject_backoff_ms(attempts: u32) -> i64 {
    // 2s, 4s, 8s, 16s, 30s, 30s… — quick escape for a transient refusal,
    // bounded patience for a persistent one.
    let ms = REJECT_BACKOFF_BASE_MS.saturating_mul(1 << (attempts.saturating_sub(1)).min(4));
    ms.min(REJECT_BACKOFF_MAX_MS)
}

/// 冻结自愈（2026-10-28 真线事故）：did the venue say the position we tried
/// to close is ALREADY GONE? The canonical text is polymarket CLOB's
/// "Insufficient position for limit sell order" — the honest refusal an exit
/// POST draws when the timed-out sell actually FILLED. Matched lowercase on
/// the stable stem, so wording drift around it is tolerated.
pub(super) fn is_position_already_gone(rejection_message: &str) -> bool {
    rejection_message
        .to_lowercase()
        .contains("insufficient position")
}

/// How long a resting `maker_then_taker` order waits before the taker leg
/// replaces it — or `None` when it must NEVER escalate.
///
/// Three-valued on purpose, and the third value is why this is a function:
/// `0` means "use the configured default" (the long-standing meaning, kept for
/// every caller that passes nothing), a positive value is the clock itself, and
/// a NEGATIVE value is the operator's off switch — `--entry-maker-timeout-ms -1`
/// makes the entry a pure maker that rests until it fills or its round ends.
///
/// Without the negative branch there was no way to run a maker-only entry: `0`
/// fell back to the default and any large positive value merely postponed the
/// escalation past the round, which is the worst of both (the clock still fires
/// on a stale, one-sided book — see the `marketable_walk` guard below).
pub(super) fn escalation_delay_ms(explicit_ms: i64, default_ms: i64) -> Option<i64> {
    let delay = if explicit_ms == 0 {
        default_ms
    } else {
        explicit_ms
    };
    (delay > 0).then_some(delay)
}

/// Session-scoped per-strategy aggregates (P-1.1 accounting), keyed
/// (strategy, account) since PR H — see `strategy_acc_key`.
#[derive(Debug, Default, Clone)]
pub(super) struct StrategyAccounting {
    pub(super) placed: u64,
    pub(super) rejected: u64,
    pub(super) limit_rejected: u64,
    /// E9-c: rejection-cause buckets for this strategy. The cause label is the
    /// machine-parseable part (see `classify_rejection`), so an operator can
    /// see WHY entries bounced without grep-ing logs. Only counted buckets are
    /// serialized (empty map → absent from the row).
    pub(super) rejection_causes: std::collections::BTreeMap<String, u64>,
    pub(super) closed_trades: u64,
    pub(super) wins: u64,
    pub(super) losses: u64,
    pub(super) fees_usd: Decimal,
    pub(super) net_pnl_usd: Decimal,
}

/// The `strategy_accounting` map key for one (strategy, account) scope. The
/// `\u{1}` separator cannot appear in a strategy name (manifest names are
/// identifier-like), so the pair is unambiguous; the default account keeps
/// the same shape as every other account — `strategy_stats` folds the
/// account dimension out for single-name reads, so no legacy spelling is
/// preserved anywhere.
pub(super) fn strategy_acc_key(strategy: &str, account: &str) -> String {
    format!("{strategy}\u{1}{account}")
}

/// The ONE cross-venue arbitrage strategy's package name. Single-instance by
/// contract: accounts.toml carries exactly one `[cross_venue_arb]` section,
/// so the arb totals on the stats rows ARE the arb unit's totals.
/// ponytail: 平台化多套利实例留待真有第二个实例再谈。
pub(crate) const ARB_STRATEGY: &str = "cross_venue_arb";

/// RFC-645 M2 deprecation warn, ONE-shot per process: the stats row's arb
/// surface rode the NAME branch (an exact `ARB_STRATEGY` match) instead of an
/// accepted `consumes_pair_book_stats` declaration. The name branch is kept
/// for exactly one release cycle (决议⑤) and removed in M3 — this log line is
/// the operator's pointer to the migration before that happens. Not per-tick
/// noise: `strategy_stats` is a read path, and the once-cell collapses every
/// later hit to nothing.
pub(super) fn arb_name_branch_warn_once(strategy: &str) {
    static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if WARNED.get().is_none() {
        // get_or_init runs the closure at most once even under races; the
        // warn rides inside it so a lost race prints nothing either.
        WARNED.get_or_init(|| {
            tracing::warn!(
                strategy = strategy,
                "strategy.stats arb* fields served via the DEPRECATED name branch \
                 (exact ARB_STRATEGY match) — declare consumes_pair_book_stats (trait \
                 default method) instead; the name branch is removed next release cycle (RFC-645 M3)"
            );
        });
    }
}

/// RFC #541 挂钩 B (c)：one declared-atomic strategy's cycle leg, captured by
/// value so the judges can read `&self` while the wholesale rejection mutates
/// the token list (the borrow split `arb_precheck_filter` lives inside).
#[derive(Clone)]
pub(super) struct CycleLeg {
    pub(super) internal_key: String,
    pub(super) token: String,
    pub(super) account: String,
    pub(super) notional: Decimal,
}

/// RFC #541 挂钩 B (c)：one declared-atomic strategy's cycle legs grouped by
/// the account each spends — `(account_id, [(internal_key, notional)])`,
/// request order preserved within an account, sorted by account id so the
/// pot sweep is deterministic. The per-account pot dimension reads exactly
/// these shares: under the v2 dual-leg stamp a pair may span two accounts,
/// and each leg's notional is judged against the pot of the account it
/// spends (no pot is charged for legs it does not own).
pub(super) fn per_account_shares(legs: &[CycleLeg]) -> Vec<(&str, Vec<(String, Decimal)>)> {
    let mut order: Vec<&str> = Vec::new();
    let mut groups: HashMap<&str, Vec<(String, Decimal)>> = HashMap::new();
    for leg in legs {
        let acct = leg.account.as_str();
        if !groups.contains_key(acct) {
            order.push(acct);
        }
        groups
            .entry(acct)
            .or_default()
            .push((leg.internal_key.clone(), leg.notional));
    }
    order.sort_unstable();
    order.dedup();
    order
        .into_iter()
        .map(|acct| {
            let share = groups.remove(acct).unwrap_or_default();
            (acct, share)
        })
        .collect()
}

/// The commit body shared by `virtual_batch_commit`: the borrow split (core
/// facts read first, the book mutated after) cannot be expressed inside one
/// `&mut self` method without re-borrowing, so it lives as a free function.
pub(super) fn virtual_batch_commit_impl(
    core: &mut Core,
    account_id: &str,
    strategy: &str,
    batch: &[(String, Decimal)],
) -> Result<(), String> {
    if !core.virtual_accounts.governed(account_id, strategy) {
        return Ok(());
    }
    let committed: Decimal = core
        .positions
        .open_positions()
        .iter()
        .filter(|p| p.account_id.as_str() == account_id && p.strategy == strategy)
        .map(|p| p.cost_usd)
        .sum();
    core.virtual_accounts
        .reserve_batch(account_id, strategy, batch, committed)
}

/// E9-c: classify a rejection message into a stable bucket label. Messages
/// come from `Risk::check`, `breaker.is_halted`, `positions.can_open`,
/// `strategy_limit_ok` and the ledger/OME reserve paths — matched by their
/// canonical prefixes so wording tweaks elsewhere stay visible: anything
/// unrecognized falls into a `other:<len-capped text>` bucket rather than
/// being silently lumped with a wrong cause.
pub(super) fn classify_rejection(err: &crate::model::CoreError) -> String {
    let msg = &err.message;
    let code_label = |c: crate::model::CoreErrorCode| {
        use crate::model::CoreErrorCode as E;
        match c {
            E::KillSwitchActive => "killswitch".to_string(),
            E::RiskRejected => "risk".to_string(),
            E::InsufficientFunds => "ledger.reserve".to_string(),
            E::WouldCross | E::InvalidTickSize | E::InvalidSize => "ome".to_string(),
            _ => "other".to_string(),
        }
    };

    if msg.contains("breaker active until") {
        "risk:breaker".to_string()
    } else if msg.starts_with("Max positions")
        || msg.starts_with("Already in")
        || msg.starts_with("Daily loss limit")
    {
        format!(
            "positions.{}",
            msg.split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join("_")
                .to_lowercase()
        )
    } else if msg.starts_with("SL cooldown") || msg.starts_with("Exit cooldown") {
        "positions.exitCooldown".to_string()
    } else if msg.starts_with("Loss cooldown") || msg.starts_with("Asset cooldown") {
        "positions.lossCooldown".to_string()
    } else if msg.contains("position cap") || msg.contains("notional cap") {
        "limit.cap".to_string()
    } else if msg.contains("in rejection cooldown") {
        // A local backoff pause after a venue refusal — the placement was
        // NOT re-attempted at all, which is a different bucket from a
        // genuine refusal (and must not inflate it).
        "venue.cooldown".to_string()
    } else if msg.starts_with("notional") && msg.contains("exceeds per-order cap") {
        "risk.perOrderCap".to_string()
    } else if msg.contains("outside") && msg.contains("price band") {
        "risk.priceBand".to_string()
    } else {
        format!("{}.{}", code_label(err.code), "other")
    }
}
