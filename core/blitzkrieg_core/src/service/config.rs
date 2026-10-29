//! Core configuration column, moved verbatim from `service.rs` (pure code
//! motion, no logic edits): the config surface (`CoreConfig`,
//! `StrategyLimit`, `VirtualAccountLimit`, `ShadowEvolutionTuning`, its
//! `Default`), the fee-quote readouts, the durable-log records
//! (`AppliedFillLog`, `AuditRuntime`), the error/placement records
//! (`LastError`, `PlaceOutcome`) and the replay bookkeeping types
//! (`InflightTaker`) with their tuning constants.
//!
//! `service.rs` re-exports the public surface with `pub use config::*;`, so
//! every path (`crate::service::CoreConfig`, ...) resolves exactly as before
//! the split. Items that were file-private in `service.rs` keep their old
//! visibility from the service module's point of view via `pub(super)`.

use super::Core;
use crate::model::*;
use crate::ome::AppliedFillRecord;
use crate::position::PositionConfig;
use crate::reconcile::{AuditReport, CashIdentity};
use crate::risk::RiskConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct CoreConfig {
    pub mode: Mode,
    pub default_maker_timeout_ms: i64,
    /// Ticks a taker exit's limit is set below the executable bid so the FOK
    /// stays marketable in thin books (the venue still fills at the bid or
    /// better). 0 = price exactly at the executable bid.
    pub exit_taker_slip_ticks: i64,
    pub risk: RiskConfig,
    /// Starting simulated cash balance in DRY mode.
    pub dry_seed_balance: Decimal,
    /// Condition ids to subscribe on the live user channel (empty = REST only).
    pub markets: Vec<String>,
    /// Position sizing/exit/cooldown config (Rust owns positions in P2).
    pub positions: PositionConfig,
    /// Consecutive-loss breaker threshold and cooldown.
    pub max_consecutive_losses: u32,
    pub breaker_cooldown_sec: i64,
    /// Round length used to derive a position's expiry from its round slot.
    pub round_duration_sec: i64,
    /// The round lengths switchable this session: the DECLARED UNION of the
    /// registered market plugins' `supported_round_secs` (`MarketPluginRegistry
    /// ::allowed_round_secs`), computed once at boot — which durations may be
    /// armed is venue knowledge, so the kernel keeps no hardcoded list; this
    /// field only mirrors what the plugins declared. EVERY consumer
    /// (`round_duration.get` readout, `round_duration.set`, the persisted-cell
    /// reader, the accounts.toml parser, `account.rounds.set`) admits a value
    /// only when this union contains it, and an EMPTY union refuses EVERY
    /// value (fail-closed: with no plugin claiming a family, accepting any
    /// value would be the kernel inventing knowledge the declaration face
    /// never made — not "allow everything").
    pub allowed_round_secs: Vec<i64>,
    /// The labels the registered market plugins declare for their round
    /// families (`MarketPluginRegistry::round_labels`, issue 648), computed
    /// once at boot — `sec → label` as the plugins named their own Gamma
    /// slug families. The kernel holds no hardcoded label table of its own:
    /// `round_duration.get` renders exactly the labels the plugins claimed,
    /// and a duration missing here reads unlabeled.
    pub round_labels: std::collections::BTreeMap<i64, &'static str>,
    /// Where the effective round length came from — one of `cli` / `env` /
    /// `runtime` / `toml` / `default`. The boot resolution owns the spelling;
    /// `round_duration.get` surfaces it so the UI can say "an explicit flag
    /// outranks this card". Not a decision input anywhere.
    pub round_duration_source: &'static str,
    /// The round-duration UI card's durable cell (`data/runtime/config.json`
    /// in production). `None` disables the write path — the backtester and
    /// most in-process tests run with `None`, and a `round_duration.set` then
    /// refuses with "persist is disabled" instead of writing into whatever
    /// checkout the caller happens to run from (the same posture as
    /// `strategy_state_path`).
    pub runtime_config_path: Option<String>,
    /// The `.env` file the launcher loads at start (`<cwd>/.env` in
    /// production) — the write half of `market.credentials.set` upserts
    /// validated credential keys HERE (the single persistent home for
    /// UI-submitted credentials; the config.json cell is retired). `None`
    /// disables the write path — the backtester and most in-process tests
    /// run with `None`, and a `market.credentials.set` then refuses with
    /// "persist is disabled" (the same posture as `runtime_config_path`).
    /// The READ side never consults this field: the kernel's own environment
    /// already carries the launcher's `.env` load, so every predicate reads
    /// `std::env` directly.
    pub env_file_path: Option<String>,
    /// Every registered plugin's DECLARED credential env names (issue 648,
    /// `plugin → [env var name]` from `MarketPlugin::required_credential_env`),
    /// computed once at boot from the registry — the same report posture as
    /// `allowed_round_secs`. The credential readout, the `market.credentials.set`
    /// validation and the RFC #541 ready predicates all read it from here; the
    /// kernel holds no hardcoded venue→env table of its own. Empty map = no
    /// credential-bearing plugin is compiled in.
    pub credential_vars: std::collections::BTreeMap<String, Vec<String>>,
    /// The account book's own file (`user_layer/configs/accounts.toml` in
    /// production) — the write half of `account.venue.set` persists the
    /// binding HERE, read back at the next boot. `None` disables the write
    /// path: the backtester and most in-process tests run with `None`, and a
    /// venue bind then refuses with "persistence is disabled" instead of
    /// writing into whatever checkout the caller happens to run from (the
    /// same posture as `runtime_config_path`, which this mirrors). The READ
    /// half never consults this — `Core::new` loads the book from the
    /// cwd-relative `ACCOUNTS_CONFIG_PATH` at boot, tests inject a book via
    /// [`Core::with_account_book`]; only tests that exercise the persist
    /// round-trip set this to their own private file.
    pub accounts_config_path: Option<String>,
    /// #458: where manual, destructive operations (`system.reset_trading`,
    /// and later `orders.force_close`) append their audit record. `None`
    /// (the backtester, most in-process tests) means a reset REFUSES rather
    /// than wipe records with no audit trail — the manual-ops line is the
    /// only trace a reset leaves. Production resolves this to
    /// `data/audit/manual-ops.jsonl` in `main.rs`, like `runtime_config_path`.
    pub manual_ops_audit_path: Option<String>,
    /// #481/④: where the cross-venue mapping ledger persists
    /// (`data/runtime/unified-mapping.json` in production). `None` keeps the
    /// ledger in memory — the backtester and most in-process tests never
    /// confirm a mapping, so there is nothing to persist.
    pub unified_ledger_path: Option<String>,
    /// When false the core never auto-submits exits on tick; positions are only
    /// closed by explicit fills/flatten. Used by order-layer tests and for a
    /// "positions managed externally" deployment. Default true.
    pub auto_exits_enabled: bool,
    /// Instantiate the self-driving market-data engine (signal → order). Default
    /// false so the P0/P1/P2 bridges keep working until the feed layer is wired.
    pub engine_enabled: bool,
    /// Assets the engine trades and its sizing/round timing (mirrors the TS config).
    pub assets: Vec<String>,
    /// Strategies to switch ON after the engine is installed, by name (E4-a).
    /// The kernel ships ZERO enabled strategies — what starts enabled is the
    /// operator's persisted intent (`strategy_state_path`) plus explicit
    /// `--enable-strategy` flags, so adding a strategy can never change what an
    /// existing session trades. Unknown names are reported and ignored, never
    /// fatal.
    pub enabled_strategies: Vec<String>,
    /// Strategies to switch OFF after the engine is installed, by name. Applied
    /// after `enabled_strategies`, so the explicit "off" wins if both name the
    /// same strategy.
    pub disabled_strategies: Vec<String>,
    /// Directory scanned at startup for user-layer strategy libraries
    /// (`*.dylib`/`*.so`): every library found is dlopen'd and REGISTERED —
    /// registration never enables; what trades is decided solely by
    /// `enabled_strategies` (persisted intent + CLI flags). `None` = no
    /// directory (auto-load off). Load failures are reported and skipped,
    /// never fatal — a broken file must not brick the kernel.
    pub strategy_dir: Option<String>,
    /// E30 (§6.4): directory scanned at startup for Lua strategy PACKAGES
    /// (directories carrying `manifest.json`). Same discipline as
    /// `strategy_dir`: every valid package LOADS and registers DISABLED; the
    /// enable decision comes from the persisted set plus `--enable-strategy`.
    /// Deliberately SEPARATE from `strategy_dir` — that path is the dylib
    /// scanner's root, and Lua packages are never dylibs (§6.4: mixing them
    /// in would blur the measurement fixtures' semantics). `None` = off;
    /// default `user_layer/strategies_lua` under the repo.
    pub lua_strategy_dir: Option<String>,
    /// Where the effective enabled-set is recorded so runtime toggles and boot
    /// flags survive a restart (`data/strategy-state.json` in production; see
    /// [`crate::strategy_state`]). `None` disables persistence — the backtester
    /// and most in-process tests run with `None`, and a toggle then changes
    /// nothing on disk.
    pub strategy_state_path: Option<String>,
    /// The strategies the OPERATOR named on the command line
    /// (`--enable-strategy`), kept apart from `enabled_strategies` because the
    /// startup self-check is about an EXPLICIT request, not about what a
    /// persisted state file replays (#265).
    ///
    /// Empty for every in-process core and for the backtester, which is why the
    /// refusal below cannot surprise a caller that never asked for a strategy by
    /// name. The two sets are related by construction: `main` builds
    /// `enabled_strategies` as the persisted set plus these names.
    pub requested_strategies: Vec<String>,
    /// #265 — start even when none of the requested strategies resolved. The
    /// default is to REFUSE: an explicit request that loads nothing used to be
    /// one `WARN` line followed by a healthy-looking kernel that had no strategy
    /// at all (exit 0, socket listening, health probe green). `--allow-zero-strategies`
    /// is the acknowledged escape hatch for a harness that wants that boot.
    pub allow_zero_strategies: bool,
    pub min_round_age_sec: i64,
    pub size_usd: Decimal,
    pub min_shares: Decimal,
    pub max_shares: Decimal,
    /// P0 #202 — the per-entry budget as a percentage of the account's cash
    /// equity, replacing the absolute `size_usd` when > 0. 0 (the default)
    /// keeps every deployment's current order sizes exactly as they are.
    pub size_pct: Decimal,
    /// Engine trend confirmation window (sec) and floor (ms) — exposed so tests
    /// and ops can shorten the confirmation window without touching code.
    pub trend_confirm_sec: i64,
    pub trend_window_floor_ms: i64,
    /// spread_arb entry knobs (E15): `None` keeps the shipped default in
    /// [`crate::signal::SpreadArbConfig`]; a value overrides it in
    /// `engine_config` — the one mapping that drives both the live server and
    /// the backtester, so a sweep varies candidates purely through config.
    pub spread_arb_trend_entry_factor: Option<Decimal>,
    pub spread_arb_entry_min_obi: Option<Decimal>,
    pub spread_arb_entry_max_spread_pct: Option<Decimal>,
    pub spread_arb_entry_dip_max_pct: Option<Decimal>,
    pub spread_arb_entry_bounce_min_pct: Option<Decimal>,
    /// Turn-filter lookback window (sec); `None` = default 5.
    pub spread_arb_entry_bounce_window_sec: Option<i64>,
    /// Start Rust-native WS feeds (P4): Polymarket orderbook + Binance spot.
    /// When enabled Node no longer has to push `books.*` / `spot.price`.
    pub feed_ws_enabled: bool,
    /// Select the operative market plugin by name (matches `MarketPlugin::name`,
    /// e.g. "polymarket"). None = first registered. Ignored when the name is not
    /// registered (falls back to the first) so a stray flag never bricks startup.
    pub market_plugin: Option<String>,
    /// Binance stream symbols (e.g. ["BTC","ETH"]); empty disables spot.
    pub binance_assets: Vec<String>,
    /// Where to append near-miss records (blocked signals + path). None = off.
    pub near_miss_path: Option<String>,
    /// #363: where the account-isolated execution policy loads from. The
    /// binary entry (`main.rs`, #380) resolves the SHIPPED default against the
    /// install root (the executable's ancestors, then the cwd's) and stores
    /// the outcome here — the library reads this path VERBATIM, so which cwd
    /// the process started in can no longer flip "guarded kernel" to "neutral
    /// kernel" (#380's defect). Embedders and tests pass an explicit path (or
    /// `None` to load nothing — the backtester and most in-process tests,
    /// which is the same as the zero-config deployment: code defaults,
    /// hardcoded-behaviour parity). A file that exists but is malformed
    /// refuses the boot (fail-closed) with a `load-refused` audit line — the
    /// kernel never trades under a policy the operator did not write.
    pub execution_policy_path: Option<String>,
    /// #386: where the execution-policy audit journal appends and the
    /// `history` arm reads back. `None` = the shipped
    /// `data/audit/execution_policy.jsonl` spelling (cwd-relative — one core
    /// per data directory owns it, the same road the other journals take;
    /// boot `load-refused` lines included). Tests pass a PRIVATE temp path so
    /// parallel test cores never share one journal file: two interleaved
    /// sets once fused into a single unparseable line and vanished from the
    /// history two tests were counting (the flake PR #378 disclosed).
    pub execution_policy_audit_path: Option<String>,
    /// #363: operator override for the per-asset position cap (the
    /// `--max-positions-per-asset` flag). `None` = the execution policy's
    /// value decides (`[accounts.<id>]` → `[defaults]` → code default 1).
    /// An explicit CLI number wins over the policy file, matching the
    /// CLI > env > TOML > default precedence the rest of the kernel uses.
    pub max_positions_per_asset: Option<u32>,
    /// Where to append closed trades (Node-compatible JSONL). None = off.
    pub trade_log_path: Option<String>,
    /// Where to persist tracked ORDERS (crash recovery + orphan detection).
    /// None = off. Default `data/orders/orders.jsonl`.
    pub order_log_path: Option<String>,
    /// Where to persist OPEN POSITIONS (crash recovery). None = off.
    /// Default `data/positions/positions.jsonl`.
    pub position_log_path: Option<String>,
    /// Discover rounds inside the core (Gamma slug queries) instead of relying on
    /// Node to push `engine.markets`. Default true when the engine is on.
    pub discovery_enabled: bool,
    /// Update-check switches (VERSIONING.md §7). Resolved in `main` through
    /// CLI > env > `configs/update.toml`; the runtime cell persisted in
    /// `data/update/state.json` outranks all three. Both default false — a
    /// default-configured stack makes zero outbound connections (INV-3).
    pub update_check_enabled: bool,
    /// Apply an update automatically (the kernel still never installs: the
    /// launcher owns that, P12). Default false.
    pub update_auto: bool,
    /// #379: seconds between AUTOMATIC update checks. `0` (the default) means
    /// NO scheduler task exists — the clock is an explicit configuration
    /// choice (`--update-interval-secs` / `BLITZKRIEG_UPDATE_INTERVAL_SECS` /
    /// `update.interval_secs`), never a default.
    pub update_interval_secs: u64,
    /// Start with Shadow Evolution enabled (opt-in; default false).
    pub shadow_evolution_enabled: bool,
    /// Optional Shadow Evolution tuning (tests/ops). None = crate defaults.
    pub shadow_evolution_tuning: Option<ShadowEvolutionTuning>,
    /// The shadow-evolution CONFIG FILE the switch verbs write back into
    /// (`user_layer/configs/shadow_evolution.toml` in production). The file is
    /// the single source of truth for `enabled` / `auto_evolve`: a runtime
    /// flip persists here and the next boot reads it back, so the panel's
    /// file/runtime conflict state cannot exist. `None` disables the write
    /// path — the backtester and most in-process tests run with `None`, and a
    /// switch flip then stays runtime-only (the same posture as
    /// `runtime_config_path` / `accounts_config_path`).
    pub shadow_config_path: Option<String>,
    /// Optional per-strategy entry caps, keyed by strategy name (P-1.1). An
    /// absent entry means unlimited — a strategy keeps its current behaviour
    /// unless an operator configures a cap.
    pub strategy_limits: HashMap<String, StrategyLimit>,
    /// E-VA: per-strategy VIRTUAL quota pots, keyed by strategy name. Empty
    /// (the shipped default) = the virtual-account layer does not exist: no
    /// pot, no reservation, no release — every strategy keeps the per-order
    /// real-ledger path byte-identically. A configured strategy that also
    /// declares `atomic_batch` gets the atomic batch pre-reservation.
    pub virtual_limits: HashMap<String, VirtualAccountLimit>,
    /// Maker→taker escalation deadline for engine entries (P-1.2). Defaults to the
    /// long-standing 5000 ms; a backtest may model a different venue latency.
    pub entry_maker_timeout_ms: i64,
    /// Fill-realism model for the dry matcher (P-1.2). Default = identity, so the
    /// live/dry path is unchanged; a backtest raises slippage/latency/probability.
    pub fill_model: crate::sim::FillModel,
    /// Where to mirror every market-data event the engine consumes (P-1.3).
    /// None = off (default). Used to build the local L2/tick corpus a backtest
    /// replays.
    pub event_archive_path: Option<String>,
    /// Stop recording once the archive reaches this size (MiB); 0 = unlimited.
    /// Nothing is ever deleted — a full archive just stops growing. This is the
    /// whole-session bound; `event_archive_rotate_mb` bounds one segment.
    pub event_archive_max_mb: u64,
    /// Rotate the archive into a new segment every this many MiB (0 = never).
    /// A 24/7 capture needs this: `≈11 MB/min` reaches any sane session cap within
    /// hours, and an un-rotated archive that hits the cap goes dark silently.
    /// Rotation renames the finished segment (UTC-stamped) and opens a fresh one;
    /// nothing is ever deleted.
    pub event_archive_rotate_mb: u64,
    /// Stop recording rather than let the archive's volume drop below this many
    /// MiB free (0 = no guard). Checked once per rotation, so it costs nothing on
    /// the hot path.
    pub event_archive_min_free_mb: u64,
    /// How old an orderbook may be before the engine refuses to price off it,
    /// in milliseconds (issue #205). `0` = the check is OFF (any age accepted).
    ///
    /// This is the knob that decides how long after the feed goes quiet the bot
    /// stops trading, so it is an operator setting rather than a compiled
    /// constant: CLI `--max-orderbook-stale-ms` / `BK_MAX_ORDERBOOK_STALE_MS`,
    /// validated at startup and echoed in the boot log. The default is
    /// [`crate::engine::DEFAULT_MAX_ORDERBOOK_STALE_MS`] — the value every
    /// deployment has always run with, so an unconfigured kernel is unchanged.
    pub max_orderbook_stale_ms: i64,
    /// 任务二 1-3 — the NTP clock-offset probe's server host. The empty string
    /// is the documented "off" spelling: `probe::should_spawn_ntp` refuses it,
    /// so the production server spawns ZERO network tasks. The measurement
    /// loop is started ONLY by `ipc::server::run` (production); a replay
    /// never constructs a socket. Default `probe::DEFAULT_NTP_HOST`.
    pub ntp_host: String,
    /// 任务二 1-3 — seconds between NTP probes. 0 = the periodic loop is off.
    /// Default `probe::DEFAULT_NTP_INTERVAL_SECS` (300). The core itself never
    /// reads this — the server does, at the one production spawn site.
    pub ntp_interval_secs: i64,
    /// How often the in-kernel accounting audit runs, in seconds (issue #189).
    /// 0 disables it. Default 30: the acceptance criterion is "drift is alerted,
    /// persisted and blocks new entries within 30s of appearing".
    pub audit_interval_sec: i64,
    /// Where to append one JSONL record per audit (issue #189). None = derive
    /// `reconcile.jsonl` beside the order log; `order_log_path: None` (the
    /// backtester and most tests) means no audit file at all.
    pub audit_log_path: Option<String>,
    /// Where to persist the applied-fill idempotency table (issue #178). None =
    /// derive `applied-fills.jsonl` beside the order log; `order_log_path: None`
    /// means no persistence, i.e. the table is per-session.
    pub applied_log_path: Option<String>,
    /// Whether a failing audit blocks NEW entries (default true; issue #189).
    /// Exits are never blocked — a drifted ledger must not be able to trap the
    /// bot in a position (the #174 failure mode). Turning this off keeps the
    /// alert and the persisted record while leaving the gates alone.
    pub audit_halt_entries: bool,
    /// Where to persist the SETTLEMENT journal (issue #175). None = derive
    /// `settlements.jsonl` beside the order log; `order_log_path: None` means no
    /// persistence, i.e. settlement idempotency is per-session.
    pub settlement_log_path: Option<String>,
    /// The ledger cash journal directory: one `data/ledger/<account>.journal.jsonl`
    /// per account, one append per cash movement, replayed at boot so the
    /// balance survives a restart (last row's `balance_after`). `None` = the
    /// historical memory-only ledger (`--no-ledger-journal`, the backtester,
    /// in-process tests) — nothing is written and nothing is replayed, and
    /// boot seeds exactly as before.
    pub ledger_journal_dir: Option<String>,
    /// TEST HOOK, dry mode only: make the first N redemption attempts of every
    /// claim fail before one succeeds. `0` (the default) is the production
    /// behaviour — the simulated redemption always lands. A failure is a real
    /// `RedemptionFailure` through the real path (`note_failure` → backoff →
    /// retry), so the retry clock, the receivable and the manual-stop rule are
    /// exercised exactly as a venue failure would exercise them. The live path
    /// never reads this field: `simulate_redemptions` is the only consumer and it
    /// runs only where `settles_locally()` holds.
    pub dry_redeem_fail: u32,
    /// TEST HOOK, dry mode only: report those injected failures as `manual`, i.e.
    /// stop the automatic retries for good (`next_attempt_ms = i64::MAX`). Only
    /// meaningful together with [`CoreConfig::dry_redeem_fail`].
    pub dry_redeem_manual: bool,
    /// This core replays archived events, and may therefore charge a taker-fee
    /// schedule other than the shipped one (#234, item 4).
    ///
    /// `--fee-model` is the counterfactual knob for a replay, and the CLI refuses
    /// it without `--backtest`; this field is the same statement made where the
    /// charge happens. The two live charge sites
    /// ([`Core::live_taker_fee_pct`]) assert that a core which has NOT declared
    /// itself a replay is charging the shipped schedule — the isolation used to
    /// live only in the CLI, which meant a future caller of `set_fee_schedule`
    /// could have repriced live fills with no code objecting. Default `false`:
    /// only the backtester sets it.
    pub fee_schedule_replay: bool,
    /// Model the venue's maker rebate in fills (almach-benchmark REPORT §5
    /// 失真点 5): the official rule pays a market's makers 20% of the taker
    /// fee that market accrued, pro-rata by maker share. A replay cannot see
    /// the pool, so `apply_delta_effects` books the per-fill approximation —
    /// each MAKER fill earns `0.20 ×` the taker fee that same notional would
    /// have paid — as a cash credit AND a position-flow accrual in one step,
    /// which keeps `balance == seed + Σ net` an identity and lets a
    /// maker-vs-taker comparison read the rebate it was missing.
    ///
    /// Replay-only for the same reason `fee_schedule_replay` is: a live core
    /// crediting modeled cash would mint money the venue never reported. The
    /// backtester sets this together with `fee_schedule_replay`; default
    /// `false` = every existing deployment and test is byte-identical.
    /// #388's stream rows (real MAKER_REBATE ledger lines) credit on top of
    /// this, never instead of it — a corpus carrying real rows and a replay
    /// that models the pool would double-count, which is a converter-choices
    /// decision an operator makes per corpus, not something the kernel
    /// guesses.
    pub model_maker_rebate: bool,
    /// E25 (#331): write the intent-audit trail (`data/audit/intents.jsonl` +
    /// throttled `INTENT_DECISION` pushes). The ARBITRATION GATES always run;
    /// this silences only the WRITES (`--no-intent-audit`), which is exactly
    /// what P1's with/without-baseline double-run proves off the trading path.
    pub intent_audit_enabled: bool,
    /// Replay fast path (`--backtest-fast`, CLI-refused without `--backtest`,
    /// mirroring `--fee-model`). Gates three classes of per-tick work that a
    /// frozen book makes provably redundant:
    ///   1. the exit LADDER for hold-to-settlement strategies (the host
    ///      discards every holder verdict anyway — settlement owns them);
    ///   2. full-book re-valuation of positions whose book did not change;
    ///   3. Lua evaluation on ticks where no data arrived and no round-timing
    ///      boundary was crossed (the same inputs reproduce the same output).
    ///
    /// Dirty-token and dirty-order tracking keep the traded paths exact.
    /// Default `false`: every default run keeps the historical byte-for-byte
    /// tick behaviour, evaluations counter included.
    pub backtest_fast: bool,
    /// Issue #390 (replay nofast): the maintenance grid the replay drives the
    /// core on (ms). The deadline-scheduled exit ladder backfills its counter
    /// ratchets over this step, so the core must know the schedule the caller
    /// runs. Default 50 ms — the live server's interval, the backtester's
    /// default and every shipped replay's step. Only read when
    /// `backtest_fast` is false and an engine drives positions (the nofast
    /// replay shape); every other path ignores it.
    pub tick_schedule_ms: i64,
    /// Issue #390: the replay maintenance scheduling. `true` makes the core
    /// apply the deadline-driven maintenance gates (Lua eval dirty-wake,
    /// settlement sync, escalation sweep, kline beat, exit-ladder schedule)
    /// to the NOFAST replay path — the same output-preserving gates
    /// `backtest_fast` applies, minus the all-holder skip (a nofast replay
    /// observes the ladder's suppressed-stop reports, so the ladder still
    /// runs on its schedule). Set ONLY by the replay driver; a live kernel
    /// keeps the historic every-tick behaviour on every path. Default
    /// `false`.
    pub replay_scheduling: bool,
}

impl CoreConfig {
    /// Build the engine config from this core config — the ONE mapping used by
    /// both the live server and the backtester, so a replay drives the identical
    /// engine a live run would (P-1.2).
    pub fn engine_config(&self) -> crate::engine::EngineConfig {
        crate::engine::EngineConfig {
            scanner: crate::scanner::ScannerConfig {
                assets: self.assets.clone(),
                round_duration_sec: self.round_duration_sec,
                min_round_age_sec: self.min_round_age_sec,
                min_time_left_sec: self.positions.exit.min_time_left_sec,
            },
            trend: crate::signal::TrendConfig {
                confirm_sec: self.trend_confirm_sec,
                window_floor_ms: self.trend_window_floor_ms,
                ..Default::default()
            },
            spread_arb: {
                let mut sa = crate::signal::SpreadArbConfig {
                    trend_confirm_sec: self.trend_confirm_sec,
                    ..Default::default()
                };
                if let Some(v) = self.spread_arb_trend_entry_factor {
                    sa.trend_entry_factor = v;
                }
                if let Some(v) = self.spread_arb_entry_min_obi {
                    sa.entry_min_obi = v;
                }
                if let Some(v) = self.spread_arb_entry_max_spread_pct {
                    sa.entry_max_spread_pct = v;
                }
                if let Some(v) = self.spread_arb_entry_dip_max_pct {
                    sa.entry_dip_max_pct = v;
                }
                if let Some(v) = self.spread_arb_entry_bounce_min_pct {
                    sa.entry_bounce_min_pct = v;
                }
                if let Some(v) = self.spread_arb_entry_bounce_window_sec {
                    sa.entry_bounce_window_sec = v;
                }
                sa
            },
            size_usd: self.size_usd,
            min_shares: self.min_shares,
            max_shares: self.max_shares,
            size_pct: self.size_pct,
            // #205: the runtime freshness budget. It flows through this one
            // mapping, so a live server and a backtest replay see the same value.
            max_orderbook_stale_ms: self.max_orderbook_stale_ms,
            strategy_sizes: self
                .strategy_limits
                .iter()
                .map(|(name, limit)| (name.clone(), limit.sizing()))
                .collect(),
            ..Default::default()
        }
    }

    /// Install the engine described by this config (the live server's startup
    /// step, reused verbatim by the backtester).
    ///
    /// The strategy selection is applied through the same `set_strategy_enabled`
    /// the IPC method uses, so a session started with `--enable-strategy` and one
    /// toggled at runtime end up in exactly the same state (including the Shadow
    /// Evolution re-registration the toggle triggers). Afterwards the effective
    /// enabled-set is persisted (when `strategy_state_path` is configured) so
    /// the next boot replays it — the boot lists and the runtime toggles write
    /// the same file, and the last explicit intent wins.
    ///
    /// Returns `Err` when an EXPLICIT request resolved to zero live strategies
    /// (#265): the operator named strategies and not one of them exists, so the
    /// kernel would boot with a healthy-looking banner and trade nothing. The
    /// refusal belongs to the caller (the server ends the boot, the backtester
    /// fails the run) rather than to this method, so the facts stay inspectable
    /// and an embedded caller decides what a zero-strategy engine means to it.
    /// [`CoreConfig::allow_zero_strategies`] is the acknowledged opt-out.
    pub fn install_engine(&self, core: &mut Core) -> Result<(), String> {
        core.enable_engine(crate::engine::Engine::new(self.engine_config()));
        self.load_strategy_dir(core);
        self.load_lua_strategy_dir(core);
        for name in &self.enabled_strategies {
            // E27 (§8.2): the startup `--enable-strategy` request runs through
            // the SAME handshake as `strategy.enable`: a strategy the active
            // plugin cannot serve is SKIPPED with an ERROR line — never
            // enabled silently, never able to trade in a market it declared
            // itself unfit for.
            let (found, applied, refusal) = core.set_strategy_enabled_checked(name, true);
            if let Some(reason) = refusal {
                tracing::error!(
                    strategy = name,
                    reason = reason,
                    "startup enable refused by the market-mode handshake; strategy skipped"
                );
                continue;
            }
            if !found || !applied {
                tracing::warn!(
                    strategy = name,
                    "unknown strategy requested at startup; ignored"
                );
            }
        }
        for name in &self.disabled_strategies {
            if !core.set_strategy_enabled(name, false) {
                tracing::warn!(
                    strategy = name,
                    "unknown strategy requested at startup; ignored"
                );
            }
        }
        // The persisted enabled-set survives a plugin switch: a strategy the
        // HANDSHAKE refused at boot stays in `strategy-state.json` under its
        // name, so switching the market plugin back (or to any other plugin
        // that serves it) re-enables it with no manual re-toggling. What is
        // recorded is the operator's INTENT; the handshake gate keeps the
        // refused strategy OFF for this session without erasing the intent.
        // (2026-10: the first cut saved the effective set only, so one
        // plugin switch silently stripped every incompatible strategy from
        // the file — and switching back brought nothing back.) An explicit
        // `--disable-strategy` / runtime disable still removes the name:
        // that is a decision, not a market mismatch.
        if let Some(path) = &self.strategy_state_path {
            let mut enabled = core.enabled_strategy_names();
            for name in &self.enabled_strategies {
                if !enabled.contains(name) && !self.disabled_strategies.contains(name) {
                    let known = core.strategy_names().iter().any(|n| n == name);
                    if known {
                        // Registered but handshake-refused this boot: keep
                        // the intent so a future session can serve it.
                        enabled.push(name.clone());
                    }
                }
            }
            crate::strategy_state::save(std::path::Path::new(path), &enabled);
        }
        // ④ 统一映射：install_engine 换掉了整个 Engine（插件切换、进化重
        // 注册），Engine 内存里的 confirmed 映射会一起丢 —— 账本是唯一真值
        // 源，每次安装后把 confirmed 事件回灌一遍。revocation 走
        // `unified.revoke`（service 层同步删 Engine 条目），Engine 自身只有
        // put 语义。
        if let Some(e) = core.engine.as_mut() {
            for event in core.unified_ledger.events() {
                e.put_unified_mapping(event.clone());
            }
        }
        self.strategy_startup_self_check(core)
    }

    /// The startup self-check (#265): state what was REQUESTED, what RESOLVED and
    /// what is enabled, and refuse a boot that resolves an explicit request to
    /// nothing.
    ///
    /// The line is printed unconditionally, refusals included, because it is the
    /// contract `scripts/walk-forward-sweep.mjs` reads back out of the kernel's
    /// log: a replay that could not resolve the strategies it asked for must be
    /// recognisable as such rather than arriving as a report with zero trades
    /// ("the strategy had no signal" and "there was no strategy" have to be
    /// different answers).
    ///
    /// A name the operator ALSO disabled explicitly is not part of the request —
    /// `--enable-strategy x --disable-strategy x` asked for nothing — so it can
    /// never be the reason this refuses.
    ///
    /// "Resolved" is asked of the live registry (`Core::strategy_names`), not of
    /// the enable loop: the question is whether the name exists as a strategy in
    /// this process, which is exactly what a missing library makes false. The
    /// persisted set is deliberately not consulted — a name that no longer
    /// resolves there was never requested by this invocation.
    pub(super) fn strategy_startup_self_check(&self, core: &Core) -> Result<(), String> {
        let requested: Vec<&str> = self
            .requested_strategies
            .iter()
            .map(String::as_str)
            .filter(|n| !self.disabled_strategies.iter().any(|d| d.as_str() == *n))
            .collect();
        let registered = core.strategy_names();
        let resolved = requested
            .iter()
            .filter(|n| registered.iter().any(|r| r == *n))
            .count();
        let enabled = core.enabled_strategy_names();
        eprintln!(
            "blitzkrieg-core: strategy startup self-check: requested={} resolved={} enabled=[{}]",
            requested.len(),
            resolved,
            enabled.join(", ")
        );
        if requested.is_empty() || resolved > 0 || self.allow_zero_strategies {
            return Ok(());
        }
        Err(format!(
            "refusing to start: none of the {} explicitly requested strateg{} resolved to a live \
         strategy ({}), so this kernel would run with no strategy at all — while reporting \
         itself healthy (the engine's live set is [{}]). Take the libraries from the strategy \
         directory (`--strategy-dir`, `BK_STRATEGY_DIR`, or the default `user_layer/strategies` \
         of the checkout the binary lives in) — every library there is refused unless it sits \
         under an approved strategy root (`{}` under a repository root, or a directory named \
         in {}) — or acknowledge the empty set with --allow-zero-strategies. See the \
         `strategy auto-load` lines above for what was rejected and why",
            requested.len(),
            if requested.len() == 1 { "y" } else { "ies" },
            requested.join(", "),
            enabled.join(", "),
            crate::strategy_engine::loader::APPROVED_ROOTS.join("`, `"),
            crate::strategy_engine::loader::ENV_ALLOW_DIRS,
        ))
    }

    /// Auto-load every strategy library under `strategy_dir` and enable it.
    ///
    /// This is the "drop a strategy in the folder and it works" path: the tree
    /// is walked (bounded depth) so both a bare `libfoo.dylib` and a
    /// `cargo build --release` product under `target/release/` are found, each
    /// library goes through the same policy-checked dlopen the IPC
    /// `strategy.load` uses, and a load that fails is reported and skipped —
    /// one broken file never blocks the rest or the kernel itself. Duplicate
    /// copies of the same strategy (debug + release) are skipped after the
    /// first registration: the engine refuses a second instance of a live name.
    /// Runs BEFORE `enabled_strategies` so a config that names a just-loaded
    /// library enables it in the same pass.
    pub(super) fn load_strategy_dir(&self, core: &mut Core) {
        #[cfg(feature = "strategy-loading")]
        let Some(dir) = &self.strategy_dir else {
            return;
        };
        #[cfg(feature = "strategy-loading")]
        {
            let mut libs = Vec::new();
            collect_strategy_libs(std::path::Path::new(dir), 0, &mut libs);
            // Plain path order. It used to rank the shipped legs first
            // (spread_arb, then trend_follow, then mean_reversion) so a
            // debug/release pair resolved to a predictable winner; with no
            // shipped strategies the ranking had no reachable branch left, and
            // path order alone still makes the first copy the one that wins.
            libs.sort();
            for path in libs {
                // E27 (§8.2 row 1): the startup scan is the LOG-ONLY handshake
                // site — an incompatible library registers disabled with the
                // mismatch on its receipt; the enable gate is where refusal
                // happens.
                let receipt = core.load_strategy_lib_scan(&path.to_string_lossy());
                // The success receipt is exactly "<name>@<version> registered
                // into the engine dispatch…"; a duplicate registration is
                // REJECTED with a message that itself contains the word
                // "registered", so the match must be on the full success shape.
                // A duplicate is benign — debug and release builds of the same
                // crate both land in the tree — so it is reported as skipped,
                // not failed, and the first copy (sorted order) wins.
                if receipt.contains("already registered") {
                    eprintln!(
                        "blitzkrieg-core: strategy auto-load skipped (duplicate): {}",
                        path.display()
                    );
                    continue;
                }
                let ok = receipt.contains("registered into the engine dispatch");
                // The message text must NOT contain the substring "auto-load":
                // the E30 gate takes the FIRST stream line carrying that
                // marker plus the package name, so a tracing line here would
                // shadow the receipt eprintln below.
                tracing::info!(path = %path.display(), ok, receipt = %receipt, "strategy package scanned");
                // The receipt IS this line's payload: the E30 gate
                // (lua-sandbox-check.mjs) parses `auto-load (ok|FAILED): …`
                // from the stream — a fixed-shape status summary (name@version,
                // registration phrase, disabled marker), not free kernel text.
                eprintln!(
                    "blitzkrieg-core: strategy auto-load {}: {}",
                    if ok { "ok" } else { "FAILED" },
                    receipt
                );
            }
        }
        #[cfg(not(feature = "strategy-loading"))]
        {
            let _ = core;
        }
    }

    /// E30 (§6.6): scan `lua_strategy_dir` for Lua strategy packages and
    /// register each one DISABLED — the Lua mirror of `load_strategy_dir`.
    /// Load failures are reported and skipped, never fatal; a duplicate name
    /// is skipped (the first package in sorted order wins, like dylibs).
    pub(super) fn load_lua_strategy_dir(&self, core: &mut Core) {
        let Some(dir) = &self.lua_strategy_dir else {
            return;
        };
        for pkg in
            crate::strategy_engine::lua_loader::discover_lua_packages(std::path::Path::new(dir))
        {
            let receipt = core.load_lua_package_inner(&pkg, false);
            if receipt.contains("already registered") {
                eprintln!(
                    "blitzkrieg-core: lua strategy auto-load skipped (duplicate): {}",
                    pkg.display()
                );
                continue;
            }
            let ok = receipt.contains("registered into the engine dispatch");
            // Same "no auto-load substring" rule as the dylib scan above —
            // the tracing line must not shadow the receipt eprintln below.
            tracing::info!(pkg = %pkg.display(), ok, receipt = %receipt, "lua strategy package scanned");
            // Same contract as the dylib scan above: the E30 gate parses
            // `auto-load (ok|FAILED): <receipt>` from the stream — a fixed-shape
            // status summary, not free kernel text.
            eprintln!(
                "blitzkrieg-core: lua strategy auto-load {}: {}",
                if ok { "ok" } else { "FAILED" },
                receipt
            );
        }
    }
}

/// Depth-bounded walk gathering `*.dylib`/`*.so` files under `dir`.
///
/// Bounded because `strategy_dir` may point at a crate checkout whose `target/`
/// tree is enormous; 6 levels comfortably covers `target/<profile>/deps/` while
/// never descending into the dependency graph's own build dirs. Symlinks are
/// not followed (a loop must not hang startup); unreadable subtrees are skipped.
pub(super) fn collect_strategy_libs(
    dir: &std::path::Path,
    depth: usize,
    out: &mut Vec<std::path::PathBuf>,
) {
    const MAX_DEPTH: usize = 6;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.')
                || name == "deps"
                || name == "build"
                || name == "incremental"
                || name.contains("probe")
                || name.contains("devcheck")
            {
                continue;
            }
            collect_strategy_libs(&path, depth + 1, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("dylib") | Some("so")
        ) {
            // Probe libraries (`devcheck_probe`) are generated into this tree by
            // `strategy-devcheck`, loaded through IPC `strategy.load`, and deleted
            // again. Auto-loading one at startup would make that gate measure a
            // strategy it did not register.
            if let Some(file_name) = path.file_name().and_then(|f| f.to_str())
                && (file_name.contains("probe") || file_name.contains("devcheck"))
            {
                continue;
            }
            out.push(path);
        }
    }
}

/// Entry caps and sizing for one strategy (E2-a). Every field is optional;
/// `None` = inherit the global value (no cap / global sizing). A per-strategy
/// sizing override can only tighten the global risk band, never widen it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyLimit {
    /// Max simultaneously open positions held by the strategy.
    pub max_open_positions: Option<usize>,
    /// Max notional (USD, sum of entry cost) held open by the strategy. A new
    /// entry is rejected when open notional + its own notional would exceed it.
    #[serde(with = "crate::decimal::opt", default)]
    pub max_open_notional_usd: Option<Decimal>,
    /// Target notional per entry (USD). Clamped to the global `size_usd`.
    #[serde(with = "crate::decimal::opt", default)]
    pub size_usd: Option<Decimal>,
    /// Floor on shares per entry. Raised to the global `min_shares` when lower.
    #[serde(with = "crate::decimal::opt", default)]
    pub min_shares: Option<Decimal>,
    /// Ceiling on shares per entry. Clamped to the global `max_shares`.
    #[serde(with = "crate::decimal::opt", default)]
    pub max_shares: Option<Decimal>,
    /// Relative allocation weight (E16/#98): a leg with a weight gets a weighted
    /// share of the per-entry budget instead of an identical one. Scales the
    /// leg's own size override or the global `size_usd`; a weight can only
    /// shrink the budget, and a non-positive weight disables the leg's entries
    /// entirely.
    #[serde(with = "crate::decimal::opt", default)]
    pub size_weight: Option<Decimal>,
    /// This leg's own equity-relative budget (#202). `None` = the global
    /// `size_pct`; a value is clamped to it when the global is armed, and
    /// stands alone when the global is off.
    #[serde(with = "crate::decimal::opt", default)]
    pub size_pct: Option<Decimal>,
}

impl StrategyLimit {
    /// The sizing knobs this limit overrides (`None` across the board when it
    /// configures only caps — the engine then uses the globals).
    pub fn sizing(&self) -> crate::engine::StrategySize {
        crate::engine::StrategySize {
            size_usd: self.size_usd,
            min_shares: self.min_shares,
            max_shares: self.max_shares,
            size_weight: self.size_weight,
            size_pct: self.size_pct,
        }
    }
}

/// E-VA: one strategy's VIRTUAL quota — a risk-layer budget on the
/// (account, strategy) axis that exists NEXT TO the real cash ledger, never
/// inside it. A strategy declaring `atomic_batch` (and listed here) reserves
/// its whole candidate batch against this pot BEFORE the first leg goes out;
/// an over-cap batch is refused whole, through the same StrategyLimit
/// rejection accounting (`limit.virtualCap` bucket) — pair legs land
/// together or not at all.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualAccountLimit {
    /// The pot's ceiling: the strategy's combined outstanding virtual
    /// reservation on each listed account (its resting reservations plus any
    /// not-yet-released batch hold). `None` = no virtual quota (the strategy
    /// keeps the per-order real-ledger path).
    #[serde(with = "crate::decimal::opt", default)]
    pub max_batch_notional_usd: Option<Decimal>,
    /// Which account scopes this strategy's pot covers. Empty = the pot is
    /// not armed anywhere (the default-off posture).
    #[serde(default)]
    pub accounts: Vec<String>,
}

/// Overridable Shadow Evolution thresholds (all optional).
#[derive(Debug, Clone, Default)]
pub struct ShadowEvolutionTuning {
    pub min_sample_count: Option<u32>,
    pub min_win_rate_improvement: Option<Decimal>,
    pub min_profit_factor_improvement: Option<Decimal>,
    pub min_observation_secs: Option<i64>,
    pub cooldown_secs: Option<i64>,
    pub variant_count: Option<usize>,
    /// Rolling metrics window (seconds). Settable from the config file, which
    /// expresses it in minutes (`evaluation_window_minutes`).
    pub evaluation_window_secs: Option<i64>,
    /// Per-evolution step ceiling (Lock 2). `main` clamps this to the built-in
    /// ceiling before it gets here — a config file may tighten the gradient lock
    /// but nothing may widen it.
    pub max_gradient: Option<Decimal>,
    /// Directory for the per-strategy audit files (`<dir>/<strategy>.jsonl`).
    /// Defaults to `data/evolution`; a harness points it at a scratch directory
    /// so an experiment never writes into the operator's real audit history.
    pub audit_dir: Option<String>,
    /// E13 (#95): start in the unattended mode (`auto_evolve`). The switch is
    /// also runtime-toggleable and its LAST runtime state is persisted, so this
    /// only seeds the very first boot.
    pub auto_evolve: Option<bool>,
    /// Seconds between DEEP evolution rounds (the config file expresses this
    /// in minutes; `main` converts).
    pub evolution_cycle_secs: Option<i64>,
    /// Seconds an undecided proposal stays decidable (default 7 days).
    pub proposal_ttl_secs: Option<i64>,
    /// Knobs one DEEP-cycle variant moves simultaneously (>= 1).
    pub deep_dims: Option<usize>,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Dry,
            default_maker_timeout_ms: 5000,
            exit_taker_slip_ticks: 2,
            risk: RiskConfig::default(),
            dry_seed_balance: Decimal::from(10_000),
            markets: Vec::new(),
            positions: PositionConfig::default(),
            max_consecutive_losses: 3,
            breaker_cooldown_sec: 300,
            round_duration_sec: 900,
            // Empty = no plugin's declaration is on file, so every
            // round-duration admission refuses (fail-closed). The production
            // boot overwrites this from the registry; in-process tests with
            // no market concern keep the historical behaviour through the
            // TEST_ALLOWED_ROUNDS helper.
            allowed_round_secs: Vec::new(),
            round_labels: std::collections::BTreeMap::new(),
            round_duration_source: "default",
            runtime_config_path: None,
            env_file_path: None,
            credential_vars: std::collections::BTreeMap::new(),
            accounts_config_path: None,
            manual_ops_audit_path: None,
            unified_ledger_path: None,
            auto_exits_enabled: true,
            intent_audit_enabled: true,
            engine_enabled: false,
            assets: vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()],
            enabled_strategies: Vec::new(),
            disabled_strategies: Vec::new(),
            strategy_dir: None,
            lua_strategy_dir: None,
            strategy_state_path: None,
            requested_strategies: Vec::new(),
            allow_zero_strategies: false,
            min_round_age_sec: 30,
            size_usd: Decimal::new(25, 1), // 2.5
            min_shares: Decimal::from(10),
            max_shares: Decimal::from(10),
            size_pct: Decimal::ZERO,
            trend_confirm_sec: 60,
            trend_window_floor_ms: 10_000,
            spread_arb_trend_entry_factor: None,
            spread_arb_entry_min_obi: None,
            spread_arb_entry_max_spread_pct: None,
            spread_arb_entry_dip_max_pct: None,
            spread_arb_entry_bounce_min_pct: None,
            spread_arb_entry_bounce_window_sec: None,
            feed_ws_enabled: false,
            market_plugin: None,
            binance_assets: vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()],
            near_miss_path: None,
            // #363: default to the shipped config spelling; the loader treats
            // a MISSING file as the zero-config deployment (code defaults).
            // #380: this is a SPELLING, not a location — the binary entry
            // resolves it against the install root before the core is built;
            // the library honours it verbatim (see the field's doc above).
            execution_policy_path: Some(
                crate::execution_policy::EXECUTION_POLICY_CONFIG_PATH.to_string(),
            ),
            // #386: the shipped `data/audit/…` spelling resolves against the
            // process cwd — production's single core owns that directory; a
            // test that wants isolation passes its own private path.
            execution_policy_audit_path: None,
            max_positions_per_asset: None,
            // #trades-mode-split: THIS default is the zero-config DRY
            // deployment's ledger — the historical `data/trades/trades.jsonl`
            // the dry-era rows and dry tooling already own. A LIVE boot never
            // reads this cell: the binary's `main` derives its default through
            // `crate::trade_db::default_trade_log_path(mode)` (live →
            // `trades-live.jsonl`) before the config is built. Keep the two
            // spellings mirrored when the default tree changes.
            trade_log_path: Some("data/trades/trades.jsonl".to_string()),
            order_log_path: Some("data/orders/orders.jsonl".to_string()),
            position_log_path: Some("data/positions/positions.jsonl".to_string()),
            discovery_enabled: true,
            update_check_enabled: false,
            update_auto: false,
            update_interval_secs: 0,
            shadow_evolution_enabled: false,
            shadow_evolution_tuning: None,
            shadow_config_path: None,
            strategy_limits: HashMap::new(),
            virtual_limits: HashMap::new(),
            entry_maker_timeout_ms: 5000,
            fill_model: crate::sim::FillModel::default(),
            event_archive_path: None,
            event_archive_max_mb: 512,
            event_archive_rotate_mb: 0,
            event_archive_min_free_mb: 0,
            // #205: the shipped freshness budget, unchanged.
            max_orderbook_stale_ms: crate::engine::DEFAULT_MAX_ORDERBOOK_STALE_MS,
            // 任务二 1-3: the shipped NTP probe posture — pool.ntp.org every
            // 5 minutes. `should_spawn_ntp("")` = off is the deployment's
            // documented kill switch (`--ntp-host ""`).
            ntp_host: crate::probe::DEFAULT_NTP_HOST.to_string(),
            ntp_interval_secs: crate::probe::DEFAULT_NTP_INTERVAL_SECS,
            audit_interval_sec: 30,
            audit_log_path: None,
            applied_log_path: None,
            ledger_journal_dir: None,
            audit_halt_entries: true,
            settlement_log_path: None,
            dry_redeem_fail: 0,
            dry_redeem_manual: false,
            // #234: nothing but the backtester may charge a counterfactual fee.
            fee_schedule_replay: false,
            model_maker_rebate: false,
            // Replay fast path: off by default — historical tick behaviour is
            // byte-for-byte until the operator opts in with --backtest-fast.
            backtest_fast: false,
            // Issue #390: the maintenance grid the exit ladder's deadline
            // schedule backfills over (the replay driver's tick step).
            tick_schedule_ms: 50,
            // Issue #390: replay maintenance scheduling off by default — the
            // live kernel stays every-tick on every path.
            replay_scheduling: false,
        }
    }
}

// ── Fee model (#182, #203) ───────────────────────────────────────────────────
//
// The taker fee was a formula copied into four places: the kernel's charge path
// and three gate/reconcile scripts. Copies cannot notice each other changing,
// and the failure is one-directional and quiet — the kernel switches its default
// schedule, the scripts keep asserting the old arithmetic, and a gate that
// passes stops meaning what it says. The scripts now take the per-share fee from
// `core.feeQuote` (below) and pin the DECLARED model, so a change here without a
// change there is a red gate instead of a wrong green one.
//
// The schedule itself lives in ONE place — `exit_policy::FeeSchedule`, whose
// `source` field names the publication each set of parameters comes from — and
// the quote below reports whatever that is, so a fee can never be charged under
// one name and declared under another. `official` (Polymarket's published
// `rate * p * (1 - p)`, exponent 1) is the succession path: switching the
// default means changing `exit_policy::legacy_quadratic_schedule` to it AND the
// pinned expectation in `scripts/lib/fee-model.mjs`, in the same change — which
// is the point. What that costs the strategies is measured, not argued:
// `scripts/fee-model-sensitivity-check.mjs` replays the frozen corpus under both
// schedules (#203).
//
// #234 finished the close-out. The curve's arithmetic has ONE spelling in the
// kernel — `FeeSchedule::fee_per_share` — so `fee_quote` no longer recomputes
// `rate * (p*(1-p))^exponent` beside it; `model_matches` now holds the charged
// fee against the PINNED parameters (`exit_policy::pinned_fee_parameters`), a
// source independent of the schedule it is checking, so a changed rate is
// reported instead of hiding behind two identical spellings. The authoritative
// cross-language self-check is `scripts/core-parity.mjs::assertPinnedFeeModel`
// against `scripts/lib/fee-model.mjs`; this one is the in-process half.

/// The fee actually charged per share at `price` under `schedule`:
/// `(schedule.fee_pct(p)/100) * p`, spelled exactly as the charge path spells it
/// (`apply_delta_effects`, `reconcile`), so a quote cannot drift from a fill.
/// Maker fills are exempt and are reported separately as zero.
pub(super) fn charged_per_share(
    schedule: crate::exit_policy::FeeSchedule,
    price: Decimal,
) -> Decimal {
    (schedule.fee_pct(price) / Decimal::ONE_HUNDRED) * price
}

/// The fee actually charged per share at `price` under the schedule in force.
pub fn charged_fee_per_share(price: Decimal) -> Decimal {
    charged_per_share(crate::exit_policy::fee_schedule(), price)
}

/// Does the schedule in force still price what this repository PINS for its
/// declared name (#234, item 1)?
///
/// The pin is `exit_policy::pinned_fee_parameters` — deliberately NOT the
/// registry the schedule itself came from, because a check that reads the same
/// source as the thing it checks cannot fail: flipping the shipped rate from
/// 0.125 to 0.07 moved the declaration and the charge together and this reported
/// `true` while the fee moved 44%. Reading the pin makes that flip report
/// `false`, which is what the field was introduced to mean.
///
/// The pin is held against the schedule's PARAMETERS, not against the number one
/// sampled price happens to produce. `fee_quote` samples a single price (0.5
/// unless the caller asks otherwise) and two different `(rate, exponent)` pairs
/// can cross there: `0.03125*(p(1-p))^1` prices the pinned legacy curve's
/// `0.0078125` per share at p=0.5 and a different fee at every other price, so a
/// value-only comparison at the sampled point reports a wrong configuration as a
/// match (see `fee_quote_tests::model_matches_rejects_a_curve_that_only_coincides_at_the_sampled_price`).
/// Because both sides of that comparison run the same expression, it cannot see
/// a change to the expression itself either — what it can see is exactly the
/// parameters, which is what the pin describes. Drift in the arithmetic is
/// covered where the arithmetic lives (`exit_policy`'s own pricing tests) and by
/// the cross-language gate below, which recomputes the fee independently.
/// A schedule whose name this repository cannot describe is reported as NOT
/// matching, rather than as a green "nothing to check".
///
/// This is the in-process half. The authoritative, cross-language fee self-check
/// is `scripts/core-parity.mjs::assertPinnedFeeModel`, which holds this kernel's
/// `core.feeQuote` against the pinned table in `scripts/lib/fee-model.mjs` — the
/// two pins must be changed together, in the same change as the schedule.
pub(super) fn schedule_reproduces_pin(schedule: crate::exit_policy::FeeSchedule) -> bool {
    let Some((rate, exponent)) = crate::exit_policy::pinned_fee_parameters(schedule.name) else {
        return false;
    };
    schedule.rate == rate && schedule.exponent == exponent
}

/// Read-only quote of the fee schedule (#182). `price` defaults to the widest
/// point of the schedule (0.5) so a caller that only wants the model metadata
/// does not have to invent a price.
pub fn fee_quote(price: Option<Decimal>) -> crate::ipc::schema::FeeQuoteResult {
    fee_quote_at(crate::exit_policy::fee_schedule(), price)
}

/// [`fee_quote`] with the schedule supplied, so a test can ask what a
/// DIFFERENT schedule would report — the check has to be able to say "no", and
/// the process-wide schedule can only be set once (#234).
pub(super) fn fee_quote_at(
    schedule: crate::exit_policy::FeeSchedule,
    price: Option<Decimal>,
) -> crate::ipc::schema::FeeQuoteResult {
    let price = price.unwrap_or_else(|| dec!(0.5));
    let charged = charged_per_share(schedule, price);
    let model_matches = schedule_reproduces_pin(schedule);
    crate::ipc::schema::FeeQuoteResult {
        model: schedule.name.to_string(),
        rate: schedule.rate,
        exponent: schedule.exponent,
        price,
        fee_per_share: charged,
        fee_pct_of_price: schedule.fee_pct(price),
        maker_fee_per_share: Decimal::ZERO,
        model_matches,
    }
}

/// `round_duration.get` readout (`round_duration_readout`): assembled from the
/// LIVE config and the persisted cell on every call — the boot resolution
/// wrote `source` into the config's twin fields at startup, so a read answers
/// "what is running" and "what will the restart run" without any cached
/// snapshot that a write could make a lie. A core with no runtime config path
/// (backtests, in-process tests) reports `persisted: false` — there is no cell
/// to read, and the effective value is the truth for the next session too.
pub fn round_duration_readout(core: &Core) -> crate::ipc::schema::RoundDurationResult {
    use crate::ipc::schema::{RoundDurationOption, RoundDurationResult};
    let effective = core.config().round_duration_sec;
    let cell = core.config().runtime_config_path.as_deref().and_then(|p| {
        crate::ipc::runtime_config::load_round_duration_from(
            std::path::Path::new(p),
            &core.config().allowed_round_secs,
        )
    });
    let next_session = cell.map(|c| c.duration_sec).unwrap_or(effective);
    // The declared union the plugins own — the kernel hardcodes nothing
    // (whichever durations appear are exactly what the plugins claimed) and
    // the labels travel the same road (issue 648): the readout renders the
    // plugins' declared names, not a kernel-side decoder table.
    let mut allowed: Vec<RoundDurationOption> = core
        .config()
        .allowed_round_secs
        .iter()
        .map(|&sec| RoundDurationOption {
            sec,
            label: core.config().round_labels.get(&sec).copied(),
        })
        .collect();
    // A non-standard boot value (e.g. a test's 60s) is not in the declared
    // union — list it too, unlabeled, so the card can name what is actually
    // running instead of silently showing the nearest allowed value (and an
    // EMPTY union still answers with the effective value: the truth is
    // reported, even though every set would refuse).
    if !core.config().allowed_round_secs.contains(&effective) {
        allowed.push(RoundDurationOption {
            sec: effective,
            label: None,
        });
    }
    RoundDurationResult {
        effective_sec: effective,
        source: core.config().round_duration_source,
        allowed,
        next_session_sec: next_session,
        persisted: cell.is_some(),
    }
}

/// One position selected by `Core::flatten`, in the order its destructuring
/// loop consumes: id, token, condition, shares, strategy, asset, direction,
/// current price, account id.
pub(super) type FlattenTarget = (
    String,
    String,
    String,
    Decimal,
    String,
    String,
    String,
    Decimal,
    AccountId,
);

/// One due `maker_then_taker` order selected by `Core::tick`, in the order its
/// destructuring loop consumes: id, side, remaining, token, condition,
/// strategy, asset, direction, round slot, maker timeout, account id.
pub(super) type EscalationTarget = (
    String,
    Side,
    Decimal,
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
    AccountId,
);

/// Append-only journal of the OME's applied-fill idempotency table (issue #178).
///
/// The table used to be memory-only, so a restart forgot which venue trade ids
/// had already been booked and the venue's own re-delivery (a WS replay or the
/// REST sweep's recent-trade window) booked the same fill a second time — the
/// position doubled and the cash moved twice. Rebuilding it from `trades.jsonl`
/// is not possible: that log stores ONE AGGREGATED record per CLOSED position and
/// carries no venue trade ids at all, so there is nothing to fold a fill key from.
///
/// Same shape as the order log: one line per MUTATION (a key's applied size, or
/// a tombstone when it is evicted), folded latest-wins on load. The file lives
/// beside the order log because it is the OME's state, and it is compacted to the
/// live table on restore so it cannot grow across restarts.
pub(super) struct AppliedFillLog {
    pub(super) path: std::path::PathBuf,
}

impl AppliedFillLog {
    pub(super) fn new(path: impl AsRef<std::path::Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub(super) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Append the mutations. Best effort, like every other log here: a write
    /// failure must never interrupt trading (the in-memory table stays
    /// authoritative for this run, and the OME warns when its journal is full).
    pub(super) fn append(&self, records: &[AppliedFillRecord]) {
        if records.is_empty() {
            return;
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut buf = String::new();
        for r in records {
            match serde_json::to_string(r) {
                Ok(line) => {
                    buf.push_str(&line);
                    buf.push('\n');
                }
                Err(e) => tracing::warn!(error = %e, "cannot serialize applied-fill record"),
            }
        }
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            if let Err(e) = f.write_all(buf.as_bytes()) {
                tracing::warn!(error = %e, path = %self.path.display(), "applied-fill log append failed");
            }
        } else {
            tracing::warn!(path = %self.path.display(), "cannot open applied-fill log");
        }
    }

    /// Load the mutations in file order (the OME folds them). Unparseable lines
    /// are skipped, never fatal: a corrupt tail must not stop the core from
    /// starting, and every record kept is a duplicate the table will refuse.
    pub(super) fn load(&self) -> Vec<AppliedFillRecord> {
        crate::jsonl::load::<AppliedFillRecord>(
            &self.path,
            "applied-fill log: skipped unparseable lines",
        )
    }

    /// Rewrite the log as the current table (one line per live key). Called after
    /// a restore, so the file is the table rather than every mutation ever made.
    pub(super) fn compact(&self, snapshot: &[AppliedFillRecord]) {
        let mut buf = String::new();
        for r in snapshot {
            match serde_json::to_string(r) {
                Ok(line) => {
                    buf.push_str(&line);
                    buf.push('\n');
                }
                Err(e) => tracing::warn!(error = %e, "cannot serialize applied-fill record"),
            }
        }
        // #463: atomic rewrite — the compact runs right after a restore, which
        // is exactly when a torn file would be trusted as the dedup table.
        if !crate::jsonl::rewrite_atomic(&self.path, buf.as_bytes()) {
            tracing::warn!(path = %self.path.display(), "applied-fill log compact failed");
        }
    }
}

/// In-kernel accounting audit state (issue #189). Session-scoped: the anchor and
/// the strike counter describe THIS run's observations, and the durable record is
/// the audit JSONL.
#[derive(Debug, Clone, Default)]
pub(super) struct AuditRuntime {
    /// Last audit instant (0 = never).
    pub(super) last_at_ms: i64,
    /// Last audit that was written to the log even though it passed (heartbeat).
    pub(super) last_heartbeat_ms: i64,
    /// The baseline the anchored cash identity is measured from. Taken at the
    /// first audit whose structural checks pass and NEVER moved while the books
    /// are wrong — re-anchoring would hide the drift it exists to expose.
    pub(super) anchor: Option<CashIdentity>,
    pub(super) report: Option<AuditReport>,
    /// Consecutive cross-source-only failures; two in a row halt (a single one
    /// can be a stale venue read).
    pub(super) cross_source_failures: u32,
    /// New entries are currently blocked by a failing audit.
    pub(super) halted: bool,
    pub(super) halt_reason: String,
    pub(super) runs: u64,
    pub(super) failures: u64,
    /// Where each audit appends its verdict (None = no persistence).
    pub(super) audit_log_path: Option<String>,
    /// The last time each venue's cash was SET from that venue rather than
    /// moved by a fill: venue → `(at_ms, delta)`. The plugin host does this
    /// deliberately when nothing rests, so the trade-leg check can tell the
    /// venue's own correction from a local inconsistency instead of halting
    /// entries over it. Keyed by venue: a realign explains a residual only on
    /// the book that venue's cash belongs to.
    pub(super) last_realign: std::collections::HashMap<String, (i64, Decimal)>,
}

/// The kernel's last recorded error, as `engine.stats.lastError` exposes it.
///
/// One slot with exactly one writer ([`Core::note_error`]): whichever path went
/// wrong last — venue refusal, safety net, or the kernel's own refusal of a leg
/// — the panel banner and any outside observer read the same fact, with the code
/// that classifies it and the instant it happened. Before #180 the only such
/// slot was `last_venue_error`, which the taker rejection below never touched,
/// so `lastError` stayed empty (or stale) while orders were being refused.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastError {
    pub ts_ms: i64,
    pub code: CoreErrorCode,
    pub message: String,
}

impl LastError {
    /// How the LEGACY `engine.stats.lastVenueError` key renders this record.
    ///
    /// The shipped panel (`ui/webapp/webui/src/pages/Overview.vue`) renders this
    /// string as free text, and that is the compatibility this method owes: the
    /// shape `{ tsMs, message }` and the "last error, newest wins" instant are
    /// unchanged, so the banner keeps working without a frontend change.
    ///
    /// It is NOT byte-identical to the pre-#180 strings on every path, and the
    /// earlier claim that it was is wrong. The venue-reject path is identical
    /// (`{:?}` on the code, a colon, the message). The self-check and reconcile
    /// paths carried NO code prefix before #180 — they were bare prose
    /// (`"trading self-check failed: …"`, `"reconciliation sweep failed (n): …"`)
    /// — and now gain one, because one slot with one writer cannot render two
    /// shapes. Nothing shipped parses those two strings: the panel shows them as
    /// text, and the only test that reads the key asserts `contains`, not
    /// equality. `lastError` above is the structured form to migrate to.
    pub fn legacy_message(&self) -> String {
        format!("{:?}: {}", self.code, self.message)
    }
}

/// One placement's result: the order's id, the status the OME now holds for it,
/// and — when the kernel itself refused the leg after accepting it (a
/// dry/read-only taker whose book cannot fill it) — the structured reason.
///
/// `status == Rejected` with a `rejection` is the leg the venue would have
/// killed; `status == Rejected` without one is a leg an external actor (the
/// venue, a reconcile) retired. Before #180 the drier path returned neither, so
/// `orders.place` answered a bare `REJECTED` and the caller could not tell "no
/// crossing liquidity" from "not enough depth" from "risk said no".
#[derive(Debug, Clone)]
pub struct PlaceOutcome {
    pub order_id: OrderId,
    pub status: OrderStatus,
    pub rejection: Option<CoreError>,
}

/// Upper bound on the kernel's pending-close exit-reason table (issue #190).
///
/// The table holds one entry per token whose closing SELL has been submitted but
/// not yet filled — a *pending-close* ledger, not a history — so its live
/// working set is the engine's tradeable fan-out: the round's tokens across the
/// configured assets (8 tokens × 3 assets = 24 on the shipped Polymarket
/// deployment) plus whatever manual flattens add on top. 64 is ≈2.5× the widest
/// live set, so a legitimate session never reaches the bound; it exists so a leak
/// is bounded for the life of the process instead of growing forever. Eviction
/// prefers entries whose position is already gone (stale by construction) and
/// drops the oldest otherwise; both are counted in `engine.stats.exitReasons`.
pub(super) const EXIT_REASON_TABLE_MAX: usize = 64;

/// The counter-evidence bar for the dry settlement ladder: a partner-token bid
/// at or above this names the held leg the loser. Same value as the live
/// venue path's own decisiveness bar (Gamma's DECISIVE_HIGH, 0.99) — the two
/// ladders demand the same strength of verdict before they book money.
pub(crate) const COUNTER_DECISIVE_HIGH: Decimal = rust_decimal_macros::dec!(0.99);

/// #351: a dry-mode taker leg in flight — the fill model prices travel time
/// (`taker_latency_ms`) and report time (`taker_rtt_ms`), so the leg must
/// never fill on the decision-time quote. Stage 1 (match) fires at the first
/// drain with `now >= execute_at_ms` against the book as it stood then
/// (strictly no future data); stage 2 (report) books the fill or the kill at
/// the first drain with `now >= report_at_ms`.
#[derive(Debug, Clone)]
pub(super) struct InflightTaker {
    pub(super) order_id: OrderId,
    /// When the leg arrives at the venue and is matched.
    pub(super) execute_at_ms: i64,
    /// When the report (fill or kill) reaches the local books.
    pub(super) report_at_ms: i64,
    /// Stage 1: the price the venue would have printed — walk VWAP plus the
    /// model's slippage, full size (FOK is all-or-nothing).
    pub(super) matched_price: Option<Decimal>,
    /// Stage 1: why the venue would have killed the FOK.
    pub(super) rejected_msg: Option<String>,
}
