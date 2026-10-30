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
use serde_json::Value;

mod part1;
pub use part1::*;

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
    /// 生效风控可编辑（用户裁决：单笔最大亏损、当日最大回撤等设置选项必须
    /// 都可以被编辑）：九项系统性限额的运行时写路径。六项热生效（每笔入场
    /// 现场判读、只约束新敞口、不携带计数器）；回撤预算与连亏熔断两项按
    /// 「下次会话」语义落盘态生效 —— 不是热旋钮（HOT_RELOAD_REFUSED 记名
    /// 拒绝的理由仍然成立），回执里逐项注明生效时机。
    pub const RISK_SET_SYSTEMIC: &str = "risk.setSystemic";
    /// 退出纪律可编辑（用户裁决：Gate 4 绑定的止损/止盈/强平必须可以改）：
    /// 三项退出纪律的运行时写路径。**live** —— 对下一笔入场立即生效（Gate 4
    /// 每次入场现场从活配置绑定）；已开仓位的退出扫描按 `PositionManager`
    /// 的活配置判读，同样立即跟随新值 —— 回执逐项写明，不静默。
    /// 未持久化：重启回到启动 flag/env/TOML 的解析结果（与 risk.setSystemic
    /// 同一句真话）。
    pub const RISK_SET_EXIT: &str = "risk.setExit";
    /// 轮次时间周期的运行时读数（UI 设置卡）：当前生效秒数、来源（cli/env/
    /// runtime/toml/default）、可切换的白名单档位与「下次会话生效」的回执
    /// 语义。只读。
    pub const ROUND_DURATION_GET: &str = "round_duration.get";
    /// 轮次时间周期的运行时写路径（用户需求：UI 可以把 15m 切到 5m 等）。
    /// **next_session** —— 轮次周期是网格坐标系（`round_slot` = now/dur 的
    /// UTC 网格索引），仓位恢复、#457 过期扫描都用当前 config 重算存量订单
    /// 的过期时间，热切会把旧 slot 全部错解；所以这里只写持久格
    /// `data/runtime/config.json`（重启后启动层读回），本会话的网格不动。
    /// 显式 `--round-sec`（普查/回放命令行） outranks 持久格 —— 逐位复现的
    /// 回放基线永远不被 UI 设置劫持。
    pub const ROUND_DURATION_SET: &str = "round_duration.set";
    /// 行情插件的运行时读数（插件页）：本会话 active 插件、编译进内核的注册表、
    /// 「下次会话将选什么」（持久格 outrank 首个注册者，显式 `--market-plugin`
    /// outrank 持久格）。只读。
    pub const MARKET_PLUGIN_GET: &str = "market.plugin.get";
    /// 行情插件的 next_session 写路径：feed/discovery/executor 生命周期都绑定
    /// 启动选择，热切在架构上不可行 —— 只写持久格 `marketPlugin`
    /// （data/runtime/config.json），重启后启动层读回。plan-first：未注册名
    /// 拒绝（注册表编译在内核里，一个不存在的名字不能等到重启才发现）。
    /// 显式 `--market-plugin` outrank 持久格 —— 回放基线不被 UI 劫持。
    pub const MARKET_PLUGIN_SET: &str = "market.plugin.set";
    /// 行情插件凭据：只读报表（每插件所需 env 变量名 + 下一会话是否齐全，
    /// 值永不过线）与 next_session 写路径（原子写持久格 credentials，
    /// 重启后 boot 注入环境；真实环境变量 outrank 格）。名字按插件需求表
    /// plan-first 校验。
    pub const MARKET_CREDENTIALS_STATUS: &str = "market.credentials.status";
    pub const MARKET_CREDENTIALS_SET: &str = "market.credentials.set";
    pub const ORDER_PLACE: &str = "orders.place";
    pub const ORDER_CANCEL: &str = "orders.cancel";
    pub const ORDER_CANCEL_ALL: &str = "orders.cancel_all";
    /// E31-c: cancel the resting remainder of a partially filled order.
    pub const ORDER_CANCEL_REMAINING: &str = "orders.cancel_remaining";
    /// E31-c: sell only the shares already filled behind an order.
    pub const ORDER_CLOSE_FILLED: &str = "orders.close_filled";
    /// #457: kernel-side force-close of a GHOST order — one whose round has
    /// expired, so it can never fill, yet no honest terminal receipt ever
    /// arrived. Refuses a still-live round (that is `orders.cancel`'s job);
    /// marks `ForceClosed` in the audit trail so "we withdrew" and "we gave
    /// up waiting" stay readable apart.
    pub const ORDER_FORCE_CLOSE: &str = "orders.force_close";
    pub const ORDER_LIST: &str = "orders.list";
    pub const LEDGER_BALANCE: &str = "ledger.balance";
    /// List open positions (enriched with current price / unrealised PnL).
    pub const POSITIONS_LIST: &str = "positions.list";
    /// Force-close a position at market (manual flatten).
    pub const POSITION_EXIT: &str = "positions.exit";
    /// #684: the operator's clear-out verb for a position whose market has
    /// EXPIRED and whose settlement verdict never arrived (the updown round
    /// list only carries future rounds, so an expired condition can never be
    /// answered). Kernel gates: position exists, expiry is genuinely past (a
    /// live round is refused — that is `positions.exit`'s job), and a
    /// manual-ops audit path is configured. Closes at a bookkeeping price
    /// (last known mid, else entry) with NO redemption claim; the audit row
    /// names the operator.
    pub const POSITION_FORCE_CLOSE: &str = "positions.force_close";
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
    /// Issue #481 (G3): one strategy's declared tunables + the values in force.
    /// Read-only; the schema source is the strategy's OWN knob declaration
    /// (manifest tunables for Lua packages), the kernel invents nothing.
    pub const STRATEGY_PARAMS_GET: &str = "strategy.params.get";
    /// Issue #481 (G3): the tunables' operator write path — validate against
    /// the declared domains (plan first, write later), hot-swap the value the
    /// strategy reads on its next evaluate, audit the change. The same
    /// `risk.setExit` posture: refused values move nothing, receipts name
    /// every old→new.
    pub const STRATEGY_PARAMS_SET: &str = "strategy.params.set";
    /// Issue #481 (G4): the confirmed cross-venue mappings as a read-only
    /// snapshot — per event: status, the venues holding a leg, and the
    /// discrepancy marks. The panel's 配对视图 data source.
    pub const UNIFIED_LIST: &str = "unified.list";
    /// ④: the latest round's cross-venue mapping CANDIDATES (auto-matching
    /// proposes, the operator disposes). Read-only: `a`/`b` listings + score.
    pub const UNIFIED_CANDIDATES: &str = "unified.candidates";
    /// ④: the operator confirms a candidate pair as a cross-venue mapping.
    /// Params: `title` (operator-facing event name), `listings` (≥2, distinct
    /// venues — the candidate JSON round-trips verbatim), optional `actor`
    /// (default "panel"). The ledger revalidates everything; a refused
    /// confirm moves nothing.
    pub const UNIFIED_CONFIRM: &str = "unified.confirm";
    /// ④: withdraw a confirmed mapping. Params: `eventId`, optional `actor`.
    pub const UNIFIED_REVOKE: &str = "unified.revoke";
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
    /// Explicitly re-read reload-safe values from `.env`.
    pub const SYSTEM_ENV_RELOAD: &str = "system.env.reload";
    /// Read/write of the update switches (§7.4). The write lands an audit record.
    pub const SYSTEM_UPDATE_CONFIGURE: &str = "system.update.configure";
    /// One manual update check (the UI's "check for updates" button).
    pub const SYSTEM_UPDATE_CHECK: &str = "system.update.check";
    /// #379: download + verify the newer release into the staging directory
    /// (`data/update/staging/`). REPLACEMENT is the launcher's job (§7.5) —
    /// this arm only advances the state machine to "restart-ready", it never
    /// touches a running binary.
    pub const SYSTEM_UPDATE_STAGE: &str = "system.update.stage";
    /// #458: wipe this core's trading state and re-seed — stop enabled
    /// strategies, cancel resting orders, clear the order/trade/position
    /// records and the in-memory books, re-seed the cash to
    /// `dry_seed_balance`. Refused in Live mode (see
    /// `RESET_TRADING_LIVE_REFUSAL`); requires `confirm: "RESET"`. Every
    /// invocation appends a record to the manual-ops audit file.
    pub const SYSTEM_RESET_TRADING: &str = "system.reset_trading";

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
    /// The account↔venue binding readout: one row per configured account
    /// (empty string = unbound). Read-only.
    pub const ACCOUNT_VENUE_GET: &str = "account.venue.get";
    /// The account↔venue binding write (`accountId` + `venue`): non-empty
    /// venue pins the account to that market-plugin name (MUST be registered
    /// — plan-first), the empty string unbinds. The SAME venue on SEVERAL
    /// accounts is the deployment model (one exchange, several API sets, one
    /// account each), so no uniqueness is enforced. Next the binding gates
    /// `orders.place` cross-venue (the existing refuse_cross_venue_order
    /// guard reads the in-memory field), and — forward-looking — it becomes
    /// the next batch's plugin-ENABLEMENT derivation input (boot reads the
    /// account book and enables every venue at least one account is bound
    /// to), which is why the write persists to accounts.toml and must never
    /// lose a sibling account's fields on the way.
    pub const ACCOUNT_VENUE_SET: &str = "account.venue.set";
    /// The account's round-length FAMILY write (`account.rounds.set`,
    /// 轮次多族 B2): the set spells exactly which (venue, duration) families
    /// the account trades. An ACCOUNT-CONFIGURATION MANAGEMENT verb, the same
    /// family as `account.venue.set`/`account.list` — it writes the CONFIG
    /// PLANE's fact (accounts.toml + the in-memory book), its consumers are
    /// the panel's account card and the ops CLI, and it is NOT the strategy
    /// runtime's business-verb family #568 vetoed (策略运行时对周期集的感知
    /// 走内存共享态，不另开业务动词). The write is ONE transaction (persist
    /// FIRST, then memory): a removed family with open positions refuses the
    /// WHOLE write, a removal with no exposure soft-unloads the family's
    /// poll output, an added family mounts its discovery instance at runtime
    /// (增族即挂). Empty set = the key is REMOVED from accounts.toml (the
    /// unrestricted spelling, identical to a missing key at the next boot).
    pub const ACCOUNT_ROUNDS_SET: &str = "account.rounds.set";
    /// The per-account trading switch (`account.trading.set`): `false` =
    /// this account's NEW ENTRIES are refused while its EXITS keep flowing
    /// (positions stay strategy-managed until flat). Persisted to
    /// accounts.toml, re-read at next boot.
    pub const ACCOUNT_TRADING_SET: &str = "account.trading.set";
    /// The per-account FREEZE (account-scoped kill switch,
    /// `account.freeze`): `{accountId, frozen, reason}` — a frozen account
    /// refuses NEW orders (manual AND strategy entries; the engine drops its
    /// candidates) while its EXITS keep flowing. SESSION-scoped: an
    /// automatic freeze (venue-rejection streak) lives in memory only, so
    /// the lift is too — a restart starts clean and re-probes.
    pub const ACCOUNT_FREEZE: &str = "account.freeze";
    /// The per-account strategy ENABLEMENT write (`account.strategies.set`
    /// / the persistence half of the panel's per-account 启用按钮): the
    /// set spells exactly which (strategy, account) scopes run; a removed
    /// strategy with open positions keeps managing them exits-only until
    /// flat.
    pub const ACCOUNT_STRATEGIES_SET: &str = "account.strategies.set";
    /// The DISPLAY pointer write (显示账户配置化): point the snapshot's
    /// `displayAccount`/`displayVenue` at the named account. A pure
    /// presentation pointer — it does NOT touch the active/accounting/
    /// strategy-enablement axes and no trading semantics attach to it.
    /// The kernel validates the account exists AND is `active` (plan-first),
    /// updates the in-memory pointer AND persists the accounts.toml
    /// top-level `display_account` key in one transaction; the next boot
    /// re-reads the file (fail-closed on a dangling id).
    pub const DISPLAY_ACCOUNT_SET: &str = "display.account.set";
    /// One account's cash-journal tail (#509's read surface): the append-only
    /// `JournalEntry` rows the ledger records for every balance change.
    /// Read-only — answers from the in-memory window
    /// (`journal_snapshot()`), never touches the file. `accountId` absent =
    /// the SESSION's default account (the same resolution `account.list`
    /// uses); `limit` defaults to 100, capped at 500.
    pub const ACCOUNT_JOURNAL: &str = "account.journal";
    /// Create ONE account at runtime (`accountId` + optional `name`/`venue`):
    /// the identity lands in the in-memory book AND accounts.toml — the same
    /// plan-and-persist transaction `account.venue.set` runs. Status is
    /// ALWAYS `active` (a runtime call that minted a restricted account would
    /// be a loosening in disguise — tightening is `account.status`'s job);
    /// market_type is ALWAYS `prediction` (the only type a 0.3 plugin
    /// exists for, identical to the file's own default). NO credential field
    /// travels here (§9.4: the value never enters the kernel) — credentials
    /// are the existing `market.credentials.set` env-name path, wired by the
    /// panel after the identity exists. `default` is refused (reserved).
    pub const ACCOUNT_CREATE: &str = "account.create";
    /// Remove ONE account at runtime: the row leaves the in-memory book AND
    /// accounts.toml. `default` is refused (reserved); an account with any
    /// OPEN position is refused (its money must not become unaddressable);
    /// removing the process-active account re-activates `default` (the
    /// session switchers resolve against the book, this only fixes the
    /// process-level cell).
    pub const ACCOUNT_REMOVE: &str = "account.remove";

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
    /// #393: read one saved package's blueprint document back (the editor's
    /// preload path — the read-only mirror of `blueprint.save`).
    pub const BLUEPRINT_LOAD: &str = "blueprint.load";
    /// 手写包的「在蓝图中打开」（用户裁决：编辑入口必须能打开所有 Lua 包）：
    /// 把手写包的 Lua 源码读回来（带 manifest 元数据），编辑器的源码模式
    /// 展示并允许原样改写落盘 —— 画布图是蓝图编译产物的专属表示，不编造。
    pub const BLUEPRINT_LOAD_SOURCE: &str = "blueprint.loadSource";
    /// `blueprint.loadSource` 的写回镜像：把手写包的 `strategy.lua` 原样写
    /// 回（sha256 重算进 manifest，装载器照常验证）。`overwrite` 显式，与
    /// `blueprint.save` 同一裁决 —— 静默覆盖可能换掉操作员正在启用的策略。
    pub const BLUEPRINT_SAVE_SOURCE: &str = "blueprint.saveSource";

    // ── 跨所套利弹窗 v2 — the arb leg-configuration surface ──────────────────
    /// Write the cross-venue arbitrage leg configuration (`arb.config.set`):
    /// two DISTINCT venues (`legAVenue`/`legBVenue`), the account each venue
    /// trades under (`legAAccount`/`legBAccount`), and the asset `pairs` the
    /// strategy pairs across them. Fail-closed validation names the offending
    /// field (version, venue/account presence, venue distinctness, account
    /// existence, account venue-binding coverage, pairs non-empty); a refused
    /// write moves NOTHING (file and memory stay untouched). The kernel
    /// persists the section to accounts.toml top-level `[cross_venue_arb]` in
    /// the same transaction the memory mirror + engine push update — the next
    /// boot re-reads the file (fail-closed on a bad hand edit).
    pub const ARB_CONFIG_SET: &str = "arb.config.set";
    /// The leg configuration readout (`arb.config.get`): `configured: true`
    /// with the four leg fields + `pairs`, or `configured: false` with null
    /// legs and an empty array — never a partial shape.
    pub const ARB_CONFIG_GET: &str = "arb.config.get";
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
    /// Names of the evolvable knobs the manifest exports (`tunables`) — empty
    /// when the blueprint has no numeric anchors. The editor shows this count
    /// so the operator knows the saved strategy is evolution-ready.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tunables: Vec<String>,
}

/// `blueprint.load` params (#393): `{ "name": "<package name>" }` — the
/// strategy whose blueprint document the editor wants back.
#[derive(Debug, Clone, Deserialize)]
pub struct BlueprintLoadParams {
    pub name: String,
}

/// `blueprint.load` result: the package's blueprint document, verbatim, plus
/// its RELATIVE path (same convention as the save receipt). Read-only — it
/// answers from the strategy root the kernel already scans, so the editor can
/// preload a package without the UI ever touching the filesystem.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlueprintLoadResult {
    pub name: String,
    pub blueprint_path: String,
    pub json: String,
}

/// `blueprint.loadSource` params: same shape as `blueprint.load` — the
/// hand-written package whose Lua source the editor's source mode wants back.
#[derive(Debug, Clone, Deserialize)]
pub struct BlueprintLoadSourceParams {
    pub name: String,
}

/// `blueprint.loadSource` result: the package's entry file verbatim + the
/// manifest fields the editor needs to re-seal a write-back. `manifestPath`
/// is relative (the save-receipt convention). `modes`/`author`/`description`
/// travel as raw JSON so the editor can round-trip them untouched.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlueprintLoadSourceResult {
    pub name: String,
    pub package_dir: String,
    pub lua_path: String,
    pub manifest_path: String,
    pub lua: String,
    /// The manifest's `sha256` of the CURRENT entry file, hex — the editor
    /// shows it and the write-back re-derives it from the new bytes.
    pub sha256: String,
    pub manifest: Value,
}

/// `blueprint.saveSource` params: the package to overwrite (must already
/// exist — this verb EDITS, it does not create; creation is a blueprint save)
/// and the new entry-file body. `overwrite` must be explicit, matching
/// `blueprint.save`.
#[derive(Debug, Clone, Deserialize)]
pub struct BlueprintSaveSourceParams {
    pub name: String,
    pub lua: String,
    #[serde(default)]
    pub overwrite: bool,
}

/// `blueprint.saveSource` result: what landed, in the save-receipt shape
/// (relative paths + the NEW sha256 the manifest now carries + byte counts).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlueprintSaveSourceResult {
    pub name: String,
    pub package_dir: String,
    pub lua_path: String,
    pub manifest_path: String,
    pub lua_sha256: String,
    pub bytes: [u64; 2],
}

// ── E26 (§4.4) — systemic risk readout ──────────────────────────────────────

/// The whole `risk.limits` readout: WHAT each systemic limit is and WHERE it
/// came from, plus the exit resolution the entry pipeline binds at Gate 4.
/// Assembled per answer from the LIVE risk config — `risk.setLimits` and
/// `risk.setSystemic` hot-write the matrix, so a boot-frozen snapshot would
/// answer with limits that are no longer in force (the arm takes the Core
/// lock and reads `risk_config()` for exactly this reason).
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
    /// The live readout: systemic limits + exit resolution exactly as they
    /// stand in the risk gate RIGHT NOW (hot edits included).
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
                trailing_enabled: exit.trailing_enabled,
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
    /// Trailing-stop arming switch, same live semantics as the triple
    /// (hot-writable via `risk.setExit`). Incremental field — old panels
    /// ignore it.
    pub trailing_enabled: bool,
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

/// #457: force-close a ghost order. Same body as a plain cancel — the distinct
/// method exists so the audit trail reads "the operator declared this order
/// dead", not "the operator withdrew a live order".
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForceCloseParams {
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

// ── 生效风控可编辑 — risk.setSystemic（九项系统性限额的运行时写路径）─────────

/// `risk.setSystemic` params: the nine systemic limits, every field optional
/// (only the named fields change). Values are USD / shares / counts / minutes
/// on the decimal-STRING wire (the readout's convention); `0` = off.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SetSystemicLimitsParams {
    // ── 账户级（五项）──
    /// 单笔最大亏损（USD）：Gate 2 从止损价推导，超额入场缩到帽内。
    #[serde(default, with = "crate::decimal::opt")]
    pub max_single_loss_usd: Option<Decimal>,
    /// 当日最大回撤（USD）：与既有的日亏熔断是同一个预算。**下次会话生效**
    /// —— 当日已发生的已实现亏损账本不能被一个中途改动的帽追溯改判。
    #[serde(default, with = "crate::decimal::opt")]
    pub max_daily_drawdown_usd: Option<Decimal>,
    /// 单仓上限（股）：整数股，超出缩到帽内。
    #[serde(default, with = "crate::decimal::opt")]
    pub max_position_size: Option<Decimal>,
    /// 连亏熔断（次）：**下次会话生效** —— 计数器已在跑，新阈值会追溯改判
    /// 在案连亏是否熔断（HOT_RELOAD_REFUSED 的理由）。
    #[serde(default, with = "crate::decimal::opt")]
    pub max_consecutive_losses: Option<Decimal>,
    /// 熔断冷静期（分钟）：**下次会话生效** —— 与连亏熔断同一裁决。
    #[serde(default, with = "crate::decimal::opt")]
    pub cooldown_minutes: Option<Decimal>,
    // ── 全局级（四项）──
    /// 全局总仓位（笔，全账户合计）。
    #[serde(default, with = "crate::decimal::opt")]
    pub max_total_position: Option<Decimal>,
    /// 全局总敞口（USD，全账户开仓名义合计）。
    #[serde(default, with = "crate::decimal::opt")]
    pub max_total_exposure_usd: Option<Decimal>,
    /// 同资产敞口上限（USD，逐资产取最大）。
    #[serde(default, with = "crate::decimal::opt")]
    pub max_correlation_usd: Option<Decimal>,
    /// 全局急停亏损（USD）：当日全账户合计已实现亏损越过即拒新开仓。
    #[serde(default, with = "crate::decimal::opt")]
    pub global_kill_switch_loss_usd: Option<Decimal>,
    /// Free-text note from the caller, echoed into the audit line.
    #[serde(default)]
    pub reason: Option<String>,
}

impl SetSystemicLimitsParams {
    /// True when the patch names no field — refused, not answered with an
    /// empty record (the same posture as `risk.setLimits`).
    pub fn is_empty(&self) -> bool {
        self.max_single_loss_usd.is_none()
            && self.max_daily_drawdown_usd.is_none()
            && self.max_position_size.is_none()
            && self.max_consecutive_losses.is_none()
            && self.cooldown_minutes.is_none()
            && self.max_total_position.is_none()
            && self.max_total_exposure_usd.is_none()
            && self.max_correlation_usd.is_none()
            && self.global_kill_switch_loss_usd.is_none()
    }
}

/// One field's resolution inside a `risk.setSystemic` reply: old value, new
/// value, and WHEN it takes effect. The three effect classes are real, not
/// documentation: six bounds are judged per entry LIVE; the drawdown budget
/// and the breaker pair arm at the NEXT KERNEL SESSION (a live change would
/// either rewrite what the day already spent or retroactively judge a running
/// streak).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemicLimitChange {
    /// The wire (camelCase) name of the field.
    pub field: &'static str,
    /// Previous effective value ("0" = off), decimal string.
    pub from: String,
    /// New effective value ("0" = off), decimal string.
    pub to: String,
    /// `live` (judged on the next entry) or `next_session` (armed at boot of
    /// a future kernel run).
    pub effect: &'static str,
}

/// `risk.setSystemic` result: the audited change list, in the
/// `RiskLimitUpdate` shape. `persisted` is ALWAYS false — the systemic set is
/// a boot-time resolution ([`SystemicRiskLimits::from_file`]); a hot edit
/// lives exactly as long as this process, and the note says so in the same
/// breath as the drawdown/breaker "next session" rows.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemicLimitUpdate {
    pub applied: Vec<SystemicLimitChange>,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub persisted: bool,
    pub note: &'static str,
}

/// [`SystemicLimitUpdate::note`] — one spelling, used by the reply and the log.
pub const SYSTEMIC_LIMIT_UPDATE_NOTE: &str = "six bounds live, drawdown + breaker pair arm next session; a restart \
     re-applies the startup flags/env/TOML (in-memory, not persisted)";

// ── 退出纪律可编辑 — risk.setExit（止损/止盈/强平的运行时写路径）────────────

/// `risk.setExit` params: the exit triple Gate 4 binds per entry, every field
/// optional (only the named fields change). Percentages are decimal STRINGS on
/// the wire (the readout's convention); `forceExitSec` is seconds.
///
/// Calibration guard: `forceExitSec: 0` DISABLES the guillotine and
/// `stopLossPct: 0` would stop on entry tick, so the percentages are refused
/// at `<= 0` while the deadline accepts `0` as the documented off switch —
/// the same posture the ExitConfig default documents (`force_exit_sec 0 = the
/// guillotine is OFF`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SetExitParams {
    /// 止损（入场价百分比）：跌破即触发保护性离场。出厂 12。
    #[serde(default, with = "crate::decimal::opt")]
    pub stop_loss_pct: Option<Decimal>,
    /// 止盈（入场价百分比）：兜底入袋规则。出厂 100（近确定二元的胜利入袋）。
    #[serde(default, with = "crate::decimal::opt")]
    pub take_profit_pct: Option<Decimal>,
    /// 强平秒数（轮剩余时间降到该值即硬性离场）。**0 = 关闭强平**（出厂
    /// 校准值：旧的 120s 截止切掉了后来赔付的轮）。负值拒绝。
    #[serde(default)]
    pub force_exit_sec: Option<i64>,
    /// 移动止盈总开关（trailing stop 是否武装）。**出厂 false** —— 10-02
    /// #356 校准裁决（「操盘手从不卖进利润，全部持有到期 REDEEM」），但
    /// spread_arb 一类「入场→坐山车→止损」的价差策略正是它该管的形态：
    /// 单边持仓到期特征暴露了旧校准的局限，故升格为热配项。true/false
    /// 都是合法值（bool 没有「非法值」可拒）。
    #[serde(default)]
    pub trailing_enabled: Option<bool>,
    /// Free-text note from the caller, echoed into the audit line.
    #[serde(default)]
    pub reason: Option<String>,
}

impl SetExitParams {
    /// True when the patch names no field — refused, not answered with an
    /// empty record (the same posture as `risk.setSystemic`).
    pub fn is_empty(&self) -> bool {
        self.stop_loss_pct.is_none()
            && self.take_profit_pct.is_none()
            && self.force_exit_sec.is_none()
            && self.trailing_enabled.is_none()
    }
}

/// One field's resolution inside a `risk.setExit` reply. All three fields are
/// **live** for every NEW entry (Gate 4 binds from the live config each
/// intent) and the exit sweep reads the same live config, so a change also
/// steers already-open positions' protective rules — the receipt says so.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitSetChange {
    /// The wire (camelCase) name of the field.
    pub field: &'static str,
    /// Previous effective value, decimal string.
    pub from: String,
    /// New effective value, decimal string.
    pub to: String,
    /// Always `live` today: every exit rule is judged per tick from the live
    /// config. A separate class exists so a future next-session knob does not
    /// reuse the string silently.
    pub effect: &'static str,
}

/// `risk.setExit` result: the audited change list, in the
/// `SystemicLimitUpdate` shape. `persisted` is ALWAYS false — the exit triple
/// is a boot-time resolution (flags/env/TOML); a hot edit lives exactly as
/// long as this process, and the note says so.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitSetUpdate {
    pub applied: Vec<ExitSetChange>,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub persisted: bool,
    pub note: &'static str,
}

/// [`ExitSetUpdate::note`] — one spelling, used by the reply and the log.
pub const EXIT_SET_UPDATE_NOTE: &str = "live for every new entry (Gate 4 binds per intent) and for the exit \
     sweep of already-open positions; a restart re-applies the startup flags/env/TOML (in-memory, not persisted)";

// ── 策略 tunables 编辑 — strategy.params.get / strategy.params.set (issue 481) ──

/// `strategy.params.get` params: `{ "name": "<strategy>" }`.
#[derive(Debug, Clone, Deserialize)]
pub struct StrategyParamsGetParams {
    pub name: String,
}

/// One declared tunable as the wire sees it: the strategy's OWN declaration
/// (name + the admissible `[min, max]` domain), the value in force, and the
/// type tag the manifest carries (`decimal` / `int` — an `int` knob's set
/// value must be a whole number). All numbers cross as decimal STRINGS.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TunableView {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<&'static str>,
    /// Current value in force (registry cell or manifest default), decimal string.
    pub value: String,
    pub min: String,
    pub max: String,
}

/// `strategy.params.get` result: the strategy's declared tunables plus the
/// effective mode. `paramsAttached` is true when the registry cell the write
/// path publishes to is also what the strategy READS this session (evolution
/// on, or an operator write has pinned the overlay); `false` means the write
/// path still works but the value takes the stage only via the attach below
/// the kernel performs on the next set — never a silent divergence.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyParamsReadout {
    pub name: String,
    pub found: bool,
    /// Empty for a strategy that declared no tunables (not evolvable) — the
    /// editor renders "该策略没有可调参数" rather than an empty table.
    pub tunables: Vec<TunableView>,
    pub evolvable: bool,
    pub params_attached: bool,
    /// Session-level note for the operator (why a set would/would not stick).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `strategy.params.set` params: one strategy, the knobs to move (decimal
/// strings — the shared wire rule), and the audit note. Multi-knob patches
/// are atomic: one bad value refuses the WHOLE patch.
#[derive(Debug, Clone, Deserialize)]
pub struct StrategyParamsSetParams {
    pub name: String,
    /// knob name → new value (decimal string or JSON number; both parse
    /// exactly, no float path).
    pub params: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// One knob's audited change inside a `strategy.params.set` reply. All
/// tunables are `live`: the strategy merges its registry cell over the
/// manifest defaults at every evaluate, so the next cycle reads the new
/// value — no restart, no per-position binding to name.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyParamChange {
    pub knob: String,
    pub from: String,
    pub to: String,
    pub effect: &'static str,
}

/// `strategy.params.set` result: the audited change list (the
/// `risk.setExit` receipt shape, so the panel's edit cards read alike).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyParamsSetUpdate {
    pub name: String,
    pub applied: Vec<StrategyParamChange>,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Always false today: the write lands in the in-memory registry cell.
    /// Restarts re-resolve from the manifest defaults (+ any adopted
    /// evolution promotions), which the note says out loud.
    pub persisted: bool,
    pub note: &'static str,
}

/// [`StrategyParamsSetUpdate::note`].
pub const STRATEGY_PARAMS_SET_NOTE: &str = "live from the strategy's next evaluation cycle (the registry cell merges over \
     the manifest defaults); a restart re-resolves from the manifest defaults and any adopted \
     evolution promotions (in-memory, not persisted)";

// ── 轮次时间周期可设置 — round_duration.get / round_duration.set ────────────

/// `round_duration.get` result: the effective round length, where it came
/// from, the switchable whitelist, and what a restart would resolve to (the
/// persisted cell outranks TOML/default but not an explicit CLI flag).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundDurationResult {
    /// The effective seconds this session runs on.
    pub effective_sec: i64,
    /// One of `cli` / `env` / `runtime` / `toml` / `default` — the boot
    /// resolution's own report, so the UI can say "an explicit flag outranks
    /// this card" instead of silently ignoring a write.
    pub source: &'static str,
    /// The switchable whitelist (labels come from the plugins' declared
    /// `round_labels`, aggregated at boot — issue 648). Empty `label` rows
    /// are still listed so the UI can name a non-standard boot value.
    pub allowed: Vec<RoundDurationOption>,
    /// What the NEXT session will run on: the persisted cell if one was
    /// written (regardless of this session's source), else the effective
    /// value. The UI reads this to prefill the card.
    pub next_session_sec: i64,
    /// Whether the persisted cell holds a value (a write happened at some
    /// point; `round_duration.set` with `reset` clears it).
    pub persisted: bool,
}

/// One row of the whitelist.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundDurationOption {
    pub sec: i64,
    /// `5m` / `15m` / `1h` / `4h` / `daily` — null for a non-standard value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<&'static str>,
}

/// `round_duration.set` params: `durationSec` must be one of the whitelist
/// values (300/900/3600/14400/86400); `reset` instead clears the persisted
/// cell so the next session falls back to the boot flags/env/TOML. Exactly
/// one of the two must be given.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRoundDurationParams {
    #[serde(default)]
    pub duration_sec: Option<i64>,
    #[serde(default)]
    pub reset: bool,
    /// Audit trail only — echoed in the reply and the log line.
    pub reason: Option<String>,
}

/// `round_duration.set` result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundDurationUpdate {
    /// What the next session will run on after this write.
    pub next_session_sec: i64,
    /// `reset` clears the cell (falls back to boot resolution) instead of
    /// writing a value.
    pub reset: bool,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: &'static str,
}

/// [`RoundDurationUpdate::note`] — one spelling, used by the reply and the log.
pub const ROUND_DURATION_SET_NOTE: &str = "next session: the round grid is a coordinate system (round_slot is a UTC \
     grid index) and live positions/orders were slotted under the old length; \
     an explicit --round-sec flag still outranks this setting";

// ── ④ 统一映射：unified.candidates / unified.confirm / unified.revoke ─────

/// `unified.confirm` params. `listings` is the candidate pair verbatim (the
/// `unified.candidates` rows round-trip: `PlatformListing` is serde both
/// ways); the ledger revalidates everything on its side.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedConfirmParams {
    pub title: String,
    pub listings: Vec<blitzkrieg_market_api::unified::PlatformListing>,
    /// Audit trail only; defaults to "panel".
    #[serde(default)]
    pub actor: Option<String>,
}

/// `unified.revoke` params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedRevokeParams {
    pub event_id: String,
    #[serde(default)]
    pub actor: Option<String>,
}

// ── 行情插件切换（next_session）— market.plugin.get / market.plugin.set ─────

/// `market.plugin.get` result: the running session's active plugin, the
/// compiled-in registry, and what a restart would select (the persisted
/// `marketPlugin` cell outranks first-registered but not an explicit
/// `--market-plugin` flag).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPluginReadout {
    /// The plugin driving THIS session (the registry's active name).
    pub active_plugin: String,
    /// Every compiled-in plugin in registration order — the switchable set.
    pub registered: Vec<MarketPluginOption>,
    /// What the NEXT session will select: the persisted cell if one holds a
    /// REGISTERED name, else this session's active plugin. A cell naming an
    /// unregistered plugin is reported but NOT armed (the boot refuses it the
    /// same way), so the next session never silently differs.
    pub next_session_plugin: String,
    /// Whether the persisted cell holds a registered name (a write happened
    /// at some point; `market.plugin.set` with `reset` clears it).
    pub persisted: bool,
    /// True when an explicit `--market-plugin` flag resolved this session —
    /// the UI must say a write cannot move a flag-pinned session.
    pub flag_pinned: bool,
    /// Where THIS session's enablement came from — the boot's four-layer
    /// resolution, disclosed: `flag`（显式 --market-plugin）、`accounts`
    /// （绑定即启用：从账户 venue 绑定派生 —— 本层生效时 market.plugin.set
    /// 被拒）、`persisted`（旧 market.plugin.set 持久格，仅在无绑定时生效）、
    /// `fallback`（全空 —— 回落首个注册者）。
    pub source: &'static str,
    /// The derived venue set (deduped non-empty `Account.venue` bindings) the
    /// accounts layer produced — empty whenever `source` is not `accounts`.
    /// Provenance only, never a decision input.
    pub derived_venues: Vec<String>,
}

/// One row of the registered-plugin list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPluginOption {
    pub name: String,
    /// The plugin's market class (`prediction` / `spot` / …).
    #[serde(rename = "type")]
    pub kind: String,
    /// True when this plugin is driving the current session.
    pub active: bool,
}

/// `market.plugin.set` params: `name` must name a REGISTERED plugin (the
/// registry is compiled in — an unregistered name is refused here, plan-first,
/// not discovered as a broken boot later); `reset` instead clears the
/// persisted cell so the next session falls back to `--market-plugin`/
/// first-registered. Exactly one of the two must be given.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetMarketPluginParams {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub reset: bool,
    /// Audit trail only — echoed in the reply and the log line.
    pub reason: Option<String>,
}

/// `market.plugin.set` result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPluginUpdate {
    /// What the next session will select after this write.
    pub next_session_plugin: String,
    /// `reset` clears the cell (falls back to boot resolution) instead of
    /// writing a value.
    pub reset: bool,
    pub at_ms: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: &'static str,
}

/// [`MarketPluginUpdate::note`] — one spelling, used by the reply and the log.
pub const MARKET_PLUGIN_SET_NOTE: &str = "next session: the data feed, discovery and executor are all built around \
     the plugin chosen at boot, so a switch is a restart; an explicit \
     --market-plugin flag still outranks this setting";
