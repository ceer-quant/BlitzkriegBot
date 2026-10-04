//! IPC contract — Unix-domain-socket JSON-RPC 2.0.
//!
//! These serde types are the SINGLE schema between Rust core and Node. Node
//! mirrors them with zod; there is no `any`/`unknown` on the wire.
//!
//! Framing: one JSON object per line (`\n`), UTF-8.
//!
//!   Node → Rust  request:      { jsonrpc:"2.0", id, method, params }
//!   Rust → Node  response:     { jsonrpc:"2.0", id, result | error }
//!   Rust → Node  notification: { jsonrpc:"2.0", method:"core.event", params: Event }
//!
//! Node never sends a private key; the core loads credentials itself.

use crate::account::AccountStatus;
use crate::kline::{Kline, KlineInterval};
use crate::model::*;
use crate::ome::FillDelta;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub const JSONRPC: &str = "2.0";
pub const EVENT_METHOD: &str = "core.event";

// ── Envelope ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    #[serde(default = "rpc_version")]
    pub jsonrpc: String,
    /// Protocol/schema version (standardisation: every IPC message carries one).
    #[serde(default = "protocol_version")]
    pub version: String,
    /// Any JSON value (string/number/null); echoed verbatim.
    pub id: serde_json::Value,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Current protocol version reported/expected on the IPC boundary.
pub const PROTOCOL_VERSION: &str = "1.1";
fn protocol_version() -> String {
    PROTOCOL_VERSION.into()
}
fn rpc_version() -> String {
    JSONRPC.into()
}

#[derive(Debug, Clone, Serialize)]
pub struct Success {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<ErrorData>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorData {
    pub core_code: CoreErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub error: RpcError,
}

impl Success {
    pub fn new(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id,
            result,
        }
    }
}
impl Failure {
    pub fn new(id: serde_json::Value, code: i32, message: String, data: Option<ErrorData>) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id,
            error: RpcError {
                code,
                message,
                data,
            },
        }
    }
    /// JSON-RPC reserved codes.
    pub const PARSE_ERROR: i32 = -32700;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    /// Application error (see CoreErrorCode in data.core_code).
    pub const APPLICATION: i32 = -32000;
}

// ── Method names ─────────────────────────────────────────────────────────────

pub mod method {
    pub const PING: &str = "core.ping";
    pub const READY: &str = "core.ready";
    /// Read-only quote of the taker fee schedule the kernel is charging (#182).
    pub const CORE_FEE_QUOTE: &str = "core.feeQuote";
    pub const RISK_KILL: &str = "risk.kill";
    pub const RISK_RESUME: &str = "risk.resume";
    /// Limited hot reload of the ENTRY limits (#191): the per-order /
    /// portfolio notional caps and the entry share band, applied in memory
    /// with an audit record. Everything else is refused by name — see
    /// [`crate::risk::hot_reload_refusal`].
    pub const RISK_SET_LIMITS: &str = "risk.setLimits";
    pub const ORDER_PLACE: &str = "orders.place";
    pub const ORDER_CANCEL: &str = "orders.cancel";
    pub const ORDER_CANCEL_ALL: &str = "orders.cancel_all";
    /// E31-c: cancel the resting remainder of a partially filled order.
    pub const ORDER_CANCEL_REMAINING: &str = "orders.cancel_remaining";
    /// E31-c: sell only the shares already filled behind an order.
    pub const ORDER_CLOSE_FILLED: &str = "orders.close_filled";
    pub const ORDER_LIST: &str = "orders.list";
    pub const LEDGER_BALANCE: &str = "ledger.balance";
    /// List open positions (enriched with current price / unrealised PnL).
    pub const POSITIONS_LIST: &str = "positions.list";
    /// Force-close a position at market (manual flatten).
    pub const POSITION_EXIT: &str = "positions.exit";
    /// Closed-trade history (Node-compatible records) for the UI.
    pub const TRADES_HISTORY: &str = "trades.history";
    pub const TRADES_SUMMARY: &str = "trades.summary";
    /// Manually trigger a reconciliation sweep with a caller-supplied snapshot
    /// (live venue snapshot is gathered internally in live mode; this method is
    /// mainly for tests/ops).
    pub const ORDER_RECONCILE: &str = "orders.reconcile";
    /// P0 bridge: Node pushes the latest L2 snapshot so DRY mode can simulate
    /// fills. Replaced in P3 when market-data ingestion moves into the core.
    pub const BOOK_SNAPSHOT: &str = "books.snapshot";
    /// Top-of-book only update (price_change / best_bid_ask).
    pub const TOP_OF_BOOK: &str = "books.top";
    /// Data-feed bridge: deliver a full L2 book straight to the self-driving
    /// engine (`Core::engine_on_data`) — the same choke point the Rust-native
    /// market feed (`--feed-ws`) drives and the one the backtester replays.
    /// Unlike `books.snapshot` it does NOT run the DRY maker-fill simulation, so
    /// a session captured through this method replays decision-for-decision.
    pub const ENGINE_BOOK: &str = "engine.book";
    /// Binance spot price tick (feeds the momentum filter).
    pub const SPOT_PRICE: &str = "spot.price";
    /// Current round state (slot / timing / market count).
    pub const ENGINE_ROUND: &str = "engine.round";
    /// Read-only depth view: the round's assets with each side's live L2 levels,
    /// best prices, OBI and spread — what the WebUI's 盘口深度 chart renders.
    /// Same books the engine trades on; purely observational, no side effects.
    pub const ENGINE_BOOKS: &str = "engine.books";
    /// Diagnostic snapshot: feed counters + confirmed trend tokens.
    pub const ENGINE_STATS: &str = "engine.stats";
    /// List strategies registered in the kernel's strategy engine.
    pub const STRATEGY_LIST: &str = "strategy.list";
    /// Enable/disable a strategy by name.
    pub const STRATEGY_ENABLE: &str = "strategy.enable";
    /// Load a user-layer strategy shared library (feature `strategy-loading`).
    pub const STRATEGY_LOAD: &str = "strategy.load";
    /// Unload a dynamic strategy (drop instance + free the library). Refuses
    /// in-tree or still-enabled strategies and open positions (E9-b).
    pub const STRATEGY_UNLOAD: &str = "strategy.unload";
    /// Atomic swap (E9-b): load the NEW dylib under the SAME registered name,
    /// dropping the old instance only after the new one parses; enable state
    /// is preserved. `params: {name, path}`.
    pub const STRATEGY_RELOAD: &str = "strategy.reload";
    /// Extension lifecycle: list / enable / disable / uninstall.
    pub const EXTENSION_LIST: &str = "extension.list";
    pub const EXTENSION_ENABLE: &str = "extension.enable";
    pub const EXTENSION_DISABLE: &str = "extension.disable";
    /// Market plugins: list registered market extensions (name/type/capabilities).
    pub const MARKET_LIST: &str = "market.list";
    /// Network self-check: probe the active market plugin's network paths
    /// (resolver → TCP → TLS → one cheap request per endpoint) and answer with a
    /// `NetCheckReport`. Read-only and credential-free; the answer to "is this a
    /// network problem or a venue problem?" when a bot has gone quiet.
    pub const NET_CHECK: &str = "net.check";
    /// Shadow Evolution control surface (opt-in feature). Every mutation is
    /// per-strategy (E2-c): `apply`/`rollback` name ONE strategy, and `status`
    /// reports each evolved strategy's own block.
    pub const SE_ENABLE: &str = "shadow_evolution.enable";
    pub const SE_DISABLE: &str = "shadow_evolution.disable";
    pub const SE_STATUS: &str = "shadow_evolution.status";
    pub const SE_HISTORY: &str = "shadow_evolution.history";
    pub const SE_ROLLBACK: &str = "shadow_evolution.rollback";
    pub const SE_APPLY: &str = "shadow_evolution.apply";
    /// E13 proposal workflow: every known proposal's latest state (the UIs'
    /// 对比表), the operator's verdict on one proposal, and the auto-evolve
    /// switch (`params.enabled: bool`, persisted across restarts).
    pub const SE_PROPOSALS: &str = "shadow_evolution.proposals";
    pub const SE_DECIDE: &str = "shadow_evolution.decide";
    pub const SE_SET_AUTO: &str = "shadow_evolution.set_auto";
    /// Supply the current round's UP/DOWN markets (discovered by Node's scanner
    /// in P3-transition; the Rust scanner takes over in P4).
    pub const ENGINE_MARKETS: &str = "engine.markets";
    /// Version + build provenance + update state (VERSIONING.md §5).
    /// Request: empty params `{}`; reply: a `SystemVersion` payload. Read-only,
    /// side-effect-free, takes no Core lock.
    pub const SYSTEM_VERSION: &str = "system.version";
    /// Read/write of the update switches (§7.4). The write lands an audit record.
    pub const SYSTEM_UPDATE_CONFIGURE: &str = "system.update.configure";
    /// One manual update check (the UI's "check for updates" button).
    pub const SYSTEM_UPDATE_CHECK: &str = "system.update.check";

    // ── v0.3 Wave 0 (#329) — the two read-only envelope freezes ─────────────
    // The two arms Wave 0 lands in `server.rs`, with their wire shapes frozen
    // here (§12.2/§12.3). Both answer truthfully with an empty envelope until
    // their producers land (E25: `data/audit/intents.jsonl`; E29: the K-line
    // aggregator); neither takes the Core lock.

    /// Historical K-lines for one `(symbol, interval)` (§12.2). Read-only.
    pub const KLINE_HISTORY: &str = "kline.history";
    /// Tail of the intent-arbitration audit log (§12.3). Read-only.
    pub const INTENT_AUDIT_TAIL: &str = "intent.audit.tail";

    // ── E28 (§12.1) — account management ────────────────────────────────────
    /// Every configured account with its own book (§12.1 AccountView).
    /// Read-only.
    pub const ACCOUNT_LIST: &str = "account.list";
    /// Move THIS connection's default account (§9.5: session-level — the
    /// process-level active account and every other connection are
    /// untouched).
    pub const ACCOUNT_SWITCH: &str = "account.switch";
    /// Tighten one account's lifecycle status (§12.1: a live call may never
    /// GRANT a capability — unfreezing is config + restart).
    pub const ACCOUNT_STATUS: &str = "account.status";

    // ── #363 — execution policy surface ─────────────────────────────────────
    /// List every account section the loaded policy names, plus the effective
    /// global defaults. Read-only.
    pub const EXECUTION_POLICY_LIST: &str = "execution_policy.list";
    /// One account's effective section (its own overrides folded over the
    /// globals) and its rules. Read-only.
    pub const EXECUTION_POLICY_GET: &str = "execution_policy.get";
    /// Write ONE account's section to the policy TOML (section-replace,
    /// never a whole-file rewrite of other accounts), then full-reload the
    /// policy in memory and land one audit line. Refuses accounts other
    /// than the named one — cross-account writes are a fail-closed no.
    pub const EXECUTION_POLICY_SET: &str = "execution_policy.set";
    /// Remove one account's section from the file (it falls back to the
    /// globals), full-reload, audit.
    pub const EXECUTION_POLICY_RESET: &str = "execution_policy.reset";
    /// The audit journal tail for one account (or all accounts).
    pub const EXECUTION_POLICY_HISTORY: &str = "execution_policy.history";
    /// #364: replay the CURRENT section over the recent closed trades of one
    /// account (`evaluate` on historical facts) — the UI preview's "these
    /// rules would skip N of the last M entries". Read-only, no audit line.
    pub const EXECUTION_POLICY_PREVIEW: &str = "execution_policy.preview";

    // ── E26 (§4.4) — systemic risk readout ────────────────────────────────
    /// The effective systemic limits and WHERE each came from. Read-only,
    /// zero side effects, and answered from a boot-time snapshot WITHOUT the
    /// Core lock: the nine new limits are restart-to-change (deliberately
    /// absent from `risk.setLimits`) and the exit resolution is not a #191
    /// hot key either, so nothing on this readout can go stale mid-run.
    pub const RISK_LIMITS: &str = "risk.limits";

    // ── E29 (§12.2) — K-line subscriptions ─────────────────────────────────
    // (`kline.history` itself was frozen by Wave 0 above; only the
    // session-scoped subscription pair is new here.)
    /// Session-scoped subscription (§12.2: dies WITH the connection, so a
    /// closed panel cannot keep receiving market pushes).
    pub const KLINE_SUBSCRIBE: &str = "kline.subscribe";
    /// Explicit counterpart; the auto-unsubscribe on disconnect makes this
    /// optional for clients, not useless (a live panel can narrow its feed).
    pub const KLINE_UNSUBSCRIBE: &str = "kline.unsubscribe";

    // ── #353 — the WebUI backtest surface (拉数据→配置→回测→看结果) ──────────
    // Every arm fronts the in-kernel job registry (`crate::backtest_jobs`):
    // the WebUI names datasets/assets/strategy names over IPC and NEVER
    // touches the local filesystem or executable code — the spec's hard
    // boundary. Pull = resumable, cache-addressable #352 pulls (one per asset
    // filter); run = the #351 mode ladder on one dataset; status/result/export
    // poll a running or finished job.
    pub const BACKTEST_ONCHAIN_PULL: &str = "backtest.onchain.pull";
    pub const BACKTEST_ONCHAIN_LIST: &str = "backtest.onchain.list";
    pub const BACKTEST_RUN: &str = "backtest.run";
    pub const BACKTEST_STATUS: &str = "backtest.status";
    pub const BACKTEST_RESULT: &str = "backtest.result";
    pub const BACKTEST_EXPORT: &str = "backtest.export";

    // ── #362 — the blueprint editor surface (compile + save) ─────────────────
    // The WebUI canvas authors a blueprint JSON and asks the KERNEL to do
    // everything that touches bytes on disk: `blueprint.compile` is the pure
    // three-stage compiler behind the preview pane (a refusal is the same
    // node-id-bearing message the CLI prints), `blueprint.save` validates THEN
    // writes the strategy package (blueprint.json + strategy.lua + manifest
    // under `user_layer/strategies_lua/<name>/`, the directory the loader
    // scans). The UI never touches the filesystem directly — the repo iron
    // rule — and the save receipt is the caller's only proof the write landed.
    pub const BLUEPRINT_COMPILE: &str = "blueprint.compile";
    pub const BLUEPRINT_SAVE: &str = "blueprint.save";
}

// ── #362 — the blueprint editor surface ──────────────────────────────────────

/// `blueprint.compile` params: `{ "json": "<blueprint document>" }`.
///
/// The document travels as a STRING, not a nested JSON object, so the compile
/// errors keep quoting the exact bytes the editor sent — the same property
/// `--blueprint-compile <file>` has on the CLI side.
#[derive(Debug, Clone, Deserialize)]
pub struct BlueprintCompileParams {
    pub json: String,
}

/// `blueprint.compile` result: the generated Lua source, verbatim.
#[derive(Debug, Clone, Serialize)]
pub struct BlueprintCompileResult {
    pub lua: String,
}

/// `blueprint.save` params. `name` is the package identity (directory name =
/// manifest name, §6.4); `json` is the blueprint document, compiled fresh by
/// the save arm — the caller sending a stale `lua` is not a thing: the kernel
/// compiles, and what it compiled is what lands on disk. `overwrite` must be
/// explicit to replace a package the loader already scans (a silent overwrite
/// could swap a strategy an operator believes they know).
#[derive(Debug, Clone, Deserialize)]
pub struct BlueprintSaveParams {
    pub name: String,
    pub json: String,
    #[serde(default)]
    pub overwrite: bool,
}

/// `blueprint.save` result: the package receipt. Every path is RELATIVE (the
/// kernel's own strategy root), so a UI can show it without learning the host
/// filesystem.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlueprintSaveResult {
    pub name: String,
    pub package_dir: String,
    pub blueprint_path: String,
    pub lua_path: String,
    pub manifest_path: String,
    /// The sha256 of the written `strategy.lua`, hex — the same digest the
    /// manifest's `sha256` field carries, so the receipt is checkable against
    /// the file without rehashing.
    pub lua_sha256: String,
    /// Bytes written per file, in blueprint/lua/manifest order — the receipt
    /// the IPC contract promises (a save that "succeeded" without saying WHAT
    /// it wrote is not auditable).
    pub bytes: [u64; 3],
}

// ── E26 (§4.4) — systemic risk readout ──────────────────────────────────────

/// The whole `risk.limits` readout: WHAT each systemic limit is and WHERE it
/// came from, plus the exit resolution the entry pipeline binds at Gate 4.
/// Assembled ONCE at boot — the nine new limits change only with a restart
/// (deliberately absent from `risk.setLimits`) and the exit ladder is not a
/// #191 hot key — so the arm serves this snapshot without the Core lock,
/// exactly like `system.version`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskLimitsResult {
    /// Readout contract version (§4.4).
    pub version: String,
    /// The per-account matrix, shown for the kernel's default account
    /// (§4.2: every account runs under the SAME configured matrix).
    pub account: RiskAccountLimitsView,
    /// The process-wide matrix.
    pub global: RiskGlobalLimitsView,
    /// The exit triple Gate 4 binds per entry.
    pub exit: RiskExitView,
}

impl RiskLimitsResult {
    /// The boot-time snapshot: systemic limits + exit resolution exactly as
    /// THIS run resolved them.
    pub fn snapshot(
        systemic: &crate::risk::limits::SystemicRiskLimits,
        exit: &crate::exit_policy::ExitConfig,
    ) -> Self {
        Self {
            version: "1.1".into(),
            account: RiskAccountLimitsView {
                id: default_account_id().as_str().to_owned(),
                limits: systemic.account.clone(),
            },
            global: RiskGlobalLimitsView {
                limits: systemic.global.clone(),
            },
            exit: RiskExitView {
                stop_loss_pct: exit.stop_loss_pct,
                take_profit_pct: exit.take_profit_pct,
                force_exit_sec: exit.force_exit_sec,
            },
        }
    }
}

/// One account's slice of the readout.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskAccountLimitsView {
    pub id: String,
    pub limits: crate::risk::limits::AccountRiskLimits,
}

/// The process-wide matrix. Bare `limits` (no id): the holder is the kernel
/// process itself, not any one account.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskGlobalLimitsView {
    pub limits: crate::risk::limits::GlobalRiskLimits,
}

/// The exit resolution Gate 4 binds: stop/take percentages of the entry fill
/// and the hard force-exit deadline. Plain numbers — the IPC contract's usual
/// Decimal convention — NOT the `Bound` envelope: these are behaviour knobs
/// with factory values, not operator limits with provenance.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskExitView {
    #[serde(with = "crate::decimal")]
    pub stop_loss_pct: Decimal,
    #[serde(with = "crate::decimal")]
    pub take_profit_pct: Decimal,
    pub force_exit_sec: i64,
}

// ── E29 (§12.2) — K-line subscriptions ───────────────────────────────────────
// (`kline.history`'s envelope was frozen by Wave 0 below; E29 extends its
// BEHAVIOUR — real bars — without re-spelling the shape. One semantic note
// now that bars exist: `klines` is oldest first with the still-growing bar
// riding LAST (`isClosed = false`), so a chart draws it translucent without
// a second call.)

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KlineSubscribeParams {
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub intervals: Vec<crate::kline::KlineInterval>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KlineSubscribeResult {
    /// Size of the session's subscription set after this call (dedup —
    /// resubscribing an existing pair changes nothing).
    pub subscribed: usize,
}

// ── Typed params / results ───────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceParams {
    #[serde(flatten)]
    pub order: OrderRequest,
    /// Resting maker order escalates to taker after this many ms if unfilled
    /// (order modes maker_then_taker). `0` = the core's configured default,
    /// a negative value = never escalate (the leg rests until cancelled or the
    /// round ends).
    #[serde(default)]
    pub maker_timeout_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlaceResult {
    #[serde(rename = "orderId")]
    pub order_id: OrderId,
    pub status: OrderStatus,
    /// Why the KERNEL refused the leg, when it did — the dry/read-only taker the
    /// book could not fill (#180). Same vocabulary a rejected call carries in
    /// `data.coreCode`, so a client branches on one or the other the same way.
    /// Absent for an order the kernel accepted (even one the venue may refuse
    /// later: that arrives on `Event::Error`, not on this result), which keeps
    /// the shape backward compatible for existing consumers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<CoreErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CancelParams {
    #[serde(rename = "orderId")]
    pub order_id: OrderId,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelAllParams {
    #[serde(default)]
    pub token_id: Option<TokenId>,
}
#[derive(Debug, Clone, Serialize)]
pub struct CancelAllResult {
    pub cancelled: usize,
}

/// E31-c: retire the resting remainder of a partially filled order. Same body
/// as a plain cancel — the distinct method exists so a strategy's DECISION
/// ("cancel what is left, keep my filled shares") is visible in ops traces.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelRemainingParams {
    pub order_id: OrderId,
}

/// E31-c: flatten only the shares the fills have already accrued behind an
/// order, leaving its resting remainder alone. The kernel prices, sizes,
/// risk-checks and submits the SELL; the strategy only names the position.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseFilledParams {
    pub order_id: OrderId,
}
#[derive(Debug, Clone, Serialize)]
pub struct CloseFilledResult {
    /// Number of positions the flatten submitted (0 or 1).
    pub closed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct BalanceResult {
    #[serde(with = "crate::decimal")]
    pub balance: Decimal,
    #[serde(with = "crate::decimal")]
    pub reserved: Decimal,
    #[serde(with = "crate::decimal")]
    pub available: Decimal,
    /// Starting principal in DRY mode (`--seed-balance`), so a UI can show the
    /// `principal + realized net − reserved` reconciliation rather than merely
    /// asserting it. `None` in LIVE, where the principal is whatever the venue
    /// reported at start and is not a number the core fixed itself. Absent on
    /// older cores, which the reader must treat as "unknown", not zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<Decimal>,
}

// ── Limited hot reload of the entry limits (#191) ────────────────────────────

/// Params of `risk.setLimits` — the ONLY knobs an operator may change without a
/// restart, and the whole of the safe subset.
///
/// `deny_unknown_fields` is what makes the boundary structural rather than
/// advisory: a field outside this set cannot even deserialize into the type that
/// carries the patch, so no downstream code path can apply one. The named
/// refusals ([`crate::risk::hot_reload_refusal`]) exist on top of it to say WHY
/// for the knobs an operator actually reaches for (breakers, exit thresholds,
/// credentials), instead of a bare "unknown field".
///
/// Every field is optional and every field means "set to this value"; an absent
/// field is left alone. Setting one to `0` is literal, not "disabled" — see each
/// field below.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetRiskLimitsParams {
    /// Absolute cap on ONE order's notional (USD). Read on every order by the
    /// risk gate, so the next order is judged by the new value. `0` binds hard:
    /// every BUY is refused (closing SELLs are exempt by construction, #174).
    #[serde(default, with = "crate::decimal::opt")]
    pub max_order_notional: Option<Decimal>,
    /// The same cap as a percentage of the account's cash equity. `0` = off.
    #[serde(default, with = "crate::decimal::opt")]
    pub max_order_notional_pct: Option<Decimal>,
    /// Cap on TOTAL open notional across all strategies (USD). `0` = off.
    #[serde(default, with = "crate::decimal::opt")]
    pub max_open_notional_usd: Option<Decimal>,
    /// Lower bound of the per-entry share lot.
    #[serde(default, with = "crate::decimal::opt")]
    pub min_shares: Option<Decimal>,
    /// Upper bound of the per-entry share lot.
    #[serde(default, with = "crate::decimal::opt")]
    pub max_shares: Option<Decimal>,
    /// Free-text note from the caller, echoed into the audit line so a log
    /// reader gets the intent beside the numbers.
    #[serde(default)]
    pub reason: Option<String>,
}

impl SetRiskLimitsParams {
    /// True when the patch carries no settable field (a request that would audit
    /// nothing is refused rather than answered with an empty record).
    pub fn is_empty(&self) -> bool {
        self.max_order_notional.is_none()
            && self.max_order_notional_pct.is_none()
            && self.max_open_notional_usd.is_none()
            && self.min_shares.is_none()
            && self.max_shares.is_none()
    }
}

/// One audited field change: old value → new value.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskLimitChange {
    /// The wire (camelCase) name of the field, as the caller spelled it.
    pub field: &'static str,
    #[serde(with = "crate::decimal")]
    pub from: Decimal,
    #[serde(with = "crate::decimal")]
    pub to: Decimal,
}

/// The record an applied `risk.setLimits` returns. The SAME facts go to the run
/// log (one line per changed field, `target: "risk"`), because an audit that
/// only exists in a reply is gone as soon as the caller disconnects.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskLimitUpdate {
    pub applied: Vec<RiskLimitChange>,
    pub at_ms: i64,
    /// Who asked — the peer's uid as recorded by the kernel, not a claim.
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// ALWAYS `false`, and on the wire on purpose: a hot update is memory-only.
    /// A restart re-applies the startup flags/env/TOML, so "I changed it" must
    /// never be read as "it stuck".
    pub persisted: bool,
    /// The same statement in words, for a panel or a log reader.
    pub note: &'static str,
}

/// [`RiskLimitUpdate::note`] — one spelling, used by the reply and the log.
pub const RISK_LIMIT_UPDATE_NOTE: &str =
    "in-memory only: a restart re-applies the startup flags/env/TOML";

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
    /// How many records matched the filters in total — `records` is only the
    /// tail window. Optional in the wire contract (§12.3).
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
