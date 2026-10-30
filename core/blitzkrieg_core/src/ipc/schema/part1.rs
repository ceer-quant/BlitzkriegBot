//! Split out of the parent module (issue #780): pure move.

use super::*;

// ── 行情插件凭据（next_session）— market.credentials.status / .set ──────────

/// One plugin's credential requirement, as the status readout reports it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCredentialRow {
    pub plugin: String,
    /// The env var names the plugin's executor reads at boot, in table order.
    pub required_vars: Vec<String>,
    /// Which of those names are resolved for the NEXT session (environment
    /// variable or persisted cell — the cell loses to the environment). Names
    /// only; values never travel back over IPC.
    pub resolved_vars: Vec<String>,
    /// All required names resolved → the executor will actually trade.
    pub ready: bool,
}

/// `market.credentials.status` result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsStatus {
    pub rows: Vec<PluginCredentialRow>,
}

/// `market.credentials.set` params: `vars` maps ENV VAR NAMES to values for
/// one plugin. Names are validated against the plugin's requirement table —
/// an unknown name is refused (a typo must fail now, not as a silent inert
/// executor). `reset: true` instead clears the plugin's cell entries.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetCredentialsParams {
    pub plugin: String,
    /// env var name → value. Empty map = clear the plugin's entries.
    #[serde(default)]
    pub vars: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub reset: bool,
    /// Audit trail only — echoed in the reply and the log line.
    pub reason: Option<String>,
}

/// `market.credentials.set` result. Values are never echoed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsUpdate {
    pub plugin: String,
    /// Var names written (or cleared).
    pub written: Vec<String>,
    pub reset: bool,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: &'static str,
}

/// [`CredentialsUpdate::note`] — one spelling, used by the reply and the log.
pub const CREDENTIALS_SET_NOTE: &str = "the values are upserted into .env (the single persistent \
     credential home); the launcher applies them at the next start, and a \
     value already in the operator's environment still outranks the file";

// ── system.reset_trading (#458) ─────────────────────────────────────────────

/// `system.reset_trading` params. `confirm` must be exactly `"RESET"` — a
/// free-text confirmation gate, not a checkbox: the verb wipes every trading
/// record this core owns (orders, trades, positions, live index) and re-seeds
/// the cash, so a misfired call must be impossible to blame on a double-click.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetTradingParams {
    pub confirm: String,
    /// Audit trail only — echoed in the reply, the log line and the
    /// manual-ops record.
    pub reason: Option<String>,
    /// issue 561 账户维隔离：重置哪个账户。`None` = 本连接的缺省账户
    /// （session → display → 进程 default，经 effective_default_account 链
    /// —— 与 ORDER_PLACE 的缺省单解析同一规则）；显式 id 照字面执行，
    /// 永不被会话格改写。重置只清洗该账户的账簿/挂单/持仓/记录，其他
    /// 账户分文不动。
    #[serde(default)]
    pub account_id: Option<String>,
}

/// `system.reset_trading` result: what was cleared, and the post-reset
/// self-check (live index empty, record files empty, balance == seed).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetTradingResult {
    pub disabled_strategies: Vec<String>,
    pub cancelled_orders: usize,
    pub cleared_open_positions: usize,
    pub cleared_closed_trades: usize,
    /// issue 561: 本次重置的账户范围 —— params.account_id 的最终解析值
    /// （显式 id 或连接缺省账户）。回执与审计行都以它为准，操作者必须
    /// 能看出「重置的是哪一本账」。
    pub account_id: String,
    /// Post-reset cash per account id. Every entry must equal the configured
    /// seed for a seeded core — the self-check asserts exactly that. issue 561
    /// 账户维重置下它只含目标账户一行；其他账户的余额不在清洗范围，也
    /// 不出现在回执里。
    pub balances: Vec<(String, Decimal)>,
    /// Pre-reset state snapshot persisted to the manual-ops audit record:
    /// jsonl line counts of the record files plus the pre-reset balance.
    pub pre_summary: ResetPreSummary,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The before-picture of a reset, carried in the reply AND the audit line so
/// the operator can later reconstruct what was deleted (the records themselves
/// are gone — this summary is what survives).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetPreSummary {
    pub order_log_lines: u64,
    pub trade_log_lines: u64,
    pub position_log_lines: u64,
    pub open_positions: usize,
    pub live_orders: usize,
    #[serde(with = "crate::decimal")]
    pub balance: Decimal,
}

/// `system.reset_trading` refuses LIVE mode with this exact reason — real
/// money records are append-only evidence, not a demo to wipe.
pub const RESET_TRADING_LIVE_REFUSAL: &str = "system.reset_trading is refused in live mode: real-money records are \
     append-only evidence and are never wiped by a UI action";

// ── Fee model quote (#182) ───────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeQuoteParams {
    /// Price to quote at. Defaults to `0.5`, where the schedule is widest.
    #[serde(default, with = "crate::decimal::opt")]
    pub price: Option<Decimal>,
}

/// The taker-fee schedule the kernel is actually charging, quoted at a price.
///
/// Exists so the gate/reconcile scripts stop carrying a COPY of the fee formula
/// (#182): a copy cannot notice the kernel changing its default, and the two
/// then disagree in exactly the direction that leaves a green gate behind. The
/// scripts take `fee_per_share` from here and assert the declared model against
/// a pinned expectation, so changing the kernel's model (or its parameters)
/// turns them red on purpose.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeQuoteResult {
    /// Declared model name, e.g. `legacy_quadratic` (see
    /// `exit_policy::fee_schedule`; a replay may run under another one, #203).
    pub model: String,
    /// Declared coefficient of the model.
    #[serde(with = "crate::decimal")]
    pub rate: Decimal,
    /// Declared exponent of the model.
    pub exponent: u32,
    /// The price this quote is for.
    #[serde(with = "crate::decimal")]
    pub price: Decimal,
    /// Fee charged per share at `price`, from the kernel's authoritative
    /// arithmetic — the number a fee assertion must be derived from.
    #[serde(with = "crate::decimal")]
    pub fee_per_share: Decimal,
    /// The same fee as a percentage of price (what `taker_fee_pct` reports and
    /// what `exitPolicy::takerFeePct` shows in trade records).
    #[serde(with = "crate::decimal")]
    pub fee_pct_of_price: Decimal,
    /// Maker fee per share: always zero (the schedule charges takers only).
    #[serde(with = "crate::decimal")]
    pub maker_fee_per_share: Decimal,
    /// True when the parameters this repository PINS for the declared model
    /// (`exit_policy::pinned_fee_parameters`) reproduce `fee_per_share` at this
    /// price — the cross-language half of that pin is `TAKER_FEE_MODELS` in
    /// `scripts/lib/fee-model.mjs`. A kernel whose shipped curve moved without
    /// its declaration moving reports `false` here: the split is reported, not
    /// silently papered over. Since #234 the check reads that pin rather than
    /// recomputing the charged curve, because two spellings of the same
    /// expression move together and can never disagree.
    pub model_matches: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BookLevel {
    #[serde(with = "crate::decimal")]
    pub price: Decimal,
    #[serde(with = "crate::decimal")]
    pub size: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookSnapshotParams {
    pub token_id: TokenId,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    #[serde(default)]
    pub ts_ms: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileParams {
    /// Exchange order ids currently resting.
    #[serde(default)]
    pub open_order_ids: Vec<String>,
    #[serde(default)]
    pub trades: Vec<ReconcileTrade>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileTrade {
    #[serde(rename = "venueOrderId")]
    pub venue_order_id: String,
    #[serde(rename = "tradeId")]
    pub trade_id: String,
    #[serde(rename = "tokenId")]
    pub token_id: TokenId,
    pub side: Side,
    #[serde(with = "crate::decimal")]
    pub size: Decimal,
    #[serde(with = "crate::decimal")]
    pub price: Decimal,
    #[serde(default)]
    pub ts_ms: i64,
    /// The venue's own maker/taker report for this trade, when Node knows it.
    /// Absent means "fall back to the order's fill policy".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maker: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileResultView {
    pub filled: usize,
    pub marked_filled: usize,
    pub marked_cancelled: usize,
    /// #457: ghosts the kernel closed itself (ForceClosed, not Cancelled).
    pub marked_force_closed: usize,
    pub ghost_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopOfBookParams {
    pub token_id: TokenId,
    #[serde(default, with = "crate::decimal::opt")]
    pub best_bid: Option<Decimal>,
    #[serde(default, with = "crate::decimal::opt")]
    pub best_ask: Option<Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotPriceParams {
    pub asset: String,
    #[serde(with = "crate::decimal")]
    pub price: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineMarketsParams {
    pub markets: Vec<CryptoMarket>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionView {
    pub id: String,
    pub asset: String,
    pub direction: String,
    pub strategy: String,
    pub token_id: TokenId,
    /// E28 (§9.2): the account this position settles into — a field read off
    /// the position (which carried it from the entry order), never a guess.
    /// The read side of "account_id 贯穿持仓": the panel filters on it after
    /// `account.switch` (整页重取，不合并显示).
    pub account_id: String,
    /// #427: the venue the position's market listed on — pure DATA for the
    /// panels' Venue column (a cross-venue pair shows one leg per venue).
    /// Empty = undeclared (old snapshots / the single-plugin world).
    #[serde(default)]
    pub venue: String,
    #[serde(with = "crate::decimal")]
    pub entry_price: Decimal,
    #[serde(with = "crate::decimal")]
    pub current_price: Decimal,
    #[serde(with = "crate::decimal")]
    pub shares: Decimal,
    #[serde(with = "crate::decimal")]
    pub unrealized_pct: Decimal,
    #[serde(with = "crate::decimal")]
    pub high_pnl_pct: Decimal,
    pub was_maker_entry: bool,
    pub entered_at_ms: i64,
    pub expires_at_ms: i64,
    pub remaining_sec: i64,
    /// The entry order's round slot (grid coordinate; slot N covers
    /// [N·dur, (N+1)·dur)). The panel judges expiry by SLOT MATCHING — same
    /// formula as the orders tab — instead of reading the countdown sign.
    /// `0` = legacy snapshot without a slot fact (the panel then falls back
    /// to `remaining_sec`'s sign).
    #[serde(default)]
    pub round_slot: i64,
    /// The position's OWN family duration in seconds (three-state fallback
    /// already resolved kernel-side). The panel multiplies `round_slot` by
    /// THIS — never by a page-level duration, which under multi-family
    /// deployment belongs to a different family (issue 733). `0` = legacy
    /// core without the field (panel falls back to its page-level hint).
    #[serde(default)]
    pub round_duration_sec: i64,
    /// The ACTUAL cash flows accrued on this position so far (E17). Exposed so an
    /// external monitor can verify the accounting identity at any instant, not
    /// only once the position is closed:
    ///
    /// ```text
    ///   balance == seed + Σ_closed net
    ///                    − Σ_open (entry_cost_usd + entry_fee_usd)
    ///                    + Σ_open (proceeds_usd − exit_fee_usd)
    /// ```
    ///
    /// Note the identity uses `entry_cost_usd` (TOTAL paid on the way in), NOT
    /// `cost_usd`: `cost_usd` is the basis of the shares STILL HELD, so a partial
    /// exit releases part of it and would otherwise be double-counted against the
    /// `proceeds_usd` that already returned it.
    ///
    /// A partially-exited position has both sides non-zero, which is exactly the
    /// case a flat-only check cannot see.
    #[serde(with = "crate::decimal")]
    pub entry_cost_usd: Decimal,
    /// Basis of the shares STILL HELD — a position-book figure, not a cash one.
    /// It shrinks on every partial exit (the released basis comes back inside
    /// `proceeds_usd`), so it must not be summed into the cash identity above.
    #[serde(with = "crate::decimal")]
    pub cost_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub entry_fee_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub proceeds_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub exit_fee_usd: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionExitParams {
    /// "all" flattens every open position; otherwise a specific position id.
    #[serde(default)]
    pub position_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionExitResult {
    pub closed: usize,
}

/// #684: `positions.force_close` params — names ONE position; every judgment
/// (expired? audited?) is the kernel's. Same body shape as
/// [`ForceCloseParams`] (the order-side verb) so the two verbs read alike.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionForceCloseParams {
    pub position_id: String,
}

/// #684: `positions.force_close` result — the closed row's id and the price
/// the bookkeeping close landed at (last known mid, else entry), so the
/// caller's receipt names exactly what was written.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionForceCloseResult {
    pub position_id: String,
    #[serde(with = "crate::decimal")]
    pub price: Decimal,
}

// ── v0.3 Wave 0 (#329): the two read-only envelopes ──────────────────────────
//
// The wire shapes the two Wave-0 `server.rs` arms speak (§12.2/§12.3), frozen
// here so E25/E28/E29 extend behaviour without re-spelling the envelopes.

/// `kline.history` params (§12.2): `{ "symbol", "interval", "limit"? }`.
///
/// `interval` is the shared [`KlineInterval`] — wire values `sec1` … `day1` —
/// so the enum spelling lives with the K-line type itself (`crate::kline`)
/// instead of a second copy of the strings here.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KlineHistoryParams {
    pub symbol: String,
    pub interval: KlineInterval,
    /// Bars requested. Default 200, capped at 1000 (§12.2). The cap is the
    /// aggregator's (E29) to enforce — no bars exist yet, so the Wave-0 arm
    /// returns the empty list and this field is carried, not applied.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `kline.history` result (§12.2): bars ascending by `openTimeMs`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KlineHistoryResult {
    pub symbol: String,
    pub interval: KlineInterval,
    pub klines: Vec<Kline>,
}

/// `intent.audit.tail` params (§12.3).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentAuditTailParams {
    /// Records to return, taken from the newest end. Defaults to 50.
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub strategy: Option<String>,
    /// `approved` / `modified` / `rejected` (§12.3), matched
    /// case-insensitively against the record's `decision.status`.
    #[serde(default)]
    pub decision: Option<String>,
}

/// `intent.audit.tail` result (§12.3).
///
/// `records` carries the log rows **verbatim** — the camelCase objects the
/// arbitration audit writes (§3.4) — rather than a mirror type: a second
/// struct over the same bytes would be a second spelling of the audit truth,
/// and one-truth-per-fact is the rule the arbitration design exists to keep.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentAuditTailResult {
    /// The newest `limit` records matching every filter, in file order
    /// (oldest → newest).
    pub records: Vec<serde_json::Value>,
    /// How many records matched within the scanned span — `records` is only
    /// the tail window. The tail read is BACKWARD (§12.3: 读尾部): when the
    /// scan stopped early (the window filled before the file head) earlier
    /// rows were never counted, so the field is OMITTED rather than report a
    /// fake full count; present only when the whole file was scanned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
}

// ── E28 (§12.1) — account management wire types ─────────────────────────────

/// `account.switch` params (§12.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSwitchParams {
    pub account_id: String,
}

/// `account.status` params (§12.1). `status` takes the wire spellings
/// (`active` / `read_only` / `frozen` / `suspended` — or the config file's
/// map form for a suspension); `reason` is the FLAT carrier the spec shows
/// for a suspension reason and overrides the placeholder a bare-string
/// `suspended` would carry. The tighten-only rule is enforced server-side,
/// not by the shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatusParams {
    pub account_id: String,
    pub status: AccountStatus,
    #[serde(default)]
    pub reason: Option<String>,
}

// ── #363 — execution policy wire types ──────────────────────────────────────

/// `execution_policy.set` params: the target account and the section to
/// write for it. Fields left `null` inherit the globals (fold semantics);
/// `rules` present REPLACES the section's rules entirely; `rules: []`
/// clears them. `cooldownSec` is the account's standing cooldown applied
/// after every entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionPolicySetParams {
    pub account_id: String,
    #[serde(default)]
    pub budget_ratio: Option<String>,
    #[serde(default)]
    pub min_budget_usd: Option<String>,
    #[serde(default)]
    pub max_budget_usd: Option<String>,
    #[serde(default)]
    pub min_equity_usd: Option<String>,
    #[serde(default)]
    pub max_positions_per_asset: Option<u32>,
    #[serde(default)]
    pub rules: Option<Vec<serde_json::Value>>,
}

/// `execution_policy.get` / `.reset` params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionPolicyGetParams {
    pub account_id: String,
}

/// `execution_policy.history` params: account optional (absent = all).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionPolicyHistoryParams {
    #[serde(default)]
    pub account_id: Option<String>,
}

/// One account section as the read arms report it — the EFFECTIVE view
/// (own overrides folded over the globals), decimals as strings.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPolicySectionView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_budget_usd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_equity_usd: Option<String>,
    pub max_positions_per_asset: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<serde_json::Value>>,
}

/// One audit line as `execution_policy.history` reports it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPolicyAuditView {
    pub ts_ms: i64,
    pub actor: String,
    pub action: String,
    pub account_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `execution_policy.preview` params (#364): the account whose section the
/// preview replays history against.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionPolicyPreviewParams {
    pub account_id: String,
}

/// `execution_policy.preview` result (#364): the CURRENT section replayed
/// over the recent closed trades, so the operator sees what their rules
/// would have done before saving them. Each historical close supplies the
/// facts a live entry would carry; `evaluate` — the ONE verdict function —
/// judges it, so the preview can never disagree with the kernel.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPolicyPreviewResult {
    /// How many historical entries were replayed.
    pub considered: usize,
    /// How many of those the current section would SKIP (a rule hit or the
    /// equity floor).
    pub skipped: usize,
    /// Mean budget across the entries the section would still place, as a
    /// string for the shortest-decimal rule (`None` when everything skips).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_budget_usd: Option<String>,
    /// One row per replayed entry, oldest first. The UI renders a few and
    /// keeps the verdict vocabulary intact.
    pub rows: Vec<ExecutionPolicyPreviewRow>,
}

/// One replayed entry (#364): the facts it carried and the verdict the
/// current section returns for them.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPolicyPreviewRow {
    pub ts_ms: i64,
    pub symbol: String,
    /// The account's balance at preview time (the ledger is a live book, not
    /// a historical snapshot — disclosed rather than pretended).
    #[serde(with = "crate::decimal")]
    pub balance: Decimal,
    #[serde(with = "crate::decimal")]
    pub price: Decimal,
    /// "place" | "skip" | "cooldown" | "refused" — the same words the
    /// kernel's own verdict path uses.
    pub verdict: String,
    /// The place budget, or the skip/cooldown reason — what the log line
    /// would have said.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One account as `account.list` reports it (§12.1 AccountView). The money
/// fields are the account's OWN book; `credentialsLoaded` is a boolean,
/// never a value (§9.4); `dayRealizedUsd` sums the account's own closes of
/// the current UTC day.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    pub id: String,
    pub name: String,
    pub market_type: blitzkrieg_market_api::MarketType,
    /// The wire tag (§12.1): `active` / `read_only` / `frozen` /
    /// `suspended` — a plain string, so a suspension does not turn the
    /// field into a map mid-list.
    pub status: String,
    /// The operator-facing reason, present only for `suspended`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    #[serde(with = "crate::decimal")]
    pub balance: Decimal,
    #[serde(with = "crate::decimal")]
    pub available: Decimal,
    #[serde(with = "crate::decimal")]
    pub reserved: Decimal,
    /// Presence fact only (§9.4): the kernel probed its own environment; the
    /// values never cross the wire in either direction.
    pub credentials_loaded: bool,
    /// The account's venue binding (a market-plugin name). Empty string =
    /// unbound — the account may trade any venue. The panel's account card
    /// renders the binding; an empty value renders as no binding.
    pub venue: String,
    /// The trading switch the panel renders as 「交易中/已停止」. `false` =
    /// Gate 2 refuses this account's new entries; its exits keep flowing.
    pub trading_enabled: bool,
    /// The account's strategy ENABLEMENT set (the 启用按钮's persistence —
    /// strategies are global, this is the per-account state). Empty = the
    /// account runs nothing explicitly (per-account enablement is the only
    /// truth; an absent key on a fresh deployment just leaves the account
    /// silent — the zero-config shape).
    pub strategies: Vec<String>,
    /// The account's round-length FAMILY declaration (轮次多族): every
    /// duration this account trades, in seconds — paired with the venue
    /// binding into the (venue, duration) families the admission gate
    /// reads. Empty = no family restriction declared (the account trades
    /// whatever the data plane feeds). Backward-compatible incremental
    /// field: old panels ignore it.
    pub rounds: Vec<i64>,
    pub open_positions: usize,
    #[serde(with = "crate::decimal")]
    pub day_realized_usd: Decimal,
    pub updated_at_ms: i64,
}

/// `account.list` result (§12.1). `active` is the SESSION's default account
/// when this connection switched, else the process-level one (§9.5).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountListResult {
    pub version: String,
    pub active: String,
    pub accounts: Vec<AccountView>,
}

/// `account.status` result (§12.1: `{ id, status }`, plus the suspension
/// reason when the tightened posture carries one).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatusResult {
    pub id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
}

// ── account↔venue binding — account.venue.get / account.venue.set ───────────

/// `account.venue.get` params: `accountId` narrows to ONE account; absent
/// (or empty) reports every configured one.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountVenueGetParams {
    #[serde(default)]
    pub account_id: Option<String>,
}

/// One account's binding row: empty string = UNBOUND (the account trades any
/// venue — the historical behaviour).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountVenueRow {
    pub account_id: String,
    pub venue: String,
    pub updated_at_ms: i64,
    /// Who performed the binding write (`account.venue.set`'s audit trail);
    /// a boot-loaded binding reports `boot`.
    pub actor: String,
}

/// `account.venue.get` result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountVenueGetResult {
    pub version: String,
    pub venues: Vec<AccountVenueRow>,
    /// This connection's default account (the SESSION cell when switched,
    /// else the process-level one — the same spelling `account.list` uses).
    pub active: String,
}

/// `account.venue.set` params: `venue` MUST name a registered market plugin
/// (plan-first) or be the empty string (unbind). `reason` is audit-only.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountVenueSetParams {
    pub account_id: String,
    pub venue: String,
    /// Audit trail only — echoed in the reply and the log line.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `account.venue.set` result: the binding that is now in force (memory AND
/// accounts.toml — `persisted` says the file write landed; a persist failure
/// refuses the whole write so the two stores cannot disagree).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountVenueSetResult {
    pub account_id: String,
    pub venue: String,
    pub persisted: bool,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: &'static str,
    /// The runtime half of 绑定即启用: whether THIS venue's data plane was
    /// mounted into the running session (feed + discovery) by the write —
    /// false when the venue was already live (boot enabled it) or the write
    /// is an unbind (empty venue). Older panels ignore the field; a newer
    /// one can say「已接入行情」instead of telling the operator to restart.
    #[serde(default)]
    pub runtime_enabled: bool,
}

/// [`AccountVenueSetResult::note`] — one spelling, used by the reply and the
/// log line.
pub const ACCOUNT_VENUE_SET_NOTE: &str = "binds orders.place to the named venue's markets (empty = \
     unbound); persisted to accounts.toml, re-read at next boot; the same \
     venue on several accounts is the deployment model, not a conflict";

// ── round-length family write — account.rounds.set (轮次多族 B2) ─────────────

/// `account.rounds.set` params: the WHOLE replacement set (not a delta — the
/// panel's card edits a list, the wire carries the list). `rounds` is
/// whitelist-validated and deduplicated (first-seen order kept), the empty
/// list removes the accounts.toml key (the unrestricted spelling). `reason`
/// is audit-only.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRoundsSetParams {
    pub account_id: String,
    pub rounds: Vec<i64>,
    /// Audit trail only — echoed in the reply and the log line.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `account.rounds.set` result: the set now in force (memory AND
/// accounts.toml — `persisted` says the file write landed; a persist failure
/// refuses the whole write so the two stores cannot disagree). Shape follows
/// [`AccountVenueSetResult`] minus the venue-specific fields — no per-family
/// change list: the reply's `rounds` IS the fact.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRoundsSetResult {
    pub account_id: String,
    pub rounds: Vec<i64>,
    pub persisted: bool,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: &'static str,
    /// The runtime half of 增族即挂: whether THIS write mounted at least one
    /// new (venue, duration) discovery instance into the running session.
    /// False when every family was already live (boot enabled it) or the set
    /// only shrank (removal is a soft-unload — see the note). Older panels
    /// ignore the field.
    #[serde(default)]
    pub runtime_enabled: bool,
}

/// [`AccountRoundsSetResult::note`] — one spelling, used by the reply and the
/// log line.
pub const ACCOUNT_ROUNDS_SET_NOTE: &str = "replaces the account's (venue, duration) family set \
     (empty = unrestricted, the key is removed from accounts.toml); persisted \
     first, memory second — a removal with open positions in the removed \
     families refuses the WHOLE write, a clean removal soft-unloads the \
     families' poll output without killing the running discovery instances, \
     an added family mounts its discovery instance at runtime; re-read at \
     next boot";

// ── 跨所套利弹窗 v2 — arb.config.set / arb.config.get ────────────────────────

/// `arb.config.set` params (the frozen Wire contract v1): two DISTINCT
/// venues, the account each trades under, and the assets to pair. Validation
/// is fail-closed server-side (APPLICATION errors naming the offending
/// field); a refused write moves nothing.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArbConfigSetParams {
    pub version: u32,
    pub leg_a_venue: String,
    pub leg_a_account: String,
    pub leg_b_venue: String,
    pub leg_b_account: String,
    pub pairs: Vec<String>,
    /// Audit trail only; defaults to "panel".
    #[serde(default)]
    pub actor: Option<String>,
}

/// `arb.config.get` result — the configured shape (v1 contract: `configured:
/// true` with every leg field + `pairs`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArbConfigGetConfigured {
    pub version: u32,
    pub configured: bool,
    pub leg_a_venue: String,
    pub leg_a_account: String,
    pub leg_b_venue: String,
    pub leg_b_account: String,
    pub pairs: Vec<String>,
}

/// `arb.config.get` result — the unconfigured shape (null legs, empty array;
/// never a partial shape).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArbConfigGetUnconfigured {
    pub version: u32,
    pub configured: bool,
    pub leg_a_venue: Option<String>,
    pub leg_a_account: Option<String>,
    pub leg_b_venue: Option<String>,
    pub leg_b_account: Option<String>,
    pub pairs: Vec<String>,
}

// ── 账户交易开关 + 策略分配 — account.trading.set / account.strategies.set ──

/// `account.trading.set` params: the per-account trading switch. `false` =
/// the account's NEW ENTRIES are refused (engine drop + Gate 2 backstop)
/// while its EXITS keep flowing — positions stay strategy-managed until
/// flat. `reason` is audit-only.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountTradingSetParams {
    pub account_id: String,
    pub enabled: bool,
    /// Audit trail only — echoed in the reply and the log line.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `account.trading.set` result: the switch that is now in force (memory
/// AND accounts.toml; a persist failure refuses the whole write).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountTradingSetResult {
    pub account_id: String,
    pub enabled: bool,
    pub persisted: bool,
    pub at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: &'static str,
}

pub const ACCOUNT_TRADING_SET_NOTE: &str = "false = new entries refused for this account, existing \
     positions stay managed until flat (exits always flow); persisted to \
     accounts.toml, re-read at next boot";

/// `account.freeze` params: the account-scoped kill switch. `frozen: true`
/// refuses the account's NEW orders (manual and strategy entries) while its
/// EXITS keep flowing; `frozen: false` lifts the freeze. Session-scoped —
/// not persisted to accounts.toml (an automatic freeze must not outlive the
/// process that observed its cause; a restart re-probes).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountFreezeParams {
    pub account_id: String,
    pub frozen: bool,
    /// Audit trail only — shown by `account.list` and echoed in the reply.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `account.freeze` result: the freeze state now in force, with the reason
/// currently on the book (a no-op re-freeze keeps the FIRST reason).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountFreezeResult {
    pub account_id: String,
    pub frozen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: &'static str,
}

pub const ACCOUNT_FREEZE_NOTE: &str = "true = new orders refused for this account (manual and \
     strategy entries), existing positions stay managed until flat (exits \
     always flow); session-scoped — a restart starts clean and re-probes";

/// `account.strategies.set` params: the account's strategy ENABLEMENT set
/// (the 启用按钮's persistence). Names are validated against the REGISTERED
/// strategy set plan-first (a typo must fail now, not as a silently inert
/// scope). `reason` is audit-only.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStrategiesSetParams {
    pub account_id: String,
    pub strategies: Vec<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `account.strategies.set` result. `kept_exits_only` names strategies the
/// account still holds positions in but that were REMOVED from the
/// assignment — they keep running as exits-only instances until flat (the
/// safety net: a removed strategy never orphans its positions).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStrategiesSetResult {
    pub account_id: String,
    pub strategies: Vec<String>,
    pub kept_exits_only: Vec<String>,
    pub persisted: bool,
    pub at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: &'static str,
}

pub const ACCOUNT_STRATEGIES_SET_NOTE: &str = "the account's strategy ENABLEMENT set (the 启用 \
     button's persistence; strategies are global, this is per-account state); \
     a scope trades ⇔ the strategy is installed AND the account enabled AND \
     the strategy ∈ this set; a removed strategy with open positions keeps \
     managing them exits-only until flat";

// ── 显示账户配置化 — display.account.set ─────────────────────────────────────

/// `display.account.set` params: the account the snapshot's
/// `displayAccount`/`displayVenue` should point at. The kernel validates the
/// account EXISTS and is `active` (plan-first), then updates the runtime
/// pointer AND the accounts.toml top-level `display_account` key in one
/// transaction. `reason` is audit-only.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayAccountSetParams {
    pub account_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `display.account.set` result: the pointer now in force (memory AND
/// accounts.toml; a persist failure refuses the whole write). `displayVenue`
/// is the pointed-at account's venue binding — empty = unbound.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayAccountSetResult {
    pub account_id: String,
    pub display_venue: String,
    pub persisted: bool,
    pub at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: &'static str,
}

pub const DISPLAY_ACCOUNT_SET_NOTE: &str = "points the snapshot's displayAccount/displayVenue at \
     the named account — a pure presentation pointer (NOT the active/ \
     accounting/strategy-enablement axis; no trading semantics attach); \
     persisted to accounts.toml's top-level display_account key, re-read at \
     next boot (fail-closed on a dangling id)";

// ── account.create / account.remove — 账户簿运行时增删 ──────────────────────

/// `account.create` params: the new account's identity. NO credential field
/// exists here (§9.4 — the panel wires credentials through the existing
/// `market.credentials.set` env-name path AFTER the identity exists); `name`
/// absent = the id; `venue` absent/empty = unbound (the account trades any
/// venue — the historical behaviour; a non-empty name is checked against the
/// plugin registry plan-first, exactly like `account.venue.set`). `actor` is
/// audit-only, `reason` audit-only.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountCreateParams {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub venue: Option<String>,
    /// Audit trail only — who asked for the identity (the panel sends
    /// "panel"); echoed in the reply and the log line.
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `account.create` result: the created id, whether the accounts.toml write
/// landed (`false` is impossible today — a persist failure refuses the whole
/// call, the field is the contract's honesty disclosure), one fixed note, and
/// whether the named venue's data plane was mounted into the running session.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountCreateResult {
    pub id: String,
    pub persisted: bool,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: &'static str,
    /// The runtime half of 绑定即启用, on the CREATE face: a request that
    /// names a venue mounts that venue's data plane (feed + discovery) into
    /// the running session — creating a bound account IS binding, and the
    /// 行情 page must light up without a restart. False when the venue was
    /// already live (boot enabled it, or an earlier runtime mount), the
    /// venue is empty (unbound), or the boot's data-plane flags are off.
    #[serde(default)]
    pub runtime_enabled: bool,
}

/// [`AccountCreateResult::note`] — one spelling, used by the reply and the
/// log line.
pub const ACCOUNT_CREATE_NOTE: &str = "creates one ACTIVE prediction account (a restricted one is \
     account.status's job, never a runtime mint); persisted to accounts.toml, \
     re-read at next boot; plugin credential names are attached automatically, \
     while values travel separately through market.credentials.set";

/// `account.remove` params: the account to retire. `actor`/`reason`
/// audit-only.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRemoveParams {
    pub id: String,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `account.remove` result: the retired id, whether the accounts.toml write
/// landed, and one fixed note.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRemoveResult {
    pub id: String,
    pub persisted: bool,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: &'static str,
}

/// [`AccountRemoveResult::note`] — one spelling, used by the reply and the
/// log line.
pub const ACCOUNT_REMOVE_NOTE: &str = "removes the account from the book (memory AND accounts.toml, \
     re-read at next boot); every account is removable — `default` included — \
     but the LAST account and an account still holding open positions are \
     refused; removing the process-active account re-activates the first \
     surviving account";

// ── #509 的读面 — account.journal（钱包流水的面板可读面）─────────────────────

/// `account.journal` params: `accountId` absent = the SESSION's default
/// account (the `account.list` resolution, §9.5); `limit` defaults to 100
/// and is clamped into `1..=500` (the same clamp posture `kline.history`
/// takes — a big ask shrinks, it never errors).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountJournalParams {
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One cash-movement row as `account.journal` reports it: `JournalEntry`
/// verbatim (camelCase, money as decimal strings through `crate::decimal`,
/// `ref` carried by the type's own rename). Ascending `atMs` — the ledger's
/// journal order IS the wire order.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountJournalRow {
    pub at_ms: i64,
    pub kind: crate::ledger::JournalKind,
    #[serde(with = "crate::decimal")]
    pub amount: Decimal,
    #[serde(with = "crate::decimal")]
    pub balance_after: Decimal,
    pub r#ref: String,
}

/// `account.journal` result: the newest `limit` rows, oldest → newest.
/// `truncated` is the disclosure the in-memory window owes the panel: TRUE
/// when the snapshot held MORE rows than `rows` returned, so a reader knows
/// the tail they see is a window, not the whole trail.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountJournalResult {
    pub account_id: String,
    pub rows: Vec<AccountJournalRow>,
    pub truncated: bool,
}

// ── Server → Node events ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Event {
    Ready {
        version: String,
        mode: Mode,
    },
    OrderUpdate {
        order: TrackedOrder,
    },
    /// Canonical position effect derived from an authoritative fill.
    Fill {
        delta: FillDeltaView,
        order: TrackedOrder,
    },
    /// A position was closed (PnL realised); Node renders the human log.
    PositionClosed {
        id: String,
        asset: String,
        direction: String,
        reason: String,
        /// The position's identity rides along because a close is only
        /// meaningful in context: `strategy` attributes the trade, and
        /// `conditionId` groups the legs of ONE trade that closed as several
        /// positions — a complete-set pair settles as two (winner $1 / loser
        /// $0) whose statistics exist only as a group.
        strategy: String,
        token_id: String,
        condition_id: String,
        /// Entry price of the position — the #353 price-band view groups
        /// closed trades by it.
        #[serde(with = "crate::decimal", rename = "entryPrice")]
        entry_price: Decimal,
        /// E28 (§9.2): the account the trade settled in — per-account history
        /// is a field read, never a strategy-name guess.
        #[serde(rename = "accountId")]
        account_id: AccountId,
        #[serde(with = "crate::decimal", rename = "netPnlUsd")]
        net_pnl_usd: Decimal,
        #[serde(with = "crate::decimal", rename = "netPnlPct")]
        net_pnl_pct: Decimal,
        #[serde(with = "crate::decimal", rename = "dailyPnlUsd")]
        daily_pnl_usd: Decimal,
    },
    RiskAlert {
        code: CoreErrorCode,
        message: String,
    },
    /// Outcome of a reconciliation sweep (also emitted when nothing changed only
    /// if there were ghosts/actions; quiet sweeps emit nothing).
    ReconcileReport {
        filled: usize,
        #[serde(rename = "markedFilled")]
        marked_filled: usize,
        #[serde(rename = "markedCancelled")]
        marked_cancelled: usize,
        /// #457: ghosts the KERNEL closed itself (venue forgot the order, no
        /// fill) — the "we gave up waiting" counter beside markedCancelled.
        #[serde(rename = "markedForceClosed", default)]
        marked_force_closed: usize,
        #[serde(rename = "ghostIds")]
        ghost_ids: Vec<String>,
    },
    Error {
        error: CoreError,
    },
    /// Shadow Evolution: a proposal was produced (pre-application).
    EvolutionSignal {
        signal: crate::shadow_evolution::EvolveSignal,
    },
    /// Shadow Evolution: parameters were atomically swapped in.
    EvolutionApplied {
        signal: crate::shadow_evolution::EvolveSignal,
    },
    /// Shadow Evolution: a proposal was rejected by a safety lock (or a rollback).
    EvolutionRejected {
        signal: crate::shadow_evolution::EvolveSignal,
        reason: String,
    },
    /// E13: a qualifying variant was HELD as a proposal — the operator decides
    /// (`shadow_evolution.decide`); nothing moved in the hot path.
    EvolutionProposed {
        proposal: crate::shadow_evolution::EvolutionProposal,
    },
    /// E13: the 72h DEEP evolution round fired — every strategy's variant set
    /// was re-anchored with compound multi-knob mutants.
    EvolutionCycle {
        #[serde(rename = "cycleSeq")]
        cycle_seq: u64,
        dims: usize,
        strategies: Vec<String>,
        #[serde(rename = "atMs")]
        at_ms: i64,
    },
    /// E25 (#331, §12.3): a suggestion was adjudicated by the four-gate
    /// pipeline. THROTTLED — pushes fold per `(strategy, gate, status)` to at
    /// most one per second; `count` is how many decisions one push summarizes.
    /// The audit (`data/audit/intents.jsonl`) is NEVER folded.
    IntentDecision {
        #[serde(rename = "accountId")]
        account_id: String,
        strategy: String,
        #[serde(rename = "intentId")]
        intent_id: String,
        /// `APPROVED` / `MODIFIED` / `REJECTED` — the same vocabulary the
        /// `intent.audit.tail` filter uses.
        status: String,
        gate: String,
        /// The kernel's own justification, verbatim from the `GateTrace`
        /// (the panel prints it as-is, §13.4 — UI never re-words it).
        detail: String,
        #[serde(rename = "tsMs")]
        ts_ms: i64,
        count: u64,
    },
    /// E29 (§12.2): one K-line bar. Pushed twice-shaped: a CLOSED bar exactly
    /// once (never throttled — a swallowed close would leave the chart
    /// missing a bar forever), and the still-growing bar at most 1/s per
    /// `(symbol, interval)` (the "正在长" preview). `isClosed` tells the two
    /// apart; the strategy-visible `on_kline` callback sees CLOSED only.
    KlineUpdate {
        kline: crate::kline::Kline,
    },
}

/// Wire view of FillDelta (field names match Node conventions).
#[derive(Debug, Clone, Serialize)]
pub struct FillDeltaView {
    #[serde(rename = "orderId")]
    pub order_id: OrderId,
    #[serde(rename = "tokenId")]
    pub token_id: TokenId,
    /// E28 (§9.2): the account the fill's cash moved in — copied from the
    /// tracked order, so a panel can group fills per account without a
    /// lookup.
    #[serde(rename = "accountId")]
    pub account_id: AccountId,
    pub side: Side,
    #[serde(with = "crate::decimal")]
    pub delta: Decimal,
    #[serde(with = "crate::decimal")]
    pub price: Decimal,
    #[serde(with = "crate::decimal")]
    pub cumulative: Decimal,
    pub strategy: String,
    pub asset: String,
    pub direction: String,
    #[serde(rename = "conditionId")]
    pub condition_id: String,
}

impl From<FillDelta> for FillDeltaView {
    fn from(d: FillDelta) -> Self {
        Self {
            order_id: d.order_id,
            token_id: d.token_id,
            account_id: d.account_id,
            side: d.side,
            delta: d.delta,
            price: d.price,
            cumulative: d.cumulative,
            strategy: d.strategy,
            asset: d.asset,
            direction: d.direction,
            condition_id: d.condition_id,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Event,
}
impl Notification {
    pub fn new(event: Event) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            method: EVENT_METHOD.into(),
            params: event,
        }
    }
}
