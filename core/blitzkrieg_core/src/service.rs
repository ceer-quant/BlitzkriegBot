//! Core service — assembles the OME, ledger, risk gate and dry matcher and
//! implements every typed command. Synchronous except for the event sink, so it
//! is fully unit-testable; the async UDS layer and live venue sit on top.

use crate::engine::BatchBook;
use crate::ipc::schema::Event;
use crate::ledger::Ledger;
use crate::market::{venue_credentials_missing, venue_credentials_ready};
use crate::model::*;
use crate::ome::{AppliedFillRecord, FillDelta, FillOutcome, LateFill, Ome, SubmitParams};
use crate::position::{EquityBasis, OpenParams, OpenPosition, PositionConfig, PositionManager};
use crate::reconcile::{AuditInput, AuditReport, CashIdentity};
use crate::risk::{LossBreakers, RiskConfig, RiskGate};
use crate::settlement::{
    EXPIRY_FALLBACK_AFTER_MS, SETTLEMENT_EXIT_REASON, SettlementBook,
    UNVERIFIED_EXPIRY_EXIT_REASON, booking_for, settlement_key,
};
use crate::shadow_evolution::{
    EvolutionOutcome, EvolutionStatus, MutableParams, ShadowEvolution, ShadowEvolutionConfig,
};
use crate::sim::{Book, rests_on_book};
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet, VecDeque};
use tokio::sync::mpsc;

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
    fn strategy_startup_self_check(&self, core: &Core) -> Result<(), String> {
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
    fn load_strategy_dir(&self, core: &mut Core) {
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
    fn load_lua_strategy_dir(&self, core: &mut Core) {
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
fn collect_strategy_libs(dir: &std::path::Path, depth: usize, out: &mut Vec<std::path::PathBuf>) {
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
fn charged_per_share(schedule: crate::exit_policy::FeeSchedule, price: Decimal) -> Decimal {
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
fn schedule_reproduces_pin(schedule: crate::exit_policy::FeeSchedule) -> bool {
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
fn fee_quote_at(
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
type FlattenTarget = (
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
type EscalationTarget = (
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
struct AppliedFillLog {
    path: std::path::PathBuf,
}

impl AppliedFillLog {
    fn new(path: impl AsRef<std::path::Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Append the mutations. Best effort, like every other log here: a write
    /// failure must never interrupt trading (the in-memory table stays
    /// authoritative for this run, and the OME warns when its journal is full).
    fn append(&self, records: &[AppliedFillRecord]) {
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
    fn load(&self) -> Vec<AppliedFillRecord> {
        crate::jsonl::load::<AppliedFillRecord>(
            &self.path,
            "applied-fill log: skipped unparseable lines",
        )
    }

    /// Rewrite the log as the current table (one line per live key). Called after
    /// a restore, so the file is the table rather than every mutation ever made.
    fn compact(&self, snapshot: &[AppliedFillRecord]) {
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
struct AuditRuntime {
    /// Last audit instant (0 = never).
    last_at_ms: i64,
    /// Last audit that was written to the log even though it passed (heartbeat).
    last_heartbeat_ms: i64,
    /// The baseline the anchored cash identity is measured from. Taken at the
    /// first audit whose structural checks pass and NEVER moved while the books
    /// are wrong — re-anchoring would hide the drift it exists to expose.
    anchor: Option<CashIdentity>,
    report: Option<AuditReport>,
    /// Consecutive cross-source-only failures; two in a row halt (a single one
    /// can be a stale venue read).
    cross_source_failures: u32,
    /// New entries are currently blocked by a failing audit.
    halted: bool,
    halt_reason: String,
    runs: u64,
    failures: u64,
    /// Where each audit appends its verdict (None = no persistence).
    audit_log_path: Option<String>,
    /// The last time each venue's cash was SET from that venue rather than
    /// moved by a fill: venue → `(at_ms, delta)`. The plugin host does this
    /// deliberately when nothing rests, so the trade-leg check can tell the
    /// venue's own correction from a local inconsistency instead of halting
    /// entries over it. Keyed by venue: a realign explains a residual only on
    /// the book that venue's cash belongs to.
    last_realign: std::collections::HashMap<String, (i64, Decimal)>,
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
const EXIT_REASON_TABLE_MAX: usize = 64;

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
struct InflightTaker {
    order_id: OrderId,
    /// When the leg arrives at the venue and is matched.
    execute_at_ms: i64,
    /// When the report (fill or kill) reaches the local books.
    report_at_ms: i64,
    /// Stage 1: the price the venue would have printed — walk VWAP plus the
    /// model's slippage, full size (FOK is all-or-nothing).
    matched_price: Option<Decimal>,
    /// Stage 1: why the venue would have killed the FOK.
    rejected_msg: Option<String>,
}

pub struct Core {
    config: CoreConfig,
    ome: Ome,
    /// #351: dry taker legs in flight — two-stage time-causal fill
    /// (match at `execute_at_ms`, report at `report_at_ms`). Empty
    /// whenever `fill_model.taker_latency_ms <= 0 && taker_rtt_ms <= 0`.
    inflight_takers: Vec<InflightTaker>,
    /// E28 (§9.3): the per-account money truth. "The ledger" is now a lookup
    /// keyed by the order/request's `account_id` at every money-moving site —
    /// the active account's book stands in where no order context exists
    /// (seeding, equity views, the audit's cash identity). `Ledger` itself is
    /// UNTOUCHED: reserve/release/settle keep their exact signatures.
    accounts: crate::account::AccountLedgers,
    risk: RiskGate,
    /// Per-strategy consecutive-loss breakers (KI-10 / D-18 A). The daily loss
    /// cap and kill switch stay global — see [`LossBreakers`].
    breaker: LossBreakers,
    /// E25 (#331): the intent-audit sink — one JSONL line per arbitrated
    /// suggestion plus the throttled `INTENT_DECISION` push state (§3.4).
    intent_audit: crate::arbitration::AuditSink,
    positions: PositionManager,
    books: HashMap<TokenId, Book>,
    /// Exit reason chosen for an in-flight closing SELL, keyed by token id, so a
    /// full sell fill closes the position with the right reason.
    ///
    /// A PENDING-CLOSE ledger, not a history (issue #190): an entry exists only
    /// while the token still has an open position, and the table can never
    /// exceed [`EXIT_REASON_TABLE_MAX`]. A leaked entry would be inherited by
    /// the NEXT position on the same token and mis-attribute its exit.
    exit_reasons: HashMap<TokenId, ExitReason>,
    /// Insertion order of [`Self::exit_reasons`], oldest first. The map cannot
    /// name its own oldest entry and the bound has to evict one; kept in
    /// lockstep with the map's key set by the four helpers below.
    exit_reason_order: VecDeque<TokenId>,
    /// Entries dropped by the [`EXIT_REASON_TABLE_MAX`] bound, session-scoped
    /// and surfaced in `engine.stats`: an eviction loses real exit attribution,
    /// so it is counted rather than silent.
    exit_reasons_evicted: u64,
    /// Entries dropped because their token no longer has an open position — the
    /// expected path (a close, a round rollover).
    exit_reasons_swept: u64,
    /// Strategy close intents drained from the engine in `engine_evaluate` and
    /// consumed by the shared exit-submission path in `run_exit_checks`.
    strategy_exits: Vec<crate::strategies::StrategyExitIntent>,
    /// 跨所套利弹窗 v2: the leg configuration in force (the accounts.toml
    /// `[cross_venue_arb]` section's memory mirror). `None` = unconfigured.
    /// Written by `arb_config_set_persisting` (file + memory in one
    /// transaction), read by the engine hot-push (the adapter's account
    /// stamp + the strategy's `bk.params()` leg entries).
    arb_leg_config: Option<crate::account::CrossVenueArbConfig>,
    /// RFC 568 阶段一：内核侧配对状态簿（PairBook）。事实事件驱动的观察者
    /// 账本：engine 的提交管线在 arb_precheck 过闸后把带 pair 声明的腿
    /// declare 进来，OME 的成交/死亡事实经 on_order_event 吸收，sweep 在
    /// flatten 后落 Closed。进程内 BTreeMap，不持久化。
    pair_book: crate::pair::PairBook,
    /// Optional self-driving engine (P3). When present, book/spot/round events
    /// flow in and the core evaluates strategies and places orders on its own.
    /// Crate-visible so the replay driver can host strategies without going
    /// through the loader path (the kernel ships none to load).
    pub(crate) engine: Option<crate::engine::Engine>,
    /// E27 (§8.2): market-mode declarations of loaded user strategies, keyed
    /// by strategy name. In-tree strategies and libraries without a
    /// declaration are ABSENT = undeclared = the handshake does not apply
    /// (§8.1 — every 0.2 library's state).
    strategy_declarations: HashMap<String, Vec<blitzkrieg_strategy_api::StrategyMode>>,
    /// E27 (§8.2): every active plugin's declaration snapshot, injected by the
    /// server assembly BEFORE any strategy is loaded or enabled, so all three
    /// handshake sites judge against the same view. Multi-plugin: a strategy's
    /// declaration must be satisfiable by ANY ONE of these — a venue the
    /// strategy cannot serve must not block the venues it can. Empty
    /// (backtester, tests) leaves the handshake inert: nothing to be
    /// incompatible with.
    plugin_modes: Vec<crate::market::compat::PluginModes>,
    /// Per-plugin subscription controls (P4 → multi-plugin data plane). The
    /// key is the market-plugin name; with one active plugin this map holds a
    /// single entry and behaves exactly as the old `feed: Option<...>` slot.
    /// A set of tokens always reaches ONLY the named plugin's control — a
    /// Polymarket rollover must never replace a Kalshi subscription set.
    feeds: std::collections::BTreeMap<
        String,
        std::sync::Arc<dyn blitzkrieg_market_api::SubscriptionControl>,
    >,
    /// Extension registry (plugin lifecycle).
    extensions: crate::extension::ExtensionRegistry,
    /// Shadow Evolution (opt-in; disabled by default).
    shadow_evolution: ShadowEvolution,
    /// Closed-trade log (Node-compatible), when `trade_log_path` is set.
    trade_db: Option<crate::trade_db::TradeDb>,
    /// Durable order log, when `order_log_path` is set. Orders are restored from
    /// it at startup so a restart never forgets resting orders (orphan guard).
    order_db: Option<crate::order_db::OrderDb>,
    /// Durable snapshot of the OPEN position book, when `position_log_path` is
    /// set. Restored at startup so a restart never forgets open positions
    /// (unmanaged-exit guard; same failure class as the orphan-order bug).
    position_db: Option<crate::position_db::PositionDb>,
    /// Feed/decision counters for observability (P4 diagnostics).
    stats: CoreStats,
    /// 环境差异排查探针（任务二 1-3）：tick 断流 / spot 失效 / NTP 偏移的
    /// 内核侧记账。只观测——日志 + 计数器，从不拦截数据或订单路径；所有
    /// 时间判定吃调用方的 `now_ms`（回放里是虚拟时钟）。NTP 的网络查询
    /// 不在这里：`probe::spawn_loop` 只由生产的 `server::run()` 启动，
    /// 测量结果经 `note_ntp_offset` 灌回。
    probe_ticks: crate::probe::TickHealth,
    probe_spot: crate::probe::SpotWatch,
    probe_ntp: crate::probe::NtpWatch,
    /// 最近一次汇总的窗口 tick 数（stats 面直读 probe_ticks 会拿走窗口，
    /// 汇总清零——所以汇总瞬间留档）。
    probe_last_window_ticks: u64,
    /// 最近一次汇总点名的零 tick token（已排序）。
    probe_last_zero: Vec<String>,
    /// 最近一次汇总的最长静默 ms。
    probe_last_max_silence_ms: i64,
    /// 最近一次汇总时 spot 的停更时长（None = 从未见 spot）。
    probe_last_spot_stale_ms: Option<i64>,
    /// 断流 warn 总次数与 spot 停更 warn 总次数（事件计数，独立于
    /// probe_ticks.broken 的当前状态）。
    stats_probe_stream_breaks: u64,
    stats_probe_spot_stales: u64,
    /// #377 (治 #2): conditions the archive declared to carry an on-chain
    /// verdict (`RoundMarkets` markets flagged `archive_verdict`). The dry
    /// settlement ladder must not query these markets — the stream settles
    /// them — and querying would stamp a query clock no archive answer ever
    /// resets, tripping the blind alert.
    archive_verdicts: HashSet<String>,
    /// When this core's ACCOUNTING SESSION began, in epoch ms (issue #200).
    ///
    /// A seeded (`dry`/`readonly`) core re-seeds its cash ledger here
    /// (`Ledger::set_balance(dry_seed_balance)` in `new`) while its trade log
    /// and position log outlive the process. A cold ledger beside a trade book
    /// that still holds earlier sessions answers a different question than the
    /// cash identity asks, so an out-of-process audit of a seeded core has to
    /// scope `realized` to the trades closed at/after this instant. A live core
    /// ignores it: its opening cash is the venue's, and its identity telescopes
    /// across polls instead.
    started_at_ms: i64,
    /// Optional mirror of every market-data event into a JSONL archive (P-1.3).
    /// The backtester replays this file.
    event_archive: Option<crate::data_source::EventArchive>,
    /// ④ 统一映射账本：候选配对的唯一真值源（自动配对只提议、操作员
    /// confirm 才生效）。confirmed 的事件灌进 Engine（put_unified_mapping）
    /// 供策略与 unified.list 读；本字段持有持久态与审计线。
    unified_ledger: blitzkrieg_market_api::unified::MappingLedger,
    /// ④ 统一映射：最近一轮的跨所配对候选（自动匹配只提议）。每个新轮
    /// 全量替换；confirm 成功的配对离开货架。会话内状态，不持久化 ——
    /// 下一轮会重新提议，持久化只会留下过期货架。
    unified_candidates: Vec<blitzkrieg_market_api::unified::matching::MatchCandidate>,
    /// Per-strategy order/trade accounting (P-1.1). Session-scoped: reset on
    /// restart, like `stats`; the durable per-trade record is the trade log.
    ///
    /// Keyed (strategy, account) — PR H: two accounts assigned the same
    /// strategy are two independent scopes, and the panel reads the stats of
    /// the ACTIVE account. Key form `(strategy, '\u{1}', account)`; the
    /// single-name helpers below keep the call sites readable.
    strategy_accounting: HashMap<String, StrategyAccounting>,
    /// E-VA: the strategy-level VIRTUAL quota book — per-(account, strategy)
    /// pots fed by the batch pre-reservation and drained by the order state
    /// machine's terminal transitions. Built from `config.virtual_limits`;
    /// an empty config builds an empty book whose `governed` answer is
    /// always false, so every hook below is a one-comparison no-op on the
    /// shipped default (the replay-baseline posture).
    virtual_accounts: crate::risk::virtual_account::VirtualAccounts,
    /// Venue-rejection cooldowns keyed by placement intent (`internal_key`):
    /// after the venue refuses an order, retries of the SAME intent back off
    /// exponentially and stop after a cap — a stuck exit once burned 380+
    /// rejections hammering a venue that kept refusing at full tick rate.
    place_cooldowns: HashMap<String, RejectCooldown>,
    /// Orders whose escalation this process already reported as skipped because
    /// the cross the book offers is refused by the risk gate (#261). The sweep
    /// retries on every maker timeout, so without this the same unaffordable
    /// cross would reprint the same line every few seconds. An id leaves the
    /// set as soon as the order stops being live, or when a later escalation
    /// actually goes through.
    escalation_skips: HashSet<String>,
    /// Venue cancels the core has already committed to locally (order marked
    /// Cancelled by a regime pull, round cleanup, escalation or explicit
    /// cancel) while the venue may still hold the order resting. The live
    /// bridge drains this and issues the actual venue cancels — retiring an
    /// order locally without telling the venue leaves a resting orphan.
    /// Each entry carries the owning venue (read off the order token's round
    /// market at enqueue time; `""` = untagged) so the multi-plugin drain can
    /// hand every id to ITS venue's executor — the queue is take-once, and a
    /// sibling executor swallowing another venue's id would strand the cancel
    /// forever.
    pending_venue_cancels: Vec<(String, String)>,
    /// Consecutive LIVE venue rejections (a successful placement resets).
    /// Reaching the threshold trips the freeze (kill switch).
    consecutive_venue_rejects: u32,
    /// Consecutive LIVE venue rejections PER ACCOUNT. The freeze decision
    /// reads this map — one venue's outage freezes only the accounts bound
    /// to it, not the whole process. Reset alongside the global counter.
    consecutive_venue_rejects_by_account: HashMap<String, u32>,
    /// The account-scoped kill book: account id → freeze reason. A frozen
    /// account refuses NEW orders at the submission gate and drops entry
    /// candidates in the engine (mirrored via `set_account_frozen`), while
    /// its exits keep flowing — the same 「拒开仓、放平仓」 contract the
    /// trading switch has, raised automatically instead of by hand. The
    /// GLOBAL kill switch (`risk.kill`, `is_killed`) is unchanged and
    /// still outranks everything.
    account_freezes: HashMap<String, String>,
    /// 冻结自愈（2026-10-28 真线事故）：AUTO-frozen accounts and WHEN —
    /// account id → freeze timestamp (ms). Only the three automatic freeze
    /// routes (rejection streak, failed self-check, failed sweep streak)
    /// register here, so `auto_lift_if_venue_healthy` can tell "the kernel
    /// froze this on circumstantial evidence" from an operator's deliberate
    /// `account.freeze` — the latter must never self-lift. Same `&mut self`
    /// discipline as `account_freezes`: no lock, atomic by construction.
    auto_frozen: HashMap<String, i64>,
    /// Consecutive failed reconciliation sweeps (a successful sweep resets).
    /// Reaching the threshold freezes trading — a blind safety net is the
    /// failure mode that lets ghosts and orphans accumulate (E31-b).
    consecutive_sweep_failures: u32,
    /// Per-venue streak of the same (venue-scoped freeze, 2026-10-25): each
    /// plugin's sweep reports under its own name, so one venue's failures no
    /// longer ride the global counter nor freeze its siblings' accounts.
    consecutive_sweep_failures_by_venue: HashMap<String, u32>,
    /// Last error of any kind recorded this session (venue refusal, failed
    /// self-check or reconcile sweep, kill switch, kernel-side order refusal),
    /// surfaced to the panel as `engine.stats.lastError`. Session-scoped: a
    /// restart starts with no error, and nothing clears it but a new one.
    last_error: Option<LastError>,
    /// Newest external (unknown-order) fill timestamp already folded into the
    /// position book; persisted next to the position log.
    recon_watermark_ms: i64,
    /// Whether this core's boot REPLAYED durable cash journals (at least one
    /// `data/ledger/<account>.journal.jsonl` existed on disk). The server's
    /// post-construction seed reads it: a replayed balance is a boot fact and
    /// must not be re-seeded over. Always false when
    /// `config.ledger_journal_dir` is `None`.
    boot_replayed_journal: bool,
    /// Newest trading-capability self-check report (panel visibility).
    last_self_check: Option<blitzkrieg_market_api::SelfCheckReport>,
    /// Durable journal of the applied-fill idempotency table (issue #178). The
    /// OME never touches the filesystem; the service drains its mutation journal
    /// after every applied fill and appends it here, and restores the table from
    /// it at startup.
    applied_log: Option<AppliedFillLog>,
    /// In-kernel accounting audit state (issue #189).
    audit: AuditRuntime,
    /// Settlement + redemption book (issue #175): which positions a market
    /// resolution has settled (durable, so a second round cannot re-book them),
    /// and the claims whose collateral is still on-chain. In dry mode the core
    /// resolves its own markets and simulates the redemption; in live mode the
    /// venue plugin answers and redeems.
    settlement: SettlementBook,
    /// Last free-cash figure each venue reported, as venue → `(at_ms, free)` —
    /// the audit's venue leg reads ONLY the report of the venue the audited
    /// (active) account is bound to; a sibling venue's cash is a different
    /// book's truth.
    venue_free_by_venue: std::collections::HashMap<String, (i64, Decimal)>,
    /// The event archive's recording state as THIS core last saw it (issue
    /// #669): `true` = recording (and the boot assumption — an archive that
    /// opened stopped — still arms the edge below, because `stop()` may also
    /// have fired at open). On each tick the archive's live `status().recording`
    /// is folded through here, and a `true → false` edge emits ONE RiskAlert —
    /// the disk-floor/lock/io stop is otherwise a single journal warn line that
    /// a 24/7 operator finds days later as missing corpus. Staying false is
    /// silent (the alert fired once; the banner reads `stats.archive`), and a
    /// defensive `false → true` reset just re-arms the detector.
    archive_recording_last: bool,
    /// F4: close-accounting credentials for fill-driven full closes, keyed by
    /// the OME trade key. A MATCHED sell that emptied a position books TradeDb /
    /// strategy PnL / breaker immediately; if the venue later reports that same
    /// trade FAILED, the credential is what lets the service unwind each of
    /// those ledgers instead of leaving phantom profit behind. FIFO-capped:
    /// the FAILED normally arrives seconds after the fill that created it.
    ///
    /// #463: the queue is JOURNALED — every push appends a line to
    /// `close-credentials.jsonl` (beside the order log, same derivation as the
    /// other side logs) and every consumption appends a `consumed` line. A
    /// restart between the close fill and the venue's FAILED used to lose the
    /// credential (the position was already out of every recovery file) and
    /// strand phantom PnL; now boot replays the journal, keeping only lines
    /// whose key has no later `consumed`.
    close_credentials: std::collections::VecDeque<(String, CloseCredential)>,
    /// #463: where the credential journal appends. `None` (no order log — the
    /// backtester, most tests) keeps the queue memory-only, as before.
    close_credential_log: Option<std::path::PathBuf>,
    next_id: u64,
    tx: Option<mpsc::UnboundedSender<Event>>,
    /// E29 (§12.2): per-`(symbol, interval)` wall-clock (ms) of the LAST
    /// throttled push of that bucket's still-growing bar. Only the throttle
    /// state lives here — the bars live in the engine's aggregator; the map
    /// is touched only where an engine exists, so a core without one
    /// allocates nothing.
    kline_preview_last_ms: std::collections::HashMap<(String, crate::kline::KlineInterval), i64>,
    /// #363: the account-isolated execution policy. Loaded fail-closed at
    /// boot (a malformed file refuses the boot, with a `load-refused` audit
    /// line), replaced WHOLESALE on an IPC set/reset (hot reload — no
    /// restart), and consulted by `engine_evaluate` before an entry order is
    /// placed. `None` only when the config file is absent (the zero-config
    /// deployment): every default then matches the historical hardcoded
    /// behaviour bit for bit.
    execution_policy: Option<crate::execution_policy::Policy>,
    /// #363: account-level cooldowns a policy rule opened, keyed by account
    /// id, holding the ms instant entries may resume. Checked (and expired)
    /// in `engine_evaluate`; set by the same path when a rule answers
    /// `PolicyOutput::Cooldown`.
    policy_cooldowns: HashMap<String, i64>,
    /// Replay fast path (`backtest_fast`): tokens whose book changed in the
    /// latest `engine_on_data` or fill application — the only positions whose
    /// marks can move on the next maintenance tick, and the only cycles a
    /// Lua evaluation can change its output on (entry eligibility demands a
    /// fresh book). Consumed (cleared) by each maintenance tick.
    fast_dirty_tokens: HashSet<String>,
    /// Replay fast path: an engine INPUT arrived since the last real Lua
    /// evaluation (book / top-of-book / spot / round rollover) — the only
    /// cycles whose evaluation output can differ from the previous one.
    /// Consumed by `engine_evaluate`.
    fast_eval_wake: bool,
    /// Replay fast path: the next virtual instant a round-timing verdict can
    /// flip (min-round-age / min-time-left boundaries). Recomputed after every
    /// real evaluation and round rollover; quiet ticks skip the Lua call
    /// until this instant.
    fast_next_timing_ms: i64,
    /// Replay fast path: the live-order set changed (place / cancel / any
    /// position open or close) — wakes the escalation sweep and the audit-
    /// relevant sweeps that a quiet tick would otherwise skip.
    fast_orders_dirty: bool,
    /// Replay fast path: the open-position set changed — wakes the
    /// settlement watch sync (a new position's expiry must join the watch
    /// list when it passes, exactly when the full scan would have added it).
    fast_positions_dirty: bool,
    /// Replay fast path: earliest `escalate_at_ms` over live orders
    /// (`i64::MAX` = none). Recomputed whenever the escalation sweep runs.
    fast_next_escalation_ms: i64,
    /// Replay fast path: earliest `expires_at_ms` over open positions
    /// (`i64::MAX` = none) — when it passes, the settlement watch must sync
    /// even with no event in sight. Recomputed on position-set changes.
    fast_next_expiry_ms: i64,
    /// Replay fast path: virtual time of the last Lua evaluation. Quiet ticks
    /// skip the call until `FAST_EVAL_FALLBACK_MS` elapses (a safety net for
    /// strategies with internal time-based state; entry eligibility itself
    /// always rides the dirty-token wake).
    fast_last_eval_ms: i64,
    /// Issue #390 (replay nofast): the next virtual instant one of the
    /// every-tick maintenance passes whose OUTPUT the replay observes can
    /// change — the earliest open position's expiry passes (`TimeExit`/
    /// `ForceExit`/`StaleProfit`/`StagnantProfit`/settlement track, all of
    /// them clocks), or the 1 s throttle beats (suppressed-stop reports, held
    /// exits, accounting audit). The exit LADDER is scheduled against this
    /// deadline, dirty tokens, and its own 1 s report beat; everything else
    /// this flag covers already rides its own deadline in `tick`.
    /// `i64::MAX` = nothing scheduled.
    fast_next_exit_deadline_ms: i64,
    /// #531 pair-chase state: the chase this scope last offered for a naked
    /// leg, keyed (account, strategy, naked_token) → (missing_token, at_ms).
    /// The window reader (`run_pair_orphan_sweep`) is the only consumer — a
    /// chase placed within its window buys the leg the benefit of the doubt
    /// before the sweep's flatten fallback fires, and the record retires
    /// when the fallback does. Process-local (an in-memory debounce, not a
    /// durable fact): a restart simply re-judges from the book.
    pair_chase_attempts: HashMap<(String, String, String), (String, i64)>,
    /// B1c test seam: a staged (venue, duration) subscription set the
    /// admission gate reads instead of the (still empty, pre-B1d) real one.
    /// `None` outside tests and before staging — the gate then reads the
    /// constant empty set and admits everything (today's behaviour).
    #[cfg(test)]
    test_duration_subscriptions: Option<HashSet<(String, i64)>>,
}

/// F4: what a fill-driven full close changed, so a later FAILED status for the
/// same trade can reverse every one of those ledgers exactly.
///
/// #463: `Serialize`/`Deserialize` because the credential is journaled — the
/// queue alone dies with the process, and a restart between the close fill and
/// the venue's FAILED is precisely the window this struct exists for.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloseCredential {
    /// The position-book row as it stood BEFORE the closing fill removed it —
    /// the exact inventory to restore.
    restored: crate::position::OpenPosition,
    /// The closed record `on_position_closed` booked (TradeDb / accounting /
    /// breaker / daily PnL), for unwinding.
    closed: crate::position::ClosedPosition,
    /// The strategy breaker's consecutive-loss count just before this close
    /// was recorded: a reversed WIN had reset the streak, and that is the only
    /// recoverable value (retract restores it).
    streak_before: u32,
    /// E26 (§4.2): the ACCOUNT-level streak's count just before this close —
    /// the same recovery fact for the reserved `__account__:` key. Captured
    /// even when the account bound is off (a zero count is the truth then),
    /// so a close cannot choose whether to leave evidence.
    account_streak_before: u32,
}

/// #463: one journal line for the credential log — a push (with the credential)
/// or a consumption tombstone (key only). Boot replays pushes whose key has no
/// later tombstone.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CloseCredentialLine {
    Push {
        trade_id: String,
        #[serde(flatten)]
        cred: Box<CloseCredential>,
    },
    Consumed {
        trade_id: String,
    },
}

/// Replay fast path: a quiet tick still re-runs the Lua evaluation at least
/// this often (virtual ms), as the safety net for strategies whose decisions
/// read the round clock in ways the timing boundary does not model. The
/// boundary wake (`fast_next_timing_ms`) and the dirty-input wake cover the
/// proven inputs; this bounds anything else to a 2 s staleness.
const FAST_EVAL_FALLBACK_MS: i64 = 2_000;
/// Issue #390 (replay nofast): the exit ladder re-runs at least this often on
/// quiet ticks. Bounds every THROTTLED report the ladder feeds — suppressed
/// stops (`stop_suppression_repeat_sec` in shipped configs is 1 s+; the
/// throttle itself decides what is emitted) and the held-exit / escalation
/// reports (`check_exits` stamps them from its own run) — so their
/// time-quantised stamps stay exactly what the every-tick pass produced.
const EXIT_REPORT_BEAT_MS: i64 = 1_000;

impl Core {
    /// B1b: the round-length family a token belongs to — the market's declared
    /// duration when it carries one (#354), else the configured global grid.
    /// `0` = the token belongs to no live market AND the grid is unset (no
    /// family fact anywhere); every B1b write path stamps this, and the B1a
    /// readers (`effective_round_duration_sec`) treat a 0 as "no clock fact".
    fn round_family_of_token(&self, token_id: &str) -> i64 {
        let global = self.config.round_duration_sec;
        match self
            .engine
            .as_ref()
            .and_then(|e| e.market_for_token(token_id))
            .map(|m| m.round_duration_sec)
        {
            Some(d) if d > 0 => d,
            _ => global,
        }
    }

    /// B1b: the round-length family ONE order was admitted under, resolved
    /// three-state through B1a's `effective_round_duration_sec` (read only):
    /// the order's OWN stamp when it carries one, else the market's declared
    /// duration while the engine still names the token's market, else the
    /// configured global grid. `0` = no clock fact at all — the caller's gate
    /// decides what that means.
    ///
    /// Single-family discipline: with every stamp 0 (pre-B1b archives) and
    /// every market declaring the configured grid (or 0, the legacy corpora),
    /// this folds to `self.config.round_duration_sec` — the value every
    /// consumer read before B1b, byte-for-byte.
    fn order_eff_round_sec(&self, o: &TrackedOrder) -> i64 {
        let market_decl = self
            .engine
            .as_ref()
            .and_then(|e| e.market_for_token(&o.token_id))
            .map(|m| m.round_duration_sec);
        o.effective_round_duration_sec(market_decl, self.config.round_duration_sec)
    }

    /// B1b: the round-length family ONE position was opened under — the
    /// position-shaped twin of [`Core::order_eff_round_sec`]: same tri-state
    /// fold through B1a's `effective_round_duration_sec`, same single-family
    /// equivalence (eff == `self.config.round_duration_sec` when every stamp
    /// is 0 and no market contradicts the grid).
    fn position_eff_round_sec(&self, p: &OpenPosition) -> i64 {
        let market_decl = self
            .engine
            .as_ref()
            .and_then(|e| e.market_for_token(&p.token_id))
            .map(|m| m.round_duration_sec);
        p.effective_round_duration_sec(market_decl, self.config.round_duration_sec)
    }

    /// The open-position snapshot strategies see through `bk.holdings()`,
    /// rebuilt once per evaluation cycle (the #202 equity push's position
    /// sibling). Flattened to wire strings here — the engine slices it per
    /// strategy, so a package never reads another package's book. The round
    /// slot is re-derived from the entry time on the position's OWN family
    /// grid (B1b: the tri-state eff — own stamp > market declaration >
    /// global grid, floored at 1 — so a legacy unstamped row whose market
    /// declares a different family still projects onto the family the
    /// market actually trades; a single-family deployment folds eff == cfg,
    /// the historic formula byte-for-byte); a position cannot outlive its
    /// round, so the exact boundary the entry sat on is all the gap-fill
    /// loop needs.
    fn held_position_view(&self) -> Vec<crate::strategies::HeldPosition> {
        self.positions
            .open_positions()
            .iter()
            .map(|p| {
                let duration = self.position_eff_round_sec(p).max(1);
                crate::strategies::HeldPosition {
                    strategy: p.strategy.clone(),
                    account_id: p.account_id.to_string(),
                    condition_id: p.condition_id.clone(),
                    token_id: p.token_id.clone(),
                    direction: p.direction.as_str().to_string(),
                    shares: p.shares.to_string(),
                    entry_price: p.entry_price.to_string(),
                    round_slot: p.entered_at_ms / 1000 / duration,
                }
            })
            .collect()
    }

    /// Issue #390: whether THIS core runs the replay maintenance scheduling —
    /// the deadline-driven gates the fast path pioneered, applied to the
    /// nofast replay. `backtest_fast` implies it (the fast path is the same
    /// scheduling plus the all-holder skip); the nofast replay opts in
    /// through `replay_scheduling`; a live kernel never does.
    fn replay_sched(&self) -> bool {
        self.config.backtest_fast || self.config.replay_scheduling
    }

    /// Issue #390 (replay deadline jump): the earliest virtual instant any
    /// time-driven kernel gate can next produce output, when the replay
    /// scheduler is in force. `None` on a live kernel or an unconfigured
    /// clock — the caller must then keep stepping every tick. The driver
    /// lands its grid on this instant (clamped forward to the grid), runs
    /// the tick, and RE-READS the wake: every step re-arms at least one
    /// deadline here, so the loop cannot stall and never needs to guess
    /// when a gate's deadline moves.
    ///
    /// What must NOT appear here, by design:
    ///   * engine inputs (books/tops/rounds/verdicts) — they are events, and
    ///     the driver stops on the first grid tick after every event batch
    ///     regardless of this clock;
    ///   * strategy-signal exit consumption — intents enqueued by an
    ///     evaluation are consumed by the NEXT tick's exit pass by
    ///     construction (the driver forces the next grid tick after any
    ///     evaluation that produced one), never a scheduled wake;
    ///   * place/policy cooldowns and backoffs — submission-path checks on
    ///     the place call itself, never a timer sweep.
    pub fn next_replay_wake_ms(&self, now_ms: i64) -> Option<i64> {
        if !self.replay_sched() {
            return None;
        }
        // `now_ms` arrives as (next grid point - 1): a ">=" gate due exactly
        // at the next grid point must land THERE, so the probe filters on
        // t > now_ms (t >= next grid point), not t > next.
        let mut best = i64::MAX;
        let consider = |t: i64, best: &mut i64| {
            // `i64::MAX` (and the "never armed" 0, superseded by the first
            // tick's own re-arm) is no wake — the gate's owner armed nothing.
            if t > now_ms && t < *best && t != i64::MAX {
                *best = t;
            }
        };
        consider(self.fast_next_timing_ms, &mut best);
        // The eval fallback fires at the first grid tick at or after
        // last_eval + window — the exact tick the every-tick walk used.
        consider(
            self.fast_last_eval_ms.saturating_add(FAST_EVAL_FALLBACK_MS),
            &mut best,
        );
        consider(self.fast_next_exit_deadline_ms, &mut best);
        consider(self.fast_next_escalation_ms, &mut best);
        consider(self.fast_next_expiry_ms, &mut best);
        consider(self.settlement.next_query_deadline_ms(), &mut best);
        consider(self.settlement.next_redemption_ms(), &mut best);
        for cd in self.place_cooldowns.values() {
            consider(cd.next_ok_ms, &mut best);
        }
        for until in self.policy_cooldowns.values() {
            consider(*until, &mut best);
        }
        for leg in &self.inflight_takers {
            consider(leg.execute_at_ms, &mut best);
            consider(leg.report_at_ms, &mut best);
        }
        consider(self.breaker.next_resume_ms(), &mut best);
        consider(
            self.audit
                .last_at_ms
                .checked_add(self.config.audit_interval_sec.max(0) * 1000)
                .unwrap_or(i64::MAX),
            &mut best,
        );
        consider(
            (crate::position::utc_day_index(now_ms) + 1) * crate::position::DAY_MS,
            &mut best,
        );
        if best == i64::MAX { None } else { Some(best) }
    }

    /// Issue #390 (replay deadline jump): the `evaluations` feed counter
    /// advances once per scheduled cycle (tick) even when the cycle's Lua
    /// evaluation is skipped — the sparse-cadence contract pins the SCHEDULED
    /// count, not the wall cost. A driver that lands on a subset of the grid
    /// must credit the cycles it did not run, or the report's feed block
    /// (and the tests that pin it) see a count that disagrees with the
    /// cycle schedule the replay simulated.
    pub fn replay_credit_cycles(&mut self, cycles: u64) {
        // `engine_evaluate` counts only when an engine is installed; the
        // credit must mirror that exactly or engine-less sweeps would see a
        // counter the base never produced.
        if self.engine.is_some() {
            self.stats.evaluations = self.stats.evaluations.saturating_add(cycles);
        }
    }

    /// Issue #390 (replay deadline jump): true when the driver may land on a
    /// subset of the maintenance grid. Two disqualifiers: replay scheduling
    /// itself is off (the every-tick walk IS the contract), or Shadow
    /// Evolution is enabled — its per-tick evaluation cadence is part of its
    /// observed behaviour, so a jumping driver must not skip cycles under it.
    pub fn replay_jump_allowed(&self) -> bool {
        self.replay_sched() && !self.shadow_evolution.is_enabled()
    }

    /// Issue #390 (replay deadline jump): true when THIS tick left state
    /// that the immediately following grid tick consumes — strategy exit
    /// intents awaiting their consuming exit pass, or a dirty flag
    /// (orders/positions) a scheduled pass would have observed there. The
    /// jumping driver forces that landing; skipping it could move a stamp
    /// or a verdict the every-tick walk produced.
    pub fn has_forced_next_tick(&self) -> bool {
        !self.strategy_exits.is_empty() || self.fast_orders_dirty || self.fast_positions_dirty
    }

    /// Issue #390 (replay nofast): true when the standing-candidate cache
    /// holds a suggestion the driver must not skip over — a skipped grid tick
    /// would not re-run it into the kernel-side gates the way the base walk
    /// did every tick (see `engine_replay_cycle`).
    pub fn has_standing_candidates(&self) -> bool {
        self.engine
            .as_ref()
            .is_some_and(|e| e.has_standing_candidates())
    }
}

/// ④ 统一映射：一个 listing 的面板形状（`unified.candidates` / confirm
/// 请求体共用）。`PlatformListing` 本身可 serde，但表单字段名直接固化在
/// 这里，避免把 Rust 内部字段漂移暴露成协议。
fn listing_json(l: &blitzkrieg_market_api::unified::PlatformListing) -> serde_json::Value {
    serde_json::json!({
        "venue": l.venue,
        "conditionId": l.condition_id,
        "upTokenId": l.up_token_id,
        "downTokenId": l.down_token_id,
        "asset": l.asset,
        "title": l.title,
        "settlementRules": l.settlement_rules,
        "settlementSource": l.settlement_source,
        "expiresAtMs": l.expires_at_ms,
        "status": l.status,
    })
}

/// ④ ConfirmError has no Display (the ledger crate keeps refusals as data);
/// the IPC layer needs one operator-facing line per variant.
fn confirm_error_text(e: blitzkrieg_market_api::unified::ledger::ConfirmError) -> String {
    use blitzkrieg_market_api::unified::ledger::ConfirmError;
    match e {
        ConfirmError::NeedsTwoVenues => {
            "confirm refused: a mapping needs at least two distinct venues".to_string()
        }
        ConfirmError::DuplicateVenue(v) => {
            format!(
                "confirm refused: two listings from the same venue ({})",
                v.as_str()
            )
        }
        ConfirmError::TitleTooDissimilar { a, b, similarity } => {
            format!("confirm refused: titles too dissimilar ({similarity:.2}) between {a} and {b}")
        }
        ConfirmError::AssetMismatch {
            a,
            b,
            a_asset,
            b_asset,
        } => format!(
            "confirm refused: different assets ({a_asset} vs {b_asset}) between {a} and {b}"
        ),
        ConfirmError::AlreadyMapped {
            venue,
            condition_id,
            event_id,
        } => format!(
            "confirm refused: {} listing {condition_id} is already mapped in event {event_id}",
            venue.as_str()
        ),
    }
}

impl Core {
    pub fn new(config: CoreConfig) -> Self {
        // E28 (§9.5): the account book loads from `user_layer/configs/
        // accounts.toml` relative to the process cwd — a missing file is the
        // zero-config 0.2 deployment (one `default` account), a MALFORMED one
        // refuses the boot (fail-closed: a book that is not what the operator
        // wrote must not trade). The credential probe runs here too, so
        // `account.list` answers `credentialsLoaded` from boot-time facts.
        // Tests (and embedders) inject a book directly via
        // [`Core::with_account_book`] instead of touching the cwd.
        let accounts = crate::account::config::load_account_ledgers(
            std::path::Path::new(crate::account::config::ACCOUNTS_CONFIG_PATH),
            now_ms(),
            &config.allowed_round_secs,
        )
        .unwrap_or_else(|e| {
            panic!(
                "refusing to boot: {} (fail-closed: fix accounts.toml and restart)",
                e.message
            )
        });
        Self::with_account_book(config, accounts)
    }

    /// The constructor with the account book SUPPLIED (E28): a multi-account
    /// book for tests without touching the real `user_layer/configs/
    /// accounts.toml` the process cwd decides. Production always enters
    /// through [`Core::new`], which fails closed on a malformed file; this
    /// variant trusts the caller's book.
    pub fn with_account_book(
        config: CoreConfig,
        mut accounts: crate::account::AccountLedgers,
    ) -> Self {
        // Whether the boot below replayed durable cash journals (the server's
        // post-construction seed reads it — a replayed balance must not be
        // re-seeded over).
        let mut boot_replayed_journal = false;
        // #363: the execution policy loads fail-closed at boot. A missing
        // file is the zero-config deployment (code defaults, no account
        // sections); a file that exists but does not parse, misses a field
        // or carries an out-of-range value REFUSES the boot — and leaves one
        // `load-refused` audit line saying so (谁改的/何时/为何, the audit's
        // own first question). A config-path of `None` (backtester, most
        // tests) loads nothing: every default then equals the historical
        // hardcoded behaviour bit for bit.
        //
        // #380: the DEFAULT path in [`CoreConfig::default`] is a SPELLING,
        // not a location — the binary entry (`main.rs`) resolves it against
        // the install root before constructing the config, and logs the
        // resolution (found / neutral-kernel WARN) there. The library
        // honours whatever path it is handed, verbatim, so an embedder (and
        // every in-process test) keeps exactly the semantics it asked for.
        let execution_policy = Self::load_execution_policy_at_boot(&config);
        // #363: the per-asset cap precedence — CLI flag > policy section >
        // compiled 1. The CLI already writes total `max_positions` straight
        // into `config.positions`; this mirrors that for per-asset, reading
        // the value back out of the LOADED policy so the file and the flag
        // cannot disagree silently. A config-path of `None` (or a missing
        // file, which loads the code defaults) leaves the compiled 1 — the
        // historical one-per-asset gate, byte for byte.
        let mut positions = config.positions.clone();
        if let Some(policy) = &execution_policy {
            positions.max_positions_per_asset = policy.defaults.max_positions_per_asset;
        }
        if let Some(v) = config.max_positions_per_asset {
            positions.max_positions_per_asset = v;
        }
        let trade_db = config
            .trade_log_path
            .as_ref()
            .map(crate::trade_db::TradeDb::new);
        let order_db = config
            .order_log_path
            .as_ref()
            .map(crate::order_db::OrderDb::new);
        let position_db = config
            .position_log_path
            .as_ref()
            .map(crate::position_db::PositionDb::new);
        // Both side-log paths are derived from the order log's directory when not
        // set explicitly: they belong to the same durable state, and a deployment
        // that persists orders wants its idempotency table and audit trail on
        // disk too. `order_log_path: None` (the backtester, most tests) means no
        // persistence for any of them.
        let side_log_path = |explicit: &Option<String>, name: &str| -> Option<String> {
            if let Some(p) = explicit {
                return Some(p.clone());
            }
            let dir = std::path::Path::new(config.order_log_path.as_deref()?).parent()?;
            Some(dir.join(name).to_string_lossy().to_string())
        };
        let applied_log =
            side_log_path(&config.applied_log_path, "applied-fills.jsonl").map(AppliedFillLog::new);
        // ④ 统一映射账本：有路径就加载持久态（候选→人工确认的映射跨重启
        // 存活），无路径（回测/进程内测试）就纯内存。自动配对只提议，
        // confirm 才写账本 —— ledger 的设计合同，接线必须遵守。
        let unified_ledger = match config.unified_ledger_path.as_deref() {
            Some(p) => blitzkrieg_market_api::unified::MappingLedger::load(std::path::Path::new(p)),
            None => blitzkrieg_market_api::unified::MappingLedger::new(),
        };
        let audit_log = side_log_path(&config.audit_log_path, "reconcile.jsonl");
        let settlement_log = side_log_path(&config.settlement_log_path, "settlements.jsonl");
        // #463: the close-credential journal rides the same derivation — a
        // deployment that persists the order log persists the credentials that
        // let a FAILED unwind one of its closes across a restart.
        let close_credential_log =
            side_log_path(&None, "close-credentials.jsonl").map(std::path::PathBuf::from);
        let settlement = SettlementBook::new(settlement_log.as_deref().map(std::path::Path::new));
        let shadow_cfg = {
            let mut c = ShadowEvolutionConfig {
                enabled: config.shadow_evolution_enabled,
                ..Default::default()
            };
            if let Some(t) = &config.shadow_evolution_tuning {
                if let Some(v) = t.min_sample_count {
                    c.min_sample_count = v;
                }
                if let Some(v) = t.min_win_rate_improvement {
                    c.min_win_rate_improvement = v;
                }
                if let Some(v) = t.min_profit_factor_improvement {
                    c.min_profit_factor_improvement = v;
                }
                if let Some(v) = t.min_observation_secs {
                    c.min_observation_secs = v;
                }
                if let Some(v) = t.cooldown_secs {
                    c.cooldown_secs = v;
                }
                if let Some(v) = t.variant_count {
                    c.variant_count = v;
                }
                if let Some(v) = t.evaluation_window_secs {
                    c.evaluation_window_secs = v;
                }
                if let Some(v) = t.max_gradient {
                    // Belt and braces: the caller clamps too, but this is the one
                    // place the lock could be widened, so it re-checks.
                    c.max_gradient = v.min(crate::config::MAX_GRADIENT_CEILING);
                }
                if let Some(v) = &t.audit_dir {
                    c.audit_dir = v.clone();
                }
                if let Some(v) = t.auto_evolve {
                    c.auto_evolve = v;
                }
                if let Some(v) = t.evolution_cycle_secs {
                    c.evolution_cycle_secs = v;
                }
                if let Some(v) = t.proposal_ttl_secs {
                    c.proposal_ttl_secs = v;
                }
                if let Some(v) = t.deep_dims {
                    c.deep_dims = v.max(1);
                }
            }
            // D-2: variant exits must replay the SAME policy the live position
            // manager runs, or the counterfactual is judged against an exit
            // mechanism the live path never uses.
            c.exit_cfg = config.positions.exit.clone();
            c
        };
        let breaker = LossBreakers::new(config.max_consecutive_losses, config.breaker_cooldown_sec);
        let positions = PositionManager::new(positions);
        // Optional market-data archive (P-1.3). A failure to open only disables
        // recording — trading must never be blocked by an archive path problem.
        let event_archive = config.event_archive_path.as_ref().and_then(|p| {
            let cap_bytes = config.event_archive_max_mb.saturating_mul(1024 * 1024);
            let rotate_bytes = config.event_archive_rotate_mb.saturating_mul(1024 * 1024);
            let min_free_bytes =
                config.event_archive_min_free_mb.saturating_mul(1024 * 1024);
            match crate::data_source::EventArchive::open_full(
                std::path::Path::new(p),
                cap_bytes,
                rotate_bytes,
                min_free_bytes,
            ) {
                Ok(a) => {
                    tracing::info!(
                        path = %p,
                        cap_mb = config.event_archive_max_mb,
                        rotate_mb = config.event_archive_rotate_mb,
                        min_free_mb = config.event_archive_min_free_mb,
                        "recording market-data archive"
                    );
                    Some(a)
                }
                Err(e) => {
                    tracing::warn!(path = %p, error = %e, "cannot open event archive; recording disabled");
                    None
                }
            }
        });
        // Durable cash journals (data/ledger/<account>.journal.jsonl): replay
        // BEFORE seeding. A deployment with journals on disk restores the
        // opening balance from each journal's last row and skips the boot seed
        // — re-seeding over a replay would reset the balance to the seed and
        // vaporize the盈亏 history across every restart. A fresh deployment
        // (no journal files — first boot, or a replay corpus) takes the
        // historical seed_all path, bit for bit. A malformed journal line is
        // skipped with one warn (the jsonl::load contract — the trailing
        // balance comes from the last GOOD row); the file itself is append-
        // only evidence, never rewritten by this path.
        if let Some(dir) = config.ledger_journal_dir.as_deref() {
            let dir = std::path::Path::new(dir);
            let replayed = accounts.any_journal_on_disk(dir);
            accounts.attach_journals(dir);
            // E28 (§9.5): a locally-settling mode has no venue to reconcile
            // against; seed the local cash so the reserve/overspend gate is
            // meaningful for embedders that don't seed it. Uses the predicate,
            // not `== Dry`: the equality form is invisible to the compiler and
            // left ReadOnly funding at zero. EVERY account is seeded (§9.3:
            // the books are separate wallets, not one wallet with aliases);
            // with one account this is exactly the single `seed` 0.2 ran.
            // Skipped when a journal replayed: the balance is a boot-replay
            // fact now, not the seed.
            if config.mode.settles_locally() && !replayed {
                accounts.seed_all(config.dry_seed_balance);
            }
            boot_replayed_journal = replayed;
        } else if config.mode.settles_locally() {
            // E28 (§9.5): a locally-settling mode has no venue to reconcile
            // against; seed the local cash so the reserve/overspend gate is
            // meaningful for embedders that don't seed it. Uses the predicate,
            // not `== Dry`: the equality form is invisible to the compiler and
            // left ReadOnly funding at zero. EVERY account is seeded (§9.3:
            // the books are separate wallets, not one wallet with aliases);
            // with one account this is exactly the single `seed` 0.2 ran.
            accounts.seed_all(config.dry_seed_balance);
        }
        let mut intent_audit = crate::arbitration::AuditSink::new(accounts.active_id().as_str());
        intent_audit.enabled = config.intent_audit_enabled;
        // 跨所套利弹窗 v2: the leg configuration reads from the SAME
        // accounts.toml the book came from (when a path is configured). The
        // FILE is validated at the parse layer (from_str refuses an
        // incoherent section), so a read here after a successful book load
        // cannot fail semantically — the unwrap-style default below is
        // reachable only when the file vanished between the two reads, and
        // then "unconfigured" is the honest answer.
        let arb_leg_config = config
            .accounts_config_path
            .as_deref()
            .map(std::path::Path::new)
            .and_then(|p| {
                crate::account::config::read_cross_venue_arb(p)
                    .map_err(|e| {
                        tracing::warn!(
                            error = %e.message,
                            "cross_venue_arb section unreadable at boot — running unconfigured"
                        );
                    })
                    .ok()
                    .flatten()
            })
            .inspect(|cfg| {
                tracing::info!(
                    leg_a = %format!("{}/{}", cfg.leg_a_venue, cfg.leg_a_account),
                    leg_b = %format!("{}/{}", cfg.leg_b_venue, cfg.leg_b_account),
                    pairs = ?cfg.pairs,
                    "cross-venue arb legs configured (accounts.toml [cross_venue_arb])"
                );
            });
        // E-VA: the virtual pots are built from the config's caps BEFORE the
        // config moves into the struct (the caps are copied, so a later
        // runtime config mutation cannot move a quota mid-session).
        let virtual_accounts =
            crate::risk::virtual_account::VirtualAccounts::new(&config.virtual_limits);
        // The shadow manager reads the write-back path off the config before
        // the config moves into the struct (same ordering concern as above).
        let shadow_config_path = config.shadow_config_path.clone();
        Self {
            risk: RiskGate::new(config.risk.clone()),
            config,
            ome: Ome::new(),
            inflight_takers: Vec::new(),
            accounts,
            breaker,
            intent_audit,
            positions,
            books: HashMap::new(),
            exit_reasons: HashMap::new(),
            exit_reason_order: VecDeque::new(),
            exit_reasons_evicted: 0,
            exit_reasons_swept: 0,
            strategy_exits: Vec::new(),
            engine: None,
            strategy_declarations: HashMap::new(),
            plugin_modes: Vec::new(),
            feeds: std::collections::BTreeMap::new(),
            extensions: {
                use crate::extension::Extension;
                let mut reg = crate::extension::ExtensionRegistry::new();
                // Built-in example extension (installed, not enabled by default).
                // Its config file is read and checked against the registered
                // extension (KI-11): the file is no longer inert, and a config
                // that has drifted from the code is reported at startup.
                let ext = crate::extension::builtins::BinanceSpotExtension::new();
                let path =
                    crate::extension::config_path_for(crate::extension::EXTENSIONS_DIR, ext.name());
                let cfg = crate::config::ExtensionConfig::load(&path);
                for w in &cfg.warnings {
                    tracing::warn!(extension = ext.name(), "extension config: {w}");
                }
                for k in &cfg.unknown_keys {
                    tracing::warn!(
                        extension = ext.name(),
                        key = k,
                        "extension config key not understood (ignored)"
                    );
                }
                for d in &cfg.declared_only {
                    tracing::info!(
                        extension = ext.name(),
                        key = %d.key,
                        reason = d.reason,
                        "extension config declares a setting the kernel does not act on"
                    );
                }
                let kind = format!("{:?}", ext.extension_type()).to_lowercase();
                for problem in cfg.check_against(ext.name(), ext.version(), &kind) {
                    tracing::warn!(extension = ext.name(), "extension config: {problem}");
                }
                reg.install(Box::new(ext), cfg.path);
                reg
            },
            shadow_evolution: ShadowEvolution::new_with_config_path(
                shadow_cfg,
                &[],
                shadow_config_path,
            ),
            trade_db,
            order_db,
            position_db,
            stats: CoreStats::default(),
            // 探针从零开始：没有 tick/spot/NTP 历史（与 stats 同一生命期，
            // 会话作用域）。
            probe_ticks: crate::probe::TickHealth::default(),
            probe_spot: crate::probe::SpotWatch::default(),
            probe_ntp: crate::probe::NtpWatch::default(),
            probe_last_window_ticks: 0,
            probe_last_zero: Vec::new(),
            probe_last_max_silence_ms: 0,
            probe_last_spot_stale_ms: None,
            stats_probe_stream_breaks: 0,
            stats_probe_spot_stales: 0,
            archive_verdicts: HashSet::new(),
            // The session clock for the accounting identity (issue #200). Read
            // once, at construction: the ledger below is seeded at the same
            // instant, so "cash since the seed" and "trades since startedAtMs"
            // describe the same window.
            started_at_ms: now_ms(),
            event_archive,
            strategy_accounting: HashMap::new(),
            virtual_accounts,
            place_cooldowns: HashMap::new(),
            escalation_skips: HashSet::new(),
            pending_venue_cancels: Vec::new(),
            consecutive_venue_rejects: 0,
            consecutive_venue_rejects_by_account: HashMap::new(),
            account_freezes: HashMap::new(),
            auto_frozen: HashMap::new(),
            consecutive_sweep_failures: 0,
            consecutive_sweep_failures_by_venue: HashMap::new(),
            last_error: None,
            recon_watermark_ms: 0,
            boot_replayed_journal,
            last_self_check: None,
            applied_log,
            unified_ledger,
            unified_candidates: Vec::new(),
            audit: AuditRuntime {
                audit_log_path: audit_log,
                ..Default::default()
            },
            settlement,
            venue_free_by_venue: std::collections::HashMap::new(),
            archive_recording_last: true,
            close_credentials: std::collections::VecDeque::new(),
            close_credential_log,
            next_id: 1,
            tx: None,
            kline_preview_last_ms: std::collections::HashMap::new(),
            // #363: the boot-loaded policy (None = no config file found).
            execution_policy,
            policy_cooldowns: HashMap::new(),
            // Replay fast path: nothing dirty, no deadlines known yet — the
            // first tick always runs the full sweeps (MAX stamps force them).
            fast_dirty_tokens: HashSet::new(),
            fast_eval_wake: true,
            fast_next_timing_ms: 0,
            fast_orders_dirty: true,
            fast_positions_dirty: true,
            fast_next_escalation_ms: 0,
            fast_next_expiry_ms: 0,
            fast_last_eval_ms: 0,
            // Issue #390 (nofast): 0 = due on the first tick.
            fast_next_exit_deadline_ms: 0,
            pair_chase_attempts: HashMap::new(),
            #[cfg(test)]
            test_duration_subscriptions: None,
            // 跨所套利弹窗 v2: the boot-loaded leg configuration (memory
            // mirror of accounts.toml's `[cross_venue_arb]` — the file is the
            // durable truth, this cell feeds the engine hot). None =
            // unconfigured: the legacy unified-confirm chain stays the only
            // pairing source and the account-stamp is inert.
            arb_leg_config,
            // RFC 568 阶段一：配对簿随进程启动为空账（不持久化 —— 事实事件
            // 重放经同一事件流重建，#559 3c 的口径见 pair 模块注释）。
            pair_book: crate::pair::PairBook::default(),
        }
    }

    /// #363: the boot-time policy load. Fail-closed on ANY malformed input;
    /// the refusal is both panicked (the boot must not continue) and audited
    /// (`load-refused`), so an operator who walks up to a dead kernel finds
    /// the reason in the journal, not only in a scrolled-away log line.
    fn load_execution_policy_at_boot(
        config: &CoreConfig,
    ) -> Option<crate::execution_policy::Policy> {
        let path = config
            .execution_policy_path
            .as_deref()
            .map(std::path::Path::new)?;
        match crate::execution_policy::Policy::load(path) {
            Ok(policy) => Some(policy),
            Err(reason) => {
                crate::execution_policy::append_audit_to(
                    &Self::execution_policy_audit_file(config),
                    &crate::execution_policy::PolicyAuditRecord {
                        ts_ms: now_ms(),
                        actor: crate::execution_policy::ACTOR_BOOT.to_string(),
                        action: "load-refused".to_string(),
                        account_id: "*".to_string(),
                        before: None,
                        after: None,
                        error: Some(reason.clone()),
                    },
                );
                panic!(
                    "refusing to boot: {reason} (fail-closed: fix execution_policy.toml and \
                     restart)"
                );
            }
        }
    }

    // ── #363: execution policy surface (IPC + reload) ──────────────────────

    /// One section as the wire reports it: the EFFECTIVE view (own overrides
    /// folded over the globals), decimals as strings so the shortest-decimal
    /// rule survives the JSON hop.
    pub fn execution_policy_section_view(
        &self,
        account_id: &str,
    ) -> Option<crate::ipc::schema::ExecutionPolicySectionView> {
        let policy = self.execution_policy.as_ref()?;
        let s = policy.section_for(account_id);
        Some(crate::ipc::schema::ExecutionPolicySectionView {
            budget_ratio: Some(s.budget_ratio.to_string()),
            min_budget_usd: Some(s.min_budget_usd.to_string()),
            max_budget_usd: Some(s.max_budget_usd.to_string()),
            min_equity_usd: Some(s.min_equity_usd.to_string()),
            max_positions_per_asset: s.max_positions_per_asset,
            rules: Some(
                s.rules
                    .iter()
                    .map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null))
                    .collect(),
            ),
        })
    }

    /// The account ids the loaded policy names with a section of their own.
    pub fn execution_policy_account_ids(&self) -> Vec<String> {
        match &self.execution_policy {
            Some(p) => {
                let mut ids: Vec<String> = p.accounts.keys().cloned().collect();
                ids.sort();
                ids
            }
            None => Vec::new(),
        }
    }

    /// The journal file this config's execution-policy audit lines land in
    /// (#386): the config's private override, or the shipped
    /// `data/audit/execution_policy.jsonl` spelling when unset.
    fn execution_policy_audit_file(config: &CoreConfig) -> std::path::PathBuf {
        config
            .execution_policy_audit_path
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(crate::execution_policy::EXECUTION_POLICY_AUDIT_PATH)
            })
    }

    /// Whether the policy is loaded at all (the zero-config deployment has
    /// none — the read arms report that instead of inventing a section).
    pub fn execution_policy_loaded(&self) -> bool {
        self.execution_policy.is_some()
    }

    /// Append one audit line to THIS core's journal (#386). The wire set/
    /// reset arms and the reload refusal all land through here, so a test
    /// core with a private journal never touches the checkout's data dir.
    pub fn execution_policy_append_audit(
        &self,
        record: &crate::execution_policy::PolicyAuditRecord,
    ) -> bool {
        crate::execution_policy::append_audit_to(
            &Self::execution_policy_audit_file(&self.config),
            record,
        )
    }

    /// The audit journal for the history arm — read from THIS core's own
    /// journal file (#386), not a process-wide fixed path.
    pub fn execution_policy_audit(&self) -> Vec<crate::execution_policy::PolicyAuditRecord> {
        crate::execution_policy::load_audit_from(&Self::execution_policy_audit_file(&self.config))
    }

    /// #364: replay the CURRENT section over the recent closed trades of one
    /// account — the read-only fact behind the UI's "these rules would skip
    /// N of the last M entries". Every historical close supplies the facts a
    /// live entry would carry (balance is the LIVE ledger, not a per-trade
    /// snapshot: a ledger is a book, not a tape — disclosed in the row, not
    /// pretended into history), and `evaluate` — the ONE verdict function —
    /// judges it. The preview can never disagree with the kernel because it
    /// does not re-implement the kernel.
    pub fn execution_policy_preview(
        &self,
        account_id: &str,
        limit: usize,
    ) -> crate::ipc::schema::ExecutionPolicyPreviewResult {
        use crate::execution_policy::PolicyOutput;
        let now = now_ms();
        // Closed trades of THIS account, oldest first, capped at `limit` —
        // the same rows `trades.history` reports, judged instead of shown.
        let mut closed: Vec<&crate::position::ClosedPosition> = self
            .positions
            .closed_positions()
            .iter()
            .filter(|c| c.account_id.as_str() == account_id)
            .collect();
        if closed.len() > limit {
            closed.drain(0..closed.len() - limit);
        }
        // The live book facts (the parts of PolicyInput that are not
        // per-trade): balance/equity from the account's own ledger, the
        // trailing-hour PnL, the open count, the strategy streaks.
        let balance = self
            .accounts
            .ledger_of(&blitzkrieg_market_api::AccountId::from(account_id))
            .map(|l| l.balance())
            .unwrap_or_default();
        let open_for = self
            .positions
            .open_positions()
            .iter()
            // The preview replays the entry gates: the count must match the
            // entry path's LIVE-only rule (expired rounds are settlement-dead).
            .filter(|p| p.account_id.as_str() == account_id && p.expires_at_ms > now)
            .count() as u32;
        let mut considered = 0usize;
        let mut skipped = 0usize;
        let mut budgets: Vec<Decimal> = Vec::new();
        let mut rows = Vec::with_capacity(closed.len());
        for c in &closed {
            considered += 1;
            let facts = crate::execution_policy::PolicyInput {
                available_balance: balance,
                total_equity: balance,
                open_positions: open_for,
                current_price: c.entry_price,
                // The close is long over; the round it belonged to has
                // resolved. Zero is the honest "no clock running" fact.
                time_left_sec: 0,
                symbol: c.asset.clone(),
                recent_pnl_1h: self.positions.recent_pnl_1h_for(account_id, now),
                consecutive_losses: self.breaker.consecutive_losses(&c.strategy),
            };
            let (verdict, detail) = match self
                .execution_policy
                .as_ref()
                .map(|p| crate::execution_policy::evaluate(p, account_id, &facts))
            {
                None => ("refused".to_string(), Some("no policy loaded".to_string())),
                Some(Err(refusal)) => ("refused".to_string(), Some(refusal.reason)),
                Some(Ok(PolicyOutput::Place { budget_usd, .. })) => {
                    budgets.push(budget_usd);
                    ("place".to_string(), Some(budget_usd.to_string()))
                }
                Some(Ok(PolicyOutput::Skip { reason })) => {
                    skipped += 1;
                    ("skip".to_string(), Some(reason))
                }
                Some(Ok(PolicyOutput::Cooldown { seconds })) => {
                    skipped += 1;
                    ("cooldown".to_string(), Some(format!("{seconds}s")))
                }
            };
            let symbol = facts.symbol.clone();
            rows.push(crate::ipc::schema::ExecutionPolicyPreviewRow {
                ts_ms: c.exited_at_ms,
                symbol,
                balance,
                price: c.entry_price,
                verdict,
                detail,
            });
        }
        // `in` conditions on symbols judge the asset the trade named; nothing
        // special-cased here — `evaluate` already handles every field.
        let avg_budget_usd = if budgets.is_empty() {
            None
        } else {
            let sum = budgets.iter().copied().fold(Decimal::ZERO, |a, b| a + b);
            let n = Decimal::from(budgets.len() as u64);
            // Shortest-decimal discipline applies to the WIRE (the `to_string`
            // below is what crosses IPC), not to the Decimal itself — no
            // `normalize()` here, it would shave 10.00 down to "10" and break
            // the money-string convention every other field keeps.
            Some((sum / n).to_string())
        };
        crate::ipc::schema::ExecutionPolicyPreviewResult {
            considered,
            skipped,
            avg_budget_usd,
            rows,
        }
    }

    /// Replace the in-memory policy wholesale after a file write (set/reset
    /// arms). Re-derives the per-asset cap the way boot did — CLI flag wins
    /// over the file — and lands one `load-refused` audit line + keeps the
    /// OLD policy when the new file does not parse: fail-closed means a
    /// broken write never trades unguarded, it keeps the last good guard.
    pub fn reload_execution_policy(&mut self) -> Result<(), String> {
        let path = self
            .config
            .execution_policy_path
            .clone()
            .ok_or_else(|| "execution policy disabled (no config path)".to_string())?;
        match crate::execution_policy::Policy::load(std::path::Path::new(&path)) {
            Ok(policy) => {
                let mut positions = self.config.positions.clone();
                positions.max_positions_per_asset = policy.defaults.max_positions_per_asset;
                if let Some(v) = self.config.max_positions_per_asset {
                    positions.max_positions_per_asset = v;
                }
                self.positions.set_config(positions);
                self.execution_policy = Some(policy);
                Ok(())
            }
            Err(reason) => {
                self.execution_policy_append_audit(&crate::execution_policy::PolicyAuditRecord {
                    ts_ms: now_ms(),
                    actor: crate::execution_policy::ACTOR_IPC.to_string(),
                    action: "load-refused".to_string(),
                    account_id: "*".to_string(),
                    before: None,
                    after: None,
                    error: Some(reason.clone()),
                });
                Err(reason)
            }
        }
    }

    pub fn set_event_sink(&mut self, tx: mpsc::UnboundedSender<Event>) {
        self.tx = Some(tx);
    }

    /// Restore the OME from the durable order log (crash recovery). Call once at
    /// startup, BEFORE trading begins. Live orders are re-adopted (with their
    /// venue ids) so the startup sweep can reconcile them; terminal orders are
    /// dropped and the log compacted to just the restored set.
    ///
    /// Also restores the applied-fill idempotency table (issue #178) — without it,
    /// the venue's own re-delivery of an already-booked trade id books the fill a
    /// second time after every restart — and re-establishes the BUY reservations
    /// those restored orders still commit (issue #181), which a fresh Ledger would
    /// otherwise start without.
    pub fn restore_orders(&mut self) -> usize {
        self.restore_close_credentials();
        let applied = self.restore_applied();
        if applied.0 > 0 {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!(
                    "restored {} applied-fill key(s) from {} after restart (duplicate venue \
                     trades stay idempotent)",
                    applied.0, applied.1
                ),
            });
        }
        let Some(db) = self.order_db.as_ref() else {
            return 0;
        };
        let loaded = db.load();
        if loaded.is_empty() {
            return 0;
        }
        let live: Vec<crate::model::TrackedOrder> =
            loaded.into_iter().filter(|o| o.status.is_live()).collect();
        let n = live.len();
        self.ome.restore(live.clone());
        // The restored orders keep their ORIGINAL ids and stay in the OME
        // registry, while a fresh process's id counter starts at 1. Without
        // this, the first new placements walk straight into the restored ids
        // and the dedup guard refuses the collision — production saw
        // `order already registered: dry_6` right after a restart that
        // recovered one live order. Same rule the position recovery already
        // applies (advance `next_id` past the restored ids).
        let mut max_restored: Option<u64> = None;
        for o in &live {
            if let Some((_, n)) = o.order_id.rsplit_once('_')
                && let Ok(v) = n.parse::<u64>()
            {
                max_restored = Some(max_restored.unwrap_or(0).max(v));
            }
        }
        if let Some(max_id) = max_restored
            && max_id + 1 > self.next_id
        {
            self.next_id = max_id + 1;
        }
        db.compact(&live);
        if n > 0 {
            // A re-adopted order still commits its unfilled notional. The local
            // reservation table is empty on a fresh process, so without this the
            // reserve gate would believe the whole balance is free and the next
            // entries would over-commit against cash the venue has already
            // spoken for (issue #181).
            let mut restored_notional = Decimal::ZERO;
            for o in &live {
                if o.side != Side::Buy {
                    continue;
                }
                let remaining = (o.size - o.filled_size).max(Decimal::ZERO);
                let notional = o.price * remaining;
                // E28: the reservation re-commits cash in the ORDER'S OWN book —
                // a restored order must never spend from another account.
                if let Ok(l) = self.accounts.get_mut(&o.account_id) {
                    l.sync_buy_reservation(&o.order_id, notional);
                }
                restored_notional += notional;
            }
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!(
                    "recovered {n} live order(s) from the order log after restart \
                     ({restored_notional} of BUY notional re-reserved)"
                ),
            });
        }
        n
    }

    /// #463: replay the close-credential journal — pushes whose key carries no
    /// later `consumed` tombstone re-enter the FIFO in file order, so a FAILED
    /// arriving after the restart still finds its credential. Called from
    /// [`Core::restore_orders`], NOT the constructor: recovery here follows the
    /// same explicit pattern as the order and position books, and a constructor
    /// that reads the journal would make every in-process `Core::new` (whose
    /// default config points at the process cwd's `data/orders/`) silently
    /// adopt credentials from unrelated tests sharing that directory.
    fn restore_close_credentials(&mut self) {
        let Some(path) = self.close_credential_log.clone() else {
            return;
        };
        let mut q: std::collections::VecDeque<(String, CloseCredential)> =
            std::collections::VecDeque::new();
        for line in crate::jsonl::load::<CloseCredentialLine>(
            &path,
            "close-credential journal: skipped unparseable lines",
        ) {
            match line {
                CloseCredentialLine::Push { trade_id, cred } => {
                    q.push_back((trade_id, *cred));
                }
                CloseCredentialLine::Consumed { trade_id } => {
                    if let Some(i) = q.iter().position(|(tid, _)| *tid == trade_id) {
                        q.remove(i);
                    }
                }
            }
        }
        if !q.is_empty() {
            tracing::warn!(
                path = %path.display(),
                restored = q.len(),
                "close-credential journal replayed — FAILED rollbacks survive the restart"
            );
            self.close_credentials = q;
        }
    }

    /// Fold the persisted applied-fill journal into the OME. Returns
    /// `(keys restored, path)`.
    fn restore_applied(&mut self) -> (usize, String) {
        let Some(log) = self.applied_log.as_ref() else {
            return (0, String::new());
        };
        let records = log.load();
        if records.is_empty() {
            return (0, log.path().display().to_string());
        }
        self.ome.restore_applied(records);
        let n = self.ome.applied_len();
        // The file was every mutation ever made; it is now the live table.
        self.ome.take_applied_journal();
        log.compact(&self.ome.applied_snapshot());
        (n, log.path().display().to_string())
    }

    /// Append whatever the OME journalled since the last drain. Called after every
    /// applied fill (the only thing that changes the table), so the file is
    /// crash-consistent with the in-memory table at all times.
    fn persist_applied_journal(&mut self) {
        let records = self.ome.take_applied_journal();
        if records.is_empty() {
            return;
        }
        if let Some(log) = self.applied_log.as_ref() {
            log.append(&records);
        }
    }

    /// Persist one order's current snapshot (best effort).
    fn persist_order(&self, id: &str) {
        if let (Some(db), Some(o)) = (self.order_db.as_ref(), self.ome.get(id)) {
            db.append(o);
        }
    }

    /// Restore the OPEN position book from durable storage (crash recovery). Call
    /// once at startup, BEFORE trading begins. Without this a restart forgets
    /// every open position: it is no longer valued or exit-managed, so the trade
    /// drifts to expiry unmanaged. Mirrors `restore_orders`.
    pub fn restore_positions(&mut self) -> usize {
        let Some(db) = self.position_db.as_ref() else {
            return 0;
        };
        // The watermark outlives the position set: a flat book must still
        // remember which external fills were already accounted for.
        self.recon_watermark_ms = db.load_watermark();
        let loaded = db.load();
        if loaded.is_empty() {
            return 0;
        }
        let n = loaded.len();
        self.positions.restore_open(loaded);
        if n > 0 {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!(
                    "recovered {n} open position(s) from the position log after restart"
                ),
            });
        }
        // A settlement whose close never landed is a position that looks open but
        // is already paid: finish it before anything trades (issue #175).
        self.recover_settlements();
        n
    }

    /// Persist the current OPEN position set (best effort). Positions are few and
    /// short-lived, so the whole set is rewritten on every change rather than
    /// appended (a close needs no tombstone).
    fn persist_positions(&self) {
        if let Some(db) = self.position_db.as_ref() {
            db.save(self.positions.open_positions());
        }
    }

    /// Venue order ids the OME believes are resting (orphan detection input).
    pub fn known_venue_ids(&self) -> std::collections::HashSet<String> {
        self.ome.known_venue_ids()
    }

    /// Cancel a venue order the OME has no record of. Used by the startup sweep
    /// to clear orphans left by a previous process/crash. No local state exists
    /// for these ids, so this is purely a venue-side action; the caller performs
    /// the actual cancel and reports the id here for logging/visibility.
    pub fn note_orphan_cancelled(&self, venue_order_id: &str) {
        self.emit(Event::RiskAlert {
            code: CoreErrorCode::Internal,
            message: format!("cancelled resting venue order {venue_order_id}"),
        });
    }
    pub fn mode(&self) -> Mode {
        self.config.mode
    }

    /// The instant this core's accounting session began (epoch ms) — see the
    /// field doc. Exposed read-only over `core.ready` so an external audit can
    /// scope the seeded cash identity to the trades this process can be
    /// responsible for (issue #200).
    pub fn started_at_ms(&self) -> i64 {
        self.started_at_ms
    }
    /// Read-only fee quote (#182) — the `core.feeQuote` entry point, so a gate or
    /// a reconcile script asks the kernel for the schedule it is charging instead
    /// of carrying its own copy of the formula. It sits on `Core` rather than
    /// being a bare function because the day the schedule becomes a per-core
    /// setting (the `--taker-fee-legacy` switch in flight) the lookup belongs
    /// here, and only here.
    pub fn fee_quote(&self, price: Option<Decimal>) -> crate::ipc::schema::FeeQuoteResult {
        fee_quote(price)
    }

    /// Seed the ACTIVE account's ledger cash (the boot `dry_seed_balance` and
    /// the reset paths land here). This is the process-default book's funding —
    /// NOT a venue report: venue cash truth flows through
    /// [`Self::realign_venue_cash`], which is where the audit's venue leg is fed.
    pub fn set_balance(&mut self, b: Decimal) {
        self.accounts.active_ledger_mut().set_balance(b);
    }

    /// Fold one venue's reported cash into the account bound to that venue
    /// (multi-account #532: a venue's balance belongs to the account that
    /// trades it, NEVER to whoever is "active" — the single-account-era
    /// routing here once wrote the PolySimulator's 100 USDC over the default
    /// account's real 13.59). #646: with several books on file an UNBOUND
    /// venue's report is REFUSED (warn + risk alert, nothing written) — an
    /// unowned money fact must not silently land on whoever is active. A
    /// single-account deployment keeps the legacy shape: its one book IS the
    /// active ledger, so the report lands there exactly as before.
    pub fn realign_venue_cash(&mut self, venue: &str, b: Decimal) {
        let Some(target) = self.venue_cash_account(venue) else {
            self.refuse_unbound_venue_cash(venue, b, "realign");
            return;
        };
        if self.config.mode == Mode::Live {
            let ts = now_ms();
            let previous = self
                .accounts
                .ledger_of(&target)
                .map(|l| l.balance())
                .unwrap_or_default();
            if previous != b {
                // The move is recorded PER VENUE so the audit's trade-leg check
                // can name the venue's own correction on ITS book only — a
                // sibling venue's realign must never explain this book's
                // residual.
                self.audit
                    .last_realign
                    .insert(venue.to_string(), (ts, b - previous));
                tracing::info!(
                    account = %target.as_str(),
                    venue = %venue,
                    previous = %previous,
                    reported = %b,
                    "venue cash realigned into its bound account (no resting commitment)"
                );
            }
            self.venue_free_by_venue.insert(venue.to_string(), (ts, b));
        }
        if let Ok(ledger) = self.accounts.get_mut(&target) {
            ledger.set_balance(b);
        }
        tracing::debug!(account = %target.as_str(), venue = %venue, balance = %b, "venue cash routed");
    }

    /// The account a venue's cash truth belongs to: the first ACTIVE account
    /// bound to the venue. #646: no binding falls back to the active ledger
    /// only while the book holds EXACTLY ONE account (the legacy
    /// single-account shape — that one book is the active ledger, so the
    /// fallback is the same destination the report always had); with several
    /// books on file an unbound venue has no owner and this returns None —
    /// the callers refuse the report instead of writing into whoever is
    /// active (an unowned money fact never lands on any book).
    fn venue_cash_account(&self, venue: &str) -> Option<crate::account::AccountId> {
        if let Some(a) = self.accounts.account_for_venue(venue).map(|a| a.id.clone()) {
            return Some(a);
        }
        // Legacy single-account shape: one book, it is the active ledger.
        (self.accounts.account_list().len() == 1).then(|| self.accounts.active_id().clone())
    }

    /// #646: an unbound venue's cash report in a multi-account deployment —
    /// refused, loudly. Nothing is written (no ledger, no realign/venue-free
    /// slot): the money fact has no owner, and writing it into the account
    /// that happens to be active would let one venue's report mutate a
    /// sibling book's risk input. The operator fixes it by binding the venue
    /// (`account.venue.set`); the data plane recovers on the next report.
    fn refuse_unbound_venue_cash(&self, venue: &str, reported: Decimal, path: &str) {
        tracing::warn!(
            venue = %venue,
            reported = %reported,
            path = %path,
            "venue cash report REFUSED: no account bound to this venue — \
             bind one with account.venue.set; nothing written"
        );
        self.emit(Event::RiskAlert {
            code: CoreErrorCode::Internal,
            message: format!(
                "venue cash report [{venue}] refused: no account bound to this \
                 venue (multi-account deployment) — bind one with \
                 account.venue.set; nothing written ({path})"
            ),
        });
    }

    /// The periodic venue-cash fold (`venue_free_balance`): with no resting
    /// commitments the venue's free cash IS the book's total (a fill the bot
    /// missed shows up as cash that left with no fill event — the drift
    /// killer); with resting orders the venue may or may not net them out of
    /// the reported balance, so a wide gap is SURFACED, never auto-corrected
    /// (double-counting risk). Comparison runs against the venue's own bound
    /// account, not the active one.
    pub fn fold_venue_free(&mut self, venue: &str, free: Decimal) {
        let Some(target) = self.venue_cash_account(venue) else {
            self.refuse_unbound_venue_cash(venue, free, "fold");
            return;
        };
        let Ok(ledger) = self.accounts.ledger_of(&target) else {
            return;
        };
        let (balance, reserved) = (ledger.balance(), ledger.reserved());
        if reserved == Decimal::ZERO {
            if balance != free {
                self.realign_venue_cash(venue, free);
                eprintln!(
                    "core: venue cash reconcile [{venue}]: ledger {balance} -> venue free {free}"
                );
            }
        } else {
            let expected_free = balance - reserved;
            if (expected_free - free).abs() > Decimal::new(2, 2) {
                eprintln!(
                    "core: venue cash drift [{venue}]: ledger={balance} reserved={reserved} venue free={free} (resting orders; not auto-corrected)"
                );
            }
        }
    }

    /// The active account's ledger (§9.5). Views and callers with no order
    /// context read this; money-moving sites route by the order's own
    /// `account_id` instead.
    pub fn ledger(&self) -> &Ledger {
        self.accounts.active_ledger()
    }

    /// Whether THIS core's boot replayed durable cash journals (at least one
    /// account's `data/ledger/<account>.journal.jsonl` existed on disk). The
    /// server boot reads it to skip the post-construction seed: a replayed
    /// balance is a boot fact, and re-seeding it would reset the account to
    /// the seed and vaporize the盈亏 history the journal exists to keep.
    pub fn boot_replayed_journal(&self) -> bool {
        self.boot_replayed_journal
    }

    /// #388: the wallet-level cash the core credited from the stream —
    /// `(rebates, rebates_usd, rewards, rewards_usd)`. The replay driver's
    /// report reads this so the report and the ledger cannot disagree about
    /// what was paid. `CoreStats` is private; this is the read window.
    pub(crate) fn cashflow_stats(&self) -> (u64, Decimal, u64, Decimal) {
        (
            self.stats.cashflow_rebates,
            self.stats.cashflow_rebates_usd,
            self.stats.cashflow_rewards,
            self.stats.cashflow_rewards_usd,
        )
    }

    /// E28 (§9.3): the account book — IPC `account.*` reads it, the gates
    /// route through it. Money-moving code does NOT go through this handle;
    /// it routes by the order's own `account_id` at each site.
    pub fn accounts(&self) -> &crate::account::AccountLedgers {
        &self.accounts
    }

    /// E28 `account.list` (§12.1): one [`crate::ipc::schema::AccountView`]
    /// per configured account, sorted by id (`AccountLedgers::account_list`
    /// order). Every money field is the account's OWN book — this view is
    /// how cross-account isolation is READ; `dayRealizedUsd` sums the
    /// account's own closes of the current UTC day.
    pub fn account_views(&self, now_ms: i64) -> Vec<crate::ipc::schema::AccountView> {
        let today = crate::position::utc_day_index(now_ms);
        self.accounts
            .account_list()
            .iter()
            .map(|a| {
                let id = &a.id;
                let ledger = self
                    .accounts
                    .ledger_of(id)
                    .expect("configured account always has a ledger");
                let open_positions = self
                    .positions
                    .open_positions()
                    .iter()
                    .filter(|p| &p.account_id == id)
                    .count();
                let day_realized_usd = self
                    .positions
                    .closed_positions()
                    .iter()
                    .filter(|c| {
                        &c.account_id == id
                            && crate::position::utc_day_index(c.exited_at_ms) == today
                    })
                    .map(|c| c.net_pnl_usd)
                    .sum();
                let (status_tag, status_reason) = match &a.status {
                    crate::account::AccountStatus::Suspended { reason } => {
                        (a.status.wire_tag(), Some(reason.clone()))
                    }
                    _ => (a.status.wire_tag(), None),
                };
                crate::ipc::schema::AccountView {
                    id: id.as_str().to_string(),
                    name: a.name.clone(),
                    market_type: a.market_type,
                    status: status_tag.to_string(),
                    status_reason,
                    balance: ledger.balance(),
                    available: ledger.available(),
                    reserved: ledger.reserved(),
                    credentials_loaded: a.credential_keys.loaded,
                    venue: a.venue.clone(),
                    trading_enabled: a.trading_enabled,
                    strategies: a.strategies.clone(),
                    rounds: a.rounds.clone(),
                    open_positions,
                    day_realized_usd,
                    updated_at_ms: a.updated_at_ms,
                }
            })
            .collect()
    }

    /// E28 `account.status` (§12.1): a runtime call may only TIGHTEN — a
    /// loosening (`frozen -> active`, `read_only -> frozen`) is refused with
    /// INVALID_PARAMS, because an unwind a single IPC call could undo is not
    /// an unwind. Unfreezing is config + restart.
    pub fn tighten_account_status(
        &mut self,
        id: &crate::account::AccountId,
        status: crate::account::AccountStatus,
        now_ms: i64,
    ) -> CoreResult<()> {
        self.accounts.require(id)?;
        let current = self
            .accounts
            .account(id)
            .expect("require checked existence")
            .status
            .clone();
        if !current.can_tighten_to(&status) {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account `{id}` is {current} — changing it to {status} would LOOSEN it; \
                     tighten-only at runtime (unfreeze = config + restart)"
                ),
            ));
        }
        self.accounts.set_status(id, status, now_ms)
    }

    /// The account↔venue binding write (`account.venue.set`): the in-memory
    /// write through [`crate::account::AccountLedgers::set_venue`] and the
    /// durable write into the account book's own file are ONE transaction —
    /// the persist happens FIRST (plan-and-persist before apply: a persist
    /// failure leaves both the file and the memory exactly as they were, so
    /// the two stores can never disagree), the in-memory record moves only
    /// after the file accepted. A core without an
    /// [`CoreConfig::accounts_config_path`] (backtester, most in-process
    /// tests) refuses instead of writing into whatever checkout it happens
    /// to run from — the `runtime_config_path` posture, kept.
    ///
    /// Validation of the venue NAME (a registered plugin, or empty = unbind)
    /// is the caller's job — it needs the compiled-in registry, which the
    /// market-agnostic core deliberately cannot see.
    pub fn set_account_venue_persisting(
        &mut self,
        id: &crate::account::AccountId,
        venue: &str,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> CoreResult<()> {
        use crate::ipc::schema::ACCOUNT_VENUE_SET_NOTE;
        // Existence first, with the standard unknown-account refusal, before
        // anything touches disk.
        self.accounts.require(id)?;
        let Some(path) = self.config.accounts_config_path.as_deref() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.venue.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::persist_account_venue(
            std::path::Path::new(path),
            id.as_str(),
            venue,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.venue.set: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts.set_venue(id, venue, now_ms)?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            venue = %venue,
            reason = %reason,
            "account venue binding written — orders.place is gated by it \
             immediately{}; the binding re-reads at next boot \
             ({ACCOUNT_VENUE_SET_NOTE})",
            if venue.is_empty() { " (empty venue = unbound)" } else { "" },
        );
        Ok(())
    }

    /// The account round-FAMILY write (`account.rounds.set`, 轮次多族 B2):
    /// validate → persist FIRST (a failure leaves both stores untouched) →
    /// the in-memory book. The venue.set transaction posture, kept: the
    /// persist happens FIRST (plan-and-persist before apply — the two stores
    /// can never disagree), and a core without an
    /// [`CoreConfig::accounts_config_path`] refuses instead of writing into
    /// whatever checkout it happens to run from.
    ///
    /// Validation reuses [`crate::account::config::validate_rounds`] (the
    /// SAME whitelist + dedup the parser runs — one spelling of the rule,
    /// never two) over `Some(rounds)`; an empty list passes through as the
    /// legal unrestricted set and REMOVES the file key (missing == `[]` at
    /// the next boot; the deleted key is the cleaner spelling). The
    /// open-exposure refusal for a shrinking set is the IPC arm's job — it
    /// needs the position book AND the removed-family arithmetic, and its
    /// refusal must move NOTHING (memory and disk exactly as they were).
    pub fn set_account_rounds_persisting(
        &mut self,
        id: &crate::account::AccountId,
        rounds: Vec<i64>,
        actor: &str,
        reason: &str,
        _now_ms: i64,
    ) -> CoreResult<()> {
        use crate::ipc::schema::ACCOUNT_ROUNDS_SET_NOTE;
        // Existence first, with the standard unknown-account refusal, before
        // anything touches disk.
        self.accounts.require(id)?;
        // One validation, the parser's own: the plugins' declared union +
        // dedup, first-seen order kept. `Some(..)` (not `None`) — the wire
        // asked for a set.
        let rounds =
            crate::account::config::validate_rounds(Some(rounds), &self.config.allowed_round_secs)?;
        let Some(path) = self.config.accounts_config_path.as_deref() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.rounds.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::persist_account_rounds(
            std::path::Path::new(path),
            id.as_str(),
            &rounds,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.rounds.set: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        // The write-side re-check inside `set_rounds` runs again (same
        // fail-closed posture at every door) — it cannot refuse what
        // `validate_rounds` passed, but the door stays.
        self.accounts
            .set_rounds(id, rounds.clone(), &self.config.allowed_round_secs)?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            rounds = ?rounds,
            reason = %reason,
            "account round families written — the (venue, duration) admission \
             gate reads the new set immediately{}; the declaration re-reads at \
             next boot ({ACCOUNT_ROUNDS_SET_NOTE})",
            if rounds.is_empty() {
                " (empty set = unrestricted, key removed from accounts.toml)"
            } else {
                ""
            },
        );
        Ok(())
    }

    // ── 跨所套利弹窗 v2: the arb leg configuration surface ──────────────────

    /// The arb leg configuration in force (`arb.config.get`'s data source).
    /// `None` = unconfigured.
    pub fn arb_leg_config(&self) -> Option<&crate::account::CrossVenueArbConfig> {
        self.arb_leg_config.as_ref()
    }

    /// The arb leg configuration write (`arb.config.set`): validate → persist
    /// FIRST (a failure leaves both stores untouched) → memory → hot-push the
    /// engine. The hot-push is the whole runtime effect: the adapter stamps
    /// entry/exit accounts per token venue, and the strategy reads its
    /// legs/pairs from the `bk.params()` bag merged on the next evaluate.
    ///
    /// Validations (fail-closed, every refusal names its field):
    ///   * both venues non-empty AND distinct;
    ///   * both accounts exist in the book (accounts.toml's `[[account]]`s);
    ///   * when an account carries a venue binding, that binding must admit
    ///     its own leg's venue (an account pinned to venue V cannot trade
    ///     leg venue W — the scope gate would refuse every leg);
    ///   * pairs non-empty.
    pub fn arb_config_set_persisting(
        &mut self,
        cfg: &crate::account::CrossVenueArbConfig,
        actor: &str,
        reason: &str,
        _now_ms: i64,
    ) -> CoreResult<()> {
        let err = |msg: String| CoreError::new(CoreErrorCode::InvalidParams, msg);
        self.arb_config_validate(cfg)?;
        // Persist first: the file's parse layer re-validates coherence (the
        // same rules the boot runs), so a write that would produce a
        // next-boot-refusing file is refused HERE, before memory moved.
        let Some(path) = self.config.accounts_config_path.as_deref() else {
            return Err(err(
                "arb.config.set: no accounts config path configured — persistence is \
                 disabled for this core (backtests and in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::persist_cross_venue_arb(std::path::Path::new(path), cfg).map_err(
            |e| {
                err(format!(
                    "arb.config.set: persist failed, nothing changed: {}",
                    e.message
                ))
            },
        )?;
        // Memory half, then the hot-push: the engine's adapters read the
        // stamp source, and the strategy reads the params bag — both on the
        // next evaluate, no restart.
        self.arb_config_apply(cfg);
        let (leg_a_venue, leg_a_account, leg_b_venue, leg_b_account) = (
            cfg.leg_a_venue.trim(),
            cfg.leg_a_account.trim(),
            cfg.leg_b_venue.trim(),
            cfg.leg_b_account.trim(),
        );
        tracing::info!(
            target: "risk",
            actor = %actor,
            leg_a = %format!("{leg_a_venue}/{leg_a_account}"),
            leg_b = %format!("{leg_b_venue}/{leg_b_account}"),
            pairs = ?self.arb_leg_config.as_ref().expect("just set").pairs,
            reason = %reason,
            "cross-venue arb legs written — the account stamp and the pairing params are \
             in force on the next evaluation; the section re-reads at next boot"
        );
        Ok(())
    }

    /// The fail-closed validations `arb.config.set` runs BEFORE anything
    /// moves (shared by the persisting write and the memory-only write —
    /// every refusal names its field).
    fn arb_config_validate(&self, cfg: &crate::account::CrossVenueArbConfig) -> CoreResult<()> {
        let err = |msg: String| CoreError::new(CoreErrorCode::InvalidParams, msg);
        let leg_a_venue = cfg.leg_a_venue.trim();
        let leg_b_venue = cfg.leg_b_venue.trim();
        if leg_a_venue.is_empty() {
            return Err(err(
                "arb.config.set: legAVenue must be a non-empty venue name".into(),
            ));
        }
        if leg_b_venue.is_empty() {
            return Err(err(
                "arb.config.set: legBVenue must be a non-empty venue name".into(),
            ));
        }
        if leg_a_venue == leg_b_venue {
            return Err(err(format!(
                "arb.config.set: legBVenue `{leg_b_venue}` is the same venue as legAVenue — \
                 the two legs must sit on DIFFERENT venues"
            )));
        }
        let leg_a_account = cfg.leg_a_account.trim();
        let leg_b_account = cfg.leg_b_account.trim();
        for (field, id) in [
            ("legAAccount", leg_a_account),
            ("legBAccount", leg_b_account),
        ] {
            let account = self
                .accounts
                .account(&crate::account::AccountId::from(id))
                .ok_or_else(|| {
                    err(format!(
                        "arb.config.set: {field} `{id}` names no configured account — \
                         configured accounts are: {}",
                        self.accounts
                            .account_list()
                            .iter()
                            .map(|a| a.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
            let bound = account.venue.trim();
            let leg_venue = if field == "legAAccount" {
                leg_a_venue
            } else {
                leg_b_venue
            };
            if !bound.is_empty() && bound != leg_venue {
                return Err(err(format!(
                    "arb.config.set: {field} `{id}` is bound to venue `{bound}` — it cannot \
                     trade leg venue `{leg_venue}` (unbind it in accounts.toml or pick another \
                     account)"
                )));
            }
        }
        if cfg.pairs.is_empty() {
            return Err(err(
                "arb.config.set: pairs must name at least one asset — an empty list pairs \
                 nothing"
                    .into(),
            ));
        }
        // The pair list is a SET: a duplicate entry would double-push the
        // same asset's pairing (and a next boot would refuse the same file —
        // keep the set semantics identical to `validate_cross_venue_arb_section`).
        let mut seen_pairs = std::collections::HashSet::new();
        for p in &cfg.pairs {
            let up = p.trim().to_uppercase();
            if up.is_empty() {
                return Err(err(
                    "arb.config.set: pairs must not contain a blank asset name".into(),
                ));
            }
            if !seen_pairs.insert(up) {
                return Err(err(format!(
                    "arb.config.set: pairs `{}` appears twice — the list is a set",
                    p.trim()
                )));
            }
        }
        Ok(())
    }

    /// The memory half of a validated set: the normalized config into the
    /// Core cell, hot-pushed to the engine's adapters (the stamp source and
    /// the `bk.params()` entries both read it on the next evaluate).
    fn arb_config_apply(&mut self, cfg: &crate::account::CrossVenueArbConfig) {
        self.arb_leg_config = Some(crate::account::CrossVenueArbConfig {
            leg_a_venue: cfg.leg_a_venue.trim().to_string(),
            leg_a_account: cfg.leg_a_account.trim().to_string(),
            leg_b_venue: cfg.leg_b_venue.trim().to_string(),
            leg_b_account: cfg.leg_b_account.trim().to_string(),
            pairs: cfg
                .pairs
                .iter()
                .map(|p| p.trim().to_uppercase())
                .collect::<Vec<_>>(),
        });
        if let Some(e) = self.engine.as_mut() {
            e.set_arb_leg_config(self.arb_leg_config.clone());
        }
    }

    /// The MEMORY-ONLY leg-configuration write: the same validations and the
    /// same hot-push as [`Self::arb_config_set_persisting`], without the file.
    /// The shape a core without an `accounts.toml` path takes (backtests and
    /// in-process tests) — a restart forgets it, exactly like every other
    /// in-memory knob those cores run.
    pub fn arb_config_set(&mut self, cfg: &crate::account::CrossVenueArbConfig) -> CoreResult<()> {
        self.arb_config_validate(cfg)?;
        self.arb_config_apply(cfg);
        Ok(())
    }

    // ── RFC 568 阶段一：PairBook 的内核接线面 ───────────────────────────────

    /// 配对声明受理（engine 提交管线在 arb_precheck 过闸后逐腿调用）。
    /// 经济身份 (asset, slot) 从腿 token 经 engine scanner 的 token→market
    /// 表派生（不是策略自报的）；token 不在本轮任何市场 ⇒ slot 派生不出
    /// ⇒ 拒收（fail-closed）。owner_account 是实例主账户（实例隔离维度），
    /// 策略自报 key 只作可读标签。声明载荷整体从 engine 的 [`crate::engine::PairDeclaration`]
    /// 传入（owner/腿账户/token/shares 一个结构 —— 受理面不再逐参重抄）。
    fn pair_declare_leg(&mut self, d: &crate::engine::PairDeclaration, now_ms: i64) {
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        // token→market 表：slot/asset 的派生权威（engine scanner 本轮市场）。
        let (asset, slot) = match engine.round_slot_of(&d.token_id) {
            Some(slot) if slot > 0 => (engine.asset_of(&d.token_id).unwrap_or_default(), slot),
            _ => return, // 不在本轮任何市场：无轮次身份，不配进簿
        };
        let facts = crate::pair::TokenMarketFacts { asset, slot };
        let decl = crate::pair::PairLegDecl {
            token_id: d.token_id.clone(),
            shares: d.shares.unwrap_or(Decimal::ZERO),
            leg: d.decl.leg.clone(),
            // 盖章结果（v2 可 A≠B）——PairBook 的腿账户与投影都以它为准。
            leg_account: d.leg_account.clone(),
        };
        let _ = self.pair_book.declare(
            &d.owner_strategy,
            &d.owner_account,
            &d.decl.key,
            &decl,
            &facts,
            now_ms,
        );
    }

    /// place 成功后的 internal_key 回填（OME 事实事件经它寻址回声明腿）。
    fn pair_bind_key(
        &mut self,
        strategy: &str,
        owner_account: &str,
        token_id: &str,
        internal_key: &str,
    ) {
        self.pair_book
            .bind_by_leg(strategy, owner_account, token_id, internal_key);
    }

    /// OME 事实事件喂入（成交/死亡）——幂等，未命中声明腿的事件是空操作。
    fn pair_on_order_event(&mut self, internal_key: &str, filled_delta: Decimal, dead: bool) {
        self.pair_book
            .on_order_event(internal_key, filled_delta, dead);
    }

    /// 轮次推进清退：换轮臂（engine_on_data 的 RoundMarkets 非 top-up）调用。
    fn pair_retire_before(&mut self, before: i64) {
        self.pair_book.retire_before(before);
    }

    /// `bk.arb.pairs()` 投影的数据源：owner 过滤的配对视图（跨策略/跨账户
    /// 不可见）。`accounts` 是该实例可见的账户白名单（实例主账户 + arb 腿
    /// 账户，与 holdings 视图的拓宽同形）。
    pub(crate) fn pair_views_for(
        &self,
        strategy: &str,
        accounts: &HashSet<String>,
    ) -> Vec<crate::pair::PairView> {
        self.pair_book.views_for(strategy, accounts)
    }

    /// 配对簿的测试/审计只读快照（无过滤）——`pair_book_check.rs` 经它断言
    /// 受理链的落账结果；生产路径一律走 `pair_views_for`（owner 过滤投影）。
    pub fn pair_book_all(&self) -> Vec<crate::pair::PairState> {
        self.pair_book.all().into_iter().cloned().collect()
    }

    /// Unset the arb leg configuration (the panel's 「解除配置」): remove the
    /// file section and the memory mirror, engine hot-pushed. `arb.config.set`
    /// is the only writer in the v2 flow; this is the delete arm the wire
    /// spells with `configured: false` after a successful call.
    pub fn arb_config_clear_persisting(&mut self, actor: &str) -> CoreResult<()> {
        let err = |msg: String| CoreError::new(CoreErrorCode::InvalidParams, msg);
        let Some(path) = self.config.accounts_config_path.as_deref() else {
            return Err(err(
                "arb.config.clear: no accounts config path configured — persistence is \
                 disabled for this core (backtests and in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::clear_cross_venue_arb(std::path::Path::new(path)).map_err(|e| {
            err(format!(
                "arb.config.clear: persist failed, nothing changed: {}",
                e.message
            ))
        })?;
        self.arb_leg_config = None;
        if let Some(e) = self.engine.as_mut() {
            e.set_arb_leg_config(None);
        }
        tracing::info!(
            target: "risk",
            actor = %actor,
            "cross-venue arb legs cleared — pairing falls back to the unified-confirm chain"
        );
        Ok(())
    }

    /// The trading-switch write (`account.trading.set`): memory AND
    /// accounts.toml in one transaction — persist FIRST (a failure leaves
    /// both stores untouched), memory after. The engine mirror needs no
    /// separate push here: the next evaluation cycle re-pushes every
    /// account's trading fact, so the gate is in force within one cycle.
    /// `default` is settable like any account (a single-book deployment may
    /// want its one account's entries off too).
    pub fn set_account_trading_persisting(
        &mut self,
        id: &crate::account::AccountId,
        enabled: bool,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> CoreResult<()> {
        use crate::ipc::schema::ACCOUNT_TRADING_SET_NOTE;
        self.accounts.require(id)?;
        let Some(path) = self.config.accounts_config_path.as_deref() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.trading.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::persist_account_trading(
            std::path::Path::new(path),
            id.as_str(),
            enabled,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.trading.set: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts.set_trading(id, enabled, now_ms)?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            enabled = %enabled,
            reason = %reason,
            "account trading switch written — {} re-reads at next boot \
             ({ACCOUNT_TRADING_SET_NOTE})",
            if enabled { "entries re-open; " } else { "new entries refused (exits flow); " },
        );
        Ok(())
    }

    /// The display-pointer write (显示账户配置化 / `display.account.set`):
    /// memory AND accounts.toml in one transaction — persist FIRST (a failure
    /// leaves both stores untouched), memory after. The refusals are ALL
    /// plan-first: the id must name a CONFIGURED account.
    ///
    /// 2026-10-20 操作者裁决（issue 561）：display 是面板的唯一可信账户
    /// 事实源，切户动作 = 写 display。校验据此放宽为「账户存在即可」，
    /// 不再要求 `status = Active`。display 是显示焦点而非交易授权面：
    /// 把指针指向 frozen / read_only 账户去看它的账本与流水是合法操作
    /// （与 `account.switch` 的 existence-only 姿态同一逻辑 —— posture
    /// 管下单，不管看）。消费侧（行情过滤、策略归属、风控、下单）各自
    /// 按账户自身 posture 做门禁，display 指针不放行任何交易语义。
    ///
    /// 语义边界：this pointer never touches the process-level active
    /// account, no ledger, and no strategy enablement — the snapshot's
    /// `displayAccount`/`displayVenue` (and the issue-561 default chain)
    /// are the only readers.
    pub fn set_display_account_persisting(
        &mut self,
        id: &crate::account::AccountId,
        actor: &str,
        reason: &str,
    ) -> CoreResult<()> {
        use crate::ipc::schema::DISPLAY_ACCOUNT_SET_NOTE;
        self.accounts.require(id)?;
        let Some(path) = self.config.accounts_config_path.as_deref() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "display.account.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::persist_display_account(std::path::Path::new(path), id.as_str())
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "display.account.set: persist failed, nothing changed: {}",
                        e.message
                    ),
                )
            })?;
        self.accounts.set_display(id)?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            reason = %reason,
            "display pointer written — the snapshot's displayAccount/ \
             displayVenue follow this account; re-reads at next boot \
             ({DISPLAY_ACCOUNT_SET_NOTE})",
        );
        Ok(())
    }

    /// The strategy-assignment write (`account.strategies.set`): memory AND
    /// accounts.toml in one transaction, plus the engine scope rebuild.
    ///
    /// Names are validated against the engine's REGISTERED set (plan-first —
    /// a typo fails here, never as a silently inert assignment). The rebuild:
    /// every (account, strategy) scope NOT in the new assignment and holding
    /// NO open position for this account is torn down (Drop); one with open
    /// positions is kept as an EXITS-ONLY instance (enabled stays whatever it
    /// was, but the engine drops its entry candidates for this account from
    /// the next cycle on — see `Engine::scope_rebuild`) until the account is
    /// flat in it, then a later call (or boot) drops it. New names in the
    /// assignment are instantiated from the SAME lua package the default
    /// scope loaded (own Lua state, own book).
    ///
    /// 账户↔venue 解耦语义: `requested` is the account's strategy
    /// ENABLEMENT set (the 启用按钮's persistence), not an assignment or a
    /// visibility filter — strategies are global. The write is a whole-set
    /// decision: memory AND accounts.toml in one transaction (persist
    /// first), then the engine scopes rebuilt from it (removed-and-flat
    /// drop; removed-and-held demote exits-only; added instantiate enabled).
    ///
    /// Returns the exits-only keep-list for the reply.
    pub fn set_account_strategies_persisting(
        &mut self,
        id: &crate::account::AccountId,
        strategies: Vec<String>,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> CoreResult<Vec<String>> {
        use crate::ipc::schema::ACCOUNT_STRATEGIES_SET_NOTE;
        self.accounts.require(id)?;
        // Dedupe + order-stable: an assignment that names a strategy twice
        // is a caller bug, but the honest handling is collapse, not refusal.
        let mut requested: Vec<String> = Vec::new();
        for s in &strategies {
            let name = s.trim();
            if name.is_empty() {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    "account.strategies.set: an empty strategy name in the \
                     list cannot be addressed"
                        .to_string(),
                ));
            }
            if !requested.iter().any(|r| r == name) {
                requested.push(name.to_string());
            }
        }
        // Plan-first validation: every requested name must be REGISTERED in
        // the engine (any scope). The registered set is the loadable set —
        // the default scope hosts every loaded package.
        if self.engine.is_none() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.strategies.set: no strategy engine attached".to_string(),
            ));
        }
        for name in &requested {
            if !self
                .engine
                .as_ref()
                .expect("engine presence checked above")
                .strategy_registered(name)
            {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "account.strategies.set: strategy `{name}` is not \
                         registered — the assignable set is what \
                         `strategy.list` reports"
                    ),
                ));
            }
        }
        // Positions THIS account holds, by strategy — the exits-only safety
        // net's input.
        let held: Vec<String> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == *id)
            .map(|p| p.strategy.clone())
            .collect();
        let Some(path) = self.config.accounts_config_path.as_deref() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.strategies.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::persist_account_strategies(
            std::path::Path::new(path),
            id.as_str(),
            &requested,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.strategies.set: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts
            .set_strategies(id, requested.clone(), now_ms)?;
        // #537: the default account's writes keep the persisted enabled-set
        // in lockstep. The boot migrates the state file's set into the file
        // ONLY while the default account has no `strategies` key; without
        // this mirror, a set the operator emptied (key removed from
        // accounts.toml) would be re-imported from a stale state file at the
        // next boot — the exact resurrection the default-toggle mirror above
        // closes. Non-default accounts never migrate, so their state file is
        // untouched (the global toggle's file stays theirs alone).
        if *id == crate::account::default_account_id()
            && let Some(state_path) = self.config.strategy_state_path.as_deref()
        {
            crate::strategy_state::save(std::path::Path::new(state_path), &requested);
        }
        // Engine rebuild. Removed-and-flat scopes drop; removed-and-held
        // scopes are marked exits-only; added scopes instantiate. An EMPTY
        // assignment is explicit "this account runs no strategies" — the
        // rebuild then drops every flat scope of the account (the INHERIT
        // reading this used to carry was retired: per-account assignments
        // are the only truth, the default account included).
        let kept_exits_only = self.rebuild_account_scopes(id, &requested, &held);
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            strategies = ?requested,
            exits_only = ?kept_exits_only,
            reason = %reason,
            "account strategy assignment written — engine scopes rebuilt, \
             re-read at next boot ({ACCOUNT_STRATEGIES_SET_NOTE})",
        );
        Ok(kept_exits_only)
    }

    /// The engine half of an enablement-set write: delegate to the engine's
    /// own scope rebuild, handing it a closure that re-instantiates Lua
    /// packages (the only rebuildable source) from their dir. Returns the
    /// names kept EXITS-ONLY for this account.
    fn rebuild_account_scopes(
        &mut self,
        id: &crate::account::AccountId,
        requested: &[String],
        held: &[String],
    ) -> Vec<String> {
        let account = id.as_str().to_string();
        let Some(engine) = self.engine.as_mut() else {
            return Vec::new();
        };
        // NOTE: `build` borrows `self` mutably while `engine` is borrowed —
        // impossible directly, so the lua re-loader is a FREE function over
        // captured state (name + source dir come from the DEFAULT scope's
        // registration, read before the engine borrow opens).
        let sources: std::collections::HashMap<String, String> =
            engine.scope_sources().into_iter().collect();
        let mut build = |name: &str, account: &str| build_lua_scope(&sources, name, account);
        engine.rebuild_account_scopes(&account, requested, held, &mut build)
    }

    /// Boot application of the accounts file's strategy ENABLEMENT sets
    /// (账户↔venue 解耦语义) — the production `run()` calls this ONCE after
    /// `install_engine`. For every account with a NON-EMPTY `strategies` key
    /// the named scopes are instantiated and ENABLED; the default account
    /// with NO key and a non-empty persisted enabled-set (strategy-state
    /// .json — the old global toggle's durable form) is MIGRATED once: that
    /// set is materialized as the default account's enablement set in
    /// accounts.toml, so the upgrade keeps the production behaviour exactly
    /// (default 账户 strategies=["pair_discount_arb","spread_arb"] 升级后
    /// 行为不变) and the file is the only truth from that boot on.
    ///
    /// NOT part of `install_engine`: the replay/backtest boot goes through
    /// that path too and its single-default book must keep byte-identical
    /// outcomes (the exit-economics baseline). Production assembly is the
    /// only caller.
    ///
    /// Returns the migrated default enablement set (empty when nothing
    /// migrated) so the caller can report it on stderr.
    pub fn apply_account_strategy_assignments_at_boot(&mut self) -> Vec<String> {
        let Some(path) = self.config.accounts_config_path.clone() else {
            return Vec::new();
        };
        // The persisted enabled-set (strategy-state.json) drives the one-time
        // migration of the default account. Read BEFORE any write.
        let persisted_enabled: Vec<String> = match self.config.strategy_state_path.as_deref() {
            Some(p) => crate::strategy_state::load(std::path::Path::new(p)),
            None => Vec::new(),
        };
        let default_id = crate::account::default_account_id();
        // The default account's assignment key decides whether the boot runs
        // on the file's spelling (migration already happened) or migrates.
        let default_has_key = self
            .accounts
            .account(&default_id)
            .is_some_and(|a| !a.strategies.is_empty());
        let mut migrated: Vec<String> = Vec::new();
        if !default_has_key && !persisted_enabled.is_empty() {
            // One-time migration: the file gains the explicit default
            // assignment, spelling exactly what the old implicit behaviour
            // (inherit the persisted enabled set) produced. The write is
            // best-effort — a read-only tree keeps the implicit shape and
            // the boot proceeds (the persisted set is re-read next boot).
            //
            // #537: the persisted set is also kept in lockstep from here on —
            // the default toggle now rewrites BOTH stores, and a set the
            // operator emptied (key removed from the file, empty state file)
            // must not be resurrected by a stale state file left over from
            // the pre-toggle era. This one-time gate fires only when the file
            // never spelled an assignment; afterwards the accounts file is
            // the sole truth, exactly as the contract above states.
            if crate::account::config::persist_account_strategies(
                std::path::Path::new(&path),
                default_id.as_str(),
                &persisted_enabled,
            )
            .is_ok()
            {
                let _ =
                    self.accounts
                        .set_strategies(&default_id, persisted_enabled.clone(), now_ms());
                migrated = persisted_enabled;
                tracing::info!(
                    strategies = ?migrated,
                    "strategy enablement migration: the default account's persisted \
                     enabled set is materialized as its accounts.toml enablement set — \
                     the file is the only truth from here on"
                );
            }
        }
        // Apply every account's enablement set (the default's included —
        // after migration it spells the old behaviour). An empty key is the
        // EXPLICIT "run nothing on this account" set — it never follows the
        // global toggle (INHERIT is retired), so nothing is fanned out here
        // and the account's scopes stay silent until the operator enables
        // strategies for it on the strategies page.
        let assignments: Vec<(String, Vec<String>)> = self
            .accounts
            .account_list()
            .iter()
            .filter(|a| !a.strategies.is_empty())
            .map(|a| (a.id.as_str().to_string(), a.strategies.clone()))
            .collect();
        for (account, strategies) in assignments {
            // A set member's scope is built ENABLED — being in the account's
            // enablement set IS the operator's intent to run it there.
            // Unknown names are skipped with a warning by the rebuild (the
            // registered set is what the strategy dir loaded this boot).
            self.set_account_scopes_at_boot(&account, &strategies);
        }
        migrated
    }

    /// The boot-flavoured scope application: instantiate `strategies` for
    /// `account` and enable every one that landed. No exits-only handling —
    /// at boot the position book is restored AFTER this runs, so nothing is
    /// held yet (the restored positions reattach to the rebuilt scopes by
    /// name; a position whose strategy is not in the account's assignment
    /// keeps getting exit-managed because the position book drives exits, not
    /// the scope set).
    fn set_account_scopes_at_boot(&mut self, account: &str, strategies: &[String]) {
        // The engine borrow and the Lua re-loader cannot overlap: read the
        // source map first (same shape as `rebuild_account_scopes`).
        let sources: std::collections::HashMap<String, String> = self
            .engine
            .as_ref()
            .map(|e| e.scope_sources().into_iter().collect())
            .unwrap_or_default();
        let mut build = |name: &str, acct: &str| build_lua_scope(&sources, name, acct);
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        // DEMOTE/DROP phases are irrelevant at boot (no held positions yet);
        // the rebuild's ADD phase instantiates + enables.
        engine.rebuild_account_scopes(account, strategies, &[], &mut build);
    }

    /// `account.create`（账户簿运行时增删的建半）：新账户落内存 AND
    /// accounts.toml，一个事务 —— persist 先行（失败则两个存储都不动），文件
    /// 收下后才动内存，与 [`Self::set_account_venue_persisting`] 同一姿态。
    /// 身份合同在这里钉死：id 非空 / trim 后非空 / 不允许保留名 `default` /
    /// 不允许与簿内现有 id 重复；status 恒 `Active`（运行时铸造一个受限账户
    /// 等于变相放宽，收紧是 `account.status` 的事）；market_type 恒
    /// `Prediction`（0.3 唯一有插件的市场类型，与文件缺省一致）。凭据面完全
    /// 不存在（§9.4：值永不进内核）—— 面板在身份落地后走既有
    /// `market.credentials.set`（env var 名 → 值，值不出内核）自己组织。
    /// 无 [`CoreConfig::accounts_config_path`] 的内核（回测、多数进程内测试）
    /// 拒绝 —— 同 venue.set 的 "persistence is disabled" 文案形状。
    ///
    /// venue 名的注册校验（plan-first）是调用方的职责 —— 编译进来的插件注册
    /// 表在 IPC 层，市场无关的 core 看不见（同 [`Self::set_account_venue_persisting`]）。
    pub fn create_account_persisting(
        &mut self,
        id: &crate::account::AccountId,
        name: Option<&str>,
        venue: Option<&str>,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> CoreResult<()> {
        use crate::ipc::schema::ACCOUNT_CREATE_NOTE;
        // Reserved name first — BEFORE the empty-id check, so the refusal
        // names the contract, not the spelling (`"default"` trimmed and
        // whitespace-padded variants land on the same reserved refusal).
        let trimmed = id.as_str().trim();
        if trimmed == crate::account::DEFAULT_ACCOUNT_ID {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.create: id `default` is reserved — the zero-config \
                 deployment and the active-cell fallback both own it; add your \
                 accounts under their own ids"
                    .to_string(),
            ));
        }
        if trimmed.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.create: id is empty — an account without an identity \
                 cannot be addressed or audited"
                    .to_string(),
            ));
        }
        let id = crate::account::AccountId::from(trimmed);
        // Existence (duplicate) with an honest refusal, before disk.
        if self.accounts.account(&id).is_some() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("account.create: account `{id}` already exists"),
            ));
        }
        let Some(path) = self.config.accounts_config_path.as_deref() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.create: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::persist_account_create(
            std::path::Path::new(path),
            id.as_str(),
            name,
            venue,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.create: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts.insert(crate::account::Account {
            id: id.clone(),
            name: name.unwrap_or(id.as_str()).to_string(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: venue.filter(|v| !v.is_empty()).unwrap_or("").to_string(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            updated_at_ms: now_ms,
        })?;
        // A runtime mint on a locally-settling core gets the same seed a boot
        // account would have woken up with. Without this the strategy sizing
        // reads equity 0 and the scope never trades (`policy.skip: equity 0
        // below min_equity_usd`) — and no boot will ever repair it: the boot
        // seed (`seed_all`) is skipped wholesale once ANY account has a
        // journal, which an established deployment always does. LIVE is not
        // seeded: the venue's real cash is the balance, realigned from the
        // venue report. The journal row (kind Seed, ref `account.create`)
        // makes the funding a replayable boot fact for the NEXT boot.
        //
        // The journal only lands if the minted ledger has a journal PATH —
        // boot's `attach_journals` never sees accounts that did not exist at
        // boot, so the path is attached HERE. Without it the funding row stays
        // in the in-memory trail only and a restart wakes the account at zero
        // (生产 probe2 实锤: balance 1000 → restart → 0).
        let ledger = self.accounts.get_mut(&id)?;
        if let Some(dir) = self.config.ledger_journal_dir.as_deref() {
            ledger.set_journal_path(
                std::path::Path::new(dir).join(format!("{}.journal.jsonl", id.as_str())),
            );
        }
        if self.config.mode.settles_locally()
            && self.config.dry_seed_balance > rust_decimal::Decimal::ZERO
        {
            ledger.fund(self.config.dry_seed_balance, "account.create");
        }
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            name = %name.unwrap_or(id.as_str()),
            venue = %venue.unwrap_or(""),
            reason = %reason,
            "account created — ACTIVE prediction account minted and persisted; \
             credentials ride market.credentials.set (names only, never values) \
             ({ACCOUNT_CREATE_NOTE})",
        );
        Ok(())
    }

    /// `account.remove`（账户簿运行时增删的删半）：账户离开内存 AND
    /// accounts.toml，同一事务姿态。拒绝面：账簿的最后一个账户（空簿会让
    /// 下一次 boot 拒载，删掉它是删库式脚枪）；仍有 open position 的账户
    /// （钱不能变成不可寻址的）；未知 id 标准拒绝。`default` 只是普通账户
    /// —— 所有账户都可删（用户裁决：绑定只在建户时绑定、编辑面已收，
    /// 删户重建是绑定变更的唯一出口；零配置的 default 由下一次 boot 的
    /// 缺文件/缺行路径重新派生）。被删的是进程级 active 账户时自动回切
    /// 剩余账户的第一个（会话级切换器在下次读时照簿解析）。
    pub fn remove_account_persisting(
        &mut self,
        id: &crate::account::AccountId,
        actor: &str,
        reason: &str,
    ) -> CoreResult<()> {
        use crate::ipc::schema::ACCOUNT_REMOVE_NOTE;
        // The LAST account stays: an empty book fails the next boot's parse,
        // so retiring it would be a delete-the-deployment footgun. Every id
        // is otherwise fair game — `default` included (the zero-config
        // fallback re-derives on the next boot when the file lacks a
        // `default` row). Existence refuses first, with the standard
        // unknown-account error.
        self.accounts.require(id)?;
        if self.accounts.account_list().len() <= 1 {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.remove：`{id}` 是账户簿里最后一个账户——至少要保留一个账户 \
                     （下一次启动会拒绝空账户簿；确要清空部署请直接删除 accounts.toml）"
                ),
            ));
        }
        // Open positions pin the account: retiring it would leave inventory
        // with no book, no ledger and no close path — money must not become
        // unaddressable.
        let open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| &p.account_id == id)
            .count();
        if open > 0 {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.remove：`{id}` 还有 {open} 笔未平持仓——请先平仓 \
                     （删掉的账户不带走任何库存，钱必须始终有簿可归）"
                ),
            ));
        }
        let Some(path) = self.config.accounts_config_path.as_deref() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.remove: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::persist_account_remove(std::path::Path::new(path), id.as_str())
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "account.remove：账户簿落盘失败，两个存储都没有改动：{}",
                        e.message
                    ),
                )
            })?;
        // Journal 收尾（现金流水卡与任何账户对不上的根因修复）：把该账户的
        // 磁盘流水文件删掉，内存 trail 同步清空。同名 id 的下一次
        // `account.create` 是新账本，不是老账户续弦 —— 留着旧文件，下一次
        // boot 的 attach_journals 会把上一世的成交流水整表回放进来（生产
        // kalshi 实锤：6 轮建/删叠出 6 笔幽灵 seed@account.create，Σamount
        // 比真实余额多 5000）。删文件失败则整体拒绝 —— 此时 accounts.toml
        // 已收下了删除（persist 先行的代价），但内存未动、旧 journal 还在，
        // 重试一次即可收敛；拒绝的消息如实告知。
        if !self.accounts.get_mut(id)?.retire_journal_file() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.remove：`{id}` 的流水文件删除失败 —— accounts.toml \
                     已落盘但内存未动，旧流水文件还在；请重试一次，问题持续 \
                     请检查 data/ledger/ 的写权限"
                ),
            ));
        }
        self.accounts.remove(id)?;
        // 冻结簿随账户走：旧实现留着悬空冻结项（同名 id 重建即继承上一世
        // 的 kill switch），自愈时间戳同理一并摘除。
        self.account_freezes.remove(id.as_str());
        self.auto_frozen.remove(id.as_str());
        self.accounts.repair_active_after_removal(id);
        // 显示账户配置化: the removal must not orphan the display pointer —
        // the file was rewritten WITHOUT the row above, and the same render
        // re-derived its top-level `display_account` key when it pointed here
        // (persist_account_remove's own edit); the memory cell re-derives the
        // same way so the snapshot never names a removed account.
        self.accounts.repair_display_after_removal(id);
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            reason = %reason,
            "account removed — identity and ledger retired, the active cell \
             re-activates the first surviving account when it named the \
             removed account; the display pointer re-derives the same way \
             when the displayed account left ({ACCOUNT_REMOVE_NOTE})",
        );
        Ok(())
    }

    pub fn ome(&self) -> &Ome {
        &self.ome
    }
    pub fn positions(&self) -> &PositionManager {
        &self.positions
    }

    /// Mutable access to the position manager — the same seam `config_mut`
    /// opens for engine wiring (backtest hosting re-points the config; the
    /// #364 IPC tests seed closed rows to replay). Not for moving money:
    /// every cash path routes through the ledgers, not this book.
    pub fn positions_mut(&mut self) -> &mut PositionManager {
        &mut self.positions
    }

    /// Mutable handle on the account book — test/seed seams only (the #364
    /// IPC tests prime a ledger before replaying history). The LIVE money
    /// paths never come through here: they route by the order's own
    /// `account_id` at each site (§9.3).
    pub fn accounts_mut(&mut self) -> &mut crate::account::AccountLedgers {
        &mut self.accounts
    }

    /// Mutable access to config (engine wiring adjusts exits/positions live).
    pub fn config_mut(&mut self) -> &mut CoreConfig {
        &mut self.config
    }

    /// Read-only view of the active config (diagnostics/backtest reports).
    pub fn config(&self) -> &CoreConfig {
        &self.config
    }

    // ── Strategy engine & extensions (P0.5) ─────────────────────────────────
    /// Strategy names the kernel supports (the self-driving engine's set),
    /// deduplicated and sorted. Multi-account deployment hosts one scope per
    /// (strategy, account) pair, so the raw engine list repeats a strategy
    /// once per account — `strategy.list` and the stats surface are per-
    /// NAME tables and must see each name exactly once (PR H). Sorted, not
    /// run-length deduped: the second scope of a name registers AFTER every
    /// first-scope name, so duplicates are never adjacent.
    ///
    /// 策略全局可见（2026-10 操作者裁决）：the row set is the INSTALLED set
    /// (`package_sources` — sticky provenance, one entry per package LOADED
    /// this session, surviving scope teardowns) UNION the live scopes. Boot
    /// scans every package disabled, then the account enablement sets tear
    /// the unassigned scopes down — a registry read of live scopes alone
    /// would shrink the strategies page to the enabled few and lock every
    /// other strategy out of the panel's enable buttons (its assignable set
    /// is `strategy.list`). Torn-down names carry no live scope, so their
    /// row reads `accounts: []`, sizing/gate views fall back to the globals,
    /// and enabling one re-instantiates the scope from the sticky source.
    pub fn strategy_names(&self) -> Vec<String> {
        let mut names = self
            .engine
            .as_ref()
            .map(|e| e.supported_strategies())
            .unwrap_or_default();
        if let Some(e) = self.engine.as_ref() {
            for (name, _) in e.scope_sources() {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }
    /// Enabled strategy names.
    pub fn enabled_strategy_names(&self) -> Vec<String> {
        self.engine
            .as_ref()
            .map(|e| e.enabled_strategies())
            .unwrap_or_default()
    }
    /// Toggle a strategy on the driving engine. Returns false when the engine
    /// is not attached or the name is unknown.
    ///
    /// A toggle changes which strategies Shadow Evolution watches (it registers
    /// around the enabled set), so the manager is rewired immediately: switching a
    /// strategy on brings its parameters and variants in, switching it off drops
    /// them. No restart, and nothing is traded by a strategy that is off.
    ///
    /// 账户↔venue 解耦语义: this is the GLOBAL strategy switch (the master
    /// power). The per-account 启用按钮 rides
    /// [`Self::set_scoped_strategy_enabled`] below.
    ///
    /// PERSISTENCE ASYMMETRY (issue 552 audit, honest wording): this toggle is
    /// a SESSION-level fact on the default scope's enabled flag — it does not
    /// persist. The durable per-account truth is only the accounts.toml
    /// enablement set: a kernel restart replays it, so a scope named in an
    /// account's set boots ENABLED even if the session toggled the global
    /// switch off. To keep a strategy off across restarts, remove it from
    /// every account's set (the panel's 启用按钮 / `account.strategies.set`).
    pub fn set_strategy_enabled(&mut self, name: &str, enabled: bool) -> bool {
        let ok = self
            .engine
            .as_mut()
            .map(|e| e.set_strategy_enabled(name, enabled))
            .unwrap_or(false);
        if ok {
            self.rewire_hot_params(now_ms());
            // Record the operator's intent: the next boot replays this set.
            // `None` (backtests, most tests) writes nothing.
            if let Some(path) = &self.config.strategy_state_path {
                crate::strategy_state::save(
                    std::path::Path::new(path),
                    &self.enabled_strategy_names(),
                );
            }
        }
        ok
    }

    // ── 账户↔venue 解耦：per-account strategy ENABLEMENT 语义层 ─────────────
    //
    // 语义裁决（本轮）：不存在「为账户分配策略」的呈现逻辑 —— 策略全局可见；
    // accounts.toml 的 `strategies` 记录的是每账户×策略的启用状态（面板启用
    // 按钮的持久化）。scope (strategy, account) 运行 ⇔ 策略已安装 AND 账户
    // 已启用 AND 策略 ∈ 该账户启用集。被停用的 scope 若有持仓 → exits-only：
    // 不再开新仓，TP/SL/挂单巡逻/退出意图照常管理到平完。以下三个原语是
    // server.rs 的 handler 接线面（strategy.enable / strategy.disable /
    // strategy.list 的语义半）；wire 形状由 ipc 层负责。

    /// The per-account half of `strategy.enable {name, accountId?, reason?}`:
    /// set ONE (strategy, account) scope's enablement — persist to the
    /// account book's `strategies` set (memory first, disk after, one
    /// transaction) and drive the engine's scope state from it.
    ///
    /// Enable: the strategy joins the account's enablement set; a scope that
    /// does not exist yet is instantiated (Lua sources only — the same
    /// rebuild the boot uses) and arrives enabled; an existing
    /// exits-only-demoted scope returns to normal. Disable: the scope leaves
    /// the set; a flat scope is torn down, a scope holding positions is
    /// demoted EXITS-ONLY (no new entries, exits keep flowing to flat) —
    /// `kept_exits_only` names what was demoted, for the reply.
    ///
    /// `id` MUST be a configured account (the standard unknown-account
    /// refusal otherwise). `default` is a legal target: its scopes are the
    /// historical global-toggle shape, and its enablement set rides the same
    /// file. The GLOBAL toggle (`set_strategy_enabled`) is NOT touched —
    /// this is per-account state only.
    ///
    /// Returns `(kept_exits_only, set_now)` — the demoted names, and the
    /// account's enablement set as it now stands (the reply's echo).
    pub fn set_scoped_strategy_enabled(
        &mut self,
        id: &crate::account::AccountId,
        name: &str,
        enabled: bool,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> CoreResult<(Vec<String>, Vec<String>)> {
        use crate::ipc::schema::ACCOUNT_STRATEGIES_SET_NOTE;
        // Existence first, with the standard unknown-account refusal, before
        // anything touches disk.
        self.accounts.require(id)?;
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "strategy.enable: an empty strategy name cannot be addressed".to_string(),
            ));
        }
        // The assignable set is what `strategy.list` reports: every package
        // LOADED this session. Enabling an uninstalled strategy is refused —
        // a silent never-trading scope is the exact production accident the
        // rebuilt-scopes-start-ENABLED rule exists against.
        if self
            .engine
            .as_ref()
            .is_none_or(|e| !e.strategy_registered(trimmed))
        {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "strategy.enable: strategy `{trimmed}` is not registered — \
                     the assignable set is what `strategy.list` reports"
                ),
            ));
        }
        // The account's current enablement set (the book is the truth), then
        // the toggle.
        let mut set: Vec<String> = self
            .accounts
            .account(id)
            .map(|a| a.strategies.clone())
            .unwrap_or_default();
        let present = set.iter().any(|s| s == trimmed);
        if enabled && !present {
            set.push(trimmed.to_string());
        } else if !enabled {
            set.retain(|s| s != trimmed);
        }
        // Persist FIRST (a failure leaves both stores untouched — the same
        // memory-after-disk transaction every accounts.toml writer runs).
        let Some(path) = self.config.accounts_config_path.as_deref() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "strategy.enable: no accounts config path configured — persistence \
                 is disabled for this core (backtests and in-process tests); \
                 nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::persist_account_strategies(
            std::path::Path::new(path),
            id.as_str(),
            &set,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "strategy.enable: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts.set_strategies(id, set.clone(), now_ms)?;
        // #537: the default account's scoped write keeps the persisted
        // enabled-set in lockstep (the same mirror `set_account_strategies_
        // persisting` runs): the boot migrates the state file ONLY while the
        // default account has no `strategies` key, so a stale state file
        // would otherwise re-import a set the operator already emptied.
        if *id == crate::account::default_account_id()
            && let Some(state_path) = self.config.strategy_state_path.as_deref()
        {
            crate::strategy_state::save(std::path::Path::new(state_path), &set);
        }
        // Engine rebuild over the new set: removed-and-flat scopes drop,
        // removed-and-held scopes demote exits-only, added scopes
        // instantiate enabled.
        let held: Vec<String> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == *id)
            .map(|p| p.strategy.clone())
            .collect();
        let kept_exits_only = self.rebuild_account_scopes(id, &set, &held);
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            strategy = %trimmed,
            enabled,
            set = ?set,
            exits_only = ?kept_exits_only,
            reason = %reason,
            "per-account strategy enablement written — scope rebuilt from the \
             account's enablement set, re-read at next boot ({ACCOUNT_STRATEGIES_SET_NOTE})",
        );
        Ok((kept_exits_only, set))
    }

    /// The per-account enablement readout: the accounts whose enablement set
    /// names this strategy, sorted, plus the default scope when the global
    /// toggle is ON and default carries no key of its own (the zero-config
    /// shape has no enablement-set row to point at). This is the WRITE-path
    /// truth ("which accounts does this strategy trade on"), kept for the
    /// boot-replay teeth and any caller that wants enablement, not hosting.
    ///
    /// NOTE the deliberate difference from `Engine::strategy_scopes` — what
    /// `strategy.list`'s per-row `accounts` field serves (the coordination
    /// contract, wired in server.rs): the HOSTED scope list, exits-only
    /// demotions included. A scope pulled out of the enablement set while
    /// holding positions must keep showing on the matrix — the panel needs
    /// to see "this account still manages its exits here" — while this
    /// function answers the narrower "whose enablement button names it".
    pub fn strategy_enabled_accounts(&self, name: &str) -> Vec<String> {
        let mut accounts: Vec<String> = self
            .accounts
            .account_list()
            .into_iter()
            .filter(|a| a.strategies.iter().any(|s| s == name))
            .map(|a| a.id.as_str().to_string())
            .collect();
        // The zero-config shape: a `default` with no enablement key whose
        // global toggle is ON still runs the strategy — its row says so.
        let default_id = crate::account::default_account_id();
        let default_has_key = self
            .accounts
            .account(&default_id)
            .is_some_and(|a| !a.strategies.is_empty());
        if !default_has_key
            && self
                .engine
                .as_ref()
                .is_some_and(|e| e.enabled_strategies().iter().any(|n| n == name))
        {
            accounts.push(default_id.as_str().to_string());
        }
        accounts.sort();
        accounts.dedup();
        accounts
    }

    /// The handshake verdict for one declaration, as an associated function so
    /// the load path can run it while the engine borrow is live (only
    /// field-disjoint access is legal there). `None` = compatible: an
    /// undeclared strategy participates not (§8.1 — every 0.2 library's
    /// state), and no active plugin (backtester, tests) leaves nothing to
    /// refuse against.
    ///
    /// Multi-plugin: the declaration is judged against EVERY snapshot and the
    /// strategy loads when ANY ONE plugin can serve it; the refusal names each
    /// plugin's own mismatch so the diagnostic stays complete. Single-plugin
    /// books render exactly the historical message (no per-plugin prefix).
    fn modes_refusal(
        plugins: &[crate::market::compat::PluginModes],
        name: &str,
        modes: &[blitzkrieg_strategy_api::StrategyMode],
    ) -> Option<String> {
        if modes.is_empty() {
            return None;
        }
        if plugins.is_empty() {
            return None;
        }
        let refusals: Vec<String> = plugins
            .iter()
            .filter_map(|p| crate::market::compat::check_compatibility(name, modes, p).err())
            .collect();
        if refusals.len() < plugins.len() {
            // At least one plugin passed (its refusal is absent): ANY one
            // satisfiable venue loads the strategy — a venue the strategy
            // cannot serve must not block the venues it can.
            return None;
        }
        // Every plugin refused: the diagnostic names each mismatch. ONE plugin
        // → the bare historical message, byte-identical; several → prefixed
        // and joined so the full mismatch surface is visible.
        if plugins.len() == 1 {
            return refusals.into_iter().next();
        }
        Some(format!(
            "incompatible modes: strategy '{name}' is not served by any active plugin — {}",
            refusals.join(" | ")
        ))
    }

    /// The handshake verdict for a strategy already in the dispatch, by name.
    pub(crate) fn compatibility_refusal(&self, name: &str) -> Option<String> {
        match self.strategy_declarations.get(name) {
            Some(modes) => Self::modes_refusal(&self.plugin_modes, name, modes),
            None => None,
        }
    }

    /// Inject every active plugin's declaration snapshot (server assembly,
    /// BEFORE any strategy is loaded or enabled, so all three §8.2 handshake
    /// sites judge against the same view). Empty leaves the handshake inert
    /// (backtester, tests).
    pub fn set_plugin_modes(&mut self, modes: Vec<crate::market::compat::PluginModes>) {
        self.plugin_modes = modes;
    }

    /// Test-only: record a declaration for a strategy the loader would have
    /// registered (the test adapters carry no C ABI declaration of their
    /// own). Same map the real load path fills.
    #[cfg(test)]
    pub(crate) fn set_strategy_declarations_for_test(
        &mut self,
        name: &str,
        modes: Vec<blitzkrieg_strategy_api::StrategyMode>,
    ) {
        self.strategy_declarations.insert(name.to_string(), modes);
    }

    /// The enable path with the handshake. Returns `(found, applied, reason)`:
    /// `found` says the name exists in the dispatch, `applied` says the toggle
    /// took effect, and `reason` carries the §8.2 diagnostic when the
    /// handshake refused an enable. Disabling is never gated — standing down
    /// is always allowed.
    pub fn set_strategy_enabled_checked(
        &mut self,
        name: &str,
        enabled: bool,
    ) -> (bool, bool, Option<String>) {
        let found = self.strategy_names().iter().any(|n| n == name);
        if enabled
            && found
            && let Some(reason) = self.compatibility_refusal(name)
        {
            return (true, false, Some(reason));
        }
        let applied = self.set_strategy_enabled(name, enabled);
        (found, applied, None)
    }

    /// E27 (§8.3): the `strategy.list` view for one strategy — the declared
    /// modes as the §2.3 wire objects (`null` = undeclared), the compatible
    /// flag and the refusal reason when incompatible. Declarations describe
    /// live dispatch members only (recorded at load, cleared at unload).
    pub fn strategy_compat_view(&self, name: &str) -> (serde_json::Value, bool, Option<String>) {
        match self.strategy_declarations.get(name) {
            None => (serde_json::Value::Null, true, None),
            Some(modes) => {
                let wire: Vec<serde_json::Value> = modes
                    .iter()
                    .map(blitzkrieg_strategy_api::modes::mode_wire_json)
                    .collect();
                let refusal = Self::modes_refusal(&self.plugin_modes, name, modes);
                (serde_json::Value::Array(wire), refusal.is_none(), refusal)
            }
        }
    }

    // ── 策略 tunables 读/写（issue #481 G3）─────────────────────────────────

    /// `strategy.params.get`: one strategy's declared tunables plus the value
    /// in force. The schema source is the strategy's OWN knob declaration
    /// (manifest tunables for Lua packages) — the kernel invents no domain,
    /// and a strategy that declared nothing reports an empty list.
    pub fn strategy_params_readout(&self, name: &str) -> crate::ipc::schema::StrategyParamsReadout {
        use crate::ipc::schema::{StrategyParamsReadout, TunableView};
        let specs: Vec<_> = self
            .engine
            .as_ref()
            .map(|e| {
                e.strategy_refs()
                    .iter()
                    .find(|s| s.name() == name)
                    .map(|s| s.evolvable_knobs())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let found = self.strategy_names().iter().any(|n| n == name);
        // The current value in force: the registry cell when one exists, else
        // the declared value (the same thing while no override ever landed).
        let mut tunables: Vec<TunableView> = specs
            .iter()
            .map(|k| {
                let value = self
                    .shadow_evolution
                    .params_for(name)
                    .and_then(|p| p.get(&k.name))
                    .unwrap_or(k.value);
                TunableView {
                    name: k.name.clone(),
                    // All in-tree declarations are decimal-typed; the `int`
                    // tag only exists on the Lua manifest surface, which the
                    // set path re-checks against the declared domain. Expose
                    // the knob names the kernel actually enforces.
                    r#type: None,
                    value: value.to_string(),
                    min: k.min.to_string(),
                    max: k.max.to_string(),
                }
            })
            .collect();
        tunables.sort_by(|a, b| a.name.cmp(&b.name));
        let evolvable = !specs.is_empty();
        let params_attached = self.has_hot_params();
        let note = if !found {
            Some(format!("strategy {name} is not loaded"))
        } else if !evolvable {
            Some("该策略没有可调参数（manifest 未声明 tunables 域）".to_string())
        } else {
            None
        };
        StrategyParamsReadout {
            name: name.to_string(),
            found,
            tunables,
            evolvable,
            params_attached,
            note,
        }
    }

    /// `strategy.params.set` (issue #481 G3): the tunables' operator write
    /// path — the `risk.setExit` posture (plan first, write later, audit the
    /// receipt). Validation is against the strategy's OWN declared domains;
    /// a refused patch moves nothing.
    ///
    /// 生效路径（本轮裁决）：写入 = `registry.publish`（该策略 cell 的新值）
    /// 加引擎侧 attach。进化关闭时 `rewire_hot_params` 会把覆盖层脱钩，所以
    /// 一次操作员写入要生效必须把覆盖层重新挂上。这与「关闭进化 = 行为与
    /// 改动前一致」的验收共存：脱钩语义是「没有写入时等于没有覆盖层」，而
    /// 一旦操作员显式写了 tunables，挂载覆盖层就是那个写入的直接后果 ——
    /// 下次 evaluate 起生效；重启后覆盖层消失，回落 manifest 默认（回执
    /// note 明说 `persisted: false`）。进化的梯度锁（±5%）刻意不用于此路径：
    /// 操作员编辑不是进化的一个变体步，只有声明域是硬边界。
    pub fn apply_strategy_params_set(
        &mut self,
        name: &str,
        knobs: &std::collections::BTreeMap<String, serde_json::Value>,
        actor: &str,
        reason: Option<String>,
        now_ms: i64,
    ) -> Result<crate::ipc::schema::StrategyParamsSetUpdate, crate::model::CoreError> {
        use crate::ipc::schema::StrategyParamChange;
        use crate::model::{CoreError, CoreErrorCode};
        use crate::shadow_evolution::knobs::StrategyParams as KnobBag;

        let err = |msg: String| CoreError::new(CoreErrorCode::InvalidParams, msg);
        // The declaration lives on the live instance (E2-c); a strategy that
        // is not hosted has nothing to validate against and nothing to write.
        let specs = self
            .engine
            .as_ref()
            .and_then(|e| {
                e.strategy_refs()
                    .iter()
                    .find(|s| s.name() == name)
                    .map(|s| s.evolvable_knobs())
            })
            .unwrap_or_default();
        if specs.is_empty() {
            return Err(err(format!(
                "strategy {name} declares no tunables (nothing is writable); \
                 read the declared domains with strategy.params.get"
            )));
        }
        if knobs.is_empty() {
            return Err(err(
                "strategy.params.set needs at least one knob; read the declared \
                 domains with strategy.params.get"
                    .into(),
            ));
        }

        // Plan first: every knob must be declared, parse exactly (decimal
        // string or JSON number — no float path) and sit inside its domain.
        let mut plan: Vec<(String, Decimal)> = Vec::new();
        for (knob, raw) in knobs {
            let Some(spec) = specs.iter().find(|s| &s.name == knob) else {
                return Err(err(format!(
                    "knob {knob} is not declared by {name} — nothing was applied; \
                     the declared tunables are: {}",
                    specs
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            };
            let value = match raw {
                serde_json::Value::String(s) => Decimal::from_str_exact(s.trim()),
                serde_json::Value::Number(n) => Decimal::from_str_exact(&n.to_string()),
                other => {
                    return Err(err(format!(
                        "knob {knob}: expected a decimal string, got {other}"
                    )));
                }
            };
            let Ok(value) = value else {
                return Err(err(format!(
                    "knob {knob}: {raw} is not an exact decimal — nothing was applied"
                )));
            };
            if !spec.contains(value) {
                return Err(err(format!(
                    "knob {knob}: {value} is outside the declared domain [{}, {}] — \
                     nothing was applied",
                    spec.min, spec.max
                )));
            }
            plan.push((knob.clone(), value));
        }

        // Write phase: merge the patch over the cell's current values (a
        // single-knob patch keeps the strategy's other knobs in force), then
        // publish + attach so the strategy reads it on the next evaluate.
        let registry = self.shadow_evolution.registry();
        let mut next: KnobBag = registry
            .handle_for(name)
            .map(|c| (**c.load()).clone())
            .unwrap_or_else(|| KnobBag::from_knobs(&specs));
        let mut changes: Vec<StrategyParamChange> = Vec::new();
        for (knob, value) in &plan {
            let from = next.get(knob).unwrap_or(Decimal::ZERO).to_string();
            next.set(knob, *value);
            changes.push(StrategyParamChange {
                knob: knob.clone(),
                from,
                to: value.to_string(),
                effect: "live",
            });
        }
        registry.publish(name, next);
        // Attach the overlay to the engine (see the method doc for why this is
        // unconditional): the set path IS an overlay write.
        if let Some(e) = self.engine.as_mut() {
            e.set_hot_params(Some(std::sync::Arc::clone(&registry)));
        }

        for c in &changes {
            tracing::info!(
                target: "strategy_params",
                actor = %actor,
                strategy = %name,
                knob = c.knob,
                from = %c.from,
                to = %c.to,
                effect = c.effect,
                reason = reason.as_deref().unwrap_or(""),
                "strategy tunables update: {}.{} {} -> {}",
                name,
                c.knob,
                c.from,
                c.to,
            );
        }
        // The evolution audit log carries manual overrides; this write is one
        // (same shape `shadow_evolution_apply` records), so an operator edit
        // shows up next to every adopted promotion.
        let before = {
            let mut bag = KnobBag::from_knobs(&specs);
            // from-values as recorded in the change list (the pre-write cell
            // read raced nothing: both reads happened inside this lock).
            for c in &changes {
                if let Ok(v) = Decimal::from_str_exact(&c.from) {
                    bag.set(&c.knob, v);
                }
            }
            let mut m = crate::shadow_evolution::knobs::MutableParams::new();
            m.set_strategy(name, bag);
            m
        };
        let after = {
            let mut m = crate::shadow_evolution::knobs::MutableParams::new();
            if let Some(p) = registry.handle_for(name).map(|c| (**c.load()).clone()) {
                m.set_strategy(name, p);
            }
            m
        };
        self.shadow_evolution.audit_manual_override(
            name,
            now_ms,
            &before,
            &after,
            "operator tunables edit",
        );

        Ok(crate::ipc::schema::StrategyParamsSetUpdate {
            name: name.to_string(),
            applied: changes,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason,
            persisted: false,
            note: crate::ipc::schema::STRATEGY_PARAMS_SET_NOTE,
        })
    }

    /// `unified.list` (issue #481 G4): the confirmed cross-venue mappings as a
    /// read-only snapshot — per event: status, the venues holding a leg, and
    /// the discrepancy marks. Data only; the panel renders venues as rows, no
    /// venue branch anywhere.
    pub fn unified_list(&self) -> Vec<serde_json::Value> {
        self.engine
            .as_ref()
            .map(|e| {
                e.unified_mappings()
                    .values()
                    .map(|ev| {
                        serde_json::json!({
                            "id": ev.id,
                            "title": ev.title,
                            "status": ev.status,
                            "settlementSource": ev.settlement_source,
                            "venues": ev
                                .listings
                                .values()
                                .map(|l| serde_json::json!({
                                    "venue": l.venue,
                                    "conditionId": l.condition_id,
                                    "asset": l.asset,
                                    "expiresAtMs": l.expires_at_ms,
                                    "status": l.status,
                                }))
                                .collect::<Vec<_>>(),
                            "discrepancies": ev
                                .discrepancies
                                .iter()
                                .map(|d| d.as_str())
                                .collect::<Vec<_>>(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// ④ 统一映射：一轮 RoundMarkets 到达后，把这轮每个市场转成
    /// [`PlatformListing`] 喂给映射账本的候选匹配器（只提议、绝不自动
    /// confirm —— ledger 的合同是 "automatic matching proposes, the operator
    /// disposes"，自动配对会违反 cross_venue_replay_check 的 0 订单负向门）。
    /// settlement 字段发现链路拿不到：空 rules + Unknown source（双空规则在
    /// matcher 里不捏造冲突）。结果存 `unified_candidates` 供
    /// `unified.candidates` 读；每个新轮全量替换（旧轮的过期候选不留）。
    fn propose_unified_candidates(&mut self, markets: &[crate::model::CryptoMarket], now_ms: i64) {
        use blitzkrieg_market_api::unified::{
            ListingStatus, PlatformListing, SettlementSource, Venue,
        };
        self.unified_candidates.clear();
        let venue_of = |s: &str| match s {
            "polymarket" => Some(Venue::Polymarket),
            "kalshi" => Some(Venue::Kalshi),
            "predictfun" => Some(Venue::PredictFun),
            "polysimulator" => Some(Venue::PolySimulator),
            _ => None, // undeclared venue (old archives): nothing to pair
        };
        let mut by_venue: std::collections::BTreeMap<Venue, Vec<PlatformListing>> =
            Default::default();
        for m in markets {
            let Some(v) = venue_of(m.venue.as_str()) else {
                continue;
            };
            by_venue.entry(v).or_default().push(PlatformListing {
                venue: v,
                condition_id: m.condition_id.clone(),
                up_token_id: m.up_token_id.clone(),
                down_token_id: m.down_token_id.clone(),
                // matcher 的资产信号是归一化小写（"bitcoin"）；发现链路给的是 "BTC"。
                asset: Some(m.asset.to_lowercase()),
                title: m.question.clone(),
                settlement_rules: String::new(),
                settlement_source: SettlementSource::Unknown,
                expires_at_ms: m.expires_at_ms,
                status: if m.expires_at_ms > now_ms {
                    ListingStatus::Active
                } else {
                    ListingStatus::Expired
                },
            });
        }
        if by_venue.len() < 2 {
            return;
        }
        let buckets: Vec<Vec<PlatformListing>> = by_venue.into_values().collect();
        for i in 0..buckets.len() {
            for j in (i + 1)..buckets.len() {
                for cand in self
                    .unified_ledger
                    .candidates(&buckets[i], &buckets[j], 10 * 60 * 1000)
                {
                    tracing::info!(
                        target: "risk",
                        score = cand.score,
                        a_venue = cand.a.venue.as_str(),
                        b_venue = cand.b.venue.as_str(),
                        title_a = %cand.a.title,
                        title_b = %cand.b.title,
                        "unified: cross-venue mapping candidate proposed — confirm via unified.confirm"
                    );
                    self.unified_candidates.push(cand);
                }
            }
        }
    }

    /// `unified.candidates` (④): the latest proposal snapshot as read-only
    /// JSON. Empty when the current round has fewer than two declared venues.
    pub fn unified_candidates(&self) -> Vec<serde_json::Value> {
        self.unified_candidates
            .iter()
            .map(|c| {
                serde_json::json!({
                    "a": listing_json(&c.a),
                    "b": listing_json(&c.b),
                    "score": c.score,
                    "expiryDeltaMs": c.expiry_delta_ms,
                })
            })
            .collect()
    }

    /// `unified.confirm` (④): the operator confirms a candidate pair as a
    /// cross-venue mapping. The ledger revalidates (title similarity, asset
    /// identity, one-listing-one-event) and persists; the confirmed event is
    /// pushed into the Engine so strategies see it immediately, and the
    /// confirmed listings leave the proposal shelf.
    pub fn unified_confirm(
        &mut self,
        title: &str,
        actor: &str,
        listings: Vec<blitzkrieg_market_api::unified::PlatformListing>,
        at_ms: i64,
    ) -> Result<blitzkrieg_market_api::unified::UnifiedEvent, String> {
        let event = self
            .unified_ledger
            .confirm(title, actor, &listings, at_ms)
            .map_err(confirm_error_text)?;
        let confirmed_ids: std::collections::HashSet<&str> = event
            .listings
            .values()
            .map(|l| l.condition_id.as_str())
            .collect();
        self.unified_candidates.retain(|c| {
            !confirmed_ids.contains(c.a.condition_id.as_str())
                && !confirmed_ids.contains(c.b.condition_id.as_str())
        });
        if let Some(e) = self.engine.as_mut() {
            e.put_unified_mapping(event.clone());
        }
        Ok(event)
    }

    /// `unified.revoke` (④): withdraw a confirmed mapping. The ledger drops
    /// the event (audited, persisted); the Engine's copy is removed here too
    /// — the Engine has only `put` semantics, so revocation is the host's
    /// job.
    pub fn unified_revoke(
        &mut self,
        event_id: &str,
        actor: &str,
        at_ms: i64,
    ) -> Result<(), String> {
        self.unified_ledger
            .revoke(event_id, actor, at_ms)
            .map_err(|e| format!("unified revoke refused: {e:?}"))?;
        if let Some(e) = self.engine.as_mut() {
            e.remove_unified_mapping(event_id);
        }
        Ok(())
    }

    /// Load an external v2 strategy shared library and register it into the
    /// driving engine's live dispatch (the IPC `strategy.load` entry). Runs
    /// the E27 handshake in ENFORCE mode: a library whose declaration the
    /// active plugin cannot serve is refused, not registered-and-unable-to-
    /// enable (§8.2 row 2).
    pub fn load_strategy_lib(&mut self, path: &str) -> String {
        self.load_strategy_lib_inner(path, true)
    }

    /// The startup scan path (`load_strategy_dir`). §8.2 row 1: the scan only
    /// LOGS what was declared — it never refuses. An incompatible library
    /// still registers (disabled) with the mismatch named on its receipt;
    /// the enable handshake (both `strategy.enable` and the startup
    /// `--enable-strategy` loop) is the gate.
    fn load_strategy_lib_scan(&mut self, path: &str) -> String {
        self.load_strategy_lib_inner(path, false)
    }

    fn load_strategy_lib_inner(&mut self, path: &str, enforce: bool) -> String {
        #[cfg(feature = "strategy-loading")]
        {
            use crate::strategy_engine::loader::load_foreign;
            let p = std::path::Path::new(path);
            // E30 (§6.4): a DIRECTORY is a Lua package, never a dylib — route
            // it through the Lua loader so the two stacks cannot load each
            // other's artifacts (the dylib scanner owns `*.dylib`, the Lua
            // loader owns `manifest.json` packages).
            if p.is_dir() {
                return self.load_lua_package_inner(p, enforce);
            }
            let Some(engine) = self.engine.as_mut() else {
                return "Failed: strategy engine not attached (load libraries after engine init)"
                    .into();
            };
            match load_foreign(p) {
                Ok(loaded) => {
                    let name = loaded.name.clone();
                    let version = loaded.version.clone();
                    // E27 (§8.2): the handshake verdict BEFORE registration.
                    // ENFORCE (the IPC `strategy.load` path): a library whose
                    // declaration the active plugin cannot serve is refused,
                    // not registered-and-silently-unable-to-enable. SCAN (the
                    // startup `strategy_dir` walk): §8.2 row 1 — the scan only
                    // logs, so the library registers disabled and the receipt
                    // itself names the mismatch; the enable handshake is the
                    // gate there. Either way the text names both sides (R-B).
                    let refusal =
                        Self::modes_refusal(&self.plugin_modes, &name, &loaded.declared_modes);
                    if enforce && let Some(refusal) = refusal {
                        // `refusal` already opens with "incompatible modes:" and
                        // names both sides (§8.2) — prefix only the verdict tag.
                        return format!("Failed: {refusal}");
                    }
                    let incompatible = match refusal {
                        Some(r) => format!("; INCOMPATIBLE: {r}"),
                        None => String::new(),
                    };
                    // E2-b: the load receipt names the gates the library declared
                    // unnecessary, so an opt-out is visible before it is enabled.
                    // D-31: if the declaration carries a `time_left_sec` floor,
                    // state it — the receipt is the operator's only pre-enable
                    // view of how far the opt-out actually reaches.
                    let declared = if loaded.gate_exemptions.any() {
                        let floor = match loaded.gate_exemptions.timing_min_time_left_sec {
                            Some(n) => format!(" (timing floor {n}s)"),
                            None => String::new(),
                        };
                        format!(
                            "; declares gate exemptions: {}{floor}",
                            loaded.gate_exemptions.gates().join(",")
                        )
                    } else {
                        String::new()
                    };
                    // E2-c: the same receipt names the evolvable knobs, so "not
                    // evolvable" (no optional symbol) is distinguishable at load
                    // from "evolvable but nothing applied yet".
                    let evolvable = if loaded.evolvable_knobs.is_empty() {
                        "; not evolvable (no knobs declared)".to_string()
                    } else {
                        let names: Vec<&str> = loaded
                            .evolvable_knobs
                            .iter()
                            .map(|k| k.name.as_str())
                            .collect();
                        format!("; declares evolvable knobs: {}", names.join(","))
                    };
                    match engine.register_user_strategy(
                        Box::new(loaded.strategy),
                        format!("dylib:{}", p.display()),
                    ) {
                        Ok(_) => {
                            // E27: the declaration is recorded only on a
                            // successful registration, so `strategy.list` and
                            // the enable-time handshake describe live dispatch
                            // members and nothing else.
                            self.strategy_declarations
                                .insert(name.clone(), loaded.declared_modes.clone());
                            // The new strategy's declaration must reach the manager
                            // too: a library loaded AFTER evolution was enabled would
                            // otherwise never get a unit, so it could not evolve at
                            // all until a restart.
                            self.rewire_hot_params(now_ms());
                            // API 1.0 naming (DEV_V0_3 §2.2, E24 #330): the
                            // receipt KEEPS its old text — the test above pins
                            // it — and only APPENDS the protocol line. E24
                            // handed this hunk to the service.rs hotspot
                            // guardian (E25); it lands here.
                            format!(
                                "{name}@{version} registered into the engine dispatch (disabled{declared}{evolvable}{incompatible}); API 1.0 (line protocol 2)"
                            )
                        }
                        Err(reason) => format!("rejected: {reason}"),
                    }
                }
                Err(outcome) => format!("{outcome:?}"),
            }
        }
        #[cfg(not(feature = "strategy-loading"))]
        {
            let outcome = crate::strategy_engine::loader::load_strategy(std::path::Path::new(path));
            format!("{outcome:?}")
        }
    }

    /// E30 (#336): load one Lua strategy package (a directory carrying
    /// `manifest.json`) and register it into the live dispatch. The E27
    /// handshake applies UNCHANGED — a Lua declaration the active plugin
    /// cannot serve is refused in ENFORCE mode (IPC `strategy.load`) or
    /// registered-disabled-with-verbatim-receipt in SCAN mode (the startup
    /// walk), byte-identical to the dylib rows (§8.2). The receipt carries
    /// the `(lua)` marker so an operator sees the runtime at a glance
    /// (§6.6: 回执含 `(lua)` 标记). A poisoning breach inside the sandbox is
    /// reported on the receipt; it is not a load failure (the strategy is
    /// alive, it just refused to run).
    fn load_lua_package_inner(&mut self, dir: &std::path::Path, enforce: bool) -> String {
        use crate::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
        let Some(engine) = self.engine.as_mut() else {
            return "Failed: strategy engine not attached (load libraries after engine init)"
                .into();
        };
        match load_lua_package(dir) {
            Ok(loaded) => {
                let name = loaded.name.clone();
                let version = loaded.version.clone();
                let tunables = loaded.tunables.clone();
                let holds = loaded.holds_to_settlement;
                let exemptions = loaded.gate_exemptions;
                let atomic = loaded.atomic_batch;
                let trailing = loaded.trailing_exit;
                // E27 (§8.2): the SAME handshake verdict BEFORE registration.
                let refusal =
                    Self::modes_refusal(&self.plugin_modes, &name, &loaded.declared_modes);
                if enforce && let Some(refusal) = refusal {
                    return format!("Failed: {refusal}");
                }
                let incompatible = match refusal {
                    Some(r) => format!("; INCOMPATIBLE: {r}"),
                    None => String::new(),
                };
                // The fee projection (`bk.fees()`): the kernel's ONE schedule,
                // stamped at load so a strategy prices the fee from the same
                // curve the charge path settles in.
                let sched = crate::exit_policy::fee_schedule();
                let fee_view = blitzkrieg_lua_runtime::FeeScheduleView {
                    name: sched.name.to_string(),
                    rate: sched.rate.to_string(),
                    exponent: sched.exponent,
                };
                let adapter = LuaEngineAdapter::new(loaded.strategy, tunables)
                    .declare(holds, exemptions)
                    .declare_atomic_batch(atomic)
                    .declare_trailing_exit(trailing)
                    // RFC-645 A-1: the ONE arbiter package declares the
                    // pair-book statistics surface (决议②: the loader answers
                    // for the Lua side); every other package keeps the
                    // `false` default. The engine's acceptance predicate
                    // still gates the row's arb* fields.
                    .declare_pair_book_stats(name == ARB_STRATEGY);
                adapter.set_fee_schedule(fee_view);
                match engine
                    .register_user_strategy(Box::new(adapter), format!("lua:{}", dir.display()))
                {
                    Ok(_) => {
                        // E30: an ABSENT manifest.modes stays absent — an empty
                        // Vec would render `modes: []` on strategy.list, and
                        // §8.3's contract is `null = undeclared` (E27). Insert
                        // only a package that actually declared modes.
                        if !loaded.declared_modes.is_empty() {
                            self.strategy_declarations
                                .insert(name.clone(), loaded.declared_modes.clone());
                        }
                        self.rewire_hot_params(now_ms());
                        format!(
                            "{name}@{version} (lua) registered into the engine dispatch (disabled{incompatible})"
                        )
                    }
                    Err(reason) => format!("rejected: {reason}"),
                }
            }
            Err(reason) => format!("Failed: {reason}"),
        }
    }

    /// Unload a dynamic strategy library (E9-b): drop its instance from the
    /// live dispatch and `dlclose` it. The engine refuses in-tree or still-
    /// enabled strategies; here we additionally refuse an OPEN POSITION held
    /// under this strategy's name — a strategy that still owns exposure is not
    /// removed from under the exit machinery. Auditable by design: the outcome
    /// string is what the operator sees on the IPC receipt.
    pub fn unload_strategy(&mut self, name: &str) -> String {
        let open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.strategy == name)
            .count();
        if open > 0 {
            return format!(
                "rejected: {name} still holds {open} open position(s) — close them before unloading"
            );
        }
        match self
            .engine
            .as_mut()
            .map(|e| e.unregister_user_strategy(name))
        {
            Some(Ok(true)) => {
                // E27: the declaration leaves with the instance — the
                // handshake and `strategy.list` describe live members only.
                self.strategy_declarations.remove(name);
                format!("{name} unloaded (engine dispatch removed); library freed")
            }
            Some(Ok(false)) => format!("not found: no strategy named {name}"),
            Some(Err(reason)) => format!("rejected: {reason}"),
            None => "Failed: strategy engine not attached".into(),
        }
    }

    /// Load + swap in one call (E9-b `strategy.reload`): load the NEW library
    /// file first, and only when it parses and registers the SAME name does
    /// the old instance drop. A reload failure leaves the old instance live —
    /// a broken build can never blank a running dispatch. Because the swap is
    /// name-keyed, after-reload enable state is preserved.
    pub fn reload_strategy(&mut self, name: &str, path: &str) -> String {
        // 1. The old one must be removable-by-name (enabled/in-tree/open-pos
        //    guards all run below, BEFORE the new instance replaces it).
        let source = self
            .engine
            .as_ref()
            .map(|e| e.strategy_source(name).map(|s| s.to_string()))
            .unwrap_or(None);
        let Some(old_source) = source else {
            return format!("not found: no strategy named {name}");
        };
        let was_enabled = self.enabled_names_contains(name);
        // Temporary disable is NOT needed — the swap runs below; but a still-
        // enabled strategy cannot be swapped by the engine's guard, so the
        // load is tried first and the guard happens after.
        let open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.strategy == name)
            .count();
        if open > 0 {
            return format!(
                "rejected: {name} still holds {open} open position(s) — a reload with exposure in flight is undefined; close first"
            );
        }

        // 2. Load the new instance in a SCRATCH seat (a temp name would confuse
        //    profiles; instead we stage the strategy outside the engine only if
        //    the name differs). The kernel's loader gives us the Received view;
        //    both old and new live in the dispatch under the SAME name below.
        let old = self.unload_strategy(name);
        if !old.starts_with(name) && !old.contains("unloaded") {
            // rejected/not-found/Failed — refuse before loading new
            return format!("reload aborted: {old}");
        }
        let receipt = self.load_strategy_lib(path);
        if !receipt.contains("registered") {
            return format!("reload FAILED (old instance already dropped): {receipt}");
        }
        // Preserve previous enable state.
        if was_enabled {
            self.set_strategy_enabled(name, true);
        }
        let _ = old_source; // receipt text already carries the new source
        format!("reload OK: {receipt}")
    }

    fn enabled_names_contains(&self, name: &str) -> bool {
        self.enabled_strategy_names().contains(&name.to_string())
    }

    /// List installed extensions (name, type, lifecycle state).
    pub fn extension_list(
        &self,
    ) -> Vec<(
        String,
        crate::extension::ExtensionType,
        crate::extension::ExtensionState,
    )> {
        self.extensions.list()
    }

    /// Enable an extension (runs `on_load`). The extension receives ONLY a
    /// narrowed context — no credentials, venue client, socket or internal state.
    pub async fn enable_extension(&mut self, name: &str) -> Result<(), String> {
        let ctx = CoreExtensionContext {
            tx: self.tx.clone(),
            strategies: self.strategy_names(),
        };
        self.extensions.enable(name, &ctx).await
    }

    /// Disable an extension (runs `on_unload`).
    pub async fn disable_extension(&mut self, name: &str) -> Result<(), String> {
        self.extensions.disable(name).await
    }

    /// All-time closed-trade summary for the UI (totalTrades / wins / losses /
    /// totalNetPnl / …). Prefers the persisted summary tracked by the trade db;
    /// falls back to totals recomputed over the JSONL (or in-memory history)
    /// when no db is attached.
    ///
    /// `account = None` is the historical all-accounts call, byte-identical to
    /// pre-H behaviour. `account = Some(id)` recomputes the SAME aggregation
    /// over only that account's rows: the persisted summary is a GLOBAL
    /// accumulator (it cannot answer "this account alone"), so an account query
    /// always folds rows — the persisted file stays the all-accounts headline.
    /// Rows predating E28 carry no accountId and read as the default account,
    /// so an account query on a legacy log still sees them.
    pub fn trade_summary(&self, account: Option<&str>) -> serde_json::Value {
        if account.is_none()
            && let Some(db) = self.trade_db.as_ref()
        {
            return serde_json::to_value(db.summary()).unwrap_or(serde_json::Value::Null);
        }
        // Fallback / per-account path: aggregate over whatever history we can
        // reach, filtered to the requested account when one was named. The
        // account comparison goes through the record's serde-defaulted
        // accountId: empty on pre-E28 rows = the default account.
        let want = account.unwrap_or(crate::account::DEFAULT_ACCOUNT_ID);
        let empty = Vec::new();
        let all_rows = self.recent_trades(0);
        let rows: Vec<serde_json::Value> = match account {
            None => all_rows,
            Some(_) => all_rows
                .into_iter()
                .filter(|r| {
                    let rid = r
                        .get("accountId")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    // An unattributed (pre-E28) row belongs to the default
                    // account; every other row matches by exact id.
                    if rid.is_empty() {
                        want == crate::account::DEFAULT_ACCOUNT_ID
                    } else {
                        rid == want
                    }
                })
                .collect(),
        };
        let rows = if rows.is_empty() { &empty } else { &rows };
        let mut s = serde_json::json!({
            "totalTrades": 0u64, "wins": 0u64, "losses": 0u64, "winRate": 0.0,
            "totalGrossPnl": 0.0, "totalFees": 0.0, "totalNetPnl": 0.0,
            "avgHoldTimeSec": 0.0, "bestTradePnl": 0.0, "worstTradePnl": 0.0,
        });
        let (mut wins, mut losses, mut gross, mut fees, mut net, mut best, mut worst, mut holds) = (
            0u64,
            0u64,
            0.0,
            0.0,
            0.0,
            f64::NEG_INFINITY,
            f64::INFINITY,
            0.0,
        );
        for r in rows.iter() {
            let num = |k: &str| r.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let net_i = num("netPnlUsd");
            if net_i >= 0.0 {
                wins += 1
            } else {
                losses += 1
            }
            gross += num("grossPnlUsd");
            fees += num("feesUsd");
            net += net_i;
            best = best.max(net_i);
            worst = worst.min(net_i);
            holds += num("holdTimeSec");
        }
        let n = (wins + losses) as f64;
        s["totalTrades"] = serde_json::json!(wins + losses);
        s["wins"] = serde_json::json!(wins);
        s["losses"] = serde_json::json!(losses);
        s["winRate"] = serde_json::json!(if n > 0.0 {
            wins as f64 * 100.0 / n
        } else {
            0.0
        });
        s["totalGrossPnl"] = serde_json::json!(gross);
        s["totalFees"] = serde_json::json!(fees);
        s["totalNetPnl"] = serde_json::json!(net);
        s["avgHoldTimeSec"] = serde_json::json!(if n > 0.0 { holds / n } else { 0.0 });
        s["bestTradePnl"] = serde_json::json!(if rows.is_empty() { 0.0 } else { best });
        s["worstTradePnl"] = serde_json::json!(if rows.is_empty() { 0.0 } else { worst });
        s
    }

    /// Recent closed trades (for the UI). Reads the persisted log when present,
    /// else falls back to the in-memory closed positions.
    pub fn recent_trades(&self, limit: usize) -> Vec<serde_json::Value> {
        if let Some(path) = self.config.trade_log_path.as_ref() {
            let rows = crate::trade_db::read_recent(std::path::Path::new(path), limit);
            if !rows.is_empty() {
                return rows;
            }
        }
        let closed = self.positions.closed_positions();
        let start = if limit > 0 && closed.len() > limit {
            closed.len() - limit
        } else {
            0
        };
        closed[start..]
            .iter()
            .map(|c| {
                serde_json::to_value(crate::trade_db::TradeRecord::from_closed(c))
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect()
    }

    // ── Shadow Evolution (opt-in) ───────────────────────────────────────────
    pub fn shadow_evolution(&self) -> &ShadowEvolution {
        &self.shadow_evolution
    }

    /// Operator override for ONE strategy: validate + hot-swap + audit. `params`
    /// must name exactly one strategy; a multi-strategy bag is refused rather than
    /// partially applied.
    pub fn shadow_evolution_apply(
        &mut self,
        params: MutableParams,
        now_ms: i64,
    ) -> Result<(), String> {
        self.shadow_evolution.apply_params(params, now_ms)
    }

    /// Attach the per-strategy parameter registry to the driving engine so an
    /// evolution takes effect on the next tick with no restart. Also re-reads the
    /// strategies' knob declarations (their twins are built from them).
    ///
    /// While evolution is DISABLED the overlay is detached (`None`): strategies
    /// then run on the config the host pushed, which is byte-for-byte the
    /// pre-evolution behaviour (acceptance: "关闭进化时行为与改动前一致").
    fn rewire_hot_params(&mut self, now_ms: i64) {
        if let Some(e) = self.engine.as_mut() {
            if self.shadow_evolution.is_enabled() {
                // The whole host set, enabled or not: a declaration is read off the
                // live instance (E2-c), and `register_strategies` removes the cell of
                // anything ABSENT — so filtering by `enabled` here would delete a
                // switched-off strategy's evolved parameters and rollback anchor,
                // making a toggle lossy. A disabled strategy simply never emits the
                // candidates that would consume them.
                //
                // `now_ms` is the clock the CALLER acts at, forwarded rather than
                // re-read: this re-registration re-anchors every variant, and the
                // birth stamp it writes is what the observation floor is measured
                // against — one operation, one clock (#250).
                self.shadow_evolution
                    .register_strategies(&e.strategy_refs(), now_ms);
                e.set_hot_params(Some(self.shadow_evolution.registry()));
            } else {
                e.set_hot_params(None);
            }
        }
        // The strategies' trailing declarations ride the same registry-change
        // beat: a whole-map replace (stale entries of unregistered strategies
        // must not linger) into the position book, whose ladder then overlays
        // each verdict on the kernel-wide value. Registration, unregistration
        // and account-scope wiring all pass through here.
        let overrides: std::collections::HashMap<String, bool> = self
            .engine
            .as_ref()
            .map(|e| {
                e.strategy_refs()
                    .into_iter()
                    .filter_map(|s| s.wants_trailing_exit().map(|v| (s.name().to_string(), v)))
                    .collect()
            })
            .unwrap_or_default();
        self.positions.set_trailing_overrides(overrides);
    }

    pub fn shadow_evolution_enable(&mut self, now_ms: i64) -> bool {
        self.shadow_evolution.enable(now_ms);
        self.rewire_hot_params(now_ms);
        true
    }

    pub fn shadow_evolution_disable(&mut self) -> bool {
        self.shadow_evolution.disable();
        self.rewire_hot_params(now_ms());
        true
    }

    /// Aggregate status across every evolvable strategy (kept for callers that
    /// predate per-strategy status). `shadow_evolution_status_for` is the precise
    /// per-strategy query.
    pub fn shadow_evolution_status(&self, now_ms: i64) -> EvolutionStatus {
        self.shadow_evolution.aggregate_status(now_ms)
    }

    /// One strategy's status (`None` = not evolvable).
    pub fn shadow_evolution_status_for(
        &self,
        strategy: &str,
        now_ms: i64,
    ) -> Option<EvolutionStatus> {
        self.shadow_evolution.status(strategy, now_ms)
    }

    /// Roll back ONE strategy to the parameters in force before its last change.
    pub fn shadow_evolution_rollback(
        &mut self,
        strategy: &str,
        now_ms: i64,
    ) -> Result<EvolutionOutcome, String> {
        self.shadow_evolution.rollback(strategy, now_ms)
    }

    pub fn shadow_evolution_variants(
        &mut self,
        now_ms: i64,
    ) -> Vec<crate::shadow_evolution::VariantView> {
        self.shadow_evolution.variant_views(now_ms)
    }

    /// Audit history. `None` = every strategy; `Some(name)` = that strategy's file.
    pub fn shadow_evolution_history(
        &self,
        strategy: Option<&str>,
        limit: usize,
    ) -> Vec<crate::shadow_evolution::audit::AuditRecord> {
        self.shadow_evolution.history(strategy, limit)
    }

    /// E13: every known proposal's latest state, newest first (IPC surface).
    pub fn shadow_evolution_proposals(
        &self,
        limit: usize,
    ) -> Vec<crate::shadow_evolution::EvolutionProposal> {
        self.shadow_evolution.all_proposals(limit)
    }

    /// E13: the operator's verdict on one held proposal. String is the IPC-side
    /// verb ("accept"/"reject"/"defer"); anything else is refused loudly.
    pub fn shadow_evolution_decide(
        &mut self,
        id: &str,
        decision: &str,
        now_ms: i64,
    ) -> Result<crate::shadow_evolution::DecisionResult, String> {
        let decision = match decision {
            "accept" | "accepted" => crate::shadow_evolution::Decision::Accept,
            "reject" | "rejected" => crate::shadow_evolution::Decision::Reject,
            "defer" | "deferred" => crate::shadow_evolution::Decision::Defer,
            other => return Err(format!("unknown decision '{other}' (accept|reject|defer)")),
        };
        self.shadow_evolution.decide(id, decision, now_ms)
    }

    /// E13: the auto-evolve switch (the UIs' checkbox). Persisted across restarts.
    pub fn shadow_evolution_set_auto(&mut self, on: bool) -> bool {
        self.shadow_evolution.set_auto_evolve(on);
        self.shadow_evolution.auto_evolve()
    }

    /// E13: the 72h deep-evolution clock. Fires a compound-mutation round when
    /// the interval has elapsed; the returned event, if any, is emitted.
    pub fn shadow_evolution_maybe_cycle(&mut self, now_ms: i64) {
        if let Some(ev) = self.shadow_evolution.maybe_evolution_cycle(now_ms) {
            self.emit(Event::EvolutionCycle {
                cycle_seq: ev.cycle_seq,
                dims: ev.dims,
                strategies: ev.strategies,
                at_ms: ev.at_ms,
            });
        }
    }

    /// Feed the evolution engine a round's markets plus the opening book seeds the
    /// live engine is about to replay, so every twin's confirmation clock starts on
    /// the same tick as the live strategy's.
    pub fn shadow_evolution_on_round(
        &mut self,
        markets: &[CryptoMarket],
        seeds: &[(String, crate::model::OrderbookSnapshot)],
        now_ms: i64,
    ) {
        self.shadow_evolution.on_round(markets, seeds, now_ms);
    }

    /// Feed the evolution engine a book tick (observation only). Twins derive
    /// their own trend state from the books they see, exactly as an external
    /// strategy does, so no externally-computed confirmation flag is passed in.
    pub fn shadow_evolution_on_tick(
        &mut self,
        token_id: &str,
        book: &crate::model::OrderbookSnapshot,
        now_ms: i64,
    ) {
        self.shadow_evolution.on_tick(token_id, book, now_ms);
    }

    /// Run one evolution evaluation and map outcomes to kernel events.
    ///
    /// Called every tick regardless of the engine switch, because the pass is not
    /// only "evaluate": it also expires undecided proposals past their TTL, which
    /// is bookkeeping the operator's "7 天未决自动过期" is a promise about. The
    /// switch itself is the manager's own gate (#249) — one place, so a disabled
    /// engine can neither evaluate nor adopt, and cannot silently stop ageing the
    /// queue it left behind either.
    pub fn shadow_evolution_evaluate(&mut self, now_ms: i64) {
        for outcome in self.shadow_evolution.evaluate(now_ms) {
            self.emit_shadow_outcome(outcome);
        }
    }

    fn emit_shadow_outcome(&self, outcome: EvolutionOutcome) {
        match outcome {
            EvolutionOutcome::Signal(sig) => self.emit(Event::EvolutionSignal { signal: sig }),
            EvolutionOutcome::Applied(sig) => self.emit(Event::EvolutionApplied { signal: sig }),
            EvolutionOutcome::Proposed(proposal) => {
                self.emit(Event::EvolutionProposed { proposal })
            }
            EvolutionOutcome::Rejected { signal, reason } => {
                self.emit(Event::EvolutionRejected { signal, reason })
            }
            EvolutionOutcome::RolledBack { strategy, from, to } => {
                // Rollback restores the pre-change parameters for ONE strategy; the
                // audit file already records it. Emitting an applied-shaped signal
                // keeps the UI's "parameters changed" path single.
                self.emit(Event::EvolutionApplied {
                    signal: crate::shadow_evolution::EvolveSignal::new(
                        format!("rollback-{strategy}"),
                        blitzkrieg_market_api::net::now_ms(),
                        strategy,
                        from,
                        to,
                        crate::shadow_evolution::EvolutionReason::CombinedImprovement,
                        rust_decimal::Decimal::ONE,
                        0,
                        rust_decimal::Decimal::ZERO,
                        "rollback".into(),
                    ),
                })
            }
        }
    }

    // ── Self-driving engine (P3) ────────────────────────────────────────────
    pub fn enable_engine(&mut self, engine: crate::engine::Engine) {
        self.engine = Some(engine);
        // E2-c: each strategy declares its own evolvable knobs, so the manager can
        // only build its units once the strategies exist. This also attaches the
        // parameter registry, so installing an engine never leaves the overlay
        // unwired.
        self.rewire_hot_params(now_ms());
    }
    pub fn has_engine(&self) -> bool {
        self.engine.is_some()
    }

    /// Whether the driving engine currently holds a Shadow Evolution parameter
    /// overlay (E2-c / #28). `false` while evolution is off, because the overlay
    /// is DETACHED rather than merely ignored — the observable form of the
    /// acceptance criterion "evolution off ⇒ behaviour unchanged". A strategy can
    /// then only read the config the host pushed through `on_config`.
    pub fn has_hot_params(&self) -> bool {
        self.engine.as_ref().is_some_and(|e| e.has_hot_params())
    }

    /// Install the running data feed's subscription control (P4), keyed by the
    /// market-plugin name. Called by the market plugin when its `DataFeed`
    /// starts, so the core stays unaware of any concrete feed implementation.
    /// A second install for the same name replaces the first (feed restart).
    pub fn set_subscription(
        &mut self,
        plugin: &str,
        control: std::sync::Arc<dyn blitzkrieg_market_api::SubscriptionControl>,
    ) {
        self.feeds.insert(plugin.to_string(), control);
    }

    // ── 环境差异排查探针（任务二 1-3）───────────────────────────────────────

    /// NTP 循环（`probe::spawn_loop`，生产 `server::run()` 专用）把一次测量
    /// 结果灌回内核。`None` = 查询失败（静默降级，只计失败数）。偏移达到
    /// 警告线（50ms）打一条 WARNING——这是"系统时间偏差超过 100ms 且无日志
    /// 警告 → 测试必须红"验收的日志面；红线（100ms）在
    /// `engine.stats.probe.ntp.skewRed` 打标。
    pub fn note_ntp_offset(&mut self, offset_ms: Option<i64>, now_ms: i64) {
        let warn = self.probe_ntp.apply(offset_ms, now_ms);
        if warn {
            let ms = self.probe_ntp.last_offset_ms.unwrap_or_default();
            let red = self.probe_ntp.skew_red();
            tracing::warn!(
                offset_ms = ms,
                red_line = red,
                "clock offset: local clock is {}{} ms from the NTP server{}",
                if ms >= 0 { "+" } else { "" },
                ms,
                if red {
                    " (RED: at or past the 100 ms line)"
                } else {
                    ""
                },
            );
        }
    }

    /// 探针汇总到点了吗：首个窗口从第一批数据（tick/spot/round）起算满
    /// 10s，之后每 10s 一次。间隔判定基于调用方的时钟（间隔不等式，不是
    /// 固定墙钟窗口——memory: wallclock-test-flake 处方）。
    fn probe_summary_due(&self, now_ms: i64) -> bool {
        match self.probe_ticks.last_summary_ms {
            None => self
                .probe_ticks
                .first_data_ms
                .is_some_and(|first| now_ms - first >= crate::probe::SUMMARY_INTERVAL_MS),
            Some(last) => now_ms - last >= crate::probe::SUMMARY_INTERVAL_MS,
        }
    }

    /// 执行一次探针 10s 汇总：窗口 tick 数、零 tick token、最长静默、
    /// 新断流 token（warn + 计数）与 spot 停更（warn + 计数）。
    /// 只观测——这里发出的每一个字都是日志，不改变任何交易语义。
    fn probe_run_summary(&mut self, now_ms: i64) {
        // 本轮已声明的 token（零 tick 点名对象）；无引擎 = 无名单。
        let round_tokens: Vec<String> = self
            .engine
            .as_ref()
            .map(|e| e.round_token_ids())
            .unwrap_or_default();
        let (ticks, zero, max_silence, newly_broken) =
            self.probe_ticks
                .summarize(now_ms, &round_tokens, crate::probe::TICK_STALE_WARN_MS);
        // 把 venue 标签留在断流行里——排查时"哪个所的流断了"是第一问。
        for token in &newly_broken {
            let (venue, silence_ms) = match self.probe_ticks.per_token.get(token) {
                Some((v, at)) => (v.clone(), now_ms - *at),
                None => (String::new(), 0),
            };
            self.stats_probe_stream_break();
            tracing::warn!(
                token = %token,
                venue = %venue,
                silence_ms = silence_ms,
                "stream break: token had ticks before but none for the budget now"
            );
        }
        if !zero.is_empty() {
            tracing::warn!(
                tokens = %zero.join(","),
                count = zero.len(),
                "zero tick: round tokens with no data this window"
            );
        }
        if ticks > 0 || !zero.is_empty() || !newly_broken.is_empty() {
            tracing::debug!(
                window_ticks = ticks,
                zero_tick_tokens = zero.len(),
                max_silence_ms = max_silence,
                "tick health summary"
            );
        }
        self.probe_last_window_ticks = ticks;
        self.probe_last_zero = zero;
        self.probe_last_max_silence_ms = max_silence;

        // spot 停更（信号失效监控）。
        let (spot_warn, stale) = self
            .probe_spot
            .check(now_ms, crate::probe::SPOT_STALE_WARN_MS);
        if spot_warn {
            self.stats_probe_spot_stale();
            tracing::warn!(
                stale_ms = stale.unwrap_or_default(),
                "spot feed stale: no spot update for the budget — signals pricing off it \
                 are unverified (signal invalidation watch)"
            );
        }
        self.probe_last_spot_stale_ms = stale;
    }

    // 探针的三个曝光计数（stats 面从 probe_ticks/probe_spot 直接读，
    // 这两个只是把 warn 事件也钉在 CoreStats 里以防未来挪动）。
    fn stats_probe_stream_break(&mut self) {
        self.stats_probe_stream_breaks += 1;
    }
    fn stats_probe_spot_stale(&mut self) {
        self.stats_probe_spot_stales += 1;
    }

    /// Subscribe the round's tokens on the NAMED plugin's orderbook feed (P4).
    /// Only that plugin's control sees the set — replacement semantics stay
    /// per-venue. Unknown name or empty token list: no-op.
    pub async fn subscribe_feed_tokens(&self, plugin: &str, tokens: Vec<String>) {
        if let Some(feed) = self.feeds.get(plugin)
            && !tokens.is_empty()
        {
            feed.set_tokens(tokens);
        }
    }

    /// Feed a market-data event to the engine; applies any trend-break bid
    /// cancellation and, for round updates, nothing else.
    pub fn engine_on_data(&mut self, ev: crate::engine::DataEvent, now_ms: i64) {
        // #351: advance in-flight taker legs BEFORE the incoming event is
        // mirrored into `self.books`, so a leg whose `execute_at_ms` falls at
        // this clock is matched against the book as the venue had it — never
        // against the event that arrives at (or after) the same instant. The
        // tick path drains too (rounds with no data still advance time).
        self.drain_inflight_takers(now_ms);
        // Archive the raw event BEFORE anything consumes it, so a replay sees
        // exactly the stream the engine saw (P-1.3). The event's own timestamp is
        // authoritative (it is the clock the engine decides on); the caller's
        // `now_ms` is one step behind, a second drain here is a no-op
        // (legs in stage 2 wait for `report_at_ms`).
        if let Some(a) = self.event_archive.as_mut() {
            let own = crate::data_source::event_at_ms(&ev);
            let at = if own > 0 { own } else { now_ms };
            a.record_at(at, &ev);
        }
        match &ev {
            crate::engine::DataEvent::Book { token_id, .. } => {
                self.stats.books += 1;
                // 探针（任务二 1-3）：一次行情 tick。venue 拿得到就带，
                // 拿不到记空标签（untagged）。纯内存 insert——回放性能
                // 影响可忽略，且不改变任何既有语义。
                let venue = self.venue_of_token(token_id);
                self.probe_ticks.note_tick(token_id, &venue, now_ms);
                if self.replay_sched() {
                    self.fast_eval_wake = true;
                }
            }
            // 盘口批次原子化：批次是 N 本书的合体 —— 每本各计一次 books（与
            // 逐事件路径同口径），一次唤醒（引擎在批次后只应评估一次）。
            crate::engine::DataEvent::Books { books, .. } => {
                self.stats.books += books.len() as u64;
                // 探针（任务二 1-3）：批内每本都是一次行情 tick（与逐事件
                // 路径同口径）。
                for (token, _, _) in books {
                    let venue = self.venue_of_token(token);
                    self.probe_ticks.note_tick(token, &venue, now_ms);
                }
                if self.replay_sched() {
                    self.fast_eval_wake = true;
                }
            }
            crate::engine::DataEvent::TopOfBook { token_id, .. } => {
                self.stats.tops += 1;
                let venue = self.venue_of_token(token_id);
                self.probe_ticks.note_tick(token_id, &venue, now_ms);
                if self.replay_sched() {
                    self.fast_eval_wake = true;
                }
            }
            crate::engine::DataEvent::Spot { .. } => {
                self.stats.spots += 1;
                // 探针：spot 源仍在更新（信号失效监控的"活着"信号）；
                // spot 也算数据——spot-only 部署的停更监控才有起点。
                self.probe_spot.note(now_ms);
                self.probe_ticks.note_any_data(now_ms);
                if self.replay_sched() {
                    self.fast_eval_wake = true;
                }
            }
            // #352: observed on-chain prints / round boundaries are archived
            // above but are not engine inputs — no counter, no strategy effect.
            crate::engine::DataEvent::Trade { .. } => {}
            crate::engine::DataEvent::RoundEnd { .. } => {}
            // #377 (治 #2): a verdict from the stream settles its market
            // directly — recorded like any other data event (above), counted,
            // and handed to the settlement book. A fill-level wake keeps the
            // fast path's evaluator in step with the position-set change the
            // close causes.
            crate::engine::DataEvent::Resolution {
                condition_id,
                payouts,
                neg_risk,
                ..
            } => {
                self.stats.resolutions += 1;
                if self.replay_sched() {
                    self.fast_eval_wake = true;
                }
                self.on_market_resolution(
                    blitzkrieg_market_api::MarketResolution {
                        condition_id: condition_id.clone(),
                        resolved: true,
                        payouts: payouts.clone(),
                        neg_risk: *neg_risk,
                        // The converter recovered this verdict from the
                        // wallet's own redemption activity; naming the source
                        // keeps the settlement journal's provenance honest.
                        source: "archive".to_string(),
                    },
                    now_ms,
                );
            }
            // #388: a wallet-level cash row credits the active account's
            // ledger at the row's own timestamp — money the venue paid
            // outside any fill. NOTHING else moves: no position, order or
            // settlement semantics read it, so a corpus without these rows
            // behaves exactly as before (the branch itself is the delta).
            crate::engine::DataEvent::Cashflow {
                which, amount_usd, ..
            } => {
                match which {
                    crate::engine::CashflowKind::Rebate => {
                        self.stats.cashflow_rebates += 1;
                        self.stats.cashflow_rebates_usd += *amount_usd;
                    }
                    crate::engine::CashflowKind::Reward => {
                        self.stats.cashflow_rewards += 1;
                        self.stats.cashflow_rewards_usd += *amount_usd;
                    }
                }
                self.accounts
                    .active_ledger_mut()
                    .credit_cashflow(*amount_usd, which.as_str());
                // ponytail: the row names no account — DataEvent::Cashflow
                // carries none (#388) and inventing one would mis-attribute.
                // The venue wallet is a process-level funder fact; the credit
                // stays on the active book, disclosed here.
            }
            // #427: a confirmed cross-venue mapping rides the stream as
            // strategy-layer state. The core counts it (corpus completeness)
            // and stores it for the read view; nothing in the trading path
            // acts on it, so a corpus without these rows is byte-identical.
            crate::engine::DataEvent::UnifiedMapping { event, .. } => {
                self.stats.unified_mappings += 1;
                if let Some(e) = self.engine.as_mut() {
                    e.put_unified_mapping(event.clone());
                }
            }
            crate::engine::DataEvent::RoundMarkets { markets, .. } => {
                // #498: a same-slot second feed is a STAGGERED-LISTING top-up
                // (the engine merges it into the scanner without any rollover),
                // so the rollover bookkeeping below must not run for it: it
                // would kill in-flight taker legs whose token the top-up's
                // partial list doesn't carry, sweep live exit reasons and wake
                // the replay fast path for a round that never changed. The
                // predicate mirrors the engine's VENUE-BLIND slot membership
                // (`.first()` generalized to `.any()`): a same-slot feed from
                // ANY venue in a mixed table is the same staggered-listing
                // top-up. Single-venue tables hold one slot, so `any` == the
                // old `first` check — #498's corpus stays byte-identical.
                // The event still reaches `engine.on_data` below — only THIS
                // arm's extras skip.
                let slot = markets.first().map(|m| m.round_slot).unwrap_or(0);
                let venue = markets
                    .first()
                    .map(|m| m.venue.as_str())
                    .unwrap_or_default()
                    .to_string();
                // B1c/E2: slot numbers are only comparable WITHIN one duration
                // family — the membership test is the (slot, duration) PAIR the
                // engine's merge arm (engine.rs) already judges, not the bare
                // slot. The feeding batch's own declared `round_duration_sec`
                // (0 = legacy archive → the configured global grid) must match
                // the resident row's, or a foreign-family batch's first feed is
                // misread as a rollover and the kill arm below (scoped by the
                // feeding VENUE alone, pre-E2) murders a sibling family's
                // in-flight taker on the same venue. Single-family deployments
                // resolve every row and every batch to the SAME duration, so
                // the added condition is constant-true there — the verdict is
                // bit-identical with the pre-B1c slot-only check. The kill arm
                // below gains the same family scope: an in-flight leg dies only
                // when its OWN family rolled over.
                let global_duration_sec = self.config.round_duration_sec;
                let feed_duration = crate::engine::row_family_duration(
                    markets.first().map(|m| m.round_duration_sec).unwrap_or(0),
                    global_duration_sec,
                );
                let top_up = slot != 0
                    && self.engine.as_ref().is_some_and(|e| {
                        e.scanner().markets().iter().any(|m| {
                            m.round_slot == slot
                                && crate::engine::row_family_duration(
                                    m.round_duration_sec,
                                    global_duration_sec,
                                ) == feed_duration
                        })
                    });
                if !top_up {
                    self.stats.rounds += 1;
                    // RFC 568 阶段一：轮次推进 ⇒ 旧轮配对闭账（retire_before，
                    // PairBook 的清退臂 —— Closed 终态吸收迟到的迟发事件）。
                    self.pair_retire_before(slot);
                    // 探针：新一轮的 token 覆盖掉上一轮的——per_token 表键随
                    // 轮替换保持有界（旧 token 不再被汇总点名）。round 声明
                    // 也算数据（零 tick 探针的起点）。
                    self.probe_ticks.per_token.clear();
                    self.probe_ticks.broken.clear();
                    self.probe_ticks.note_any_data(now_ms);
                    // ④ 统一映射：新的一轮带来新市场 ⇒ 重新提议跨所配对候选
                    // （只提议不 confirm）。top-up 跳过：#498 增量合并不换轮，
                    // 旧候选仍然有效，重复刷只是日志噪声。
                    self.propose_unified_candidates(markets, now_ms);
                    // #377 (治 #2): record which markets the archive promised a
                    // verdict for. Their dry-ladder queries are suppressed below —
                    // the stream settles them, and a query stamped here would wait
                    // for an answer that never comes (blind-alert false positive).
                    for m in markets {
                        if m.archive_verdict {
                            self.archive_verdicts.insert(m.condition_id.clone());
                        }
                    }
                    // Replay fast path: a rollover replaces the scanner's market
                    // list and restarts the round clock — the next evaluation sees
                    // a different round context, and the timing verdict flips.
                    if self.replay_sched() {
                        self.fast_eval_wake = true;
                        self.fast_next_timing_ms = now_ms;
                    }
                    // A round rollover replaces the whole round (`engine.markets`
                    // carries the new list), so it is the moment any pending-close
                    // reason whose position is gone can no longer belong to
                    // anything live (issue #190).
                    self.sweep_exit_reasons();
                    // #351: the rollover closed every market of THIS VENUE's old
                    // round (multi-plugin data plane: a Polymarket rollover does
                    // not close Kalshi markets). A taker leg still un-matched for
                    // a token the feeding venue's new round does not carry can
                    // never fill — the venue rejects a FOK for a closed market —
                    // so the honest model kills it NOW (it reports as a rejection
                    // at its report time, reservation released). A leg that
                    // already matched keeps its fill: the venue matched it before
                    // the close.
                    //
                    // E2 (family scope): "this venue's round" means the VENUE'S
                    // FEEDING FAMILY — the engine's rollover replacement arm
                    // (B1c) is already family-scoped (a same-venue sibling
                    // family is replaced by its OWN feed), and this kill arm
                    // must not out-scope it: a 1h rollover on the same venue
                    // must not murder a 15m leg whose round is still running.
                    // The leg's family resolves three-state off its own market
                    // row (own declaration > the global grid) — the same
                    // projection `row_family_duration` gives every other
                    // consumer. Single-family tables hold one duration, so the
                    // added condition is constant-true there: byte-identical
                    // with the pre-E2 venue-only scope.
                    let tokens: Vec<String> = markets
                        .iter()
                        .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
                        .collect();
                    for leg in self.inflight_takers.iter_mut() {
                        if leg.matched_price.is_none() && leg.rejected_msg.is_none() {
                            // Only legs on the ROLLED-OVER (venue, family) are in
                            // scope: a sibling venue's leg is not closed by this
                            // feed, and neither is a sibling FAMILY's leg on the
                            // same venue. The leg's token comes from its own OME
                            // order.
                            let leg_token = self
                                .ome
                                .get(&leg.order_id)
                                .map(|o| o.token_id.clone())
                                .unwrap_or_default();
                            let (leg_venue, leg_family) = self
                                .engine
                                .as_ref()
                                .and_then(|e| e.market_for_token(&leg_token))
                                .map(|m| {
                                    (
                                        m.venue.clone(),
                                        crate::engine::row_family_duration(
                                            m.round_duration_sec,
                                            global_duration_sec,
                                        ),
                                    )
                                })
                                .unwrap_or_default();
                            if leg_venue != venue || leg_family != feed_duration {
                                continue;
                            }
                            let ended = self
                                .ome
                                .get(&leg.order_id)
                                .map(|o| !tokens.contains(&o.token_id))
                                .unwrap_or(true);
                            if ended {
                                leg.rejected_msg = Some(format!(
                                    "taker_killed_round_ended: the order's market closed while the leg was in flight (token {})",
                                    self.ome
                                        .get(&leg.order_id)
                                        .map(|o| o.token_id.clone())
                                        .unwrap_or_default()
                                ));
                            }
                        }
                    }
                }
            }
        }

        // Mirror the latest book into `self.books`, which the position/exit path
        // reads (`run_exit_checks`). Without this, a Rust-native feed (--feed-ws)
        // left Core.books empty, so open positions never re-valued and NO
        // non-forced exit (TP/trailing/SL) could ever fire.
        let mirrored_tokens: Vec<(String, i64)> = match &ev {
            crate::engine::DataEvent::Book {
                token_id,
                bids,
                asks,
                now_ms: event_ms,
            } => {
                self.books.insert(
                    token_id.clone(),
                    crate::sim::Book {
                        bids: bids.clone(),
                        asks: asks.clone(),
                        // #462: keep the FEED's receive time — the freshness
                        // gates downstream must age THIS book by when the
                        // venue last spoke, not by the host's tick clock.
                        received_ms: *event_ms,
                    },
                );
                vec![(token_id.clone(), *event_ms)]
            }
            // 盘口批次原子化：整批按事件顺序逐本镜像（每本自己的批次时间戳
            // `received_ms`）。镜像本身无撕裂可言 —— 它只喂 exit 的 mark，
            // 不喂策略评估；撕裂的根在策略读的那一面，由下面的 Batch 应用
            // 顺序消除。这里借用已结束，循环里的本地副本随取随用。
            crate::engine::DataEvent::Books {
                books,
                now_ms: event_ms,
            } => {
                for (token_id, bids, asks) in books {
                    self.books.insert(
                        token_id.clone(),
                        crate::sim::Book {
                            bids: bids.clone(),
                            asks: asks.clone(),
                            received_ms: *event_ms,
                        },
                    );
                }
                books
                    .iter()
                    .map(|(t, _, _)| (t.clone(), *event_ms))
                    .collect()
            }
            crate::engine::DataEvent::TopOfBook {
                token_id,
                best_bid,
                best_ask,
                now_ms: event_ms,
            } => {
                let mut bids = Vec::new();
                let mut asks = Vec::new();
                if let Some(b) = best_bid
                    && *b > Decimal::ZERO
                {
                    bids.push((*b, Decimal::ONE));
                }
                if let Some(a) = best_ask
                    && *a > Decimal::ZERO
                {
                    asks.push((*a, Decimal::ONE));
                }
                // Only overwrite when we actually have a side (don't clobber a
                // full book with an empty top-of-book update).
                if !bids.is_empty() || !asks.is_empty() {
                    self.books.insert(
                        token_id.clone(),
                        crate::sim::Book {
                            bids,
                            asks,
                            received_ms: *event_ms,
                        },
                    );
                    vec![(token_id.clone(), *event_ms)]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        };

        // Replay fast path: the mirrored book is the ONLY input a position's
        // marks can move on between events, so record the token as dirty.
        // Consumed (and cleared) by the next maintenance tick. #390: the
        // nofast replay needs the same wake — its ladder runs on deadlines
        // plus dirty tokens, so a book event must reach the next tick's pass
        // there too. The set is a pure dirty marker; both paths consume it.
        for (token, _) in &mirrored_tokens {
            self.fast_dirty_tokens.insert(token.clone());
        }

        // KI-1: the market path must also settle resting maker orders the way
        // `book_snapshot` does — a crossing feed event is exactly when a
        // dry maker fill would happen at the venue, and skipping it made every
        // dry entry escalate to taker (a systematically more expensive economy).
        if self.config.mode.settles_locally() {
            for (token, _) in &mirrored_tokens {
                let ids: Vec<String> = self
                    .ome
                    .live_orders()
                    .into_iter()
                    .filter(|o| o.token_id == *token && rests_on_book(o.mode))
                    .map(|o| o.order_id.clone())
                    .collect();
                for id in ids {
                    self.try_maker_fill(&id, now_ms);
                }
            }
        }

        // Shadow Evolution observation. D-3 fidelity: the shadow is fed AFTER the
        // live engine consumes this very event, so a variant replays the SAME tick
        // instant — same book and same trend confirmation — the live strategy
        // decided on. Feeding it before `engine.on_data` (the old behaviour) gave
        // every variant the PREVIOUS book, a one-tick lag that mis-timed entries
        // and diluted the counterfactual. Capture what we need before `ev` moves.
        enum ShadowFeed {
            Book(String, i64),
            /// 盘口批次原子化：一个批次的整组 (token, 批次时间戳)。双胞胎按
            /// 批次逐本喂入（同一时间戳），与 live 的 Batch 应用保持一致。
            Books(Vec<(String, i64)>),
            Round(Vec<CryptoMarket>, i64),
        }
        let shadow_feed = if self.shadow_evolution.is_enabled() {
            match &ev {
                crate::engine::DataEvent::Book {
                    token_id,
                    now_ms: t,
                    ..
                }
                | crate::engine::DataEvent::TopOfBook {
                    token_id,
                    now_ms: t,
                    ..
                } => Some(ShadowFeed::Book(token_id.clone(), *t)),
                crate::engine::DataEvent::Books { books, now_ms: t } => Some(ShadowFeed::Books(
                    books.iter().map(|(t2, _, _)| (t2.clone(), *t)).collect(),
                )),
                crate::engine::DataEvent::RoundMarkets { markets, now_ms: t } => {
                    Some(ShadowFeed::Round(markets.clone(), *t))
                }
                _ => None,
            }
        } else {
            None
        };

        // Drive the live engine (when attached). Absence must not skip the shadow
        // round feed below, so this is an `if let`, not an early return.
        if let Some(engine) = self.engine.as_mut() {
            let broken = engine.on_data(ev);
            // E29 (§12.2): CLOSED bars push immediately — NEVER throttled (a
            // swallowed close would leave the chart missing that bar forever).
            // The aggregator yields each closed bar exactly once, so the
            // "一 bar 一条，一条不少" promise is this loop alone. Drained here,
            // while `engine` is borrowed — the emits below run after the
            // borrow ends (NLL), past the cancel loop.
            let closed_bars = engine.take_closed_klines();
            for (token, _price) in broken {
                // Regime change: pull resting entry bids for this token.
                let ids: Vec<String> = self
                    .ome
                    .live_orders()
                    .into_iter()
                    .filter(|o| o.side == Side::Buy && o.token_id == token)
                    .map(|o| o.order_id.clone())
                    .collect();
                for id in ids {
                    let _ = self.cancel(&id, now_ms);
                }
            }
            for bar in closed_bars {
                self.emit(Event::KlineUpdate { kline: bar });
            }
        }

        // Now feed the shadow the tick the live engine just consumed.
        match shadow_feed {
            Some(ShadowFeed::Book(token_id, t)) => {
                let book = self
                    .engine
                    .as_ref()
                    .and_then(|e| e.book_snapshot(&token_id));
                if let Some(book) = book {
                    self.shadow_evolution_on_tick(&token_id, &book, t);
                }
            }
            Some(ShadowFeed::Books(books)) => {
                // 同一批次逐本喂给双胞胎：双胞胎的 on_book 是逐本观测的
                // （与 live 策略相同），整批共享一个时间戳。
                for (token_id, t) in books {
                    let book = self
                        .engine
                        .as_ref()
                        .and_then(|e| e.book_snapshot(&token_id));
                    if let Some(book) = book {
                        self.shadow_evolution_on_tick(&token_id, &book, t);
                    }
                }
            }
            Some(ShadowFeed::Round(markets, t)) => {
                // Seed the twins with the same opening mids the live engine replays
                // for this round (engine.rs RoundMarkets), so confirmation clocks
                // start together instead of one book late.
                let tokens: Vec<String> = markets
                    .iter()
                    .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
                    .collect();
                let seeds: Vec<(String, crate::model::OrderbookSnapshot)> = self
                    .engine
                    .as_ref()
                    .map(|e| {
                        tokens
                            .into_iter()
                            .filter_map(|t| e.book_snapshot(&t).map(|b| (t, b)))
                            .collect()
                    })
                    .unwrap_or_default();
                self.shadow_evolution_on_round(&markets, &seeds, t)
            }
            None => {}
        }
    }

    /// 盘口批次原子化：批式拉取（polymarket `POST /books` 一类）的整批 book
    /// 一次性入核 —— 调用方必须已经持有 Core 锁（或本调用就是唯一持锁点），
    /// 批内所有 book 在同一次持锁区间里完成镜像 + 引擎应用，维护/评估 tick
    /// 不可能插进来读到「一半新一半旧」的撕裂状态。
    ///
    /// 与 [`Self::engine_on_data`] 的差异只有两点：归档为一条 `k:"books"` 批
    /// 行（新增的 kind，旧语料没有它、逐事件回放基线不受影响）；无
    /// `DataEvent` 归档/影子复用 —— 引擎应用走 [`Engine::on_data_books`]，
    /// 每本书与单事件路径同一段 apply/notify 代码。返回被趋势断裂命中的
    /// token 上的 resting 买单（已撤），供调用方记账。
    ///
    /// #548：批路径曾对归档与探针双盲 —— 归档永远没有 `k:"books"` 行
    /// （编码器在 data_source.rs，无人喂）、探针记不到批内任何 tick
    /// （zero-tick 名单全部假警、stream break 对批式 venue 永久失明）。
    /// 两写点对齐 `engine_on_data` 的 Book/Books 臂：先归档（回放所见 =
    /// 引擎所见），再在镜像前逐本 note_tick（与单事件路径同口径）。
    pub fn engine_on_books(&mut self, books: Vec<BatchBook>, ts_ms: i64) {
        // #351：与 engine_on_data 同序 —— 先推进在途 taker 腿，再让本批书
        // 落进引擎（腿按「事件到达时」的盘口撮合，永远不与本批同瞬）。
        self.drain_inflight_takers(ts_ms);

        // #548：归档整批为一条 `k:"books"` 批行 —— doc-comment 一直这么
        // 承诺，实现却没写。事件自己的时间戳是权威（engine_on_data 同规）。
        if let Some(a) = self.event_archive.as_mut() {
            a.record_at(
                ts_ms,
                &crate::engine::DataEvent::Books {
                    books: books.clone(),
                    now_ms: ts_ms,
                },
            );
        }

        // #548：批内每本都是一次行情 tick（与 engine_on_data 的 Books 臂
        // 同口径）。归档/探针都只读 books 的形状，镜像写入在两者之后。
        for (token, _, _) in &books {
            let venue = self.venue_of_token(token);
            self.probe_ticks.note_tick(token, &venue, ts_ms);
        }

        // 镜像（exit/mark 路径）：与 Batch 事件同语义 —— 每本 received_ms
        // 统一为批次时间戳。
        for (token_id, bids, asks) in &books {
            self.books.insert(
                token_id.clone(),
                crate::sim::Book {
                    bids: bids.clone(),
                    asks: asks.clone(),
                    received_ms: ts_ms,
                },
            );
        }
        for (token_id, _, _) in &books {
            self.fast_dirty_tokens.insert(token_id.clone());
        }
        // KI-1：与 engine_on_data 同规则 —— 本地结算内核在盘口事件落地时
        // 结算 resting maker 单（逐 token，跨整批）。
        if self.config.mode.settles_locally() {
            for (token_id, _, _) in &books {
                let ids: Vec<String> = self
                    .ome
                    .live_orders()
                    .into_iter()
                    .filter(|o| o.token_id == *token_id && rests_on_book(o.mode))
                    .map(|o| o.order_id.clone())
                    .collect();
                for id in ids {
                    self.try_maker_fill(&id, ts_ms);
                }
            }
        }

        // 引擎：整批一次应用。撕裂的根在逐 token 的两次独立应用之间 ——
        // 这里一次调用顺序应用整批，策略观测永远对齐同一批次快照。
        if let Some(engine) = self.engine.as_mut() {
            let broken = engine.on_data_books(&books, ts_ms);
            let closed_bars = engine.take_closed_klines();
            for (token, _price) in broken {
                // Regime change: pull resting entry bids for this token.
                let ids: Vec<String> = self
                    .ome
                    .live_orders()
                    .into_iter()
                    .filter(|o| o.side == Side::Buy && o.token_id == token)
                    .map(|o| o.order_id.clone())
                    .collect();
                for id in ids {
                    let _ = self.cancel(&id, ts_ms);
                }
            }
            for bar in closed_bars {
                self.emit(Event::KlineUpdate { kline: bar });
            }
        }

        // 影子演化：与 live 同一批次逐本喂入（时间戳统一）。
        if self.shadow_evolution.is_enabled() {
            for (token_id, _, _) in &books {
                let book = self.engine.as_ref().and_then(|e| e.book_snapshot(token_id));
                if let Some(book) = book {
                    self.shadow_evolution_on_tick(token_id, &book, ts_ms);
                }
            }
        }

        // 回放唤醒：批次是引擎输入，下一拍要真评估（与 Book 事件同效果，
        // 计数在 stats match 外由调用方 on_books 完成 —— 这里补计数）。
        self.stats.books += books.len() as u64;
        if self.replay_sched() {
            self.fast_eval_wake = true;
        }
    }

    /// Run one evaluation cycle: engine produces entry orders, core places them.
    /// Entries go through the usual risk/capacity gates; rejections are skipped.
    /// A configured per-strategy cap (P-1.1) is checked before the order layer.
    /// Replay fast path: whether THIS cycle must run a real Lua evaluation —
    /// an engine input arrived since the last one (`fast_eval_wake`), a
    /// round-timing boundary the gates read is due (`fast_next_timing_ms`),
    /// or the fallback window elapsed. Between those, the same inputs
    /// reproduce the same output, so the call is skipped.
    fn fast_eval_due(&self, now_ms: i64) -> bool {
        self.fast_eval_wake
            || now_ms >= self.fast_next_timing_ms
            || now_ms - self.fast_last_eval_ms >= FAST_EVAL_FALLBACK_MS
    }

    pub fn engine_evaluate(&mut self, now_ms: i64) -> usize {
        if self.engine.is_none() {
            return 0;
        }
        self.stats.evaluations += 1;
        // Replay fast path: a quiet tick skips the Lua call entirely. Between
        // engine inputs (book/top/spot/rollover — the dirty wake) the only
        // strategy inputs that move are the round clock's integer-second
        // fields, and their gate verdicts cannot flip inside the open
        // interval the boundary wake (`fast_next_timing_ms`) covers; the
        // fallback bounds anything the boundary does not model to 2 s.
        // The evaluations COUNTER still advances every cycle: the sparse-
        // cadence contract (`sparse_stream_evaluates_every_due_cycle`) pins
        // the scheduled cycle count, not the wall cost of one cycle.
        // #390: the nofast replay runs the same gates through
        // `replay_scheduling` — the strategy surface it observes (signals,
        // fills, trades) is identical, and the eval-call health counter is
        // host diagnostic, not replay semantics.
        if self.replay_sched() && !self.shadow_evolution.is_enabled() && !self.fast_eval_due(now_ms)
        {
            // Issue #390 (replay nofast): a QUIET cycle. The base walk ran a
            // full Lua collection here and re-submitted its suggestions into
            // the kernel-side gates; this cycle re-runs the STANDING
            // suggestions of the last real evaluation through the SAME
            // kernel-side pipeline instead — the gate/counter/log/placement
            // stream is reproduced cycle for cycle without the Lua call
            // (see `engine_replay_cycle`). A stable-phase cache answers with
            // orders; an unstable one (a phase transition the last real
            // evaluation introduced — a latch fired, an emission consumed)
            // refuses, and THIS cycle runs a real evaluation: the base's
            // next collection would collect the post-transition output.
            // The wake flags stay armed either way: a quiet cycle consumes
            // neither the input wake nor the timing boundary — both belong
            // to the next REAL evaluation.
            if let Some(orders) = self.engine_replay_cycle(now_ms) {
                return orders;
            }
        }
        if self.replay_sched() {
            // A real evaluation consumes the wake; the timing boundary is
            // recomputed after the engine borrow ends (below).
            self.fast_eval_wake = false;
            self.fast_last_eval_ms = now_ms;
            self.fast_next_timing_ms = i64::MAX;
        }
        // Shadow Evolution: evaluate FIRST so any applied hot-swap is in force
        // for this very cycle's order decision (next-tick semantics, no restart).
        self.shadow_evolution_evaluate(now_ms);
        // E13: the 72h DEEP round rides the same cadence, so a long-running
        // process re-anchors its variant sets (compound mutants) without an
        // operator. The check itself is one comparison per cycle.
        self.shadow_evolution_maybe_cycle(now_ms);
        // #390: read before the engine borrow opens (the re-arm below uses it
        // while `engine` is still mutably borrowed).
        let replay_sched = self.replay_sched();
        // The position snapshot strategies see via `bk.holdings()` — built
        // before the engine borrow opens (same once-per-cycle cadence as the
        // equity push below).
        let held_view = self.held_position_view();
        // RFC 568 阶段一：本周期每策略的配对簿视图快照（owner 过滤在 Core
        // 侧做完 —— 实例主账户 + arb 腿账户白名单，与 holdings 视图拓宽同
        // 形）。也在引擎借用打开前构建；策略层经 `bk.arb.pairs()` 只读
        // （最终一致，附注 2）。
        let pair_views: Vec<crate::pair::PairView> = self
            .engine
            .as_ref()
            .map(|e| {
                e.hosted()
                    .iter()
                    .filter(|s| s.enabled)
                    .map(|s| {
                        let mut accounts: HashSet<String> = HashSet::new();
                        accounts.insert(s.account_id.clone());
                        if let Some(ls) = s.strategy.arb_leg_accounts() {
                            accounts.extend(ls.iter().cloned());
                        }
                        (s.strategy.name().to_string(), accounts)
                    })
                    .collect::<Vec<(String, HashSet<String>)>>()
            })
            .unwrap_or_default()
            .into_iter()
            .flat_map(|(name, accounts)| self.pair_views_for(&name, &accounts))
            .collect();
        let engine = self.engine.as_mut().expect("engine present");
        // #202: the equity-relative sizing is a percentage of the account, and
        // the account is the ledger — pushed here, once per cycle, so a ticket
        // can never be sized against a balance the process no longer has.
        // E28: the legacy scalar mirrors the ACTIVE account's book; the
        // per-scope pushes below carry every configured account's own book,
        // trading switch, and venue binding, so each scope sizes/gates
        // against ITS account. (§9.4: the kernel drives strategies; per-
        // account routing happens when each order passes through the gates.)
        engine.set_equity_usd(self.accounts.active_ledger().balance());
        for a in self.accounts.account_list() {
            let id = a.id.as_str();
            let equity = self
                .accounts
                .ledger_of(&a.id)
                .map(|l| l.balance())
                .unwrap_or_default();
            engine.set_account_equity(id, equity);
            engine.set_account_trading(id, a.trading_enabled);
            engine.set_account_venue(id, &a.venue);
            engine.set_account_frozen(id, self.account_freezes.contains_key(id));
        }
        engine.set_positions_view(held_view);
        // 视图落引擎的 per-cycle 快照（evaluate 的 ctx 构造从这取）。
        engine.set_pair_views_snapshot(pair_views);
        let orders = engine.evaluate(now_ms);
        // Replay fast path: recompute when the round-timing verdict can next
        // flip — quiet ticks skip the Lua call until then (or the fallback
        // beat, or a new engine input). #390: nofast replay re-arms the same
        // boundary. (`replay_sched` was read before the engine borrow opened.)
        if replay_sched {
            self.fast_next_timing_ms = engine.next_timing_boundary_ms(now_ms).unwrap_or(i64::MAX);
        }
        // E30 (§6.3): drain the sandbox poison alerts now; each is raised as
        // ONE RISK_ALERT after the engine borrow ends below — the message
        // names the strategy, the sandbox itself refuses every later call
        // from it, and the kernel keeps running (a poisoned strategy is
        // quarantined, not a crash).
        // #363: the round timing this cycle was judged against, copied out
        // while the engine borrow is alive so the policy facts below do not
        // extend it (the engine reference must end before `self.emit`).
        let policy_time_left_sec = engine.last_time_left_sec();
        let poison_alerts = engine.drain_poison_alerts();
        for (name, alert) in &poison_alerts {
            tracing::error!(strategy = %name, %alert, "lua strategy poisoned; all further calls refused");
        }
        // Close intents produced by strategies this cycle join the SAME exit
        // submission path as policy exits (handled in run_exit_checks).
        self.strategy_exits.extend(engine.drain_strategy_exits());
        engine.tally_blocked();
        // E2-b: an honoured gate exemption is always logged (and counted), so an
        // opt-out is auditable rather than a silent hole in the entry gates.
        for rec in engine.take_exemptions() {
            tracing::info!(target: "strategy", "{}", rec.audit_line());
        }
        // RFC 568 阶段一：A≠B 拒绝回执与配对声明都在引擎借用内先收下来
        // （回执 → APPLICATION 事件、声明 → precheck 过闸后 declare，都在
        // 借用结束后 —— 与 poison_alerts 的先收后发同一模式）。
        let pair_refusals = engine.take_pair_refusals();
        let pair_declarations = engine.take_pair_declarations();
        for (name, alert) in poison_alerts {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!("lua strategy '{name}' poisoned: {alert}"),
            });
        }
        self.stats.signals += orders.len() as u64;
        let mut tokens: Vec<(String, crate::model::OrderRequest)> = orders
            .into_iter()
            .map(|o| (o.token_id.clone(), o))
            .collect();
        // ── RFC 568 阶段一（补钉 5 的消费侧）：A≠B 校验的拒绝回执 →
        // APPLICATION 错误事件（点名字段 pair.key）。订单路径照旧 —— 校验
        // 拒的是「声明」，不是腿（裸腿由 arb_precheck 的整组语义与内核去重
        // 兜底）；声明被拒 ⇒ 该腿不进 PairBook。
        for reason in pair_refusals {
            self.emit(Event::Error {
                error: CoreError::new(CoreErrorCode::InvalidParams, reason),
            });
        }
        // ── E-VA: the atomic-batch pre-reservation, judged BEFORE the first
        // leg's gate runs — the whole-cycle dimensions (credentials/scope,
        // RFC #541 hook B) plus the per-account pot dimension. Strategies
        // that declared `atomic_batch` (and sit in `virtual_limits`) reserve
        // their WHOLE batch against the virtual pot: an over-cap batch is
        // cancelled wholesale — zero legs placed, zero real reservations
        // taken — with the refusal bucketed through the existing
        // StrategyLimit accounting. Everything else (every shipped strategy,
        // every undeclared batch) finds no governed scope and skips
        // untouched.
        self.arb_precheck_filter(&mut tokens);
        // ── RFC 568 阶段一（补钉 3 的落点）：声明受理在 arb_precheck 过闸
        // 之后 —— 预检拒整批 ⇒ 不入账（凭证维度由预检管，声明不重复校验）；
        // 过闸 ⇒ 逐腿 declare 进 PairBook。存活判定直接用 precheck 后的
        // 幸存单集（strategy, 落账账户, token）三元组唯一对应一条腿：scope
        // 整组丢、策略丢、去重吞掉的声明都不入账（配对簿只记将要发生的事
        // 实的配对骨架）；经济身份 (asset, slot) 在 pair_declare_leg 里从腿
        // token 经 scanner 派生。
        let survived: HashSet<(String, String, String)> = tokens
            .iter()
            .map(|(_, req)| {
                (
                    req.strategy.clone(),
                    req.account_id.to_string(),
                    req.token_id.clone(),
                )
            })
            .collect();
        for d in pair_declarations {
            if !survived.contains(&(
                d.owner_strategy.clone(),
                d.leg_account.clone(),
                d.token_id.clone(),
            )) {
                continue;
            }
            self.pair_declare_leg(&d, now_ms);
        }
        let mut placed = 0;
        // ── #363: the execution policy's account-level verdict, judged BEFORE
        // arbitration. Two decisions ride here:
        //
        //   1. `Skip` (a rule hit or the equity floor) — the intent never
        //      reaches `place()`, so it never reaches the RiskGate either;
        //      the refusal is bucketed `policy.skip` and audited with the
        //      rule's own reason. This is the ONLY place a policy verdict
        //      removes an order: everything downstream (RiskGate, breakers,
        //      capacity, reserve, OME) runs untouched — "规则不能绕过
        //      RiskEngine" holds because the policy can only REMOVE an
        //      intent, never relax a gate or resize past the risk cap.
        //   2. `Place { budget_usd }` — the budget is an UPPER BOUND: the
        //      request's own share band (min_shares/max_shares, engine
        //      sizing) still decides the ticket, and the policy shrinks it
        //      when the shares cost more than the budget allows. With the
        //      shipped defaults (ratio 0.10, floor 1, ceiling 50) the bound
        //      sits at or above every ticket the share band can produce, so
        //      no order changes size — exit-economics parity by construction.
        let policy_verdicts: HashMap<
            String,
            Result<crate::execution_policy::PolicyOutput, crate::execution_policy::PolicyRefusal>,
        > = if self.execution_policy.is_some() {
            // The open_positions fact counts LIVE positions only — an expired
            // round's stuck position is settlement-dead (same rule as
            // PositionManager::can_open) and must not trip the shipped
            // `open_positions >= N` backstop.
            let open = self.positions.open_positions();
            let open_for = |acct: &str| {
                open.iter()
                    .filter(|p| p.account_id.as_str() == acct && p.expires_at_ms > now_ms)
                    .count() as u32
            };
            tokens
                .iter()
                .map(|(_, req)| {
                    let facts = crate::execution_policy::PolicyInput {
                        available_balance: self
                            .accounts
                            .ledger_of(&req.account_id)
                            .map(|l| l.balance())
                            .unwrap_or_default(),
                        total_equity: self
                            .accounts
                            .ledger_of(&req.account_id)
                            .map(|l| l.balance())
                            .unwrap_or_default(),
                        open_positions: open_for(req.account_id.as_str()),
                        current_price: req.price,
                        time_left_sec: policy_time_left_sec,
                        symbol: req.asset.clone(),
                        recent_pnl_1h: self
                            .positions
                            .recent_pnl_1h_for(req.account_id.as_str(), now_ms),
                        consecutive_losses: self.breaker.consecutive_losses(&req.strategy),
                    };
                    let verdict = crate::execution_policy::evaluate(
                        self.execution_policy.as_ref().expect("checked above"),
                        req.account_id.as_str(),
                        &facts,
                    );
                    (req.account_id.to_string() + ":" + &req.token_id, verdict)
                })
                .collect()
        } else {
            HashMap::new()
        };
        // ── v0.3 Wave 0 (#329): the E25 arbitration delegation seam ──────────
        // E25 inserts its one-line per-intent delegation at the head of the
        // loop below: `process_intent` (arbitration/) runs BEFORE the E16
        // portfolio cap, so every strategy intent is arbitrated exactly once
        // and the SAME Request that cleared the gates is the one handed to
        // `self.place(...)`. Wave 0 lands the seam only — a comment, not a
        // call: `arbitration/` does not exist yet and this PR is
        // behavior-free by charter (deviation disclosed in the PR body).
        for (token, mut req) in tokens {
            let name = req.strategy.clone();
            // ── #406: the zero/negative-price fail-closed guard — the FIRST
            // placement gate, before the policy arm. A strategy that names a
            // price <= 0 on an entry is a malformed intent (a prediction
            // market has no legal quote at or below zero), and every price
            // the ticket touches downstream DIVIDES by it: the policy's
            // budget shrink (`budget_usd / req.price`, service.rs Place arm)
            // is the earliest such division, and a zero price PANICKED the
            // process there (rust_decimal Division by zero) before the
            // arbitration Gate 1 price-band check — which would have refused
            // this intent — ever ran. Fail closed instead: the entry is
            // refused with its own counter and cause bucket, the process
            // keeps running, and the audit trail names the strategy.
            // Closing intents are exempt: an exit prices off the book and
            // position state, never off the strategy's entry quote, and a
            // held position must always be closable.
            if req.side == crate::model::Side::Buy && req.price <= Decimal::ZERO {
                tracing::error!(target: "strategy",
                    "entry rejected: strategy={name} cause=illegal.price token={} price={} — \
                     non-positive entry price is refused fail-closed (#406): it would divide \
                     by zero in sizing/policy/physics",
                    req.token_id, req.price);
                self.stats.place_rejected += 1;
                self.stats.illegal_price_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.rejected += 1;
                *acc.rejection_causes
                    .entry("illegal.price".to_string())
                    .or_default() += 1;
                continue;
            }
            // ── #363: account-level cooldown gate. A rule that returned
            // `Cooldown` last cycle parked the ACCOUNT here for N seconds:
            // every entry this account names is refused until the clock runs
            // out (the clock is the caller's — the rule states a duration,
            // the kernel owns the time). Expired entries are dropped on
            // sight, and nothing about exits is slowed: only entries.
            if let Some(&until) = self.policy_cooldowns.get(req.account_id.as_str()) {
                if until > now_ms {
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause=policy.cooldown account={} \
                         {}s left", req.account_id, (until - now_ms + 999) / 1000);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                self.policy_cooldowns.remove(req.account_id.as_str());
            }
            // ── #363: the policy verdict for THIS intent (judged above, before
            // any gate). A Skip refuses here — the intent never reaches the
            // RiskGate, which is the only way a policy may touch an order: it
            // removes, never relaxes. A Place caps the ticket size.
            match policy_verdicts
                .get(&format!("{}:{}", req.account_id, req.token_id))
                .map(|v| v.as_ref())
            {
                Some(Ok(crate::execution_policy::PolicyOutput::Skip { reason })) => {
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause=policy.skip reason={reason}");
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                Some(Ok(crate::execution_policy::PolicyOutput::Cooldown { seconds })) => {
                    let until = now_ms + seconds * 1000;
                    self.policy_cooldowns
                        .insert(req.account_id.to_string(), until);
                    tracing::info!(target: "strategy",
                        "policy cooldown: account={} {}s (entries resume at {})",
                        req.account_id, seconds, until);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                Some(Ok(crate::execution_policy::PolicyOutput::Place { budget_usd, .. })) => {
                    // The budget is an UPPER BOUND on the ticket, not a
                    // replacement for the sizing path: the engine's share band
                    // already decided `req.size`, and the policy only shrinks it
                    // when the shares cost more than the account's budget allows.
                    // With the shipped defaults the bound never bites (see the
                    // comment at the verdict map), so no existing order changes.
                    let shares = (budget_usd / req.price).floor();
                    if shares >= Decimal::ONE && req.size > shares {
                        tracing::info!(target: "strategy",
                            "policy budget shrink: strategy={} account={} {} -> {shares} shares \
                             (budget {budget_usd} USD)",
                            name, req.account_id, req.size);
                        req.size = shares;
                    }
                    // A budget that cannot buy one whole share emits nothing:
                    // a 0-size order would only turn the refusal into a venue
                    // rejection (the #202 lesson, applied to the policy path).
                    if shares < Decimal::ONE {
                        tracing::info!(target: "strategy",
                            "entry rejected: strategy={name} cause=policy.skip reason=\
                             budget {budget_usd} USD cannot buy one share at {}",
                            req.price);
                        self.stats.place_rejected += 1;
                        let acc = self
                            .strategy_accounting
                            .entry(strategy_acc_key(&name, req.account_id.as_str()))
                            .or_default();
                        acc.rejected += 1;
                        *acc.rejection_causes
                            .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                            .or_default() += 1;
                        continue;
                    }
                }
                Some(Err(refusal)) => {
                    // "account_id 缺失 → 拒绝下单": the policy could not judge
                    // this intent at all. Refuse loudly, never fall through to
                    // defaults.
                    tracing::warn!(target: "strategy",
                        "entry refused: strategy={name} account={} policy refusal: {}",
                        req.account_id, refusal.reason);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                None => {}
            }
            // ── E25 (#331): arbitration BEFORE the E16 portfolio cap — this is
            // the seam Wave 0 planted. Every strategy intent is arbitrated
            // exactly once through the four gates; a Rejected suggestion never
            // reaches place()/OME, an Approved one is handed to the SAME path
            // as before. The gates reuse the existing implementations
            // (OrderIntent::validate / RiskGate / breakers / Ledger::reserve /
            // effective_stop_pct), so no threshold has two truths (§3.3).
            // E28 (§9.2): the intent is arbitrated in the account its own
            // request names — the pipeline's reserve lands in THAT book, not
            // the active one. The posture is judged before the request moves
            // into the intent.
            let account_id = req.account_id.clone();
            let is_close = crate::risk::is_close_intent(&req.internal_key);
            let account_entry_block = self
                .accounts
                .permits_order(&account_id, is_close)
                .err()
                .map(|e| e.message);
            let intent = crate::arbitration::StrategyIntent::from_request(req);
            let outcome = {
                let round_tokens = self
                    .engine
                    .as_ref()
                    .map(|e| e.round_token_ids())
                    .unwrap_or_default();
                let time_left_sec = self
                    .engine
                    .as_ref()
                    .map(|e| e.last_time_left_sec())
                    .unwrap_or(0);
                // E26 (§4.2) + #461: the systemic aggregates are read BEFORE
                // the account borrow — resting BUY notional joins the
                // exposure (cash already promised).
                let systemic_facts = self.systemic_facts(&intent.request.asset, now_ms);
                // E28 (§9.3): the pipeline's reserve happens in THIS account's
                // book. An unknown account is refused — no implicit empty book,
                // ever (reverse acceptance C) — with the same Gate-2 accounting
                // as an arbitration rejection.
                match self.accounts.get_mut(&account_id) {
                    Err(e) => crate::arbitration::Outcome {
                        decision: crate::arbitration::Decision::Rejected {
                            reason: crate::arbitration::RejectReason::AccountLimit,
                            gate: crate::arbitration::GateId::Risk,
                            detail: e.message,
                        },
                        gates: Vec::new(),
                        latency_us: 0,
                    },
                    Ok(ledger) => {
                        // E28: the #202 equity is a percentage of THIS
                        // account's live balance, same rule as the submission
                        // path.
                        let equity = ledger.balance();
                        let mut ctx = crate::arbitration::IntentCtx {
                            round_tokens: &round_tokens,
                            risk: &self.risk,
                            breaker: &self.breaker,
                            ledger,
                            exit_cfg: &self.config.positions.exit,
                            time_left_sec,
                            now_ms,
                            equity,
                            account_entry_block,
                            systemic_facts,
                        };
                        crate::arbitration::process_intent(&intent, &mut ctx)
                    }
                }
            };
            self.record_intent_decision(&intent, &outcome, now_ms);
            if let crate::arbitration::Decision::Rejected {
                reason,
                gate,
                detail,
                ..
            } = &outcome.decision
            {
                self.stats.place_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, intent.request.account_id.as_str()))
                    .or_default();
                acc.rejected += 1;
                // Bucket through the EXISTING classifier over the gate's own
                // message, so the attribution vocabulary is the one place()
                // would have written — the rejection happened earlier, the
                // story an operator reads does not change (E9-c buckets).
                let cause = classify_rejection(&crate::model::CoreError::new(
                    crate::arbitration::reason_code(*reason),
                    detail.clone(),
                ));
                *acc.rejection_causes.entry(cause).or_default() += 1;
                tracing::info!(target: "strategy",
                    "entry rejected: strategy={name} cause=arbitration gate={gate:?} reason={reason:?} detail={detail}");
                continue;
            }
            let mut req = intent.request;
            // E26 (§4.2): a systemic shrink rides the decision as MODIFIED —
            // the audit names the bound that bit; here the reduced size is
            // what gets placed. The pipeline never rewrote the request (the
            // suggestion's provenance stays exactly what the engine emitted),
            // so the shrink is applied at the one point that spends money.
            if let crate::arbitration::Decision::Modified {
                modification: crate::arbitration::Modification::SizeReduced { approved, .. },
                ..
            } = &outcome.decision
            {
                tracing::info!(target: "strategy",
                    "entry modified: strategy={name} systemic shrinks {} -> {approved} shares",
                    req.size);
                req.size = *approved;
            }
            // E16/#98 portfolio-level exposure cap (0 = off): the account's
            // total open commitment across ALL strategies is bounded too —
            // per-strategy caps partition it, but their sum needs its own
            // ceiling. The check ADDS a constraint; the RiskGate hard bounds
            // (kill switch, price band, per-order cap) are untouched.
            if let Err(reason) = self.portfolio_limit_ok(&req) {
                self.stats.strategy_limit_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes
                    .entry("limit.portfolioNotionalCap".into())
                    .or_default() += 1;
                tracing::info!(target: "strategy", "entry rejected: strategy={name} cause=limit.portfolioNotionalCap reason={reason}");
                continue;
            }
            if let Some(limit) = self.config.strategy_limits.get(&name).cloned()
                && let Err(reason) = self.strategy_limit_ok(&req, &limit, now_ms)
            {
                // E9-c: the reason existed before but was discarded — bucket
                // it so operator tooling can answer "why did this strategy
                // stop placing?".
                let bucket = if reason.contains("position cap") {
                    "limit.positionCap"
                } else if reason.contains("notional cap") {
                    "limit.notionalCap"
                } else {
                    "limit.other"
                };
                self.stats.strategy_limit_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes.entry(bucket.into()).or_default() += 1;
                continue;
            }
            // E-VA: the batch pre-check passed — HOLD this leg's virtual
            // quota before it spends anything real. A later per-order gate
            // may still refuse the leg: the Err arm below releases its keyed
            // reservation, and a placed leg keeps its hold until the order
            // state machine's terminal transition releases it.
            {
                let account_id = req.account_id.to_string();
                let strategy = req.strategy.clone();
                self.virtual_batch_commit(
                    &account_id,
                    &strategy,
                    &[(req.internal_key.clone(), req.price * req.size)],
                );
            }
            let placed_account = req.account_id.clone();
            let virtual_leg = if self
                .virtual_accounts
                .governed(placed_account.as_str(), &name)
            {
                Some((req.internal_key.clone(), req.price * req.size, req.side))
            } else {
                None
            };
            // RFC 568 阶段一：place 前回填 internal_key —— 声明腿与 OME 订单
            // 的事实钥匙挂钩（成交/死亡事件经它寻址回这条腿）。只对 BUY 入腿
            // 有意义（SELL 是退出，不入配对簿）；未声明的 token 是空操作。
            if req.side == Side::Buy {
                self.pair_bind_key(&name, placed_account.as_str(), &token, &req.internal_key);
            }
            match self.place(req, self.config.entry_maker_timeout_ms, now_ms) {
                Ok((_id, _)) => {
                    self.strategy_accounting
                        .entry(strategy_acc_key(&name, placed_account.as_str()))
                        .or_default()
                        .placed += 1;
                    if let Some(engine) = self.engine.as_mut() {
                        engine.note_scoped_order_placed(placed_account.as_str(), &token);
                    }
                    placed += 1;
                }
                Err(err) => {
                    // E-VA: a refused leg never rests — release the keyed
                    // virtual hold its batch took, so the pot reflects only
                    // commitments that actually exist.
                    if let Some((key, _notional, side)) = &virtual_leg
                        && *side == Side::Buy
                    {
                        self.virtual_accounts.release(
                            placed_account.as_str(),
                            &name,
                            key,
                            *_notional,
                        );
                    }
                    self.stats.place_rejected += 1;
                    let bucket = classify_rejection(&err);
                    // E9-c: attach the live reason text verbatim to the receipt
                    // log line so per-strategy accounting and logs tell the
                    // same story.
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause={bucket} reason={}",
                        err.message);
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, placed_account.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes.entry(bucket).or_default() += 1;
                }
            }
        }
        placed
    }

    /// Issue #390 (replay nofast): one QUIET evaluation cycle — the base
    /// walk's every-tick Lua collection replaced by a re-submission of the
    /// standing candidates through the SAME kernel-side pipeline
    /// (`engine.replay_candidates` → the shared submit half). Everything the
    /// base cycle produced from the re-collected suggestions — the
    /// `signals`/`place_rejected` counters, the per-strategy accounting and
    /// rejection buckets, the entry-rejected log lines, the gate verdicts and
    /// their placement instants — is reproduced cycle for cycle without the
    /// Lua call; the counters are the schedule's, not the wall cost's
    /// (mirroring `replay_credit_cycles`). Deliberately NOT touched: the eval
    /// wake flags (a quiet cycle consumes neither the input wake nor the
    /// timing boundary — both belong to the next REAL evaluation, whose
    /// Lua collection refreshes the staged set), the shadow-evolution cadence
    /// (a jumping driver is refused while it is enabled), and the eval-call
    /// health counters (host diagnostic, exempt host fields).
    /// Issue #390 (replay nofast): one QUIET evaluation cycle — the base
    /// walk's every-tick Lua collection replaced by a re-submission of the
    /// standing candidates through the SAME kernel-side pipeline
    /// (`engine.replay_candidates` → the shared submit half). Everything the
    /// base cycle produced from the re-collected suggestions — the
    /// `signals`/`place_rejected` counters, the per-strategy accounting and
    /// rejection buckets, the entry-rejected log lines, the gate verdicts and
    /// their placement instants — is reproduced cycle for cycle without the
    /// Lua call; the counters are the schedule's, not the wall cost's
    /// (mirroring `replay_credit_cycles`). Returns `None` when the staged
    /// set is not in a stable phase (the caller falls back to a real
    /// evaluation). Deliberately NOT touched: the eval wake flags (a quiet
    /// cycle consumes neither the input wake nor the timing boundary — both
    /// belong to the next REAL evaluation, whose Lua collection refreshes the
    /// staged set), the shadow-evolution cadence (a jumping driver is refused
    /// while it is enabled), and the eval-call health counters (host
    /// diagnostic, exempt host fields).
    fn engine_replay_cycle(&mut self, now_ms: i64) -> Option<usize> {
        let held_view = self.held_position_view();
        let staged = self
            .engine
            .as_mut()
            .map(|e| {
                // #202: the same per-cycle equity push the real cycle makes,
                // so a replayed ticket is sized against the balance the
                // process has NOW. The per-scope mirrors ride along (the
                // replay book is single-default — the loop is one iteration,
                // byte-identical outcomes with the pre-scope shape).
                e.set_equity_usd(self.accounts.active_ledger().balance());
                for a in self.accounts.account_list() {
                    let id = a.id.as_str();
                    let equity = self
                        .accounts
                        .ledger_of(&a.id)
                        .map(|l| l.balance())
                        .unwrap_or_default();
                    e.set_account_equity(id, equity);
                    e.set_account_trading(id, a.trading_enabled);
                    e.set_account_venue(id, &a.venue);
                    e.set_account_frozen(id, self.account_freezes.contains_key(id));
                }
                e.set_positions_view(held_view);
                e.replay_candidates(now_ms)
            })
            .unwrap_or_else(|| Some(Vec::new()))?;
        let orders = staged;
        self.stats.signals += orders.len() as u64;
        let mut tokens: Vec<(String, crate::model::OrderRequest)> = orders
            .into_iter()
            .map(|o| (o.token_id.clone(), o))
            .collect();
        // ── E-VA: the SAME whole-cycle pre-check the real cycle runs (issue
        // #390's parity rule: the nofast twin reproduces the base cycle's
        // kernel-side pipeline cycle for cycle). Ungoverned scopes
        // short-circuit before touching anything.
        self.arb_precheck_filter(&mut tokens);
        // The physics projection the base cycle judged (E25): refreshed for
        // THIS cycle's round clock by `replay_candidates`.
        let policy_time_left_sec = self
            .engine
            .as_ref()
            .map(|e| e.last_time_left_sec())
            .unwrap_or(0);
        let mut placed = 0;
        for (token, mut req) in tokens {
            let name = req.strategy.clone();
            // ── #363: the policy verdict for THIS intent. A Skip refuses here
            // — the intent never reaches the RiskGate. A Place caps the
            // ticket size. The shipped defaults never bite (see the comment
            // at the real cycle's verdict map).
            let policy = self.execution_policy.clone();
            let policy_verdict = policy.map(|policy| {
                // open_positions counts LIVE positions only (expired rounds
                // are settlement-dead; see engine_evaluate's verdict map).
                let open = self.positions.open_positions();
                let open_for = |acct: &str| {
                    open.iter()
                        .filter(|p| p.account_id.as_str() == acct && p.expires_at_ms > now_ms)
                        .count() as u32
                };
                let facts = crate::execution_policy::PolicyInput {
                    available_balance: self
                        .accounts
                        .ledger_of(&req.account_id)
                        .map(|l| l.balance())
                        .unwrap_or_default(),
                    total_equity: self
                        .accounts
                        .ledger_of(&req.account_id)
                        .map(|l| l.balance())
                        .unwrap_or_default(),
                    open_positions: open_for(req.account_id.as_str()),
                    current_price: req.price,
                    time_left_sec: policy_time_left_sec,
                    symbol: req.asset.clone(),
                    recent_pnl_1h: self
                        .positions
                        .recent_pnl_1h_for(req.account_id.as_str(), now_ms),
                    consecutive_losses: self.breaker.consecutive_losses(&req.strategy),
                };
                crate::execution_policy::evaluate(&policy, req.account_id.as_str(), &facts)
            });
            match policy_verdict {
                Some(Ok(crate::execution_policy::PolicyOutput::Skip { reason })) => {
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause=policy.skip reason={reason}");
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                Some(Ok(crate::execution_policy::PolicyOutput::Cooldown { seconds })) => {
                    let until = now_ms + seconds * 1000;
                    self.policy_cooldowns
                        .insert(req.account_id.to_string(), until);
                    tracing::info!(target: "strategy",
                        "policy cooldown: account={} {}s (entries resume at {})",
                        req.account_id, seconds, until);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                Some(Ok(crate::execution_policy::PolicyOutput::Place { budget_usd, .. })) => {
                    let shares = (budget_usd / req.price).floor();
                    if shares >= Decimal::ONE && req.size > shares {
                        tracing::info!(target: "strategy",
                            "policy budget shrink: strategy={} account={} {} -> {shares} shares \
                             (budget {budget_usd} USD)",
                            name, req.account_id, req.size);
                        req.size = shares;
                    }
                    if shares < Decimal::ONE {
                        tracing::info!(target: "strategy",
                            "entry rejected: strategy={name} cause=policy.skip reason=\
                             budget {budget_usd} USD cannot buy one share at {}",
                            req.price);
                        self.stats.place_rejected += 1;
                        let acc = self
                            .strategy_accounting
                            .entry(strategy_acc_key(&name, req.account_id.as_str()))
                            .or_default();
                        acc.rejected += 1;
                        *acc.rejection_causes
                            .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                            .or_default() += 1;
                        continue;
                    }
                }
                Some(Err(refusal)) => {
                    tracing::warn!(target: "strategy",
                        "entry refused: strategy={name} account={} policy refusal: {}",
                        req.account_id, refusal.reason);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                None => {}
            }
            // ── E25 (#331): the arbitration seam, identical to the real
            // cycle's (the gates reuse the existing implementations, so no
            // threshold has two truths).
            let account_id = req.account_id.clone();
            let is_close = crate::risk::is_close_intent(&req.internal_key);
            let account_entry_block = self
                .accounts
                .permits_order(&account_id, is_close)
                .err()
                .map(|e| e.message);
            let intent = crate::arbitration::StrategyIntent::from_request(req);
            let outcome = {
                let round_tokens = self
                    .engine
                    .as_ref()
                    .map(|e| e.round_token_ids())
                    .unwrap_or_default();
                // #461: the systemic aggregates are read BEFORE the account
                // borrow — resting BUY notional joins the exposure.
                let systemic_facts = self.systemic_facts(&intent.request.asset, now_ms);
                // E28 (§9.3): the reserve happens in THIS account's book.
                match self.accounts.get_mut(&account_id) {
                    Err(e) => crate::arbitration::Outcome {
                        decision: crate::arbitration::Decision::Rejected {
                            reason: crate::arbitration::RejectReason::AccountLimit,
                            gate: crate::arbitration::GateId::Risk,
                            detail: e.message,
                        },
                        gates: Vec::new(),
                        latency_us: 0,
                    },
                    Ok(ledger) => {
                        let equity = ledger.balance();
                        let mut ctx = crate::arbitration::IntentCtx {
                            round_tokens: &round_tokens,
                            risk: &self.risk,
                            breaker: &self.breaker,
                            ledger,
                            exit_cfg: &self.config.positions.exit,
                            time_left_sec: policy_time_left_sec,
                            now_ms,
                            equity,
                            account_entry_block,
                            systemic_facts,
                        };
                        crate::arbitration::process_intent(&intent, &mut ctx)
                    }
                }
            };
            self.record_intent_decision(&intent, &outcome, now_ms);
            if let crate::arbitration::Decision::Rejected {
                reason,
                gate,
                detail,
                ..
            } = &outcome.decision
            {
                self.stats.place_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, intent.request.account_id.as_str()))
                    .or_default();
                acc.rejected += 1;
                let cause = classify_rejection(&crate::model::CoreError::new(
                    crate::arbitration::reason_code(*reason),
                    detail.clone(),
                ));
                *acc.rejection_causes.entry(cause).or_default() += 1;
                tracing::info!(target: "strategy",
                    "entry rejected: strategy={name} cause=arbitration gate={gate:?} reason={reason:?} detail={detail}");
                continue;
            }
            let mut req = intent.request;
            if let crate::arbitration::Decision::Modified {
                modification: crate::arbitration::Modification::SizeReduced { approved, .. },
                ..
            } = &outcome.decision
            {
                tracing::info!(target: "strategy",
                    "entry modified: strategy={name} systemic shrinks {} -> {approved} shares",
                    req.size);
                req.size = *approved;
            }
            // E16/#98 portfolio-level exposure cap (0 = off).
            if let Err(reason) = self.portfolio_limit_ok(&req) {
                self.stats.strategy_limit_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes
                    .entry("limit.portfolioNotionalCap".into())
                    .or_default() += 1;
                tracing::info!(target: "strategy", "entry rejected: strategy={name} cause=limit.portfolioNotionalCap reason={reason}");
                continue;
            }
            if let Some(limit) = self.config.strategy_limits.get(&name).cloned()
                && let Err(reason) = self.strategy_limit_ok(&req, &limit, now_ms)
            {
                let bucket = if reason.contains("position cap") {
                    "limit.positionCap"
                } else if reason.contains("notional cap") {
                    "limit.notionalCap"
                } else {
                    "limit.other"
                };
                self.stats.strategy_limit_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes.entry(bucket.into()).or_default() += 1;
                continue;
            }
            // E-VA: the batch pre-check passed — HOLD this leg's virtual
            // quota before it spends anything real. A later per-order gate
            // may still refuse the leg: the Err arm below releases its keyed
            // reservation, and a placed leg keeps its hold until the order
            // state machine's terminal transition releases it.
            {
                let account_id = req.account_id.to_string();
                let strategy = req.strategy.clone();
                self.virtual_batch_commit(
                    &account_id,
                    &strategy,
                    &[(req.internal_key.clone(), req.price * req.size)],
                );
            }
            let placed_account = req.account_id.clone();
            let virtual_leg = if self
                .virtual_accounts
                .governed(placed_account.as_str(), &name)
            {
                Some((req.internal_key.clone(), req.price * req.size, req.side))
            } else {
                None
            };
            // RFC 568 阶段一：place 前回填 internal_key —— 声明腿与 OME 订单
            // 的事实钥匙挂钩（成交/死亡事件经它寻址回这条腿）。只对 BUY 入腿
            // 有意义（SELL 是退出，不入配对簿）；未声明的 token 是空操作。
            if req.side == Side::Buy {
                self.pair_bind_key(&name, placed_account.as_str(), &token, &req.internal_key);
            }
            match self.place(req, self.config.entry_maker_timeout_ms, now_ms) {
                Ok((_id, _)) => {
                    self.strategy_accounting
                        .entry(strategy_acc_key(&name, placed_account.as_str()))
                        .or_default()
                        .placed += 1;
                    if let Some(engine) = self.engine.as_mut() {
                        engine.note_scoped_order_placed(placed_account.as_str(), &token);
                    }
                    placed += 1;
                }
                Err(err) => {
                    // E-VA: a refused leg never rests — release the keyed
                    // virtual hold its batch took, so the pot reflects only
                    // commitments that actually exist.
                    if let Some((key, _notional, side)) = &virtual_leg
                        && *side == Side::Buy
                    {
                        self.virtual_accounts.release(
                            placed_account.as_str(),
                            &name,
                            key,
                            *_notional,
                        );
                    }
                    self.stats.place_rejected += 1;
                    let bucket = classify_rejection(&err);
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause={bucket} reason={}",
                        err.message);
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, placed_account.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes.entry(bucket).or_default() += 1;
                }
            }
        }
        Some(placed)
    }

    /// Portfolio-level exposure cap (E16/#98): the sum of open notional across
    /// ALL strategies plus the incoming entry stays under
    /// `risk.max_open_notional_usd` (0 = disabled).
    /// #461 Fix 3: the systemic aggregates the nine limits judge, with
    /// RESTING exposure counted. Positions carry their cost basis (cash
    /// committed); live BUY orders carry their remaining notional (cash
    /// PROMISED — a resting bid fills at any moment and the exposure cap
    /// must not discover the commitment only after it became a position).
    /// SELL orders commit nothing new (they release inventory), so only the
    /// buy side counts.
    fn systemic_facts(&self, asset: &str, now_ms: i64) -> crate::risk::limits::SystemicFacts {
        // The systemic COUNT judges live positions only (expired rounds are
        // settlement-dead — same rule as PositionManager::can_open); the
        // exposure sums keep every position: their cost basis is cash
        // actually committed until the venue settles.
        crate::risk::limits::SystemicFacts {
            open_positions: self
                .positions
                .open_positions()
                .iter()
                .filter(|p| p.expires_at_ms > now_ms)
                .count(),
            open_exposure_usd: self
                .positions
                .open_positions()
                .iter()
                .map(|p| p.cost_usd)
                .chain(
                    self.ome
                        .live_orders()
                        .iter()
                        .filter(|o| o.side == Side::Buy)
                        .map(|o| (o.price * (o.size - o.filled_size)).max(Decimal::ZERO)),
                )
                .sum(),
            same_asset_exposure_usd: self
                .positions
                .open_positions()
                .iter()
                .filter(|p| p.asset == asset)
                .map(|p| p.cost_usd)
                .chain(
                    self.ome
                        .live_orders()
                        .iter()
                        .filter(|o| o.side == Side::Buy && o.asset == asset)
                        .map(|o| (o.price * (o.size - o.filled_size)).max(Decimal::ZERO)),
                )
                .sum(),
            day_realized_usd: self.positions.daily_pnl_total(),
        }
    }

    /// #461 Fix 2: the worst per-share basis ALREADY PAID on the counterpart
    /// leg of this condition (None when no counterpart is held). One UP share
    /// plus one DOWN share redeems exactly $1.00, so the pair economics are
    /// per-share: `counterpart_basis/share + this leg's price ≤ 1.00`. The
    /// WORST (highest-basis) leg prices the cap — a blend would let one
    /// expensive leg hide behind a cheap one.
    ///
    /// 跨账户双腿裁决（多账户解耦收口，docs/acceptance-account-decouple.md
    /// 披露 1）：pair 的经济单位 = (账户, 条件, 资产) —— 一个 UP share +
    /// 一个 DOWN share 在**同一账户的账本里**赎回 $1；每账户独立账本独立
    /// 赎回，跨账户的对手腿在结算时永不合并。所以分母只看本账户的对手腿：
    /// A 的 0.90 持仓不得把 B 的 0.05 新腿顶过 $1。
    fn counterpart_pair_basis(
        &self,
        condition_id: &str,
        asset: &str,
        direction: &SignalDirection,
        account_id: &AccountId,
    ) -> Option<Decimal> {
        self.positions
            .open_positions()
            .iter()
            .filter(|p| {
                p.condition_id == condition_id
                    && p.asset == asset
                    && p.direction != *direction
                    && p.shares > Decimal::ZERO
                    && &p.account_id == account_id
            })
            .map(|p| p.cost_usd / p.shares)
            .max()
    }

    fn portfolio_limit_ok(&self, req: &crate::model::OrderRequest) -> Result<(), String> {
        // Read the LIVE risk config: a runtime update (risk_config_mut /
        // IPC) lands in the gate, not in the static CoreConfig.
        let cap = self.risk.config().max_open_notional_usd;
        if cap <= Decimal::ZERO {
            return Ok(());
        }
        // The cap is per-ACCOUNT: each account's notional pot is its own
        // (positions + resting buys of THAT account only) — A's full book
        // must not freeze B's fresh entry. The default scope's filter is a
        // no-op on a single-book deployment (every position already carries
        // `default`), so the historical numbers are unchanged.
        let used: Decimal = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == req.account_id)
            .map(|p| p.cost_usd)
            // #461: resting BUY orders are committed cash — they count
            // against the portfolio cap the moment they rest, not only
            // when they fill.
            .chain(
                self.ome
                    .live_orders()
                    .iter()
                    .filter(|o| o.side == Side::Buy && o.account_id == req.account_id)
                    .map(|o| (o.price * (o.size - o.filled_size)).max(Decimal::ZERO)),
            )
            .sum();
        let incoming = req.price * req.size;
        if used + incoming > cap {
            return Err(format!(
                "portfolio notional cap ({used}+{incoming} > {cap})"
            ));
        }
        Ok(())
    }

    /// Check a configured per-strategy entry cap against the strategy's current
    /// open exposure. Returns the rejection reason when a cap would be exceeded.
    fn strategy_limit_ok(
        &self,
        req: &crate::model::OrderRequest,
        limit: &StrategyLimit,
        now_ms: i64,
    ) -> Result<(), String> {
        // The position-cap count judges LIVE positions only — an expired
        // round's stuck position is settlement-dead and must not consume
        // quota (the same rule PositionManager::can_open applies at the
        // global cap). The notional sum below keeps ALL positions: their
        // cost basis is cash still actually committed.
        let open: Vec<&crate::position::OpenPosition> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.strategy == req.strategy && p.account_id == req.account_id)
            .collect();
        if let Some(max_pos) = limit.max_open_positions {
            let live = open.iter().filter(|p| p.expires_at_ms > now_ms).count();
            if live >= max_pos {
                return Err(format!(
                    "strategy {} at position cap ({live}/{max_pos})",
                    req.strategy
                ));
            }
        }
        if let Some(max_notional) = limit.max_open_notional_usd {
            let used: Decimal = open.iter().map(|p| p.cost_usd).sum::<Decimal>()
                // #461: the strategy's own resting BUY orders are committed
                // cash too — same rule as the portfolio cap. Both pots read
                // THIS account's slice (a per-strategy cap is per-scope).
                + self
                    .ome
                    .live_orders()
                    .iter()
                    .filter(|o| {
                        o.side == Side::Buy
                            && o.strategy == req.strategy
                            && o.account_id == req.account_id
                    })
                    .map(|o| (o.price * (o.size - o.filled_size)).max(Decimal::ZERO))
                    .sum::<Decimal>();
            let incoming = req.price * req.size;
            if used + incoming > max_notional {
                return Err(format!(
                    "strategy {} notional cap ({used}+{incoming} > {max_notional})",
                    req.strategy
                ));
            }
        }
        Ok(())
    }

    // ── E-VA: strategy-level virtual accounts ───────────────────────────────
    //
    // The virtual quota book itself never appears in the placement pipeline —
    // these helpers are the ONLY seams. Each one short-circuits on
    // `governed`, which is false for every scope `config.virtual_limits` does
    // not name, so an unconfigured kernel takes the exact historical path.

    /// The strategy's virtual reservation outstanding on one account, for the
    /// readout / tests. Zero when the scope is not governed.
    pub fn virtual_used(&self, account_id: &str, strategy: &str) -> Decimal {
        self.virtual_accounts.used(account_id, strategy)
    }

    /// The configured virtual cap, when this scope has one.
    pub fn virtual_cap(&self, account_id: &str, strategy: &str) -> Option<Decimal> {
        self.virtual_accounts.cap(account_id, strategy)
    }

    /// RFC #541 挂钩 B — the WHOLE-CYCLE batch pre-check, the single point
    /// both call sites (the real cycle's submit pipeline and the nofast
    /// twin's `engine_replay_cycle`) share. For every declared-atomic
    /// strategy that emitted BUY legs this cycle, judge the strategy's legs
    /// AS ONE TICKET and remove every leg wholesale on a refusal:
    ///
    /// (a) credentials (Live only — a non-trading mode has no venue egress
    ///     and no credentials story, so replay/backtest corpora are not
    ///     batch-refused over shell env): any leg's venue lacking its
    ///     plugin's required credentials refuses the WHOLE strategy cycle.
    ///     Credentials are VENUE-scoped — judged across the strategy's full
    ///     cycle legs, NOT per `(account, strategy)` batch: the v2 dual-leg
    ///     stamp (entries and exits stamp the token's venue's leg account)
    ///     splits one pair across two accounts, so a per-account grouping
    ///     would judge each sub-batch against only its own legs and let the
    ///     credentialed venue's leg fly naked (RFC failure mode ①).
    /// (b) scope (all modes): any leg's token in conflict with its STAMPED
    ///     account's venue binding refuses the WHOLE strategy cycle — the
    ///     batch loop's last line of defence behind engine hook A (which
    ///     already drops declared-atomic strategies upstream, before any
    ///     OrderRequest exists).
    /// (c) the virtual pot (per governed account): each leg tries its
    ///     notional against ITS OWN (account, strategy) pot. A strategy may
    ///     legitimately span two accounts under the v2 stamp, so the pot is
    ///     the one dimension that stays per-account — an over-cap share
    ///     refuses the whole cycle so a partial pair never egresses.
    ///
    /// Undeclared strategies never enter (the grouping predicate is
    /// `strategy_atomic_batch`); unconfigured pots are transparent
    /// (`governed() == false` ⇒ that account's share always fits). Refusals
    /// bucket per leg under the strategy's (strategy, stamped account)
    /// accounting row with the SAME cause keys the batch loop carried
    /// (`credentials.notReady` / `scope.legRefused` / `limit.virtualCap`),
    /// bump `limit_rejected` per leg, log ONE `batch rejected:` INFO line
    /// naming the missing vars or the binding conflict, and strip every leg
    /// of the strategy from the cycle (0 legs to OME — no naked first leg).
    fn arb_precheck_filter(&mut self, tokens: &mut Vec<(String, crate::model::OrderRequest)>) {
        let declared_atomic = |name: &str| {
            self.engine
                .as_ref()
                .is_some_and(|e| e.strategy_atomic_batch(name))
        };
        // Owned leg facts (the borrow split: the judges below need `&self`
        // while the rejection needs `&mut tokens`, so the cycle's leg list
        // is captured by value). Declared-atomic strategy → the cycle's
        // whole BUY leg list; a BTreeMap keeps multi-strategy traversal
        // deterministic.
        let mut cycles: std::collections::BTreeMap<String, Vec<CycleLeg>> =
            std::collections::BTreeMap::new();
        for (_, req) in tokens.iter() {
            if req.side == Side::Buy && declared_atomic(&req.strategy) {
                cycles
                    .entry(req.strategy.clone())
                    .or_default()
                    .push(CycleLeg {
                        internal_key: req.internal_key.clone(),
                        token: req.token_id.to_string(),
                        account: req.account_id.to_string(),
                        notional: req.price * req.size,
                    });
            }
        }
        for (strategy, legs) in cycles {
            // (a) credentials — venue-scoped, judged across the WHOLE cycle
            // once. The kernel's environment IS the credential truth (the
            // launcher loads `.env` before spawning this process; the old
            // config.json cell is retired), so the predicate reads `std::env`
            // directly — no per-cycle file load.
            if self.config.mode.may_trade() {
                let mut refusal: Option<String> = None;
                for leg in &legs {
                    let venue = self.execution_venue_of(&leg.account, &leg.token);
                    // tag 腿的 venue 插件必需凭证必须全部可解析（进程环境，
                    // 与 market::credentials_status 同一谓词；声明表来自
                    // 插件自声明，boot 聚合进 config）；
                    // untagged 腿不阻断——scope 门对无标签语料同样放行。
                    if !venue.is_empty()
                        && !venue_credentials_ready(&venue, &self.config.credential_vars)
                    {
                        // RFC #541 边界三（拒绝可见性）：the refusal names the
                        // leg, the venue, and the EXACT unresolved vars — the
                        // operator fixes the batch from this line alone.
                        let missing =
                            venue_credentials_missing(&venue, &self.config.credential_vars)
                                .join(",");
                        refusal = Some(format!(
                            "credentials.notReady: leg {} (token {}) venue `{venue}` has \
                             unresolved executor credentials: missing [{missing}]",
                            leg.internal_key, leg.token
                        ));
                        break;
                    }
                }
                if let Some(reason) = refusal {
                    self.reject_whole_cycle(
                        tokens,
                        &strategy,
                        &legs,
                        "credentials.notReady",
                        &reason,
                    );
                    continue;
                }
            }
            // (b) scope — the leg's STAMPED account's binding judges its own
            // leg (the v2 stamp precedes the gates intrinsically: 钉2). The
            // venue read is the leg's EXECUTION venue: on a shared token the
            // leg account's binding names its own (venue, token) row, so the
            // leg is judged against its OWN binding, never a sibling row's.
            let mut refusal: Option<String> = None;
            for leg in &legs {
                let venue = self.execution_venue_of(&leg.account, &leg.token);
                let bound = self
                    .accounts
                    .account(&AccountId::from(leg.account.as_str()))
                    .map(|a| a.venue.as_str())
                    .unwrap_or_default();
                if !bound.is_empty() && !venue.is_empty() && venue != bound {
                    refusal = Some(format!(
                        "scope.legRefused: leg {} (token {}) belongs to venue `{venue}` \
                         but account `{}` is bound to venue `{bound}`",
                        leg.internal_key, leg.token, leg.account
                    ));
                    break;
                }
            }
            if let Some(reason) = refusal {
                self.reject_whole_cycle(tokens, &strategy, &legs, "scope.legRefused", &reason);
                continue;
            }
            // (c) the virtual pot — per governed (account, strategy) scope:
            // the ONE dimension that stays per-account under the v2 stamp
            // (each leg's notional hits the pot of the account it spends).
            // An over-cap share refuses the whole cycle: a partial pair
            // never egresses, and the pot keeps its check-then-hold split
            // (the per-order `virtual_batch_commit` is still the ONLY
            // reservation point — DRY-RUN pre-checks hold nothing here).
            let mut over: Option<String> = None;
            for (account_id, share) in per_account_shares(&legs) {
                // DRY-RUN semantics (the pre-check must not hold anything —
                // the per-order loop's `virtual_batch_commit` is the ONLY
                // reservation point; reserving here too double-charged every
                // leg, the 4 red tests' ×2 signature): the occupied quota
                // OUTSIDE the pot is the strategy's open-position cost basis
                // on THIS account; resting BUY orders are NOT added (their
                // commitment IS their reservation — counting both would
                // double-charge the pot).
                let committed: Decimal = self
                    .positions
                    .open_positions()
                    .iter()
                    .filter(|p| p.account_id.as_str() == account_id && p.strategy == strategy)
                    .map(|p| p.cost_usd)
                    .sum();
                if let Err(reason) = self
                    .virtual_accounts
                    .batch_fits(account_id, &strategy, &share, committed)
                {
                    over = Some(reason);
                    break;
                }
            }
            if let Some(reason) = over {
                self.reject_whole_cycle(tokens, &strategy, &legs, "limit.virtualCap", &reason);
                continue;
            }
        }
    }

    /// The wholesale side effect shared by all three dimensions: bucket every
    /// leg under its (strategy, stamped account) row, strip the strategy's
    /// legs from the cycle, log ONE auditable INFO line. `legs` is the
    /// strategy's full cycle leg list as captured BEFORE any removal.
    fn reject_whole_cycle(
        &mut self,
        tokens: &mut Vec<(String, crate::model::OrderRequest)>,
        strategy: &str,
        legs: &[CycleLeg],
        cause: &str,
        reason: &str,
    ) {
        self.stats.strategy_limit_rejected += 1;
        for leg in legs {
            let acc = self
                .strategy_accounting
                .entry(strategy_acc_key(strategy, leg.account.as_str()))
                .or_default();
            acc.limit_rejected += 1;
            *acc.rejection_causes.entry(cause.to_string()).or_default() += 1;
        }
        tracing::info!(target: "strategy",
            "batch rejected: strategy={strategy} cause={cause} legs={} reason={reason}",
            legs.len());
        let refused: HashSet<&str> = legs.iter().map(|leg| leg.internal_key.as_str()).collect();
        tokens.retain(|(_, req)| !refused.contains(req.internal_key.as_str()));
    }

    /// The commit twin of the whole-cycle pre-check: it RUNS at the head of
    /// the per-order loop and holds the leg's quota until the state machine
    /// releases it. Called only when the pre-check passed, so the reserve
    /// cannot fail here — an unexpected refusal (a concurrent mutation
    /// cannot happen on `&mut self`) is logged loudly and degrades to the
    /// per-order path rather than wedging the cycle.
    fn virtual_batch_commit(
        &mut self,
        account_id: &str,
        strategy: &str,
        batch: &[(String, Decimal)],
    ) {
        let _ = virtual_batch_commit_impl(self, account_id, strategy, batch);
    }

    /// E-VA full release of an order's remaining virtual reservation — the
    /// terminal-transition hook (cancelled / rejected / expired /
    /// force-closed). A no-op for SELL legs and ungoverned scopes. The
    /// fill-proportional conversion hook lives in `apply_delta_effects` (the
    /// one place a fill's signed delta and its LIMIT price are both at hand).
    fn virtual_release_order(&mut self, order: &crate::model::TrackedOrder) {
        if order.side != Side::Buy {
            return;
        }
        self.virtual_accounts.release_all(
            order.account_id.as_str(),
            &order.strategy,
            &order.internal_key,
        );
    }

    /// Per-strategy accounting (P-1.1): open exposure (live) plus session order
    /// counters and realized PnL per registered strategy.
    ///
    /// PR H: one row per strategy NAME (a strategy assigned to two accounts is
    /// one strategy — the old per-scope fan-out produced duplicate rows the
    /// panel read as separate strategies). The row aggregates across scopes and
    /// carries `accounts`: the account ids that hold a live scope of this
    /// strategy, so the panel can slice to the active account. The
    /// `enabled` flag reads "any scope enabled" — a name is either globally
    /// togglable or per-scope from the assignment write, and both arrive here.
    ///
    /// 口径（issue 552 审计）：`enabled` 是 master-power 位（默认 scope 的
    /// 全局开关），**不是** (账户,策略) 复合键的真值 —— 复合真值在
    /// `accounts`（该策略有 live scope 的账户 = 启用集点名了它）。面板已
    /// 不直接消费该位（策略页按钮按账户 feature-detect）；Overview 的
    /// 「尚无启用策略」提示走同一复合口径。
    pub fn strategy_stats(&self) -> Vec<serde_json::Value> {
        let enabled = self.enabled_strategy_names();
        // The row set is the engine's registered names UNION every name that
        // already has accounting slices. A strategy whose scope was torn down
        // (or a test core without an engine) still owns its closed-trade
        // history — dropping the row would make realized PnL vanish from the
        // page mid-session.
        let mut names = self.strategy_names();
        for k in self.strategy_accounting.keys() {
            if let Some(name) = k.split('\u{1}').next()
                && !names.iter().any(|n| n == name)
            {
                names.push(name.to_string());
            }
        }
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|name| {
                // Fold the (strategy, account) map across every scope of this
                // strategy: the row's counters are the strategy's TOTALS, and
                // `accounts` names the slices a per-account view can pick.
                // The set is the LIVE SCOPES (engine registry, the same
                // read `strategy.list` rows carry) UNION the accounting keys
                // (a torn-down scope's closed-trade history must keep its
                // attribution even after its scope is gone).
                let mut acc = StrategyAccounting::default();
                let prefix = format!("{name}\u{1}");
                let mut accounts: Vec<String> = self
                    .engine
                    .as_ref()
                    .map(|e| e.strategy_scopes(&name))
                    .unwrap_or_default();
                for k in self.strategy_accounting.keys() {
                    if let Some(a) = k.strip_prefix(&prefix)
                        && !accounts.iter().any(|v| v == a)
                    {
                        accounts.push(a.to_string());
                    }
                }
                accounts.sort();
                accounts.dedup();
                for a in &accounts {
                    if let Some(one) = self.strategy_accounting.get(&strategy_acc_key(&name, a)) {
                        acc.placed += one.placed;
                        acc.rejected += one.rejected;
                        acc.limit_rejected += one.limit_rejected;
                        acc.closed_trades += one.closed_trades;
                        acc.wins += one.wins;
                        acc.losses += one.losses;
                        acc.fees_usd += one.fees_usd;
                        acc.net_pnl_usd += one.net_pnl_usd;
                        for (cause, n) in &one.rejection_causes {
                            *acc.rejection_causes.entry(cause.clone()).or_default() += n;
                        }
                    }
                }
                let opens: Vec<&crate::position::OpenPosition> = self
                    .positions
                    .open_positions()
                    .iter()
                    .filter(|p| p.strategy == name)
                    .collect();
                let open_notional: Decimal = opens.iter().map(|p| p.cost_usd).sum();
                // Source: the live scope's provenance when one exists, else the
                // sticky package_sources entry (a torn-down scope's row keeps
                // naming where the package lives — the strategies page shows
                // every INSTALLED strategy, enabled or not).
                let source = self
                    .engine
                    .as_ref()
                    .map(|e| {
                        e.strategy_source(&name).map(|s| s.to_string()).or_else(|| {
                            e.scope_sources()
                                .into_iter()
                                .find(|(n, _)| n == &name)
                                .map(|(_, s)| s)
                        })
                    })
                    .unwrap_or_default()
                    .unwrap_or_default();
                // E2-a: the sizing actually in force for this strategy (its own
                // override clamped by the globals, else the globals) plus the
                // configured caps, so the report shows both quota and occupancy.
                let effective = self
                    .engine
                    .as_ref()
                    .map(|e| e.effective_sizing(&name))
                    .unwrap_or(crate::engine::EffectiveSizing {
                        size_usd: self.config.size_usd,
                        min_shares: self.config.min_shares,
                        max_shares: self.config.max_shares,
                        size_pct: self.config.size_pct,
                        strategy_scoped: false,
                        size_weight: None,
                    });
                let limit = self.config.strategy_limits.get(&name);
                // E2-b: what this strategy DECLARED it does not need, next to how
                // often a gate actually stopped it and how often an exemption let
                // a candidate through — so an opt-out is never invisible beside an
                // un-exempted strategy's rejections.
                let declared = self
                    .engine
                    .as_ref()
                    .and_then(|e| e.strategy_gate_exemptions(&name))
                    .unwrap_or_default();
                let tally = self
                    .engine
                    .as_ref()
                    .map(|e| e.gate_tally(&name))
                    .unwrap_or_default();
                let mut row = serde_json::json!({
                    "name": name,
                    "enabled": enabled.contains(&name),
                    "source": source,
                    // PR H: the account scopes holding this strategy (sorted;
                    // empty = registered but assigned to no account).
                    "accounts": accounts,
                    "openPositions": opens.len(),
                    "openNotionalUsd": dec_json(open_notional),
                    // ── E2-a quota + effective sizing ──
                    "maxOpenPositions": limit.and_then(|l| l.max_open_positions),
                    "maxOpenNotionalUsd": limit
                        .and_then(|l| l.max_open_notional_usd)
                        .map(dec_json),
                    "sizingSource": if effective.strategy_scoped { "strategy" } else { "global" },
                    "effectiveSizeUsd": dec_json(effective.size_usd),
                    "effectiveMinShares": dec_json(effective.min_shares),
                    "effectiveMaxShares": dec_json(effective.max_shares),
                    "sizeWeight": effective.size_weight.map(dec_json),
                    // #202: the equity-relative budget in force for this leg and
                    // what it is worth on the CURRENT balance, so "how much will
                    // this account commit per entry?" is a number on the panel
                    // rather than a reading of the flags.
                    "effectiveSizePct": dec_json(effective.size_pct),
                    "entryBudgetUsd": dec_json(
                        self.engine
                            .as_ref()
                            .map(|e| e.entry_budget_usd(&name))
                            .unwrap_or(effective.size_usd),
                    ),
                    // ── E2-b declared gate exemptions + per-strategy gate counts ──
                    "gateExemptions": declared.gates(),
                    // D-31: the declared `time_left_sec` floor, or null when the
                    // strategy left it to the kernel. Null is meaningful here —
                    // it says "this opt-out still stops at the global window".
                    "gateExemptionTimingFloorSec": declared.timing_min_time_left_sec,
                    "blockedTiming": tally.blocked_timing,
                    "blockedMomentum": tally.blocked_momentum,
                    "gateExemptedTiming": tally.exempted_timing,
                    "gateExemptedMomentum": tally.exempted_momentum,
                    "ordersPlaced": acc.placed,
                    "ordersRejected": acc.rejected,
                    "limitRejected": acc.limit_rejected,
                    // E9-c: cause → count; only counted buckets appear.
                    "rejectionCauses": if acc.rejection_causes.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!(acc.rejection_causes)
                    },
                    "closedTrades": acc.closed_trades,
                    "wins": acc.wins,
                    "losses": acc.losses,
                    "feesUsd": dec_json(acc.fees_usd),
                    "netPnlUsd": dec_json(acc.net_pnl_usd),
                });
                // 套利合计字段（契约）：仅「声明被受理」的行携带 —— RFC-645
                // A-1（决议②③）：套利面的裁判从名字比较改为声明驱动。受理
                // 谓词 = 该策略声明 consumes_pair_book_stats() 且声明
                // atomic_batch() 且配对簿里有它 owner_strategy 名下至少一条
                // 在册（非 Closed）条目，三者缺一 = 视为未声明（不附加
                // arb* 字段），并在声明在场却未被受理时 warn 一条（说明缺
                // 哪条腿）。M2 过渡期：名字恰好等于 ARB_STRATEGY 的行仍按
                // 旧名字分支受理（保留一个发布周期），命中时打一次性的
                // deprecation 告警；M3 删名字分支。双腿平仓各记在该策略的
                // (strategy, 各腿账户) 键下（`on_position_closed`），上面的
                // fold 已经是该策略的全口径合计，原样再报一遍专名字段，前端
                // 不必知道键形状；`arbActivePairs` 读配对簿的在册（非
                // Closed）配对数。未受理行不带这些键。
                let arb_accepted = self
                    .engine
                    .as_ref()
                    .map(|e| e.pair_book_stats_accepted(&name, &self.pair_book))
                    .unwrap_or(false);
                // M2 deprecation bridge: the NAME branch (an exact
                // ARB_STRATEGY match) stays authoritative for one release
                // cycle. A row that rides the name instead of an accepted
                // declaration gets a one-shot (per process) deprecation warn;
                // a row whose declaration IS accepted rides the new path
                // silently.
                let name_bridge = name == ARB_STRATEGY && !arb_accepted;
                if name_bridge {
                    arb_name_branch_warn_once(&name);
                }
                if arb_accepted || name_bridge {
                    if let Some(reason) = self
                        .engine
                        .as_ref()
                        .and_then(|e| e.pair_book_stats_rejection_reason(&name, &self.pair_book))
                    {
                        tracing::warn!(
                            strategy = name.as_str(),
                            reason,
                            "consumes_pair_book_stats declared but NOT accepted — the arb* face \
                             rides the deprecated name branch (ARB_STRATEGY match) or is omitted \
                             entirely; a pair consumer must also declare atomic_batch and hold at \
                             least one live PairBook entry"
                        );
                    }
                    row["arbNetPnlUsd"] = dec_json(acc.net_pnl_usd);
                    row["arbWins"] = serde_json::json!(acc.wins);
                    row["arbLosses"] = serde_json::json!(acc.losses);
                    row["arbClosedTrades"] = serde_json::json!(acc.closed_trades);
                    row["arbActivePairs"] =
                        serde_json::json!(self.pair_book.active_pairs_count(&name));
                }
                row
            })
            .collect()
    }

    /// PR H: per-STRATEGY accounting for ONE account — the same counters as
    /// `strategy_stats`, folded over a single account's scopes only. The
    /// strategy page slices with this so a strategy that trades on two
    /// accounts does not print account B's PnL on account A's page.
    pub fn strategy_stats_for_account(&self, account: &str) -> Vec<serde_json::Value> {
        let mut rows = self.strategy_stats();
        // Per-scope counters of just this account, folded back over the row's
        // name — the shape stays identical to `strategy_stats` (the panel's
        // table code renders one shape), only the counter values differ.
        for row in &mut rows {
            let Some(name) = row.get("name").and_then(|v| v.as_str()).map(String::from) else {
                continue;
            };
            // The arb totals the GLOBAL row just carried (the arb unit's whole
            // fold across its leg accounts); read before any slice overwrites
            // the row, and re-attached below so every account's page shows the
            // arb unit's combined take — the legs' own slices stay per-account
            // in the plain counters. RFC-645 A-1: which rows carry the arb*
            // keys was decided by the acceptance predicate in `strategy_stats`
            // (declaration accepted, or the M2 name bridge) — the global row's
            // `arbNetPnlUsd` presence IS the verdict, so this read follows it
            // verbatim and the slice re-attachment cannot drift from the row
            // that produced it.
            let arb_totals = if row.get("arbNetPnlUsd").is_some() {
                Some((
                    row.get("arbNetPnlUsd").cloned().unwrap_or_default(),
                    row.get("arbWins").cloned().unwrap_or_default(),
                    row.get("arbLosses").cloned().unwrap_or_default(),
                    row.get("arbClosedTrades").cloned().unwrap_or_default(),
                    row.get("arbActivePairs").cloned().unwrap_or_default(),
                ))
            } else {
                None
            };
            let one = self
                .strategy_accounting
                .get(&strategy_acc_key(&name, account))
                .cloned()
                .unwrap_or_default();
            row["ordersPlaced"] = serde_json::json!(one.placed);
            row["ordersRejected"] = serde_json::json!(one.rejected);
            row["limitRejected"] = serde_json::json!(one.limit_rejected);
            row["rejectionCauses"] = if one.rejection_causes.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::json!(one.rejection_causes)
            };
            row["closedTrades"] = serde_json::json!(one.closed_trades);
            row["wins"] = serde_json::json!(one.wins);
            row["losses"] = serde_json::json!(one.losses);
            row["feesUsd"] = dec_json(one.fees_usd);
            row["netPnlUsd"] = dec_json(one.net_pnl_usd);
            // Gate counts follow the account too: the engine folds them per
            // (strategy, account), so this account's page never prints a
            // sibling account's blocked/exempted numbers.
            let tally = self
                .engine
                .as_ref()
                .map(|e| e.gate_tally_for_account(&name, account))
                .unwrap_or_default();
            row["blockedTiming"] = serde_json::json!(tally.blocked_timing);
            row["blockedMomentum"] = serde_json::json!(tally.blocked_momentum);
            row["gateExemptedTiming"] = serde_json::json!(tally.exempted_timing);
            row["gateExemptedMomentum"] = serde_json::json!(tally.exempted_momentum);
            // Open exposure: THIS account's slice of the strategy.
            let opens: Vec<&crate::position::OpenPosition> = self
                .positions
                .open_positions()
                .iter()
                .filter(|p| p.strategy == name && p.account_id.as_str() == account)
                .collect();
            row["openPositions"] = serde_json::json!(opens.len());
            row["openNotionalUsd"] = dec_json(opens.iter().map(|p| p.cost_usd).sum());
            // The arb unit's combined take rides EVERY account's page
            // (契约：无论面板选哪个账户，套利行显示套利单元合计收益).
            if let Some((net, wins, losses, closed, pairs)) = arb_totals {
                row["arbNetPnlUsd"] = net;
                row["arbWins"] = wins;
                row["arbLosses"] = losses;
                row["arbClosedTrades"] = closed;
                row["arbActivePairs"] = pairs;
            }
        }
        rows
    }

    /// Diagnostic snapshot: feed counters + engine trend/confirmed state.
    pub fn engine_stats(&self) -> serde_json::Value {
        self.engine_stats_at(now_ms())
    }

    /// E29 (§12.2): `kline.history` body — the aggregator's closed tail plus
    /// the still-growing bar as the last element. A core without an engine
    /// answers the empty list: the documented envelope for "no bars exist",
    /// exactly what the Wave-0 placeholder arm used to answer.
    pub fn kline_history(
        &self,
        symbol: &str,
        interval: crate::kline::KlineInterval,
        limit: usize,
    ) -> Vec<crate::kline::Kline> {
        self.engine
            .as_ref()
            .map(|e| e.kline_history(symbol, interval, limit))
            .unwrap_or_default()
    }

    /// E29: aggregation counters for the stats surface (§10.3: drops must be
    /// visible). Zeros when no engine is attached.
    pub fn kline_stats(&self) -> Option<crate::kline::AggregatorStats> {
        self.engine.as_ref().map(|e| e.kline_stats())
    }

    /// The same snapshot as of an explicit instant. The token lists are sorted:
    /// their source is a `HashSet`, whose iteration order is randomised per
    /// process, and a report that shuffles between runs cannot be diffed.
    pub fn engine_stats_at(&self, as_of_ms: i64) -> serde_json::Value {
        // P0 #670: fd 配额使用率——EMFILE 自旋（accept 刷日志写满磁盘）的
        // 前夜观测。只在本次 stats 求值时算一次（面板轮询频率），不在数据/
        // 订单路径上。open/usagePct 在 macOS 上为 null（无 procfs），
        // critical 随之恒 false。
        let probe_fd = crate::probe::fd_usage();
        let probe_fd_json = serde_json::json!({
            "soft": probe_fd.as_ref().map(|u| u.soft),
            "hard": probe_fd.as_ref().map(|u| u.hard),
            "open": probe_fd.as_ref().and_then(|u| u.open),
            "usagePct": probe_fd.as_ref().and_then(|u| u.usage_pct()),
            "critical": probe_fd.as_ref().is_some_and(|u| u.critical()),
        });
        let mut confirmed = self
            .engine
            .as_ref()
            .map(|e| e.confirmed_tokens().into_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        confirmed.sort();
        let mut confirmed_detail = self
            .engine
            .as_ref()
            .map(|e| e.confirmed_diagnostics(as_of_ms))
            .unwrap_or_default();
        confirmed_detail.sort_by(|a, b| a["token"].as_str().cmp(&b["token"].as_str()));
        let blocked = self
            .engine
            .as_ref()
            .map(|e| {
                // Per-strategy attribution (E2-b / #27): the totals stay global,
                // and `byStrategy` names whose candidates each block belonged to.
                let mut by_strategy = serde_json::Map::new();
                for name in self.strategy_names() {
                    let t = e.gate_tally(&name);
                    if t.blocked_timing == 0 && t.blocked_momentum == 0 {
                        continue;
                    }
                    by_strategy.insert(
                        name,
                        serde_json::json!({
                            "timing": t.blocked_timing,
                            "momentum": t.blocked_momentum,
                        }),
                    );
                }
                let declared: Vec<serde_json::Value> = e
                    .declared_gate_exemptions()
                    .into_iter()
                    .map(|(name, x)| {
                        // D-31: name the remaining-time floor beside the gates, so
                        // "which opt-outs reach into the closing window" is
                        // answerable from the stats snapshot alone.
                        let mut v = serde_json::json!({ "strategy": name, "gates": x.gates() });
                        if let Some(n) = x.timing_min_time_left_sec {
                            v["timingFloorSec"] = serde_json::json!(n);
                        }
                        v
                    })
                    .collect();
                serde_json::json!({
                    "timing": e.blocked_timing_count(),
                    "momentum": e.blocked_momentum_count(),
                    "byStrategy": by_strategy,
                    "declaredExemptions": declared,
                })
            })
            .unwrap_or(serde_json::Value::Null);
        serde_json::json!({
            "books": self.stats.books,
            "tops": self.stats.tops,
            "spots": self.stats.spots,
            "rounds": self.stats.rounds,
            // #377 (治 #2): archive verdict events replayed into the core —
            // settlements that came from the stream instead of the dry ladder.
            "resolutions": self.stats.resolutions,
            // #388: wallet-level cash rows credited from the stream — the
            // count and exact USD total per kind, so a report reconciles the
            // credit against the ledger it built.
            "cashflowRebates": self.stats.cashflow_rebates,
            "cashflowRewards": self.stats.cashflow_rewards,
            "cashflowRebatesUsd": dec_json(self.stats.cashflow_rebates_usd),
            "cashflowRewardsUsd": dec_json(self.stats.cashflow_rewards_usd),
            "evaluations": self.stats.evaluations,
            "signals": self.stats.signals,
            "placeRejected": self.stats.place_rejected,
            "strategyLimitRejected": self.stats.strategy_limit_rejected,
            "venueRejected": self.stats.venue_rejected,
            "illegalPriceRejected": self.stats.illegal_price_rejected,
            // #180: the ONE panel-visible error slot — structured (code + when +
            // what), written by every internal refusal path, and read by the
            // panel's error banner. `code` is the same `CoreErrorCode`
            // vocabulary a rejected RPC carries in `data.coreCode`, so a client
            // that knows how to branch on one knows how to branch on the other.
            "lastError": match &self.last_error {
                Some(e) => serde_json::json!({
                    "tsMs": e.ts_ms,
                    "code": e.code,
                    "message": e.message,
                }),
                None => serde_json::Value::Null,
            },
            // Legacy spelling of the same record, kept because the shipped panel
            // (ui/webapp/webui/src/pages/Overview.vue) reads this key for its
            // "last trading error" banner. Same record, one writer, one
            // timestamp — never a second, staler copy. The message is rendered
            // exactly as this key always rendered it (`<code>: <message>`), so a
            // consumer that parsed the old string keeps working; `lastError`
            // above is the structured form to migrate to.
            "lastVenueError": match &self.last_error {
                Some(e) => serde_json::json!({
                    "tsMs": e.ts_ms,
                    "message": e.legacy_message(),
                }),
                None => serde_json::Value::Null,
            },
            // E31-b: how close the reconcile sweep is to freezing trading. The
            // counter is the missing half of the last error — the error says a
            // sweep failed, the counter says how many in a row, and the threshold
            // says when the freeze lands. Read-only: nothing here sets either
            // value, and the sweep's own success still resets the streak
            // (`Core::reconcile`). Exposed because the transition itself is only
            // reachable from a market plugin, so this is the only way an outside
            // observer (panel, gate) can see the kernel's own threshold.
            "reconcile": {
                "consecutiveSweepFailures": self.consecutive_sweep_failures,
                "freezeThreshold": SWEEP_FAILURE_FREEZE,
            },
            "selfCheck": match &self.last_self_check {
                Some(r) => serde_json::json!({
                    "ok": r.ok,
                    "tsMs": r.ts_ms,
                    "items": r.items.iter().map(|i| serde_json::json!({
                        "name": i.name, "ok": i.ok, "detail": i.detail,
                    })).collect::<Vec<_>>(),
                }),
                None => serde_json::Value::Null,
            },
            "tradingFrozen": if self.risk.is_killed() {
                serde_json::json!({
                    "active": true,
                    "reason": self.risk.kill_reason().unwrap_or("kill switch active"),
                })
            } else {
                serde_json::json!({ "active": false })
            },
            // The account-scoped kill book (account.freeze / auto-freeze on
            // the venue-rejection streak): account id → why. Empty object when
            // nothing is frozen — a client distinguishes "global freeze"
            // (tradingFrozen.active) from "these accounts only" without
            // parsing reasons.
            "accountFreezes": self.account_freezes,
            // In-kernel accounting audit (issue #189): the latest verdict, so the
            // panel shows what the kernel itself concluded about the books.
            "accountingAudit": self.accounting_audit_view(),
            // #173: the day's realized-loss budget as the panel reads it —
            // visible even when healthy, so "is the breaker armed, and against
            // what cap?" is answerable without grepping the startup log.
            // The daily budget is PER-ACCOUNT (2026-10-25 verdict): the card
            // mirrors the DISPLAY account's day (the panel follows the account
            // everywhere since #592), and carries every account's day beside it
            // so no book's freeze is invisible.
            "dailyLoss": {
                "account": self.accounts.display_id(),
                "dayIndex": self.positions.daily_state_for(self.accounts.display_id()).day_index,
                "realizedPnlUsd": self
                    .positions
                    .daily_pnl_for(self.accounts.display_id()),
                "limitUsd": self
                    .positions
                    .effective_daily_loss_limit_for_public(self.accounts.display_id()),
                "openingEquityUsd": self
                    .positions
                    .daily_state_for(self.accounts.display_id())
                    .opening_equity_usd,
                // P1 #235: WHICH book that base belongs to ("dry"/"live", null
                // when the day predates the field). The cap is only as
                // meaningful as the account it is a share of, and the panel
                // used to show a self-consistent pair of numbers from two
                // different books.
                "equityBasis": self
                    .positions
                    .daily_state_for(self.accounts.display_id())
                    .equity_basis,
                "tripped": self
                    .positions
                    .daily_loss_tripped_for(self.accounts.display_id()),
                "trippedAtMs": self
                    .positions
                    .daily_state_for(self.accounts.display_id())
                    .tripped_at_ms,
                "anyTripped": self
                    .positions
                    .daily_states()
                    .values()
                    .any(|s| s.tripped),
                "accounts": self
                    .positions
                    .daily_states()
                    .iter()
                    .map(|(id, s)| serde_json::json!({
                        "account": id.as_str(),
                        "realizedPnlUsd": s.realized_pnl_usd,
                        "tripped": s.tripped,
                        "dayIndex": s.day_index,
                    }))
                    .collect::<Vec<_>>(),
                // #177: protective stops the wick guard withheld — "should have
                // triggered" is a number on the panel, not a silent no-op.
                "suppressedStops": self.positions.suppressed_stop_count(),
            },
            // #205: how stale a book may be before the engine stops pricing off
            // it — the value that actually gates entries, read off the engine
            // that applies it (a backtest can configure its own). "How long
            // after the feed goes quiet does the bot stop trading?" is a runtime
            // question and must be answerable from the panel.
            "orderbookFreshness": self.orderbook_freshness_view(),
            // P0 #202: what ONE order can commit on THIS account. The ceiling is
            // read off the kernel's own knobs — no ticket can hold more than
            // `max_shares` (every leg is clamped to it) or pay over `max_price`
            // a share, and when the equity-relative cap is armed the
            // `balance × k` bound applies on top. `scripts/risk-sizing-check.mjs`
            // asserts exactly the last line of this block on a live dry core.
            "sizing": self.sizing_view(),
            // #190: the pending-close exit-reason table — how many reasons are
            // held against the bound, and what the two clean-up paths removed.
            // `orphaned` must read 0: an entry whose position is gone is the
            // ghost reason the table used to accumulate.
            "exitReasons": self.exit_reasons_view(),
            // Settlement & redemption (issue #175): settled-but-unredeemed claims
            // are money the chain still owes, and this is where the panel sees
            // them — the same outlet as everything else, no new event type.
            "settlement": self.settlement_view(as_of_ms),
            "blocked": blocked,
            "confirmed": confirmed,
            "confirmedDetail": confirmed_detail,
            "strategies": self.strategy_stats(),
            "archive": self
                .event_archive
                .as_ref()
                .map(|a| serde_json::to_value(a.status()).unwrap_or(serde_json::Value::Null))
                .unwrap_or(serde_json::Value::Null),
            // E29 (§12.2/§10.3): the K-line aggregator's counters — drops MUST
            // be visible (§10.3), and this is where an outside observer sees
            // them. `droppedOutOfOrder` counts discarded late ticks,
            // `noDataBars` counts silently skipped empty buckets. Null when no
            // engine is attached (same posture as `archive`).
            "kline": self
                .kline_stats()
                .map(|s| serde_json::to_value(s).unwrap_or(serde_json::Value::Null))
                .unwrap_or(serde_json::Value::Null),
            // 环境差异排查探针（任务二 1-3）：时钟偏差 / 数据流断流 / spot
            // 失效的观测面。字段名即契约（tests/env_probes.rs 钉死）：
            // * ntp: 上次 SNTP 测量（offsetMs/skewRed/失败计数）
            // * tickHealth: 最近 10s 汇总（窗口 tick 数/零 tick token/最长
            //   静默/断流 warn 计数）
            // * spot: 停更监控（最近更新时刻/停更时长/warn 计数）
            // * fd: P0 #670 的 fd 配额使用率（函数开头算好的 probe_fd_json）。
            "probe": serde_json::json!({
                "ntp": {
                    "synced": self.probe_ntp.last_offset_ms.is_some(),
                    "offsetMs": self.probe_ntp.last_offset_ms,
                    "skewRed": self.probe_ntp.skew_red(),
                    "failures": self.probe_ntp.failures,
                    "lastCheckMs": self.probe_ntp.last_check_ms,
                },
                "tickHealth": {
                    // 进行中的窗口计数（实时）。汇总不立刻清零：值被
                    // capture 留档，下一个非汇总 tick 才重新计数——
                    // 汇总瞬间读 stats 必须还能看到刚结束的窗口。
                    "windowTicks": self.probe_ticks.window_ticks,
                    "zeroTickTokens": self.probe_last_zero,
                    "maxSilenceMs": self.probe_last_max_silence_ms,
                    "streamBreakWarnings": self.probe_ticks.stream_break_warnings,
                    "lastSummaryMs": self.probe_ticks.last_summary_ms,
                },
                "spot": {
                    "lastSeenMs": self.probe_spot.last_seen_ms,
                    "staleMs": self.probe_last_spot_stale_ms,
                    "staleWarnings": self.probe_spot.stale_warnings,
                },
                "fd": probe_fd_json,
            }),
        })
    }

    /// P0 #202 — "how much can any ONE order commit on this account, right
    /// now?", answered from the kernel's own knobs so the panel (and the sizing
    /// gate) never re-derive it from the source:
    ///
    /// * `shareBandUsd` = `max_shares × max_price`: the widest ticket the
    ///   engine's own clamp allows, valued at the risk gate's own price ceiling.
    ///   This is the bound that existed before #202, and on the live 4.8 USDC
    ///   account it is 10.00 USD — 208% of the book.
    /// * `sizeBudgetUsd` (#202, when `size_pct > 0`) = `equity × pct%`.
    /// * `equityCapUsd` (#202, when the relative cap is armed) = `equity × k%`,
    ///   the bound the risk gate REJECTS over.
    /// * `worstCaseOrderUsd` = the tightest of the three. Closing intents are
    ///   exempt from the equity cap by design (`closes_exposure`), and a close
    ///   is bounded by the position it reduces — never by these knobs — so the
    ///   number describes NEW exposure, which is what "max possible loss on one
    ///   order" means for a binary-market BUY (the whole notional can go to 0).
    fn sizing_view(&self) -> serde_json::Value {
        // E28: the sizing view describes the ACTIVE account's book (the panel
        // is a per-session view of the session's account, §9.5).
        let equity = self.accounts.active_ledger().balance();
        let globals = self.engine.as_ref().map(|e| e.global_sizing()).unwrap_or(
            crate::engine::EffectiveSizing {
                size_usd: self.config.size_usd,
                min_shares: self.config.min_shares,
                max_shares: self.config.max_shares,
                size_pct: self.config.size_pct,
                strategy_scoped: false,
                size_weight: None,
            },
        );
        let risk = self.risk.config();
        let share_band = globals.max_shares.max(Decimal::ZERO) * risk.max_price.max(Decimal::ZERO);
        let able = equity > Decimal::ZERO;
        let size_budget = (globals.size_pct > Decimal::ZERO && able)
            .then(|| equity * globals.size_pct / Decimal::ONE_HUNDRED);
        let equity_cap = (risk.max_order_notional_pct > Decimal::ZERO && able)
            .then(|| equity * risk.max_order_notional_pct / Decimal::ONE_HUNDRED);
        let mut worst = share_band;
        if let Some(b) = size_budget {
            worst = worst.min(b);
        }
        if let Some(c) = equity_cap {
            worst = worst.min(c);
        }
        serde_json::json!({
            "equityUsd": dec_json(equity),
            "minShares": dec_json(globals.min_shares),
            "maxShares": dec_json(globals.max_shares),
            "maxPrice": dec_json(risk.max_price),
            "shareBandUsd": dec_json(share_band),
            "sizePct": dec_json(globals.size_pct),
            "sizeBudgetUsd": size_budget.map(dec_json),
            "maxOrderNotionalUsd": dec_json(risk.max_order_notional),
            "maxOrderNotionalPct": dec_json(risk.max_order_notional_pct),
            "equityCapUsd": equity_cap.map(dec_json),
            "worstCaseOrderUsd": dec_json(worst),
            "worstCasePctOfEquity": if able {
                dec_json(worst / equity * Decimal::ONE_HUNDRED)
            } else {
                serde_json::Value::Null
            },
            // The escape hatch, stated where the bounds are: a close/reduce is
            // never judged by the equity cap, so a bound that would trap a
            // position cannot be read off this block either (#174/#202).
            "closesExemptFromEquityCap": true,
            "sizePctSkippedSignals": self
                .engine
                .as_ref()
                .map(|e| e.size_pct_skip_count())
                .unwrap_or(0),
        })
    }

    /// The latest accounting audit as the panel reads it. `null` until the first
    /// audit runs (the audit interval has not elapsed, or it is disabled).
    pub fn accounting_audit_view(&self) -> serde_json::Value {
        let mut v = match self.audit.report.as_ref() {
            Some(r) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
            None => serde_json::json!({ "ok": true, "note": "audit not run yet" }),
        };
        v["halted"] = serde_json::json!(self.audit.halted);
        v["haltReason"] = serde_json::json!(self.audit.halt_reason);
        v["runs"] = serde_json::json!(self.audit.runs);
        v["failures"] = serde_json::json!(self.audit.failures);
        v["anchored"] = serde_json::json!(self.audit.anchor.is_some());
        // Settled-but-unredeemed payout: money owed to us, part of the identity's
        // `total` but not of `balance` until the redeem transaction confirms
        // (issue #175). `mergeReceivableUsd` splits out #461's burned-pair
        // collateral waiting on its merge receipt — both are inside
        // `receivableUsd` (the identity telescopes over all receivables).
        // The view mirrors the AUDITED identity: the ACTIVE account's own
        // receivable only (a sibling account's claim is a different wallet).
        let active_account = self.accounts.active_id().as_str().to_string();
        v["receivableUsd"] = dec_json(self.settlement.receivable_usd_for(&active_account));
        v["mergeReceivableUsd"] =
            dec_json(self.settlement.merge_receivable_usd_for(&active_account));
        v["summary"] = serde_json::json!(
            self.audit
                .report
                .as_ref()
                .map(|r| r.summary())
                .unwrap_or_default()
        );
        v
    }

    /// Snapshot the current round for the UI.
    pub fn round_view(&self) -> Option<RoundView> {
        let engine = self.engine.as_ref()?;
        let now = now_ms();
        let r = engine.scanner().round_state(now);
        let can_trade = engine.scanner().can_trade(now).is_ok();
        // One price card per ASSET: a round may carry several markets for the
        // same asset (strike ladders), and the panel keys the cards by asset —
        // duplicates there are rendered as repeated cards with colliding keys.
        let mut market_prices: Vec<MarketPriceView> = Vec::new();
        for m in &r.markets {
            if market_prices.iter().any(|p| p.asset == m.asset) {
                continue;
            }
            let up = engine
                .book_snapshot(&m.up_token_id)
                .map(|b| b.mid_price)
                .filter(|p| *p > Decimal::ZERO)
                .unwrap_or(m.up_price);
            let down = engine
                .book_snapshot(&m.down_token_id)
                .map(|b| b.mid_price)
                .filter(|p| *p > Decimal::ZERO)
                .unwrap_or(m.down_price);
            market_prices.push(MarketPriceView {
                asset: m.asset.clone(),
                up,
                down,
            });
        }
        Some(RoundView {
            slot: r.slot,
            age_sec: r.age_sec,
            time_left_sec: r.time_left_sec,
            markets: r.markets.len(),
            can_trade,
            market_prices,
        })
    }

    /// Read-only depth view for the UI (`engine.books`, E8-c 盘口深度).
    ///
    /// Reads `Core.books` — the mirror that every ingest path updates (Node
    /// `books.snapshot` bridge and the Rust-native feed alike), and the same
    /// store the exit path values open positions from — so the view can never
    /// contradict what trading actually saw. Levels and metrics come from
    /// `OrderbookSnapshot::from_levels`, the same builder the strategy layer
    /// consumes. Purely observational: no locks beyond the core's own, no side
    /// effects. An absent round or an unknown token simply yields nothing.
    ///
    /// #560 的 slot 过滤 + (venue, asset) 唯一化在这里同样适用：快照装配层
    /// 只修了 `engine.round` 面，`engine.books` 是第二个逐市场平铺的数据源
    /// ——slot 边界附近相邻两轮（当前+1 与当前+2）并存于 scanner，盘口
    /// 深度卡每资产出现两行、选择器按 (venue,asset) 撞重复 key。与
    /// `engine.round` 同判据：到期 slot = 当前 slot + 1（正在交易的一轮），
    /// round_slot == 0（老语料 serde default）照旧放行；再先到先得唯一化。
    /// 判定钟由调用方传入（`round_state` / `current_slot` 同风格）：窗口边
    /// 界附近「喂入轮恰好是视图当下认定的 trading slot」是真实链路里插件
    /// 5s 轮询自带的性质，测试要精确钉住这个关系就不能赌宿主墙钟不翻格。
    ///
    /// E2（多族读面）：过滤谓词是 scanner 的共用纯函数
    /// `is_live_round_row` —— 每行按自己的 (venue, duration) 族钟判定
    /// 「到期 slot = 该族当前 slot + 1」，`engine.round` 行情行（ipc/server）
    /// 与本视图同谓词。老语料（round_slot == 0）无条件放行，单族部署下
    /// 族钟 ≡ 全局网格（偏移 0、行声明 ≡ cfg），判定与升级前的
    /// `current_slot(now) + 1` 逐位一致。
    pub fn books_view(&self, max_levels: usize, now: i64) -> Vec<AssetBooksView> {
        let Some(engine) = self.engine.as_ref() else {
            return Vec::new();
        };
        let scanner = engine.scanner();
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        // RFC-661 M3: natural rows only. The dual-key table holds every
        // venue's OWN (venue, token) rows (M1), so the round-state sweep IS
        // the whole display.
        let rows: Vec<AssetBooksView> = scanner
            .round_state(now)
            .markets
            .iter()
            .filter(|m| scanner.is_live_round_row(m, now))
            .filter(|m| seen.insert((m.venue.clone(), m.asset.clone())))
            .map(|m| AssetBooksView {
                venue: m.venue.clone(),
                asset: m.asset.clone(),
                up_token_id: m.up_token_id.clone(),
                down_token_id: m.down_token_id.clone(),
                up: Self::book_side_view(&self.books, &m.up_token_id, max_levels),
                down: Self::book_side_view(&self.books, &m.down_token_id, max_levels),
            })
            .collect();
        rows
    }

    fn book_side_view(
        books: &HashMap<TokenId, Book>,
        token_id: &str,
        max_levels: usize,
    ) -> BookSideView {
        let Some(b) = books.get(token_id) else {
            return BookSideView::empty();
        };
        if b.bids.is_empty() && b.asks.is_empty() {
            return BookSideView::empty();
        }
        let s = OrderbookSnapshot::from_levels(
            token_id.to_string(),
            b.bids.clone(),
            b.asks.clone(),
            now_ms(),
        );
        let level = |(p, q): &(Decimal, Decimal)| BookLevelView {
            price: *p,
            size: *q,
        };
        BookSideView {
            bids: s.bids.iter().take(max_levels).map(level).collect(),
            asks: s.asks.iter().take(max_levels).map(level).collect(),
            best_bid: Some(s.best_bid).filter(|p| *p > Decimal::ZERO),
            best_ask: Some(s.best_ask).filter(|p| *p < Decimal::ONE),
            mid_price: Some(s.mid_price).filter(|p| *p > Decimal::ZERO),
            obi: Some(s.obi),
            spread: Some(s.spread),
            spread_pct: Some(s.spread_pct),
        }
    }

    /// Open positions enriched for the UI (unrealised PnL uses the current price
    /// we track; the book tick updates it on every check).
    pub fn position_views(&self, now_ms: i64) -> Vec<crate::ipc::schema::PositionView> {
        use crate::exit_policy::pnl_pct;
        self.positions
            .open_positions()
            .iter()
            .map(|p| crate::ipc::schema::PositionView {
                id: p.id.clone(),
                asset: p.asset.clone(),
                direction: p.direction.as_str().to_string(),
                strategy: p.strategy.clone(),
                token_id: p.token_id.clone(),
                // E28 (§9.2): the account the position settles into — a field
                // read, and the row the panel filters per account.
                account_id: p.account_id.as_str().to_string(),
                // #427: the Venue column — a field read off the position.
                venue: p.venue.clone(),
                entry_price: p.entry_price,
                current_price: p.current_price,
                shares: p.shares,
                unrealized_pct: pnl_pct(p.current_price, p.entry_price),
                high_pnl_pct: p.high_pnl_pct(),
                was_maker_entry: p.was_maker_entry,
                entered_at_ms: p.entered_at_ms,
                expires_at_ms: p.expires_at_ms,
                remaining_sec: (p.expires_at_ms - now_ms) / 1000,
                // Cash actually moved on this position, so an external monitor
                // can check the ledger identity without waiting for a flat book.
                // `entry_cost_usd` is the TOTAL entry notional (what left the
                // ledger); `cost_usd` is only the basis still held, so it is the
                // former the identity must sum.
                entry_cost_usd: p.flows.entry_cost_usd,
                cost_usd: p.cost_usd,
                entry_fee_usd: p.flows.entry_fee_usd,
                proceeds_usd: p.flows.proceeds_usd,
                exit_fee_usd: p.flows.exit_fee_usd,
            })
            .collect()
    }
    pub fn list_orders(&self) -> Vec<TrackedOrder> {
        self.ome
            .all()
            .into_iter()
            .map(|o| {
                let mut o = o.clone();
                // The order's venue tag is the venue binding of the account it
                // was placed from — the same fact `account.list` reports (the
                // execution routes by it too). Unknown account / unbound → empty
                // (the field is skipped on the wire, the panel keeps the row).
                o.venue = self
                    .accounts
                    .account(&o.account_id)
                    .map(|a| a.venue.clone())
                    .unwrap_or_default();
                o
            })
            .collect()
    }

    // ── Live venue entry points ─────────────────────────────────────────────
    // In DRY the core synthesises fills itself. In LIVE the CLOB adapter drives
    // these same OME/ledger paths from venue acknowledgements and the user-WS
    // stream, so dry and live share one authoritative state machine.

    /// Reserve/risk-gate/register an order as Pending without simulating a fill.
    /// Used by the live adapter before the async CLOB POST resolves.
    pub fn place_pending(
        &mut self,
        req: OrderRequest,
        now_ms: i64,
    ) -> CoreResult<(OrderId, OrderStatus)> {
        if req.side == Side::Buy {
            // Same entry gate as `place`: a pending opening order is an opening
            // order, however it is submitted.
            self.audit_entry_gate()?;
        }
        // The #202 equity-relative cap is a percentage of the account as it is
        // AT SUBMISSION, so the gate is handed the live balance here — the same
        // number the daily-loss breaker opens its day with. E28: the account
        // is the one the request names, and its posture gates the place too.
        self.accounts
            .permits_order(&req.account_id, is_close_intent(&req.internal_key))?;
        // Account-scoped kill switch, same contract as in `place_gated`:
        // frozen account → new orders refused, closes pass.
        self.account_freeze_permits(&req)?;
        // G5: a close settles into the book that PAID for the shares — a
        // close order naming another account is refused, never re-routed.
        self.refuse_cross_account_close(&req)?;
        // Account↔venue binding (first batch), same rule as `place`: an
        // account pinned to venue V may not trade another venue's tokens.
        self.refuse_cross_venue_order(&req)?;
        // Duration-family admission (B1c), same rule as `place_gated`: the
        // account's (venue, duration) subscription set gates the place; an
        // empty set (today's always-shape) admits everything, closes exempt.
        self.refuse_unsubscribed_duration(&req)?;
        self.risk
            .check_with_equity(&req, self.accounts.ledger_of(&req.account_id)?.balance())?;
        let id = self.new_order_id();
        let buy = req.side == Side::Buy;
        let req_account = req.account_id.clone();
        let req_token = req.token_id.clone();
        if buy {
            self.accounts
                .get_mut(&req.account_id)?
                .reserve(&id, req.price * req.size)?;
        }
        if let Err(e) = self.ome.submit(SubmitParams {
            // #351: the pre-submit path settles locally with zero latency
            // dials — decision, execution and report all land here.
            decision_at_ms: now_ms,
            execute_at_ms: now_ms,
            report_at_ms: now_ms,
            order_id: id.clone(),
            request: req,
            submitted_at_ms: now_ms,
            // The pre-submit path cannot know the entry's escalation window;
            // `confirm_live` falls back to the default clock when it arms.
            maker_timeout_ms: 0,
            // B1b: the order's round-length family, resolved three-state off
            // the token's market and the global grid.
            round_duration_sec: self.round_family_of_token(&req_token),
        }) {
            // #537: a submit refusal happens AFTER the notional reservation —
            // leaving it on the books strands cash on an order that does not
            // exist, and the accounting audit correctly halts entries on the
            // phantom holds. Roll the reservation back with the order.
            if buy && let Some(l) = self.accounts.get_mut(&req_account).ok() {
                l.release(&id);
            }
            return Err(e);
        }
        self.emit_order(&id);
        Ok((id, OrderStatus::Pending))
    }

    /// Venue acknowledged the order (resting live). Optionally map to its venue id.
    pub fn confirm_live(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        if let Some(o) = self.ome.get(id).cloned() {
            self.ome.mark_live(id, now_ms)?;
            // Release reservation only on fill/cancel; keep it while resting.
            self.emit_order(id);
            // The venue just accepted an order from us: capability proven for
            // this instant, so the failure streak and this intent's cooldown
            // both end here — the global readout AND the account's own
            // streak (its venue proved itself; a sibling venue's streak is
            // none of its business).
            self.consecutive_venue_rejects = 0;
            self.consecutive_venue_rejects_by_account
                .remove(o.account_id.as_str());
            // 冻结自愈：the venue just accepted an order from this account —
            // the strongest proof of life there is. An AUTO freeze rides
            // circumstantial evidence (a rejection streak, a failed probe);
            // positive evidence outranks it. Manual freezes never lift here.
            self.auto_lift_if_venue_healthy(o.account_id.as_str(), "venue accepted order", now_ms);
            self.place_cooldowns.remove(&o.internal_key);
            // A `maker_then_taker` order rests as GTC; the escalation clock
            // starts on the venue's acceptance (the submit path cannot arm it
            // in LIVE — the ack arrives asynchronously). Without this the
            // maker leg rests forever and the taker fallback never fires.
            // A negative entry timeout is the operator's "never escalate"
            // switch: no clock is armed, so the leg stays a pure maker.
            if o.mode == FillPolicy::MakerThenTaker
                && o.escalate_at_ms.is_none()
                && let Some(delay) =
                    escalation_delay_ms(o.maker_timeout_ms, self.config.default_maker_timeout_ms)
            {
                self.ome.set_escalation(id, now_ms + delay)?;
            }
        }
        Ok(())
    }

    /// The venue POST timed out — the venue MAY have accepted the order
    /// (#462). The order stays Pending with its reservation held, and the
    /// periodic reconciliation sweep decides: resting at the venue → adopted
    /// as ours (ghost repair / `known_venue_order_ids`), filled → gap-fill
    /// into the OME, absent from the venue → MarkedCancelled. Rejecting it
    /// here (the old behaviour) would release cash the venue may already have
    /// spent and strand a real BUY as an unmanaged position.
    pub fn note_place_timeout(&mut self, id: &str) {
        let Some(o) = self.ome.get(id) else {
            return;
        };
        if !o.status.is_live() {
            return;
        }
        tracing::warn!(
            order = %id,
            side = ?o.side,
            "venue place timed out — order stays pending; reconciliation will decide"
        );
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::Timeout,
            message: format!(
                "place timed out for {id}: kept pending, reconciliation decides its fate"
            ),
        });
    }

    /// Venue rejected/killed an order before acceptance: release any reservation.
    pub fn reject_live(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        self.reject_live_result(id, None, now_ms)
    }

    /// Same as [`Core::reject_live`], carrying the venue's own failure text so
    /// cooldowns, per-strategy accounting and the freeze decision all classify
    /// against the REAL reason instead of a bare "rejected".
    pub fn reject_live_result(
        &mut self,
        id: &str,
        err: Option<crate::model::CoreError>,
        now_ms: i64,
    ) -> CoreResult<()> {
        if let Some(o) = self.ome.get(id).cloned() {
            if o.side == Side::Buy {
                // E28: the reservation is released in the ORDER'S OWN book —
                // a venue reject must never refund another account's cash.
                if let Ok(l) = self.accounts.get_mut(&o.account_id) {
                    l.release(id);
                }
                // E-VA: the virtual hold leaves the pot with the real one.
                self.virtual_release_order(&o);
            }
            self.ome.mark_terminal(id, OrderStatus::Rejected, now_ms)?;
            // RFC 568 阶段一：死亡事实（venue 拒单）喂配对簿。
            self.pair_on_order_event(&o.internal_key, Decimal::ZERO, true);
            // The rejected entry never rested: its token may be re-attempted.
            if let Some(engine) = self.engine.as_mut() {
                engine.release_pending_token_for_strategy(
                    o.account_id.as_str(),
                    &o.strategy,
                    &o.token_id,
                );
            }
            self.emit_order(id);
            let e = err.unwrap_or_else(|| {
                crate::model::CoreError::new(
                    blitzkrieg_market_api::CoreErrorCode::VenueError,
                    "venue rejected order",
                )
            });
            // A `maker_then_taker` entry whose maker leg cannot even REST
            // (post-only would cross the book) never gets a turn to escalate
            // by timer — the venue refused the resting order itself. The
            // mode's contract is to cross instead: place the taker leg now.
            // Other failure classes (balance, auth, venue down) would fail as
            // takers too, so they keep the normal backoff/retry path.
            if o.mode == FillPolicy::MakerThenTaker
                && e.code == blitzkrieg_market_api::CoreErrorCode::WouldCross
            {
                // Same contract as the timer escalation: cross at the price
                // the CURRENT book actually offers, not the rejected passive
                // limit. If no crossing depth exists even at the grid's
                // extreme, there is nothing to escalate to — the intent goes
                // to the normal rejection accounting (backoff/freeze) and a
                // later tick re-enters if the signal persists.
                let cap = if o.side == Side::Buy {
                    Decimal::new(99, 2)
                } else {
                    Decimal::new(1, 2)
                };
                if let Some((_, worst)) =
                    self.marketable_walk(o.side, &o.token_id, o.size, cap, now_ms)
                {
                    // issue 568 阶段一：追单是同一入腿事实链的续段 —— 其
                    // 成交/死亡事实必须寻址回声明腿。原腿的 key 已随死亡
                    // 事实落在簿里；续段 key（{id}:escalated）在此登记。
                    self.pair_book.bind_continuation(
                        &o.strategy,
                        o.account_id.as_str(),
                        &o.token_id,
                        &format!("{id}:escalated"),
                    );
                    let req = OrderRequest {
                        token_id: o.token_id.clone(),
                        condition_id: o.condition_id.clone(),
                        side: o.side,
                        mode: FillPolicy::Taker,
                        price: worst,
                        size: o.size,
                        // Only the engine's ENTRY order is `maker_then_taker`
                        // (exits are Maker or Taker), so this escalated leg is
                        // always new exposure and rightly stays subject to the
                        // kill switch — it is not a close intent (P0 #174).
                        internal_key: format!("{id}:escalated"),
                        strategy: o.strategy.clone(),
                        asset: o.asset.clone(),
                        direction: o.direction.clone(),
                        round_slot: o.round_slot,
                        // E28 (§9.2): the escalated leg spends from the same
                        // account as the maker order it replaces.
                        account_id: o.account_id.clone(),
                    };
                    // The FULL risk verdict gets the first word here too (#461:
                    // real equity + systemic limits + the pair-cost cap, the
                    // same guard the timer escalation runs) — a cross the book
                    // offers but this core may not afford is not an escalation,
                    // it is a second refusal — and taking it would spend the
                    // intent's place in the placement history on a leg that
                    // never had a chance. Falling through books the original
                    // venue rejection instead, which is the same handling as
                    // "no crossing depth".
                    match self.escalation_guard(&req, worst) {
                        Ok(()) => match self.place_escalated(req, now_ms) {
                            Ok(_) => {
                                return Ok(());
                            }
                            Err(e2) => self.emit_error(e2),
                        },
                        Err(reason) => tracing::info!(
                            order = %id,
                            strategy = %req.strategy,
                            price = %worst,
                            size = %req.size,
                            reason = %reason,
                            "escalation skipped after WouldCross: the chase is refused by the full risk guard"
                        ),
                    }
                }
            }
            self.note_venue_rejection(
                &o.internal_key,
                o.strategy.clone(),
                o.account_id.as_str(),
                &e,
                now_ms,
            );
        }
        Ok(())
    }

    /// Book a venue rejection: per-strategy accounting (`ordersRejected` /
    /// `rejectionCauses`), the intent's retry cooldown with exponential
    /// backoff, the panel-visible last error, and — once the consecutive
    /// streak hits [`FREEZE_ON_REJECTS`] — the trading freeze itself.
    fn note_venue_rejection(
        &mut self,
        internal_key: &str,
        strategy: String,
        account_id: &str,
        err: &crate::model::CoreError,
        now_ms: i64,
    ) {
        self.stats.venue_rejected += 1;
        // 冻结自愈（2026-10-28 真线事故）：a close intent refused with
        // "Insufficient position" means the venue says the position is ALREADY
        // GONE — almost always the exit POST that timed out actually FILLED.
        // That is benign bookkeeping drift, not a venue defect: feeding it to
        // the freeze streaks froze accounts for hours (5 honest rejections in
        // a row = kill switch). Close intents keep their per-intent backoff
        // below; only the freeze counters are exempt. Opening intents with
        // the same reason stay counted — for them the refusal is a real gate.
        let benign_close = is_close_intent(internal_key) && is_position_already_gone(&err.message);
        if benign_close {
            if !self.place_cooldowns.contains_key(internal_key) {
                tracing::warn!(
                    key = %internal_key,
                    account = %account_id,
                    reason = %err.message,
                    "benign close-intent rejection (Insufficient position) — backoff retry continues, freeze streak not fed"
                );
            }
        } else {
            self.consecutive_venue_rejects = self.consecutive_venue_rejects.saturating_add(1);
        }
        let account_streak = if benign_close {
            0
        } else {
            let streak = self
                .consecutive_venue_rejects_by_account
                .entry(account_id.to_string())
                .or_default();
            *streak = streak.saturating_add(1);
            *streak
        };
        self.note_error(err, now_ms);
        {
            let acc = self
                .strategy_accounting
                .entry(strategy_acc_key(&strategy, account_id))
                .or_default();
            acc.rejected += 1;
            *acc.rejection_causes
                .entry(format!("venue.{:?}", err.code))
                .or_default() += 1;
        }
        let (attempts, _next_ok) = {
            let cd = self
                .place_cooldowns
                .entry(internal_key.to_string())
                .or_default();
            cd.attempts = cd.attempts.saturating_add(1);
            cd.last_error = err.message.clone();
            cd.next_ok_ms = now_ms + reject_backoff_ms(cd.attempts);
            (cd.attempts, cd.next_ok_ms)
        };
        if benign_close {
            // 良性拒单的 40 哨兵（≈15–20 分钟连续退避重试）：场所账本和本地
            // 簿已经分歧到退出永远落不下去。平仓意图本身仍继续重试（裸露
            // 敞口不能弃），分歧由 #684 的两层兜底收口：过期且场地永不裁决
            // 的持仓由 `run_expiry_fallback_sweep` 平账入历史；操作员可用
            // `positions.force_close` 点名单清（过期市场限定 + manual-ops
            // 审计）。在此之前的哨兵响声仍然值得 —— 对账 sweep 折回成交前，
            // 操作员先听见一次。
            if attempts == BENIGN_CLOSE_DIVERGENCE_ATTEMPTS {
                let message = format!(
                    "close intent {internal_key} account {account_id} rejected {attempts} times \
                     in a row (Insufficient position) — venue state diverged, reconcile sweep \
                     should fold the fill; manual attention advised"
                );
                tracing::error!("{message}");
                self.emit(Event::RiskAlert {
                    code: blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message,
                });
            }
        }
        tracing::info!(
            "venue rejected order: key={internal_key} attempts={} next_retry_in={}ms reason={}",
            attempts,
            reject_backoff_ms(attempts),
            err.message
        );
        // The freeze is per ACCOUNT: the rejections came from one venue, so
        // only the accounts that trade it stop entering. The global counter
        // survives for the legacy readout; the trip decision is the
        // account-scoped one.
        if account_streak >= FREEZE_ON_REJECTS {
            self.freeze_account_auto(
                account_id,
                format!(
                    "{account_streak} consecutive venue rejections on account {account_id}; last: {}",
                    err.message
                ),
                now_ms,
            );
        } else if attempts >= MAX_PLACE_ATTEMPTS {
            // One intent keeps bouncing. Entries stop here; closing intents
            // keep retrying past the cap (unhedged exposure may not be
            // abandoned), but both deserve ONE loud panel alert.
            let (first_time, close) = {
                let cd = self
                    .place_cooldowns
                    .get_mut(internal_key)
                    .expect("cooldown entry just written");
                let first = !cd.alerted;
                cd.alerted = true;
                (first, is_close_intent(internal_key))
            };
            if first_time {
                let status = if close {
                    format!(
                        "still retrying (close intent, backoff only) after {attempts} venue rejections: {}",
                        err.message
                    )
                } else {
                    format!(
                        "abandoned after {attempts} venue rejections: {}",
                        err.message
                    )
                };
                self.emit(Event::RiskAlert {
                    code: blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message: format!("placement intent {internal_key} {status}"),
                });
            }
        }
    }

    /// Whether an intent is currently blocked from (re)placement: `Some` while
    /// its cooldown is running or its attempt cap is spent. Callers skip
    /// silently — the rejection was already accounted for when it happened.
    fn placement_blocked(&self, internal_key: &str, now_ms: i64) -> bool {
        self.place_cooldowns.get(internal_key).is_some_and(|c| {
            now_ms < c.next_ok_ms
                || (c.attempts >= MAX_PLACE_ATTEMPTS && !is_close_intent(internal_key))
        })
    }

    /// Freeze trading (kill switch) with a reason the panel can show. A
    /// restart or an explicit `risk.resume` clears it; the startup self-check
    /// re-probes either way. The freeze is logged at error level (#184) — a kill
    /// switch is the loudest thing this kernel does, and every path that can
    /// reach it (venue-refusal streak, failed self-check, failed sweep, audit
    /// halt) must be countable in a run log.
    ///
    /// It deliberately does NOT write the error slot: the slot keeps the ROOT
    /// CAUSE (the raw venue error that pushed a streak over the line), and
    /// `engine.stats.tradingFrozen.reason` already carries this message for the
    /// panel's freeze banner. Overwriting the slot here is what would make it
    /// show a paraphrase instead of the error an operator has to act on.
    fn freeze_trading(&mut self, reason: String, now_ms: i64) {
        if self.risk.is_killed() {
            return; // already frozen — do not re-alarm on every rejection
        }
        let _ = now_ms;
        self.risk.kill(reason.clone());
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!("trading frozen: {reason}"),
        });
        tracing::error!(reason = %reason, "trading frozen (kill switch)");
        self.emit_error_event(crate::model::CoreError::new(
            blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            reason,
        ));
    }

    /// Freeze ONE account (the account-scoped kill switch). Same 「拒开仓、
    /// 放平仓」 contract as the global kill and the trading switch: the
    /// submission gate refuses the account's new orders, the engine mirror
    /// drops its entry candidates, its exits keep flowing. The reason is
    /// shown by `account.list` (`accountFreezes`) and the doctor's accounts
    /// section. Idempotent: the first reason wins — a re-freeze while
    /// already frozen does not overwrite the cause an operator first saw.
    pub fn freeze_account(&mut self, account: &str, reason: String) {
        if self.account_freezes.contains_key(account) {
            return;
        }
        self.account_freezes
            .insert(account.to_string(), reason.clone());
        self.engine_set_account_frozen(account, true);
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!("account {account} frozen: {reason}"),
        });
        tracing::error!(account = %account, reason = %reason, "account frozen (account-scoped kill switch)");
    }

    /// The AUTOMATIC freeze route: [`Self::freeze_account`] plus a timestamp
    /// in `auto_frozen` — but only when the freeze actually landed. The
    /// freeze is idempotent (first reason wins), so a re-freeze of an
    /// already-frozen account keeps the ORIGINAL stamp: the clock an
    /// auto-lift debounces against is the moment the operator first lost
    /// the account, not the latest symptom.
    ///
    /// 手动冻结（`account.freeze` 直达 `freeze_account`）从不登记 —— 操作员
    /// 的裁决只能由操作员推翻，健康探针不得越权。
    fn freeze_account_auto(&mut self, account: &str, reason: String, now_ms: i64) {
        let fresh = !self.account_freezes.contains_key(account);
        self.freeze_account(account, reason);
        if fresh {
            self.auto_frozen.insert(account.to_string(), now_ms);
        }
    }

    /// 冻结自愈的解冻半边：only an AUTO-frozen account may self-lift, and
    /// only on positive evidence the venue recovered (`trigger` names it).
    /// A manual freeze is never touched — `auto_frozen` is the ledger that
    /// separates kernel circumstantial freezes from operator verdicts.
    fn auto_lift_if_venue_healthy(&mut self, account: &str, trigger: &str, now_ms: i64) {
        let Some(frozen_at) = self.auto_frozen.get(account).copied() else {
            return;
        };
        self.resume_account(account);
        self.auto_frozen.remove(account);
        let minutes = (now_ms - frozen_at).max(0) / 60_000;
        tracing::warn!(
            account = %account,
            frozen_duration_ms = now_ms - frozen_at,
            trigger = %trigger,
            "auto-frozen account auto-lifted — venue proved healthy"
        );
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!(
                "自动冻结已解除（{trigger}）：账户 {account} 冻结 {minutes} 分钟后场地恢复"
            ),
        });
    }

    /// Lift one account's freeze (`account.freeze` with `frozen: false`, the
    /// reset path, or a future operator verb). Unknown account = no-op: the
    /// freeze book only ever contains accounts the kernel knows.
    pub fn resume_account(&mut self, account: &str) {
        if self.account_freezes.remove(account).is_some() {
            self.engine_set_account_frozen(account, false);
            tracing::info!(account = %account, "account freeze lifted");
        }
        // 手动解冻也要摘掉自愈时间戳：freeze book 与 auto ledger 对同一
        // 账户要么都有意、要么都干净，半悬空状态只会让下一次 auto-lift
        // 读到一个已不存在的冻结。
        self.auto_frozen.remove(account);
    }

    /// Why an account is frozen, when it is.
    pub fn account_freeze_reason(&self, account: &str) -> Option<&str> {
        self.account_freezes.get(account).map(String::as_str)
    }

    /// The whole freeze book, for the stats readout.
    pub fn account_freezes(&self) -> &HashMap<String, String> {
        &self.account_freezes
    }

    /// Is this account's order admitted under the account-scoped kill
    /// switch? CLOSING intents always pass (the #174 escape hatch is per
    /// account here, exactly as it is globally). Called from the submission
    /// gate right beside Gate 2.
    fn account_freeze_permits(&self, req: &OrderRequest) -> CoreResult<()> {
        if let Some(reason) = self.account_freezes.get(req.account_id.as_str())
            && !crate::risk::is_close_intent(&req.internal_key)
        {
            return Err(CoreError::new(
                CoreErrorCode::KillSwitchActive,
                format!(
                    "account `{}` is frozen (account-scoped kill switch): {reason} — \
                     new orders refused, exits keep flowing",
                    req.account_id
                ),
            ));
        }
        Ok(())
    }

    /// Push one account's freeze fact into the engine mirror. `None` = the
    /// engine is not installed (backtest/in-process shapes) — nothing to
    /// mirror.
    fn engine_set_account_frozen(&mut self, account: &str, frozen: bool) {
        if let Some(e) = self.engine.as_mut() {
            e.set_account_frozen(account, frozen);
        }
    }

    // ── In-kernel accounting audit (issue #189) ─────────────────────────────
    // The kernel's own three-way money check, on a timer: the ledger's books
    // against themselves, against the trade records, and against the venue's
    // reported free cash. A failure alerts, persists a record and blocks NEW
    // entries — deliberately not the kill switch, which (see #174) blocks closes
    // too and would trap the position the drift is about.

    /// Assemble the audit's input from the core's own state. Cheap enough to run
    /// on the maintenance tick: a handful of sums over live orders, open
    /// positions and the reservation table.
    fn build_audit_input(&self, now_ms: i64) -> AuditInput {
        // The audit's cash identity describes the ACTIVE account's book (§9.5,
        // E28): `reserved` reads the active ledger, so the live-BUY notional it
        // is compared against must count only the ACTIVE account's orders. A
        // sibling account's live BUY is spoken for in ITS book — counting it
        // here made `reservations_match_open_buys` drift by that notional
        // whenever any other account held an open order.
        let active_id = self.accounts.active_id().clone();
        let mut open_buy_notional = Decimal::ZERO;
        let mut open_buys = 0usize;
        for o in self.ome.live_orders() {
            if o.side != Side::Buy || o.account_id != active_id {
                continue;
            }
            open_buys += 1;
            open_buy_notional += o.price * (o.size - o.filled_size).max(Decimal::ZERO);
        }
        // A reservation is "stray" when the order behind it is not a tracked live
        // BUY: nobody can ever release it, so it is a permanent phantom hold.
        let stray_reservations: Vec<(OrderId, Decimal)> = self
            .accounts
            .active_ledger()
            .reservations_snapshot()
            .into_iter()
            .filter(|(id, _)| {
                !self
                    .ome
                    .get(id)
                    .map(|o| o.side == Side::Buy && o.status.is_live())
                    .unwrap_or(false)
            })
            .collect();
        // E28 (§9.2): every fold below scopes to the ACTIVE account — the
        // identity describes ONE book, so a sibling account's realized PnL,
        // pending redemption, or open position must not move it (they once
        // did: the global summary/global receivable/all-books positions fold
        // made `trade_ledger_identity` drift by exactly what the sibling
        // account traded).
        let (realized, seen_trades, has_trade_log) = match self.trade_db.as_ref() {
            Some(db) => {
                let s = db.summary_for(active_id.as_str());
                (f64_to_decimal(s.total_net_pnl), s.total_trades, true)
            }
            None => (Decimal::ZERO, 0, false),
        };
        let mut spent = Decimal::ZERO;
        let mut received = Decimal::ZERO;
        for p in self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == active_id)
        {
            spent += p.flows.entry_cost_usd + p.flows.entry_fee_usd;
            received += p.flows.proceeds_usd - p.flows.exit_fee_usd;
        }
        let open_positions = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == active_id)
            .count();
        // E28: the audit's cash identity describes the ACTIVE account's book
        // (§9.5). Cross-account isolation is asserted by the dedicated gate
        // (account-cross-isolation-check.mjs), not by widening this identity.
        let active = self.accounts.active_ledger();
        // The venue report comparable to THIS book is the report of the venue
        // the active account is BOUND to (#505): a sibling venue's cash is a
        // different book's truth, and comparing across books once produced a
        // phantom `venue_free_cash` drift of exactly (other venue's cash −
        // this book's balance).
        let active_venue = self.accounts.active_venue().to_string();
        let venue_free = self
            .venue_free_by_venue
            .get(&active_venue)
            .map(|(t, b)| (*t, *b));
        let ledger_realigned = self.audit.last_realign.get(&active_venue).copied();
        // The venue leg compares the ledger against a PAPER site's own figure
        // when this venue declared itself SIMULATED (its plugin's declaration
        // snapshot — the same `set_plugin_modes` injection the strategy
        // handshake reads): the paper site freezes its reported free cash on a
        // resting BUY and never nets the reservation back out, so the leg's
        // `balance − reserved` expectation is fee-level cents off by
        // construction and two strikes halted new entries over arithmetic that
        // could never agree (external operator report, issue #669). The
        // declaration is the plugin's identity — it cannot change mid-session.
        let venue_simulated = self
            .plugin_modes
            .iter()
            .find(|p| p.name == active_venue)
            .map(|p| {
                p.modes.iter().any(|m| {
                    m.capabilities
                        .satisfies(blitzkrieg_market_api::MarketCapabilities::SIMULATED)
                })
            })
            .unwrap_or(false);
        AuditInput {
            now_ms,
            live: self.config.mode == Mode::Live,
            identity: CashIdentity {
                balance: active.balance(),
                // Settled but not yet redeemed: owed to US — this book's us,
                // not the process's (each claim carries the account it was
                // booked from, E28 §9.2).
                receivable: self.settlement.receivable_usd_for(active_id.as_str())
                    + self.settlement.merge_receivable_usd_for(active_id.as_str()),
                realized,
                spent,
                received,
            },
            reserved: active.reserved(),
            ledger_balanced: active.is_balanced(),
            unfunded_reserved: active.unfunded_reserved(),
            open_buy_notional,
            open_buys,
            stray_reservations,
            venue_free: venue_free.map(|(_, b)| b),
            venue_free_at_ms: venue_free.map(|(t, _)| t),
            ledger_realigned,
            open_positions,
            seen_trades,
            has_trade_log,
            venue_simulated,
        }
    }

    /// Run the accounting audit now and act on its verdict: alert, persist, and
    /// block/halt new entries. Returns the report (also kept for the panel).
    ///
    /// Called from `tick` on `audit_interval_sec`, and directly by tests.
    pub fn run_accounting_audit(&mut self, now_ms: i64) -> AuditReport {
        let input = self.build_audit_input(now_ms);
        let report = crate::reconcile::audit_accounting(&input, self.audit.anchor.as_ref());

        // Anchor: taken at the first audit the STRUCTURAL checks pass, then never
        // moved while anything is failing — re-anchoring on a failure would hide
        // exactly the movement the identity exists to expose.
        if self.audit.anchor.is_none() && !report.structural_failure {
            self.audit.anchor = Some(input.identity);
        }
        self.audit.runs += 1;
        if report.ok {
            self.audit.cross_source_failures = 0;
        } else {
            self.audit.failures += 1;
            if report.structural_failure {
                // Local inconsistency: no fetch race to forgive.
                self.audit.cross_source_failures = 0;
            } else {
                self.audit.cross_source_failures += 1;
            }
        }
        let cross_strike = self.audit.cross_source_failures >= 2;
        // A halt is warranted by a structural failure immediately, by a venue
        // mismatch only after it repeats (a single stale read must not stop
        // entries), and only when the operator left the brake enabled.
        let should_halt = !report.ok
            && (report.structural_failure || cross_strike)
            && self.config.audit_halt_entries;
        let was_halted = self.audit.halted;
        let mut resumed = false;

        if should_halt && !was_halted {
            self.audit.halted = true;
            self.audit.halt_reason = report.note.clone();
            let message = format!(
                "accounting audit FAILED — new entries blocked until it passes: {}",
                report.note
            );
            eprintln!("core: {message}");
            tracing::error!(drift = %report.drift_usd, "{}", message);
            // The halt is an error of the highest order (entries are blocked), so
            // it also lands in the one panel-visible slot (#180).
            self.note_error(
                &CoreError::new(CoreErrorCode::Internal, message.clone()),
                now_ms,
            );
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message,
            });
        } else if should_halt {
            self.audit.halt_reason = report.note.clone();
        } else if was_halted {
            self.audit.halted = false;
            self.audit.halt_reason.clear();
            resumed = true;
            let message = format!(
                "accounting audit recovered — new entries resumed ({})",
                report.note
            );
            eprintln!("core: {message}");
            tracing::info!("{message}");
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message,
            });
        } else if !report.ok {
            // Failing but not (yet) halting: still loud, still persisted.
            let message = format!("accounting audit drift: {}", report.note);
            eprintln!("core: {message}");
            tracing::warn!(drift = %report.drift_usd, "{}", message);
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message,
            });
        }

        // Persist on every failure, every halt/resume, and otherwise as a
        // heartbeat — a healthy audit is the evidence that the check RAN. The
        // very first audit is written too: a fresh process must be able to prove
        // it checked before anything went wrong.
        let heartbeat = self.audit.last_heartbeat_ms == 0
            || now_ms - self.audit.last_heartbeat_ms >= AUDIT_HEARTBEAT_MS;
        if !report.ok || should_halt || was_halted || resumed || heartbeat {
            self.audit.last_heartbeat_ms = now_ms;
            self.persist_audit_report(&report);
        }
        self.audit.report = Some(report.clone());
        self.audit.last_at_ms = now_ms;
        report
    }

    /// The latest audit verdict, for the panel (and for a caller that wants the
    /// report without running one).
    pub fn accounting_audit(&self) -> Option<&AuditReport> {
        self.audit.report.as_ref()
    }

    /// New entries are blocked by a failing accounting audit (issue #189).
    pub fn audit_blocks_entries(&self) -> bool {
        self.audit.halted
    }

    /// Why new entries are blocked (empty when they are not).
    pub fn audit_halt_reason(&self) -> &str {
        &self.audit.halt_reason
    }

    /// The gate itself: refuses a NEW entry while the audit is failing. Exits
    /// (SELL) never consult this — a drifted ledger must not be able to trap the
    /// bot in a position, which is what routing this through the kill switch did
    /// (#174).
    fn audit_entry_gate(&self) -> CoreResult<()> {
        if self.audit.halted {
            return Err(CoreError::new(
                CoreErrorCode::RiskRejected,
                format!(
                    "accounting audit halted new entries: {}",
                    self.audit.halt_reason
                ),
            ));
        }
        Ok(())
    }

    /// Mutable ledger access, for tests that must put the books into the exact
    /// state a bug leaves behind (a stranded reservation, cash that moved with no
    /// trade) and assert the audit NOTICES it. Gated behind the `test-support`
    /// feature the crate enables for its own test targets, so no production build
    /// can reach it.
    #[cfg(feature = "test-support")]
    pub fn ledger_mut(&mut self) -> &mut Ledger {
        // E28: tests poking the books target the ACTIVE account's ledger —
        // the same default a 0.2 deployment has exactly one of.
        self.accounts.active_ledger_mut()
    }

    /// Append one audit verdict (or a late-fill record) to the audit log.
    fn persist_audit_value(&self, value: serde_json::Value) {
        let Some(path) = self.audit_log_path() else {
            return;
        };
        crate::jsonl::append(std::path::Path::new(path), &value);
    }

    fn persist_audit_report(&self, report: &AuditReport) {
        match serde_json::to_value(report) {
            Ok(v) => self.persist_audit_value(v),
            Err(e) => tracing::warn!(error = %e, "cannot serialize audit report"),
        }
    }

    fn audit_log_path(&self) -> Option<&str> {
        self.audit.audit_log_path.as_deref()
    }

    /// Trading-capability self-check result from the live venue bridge. A
    /// failed report freezes trading — the probe exercised the venue paths a
    /// live order shares (authenticated balance, reconciliation sweep), so
    /// failing them means order egress cannot work either.
    ///
    /// 2026-10-25 多场馆雪崩定谳：the freeze is VENUE-SCOPED — the failing
    /// probes are one venue's transport, so only the accounts bound to that
    /// venue stop entering. A global freeze here let polymarket's network
    /// blip veto a healthy polysimulator account's entries for the rest of
    /// the session (the two venues "took turns" being alive — each one's
    /// failure froze the other's survivors). The legacy global
    /// `freeze_trading` stays for causes with no venue dimension (audit
    /// halt, or a venue with no bound account); the `engine.stats.selfCheck`
    /// readout carries the last report regardless of which venue filed it.
    pub fn on_venue_self_check(
        &mut self,
        venue: &str,
        report: blitzkrieg_market_api::SelfCheckReport,
    ) {
        let ok = report.ok;
        let ts = report.ts_ms;
        for item in &report.items {
            eprintln!(
                "core: self-check [{}] {}: {} ({})",
                venue,
                item.name,
                if item.ok { "ok" } else { "FAIL" },
                item.detail
            );
        }
        self.last_self_check = Some(report);
        if !ok {
            // #184: name the failing probe(s) in the message, not just "failed".
            // The items are already in `engine.stats.selfCheck`, but an operator
            // reading the log needs the reason on the line itself.
            let failed = self
                .last_self_check
                .as_ref()
                .map(|r| {
                    r.items
                        .iter()
                        .filter(|i| !i.ok)
                        .map(|i| format!("{}: {}", i.name, i.detail))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let message = if failed.is_empty() {
                format!(
                    "trading self-check failed on {venue} (venue unreachable or credentials rejected)"
                )
            } else {
                format!(
                    "trading self-check failed on {venue}: {}",
                    failed.join("; ")
                )
            };
            tracing::error!(detail = %message, venue = %venue, "trading self-check failed");
            // Root cause into the one slot (code VENUE_ERROR, not the freeze's
            // own wording) — this path has no other carrier of its own.
            self.note_error(
                &crate::model::CoreError::new(
                    blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message.clone(),
                ),
                ts,
            );
            // The freeze lands on the venue's bound accounts (multi-account
            // #532 routing): same contract as `freeze_account` — new entries
            // refused, closes keep flowing.
            for account in self.accounts.account_ids_bound_to_venue(venue) {
                self.freeze_account_auto(&account, message.clone(), ts);
            }
            // No bound account (unbound deployments, dry tests): keep the
            // legacy global freeze so the failure is never silently dropped.
            if !self.accounts.account_ids_bound_to_venue(venue).is_empty() {
                return;
            }
            self.freeze_trading(message, ts);
        }
    }

    /// E31-b: the executor's reconciliation sweep failed three times in a row.
    /// The sweep is the safety net that repairs missed fills, retires ghosts
    /// and cancels orphans; a blind sweep lets the book drift silently, so
    /// trading freezes until the operator or a restart re-probes the channel.
    ///
    /// VENUE-SCOPED (2026-10-25 多场馆雪崩定谳): `venue` names the reporting
    /// plugin; the freeze lands on the accounts bound to it. The per-venue
    /// streak lives in `consecutive_sweep_failures_by_venue` — a healthy
    /// sibling venue's sweeps keep resetting nothing here, and their accounts
    /// keep trading. The legacy global counter/`freeze_trading` path survives
    /// only for callers with no venue attribution (the in-kernel dry sweep).
    pub fn on_venue_reconcile_failed(
        &mut self,
        venue: &str,
        err: crate::model::CoreError,
        now_ms: i64,
    ) {
        let streak = self
            .consecutive_sweep_failures_by_venue
            .entry(venue.to_string())
            .or_default();
        *streak = streak.saturating_add(1);
        let streak = *streak;
        let message = format!("reconciliation sweep failed ({streak}): {}", err.message);
        // #184: the safety net reporting on itself is an ops-critical line, and it
        // belongs at error level from the FIRST failure — not only when the third
        // one finally freezes trading. The streak and the threshold ride along so
        // one line answers "how bad is this and what happens next".
        tracing::error!(
            venue = %venue,
            streak,
            threshold = SWEEP_FAILURE_FREEZE,
            code = ?err.code,
            "{}",
            message
        );
        self.note_error(
            &crate::model::CoreError::new(err.code, message.clone()),
            now_ms,
        );
        if streak >= SWEEP_FAILURE_FREEZE {
            let accounts = self.accounts.account_ids_bound_to_venue(venue);
            if accounts.is_empty() {
                self.freeze_trading(
                    format!(
                        "reconciliation sweep failed {streak} times in a row on {venue}: {}",
                        err.message
                    ),
                    now_ms,
                );
                return;
            }
            for account in accounts {
                self.freeze_account_auto(
                    &account,
                    format!(
                        "reconciliation sweep failed {streak} times in a row on {venue}: {}",
                        err.message
                    ),
                    now_ms,
                );
            }
        }
    }

    /// Legacy global form kept for callers with no venue attribution. The
    /// in-kernel dry sweep and any pre-plugin path land here.
    pub fn on_reconcile_failed(&mut self, err: crate::model::CoreError, now_ms: i64) {
        self.consecutive_sweep_failures = self.consecutive_sweep_failures.saturating_add(1);
        let streak = self.consecutive_sweep_failures;
        let message = format!("reconciliation sweep failed ({streak}): {}", err.message);
        // #184: the safety net reporting on itself is an ops-critical line, and it
        // belongs at error level from the FIRST failure — not only when the third
        // one finally freezes trading. The streak and the threshold ride along so
        // one line answers "how bad is this and what happens next".
        tracing::error!(
            streak,
            threshold = SWEEP_FAILURE_FREEZE,
            code = ?err.code,
            "{}",
            message
        );
        self.note_error(
            &crate::model::CoreError::new(err.code, message.clone()),
            now_ms,
        );
        if streak >= SWEEP_FAILURE_FREEZE {
            self.freeze_trading(
                format!(
                    "reconciliation sweep failed {streak} times in a row: {}",
                    err.message
                ),
                now_ms,
            );
        }
    }

    /// Apply an authoritative venue/user-WS fill (cumulative size) through the
    /// same idempotent ledger + OME path the dry matcher uses, then project the
    /// resulting delta onto the position book.
    pub fn ingest_fill(&mut self, fill: Fill, now_ms: i64) -> CoreResult<()> {
        let outcome = self.ome.apply_fill_report(fill, now_ms)?;
        self.report_fill_outcome(outcome, now_ms);
        Ok(())
    }

    /// Ledger/position projection plus the late-fill signal for one applied fill
    /// (issue #178). A late fill still moves size and cash; what it must NOT do is
    /// rewrite a terminal order status, and it must reach the operator.
    fn report_fill_outcome(&mut self, outcome: FillOutcome, now_ms: i64) {
        if let Some(d) = outcome.delta {
            self.apply_delta_effects(d, now_ms);
        }
        if let Some(late) = outcome.late_fill {
            self.emit_late_fill(&late, now_ms);
        }
    }

    /// Surface a fill that landed on an order which had already ended (or that
    /// reports more size than the order could hold). Returns the alert text.
    ///
    /// `ipc/schema.rs` is outside this change's territory, so this rides the
    /// existing `RiskAlert` event rather than adding a dedicated variant; the
    /// message is a greppable `late_fill …` line so an operator, the audit log
    /// and the panel all show the same fact without a new wire type.
    pub fn emit_late_fill(&mut self, late: &LateFill, now_ms: i64) -> String {
        let status = late
            .absorbed_by
            .map(|s| format!("{s:?}"))
            .unwrap_or_else(|| "live".to_string());
        let message = format!(
            "late_fill order={} trade={} status={} size={} cumulative={}/{} reported={} overfill={}",
            late.order_id,
            if late.trade_id.is_empty() {
                "-"
            } else {
                &late.trade_id
            },
            status,
            late.size,
            late.cumulative,
            late.order_size,
            late.reported_cumulative,
            late.overfill,
        );
        tracing::warn!(
            order = %late.order_id,
            trade = %late.trade_id,
            status = %status,
            size = %late.size,
            cumulative = %late.cumulative,
            order_size = %late.order_size,
            reported_cumulative = %late.reported_cumulative,
            overfill = late.overfill,
            "fill arrived for an order that had already ended (status kept, size booked)"
        );
        eprintln!("core: {message}");
        self.emit(Event::RiskAlert {
            code: if late.overfill {
                // The venue thinks more shares exist than the order asked for:
                // over-delivery, the one case that can exceed our own books.
                CoreErrorCode::RiskRejected
            } else {
                CoreErrorCode::Internal
            },
            message: message.clone(),
        });
        self.persist_audit_value(serde_json::json!({
            "atMs": now_ms,
            "event": "late_fill",
            "orderId": late.order_id,
            "tradeId": late.trade_id,
            "absorbedBy": status,
            "size": late.size.to_string(),
            "cumulative": late.cumulative.to_string(),
            "orderSize": late.order_size.to_string(),
            "reportedCumulative": late.reported_cumulative.to_string(),
            "overfill": late.overfill,
        }));
        message
    }

    /// The taker fee a LIVE charge site may use, at `price` (#234, item 4).
    ///
    /// The schedule is process-wide and set-once, and only a replay may install
    /// one other than the shipped default — `--fee-model` is refused without
    /// `--backtest`, and the backtester is the only caller of `set_fee_schedule`.
    /// That isolation used to live ENTIRELY in the CLI: the two live charge sites
    /// read whatever was installed and could not tell a replay from a live
    /// session. This makes the invariant explicit where the money is: a core that
    /// has not declared itself a replay charges the shipped schedule or the
    /// process stops.
    ///
    /// It fails CLOSED on purpose. A counterfactual fee on a real fill is a wrong
    /// number in the ledger, in every PnL derived from it and in the loss
    /// breakers that read them — silent and unrepairable after the fact — while a
    /// refused charge is loud and stops before it can book. If a live run ever
    /// legitimately needs another schedule, that is a design change to the charge
    /// path, not a flag flip.
    fn live_taker_fee_pct(&self, price: Decimal) -> Decimal {
        let schedule = crate::exit_policy::fee_schedule();
        assert!(
            self.may_charge(schedule),
            "non-replay core would charge the '{}' taker-fee schedule: --fee-model is \
             replay-only (set CoreConfig::fee_schedule_replay only in a backtest) (#234)",
            schedule.name
        );
        schedule.fee_pct(price)
    }

    /// Whether a charge under `schedule` is legal for THIS core (#234, item 4).
    /// Split out from [`Core::live_taker_fee_pct`] so the predicate can be tested
    /// without installing a schedule: the process-wide schedule is set-once, so a
    /// test cannot put a second one in place.
    fn may_charge(&self, schedule: crate::exit_policy::FeeSchedule) -> bool {
        self.config.fee_schedule_replay
            || schedule == crate::exit_policy::legacy_quadratic_schedule()
    }

    /// Ledger + position projection for a canonical fill delta. Single choke
    /// point shared by dry fills, live user-WS fills and reconciliation gaps.
    ///
    /// E17: the fee follows the fill's RESOLVED role (`d.role`), and the cash
    /// movement and the position accrual are computed from ONE fee value, so the
    /// ledger and the trade record cannot disagree about what was paid.
    ///
    /// #181: this is also where the BUY reservation is retired. Two numbers move:
    /// the CASH (the execution price that actually left) and the RESERVATION (the
    /// limit-price notional those shares had reserved up front). They differ on a
    /// price-improved fill, and then the reservation is re-synced to the order's
    /// real outstanding commitment — so a full fill, a partial fill and a
    /// terminal transition all leave it exactly right rather than leaving residue
    /// behind (which no cancel path would ever release: a Filled order never gets
    /// cancelled).
    fn apply_delta_effects(&mut self, d: FillDelta, now_ms: i64) {
        let px = d.price;
        // RFC 568 阶段一：配对簿的成交事实喂入（观察者、非判定输入）。
        // FillDelta 不携带 internal_key —— 经 ome.get 取 TrackedOrder 的
        // internal_key；BUY 入腿的正 delta 才入账（SELL 是退出，回滚不
        // 改成交事实的正累计）。未命中声明腿的事件是空操作。
        if d.side == Side::Buy
            && d.delta > Decimal::ZERO
            && let Some(order) = self.ome.get(&d.order_id)
        {
            let key = order.internal_key.clone();
            self.pair_on_order_event(&key, d.delta, false);
        }
        // Replay fast path: a fill advances (or retires) a live order and
        // moves the position book — the escalation floor and the settlement
        // watch may both change on the next tick. It is also an ENGINE INPUT:
        // the strategy's merge-intent emission keys on position completeness,
        // and the equity push feeds sizing — the next evaluation must run so
        // its output lands at the same virtual instant the every-tick pass
        // would have produced it.
        if self.replay_sched() {
            self.fast_orders_dirty = true;
            self.fast_eval_wake = true;
        }
        // The fee reads only the schedule in force (an &self read), so it is
        // computed BEFORE the account's ledger is taken mutably (E28: the two
        // borrows must not overlap).
        let mut fee_usd = Decimal::ZERO;
        if d.delta > Decimal::ZERO {
            let notional = px * d.delta;
            let fee_pct = if d.role.is_maker() {
                Decimal::ZERO
            } else {
                self.live_taker_fee_pct(px)
            };
            fee_usd = (fee_pct / Decimal::ONE_HUNDRED) * notional;
        }
        // E28 (§9.3): every cash movement below lands in the fill's OWN book —
        // the delta carries the account its tracked order was placed with.
        // An unknown account here is a data-integrity fault (a fill arrived
        // for an account this core never configured): it is logged and the
        // fill is dropped rather than silently minting an implicit book
        // (reverse acceptance C) — the position projection and the event
        // emission below are skipped with it.
        let Ok(ledger) = self.accounts.get_mut(&d.account_id) else {
            tracing::error!(
                order = %d.order_id,
                account = %d.account_id.as_str(),
                "fill for an unknown account dropped — the money fact is not booked"
            );
            return;
        };
        // A maker fill pays no fee; a taker fill pays the schedule in force on the
        // fill's own notional. `d.role` is what the fill actually did — the old
        // `match d.mode` read the REQUESTED policy, which charged a
        // MakerThenTaker order the taker fee while its position record said maker.
        // Fee applies to the incremental delta (not the cumulative), so partial
        // fills charge exactly once per share; rollbacks (delta <= 0) charge none.
        if d.delta > Decimal::ZERO {
            let notional = px * d.delta;
            match d.side {
                Side::Buy => {
                    // Release the notional reserved at the LIMIT price for these
                    // shares, and pay the execution price in cash.
                    ledger.settle_buy_fill(&d.order_id, notional, d.limit_price * d.delta);
                    ledger.charge_fee(fee_usd);
                }
                Side::Sell => ledger.settle_sell_fill(notional, fee_usd),
            }
        } else {
            // Rollback/correction: revert the cash with no fee.
            match d.side {
                Side::Buy => ledger.settle_sell_fill(-px * d.delta, Decimal::ZERO),
                // A reversed SELL gives the cash back to the venue; no reservation
                // is involved (SELLs never reserve), so the release is zero.
                Side::Sell => {
                    ledger.settle_buy_fill(&d.order_id, -px * d.delta, Decimal::ZERO);
                }
            }
        }
        // Re-sync the reservation to the order's true outstanding commitment: the
        // filled shares are no longer committed (zero when the order ended); a
        // BUY rollback puts them back.
        if d.side == Side::Buy {
            let target = match self.ome.get(&d.order_id) {
                Some(o) if o.status.is_live() => o.price * self.ome.remaining(&d.order_id),
                _ => Decimal::ZERO,
            };
            let before = ledger.sync_buy_reservation(&d.order_id, target);
            if before > Decimal::ZERO && target == Decimal::ZERO {
                tracing::debug!(
                    order = %d.order_id,
                    released = %before,
                    "buy reservation retired (order no longer has an outstanding commitment)"
                );
            }
            // E-VA: the fill converts its own slice of the VIRTUAL hold into
            // position occupancy — released at the LIMIT price (the basis the
            // shares were reserved against, not the execution price), for the
            // signed share delta so a rollback re-arms the hold by the same
            // amount. Positive delta only: a rollback's re-arm happens
            // through the position book (the next cycle's batch check counts
            // the position), not by re-reserving here.
            if d.delta > Decimal::ZERO
                && let Some(order) = self.ome.get(&d.order_id)
            {
                let account = order.account_id.clone();
                let strategy = order.strategy.clone();
                let internal_key = order.internal_key.clone();
                let limit = d.limit_price;
                let shares = d.delta;
                self.virtual_accounts.release(
                    account.as_str(),
                    &strategy,
                    &internal_key,
                    limit * shares,
                );
            }
        }
        self.project_fill_delta(&d, fee_usd, now_ms);
        self.emit_fill(d);
        // The idempotency table changed with this fill; get it on disk before the
        // next event can arrive (issue #178).
        self.persist_applied_journal();
    }

    /// Keep the position book in sync with fills: BUY opens/averages-in, SELL
    /// reduces/closes. One position per (ACCOUNT, token) — 账户↔venue 解耦:
    /// two accounts running the same strategy on the same token are two
    /// independent books, so the token alone no longer identifies the row
    /// (the old single-key merge landed paper's fill onto default's position
    /// and left `count_positions("paper") == 0`). The account rides the
    /// `FillDelta` (E28: copied from the tracked order), so every legacy
    /// caller that never named an account still keys to `default` — the
    /// single-book behaviour is byte-identical.
    ///
    /// `fee_usd` is the cash fee the ledger just charged for this same delta, so
    /// the position's cost basis is the money actually spent (E17-d).
    fn project_fill_delta(&mut self, d: &FillDelta, fee_usd: Decimal, now_ms: i64) {
        let px = d.price;
        let token = &d.token_id;
        let account = &d.account_id;
        // Replay fast path: a fill is the one thing that changes the position
        // set (open/close/drop) and the token's mark. #390: the nofast
        // replay's deadline-scheduled ladder reads the same marker — a fill
        // is a book-level input to the ladder's next verdict.
        if self.replay_sched() {
            self.fast_positions_dirty = true;
        }
        self.fast_dirty_tokens.insert(token.clone());
        match d.side {
            Side::Buy => {
                let existing = self
                    .positions
                    .open_positions()
                    .iter()
                    .find(|p| &p.token_id == token && &p.account_id == account)
                    .map(|p| p.id.clone());
                let id = match existing {
                    Some(id) => id,
                    None => {
                        let direction = parse_direction(&d.direction);
                        // B1b: the fill's own order carries the family it was
                        // admitted under; the three-state eff (own stamp >
                        // market declaration > global grid) projects the slot
                        // onto the family grid the ORDER actually traded, not
                        // the configured grid. Single-family equivalence: a
                        // deployment on the configured grid alone folds eff ==
                        // cfg, byte-for-byte the historic formula. (Falls back
                        // to the global grid when the order is unknown to the
                        // OME — a synthetic/reconciliation fill.)
                        let eff_sec = self
                            .ome
                            .get(&d.order_id)
                            .map(|o| self.order_eff_round_sec(o))
                            .filter(|eff| *eff > 0)
                            .unwrap_or(self.config.round_duration_sec);
                        // Segment 2 of the wiring template: the lazy stamp —
                        // the order's family is written from the resolution
                        // this projection just made (idempotent; a non-0
                        // stamp is never overwritten).
                        self.ome.backfill_round_duration(&d.order_id, eff_sec);
                        // A round slot N covers [N*dur, (N+1)*dur): the market
                        // expires at the END of the slot. (Using N*dur would place
                        // expiry in the past and force-exit immediately.)
                        let expires_at_ms = if d.round_slot > 0 {
                            (d.round_slot + 1) * eff_sec * 1000
                        } else {
                            now_ms + eff_sec * 1000
                        };
                        let p = OpenParams {
                            strategy: d.strategy.clone(),
                            asset: d.asset.clone(),
                            direction,
                            token_id: d.token_id.clone(),
                            condition_id: d.condition_id.clone(),
                            entry_price: d.price,
                            expires_at_ms,
                            was_maker: d.role.is_maker(),
                            target_exit_price: None,
                            // E28 (§9.2): the position belongs to the account
                            // whose fill opened it.
                            account_id: d.account_id.clone(),
                            // #427: the panels' Venue column, never a decision
                            // input. The stamp is the ORDER'S EXECUTION venue
                            // — under the (venue, token) dual-key table the
                            // stamp equals the fill's own row's venue; for a
                            // shared token the placing account's binding
                            // picks its own row (the same read the executor
                            // drain routes by). Pre-M1 it was a correction
                            // over the token-unique table's first-row venue:
                            // without it the position/trade rows carried a
                            // venue tag the account-bound venue filter hid —
                            // the history badge counted a trade no row ever
                            // showed.
                            venue: self.execution_venue_of(d.account_id.as_str(), &d.token_id),
                            // The condition's other token, stamped from the
                            // SAME market lookup: the dry settlement ladder's
                            // counter-evidence key. Empty when no market is
                            // live for the token — the ladder then has no
                            // partner to consult and withholds as before.
                            partner_token_id: self
                                .engine
                                .as_ref()
                                .and_then(|e| e.partner_of(&d.token_id))
                                .unwrap_or_default(),
                            // B1b: the family the entry was admitted under —
                            // the order's own resolution (its stamp, else the
                            // market declaration, else the grid), so the
                            // position and the order that opened it carry the
                            // SAME family.
                            round_duration_sec: eff_sec,
                        };
                        self.positions.open(p, now_ms).id
                    }
                };
                // Accrue the basis, the fee and the role from the actual fill.
                // `d.delta` is SIGNED: a FAILED rollback reverses the accrual by
                // the same amounts the ledger just refunded, so a rolled-back
                // entry leaves neither cash nor shares behind.
                self.positions
                    .apply_entry_fill(&id, d.delta, px, fee_usd, d.role);
                // A fully rolled-back entry has no shares and no basis: drop the
                // shell so it cannot block `can_open` or render as an empty row.
                self.positions.drop_if_empty(&id);
                self.persist_positions();
            }
            Side::Sell => {
                let found = self
                    .positions
                    .open_positions()
                    .iter()
                    .find(|p| &p.token_id == token && &p.account_id == account)
                    .map(|p| p.id.clone());
                match found {
                    Some(id) => {
                        // F4: the row BEFORE this exit fill accrues is the exact
                        // inventory a later FAILED status for this trade must
                        // restore (after apply_exit_fill the row is a zeroed
                        // shell — useless for a restore).
                        let pre_exit = self
                            .positions
                            .open_positions()
                            .iter()
                            .find(|p| p.id == id)
                            .cloned();
                        // Accrue the exit first, so the close reads final flows.
                        let left = self
                            .positions
                            .apply_exit_fill(&id, d.delta, px, fee_usd, d.role);
                        // A reversal that is already accounted for on a CLOSED
                        // trade has nothing open to accrue onto; the cash is
                        // refunded but the realized record stands. Say so — a
                        // silent drop here would be exactly the drift E17 exists
                        // to remove.
                        if d.delta < Decimal::ZERO && left.is_none() {
                            self.emit(Event::Error {
                                error: CoreError::new(
                                    CoreErrorCode::Internal,
                                    format!(
                                        "exit rollback {} for token {token} has no open position to reverse",
                                        d.order_id
                                    ),
                                ),
                            });
                            return;
                        }
                        match left {
                            // Sub-grid remainder: nothing sellable is left, so the
                            // position is done — it must close, or it would sit on
                            // the books forever with an untradeable stub of shares.
                            Some(left)
                                if left <= Decimal::ZERO
                                    || crate::position::floor_to_grid(left) == Decimal::ZERO =>
                            {
                                let reason =
                                    self.take_exit_reason(token).unwrap_or(ExitReason::Manual);
                                let was_maker = d.role.is_maker();
                                let streak_before = self.breaker.consecutive_losses(&d.strategy);
                                // E26 (§4.2): the account streak's count just
                                // before this close, from the SAME map — the
                                // evidence a FAILED rollback needs, captured
                                // whether or not the bound is armed.
                                let account_streak_before = self.breaker.consecutive_losses(
                                    &crate::risk::limits::account_breaker_key(
                                        d.account_id.as_str(),
                                    ),
                                );
                                if let Some(closed) =
                                    self.positions.close(&id, px, reason, was_maker, now_ms)
                                {
                                    if let Some(restored) = pre_exit {
                                        self.push_close_credential(
                                            d.trade_id.clone(),
                                            CloseCredential {
                                                restored,
                                                closed: closed.clone(),
                                                streak_before,
                                                account_streak_before,
                                            },
                                        );
                                    }
                                    self.persist_positions();
                                    self.on_position_closed(&closed, now_ms);
                                }
                            }
                            Some(_) => {
                                self.persist_positions();
                            }
                            None => {}
                        }
                    }
                    None if d.delta < Decimal::ZERO => {
                        // F4: the fill this rollback reverses may have already
                        // fully closed the position and booked the close
                        // (TradeDb / strategy PnL / breaker / daily PnL). Unwind
                        // all of it; only a rollback with NO credential on file
                        // is unexplainable. The credential pool is filtered to
                        // THIS account's positions: two accounts can hold the
                        // same token, and a default leg's rollback must never
                        // unwind paper's close (the (account, token) merge key
                        // above is what makes both rows exist).
                        let consumed = self
                            .close_credentials
                            .iter()
                            .position(|(tid, cred)| {
                                *tid == d.trade_id && cred.restored.account_id == d.account_id
                            })
                            .map(|i| self.close_credentials.remove(i).unwrap().1);
                        // #463: tombstone the consumption so a restart replay
                        // does not resurrect a credential already unwound.
                        if let (true, Some(path)) =
                            (consumed.is_some(), self.close_credential_log.as_ref())
                        {
                            crate::jsonl::append(
                                path,
                                &CloseCredentialLine::Consumed {
                                    trade_id: d.trade_id.clone(),
                                },
                            );
                        }
                        match consumed {
                            Some(cred) => self.unwind_closed_trade(cred, d, now_ms),
                            // No position at all for the token: the reversal cannot
                            // be applied. Never swallow it.
                            None => self.emit(Event::Error {
                                error: CoreError::new(
                                    CoreErrorCode::Internal,
                                    format!(
                                        "exit rollback {} for token {token} has no open position to reverse",
                                        d.order_id
                                    ),
                                ),
                            }),
                        }
                    }
                    None => {}
                }
            }
        }
    }

    /// Breaker + event emission when a position closes.
    fn on_position_closed(&mut self, closed: &crate::position::ClosedPosition, now_ms: i64) {
        // Replay fast path: the position set changed (merge/settlement/exit
        // close all funnel here) — the settlement watch and audit input must
        // resync on the next tick.
        if self.replay_sched() {
            self.fast_positions_dirty = true;
        }
        // The pending-close reason for this token died with the position it
        // belonged to (issue #190). Every close path funnels through here, and
        // an entry left behind would be inherited by the NEXT position on the
        // same token — the ghost reason that mis-attributed exits.
        self.clear_exit_reason(&closed.token_id);
        // Persist first so the panel/analysis have the record even if a later
        // step fails (matches the Node behaviour of saving on close).
        if let Some(db) = self.trade_db.as_mut() {
            let rec = crate::trade_db::TradeRecord::from_closed(closed);
            db.record(&rec, now_ms);
        }
        // Per-strategy accounting (P-1.1): session realized PnL + fee totals,
        // keyed (strategy, account) so the panel's per-account slice sums ONLY
        // the active account's closes (PR H).
        {
            let entry_fee =
                (closed.entry_fee_pct / Decimal::ONE_HUNDRED) * closed.entry_price * closed.shares;
            let exit_fee =
                (closed.exit_fee_pct / Decimal::ONE_HUNDRED) * closed.exit_price * closed.shares;
            let acc = self
                .strategy_accounting
                .entry(strategy_acc_key(
                    &closed.strategy,
                    closed.account_id.as_str(),
                ))
                .or_default();
            acc.closed_trades += 1;
            if closed.net_pnl_usd >= Decimal::ZERO {
                acc.wins += 1;
            } else {
                acc.losses += 1;
            }
            acc.fees_usd += entry_fee + exit_fee;
            acc.net_pnl_usd += closed.net_pnl_usd;
        }
        // KI-10 / D-18 A: the streak is attributed to the position's own
        // strategy, so only that leg freezes. `PositionClosed` already carries
        // the strategy name, so no new data is needed for attribution.
        let tripped = self
            .breaker
            .record(&closed.strategy, closed.net_pnl_usd, now_ms);
        self.emit(Event::PositionClosed {
            id: closed.id.clone(),
            asset: closed.asset.clone(),
            direction: closed.direction.as_str().to_string(),
            reason: format!("{:?}", closed.exit_reason),
            strategy: closed.strategy.clone(),
            token_id: closed.token_id.clone(),
            condition_id: closed.condition_id.clone(),
            entry_price: closed.entry_price,
            account_id: closed.account_id.clone(),
            net_pnl_usd: closed.net_pnl_usd,
            net_pnl_pct: closed.net_pnl_pct,
            daily_pnl_usd: self.positions.daily_pnl_for(closed.account_id.as_str()),
        });
        if tripped {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::RiskRejected,
                message: format!(
                    "consecutive-loss breaker tripped for strategy {}: {} losses, halting its new entries {}s",
                    closed.strategy,
                    self.breaker.consecutive_losses(&closed.strategy),
                    self.config.breaker_cooldown_sec
                ),
            });
        }
        // E26 (§4.2): the ACCOUNT-level streak lives in the SAME breaker map
        // under the reserved `__account__:` key, with the account matrix's
        // own pair (`max_consecutive_losses`, `cooldown_minutes` → secs; a
        // cooldown the matrix does not name falls back to the shipped
        // `breaker_cooldown_sec`). Off at the factory: nothing is recorded
        // unless the operator armed the bound.
        {
            let account_limits = &self.risk.config().systemic.account;
            let max_losses =
                u32::try_from(account_limits.max_consecutive_losses.value().floor()).unwrap_or(0);
            if max_losses >= 1 {
                let cooldown_sec = if account_limits.cooldown_minutes.is_enabled() {
                    i64::try_from(account_limits.cooldown_minutes.value() * Decimal::from(60))
                        .unwrap_or(self.config.breaker_cooldown_sec)
                } else {
                    self.config.breaker_cooldown_sec
                };
                let account_key =
                    crate::risk::limits::account_breaker_key(closed.account_id.as_str());
                let account_tripped = self.breaker.record_with(
                    &account_key,
                    max_losses,
                    cooldown_sec,
                    closed.net_pnl_usd,
                    now_ms,
                );
                if account_tripped {
                    self.emit(Event::RiskAlert {
                        code: CoreErrorCode::RiskRejected,
                        message: format!(
                            "consecutive-loss breaker tripped for account {}: {} losses, halting its new entries {}s",
                            closed.account_id.as_str(),
                            self.breaker.consecutive_losses(&account_key),
                            cooldown_sec
                        ),
                    });
                }
            }
        }
        // #173: the daily budget freezes entries the moment its cap is reached —
        // reported here (not only on the next tick) so the freeze and the loss
        // land in the same audit window.
        self.emit_daily_trip();
    }

    // ── Pending-close exit reasons (issue #190) ─────────────────────────────
    //
    // The table remembers which reason an in-flight closing SELL was submitted
    // with, so the fill that completes the close records the right attribution.
    // An entry therefore lives exactly as long as the close it belongs to, and
    // these four helpers are the only way in or out — `exit_reasons` is never
    // touched directly. Before #190 the table was only ever inserted into: an
    // exit that never filled (cancelled, replaced by a settlement, a venue pull)
    // left its reason behind for the NEXT position on that token to inherit.

    /// Record the reason an in-flight close of `token` was submitted with.
    fn note_exit_reason(&mut self, token: &str, reason: ExitReason) {
        if self
            .exit_reasons
            .insert(token.to_string(), reason)
            .is_none()
        {
            self.exit_reason_order.push_back(token.to_string());
        }
        self.enforce_exit_reason_bound();
    }

    /// Consume the pending reason for `token`: the close that fills uses it, and
    /// it must not outlive that close.
    fn take_exit_reason(&mut self, token: &str) -> Option<ExitReason> {
        let reason = self.exit_reasons.remove(token);
        if reason.is_some() {
            self.exit_reason_order.retain(|t| t != token);
        }
        reason
    }

    /// Forget the pending reason for `token` without consuming it — the close
    /// that landed carried its own reason (a settlement, a reconciled venue
    /// close), so the in-flight one is stale either way.
    fn clear_exit_reason(&mut self, token: &str) {
        if self.exit_reasons.remove(token).is_some() {
            self.exit_reason_order.retain(|t| t != token);
        }
    }

    /// Drop every entry whose token no longer has an open position.
    ///
    /// Run when the round rolls over (`engine.markets` replaces the whole round)
    /// and after a close: that is the point at which a leftover entry can no
    /// longer belong to anything live, and an entry whose position is gone would
    /// otherwise be inherited by the next position on that token.
    fn sweep_exit_reasons(&mut self) {
        if self.exit_reasons.is_empty() {
            return;
        }
        let open: HashSet<String> = self
            .positions
            .open_positions()
            .iter()
            .map(|p| p.token_id.clone())
            .collect();
        let stale: Vec<String> = self
            .exit_reasons
            .keys()
            .filter(|t| !open.contains(*t))
            .cloned()
            .collect();
        for token in stale {
            self.exit_reasons.remove(&token);
            self.exit_reasons_swept += 1;
        }
        self.repair_exit_reason_order();
    }

    /// Keep the table within [`EXIT_REASON_TABLE_MAX`], evicting what is least
    /// likely to still be needed: first the entries whose token has no open
    /// position (stale by construction), then the oldest.
    fn enforce_exit_reason_bound(&mut self) {
        if self.exit_reasons.len() <= EXIT_REASON_TABLE_MAX {
            return;
        }
        let open: HashSet<String> = self
            .positions
            .open_positions()
            .iter()
            .map(|p| p.token_id.clone())
            .collect();
        let ghosts: Vec<String> = self
            .exit_reason_order
            .iter()
            .filter(|t| !open.contains(*t))
            .cloned()
            .collect();
        for token in ghosts {
            if self.exit_reasons.len() <= EXIT_REASON_TABLE_MAX {
                break;
            }
            self.exit_reasons.remove(&token);
            self.exit_reasons_evicted += 1;
        }
        while self.exit_reasons.len() > EXIT_REASON_TABLE_MAX {
            let Some(oldest) = self.exit_reason_order.pop_front() else {
                break;
            };
            if self.exit_reasons.remove(&oldest).is_some() {
                self.exit_reasons_evicted += 1;
            }
        }
        self.repair_exit_reason_order();
    }

    /// Drop order entries whose reason is no longer in the map, so the deque
    /// always describes exactly the map's key set (the invariant the eviction
    /// path relies on to name the oldest entry).
    fn repair_exit_reason_order(&mut self) {
        self.exit_reason_order
            .retain(|t| self.exit_reasons.contains_key(t));
        debug_assert_eq!(
            self.exit_reason_order.len(),
            self.exit_reasons.len(),
            "exit_reason_order must mirror exit_reasons"
        );
    }

    /// The pending-close table as the panel reads it (issue #190): how many
    /// reasons are held, the bound, and what the two clean-up paths removed.
    /// `orphaned` counts entries whose position is gone — the ghost this table
    /// used to accumulate; it is 0 at rest.
    fn exit_reasons_view(&self) -> serde_json::Value {
        let open: HashSet<&str> = self
            .positions
            .open_positions()
            .iter()
            .map(|p| p.token_id.as_str())
            .collect();
        let orphaned = self
            .exit_reasons
            .keys()
            .filter(|t| !open.contains(t.as_str()))
            .count();
        serde_json::json!({
            "tracked": self.exit_reasons.len(),
            "capacity": EXIT_REASON_TABLE_MAX,
            "orphaned": orphaned,
            "evictedTotal": self.exit_reasons_evicted,
            "sweptTotal": self.exit_reasons_swept,
        })
    }

    /// The orderbook freshness budget as the panel reads it (issue #205). The
    /// engine's own value wins when an engine is installed (a backtest or a
    /// replay configures its own); without one the configured value is reported,
    /// which is what the next `install_engine` will apply.
    fn orderbook_freshness_view(&self) -> serde_json::Value {
        let max_stale_ms = self
            .engine
            .as_ref()
            .map(|e| e.max_orderbook_stale_ms())
            .unwrap_or(self.config.max_orderbook_stale_ms);
        serde_json::json!({
            "maxStaleMs": max_stale_ms,
            // `maxStaleMs: 0` is not "a zero budget" (which would refuse every
            // book the instant it is stamped): it is the explicit "freshness
            // check OFF". Named here so the panel never renders the two alike.
            "checkEnabled": max_stale_ms > 0,
        })
    }

    /// F4: file a close-accounting credential under a trade key. FIFO-capped —
    /// the FAILED that consumes a credential normally arrives seconds after the
    /// fill that created it, so a small window is all the pairing needs.
    /// #463: the push is journalled so the window survives a restart.
    fn push_close_credential(&mut self, trade_id: String, cred: CloseCredential) {
        if trade_id.is_empty() {
            return;
        }
        while self.close_credentials.len() >= 512 {
            let (evicted, _) = self
                .close_credentials
                .pop_front()
                .expect("len checked above");
            // The journal records the eviction as consumed, so a replay after a
            // restart does not resurrect a credential the cap already dropped.
            if let Some(path) = &self.close_credential_log {
                crate::jsonl::append(path, &CloseCredentialLine::Consumed { trade_id: evicted });
            }
        }
        if let Some(path) = &self.close_credential_log {
            crate::jsonl::append(
                path,
                &CloseCredentialLine::Push {
                    trade_id: trade_id.clone(),
                    cred: Box::new(cred.clone()),
                },
            );
        }
        self.close_credentials.push_back((trade_id, cred));
    }

    /// F4: a FAILED status arrived for a trade whose sell fill had already
    /// fully closed a position and booked the close. Reverse every ledger that
    /// close touched, in the order the close applied them:
    ///   1. position inventory — restore the exact open row (shares, basis,
    ///      flows) and hand back the daily PnL the close added;
    ///   2. TradeDb — retract the record (JSONL line dropped, summary rebuilt);
    ///   3. per-strategy accounting — counts, fees and net PnL back out;
    ///   4. consecutive-loss breaker — remove the loss / restore the streak
    ///      the win had reset, lifting a halt this close caused.
    ///
    /// Cash needs nothing here: `apply_delta_effects` already refunded it
    /// before projecting this rollback.
    fn unwind_closed_trade(&mut self, cred: CloseCredential, d: &FillDelta, now_ms: i64) {
        let closed = &cred.closed;
        let restored = self.positions.restore_closed(cred.restored, closed);
        if restored {
            self.persist_positions();
        }
        if let Some(db) = self.trade_db.as_mut() {
            let rec = crate::trade_db::TradeRecord::from_closed(closed);
            db.retract(&rec, now_ms);
        }
        {
            let entry_fee =
                (closed.entry_fee_pct / Decimal::ONE_HUNDRED) * closed.entry_price * closed.shares;
            let exit_fee =
                (closed.exit_fee_pct / Decimal::ONE_HUNDRED) * closed.exit_price * closed.shares;
            let acc = self
                .strategy_accounting
                .entry(strategy_acc_key(
                    &closed.strategy,
                    closed.account_id.as_str(),
                ))
                .or_default();
            acc.closed_trades = acc.closed_trades.saturating_sub(1);
            if closed.net_pnl_usd >= Decimal::ZERO {
                acc.wins = acc.wins.saturating_sub(1);
            } else {
                acc.losses = acc.losses.saturating_sub(1);
            }
            acc.fees_usd -= entry_fee + exit_fee;
            acc.net_pnl_usd -= closed.net_pnl_usd;
        }
        self.breaker
            .retract(&closed.strategy, closed.net_pnl_usd, cred.streak_before);
        // E26 (§4.2): the ACCOUNT-level streak unwinds symmetrically — the
        // same retract, against the reserved `__account__:` key with the
        // count captured before the close. A reversed loss drops the account
        // streak (lifting a halt its own threshold caused); a reversed win
        // restores the streak the win had reset.
        self.breaker.retract(
            &crate::risk::limits::account_breaker_key(closed.account_id.as_str()),
            closed.net_pnl_usd,
            cred.account_streak_before,
        );
        tracing::warn!(
            order_id = %d.order_id,
            token = %d.token_id,
            trade = %d.trade_id,
            position = %closed.id,
            inventory_restored = restored,
            net_pnl_reversed = %closed.net_pnl_usd,
            "FAILED trade reversed a full close: position inventory restored, close ledgers unwound"
        );
    }

    /// Map a venue order id to a core order (for user-WS events). Returns the
    /// core id when known.
    pub fn core_id_for_venue(&self, venue_or_core: &str) -> Option<String> {
        self.ome
            .by_venue_or_id(venue_or_core)
            .map(|o| o.order_id.clone())
    }

    /// Live orders accepted locally but not yet submitted to the venue (no
    /// venue id bound). The live bridge submits these each tick.
    pub fn pending_unbound(&self) -> Vec<TrackedOrder> {
        self.ome
            .live_orders()
            .into_iter()
            .filter(|o| o.venue_order_id.is_none())
            .cloned()
            .collect()
    }

    /// [`Self::pending_unbound`] filtered to ONE venue: the multi-plugin
    /// executor drain. The routing truth is the ORDER'S OWN execution venue
    /// (`execution_venue_for`: RFC-661 M1 — the placing account's binding
    /// resolves its OWN (venue, token) dual-key row first, so a shared
    /// token's two accounts each route to their own executor; the account
    /// dimension is the AMBIGUITY DISCRIMINATOR, not an override — a
    /// token-only read refuses on two candidate rows and could not pick)
    /// — an order whose execution venue
    /// resolves to nothing, or to a market with no venue label, is UNTAGGED
    /// and routes to the requested plugin only when it is the host's active
    /// plugin — the single-plugin behaviour, unchanged. With the lookup empty
    /// (engine down, replay corpora) every order is untagged, so each executor
    /// sees all of them exactly as the pre-routing bridge did.
    pub fn pending_unbound_for_venue(&self, venue: &str) -> Vec<TrackedOrder> {
        self.pending_unbound()
            .into_iter()
            .filter(|o| self.execution_venue_of(o.account_id.as_str(), &o.token_id) == venue)
            .collect()
    }

    /// The venue that should execute this order: the placing account's
    /// binding rides through [`Engine::execution_venue_for`] — RFC-661 M1:
    /// the (venue, token) dual-key table holds each venue's OWN row for a
    /// byte-identical token, so the binding's own row names the executor.
    /// The account parameter is the AMBIGUITY DISCRIMINATOR (a shared token
    /// carries one row per venue; a token-only read refuses on two
    /// candidates), not a mirror-era leftover — it stays. `""` when the
    /// engine is down or the token belongs to no market: UNTAGGED, never
    /// "venue empty-string" — callers route
    /// untagged work to the active plugin instead of guessing. Fallbacks to
    /// the plain table lookup when the engine is down (same empty answer,
    /// kept total for the host's drain seam, which cannot go through the
    /// engine handle).
    pub fn execution_venue_of(&self, account: &str, token_id: &str) -> String {
        self.engine
            .as_ref()
            .and_then(|e| e.execution_venue_for(account, token_id))
            .unwrap_or_default()
    }

    /// The venue a token belongs to under the live round list, `""` when the
    /// engine is down or the token belongs to no labelled market. Empty means
    /// UNTAGGED, never "venue empty-string": callers route untagged work to
    /// the active plugin instead of guessing.
    pub fn venue_of_token(&self, token_id: &str) -> String {
        self.engine
            .as_ref()
            .and_then(|e| e.venue_of(token_id))
            .unwrap_or_default()
    }

    /// Attach the venue id returned by a successful LIVE POST and mark it live.
    pub fn bind_venue(
        &mut self,
        core_id: &str,
        venue_order_id: String,
        now_ms: i64,
    ) -> CoreResult<()> {
        self.ome.bind_venue(core_id, venue_order_id, now_ms)?;
        self.ome.mark_live(core_id, now_ms)?;
        self.emit_order(core_id);
        Ok(())
    }

    /// Run a reconciliation sweep against a venue snapshot. Applies missed fills
    /// and repairs ghost/partial orders, emitting a report event.
    pub fn reconcile(
        &mut self,
        snap: crate::reconcile::VenueSnapshot,
    ) -> CoreResult<crate::reconcile::ReconcileReport> {
        let report = crate::reconcile::reconcile(&mut self.ome, &snap)?;
        // The sweep channel just proved itself alive — a failed-sweep streak
        // only counts failures in a row (E31-b). The per-venue table clears
        // with it (the snapshot does not carry the reporting venue; one
        // successful sweep anywhere is evidence the transport fabric is
        // alive, and a fresh failure just rebuilds its own streak).
        self.consecutive_sweep_failures = 0;
        self.consecutive_sweep_failures_by_venue.clear();
        // 冻结自愈：a successful sweep proves the transport fabric alive, but
        // the snapshot names no venue — so every AUTO-frozen account gets the
        // same evidence, debounced by FREEZE_AUTO_LIFT_MIN_MS (a fresh freeze
        // rides out the flash outage that caused it before the lift fires).
        // Manual freezes are invisible to this loop by construction.
        for account in self.auto_frozen.keys().cloned().collect::<Vec<_>>() {
            if let Some(t) = self.auto_frozen.get(account.as_str())
                && snap.now_ms - t >= FREEZE_AUTO_LIFT_MIN_MS
            {
                self.auto_lift_if_venue_healthy(
                    account.as_str(),
                    "reconcile sweep succeeded",
                    snap.now_ms,
                );
            }
        }
        // Ledger + position effects for any gap fills the OME just applied.
        for gap in report.actions.iter().filter_map(|a| match a {
            crate::reconcile::ReconcileAction::FilledGap { delta, .. } => Some(delta.clone()),
            _ => None,
        }) {
            self.apply_delta_effects(gap, snap.now_ms);
        }
        // Fills the sweep absorbed into an order that had already ended (#178):
        // the size is booked, the terminal status kept, and the operator told.
        for late in report.late_fills.clone() {
            self.emit_late_fill(&late, snap.now_ms);
        }
        // Trades on orders the OME never issued (manual closes made directly
        // on the venue): fold them into the position book so the position
        // cannot haunt the exit rules forever.
        self.apply_external_fills(&report.unknown_fills, snap.now_ms);
        // A terminal status the sweep just stamped (filled-without-gap,
        // vanished→cancelled, ghost→force-closed) must reach orders.jsonl the
        // same way every other status change does — otherwise a restart
        // re-adopts terminal orders as LIVE and force-closes them all over
        // again. emit_order already persists; these mutations bypass it.
        for id in report.actions.iter().filter_map(|a| match a {
            crate::reconcile::ReconcileAction::MarkedFilled { core_order_id }
            | crate::reconcile::ReconcileAction::MarkedCancelled { core_order_id }
            | crate::reconcile::ReconcileAction::MarkedForceClosed { core_order_id } => {
                Some(core_order_id.as_str())
            }
            crate::reconcile::ReconcileAction::FilledGap { .. } => None,
        }) {
            self.persist_order(id);
        }
        // #462: retire place-timeout orders the venue never gave evidence for
        // (see `resolve_place_timeouts`).
        self.resolve_place_timeouts(&snap);
        if !report.actions.is_empty()
            || !report.suspect_ghost_ids.is_empty()
            || !report.unknown_fills.is_empty()
        {
            let filled = report
                .actions
                .iter()
                .filter(|a| matches!(a, crate::reconcile::ReconcileAction::FilledGap { .. }))
                .count();
            let marked_filled = report
                .actions
                .iter()
                .filter(|a| matches!(a, crate::reconcile::ReconcileAction::MarkedFilled { .. }))
                .count();
            let marked_cancelled = report
                .actions
                .iter()
                .filter(|a| matches!(a, crate::reconcile::ReconcileAction::MarkedCancelled { .. }))
                .count();
            let marked_force_closed = report
                .actions
                .iter()
                .filter(|a| {
                    matches!(
                        a,
                        crate::reconcile::ReconcileAction::MarkedForceClosed { .. }
                    )
                })
                .count();
            // #184: a sweep that had to REPAIR something is a drift report — the
            // local book disagreed with the venue and the safety net caught it.
            // The net working is good news; the divergence is not, and an
            // operator has to be able to see it in the run log without reading
            // the panel. Only repairs and ghost suspicions are errors: external
            // fills alone are a manual close made on the venue (informational).
            if !report.actions.is_empty() || !report.suspect_ghost_ids.is_empty() {
                tracing::error!(
                    filled_gaps = filled,
                    marked_filled,
                    marked_cancelled,
                    marked_force_closed,
                    ghosts = ?report.suspect_ghost_ids,
                    "reconciliation drift repaired: the venue disagreed with the core's book"
                );
            } else {
                tracing::info!(
                    external_fills = report.unknown_fills.len(),
                    "reconciliation: external (manual) fills folded into the position book"
                );
            }
            self.emit(Event::ReconcileReport {
                filled,
                marked_filled,
                marked_cancelled,
                marked_force_closed,
                ghost_ids: report.suspect_ghost_ids.clone(),
            });
        }
        Ok(report)
    }

    /// Fold external fills (venue trades on orders the OME does not track) into
    /// the position book. Only SELLs are applied: a manual close reduces the
    /// matching position by token id, settling the ledger for the cash that
    /// actually moved, and closes the position outright when it empties.
    /// Manual BUYS are skipped — cash truth comes from the periodic
    /// `venue_free_balance` realign, and inventing an unmanaged position here
    /// would fight the engine. Dedup is a persisted watermark: only fills
    /// strictly newer than the last applied one count, so a restart cannot
    /// re-apply what the sweep still reports.
    ///
    /// F2: the venue adapter only forwards legs it proved belong to our
    /// funder, and this loop stays conservative anyway — an unknown SELL is
    /// never booked beyond the inventory the token's position can actually
    /// attribute: size AND proceeds are truncated to what is held, so a
    /// mis-attributed over-sell cannot mint cash.
    fn apply_external_fills(&mut self, fills: &[crate::reconcile::VenueTrade], now_ms: i64) {
        if fills.is_empty() {
            return;
        }
        let fresh: Vec<&crate::reconcile::VenueTrade> = fills
            .iter()
            .filter(|t| t.ts_ms > self.recon_watermark_ms)
            .collect();
        if fresh.is_empty() {
            return;
        }
        let mut applied = 0usize;
        for t in &fresh {
            if t.side != Side::Sell {
                continue;
            }
            let Some((position_id, held, position_account)) = self
                .positions
                .open_positions()
                .iter()
                .find(|p| p.token_id == t.token_id)
                .map(|p| (p.id.clone(), p.shares, p.account_id.clone()))
            else {
                continue;
            };
            // F2: clamp the size to attributable inventory and price the cash
            // on the CLAMPED size — truncating the size while keeping the full
            // notional would still book the counterparty's money.
            let size = t.size.min(held);
            if size <= Decimal::ZERO {
                continue;
            }
            let notional = t.price * size;
            let fee_usd = if t.maker == Some(true) {
                Decimal::ZERO
            } else {
                // The fee follows the schedule in force, under the same
                // replay-only guard the fill choke point uses (#234, item 4).
                (self.live_taker_fee_pct(t.price) / Decimal::ONE_HUNDRED) * notional
            };
            // E28: a manual (external) close settles into the POSITION'S OWN
            // book — the proceeds belong to the account that paid for the
            // shares. An unknown account is logged and skipped (no implicit
            // book, no silent mint).
            if let Ok(l) = self.accounts.get_mut(&position_account) {
                l.settle_sell_fill(notional, fee_usd);
            } else {
                tracing::error!(
                    token = %t.token_id,
                    account = %position_account.as_str(),
                    "external sell for an unknown account dropped — proceeds not booked"
                );
                continue;
            }
            let role = if t.maker == Some(true) {
                crate::model::OrderRole::Maker
            } else {
                crate::model::OrderRole::Taker
            };
            let remaining = self
                .positions
                .apply_exit_fill(&position_id, size, t.price, fee_usd, role)
                .unwrap_or(Decimal::ZERO);
            if remaining == Decimal::ZERO {
                // F10: a manual full close must hit the SAME close ledger the
                // engine's own fills do — TradeDb, strategy PnL, win/loss and
                // the consecutive-loss breaker. `close()` removes the position
                // from the book, so re-reporting the same trade cannot close
                // it twice: that removal IS the idempotence guard.
                if let Some(closed) = self.positions.close(
                    &position_id,
                    t.price,
                    ExitReason::Manual,
                    t.maker == Some(true),
                    now_ms,
                ) {
                    self.persist_positions();
                    self.on_position_closed(&closed, now_ms);
                }
                // Belt and braces: this path can close without the OME ever
                // issuing an order, and the pending-close reason must not
                // outlive the position it belonged to (issue #190) — the
                // reason is retired here even if no closed record came back.
                self.clear_exit_reason(&t.token_id);
            }
            self.persist_positions();
            applied += 1;
            tracing::info!(
                token = %t.token_id,
                requested_size = %t.size,
                applied_size = %size,
                price = %t.price,
                remaining = %remaining,
                "external close reconciled (manual venue sell folded into the position book)"
            );
        }
        // #462: fold unknown BUYs into the position book — a place-timeout
        // order the venue really filled arrives here (the OME never saw the
        // order id), and skipping it left real shares unmanaged while the
        // cash was already gone. Rebuilt through `apply_delta_effects`, the
        // ONE ledger+position+event path every other fill takes, so the
        // reconstructed position feeds exits/settlement exactly like an
        // engine-placed one. Metadata (strategy/condition/slot/direction)
        // comes from the engine's market registry for the token; a token no
        // live market names stays skipped (no inventable metadata), and the
        // default account takes the position (the ABI carries no account on
        // venue trades — the same default the SELL fold settles into).
        for t in fresh.iter().filter(|t| t.side == Side::Buy) {
            let Some(market) = self.engine.as_ref().and_then(|e| {
                e.scanner()
                    .markets()
                    .iter()
                    .find(|m| m.up_token_id == t.token_id || m.down_token_id == t.token_id)
            }) else {
                continue;
            };
            let direction = if market.up_token_id == t.token_id {
                "up"
            } else {
                "down"
            };
            let role = if t.maker == Some(true) {
                crate::model::OrderRole::Maker
            } else {
                crate::model::OrderRole::Taker
            };
            let d = crate::ome::FillDelta {
                order_id: format!("recon:{}", t.venue_order_id),
                token_id: t.token_id.clone(),
                account_id: crate::model::default_account_id(),
                side: Side::Buy,
                delta: t.size,
                price: t.price,
                cumulative: t.size,
                strategy: "reconciled".to_string(),
                asset: market.asset.clone(),
                direction: direction.to_string(),
                condition_id: market.condition_id.clone(),
                round_slot: market.round_slot,
                mode: crate::model::FillPolicy::Taker,
                role,
                limit_price: t.price,
                trade_id: t.trade_id.clone(),
            };
            tracing::warn!(
                token = %t.token_id,
                venue_order = %t.venue_order_id,
                size = %t.size,
                price = %t.price,
                "external BUY reconciled — position rebuilt (a timed-out place the venue really filled, #462)"
            );
            self.apply_delta_effects(d, now_ms);
        }
        // Sells with no matching position are expected noise; one line per
        // sweep keeps the log honest. BUYs are folded above (#462).
        let unmatched = fresh.iter().filter(|t| t.side == Side::Sell).count() - applied;
        if unmatched > 0 {
            eprintln!("core: external fills not matched to a position: sells={unmatched}");
        }
        let new_mark = fresh
            .iter()
            .map(|t| t.ts_ms)
            .max()
            .unwrap_or(self.recon_watermark_ms);
        self.recon_watermark_ms = self.recon_watermark_ms.max(new_mark);
        if let Some(db) = self.position_db.as_ref() {
            db.save_watermark(self.recon_watermark_ms);
        }
    }

    /// Retire every order a place TIMEOUT left pending (#462) when the venue
    /// never gave evidence for it.
    ///
    /// `note_place_timeout` deliberately does NOT reject: the venue may have
    /// accepted the POST after the client gave up. But "pending forever"
    /// leaks the cash reservation and strands the intent, and the OME-side
    /// ghost sweep cannot judge these orders — they carry no venue id to
    /// match, so it skips them. The three endings all resolve without
    /// adoption:
    ///   really filled   → the unknown BUY fold in `apply_external_fills`
    ///                     books the position; this retirement then frees
    ///                     the now-stale reservation;
    ///   really resting  → the bridge's orphan sweep cancels the unknown
    ///                     venue id (exposure removed at the venue);
    ///   never received  → nothing at the venue; only this retirement
    ///                     closes the local intent.
    /// The budget clears the bridge's sweep cadence (5 s) plus the place
    /// timeout itself, so a POST that is merely slow is never raced, and
    /// the folded position occupies the per-asset slot in the window
    /// before retirement — a strategy re-attempt cannot double the entry.
    fn resolve_place_timeouts(&mut self, snap: &crate::reconcile::VenueSnapshot) {
        for (id, submitted_at) in self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| o.venue_order_id.is_none() && o.status == OrderStatus::Pending)
            .map(|o| (o.order_id.clone(), o.submitted_at_ms))
            .collect::<Vec<_>>()
        {
            if snap.now_ms - submitted_at < PLACE_TIMEOUT_VERDICT_MS {
                continue;
            }
            if let Err(e) = self.cancel(&id, snap.now_ms) {
                tracing::warn!(order = %id, error = %e, "place-timeout retirement failed");
            } else {
                tracing::warn!(
                    order = %id,
                    waited_ms = snap.now_ms - submitted_at,
                    "place timed out and the venue never acknowledged it — order retired, reservation released (#462)"
                );
            }
        }
    }

    // ── #457: ghost orders die on the clock, not only on the sweep ───────────
    //
    // The reconciliation sweep only closes a ghost when a snapshot proves the
    // venue forgot it. But the dominant ghost on this market needs no proof at
    // all: an order resting on a round that has EXPIRED can never fill — its
    // market is settlement-dead — and if the venue never acks the cancel, the
    // order haunted `orders.list` forever. The kernel now closes those itself:
    //   * an order whose round ended (round_slot N covers [N·dur, (N+1)·dur))
    //     is ForceClosed at the tick that observes the expiry — the kernel
    //     closing an order nobody else closed. In DRY the mark IS the cancel
    //     (nothing external can hold it); in LIVE the venue-side cancel still
    //     rides through `queue_venue_cancel` as evidence-gathering.
    //   * `force_close_order` is the manual verb: same semantics, operator
    //     intent, but REFUSES live orders that could still fill — a manual
    //     force-close is for ghosts, not for dodging a cancel.
    //
    // Replay sched is excluded wholesale: the sweep mutates the order set (a
    // tradesSha256 input) and replayed archives have no ghost-relevant clock —
    // the historical per-order flows own every transition there.
    fn run_expiry_sweep(&mut self, now_ms: i64) {
        if self.replay_sched() {
            return;
        }
        let dur_ms = self.config.round_duration_sec * 1000;
        if dur_ms <= 0 {
            return;
        }
        // B1b: EVERY live order is judged on ITS OWN family clock — the
        // three-state effective duration (own stamp > market declaration >
        // global grid; a `0` family folds to the global grid, exactly the
        // pre-B1b value). Single-family equivalence: a deployment where every
        // market declares the configured grid (or nothing) and every stamp
        // rides the grid resolves eff == cfg here, byte-for-byte the old
        // filter. The expired ids are collected BEFORE the mutation pass (the
        // market lookup reads the engine).
        let mut expired: Vec<(String, i64)> = Vec::new();
        for o in self.ome.live_orders() {
            if o.round_slot <= 0 {
                continue;
            }
            let eff = self.order_eff_round_sec(o);
            if eff > 0 && now_ms >= (o.round_slot + 1) * eff * 1000 {
                expired.push((o.order_id.clone(), eff));
            }
        }
        for (id, eff) in expired {
            // Segment 2 of the wiring template: the lazy idempotent stamp —
            // a `0` family is written once from the resolution the expiry
            // judgment just made; a non-0 stamp is never touched.
            self.ome.backfill_round_duration(&id, eff);
            self.force_close_order(&id, now_ms, "round expired").ok();
        }
        // B1b lazy backfill: positions opened before the family stamp existed
        // (restored legacy snapshots) get their family written the first time
        // the expiry consumer touches them — idempotent
        // (`backfill_round_duration` never overwrites a non-0 stamp), so
        // running this every sweep is free. The families are resolved BEFORE
        // the mutable borrow (the market lookup reads the engine).
        let families: std::collections::HashMap<String, i64> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.round_duration_sec == 0)
            .map(|p| (p.token_id.clone(), self.round_family_of_token(&p.token_id)))
            .collect();
        self.positions
            .backfill_round_families(|tok| families.get(tok).copied().unwrap_or(0));
    }

    /// E-VA companion: the single-leg orphan sweep for ATOMIC pair strategies.
    ///
    /// The atomic batch (E-VA) guarantees pair legs leave the book together —
    /// but only at SUBMISSION. Afterwards the legs live and die on their own:
    /// one leg fills, the partner's resting order is cancelled (manual, the
    /// #457 expiry sweep, a venue reject) and the strategy is left holding a
    /// NAKED leg — exposure the pair economics never priced, earning nothing
    /// and settling at the coin flip. This sweep is the compensation watch:
    /// for every (account, strategy) scope that BOTH declared `atomic_batch`
    /// AND has a governed virtual pot, a position whose partner token has no
    /// sibling position, no live partner BUY order and no in-flight partner
    /// taker past the grace window is the orphan — the sweep first tries to
    /// CHASE the missing cross-venue leg inside the pair's budget (below),
    /// and only when that is refused — the book too dear, a guard or a
    /// placement refusal — alerts once (the existing `RiskAlert` event, no
    /// new wire type) and flattens through the manual exit path (`flatten`,
    /// Taker walk out of the position's own account).
    ///
    /// Scope discipline:
    ///   - zero-config = zero behaviour: a scope with no cap in
    ///     `virtual_limits` is ungoverned and never touched — the replay
    ///     baseline stays byte-identical;
    ///   - live path only (`replay_sched()` skips — the sweep is live-panel
    ///     semantics, the nofast replay loop keeps its own shape);
    ///   - the 5 s grace window covers the serial same-tick leg landing (both
    ///     legs of one batch fill inside one evaluate cycle, and the partner's
    ///     position row can lag the first leg's by that whole pass); a
    ///     constant, not a tunable — the window is a bookkeeping race bound,
    ///     not a policy dial;
    ///   - debounce is structural: a flattened leg leaves the book, so the
    ///     next tick's condition is simply false; a flatten that failed
    ///     because a live SELL already rests for the token is skipped by
    ///     `flatten` itself (no retry storm, no extra ledger).
    ///
    /// Chase compensation (#531, the feasible arm): the missing leg is the
    /// strategy's most recent BUY order on this account for a token that is
    /// NEITHER the naked leg NOR its same-market partner (the cross-venue
    /// counterpart — `partner_token_id` names only the same-market completion
    /// token, and OME history is the only place the dead leg's token, venue
    /// provenance and round identity survive). The budget is the pair's own:
    /// the naked leg's per-share basis (`cost_usd / shares`) plus the chase
    /// price must stay within the $1.00 a UP+DOWN share pair redeems — this
    /// is the same arithmetic `escalation_guard`'s pair-cost cap applies,
    /// but that cap is keyed by `condition_id` and cannot see a cross-venue
    /// counterpart whose legs carry different condition ids, so the sweep
    /// prices the budget ITSELF into the chase limit and hands the guard a
    /// request that cannot trip the cap. A stale or empty book (`None`
    /// walk) and any guard/placement refusal are the INFEASIBLE arm: the
    /// existing alert+flatten fallback below fires unchanged.
    fn run_pair_orphan_sweep(&mut self, now_ms: i64) {
        if self.replay_sched() {
            return;
        }
        /// Same-tick serial landing window for a pair's two legs (see the
        /// scope discipline above). Not a tunable.
        const PAIR_LAND_GRACE_MS: i64 = 5_000;
        /// How long a placed chase has to land before the sweep stops giving
        /// it the benefit of the doubt and falls back to alert+flatten. A
        /// dry taker fills inside the same `place` call (zero latency dials)
        /// so the window is really only a LIVE-path patience bound and a
        /// debounce: while it runs, the orphan condition below keeps being
        /// true (the partner still has no position/order) and without the
        /// window the sweep would flatten the leg out from under its own
        /// chase on the very next tick. A constant, not a tunable — the
        /// flatten fallback stays the final answer either way.
        const PAIR_CHASE_WINDOW_MS: i64 = 2_000;
        // The governed atomic scopes: every (account, strategy) pair the pot
        // book has a cap for — the strategy's `atomic_batch` declaration is
        // implied by the pot existing (`virtual_limits` is only wired to
        // atomic strategies in practice; a governed scope for a non-atomic
        // strategy is a config decision we honor the same way, since the
        // orphan condition below is strategy-shape agnostic).
        let governed: Vec<(String, String)> = self.virtual_accounts.scopes();
        for (account_id, strategy) in governed {
            // The live BUY orders this strategy rests on this account, keyed
            // by token: a partner with a still-breathing entry order is not
            // orphaned (its leg is arriving), and the position side of the
            // SAME order set covers the inverse asymmetry (first leg still
            // resting + second leg already held).
            let live_partner_buys: std::collections::HashSet<String> = self
                .ome
                .live_orders()
                .into_iter()
                .filter(|o| {
                    o.side == Side::Buy
                        && o.strategy == strategy
                        && o.account_id.as_str() == account_id.as_str()
                })
                .map(|o| o.token_id.clone())
                .collect();
            let inflight_tokens: std::collections::HashSet<String> = self
                .inflight_takers
                .iter()
                .filter_map(|leg| self.ome.get(&leg.order_id))
                .filter(|o| {
                    o.side == Side::Buy
                        && o.strategy == strategy
                        && o.account_id.as_str() == account_id.as_str()
                })
                .map(|o| o.token_id.clone())
                .collect();
            let orphans: Vec<(String, String, String, String, Decimal, Decimal)> = self
                .positions
                .open_positions()
                .iter()
                .filter(|p| {
                    p.strategy == strategy
                        && p.account_id.as_str() == account_id
                        && !p.partner_token_id.is_empty()
                        // Settlement-dead positions belong to the settlement
                        // ladder, not the unwind desk: a hold-to-settlement
                        // pair rides PAST its round end by design (the same
                        // live-only rule `can_open` and the systemic facts
                        // count by). A naked leg is only unwound at market
                        // prices while its market is still live.
                        && p.expires_at_ms > now_ms
                        && now_ms - p.entered_at_ms >= PAIR_LAND_GRACE_MS
                        && !self.positions.open_positions().iter().any(|sib| {
                            sib.token_id == p.partner_token_id
                                && sib.strategy == strategy
                                && sib.account_id.as_str() == account_id
                        })
                        // The #427 cross-venue completion set: the package's
                        // pair lands as OPPOSITE directions on the same asset
                        // in the same round (one leg per venue), while
                        // `partner_token_id` only ever names the SAME-market
                        // completion token — without this clause a fully
                        // landed pair would read as two orphans and be
                        // flattened whole, destroying the locked profit the
                        // watch exists to protect. An opposite-direction
                        // sibling in the same scope and round window (the
                        // shared `expires_at_ms` is the round identity the
                        // position carries) is the market-neutral set the
                        // strategy built; it is not naked.
                        && !self.positions.open_positions().iter().any(|sib| {
                            sib.strategy == strategy
                                && sib.account_id.as_str() == account_id
                                && sib.asset == p.asset
                                && sib.expires_at_ms == p.expires_at_ms
                                && sib.direction != p.direction
                        })
                        && !live_partner_buys.contains(&p.partner_token_id)
                        && !inflight_tokens.contains(&p.partner_token_id)
                })
                .map(|p| {
                    (
                        p.id.clone(),
                        p.token_id.clone(),
                        p.venue.clone(),
                        p.partner_token_id.clone(),
                        // The chase budget's paid half: the naked leg's
                        // per-share basis. The entry notional spent per
                        // share is what the missing leg's price must fit
                        // under ($1.00 a pair redeems).
                        p.cost_usd / p.shares,
                        p.shares,
                    )
                })
                .collect();
            for (id, token, venue, partner, basis, shares) in orphans {
                // ── Chase arm (#531 feasible): find the dead cross-venue leg
                // in OME history and re-offer it within the pair budget. The
                // infeasible arm keeps the historical alert+flatten shape
                // byte-for-byte.
                let account = AccountId::from(account_id.as_str());
                let chase = self.pair_chase_target(&account, &strategy, &token, &partner);
                let chased = match chase
                    .map(|t| {
                        // The budget cap the chase may state: the naked leg's
                        // basis is the paid half; the missing leg buys at the
                        // counterparty and the PAIR must stay inside $1.00.
                        (t, Decimal::ONE - basis)
                    })
                    .filter(|(_, cap)| *cap > Decimal::ZERO)
                {
                    Some((target, cap)) => self.pair_chase_missing_leg(
                        &target,
                        (&token, &account, &strategy),
                        shares,
                        cap,
                        now_ms,
                    ),
                    None => false,
                };
                if chased {
                    // The chase is out (a filled position or a resting BUY).
                    // The naked leg is not orphaned any more — the next tick's
                    // condition either reads the partner side (filled) or the
                    // live-BUY clause (resting), and the chase window above
                    // keeps this tick's re-judgment off the leg entirely.
                    continue;
                }
                // Chase window debounce: a chase this scope placed within the
                // window is still adjudicating (a live taker, a resting leg).
                // Skip WITHOUT alerting or flattening — the window is the
                // chase's benefit of the doubt; after it, the fallback below
                // fires.
                if let Some((_, at)) = self
                    .pair_chase_attempts
                    .get(&(account_id.clone(), strategy.clone(), token.clone()))
                    .cloned()
                    && now_ms - at < PAIR_CHASE_WINDOW_MS
                {
                    continue;
                }
                let message = format!(
                    "pair leg orphaned: strategy={strategy} account={account_id} \
                     held={token}@{venue} partner_missing={partner} — \
                     flattening the naked leg"
                );
                tracing::warn!(position = %id, token = %token, partner = %partner,
                    "pair leg orphaned — flattening the naked leg");
                self.emit(Event::RiskAlert {
                    code: CoreErrorCode::RiskRejected,
                    message,
                });
                // The manual exit path: SELL Taker walk out of the position's
                // own account (E28 §9.2), ExitReason::Manual — no new enum
                // value, the audit line above carries the real cause. A
                // refusal (a live SELL already resting, a halted breaker) is
                // the sweep's own skip posture: the position stays in the
                // book and the next tick re-judges it, so the result is
                // honestly dropped rather than swallowed.
                let _ = self.flatten(Some(&id), now_ms);
                // RFC 568 阶段一（观察者写点）：flatten 已处置 ⇒ 配对落
                // Closed 终态（判定链零改动 —— sweep 的 orphan 判定照旧直读
                // OME 与持仓，这里只落账）。寻址按 token 形（close_by_token）：
                // 经济键第二元是实例主账户，sweep 手里只有腿的落账账户
                // （v2 A≠B 时两者不同），token 唯一对应本轮一个市场，绕开
                // 这层不对称；幂等（同 token 跨轮旧键也一并闭账）。
                self.pair_book.close_by_token(&strategy, &token);
                // The chase did not save the leg: retire its attempt record so
                // the next tick can offer a fresh chase (a repriced book may
                // have made the pair feasible again) instead of waiting out a
                // stale window.
                self.pair_chase_attempts.remove(&(
                    account_id.clone(),
                    strategy.clone(),
                    token.clone(),
                ));
            }
        }
    }

    /// The chase arm's TARGET: the missing cross-venue leg's token, read out
    /// of the OME's order history. It is this (account, strategy) scope's
    /// most recent BUY for a token that is neither the naked leg nor the
    /// naked leg's same-market partner (`partner_token_id` names only the
    /// same-market completion token; the cross-venue counterpart's token is
    /// nowhere on the position row — the dead ORDER is the only evidence of
    /// which token the pair attempt was completing with). `None` = no
    /// missing-leg order ever existed (the entry died before resting — the
    /// starved-ledger shape) → the sweep goes straight to alert+flatten.
    fn pair_chase_target(
        &self,
        account_id: &AccountId,
        strategy: &str,
        naked_token: &str,
        partner: &str,
    ) -> Option<String> {
        // The most recent attempt is the leg the strategy was completing:
        // `by_submitted` is the insertion order, so `.all()` is oldest first
        // — `.rfind()` reads the newest.
        self.ome
            .all()
            .into_iter()
            .filter(|o| {
                o.side == Side::Buy
                    && o.token_id != naked_token
                    && o.token_id != partner
                    && o.strategy == strategy
                    && o.account_id == *account_id
            })
            .rfind(|_| true)
            .map(|o| o.token_id.clone())
    }

    /// Re-offer the missing leg: price it off the CURRENT book (worst fill
    /// price the walk needs, `cap` = the pair budget's chase half), state the
    /// full naked size as a dry-FOK Taker, run the same guard every
    /// escalation runs, and place through `place_escalated` (entry gates off —
    /// this COMPLETES the naked position's pair, exactly the gap-fill case
    /// the one-per-asset gate would otherwise refuse). `Ok(true)` = the leg
    /// is out (filled into a position or resting live); `Ok(false)` or any
    /// error = infeasible, the caller's alert+flatten fallback proceeds.
    ///
    /// The internal key carries the slot so a NEXT round's orphan gets a
    /// fresh key (the OME refuses a duplicate live intent key): the round
    /// identity comes from the historical order, the natural fingerprint of
    /// "this pair attempt".
    /// The naked leg's identity, borrowed: (token, account, strategy) — the
    /// one tuple every chase bookkeeping (request provenance, attempt key,
    /// tracing) reads.
    fn pair_chase_missing_leg(
        &mut self,
        missing_token: &str,
        naked: (&str, &AccountId, &str),
        size: Decimal,
        cap: Decimal,
        now_ms: i64,
    ) -> bool {
        let (naked_token, account_id, strategy) = naked;
        let Some((vwap, worst)) = self.marketable_walk(Side::Buy, missing_token, size, cap, now_ms)
        else {
            // The book cannot cover the naked size within the pair budget (or
            // is stale/absent) — the infeasible arm.
            return false;
        };
        // B1b: the chase completes the NAKED leg's pair attempt, so it runs on
        // the naked leg's family clock, not the configured grid — the missing
        // leg is sized/timed against the same round the pair was admitted
        // under. The naked POSITION carries the family (stamped at open,
        // backfilled at the expiry consumer); a `0` stamp folds to the global
        // grid, the pre-B1b slot fallback byte-for-byte.
        let naked_family = self
            .positions
            .open_positions()
            .iter()
            .find(|p| {
                p.token_id == naked_token
                    && p.account_id.as_str() == account_id.as_str()
                    && p.strategy == strategy
            })
            .map(|p| self.position_eff_round_sec(p))
            .filter(|eff| *eff > 0);
        let slot = self.books_round_slot(missing_token).unwrap_or_else(|| {
            let fam = naked_family
                .unwrap_or(self.config.round_duration_sec)
                .max(1);
            now_ms / (fam * 1000).max(1)
        });
        let internal_key = format!("pair-chase:{naked_token}:{missing_token}:{slot}");
        let req = OrderRequest {
            token_id: missing_token.to_string(),
            // The guard's pair-cost cap keys on condition_id and the
            // counterpart legs carry DIFFERENT condition ids (that is why the
            // sweep prices the budget itself); the request carries the naked
            // leg's identity so the accounting and events name the pair
            // attempt, not a fresh condition.
            condition_id: naked_token.to_string(),
            side: Side::Buy,
            mode: FillPolicy::Taker,
            price: worst,
            size,
            internal_key: internal_key.clone(),
            strategy: strategy.to_string(),
            asset: self
                .books_asset(missing_token)
                .unwrap_or_else(|| "BTC".to_string()),
            direction: self
                .books_direction(missing_token)
                .unwrap_or_else(|| "up".to_string()),
            round_slot: slot,
            account_id: account_id.clone(),
        };
        // One attempt per window: stamp BEFORE placing — if the place dies,
        // the stamp is what keeps this tick's fallback and the window's
        // re-judgment from double-firing.
        self.pair_chase_attempts.insert(
            (
                account_id.as_str().to_string(),
                strategy.to_string(),
                naked_token.to_string(),
            ),
            (missing_token.to_string(), now_ms),
        );
        if let Err(reason) = self.escalation_guard(&req, vwap) {
            tracing::info!(
                strategy = %strategy,
                account = %account_id.as_str(),
                token = %missing_token,
                naked = %naked_token,
                price = %worst,
                cap = %cap,
                reason = %reason,
                "pair chase refused by the guard — falling back to alert+flatten"
            );
            return false;
        }
        match self.place_escalated(req, now_ms) {
            Ok((order_id, status)) => {
                // B1b: the chase order inherits the naked leg's family — the
                // same lazy idempotent stamp the placement path's market
                // lookup would have resolved, but the PAIR attempt's family
                // is authoritative (the missing leg's own market may declare
                // a different family than the round the pair was priced
                // under). A non-0 stamp is never overwritten.
                if let Some(fam) = naked_family {
                    self.ome.backfill_round_duration(&order_id, fam);
                }
                tracing::info!(
                    order = %order_id,
                    ?status,
                    strategy = %strategy,
                    account = %account_id.as_str(),
                    token = %missing_token,
                    naked = %naked_token,
                    price = %worst,
                    size = %size,
                    "pair chase placed — the missing leg is re-offered past the counterparty"
                );
                // RFC 568 阶段一（观察者写点）：chase 成功 ⇒ 把 chase 订单的
                // internal_key 绑到配对簿的缺失腿（naked token 的配对腿）——
                // chase 的成交/死亡事实经同一 on_order_event 通道更新配对状
                // 态（Partial → Complete 或 Orphan）。判定链零改动：绑回是
                // 落账，不是输入。
                let key = self
                    .ome
                    .get(&order_id)
                    .map(|o| o.internal_key.clone())
                    .unwrap_or_else(|| format!("pair-chase:{naked_token}:{missing_token}:{slot}"));
                self.pair_book
                    .bind_by_leg(strategy, account_id.as_str(), missing_token, &key);
                true
            }
            Err(e) => {
                tracing::info!(
                    strategy = %strategy,
                    account = %account_id.as_str(),
                    token = %missing_token,
                    naked = %naked_token,
                    reason = %e.message,
                    "pair chase placement failed — falling back to alert+flatten"
                );
                false
            }
        }
    }

    /// #457: close a STILL-LIVE order the kernel judges settlement-dead —
    /// mark `ForceClosed`, release its reservation, and (live mode) queue a
    /// venue cancel so the real venue state converges with the book. Refuses
    /// an order that could still fill: the manual verb is for ghosts, and a
    /// caller that wants to withdraw a live order has `cancel`.
    pub fn force_close_order(&mut self, id: &str, now_ms: i64, reason: &str) -> CoreResult<()> {
        let order = self
            .ome
            .get(id)
            .cloned()
            .ok_or_else(|| CoreError::new(CoreErrorCode::UnknownOrder, id.to_string()))?;
        if !order.status.is_live() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidOrderState,
                format!("order {id} is already terminal ({:?})", order.status),
            ));
        }
        if order.round_slot > 0 {
            // B1b: the round's liveness is judged on the ORDER'S OWN family
            // clock — the three-state eff (own stamp > market declaration >
            // global grid), not the configured grid: a 3600s order must stay
            // live for its full hour even while the core runs 900s rounds.
            // A `0` family folds to the global grid — the pre-B1b value —
            // and the resolution just made is the lazy idempotent stamp.
            let eff = self.order_eff_round_sec(&order);
            let dur_ms = eff * 1000;
            let expires_at = (order.round_slot + 1) * dur_ms;
            self.ome.backfill_round_duration(id, eff);
            if now_ms < expires_at {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidOrderState,
                    format!(
                        "order {id}'s round is still live until {expires_at} — use cancel, not force-close"
                    ),
                ));
            }
        }
        let was_live = order.status.is_live();
        if order.side == Side::Buy
            && let Ok(l) = self.accounts.get_mut(&order.account_id)
        {
            l.release(id);
        }
        // E-VA: expiry/force-close releases the virtual hold too.
        self.virtual_release_order(&order);
        self.ome
            .mark_terminal(id, OrderStatus::ForceClosed, now_ms)?;
        // RFC 568 阶段一：死亡事实（过期强平）喂配对簿。
        self.pair_on_order_event(&order.internal_key, Decimal::ZERO, true);
        if let Some(engine) = self.engine.as_mut() {
            engine.release_pending_token_for_strategy(
                order.account_id.as_str(),
                &order.strategy,
                &order.token_id,
            );
        }
        if self.replay_sched() {
            self.fast_orders_dirty = true;
        }
        if was_live && let Some(vid) = order.venue_order_id {
            let venue = self.execution_venue_of(order.account_id.as_str(), &order.token_id);
            self.queue_venue_cancel(venue, vid);
        }
        tracing::warn!(
            order = %id,
            strategy = %order.strategy,
            token = %order.token_id,
            reason = %reason,
            "order force-closed by the kernel — round expired and no honest terminal receipt arrived (#457)"
        );
        // The audit trail's whole point: "we withdrew" (Cancelled) and "we gave
        // up waiting" (ForceClosed) must stay readable apart on disk.
        self.persist_audit_value(serde_json::json!({
            "kind": "force_close",
            "ts_ms": now_ms,
            "order_id": id,
            "strategy": order.strategy,
            "token_id": order.token_id,
            "round_slot": order.round_slot,
            "reason": reason,
        }));
        self.emit_order(id);
        Ok(())
    }

    fn emit(&self, ev: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev);
        }
    }
    fn emit_order(&self, id: &str) {
        if let Some(o) = self.ome.get(id) {
            // Durable persistence: every status change flows through here (submit /
            // confirm / reject / bind / cancel), so the order log always reflects
            // the latest state and a restart can rebuild it (orphan guard).
            self.persist_order(id);
            self.emit(Event::OrderUpdate { order: o.clone() });
        }
    }
    fn emit_fill(&self, d: FillDelta) {
        if let Some(o) = self.ome.get(&d.order_id) {
            self.persist_order(&d.order_id);
            self.emit(Event::Fill {
                delta: d.into(),
                order: o.clone(),
            });
        }
    }

    fn new_order_id(&mut self) -> String {
        // "live" is reserved for orders that can actually reach a venue. A
        // read-only order is not live — stamping it `live_` would put a
        // misleading label on the one field an operator reads to decide what is
        // real, so it is named for what it is.
        let prefix = match self.config.mode {
            Mode::Live => "live",
            Mode::ReadOnly => "readonly",
            Mode::Dry => "dry",
        };
        let id = format!("{prefix}_{}", self.next_id);
        self.next_id += 1;
        id
    }

    // ── Risk ───────────────────────────────────────────────────────────────
    pub fn kill(&mut self, reason: String) {
        self.risk.kill(reason.clone());
        self.emit(Event::RiskAlert {
            code: CoreErrorCode::KillSwitchActive,
            message: reason,
        });
    }
    /// Flush pending near-miss records to disk (call on shutdown).
    pub fn flush_near_misses(&mut self) {
        if let Some(path) = self.config.near_miss_path.clone() {
            let recs = self
                .engine
                .as_mut()
                .map(|e| e.flush_near_misses())
                .unwrap_or_default();
            if !recs.is_empty()
                && let Err(e) =
                    crate::shadow::persist_near_misses(std::path::Path::new(&path), &recs)
            {
                tracing::warn!(error = %e, path = %path, "near-miss flush failed");
            }
        }
    }

    pub fn resume(&mut self) {
        self.risk.resume();
    }
    pub fn is_killed(&self) -> bool {
        self.risk.is_killed()
    }

    /// Hot-swap the live risk gate configuration without a restart (test and
    /// runtime-config path; `place()` reads it on every check).
    pub fn risk_config_mut(&mut self) -> &mut RiskConfig {
        self.risk.config_mut()
    }

    /// Test-only: re-arm the virtual-account book after construction (the
    /// production surface is `CoreConfig::virtual_limits` at boot; tests
    /// build the core first and opt a strategy in afterwards).
    #[cfg(test)]
    pub(crate) fn set_virtual_limits_for_test(
        &mut self,
        limits: HashMap<String, VirtualAccountLimit>,
    ) {
        self.virtual_accounts = crate::risk::virtual_account::VirtualAccounts::new(&limits);
    }

    /// The live risk gate config — the read side of
    /// [`Self::risk_config_mut`]: a `risk.setLimits`/`risk.setSystemic` write
    /// is visible here immediately, which is what keeps the `risk.limits`
    /// readout honest after a hot edit.
    pub fn risk_config(&self) -> &RiskConfig {
        self.risk.config()
    }

    /// Apply a hot update of the ENTRY limits (#191) — the whole safe subset
    /// ([`crate::risk::HOT_RELOADABLE`]), in memory, with an audit record.
    ///
    /// What makes this safe enough to offer at runtime is what the whitelist
    /// selects for: the order path reads every one of these LIVE (the risk gate
    /// on each check, the engine's own config on each ticket), they bound NEW
    /// exposure only — a close is exempt by construction (#174), so tightening a
    /// limit can never trap an open position — and none of them carries
    /// accumulated state, so a change cannot retroactively decide something the
    /// day's book of events already decided.
    ///
    /// Atomic: the patch is validated in full (non-negative, coherent band)
    /// before anything is written, so a refused patch leaves NO trace. Nothing is
    /// persisted — a restart re-applies the startup flags (#191: "hot" must never
    /// be read as "permanent"), and the audit says so in the same breath.
    ///
    /// The audit is one `tracing::info!` line per changed field (`target: "risk"`,
    /// old → new + actor + instant) in the run log every deployment already
    /// writes; the same facts are returned to the caller, so the reply and the log
    /// cannot tell two different stories.
    pub fn apply_risk_limits(
        &mut self,
        patch: &crate::ipc::schema::SetRiskLimitsParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::ipc::schema::RiskLimitUpdate> {
        use crate::ipc::schema::{RISK_LIMIT_UPDATE_NOTE, RiskLimitChange, RiskLimitUpdate};
        use crate::risk::HOT_RELOADABLE;

        if patch.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "risk.setLimits needs at least one field; hot-reloadable: {}",
                    HOT_RELOADABLE.join(", ")
                ),
            ));
        }

        // Plan first, write later: a bad field must not leave a half-applied
        // patch behind.
        let risk = self.risk.config();
        let (notional, notional_pct, open_notional) = (
            risk.max_order_notional,
            risk.max_order_notional_pct,
            risk.max_open_notional_usd,
        );
        let (min_shares, max_shares) = (self.config.min_shares, self.config.max_shares);
        let mut changes: Vec<RiskLimitChange> = Vec::new();
        let mut plan =
            |field: &'static str, from: Decimal, to: Option<Decimal>| -> CoreResult<()> {
                let Some(to) = to else {
                    return Ok(());
                };
                if to < Decimal::ZERO {
                    return Err(CoreError::new(
                        CoreErrorCode::InvalidParams,
                        format!("{field} must be >= 0, got {to}; nothing was applied"),
                    ));
                }
                changes.push(RiskLimitChange { field, from, to });
                Ok(())
            };
        plan("maxOrderNotional", notional, patch.max_order_notional)?;
        plan(
            "maxOrderNotionalPct",
            notional_pct,
            patch.max_order_notional_pct,
        )?;
        plan(
            "maxOpenNotionalUsd",
            open_notional,
            patch.max_open_notional_usd,
        )?;
        plan("minShares", min_shares, patch.min_shares)?;
        plan("maxShares", max_shares, patch.max_shares)?;

        // The band is a band: `effective_sizing` caps the floor by the ceiling, so
        // an incoherent pair would silently read as something the operator did not
        // ask for. Refuse it instead.
        let (new_min, new_max) = (
            patch.min_shares.unwrap_or(min_shares),
            patch.max_shares.unwrap_or(max_shares),
        );
        if new_min > new_max {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "minShares {new_min} would exceed maxShares {new_max}; \
                     the share band must stay coherent — nothing was applied"
                ),
            ));
        }

        let band_moved = changes
            .iter()
            .any(|c| c.field == "minShares" || c.field == "maxShares");
        for c in &changes {
            match c.field {
                "maxOrderNotional" => self.risk.config_mut().max_order_notional = c.to,
                "maxOrderNotionalPct" => self.risk.config_mut().max_order_notional_pct = c.to,
                "maxOpenNotionalUsd" => self.risk.config_mut().max_open_notional_usd = c.to,
                "minShares" => self.config.min_shares = c.to,
                "maxShares" => self.config.max_shares = c.to,
                // Unreachable today (both lists live ten lines apart), and loud
                // rather than silent if that ever stops being true: an arm that
                // matched nothing would leave the reply and the audit claiming a
                // change this function never made.
                other => {
                    debug_assert!(false, "unmapped hot-reloadable field {other}");
                    tracing::error!(
                        target: "risk",
                        field = other,
                        "hot-reloadable field has no write arm; the audit record would be a lie"
                    );
                }
            }
        }
        if band_moved {
            // The engine holds the band the tickets are cut from; the CoreConfig
            // copy above is what a reader with no engine sees.
            if let Some(engine) = self.engine.as_mut() {
                engine.set_share_band(self.config.min_shares, self.config.max_shares);
            }
        }

        for c in &changes {
            tracing::info!(
                target: "risk",
                actor = %actor,
                field = c.field,
                from = %c.from,
                to = %c.to,
                reason = patch.reason.as_deref().unwrap_or(""),
                "risk limit hot-update: {} {} -> {} ({})",
                c.field,
                c.from,
                c.to,
                RISK_LIMIT_UPDATE_NOTE,
            );
        }
        Ok(RiskLimitUpdate {
            applied: changes,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            persisted: false,
            note: RISK_LIMIT_UPDATE_NOTE,
        })
    }

    /// 生效风控可编辑（`risk.setSystemic`）：九项系统性限额的运行时写路径，
    /// 仿 [`Self::apply_risk_limits`] 的 plan-first + 审计姿态。
    ///
    /// 三个生效档位都是真实的，不是文档话术：
    ///
    /// - **live（六项）**—— `max_single_loss_usd` / `max_position_size` /
    ///   `max_total_position` / `max_total_exposure_usd` / `max_correlation_usd`
    ///   / `global_kill_switch_loss_usd`。入场流水线每笔现场
    ///   `systemic_snapshot()` 判读，只约束新敞口（平仓从不走这条路），不携带
    ///   计数器 —— 写进内存后下一笔入场立即按新值裁决。
    /// - **next_session（两项）**—— `max_consecutive_losses` /
    ///   `cooldown_minutes`。连亏计数器已在跑（`record_with` 的 breaker 带着
    ///   出生时的阈值对过一辈子），热改会追溯改判在案连亏 ——
    ///   `HOT_RELOAD_REFUSED` 的理由对这两项仍然成立。读数照写（操作员的
    ///   意图进入 matrix，重启后 `from_file` 的解析与它一致），但本会话的
    ///   熔断行为不变，回执里逐项注明。
    /// - **next_session（一项）**—— `max_daily_drawdown_usd`。它与日亏熔断
    ///   是同一个预算；当日已发生的已实现亏损账本不能被中途改动的帽追溯
    ///   改判。同上：matrix 照写、本会话的 `PositionConfig::max_daily_loss_usd`
    ///   不动。
    ///
    /// Atomic：patch 先全量校验（非负）再落笔，被拒的 patch 不留半点痕迹。
    /// 不持久化 —— 重启后 `from_file` 重走启动层，与 `risk.setLimits` 的
    /// 「热≠永久」同一句真话。
    pub fn apply_systemic_limits(
        &mut self,
        patch: &crate::ipc::schema::SetSystemicLimitsParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::ipc::schema::SystemicLimitUpdate> {
        use crate::ipc::schema::{
            SYSTEMIC_LIMIT_UPDATE_NOTE, SystemicLimitChange, SystemicLimitUpdate,
        };

        if patch.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "risk.setSystemic needs at least one of the nine systemic fields; \
                 read the current values with risk.limits"
                    .to_string(),
            ));
        }

        // Plan first, write later —— 被拒的 patch 一个字节都不动。
        let matrix = self.risk.config().systemic.clone();
        let mut changes: Vec<SystemicLimitChange> = Vec::new();
        let mut plan = |field: &'static str,
                        from: &crate::risk::limits::Bound,
                        to: Option<Decimal>,
                        effect: &'static str|
         -> CoreResult<()> {
            let Some(to) = to else { return Ok(()) };
            if to < Decimal::ZERO {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("{field} must be >= 0, got {to}; nothing was applied"),
                ));
            }
            changes.push(SystemicLimitChange {
                field,
                from: from.value().to_string(),
                to: to.to_string(),
                effect,
            });
            Ok(())
        };

        plan(
            "maxSingleLossUsd",
            &matrix.account.max_single_loss_usd,
            patch.max_single_loss_usd,
            "live",
        )?;
        plan(
            "maxDailyDrawdownUsd",
            &matrix.account.max_daily_drawdown_usd,
            patch.max_daily_drawdown_usd,
            "next_session",
        )?;
        plan(
            "maxPositionSize",
            &matrix.account.max_position_size,
            patch.max_position_size,
            "live",
        )?;
        plan(
            "maxConsecutiveLosses",
            &matrix.account.max_consecutive_losses,
            patch.max_consecutive_losses,
            "next_session",
        )?;
        plan(
            "cooldownMinutes",
            &matrix.account.cooldown_minutes,
            patch.cooldown_minutes,
            "next_session",
        )?;
        plan(
            "maxTotalPosition",
            &matrix.global.max_total_position,
            patch.max_total_position,
            "live",
        )?;
        plan(
            "maxTotalExposureUsd",
            &matrix.global.max_total_exposure_usd,
            patch.max_total_exposure_usd,
            "live",
        )?;
        plan(
            "maxCorrelationUsd",
            &matrix.global.max_correlation_usd,
            patch.max_correlation_usd,
            "live",
        )?;
        plan(
            "globalKillSwitchLossUsd",
            &matrix.global.global_kill_switch_loss_usd,
            patch.global_kill_switch_loss_usd,
            "live",
        )?;

        if changes.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "risk.setSystemic: no field carried a change — nothing to audit",
            ));
        }

        // Write phase: the six live bounds land in the matrix the pipeline
        // snapshots per entry; the three next-session rows also land there
        // (the readout must show the operator's intent and the restart
        // resolution agrees), but nothing re-judges this session — the
        // breaker keeps its birth pair, the day's loss book stays closed.
        {
            let cfg = self.risk.config_mut();
            let matrix = &mut cfg.systemic;
            let set = |bound: &mut crate::risk::limits::Bound,
                       to: Option<Decimal>,
                       source: crate::risk::limits::LimitSource| {
                if let Some(v) = to {
                    *bound = crate::risk::limits::Bound::new(v, source);
                }
            };
            let flag = crate::risk::limits::LimitSource::Flag;
            set(
                &mut matrix.account.max_single_loss_usd,
                patch.max_single_loss_usd,
                flag,
            );
            set(
                &mut matrix.account.max_daily_drawdown_usd,
                patch.max_daily_drawdown_usd,
                flag,
            );
            set(
                &mut matrix.account.max_position_size,
                patch.max_position_size,
                flag,
            );
            set(
                &mut matrix.account.max_consecutive_losses,
                patch.max_consecutive_losses,
                flag,
            );
            set(
                &mut matrix.account.cooldown_minutes,
                patch.cooldown_minutes,
                flag,
            );
            set(
                &mut matrix.global.max_total_position,
                patch.max_total_position,
                flag,
            );
            set(
                &mut matrix.global.max_total_exposure_usd,
                patch.max_total_exposure_usd,
                flag,
            );
            set(
                &mut matrix.global.max_correlation_usd,
                patch.max_correlation_usd,
                flag,
            );
            set(
                &mut matrix.global.global_kill_switch_loss_usd,
                patch.global_kill_switch_loss_usd,
                flag,
            );
        }

        for c in &changes {
            tracing::info!(
                target: "risk",
                actor = %actor,
                field = c.field,
                from = %c.from,
                to = %c.to,
                effect = c.effect,
                reason = patch.reason.as_deref().unwrap_or(""),
                "systemic limit update: {} {} -> {} ({})",
                c.field,
                c.from,
                c.to,
                SYSTEMIC_LIMIT_UPDATE_NOTE,
            );
        }
        Ok(SystemicLimitUpdate {
            applied: changes,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            persisted: false,
            note: SYSTEMIC_LIMIT_UPDATE_NOTE,
        })
    }

    /// 退出纪律可编辑（`risk.setExit`）：Gate 4 绑定的止损/止盈/强平三项的
    /// 运行时写路径，仿 [`Self::apply_systemic_limits`] 的 plan-first + 审计
    /// 姿态。
    ///
    /// 生效语义是真实的，不是文档话术：
    ///
    /// - **每笔新入场 live** —— 仲裁管线（Gate 4）每个意图现场从
    ///   `self.config.positions.exit` 取 [`ExitConfig`]（`process_intent` 的
    ///   调用点），写入后下一笔入场立即按新值绑定退出纪律投影。
    /// - **已开仓位的退出扫描同样 live** —— [`PositionManager`] 的退出梯
    ///   （止损/止盈/强平判读）读自己的 `config.exit` 副本，所以写路径必须
    ///   同步两处：`config.positions.exit`（入场绑定）+ `positions.set_config`
    ///   （退出扫描）。漏掉后者会造出「新仓位一种纪律、旧仓位另一种纪律」
    ///   的静默分裂。
    ///
    /// 校准护栏：`forceExitSec: 0` 是文档化的「关闭强平」开关（出厂校准值），
    /// 照常接受；两个百分比拒绝 `<= 0`（`stopLossPct: 0` 会在入场 tick 就
    /// 止损，`takeProfitPct: 0` 语义同样荒谬 —— 负数/零不是操作员想要的
    /// 「关闭」，是笔误）。强平接受 `0`、拒绝负数。
    ///
    /// 不持久化 —— 重启后回到启动 flag/env/TOML 的解析结果，与
    /// `risk.setSystemic` 的「热≠永久」同一句真话。
    pub fn apply_exit_set(
        &mut self,
        patch: &crate::ipc::schema::SetExitParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::ipc::schema::ExitSetUpdate> {
        use crate::ipc::schema::{EXIT_SET_UPDATE_NOTE, ExitSetChange, ExitSetUpdate};

        if patch.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "risk.setExit needs at least one of stopLossPct / takeProfitPct / \
                 forceExitSec / trailingEnabled; read the current values with risk.limits"
                    .to_string(),
            ));
        }

        // Plan first, write later —— 被拒的 patch 一个字节都不动。
        let current = self.config.positions.exit.clone();
        let mut changes: Vec<ExitSetChange> = Vec::new();
        let mut plan = |field: &'static str,
                        from: Decimal,
                        to: Option<Decimal>,
                        effect: &'static str|
         -> CoreResult<()> {
            let Some(to) = to else { return Ok(()) };
            if to <= Decimal::ZERO {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "{field} must be > 0 (a percentage; there is no `off` \
                             spelling here), got {to}; nothing was applied"
                    ),
                ));
            }
            changes.push(ExitSetChange {
                field,
                from: from.to_string(),
                to: to.to_string(),
                effect,
            });
            Ok(())
        };

        plan(
            "stopLossPct",
            current.stop_loss_pct,
            patch.stop_loss_pct,
            "live",
        )?;
        plan(
            "takeProfitPct",
            current.take_profit_pct,
            patch.take_profit_pct,
            "live",
        )?;

        if let Some(sec) = patch.force_exit_sec {
            if sec < 0 {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "forceExitSec must be >= 0 (0 = the guillotine is OFF, \
                             the calibrated default), got {sec}; nothing was applied"
                    ),
                ));
            }
            changes.push(ExitSetChange {
                field: "forceExitSec",
                from: current.force_exit_sec.to_string(),
                to: sec.to_string(),
                effect: "live",
            });
        }

        // A bool has no illegal value to refuse — the patch either names it or
        // it doesn't. The from/to audit strings are `true`/`false` verbatim,
        // same convention as the decimal fields' `to_string`.
        if let Some(on) = patch.trailing_enabled {
            changes.push(ExitSetChange {
                field: "trailingEnabled",
                from: current.trailing_enabled.to_string(),
                to: on.to_string(),
                effect: "live",
            });
        }

        if changes.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "risk.setExit: no field carried a change — nothing to audit",
            ));
        }

        // Write phase: BOTH copies. `config.positions.exit` is what Gate 4
        // binds per new intent; `PositionManager.config.exit` is what the exit
        // sweep (stop/take/force judgement, the dynamic-stop ramp) reads every
        // tick. set_config keeps everything else (budgets, cooldowns) as-is —
        // only the exit triple's two fields move.
        {
            let mut exit = self.config.positions.exit.clone();
            if let Some(v) = patch.stop_loss_pct {
                exit.stop_loss_pct = v;
            }
            if let Some(v) = patch.take_profit_pct {
                exit.take_profit_pct = v;
            }
            if let Some(sec) = patch.force_exit_sec {
                exit.force_exit_sec = sec;
            }
            if let Some(on) = patch.trailing_enabled {
                exit.trailing_enabled = on;
            }
            self.config.positions.exit = exit.clone();
            let mut positions = self.config.positions.clone();
            positions.exit = exit;
            self.positions.set_config(positions);
        }

        for c in &changes {
            tracing::info!(
                target: "risk",
                actor = %actor,
                field = c.field,
                from = %c.from,
                to = %c.to,
                effect = c.effect,
                reason = patch.reason.as_deref().unwrap_or(""),
                "exit discipline update: {} {} -> {} ({})",
                c.field,
                c.from,
                c.to,
                EXIT_SET_UPDATE_NOTE,
            );
        }
        // Issue #464, the ladder/stopLossPct semantic note: the two are
        // INDEPENDENT mechanisms — `stop_loss_pct` is a percentage of the
        // entry price re-judged every tick, a ladder's `move_stop_to` an
        // absolute price floor recorded per step — so neither overwrite the
        // other and there is no state to reconcile. The one interaction worth
        // saying out loud: a changed `stop_loss_pct` re-bases the policy side
        // of the ladder's `max(policy stop, ladder floor)` on the NEXT exit
        // tick (already-open positions included), while floors already raised
        // by fired steps stay exactly where they are.
        if patch.stop_loss_pct.is_some() && !self.config.positions.exit_ladder.is_empty() {
            tracing::info!(
                target: "risk",
                actor = %actor,
                "exit ladder is active: the changed stop_loss_pct re-bases the \
                 ladder's stop floor on the next exit tick (fired move_stop_to \
                 floors are absolute and unaffected)"
            );
        }
        Ok(ExitSetUpdate {
            applied: changes,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            persisted: false,
            note: EXIT_SET_UPDATE_NOTE,
        })
    }

    /// 轮次时间周期的运行时写路径（`round_duration.set`，UI 设置卡）：
    /// **next_session** —— 白名单校验通过后只写持久格（生产
    /// `data/runtime/config.json`），本会话的网格一个字节都不动。
    ///
    /// 为什么不是热旋钮：轮次周期是网格坐标系 —— `round_slot` 是用会话的
    /// duration 推导的 UTC 网格索引，仓位恢复（`OpenParams.expires_at_ms`）、
    /// #457 过期扫描都用**当前** config 重算存量订单/仓位的过期时间；热切
    /// duration 会把旧 slot 全部错解（15m 轮的 slot 3 在 5m 网格里是另一个
    /// 时间窗）。重启后启动层读回持久格，新网格从干净状态开始。
    ///
    /// `reset: true` 清格 —— 下次会话回落启动解析（显式 flag > env >
    /// TOML > 默认）。`durationSec` 与 `reset` 恰好给一个，都给或都不给
    /// 拒绝。`runtime_config_path: None`（回测器/进程内测试）拒绝写并如实
    /// 回报 —— 测试不往调用者恰好在的 checkout 里落文件。
    pub fn apply_round_duration_set(
        &mut self,
        patch: &crate::ipc::schema::SetRoundDurationParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::ipc::schema::RoundDurationUpdate> {
        use crate::ipc::schema::{ROUND_DURATION_SET_NOTE, RoundDurationUpdate};

        if patch.reset && patch.duration_sec.is_some() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "round_duration.set: give durationSec or reset, not both".to_string(),
            ));
        }
        let sec = patch.duration_sec;
        if !patch.reset && sec.is_none() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "round_duration.set needs durationSec (one of {:?}) or reset: true",
                    self.config.allowed_round_secs
                ),
            ));
        }

        let persist = |value: Option<i64>| -> CoreResult<()> {
            let Some(path) = self.config.runtime_config_path.as_deref() else {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    "round_duration.set: no runtime config path configured — \
                     persistence is disabled for this core (backtests and \
                     in-process tests); nothing was applied"
                        .to_string(),
                ));
            };
            crate::ipc::runtime_config::persist_round_duration_to(
                std::path::Path::new(path),
                value,
                actor,
                now_ms,
            )
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("round_duration.set: persist failed, nothing changed: {e}"),
                )
            })
        };

        // reset 路径：清格，下次会话回落启动解析。
        if patch.reset {
            persist(None)?;
            tracing::info!(
                target: "risk",
                actor = %actor,
                reason = patch.reason.as_deref().unwrap_or(""),
                "round duration cell cleared — next session re-resolves from \
                 the boot flags/env/TOML ({ROUND_DURATION_SET_NOTE})"
            );
            return Ok(RoundDurationUpdate {
                next_session_sec: self.config.round_duration_sec,
                reset: true,
                at_ms: now_ms,
                actor: actor.to_string(),
                reason: patch.reason.clone(),
                note: ROUND_DURATION_SET_NOTE,
            });
        }

        // 写路径：声明并集校验（一格越界值就是一台没人能解读的网格），然后
        // 原子写持久格。并集来自插件申报（`supported_round_secs`），不是内核
        // 知识；空并集 = 没有插件申报任何档 = 全拒（fail-closed）。
        let Some(sec) = sec else {
            unreachable!("the reset path returned above; a write path has a durationSec");
        };
        if !self.config.allowed_round_secs.contains(&sec) {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "round_duration.set: {sec} is not declared by any market plugin \
                     (declared union: {:?}) — nothing was applied",
                    self.config.allowed_round_secs
                ),
            ));
        }
        persist(Some(sec))?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            from = self.config.round_duration_sec,
            to = sec,
            current_session_unchanged = true,
            reason = patch.reason.as_deref().unwrap_or(""),
            "round duration set to {sec}s — next session ({ROUND_DURATION_SET_NOTE})"
        );
        Ok(RoundDurationUpdate {
            next_session_sec: sec,
            reset: false,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            note: ROUND_DURATION_SET_NOTE,
        })
    }

    /// #458: 交易环境重置（`system.reset_trading`，UI 设置卡 + 确认词）。
    /// 把本核拥有的交易状态整表清零并重新 seed —— 停用全部已启用策略、撤销
    /// 全部 resting 挂单、清空订单/成交/持仓记录（内存 + 磁盘）、按
    /// `dry_seed_balance` 复原每个账户现金、重置连亏熔断。
    ///
    /// 三道门：Live 拒绝（真实资金记录是 append-only 证据，不是 UI 可擦的
    /// 演示盘）；`confirm` 必须逐字等于 `RESET`（自由文本确认门，不是勾选框
    /// —— 误触必须不可能被归咎于双击）；`manual_ops_audit_path: None`
    /// （回测器/进程内测试）拒绝 —— manual-ops 审计行是重置留下的唯一痕迹，
    /// 无处审计的擦除必须失败而不是静默发生。
    ///
    /// 保留（显式不动）：配置、策略文件、全部既有审计流、
    /// strategy-state.json（停用策略会走 `set_strategy_enabled` 的既有持久化
    /// 路径重写 enabled 集 —— 这是行为语义的一部分，不是泄漏）。
    pub fn reset_trading(
        &mut self,
        params: &crate::ipc::schema::ResetTradingParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::ipc::schema::ResetTradingResult> {
        use crate::ipc::runtime_config::append_manual_op;
        use crate::ipc::schema::{RESET_TRADING_LIVE_REFUSAL, ResetPreSummary, ResetTradingResult};

        if self.config.mode == Mode::Live {
            return Err(CoreError::new(
                CoreErrorCode::RiskRejected,
                RESET_TRADING_LIVE_REFUSAL.to_string(),
            ));
        }
        if params.confirm != "RESET" {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "system.reset_trading: confirm must be exactly \"RESET\" — \
                 this wipes every trading record this core owns"
                    .to_string(),
            ));
        }
        // issue 561 账户维隔离：params.account_id 在派发层已填缺省
        // （session → display → 进程 default 链，RAW params 判定显式 id），
        // 这里照字面执行。显式拒绝未知账户 —— 一个静默的空清洗比拒绝更糟。
        let account_id = match params.account_id.as_deref() {
            Some(id) => {
                let id = crate::account::AccountId::from(id);
                if self.accounts.account(&id).is_none() {
                    return Err(CoreError::new(
                        CoreErrorCode::InvalidParams,
                        format!(
                            "system.reset_trading: unknown account \"{id}\" — \
                             the reset scope must name a configured account"
                        ),
                    ));
                }
                id
            }
            // 兜底（直连内核、不经 IPC 派发层的调用方）：进程 display 格。
            None => crate::account::AccountId::from(self.accounts.display_id()),
        };
        let audit_path = self
            .config
            .manual_ops_audit_path
            .clone()
            .map(std::path::PathBuf::from);
        let Some(audit_path) = audit_path.as_deref() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "system.reset_trading: no manual-ops audit path configured — \
                 a wipe with no audit trail is refused (backtests and \
                 in-process tests carry None)"
                    .to_string(),
            ));
        };
        let audit_path = std::path::Path::new(audit_path);

        // 重置前快照：行数 + 余额 + 活跃集合。这是审计行与回执里唯一存活的
        // 「删了什么」的证据 —— 记录本身不会留下。issue 561: 全部行数按
        // 账户过滤 —— 全局数会谎报清洗范围（重置 B 不删 A 的行）。
        let pre = ResetPreSummary {
            order_log_lines: self
                .order_db
                .as_ref()
                .map(|db| {
                    db.load()
                        .iter()
                        .filter(|o| o.account_id.as_str() == account_id.as_str())
                        .count() as u64
                })
                .unwrap_or(0),
            trade_log_lines: self
                .trade_db
                .as_ref()
                .map(|db| {
                    crate::trade_db::read_recent(db.jsonl_path(), 0)
                        .iter()
                        .filter(|r| {
                            let rid = r
                                .get("accountId")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            if rid.is_empty() {
                                account_id.as_str() == crate::account::default_account_id().as_str()
                            } else {
                                rid == account_id.as_str()
                            }
                        })
                        .count() as u64
                })
                .unwrap_or(0),
            position_log_lines: self
                .position_db
                .as_ref()
                .map(|db| {
                    db.load()
                        .iter()
                        .filter(|p| p.account_id.as_str() == account_id.as_str())
                        .count() as u64
                })
                .unwrap_or(0),
            open_positions: self
                .positions
                .open_positions()
                .iter()
                .filter(|p| p.account_id.as_str() == account_id.as_str())
                .count(),
            live_orders: self
                .ome
                .live_orders()
                .iter()
                .filter(|o| o.account_id.as_str() == account_id.as_str())
                .count(),
            balance: self
                .accounts
                .ledger_of(&account_id)
                .map(|l| l.balance())
                .unwrap_or(Decimal::ZERO),
        };

        // 1) 停用目标账户 scopes 里的已启用策略（复用 strategy.enable 的
        //    停用路径）。账户维裁决：只动该账户的 scopes —— 全局
        //    `enabled_strategy_names` 会连其他账户的同名 scope 一起停。
        //    accounts.toml 的启用集（持久真相）不写回：重置是运行时清洗
        //    （会话级 OFF），不是授权变更，重启后启用集原样生效。
        let account_scopes: Vec<String> = self
            .engine
            .as_ref()
            .map(|e| {
                e.strategy_scopes_all()
                    .into_iter()
                    .filter(|(a, _)| a == account_id.as_str())
                    .map(|(_, n)| n)
                    .collect()
            })
            .unwrap_or_default();
        let disabled: Vec<String> = {
            let mut names: Vec<String> = Vec::new();
            if let Some(e) = self.engine.as_ref() {
                for name in &account_scopes {
                    if e.enabled_strategies().iter().any(|n| n == name) {
                        names.push(name.clone());
                    }
                }
            }
            names.sort();
            names.dedup();
            names
        };
        for name in &disabled {
            self.set_strategy_enabled(name, false);
        }

        // 2) 撤销目标账户的 resting 挂单。逐单走 `cancel` 释放 BUY 预留
        //    （在订单自己的账簿里，E28）、清 engine 的 pending-token 闩、记
        //    fast_orders_dirty；随后 remove_account_orders 负责「连记录都
        //    消失」的部分。账户维裁决：token=None 改为账户过滤，其他账户
        //    的挂单原样保留。
        let cancelled = {
            let ids: Vec<String> = self
                .ome
                .live_orders()
                .into_iter()
                .filter(|o| o.account_id.as_str() == account_id.as_str())
                .map(|o| o.order_id.clone())
                .collect();
            let mut n = 0usize;
            for id in &ids {
                if let Err(e) = self.cancel(id, now_ms) {
                    tracing::warn!(error = %e, order = %id, "reset_trading: cancel failed mid-reset");
                } else {
                    n += 1;
                }
            }
            n
        };

        // 3) 内存态按账户维清洗（issue 561 清单 A 项）+ 全局维照旧（清单
        //    B 项）。OME 不再整替：整替会把其他账户的订单/幂等表一起抹掉，
        //    只拿掉目标账户的订单行。副作用（tx 事件）不重放：重置不是一
        //    个需要逐单通知的操作。
        let live_before = pre.live_orders;
        self.ome.remove_account_orders(account_id.as_str());
        self.inflight_takers.clear();
        self.fast_dirty_tokens.clear();
        self.fast_eval_wake = false;
        self.fast_orders_dirty = true;
        self.fast_positions_dirty = true;
        // exit 路由三态：exit_reasons 的键是 token——只清仍归属目标账户的
        // token（其他账户可能持有同一 token 的独立行，账户↔venue 解耦后
        // 双账户可同持一 token）。strategy_exits 按意图自带 account_id 过滤。
        let account_tokens: std::collections::HashSet<String> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id.as_str() == account_id.as_str())
            .map(|p| p.token_id.clone())
            .chain(
                self.ome
                    .all()
                    .iter()
                    .filter(|o| o.account_id.as_str() == account_id.as_str())
                    .map(|o| o.token_id.clone()),
            )
            .collect();
        // 摘除该账户持仓/订单前先记下它的 token 集（上面），此刻 positions
        // 还未动 —— remove_account_positions 在第 4 步执行，token 集先取。
        let doomed_tokens: Vec<String> = {
            let before = self
                .positions
                .open_positions()
                .iter()
                .filter(|p| p.account_id.as_str() == account_id.as_str())
                .map(|p| p.token_id.clone())
                .collect::<std::collections::HashSet<_>>();
            account_tokens.union(&before).cloned().collect()
        };
        for t in &doomed_tokens {
            if let Some(reason) = self.exit_reasons.remove(t) {
                self.exit_reasons_evicted += 0; // 计数语义未变：清的是重置面
                let _ = reason;
            }
            self.exit_reason_order.retain(|k| k != t);
        }
        self.strategy_exits
            .retain(|x| x.account_id.as_str() != account_id.as_str());
        self.place_cooldowns.clear();
        self.escalation_skips.clear();
        self.pending_venue_cancels.clear();
        self.books.clear();
        self.consecutive_venue_rejects = 0;
        self.consecutive_venue_rejects_by_account
            .remove(account_id.as_str());
        self.account_freezes.remove(account_id.as_str());
        self.auto_frozen.remove(account_id.as_str());
        self.consecutive_sweep_failures = 0;
        self.last_error = None;
        // #537: the strategy page's PnL counters are the OTHER survivor —
        // trade_db and the cash ledger reset, but these per-(strategy,account)
        // slices kept their closed_trades/wins/net_pnl_usd, so the panel's
        // 「真实余额 = 本金 + 净利」 kept folding a history the reset had
        // erased. A wiped environment reports zero for every counter.
        // issue 561: 只摘目标账户的切片（键尾 `\u{1}account`），其他账户
        // 的计数原样保留。
        let acct_suffix = format!("\u{1}{}", account_id.as_str());
        self.strategy_accounting
            .retain(|k, _| !k.ends_with(&acct_suffix));
        if let Some(l) = self.applied_log.as_ref() {
            l.compact(&[]);
        }
        self.breaker = crate::risk::LossBreakers::new(
            self.config.max_consecutive_losses,
            self.config.breaker_cooldown_sec,
        );
        self.risk.resume();

        // 4) 持仓簿账户维摘除 + 磁盘记录按账户截断（不重启进程）。
        let open_before = pre.open_positions;
        let (removed_open, _removed_closed) =
            self.positions.remove_account_positions(account_id.as_str());
        let _ = removed_open;
        if let Some(db) = &self.position_db {
            // 保留其他账户的行原样落盘 —— 整簿 save(&[]) 会把 A 的持仓也
            // 抹掉（账户维裁决）。
            let remaining: Vec<crate::position::OpenPosition> =
                self.positions.open_positions().to_vec();
            db.save(&remaining);
        }
        if let Some(db) = &self.order_db {
            db.compact_account(account_id.as_str());
        }
        if let Some(db) = &mut self.trade_db {
            db.retract_account(account_id.as_str(), now_ms);
        }
        // #178/#175 的幂等簿与结算簿：重置后历史结算不再需要去重（记录文件
        // 已空，结算簿对着空文件重放会凭空恢复 claims）。全局维（清单 B）：
        // 结算簿键无账户归属，保持整簿重置 —— 账户维清算绝不产生跨账户
        // claims 复活。
        self.settlement = crate::settlement::SettlementBook::new(None);
        self.recon_watermark_ms = 0;

        // 记账审计与交易记录同生共死：trade_db 清零后，带着旧 realized 的锚点
        // 会让 trade_ledger_identity 永久假红（锚点不动但账目已归零），
        // halt_entries 把之后每一笔意图都拦在门外。重置即回到 boot 同款
        // AuditRuntime（保留 audit_log_path）——下一次审计在干净账目上重新
        // 取锚，与「重置=回到全新环境」的语义一致。
        let audit_log_path = self.audit.audit_log_path.clone();
        self.audit = AuditRuntime {
            audit_log_path,
            ..Default::default()
        };

        // 5) 重新 seed 目标账户到配置值（issue 561 账户维原语）。语义与
        //    启动 seed 一致（余额回到种子值），journal 重写为单行 `Reset`
        //    行 —— boot 回放后 balance == 种子（与自检断言一致，裁决 3 修
        //    复），其他账户的 journal 一字不动。
        let journal_rewrote = self
            .accounts
            .reset_account_to_seed(&account_id, self.config.dry_seed_balance)?;
        if !journal_rewrote {
            tracing::warn!(
                account = %account_id.as_str(),
                "issue 561: account journal rewrite failed at reset — the \
                 next boot may replay pre-reset rows"
            );
        }

        // 6) 自校验：目标账户 live 索引空 / 该账户 open 持仓空 / 该账户在
        //    三个记录文件中的行数归零 / 余额 == seed。其他账户的行不在断言
        //    面 —— 它们本就不该被清洗。
        let mut problems: Vec<String> = Vec::new();
        let leftover_live = self
            .ome
            .live_orders()
            .iter()
            .filter(|o| o.account_id.as_str() == account_id.as_str())
            .count();
        if leftover_live != 0 {
            problems.push(format!(
                "account {account_id} live index not empty after reset ({leftover_live} orders)"
            ));
        }
        let leftover_open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id.as_str() == account_id.as_str())
            .count();
        if leftover_open != 0 {
            problems.push(format!(
                "account {account_id} open positions not empty after reset ({leftover_open} remain)"
            ));
        }
        if let Some(db) = &self.order_db {
            let n = db
                .load()
                .iter()
                .filter(|o| o.account_id.as_str() == account_id.as_str())
                .count();
            if n != 0 {
                problems.push(format!(
                    "account {account_id} order log still has {n} lines after reset"
                ));
            }
        }
        if let Some(db) = &self.trade_db {
            let n = crate::trade_db::read_recent(db.jsonl_path(), 0)
                .iter()
                .filter(|r| {
                    let rid = r
                        .get("accountId")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if rid.is_empty() {
                        account_id.as_str() == crate::account::default_account_id().as_str()
                    } else {
                        rid == account_id.as_str()
                    }
                })
                .count();
            if n != 0 {
                problems.push(format!(
                    "account {account_id} trade log still has {n} lines after reset"
                ));
            }
        }
        if let Some(db) = &self.position_db {
            let n = db
                .load()
                .iter()
                .filter(|p| p.account_id.as_str() == account_id.as_str())
                .count();
            if n != 0 {
                problems.push(format!(
                    "account {account_id} position log still has {n} lines after reset"
                ));
            }
        }
        let balances: Vec<(String, Decimal)> = vec![(
            account_id.as_str().to_string(),
            self.accounts
                .ledger_of(&account_id)
                .map(|l| l.balance())
                .unwrap_or(Decimal::ZERO),
        )];
        for (id, bal) in &balances {
            if *bal != self.config.dry_seed_balance {
                problems.push(format!(
                    "account {id} balance {bal} != seed {} after reset",
                    self.config.dry_seed_balance
                ));
            }
        }
        if !problems.is_empty() {
            let msg = format!(
                "system.reset_trading: post-reset self-check FAILED — {}",
                problems.join("; ")
            );
            // 自检失败也要留审计行（审计写失败再叠加错误）。
            let _ = append_manual_op(
                audit_path,
                &serde_json::json!({
                    "op": "reset_trading",
                    "phase": "self_check_failed",
                    "atMs": now_ms,
                    "actor": actor,
                    "account": account_id.as_str(),
                    "reason": params.reason,
                    "problems": problems,
                }),
            );
            return Err(CoreError::new(CoreErrorCode::Internal, msg));
        }

        // 7) manual-ops 审计行：重置留下的唯一痕迹。写失败 = 整体失败
        //    （状态已清但审计缺席是「不可追溯的擦除」，必须显式暴露）。
        append_manual_op(
            audit_path,
            &serde_json::json!({
                "op": "reset_trading",
                "atMs": now_ms,
                "actor": actor,
                "account": account_id.as_str(),
                "reason": params.reason,
                "mode": format!("{:?}", self.config.mode).to_lowercase(),
                "pre": {
                    "orderLogLines": pre.order_log_lines,
                    "tradeLogLines": pre.trade_log_lines,
                    "positionLogLines": pre.position_log_lines,
                    "openPositions": pre.open_positions,
                    "liveOrders": pre.live_orders,
                    "balance": pre.balance.to_string(),
                },
                "post": {
                    "disabledStrategies": disabled,
                    "cancelledOrders": cancelled,
                    "balances": balances.iter()
                        .map(|(id, b)| serde_json::json!({
                            "accountId": id, "balance": b.to_string()
                        }))
                        .collect::<Vec<_>>(),
                },
            }),
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("system.reset_trading: manual-ops audit write failed: {e}"),
            )
        })?;

        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %account_id.as_str(),
            live_orders = live_before,
            open_positions = open_before,
            cancelled = cancelled,
            disabled = disabled.len(),
            "trading environment reset (account-scoped) — the account's \
             records wiped, its cash re-seeded; other accounts untouched"
        );

        Ok(ResetTradingResult {
            disabled_strategies: disabled,
            cancelled_orders: cancelled,
            cleared_open_positions: open_before,
            cleared_closed_trades: pre.trade_log_lines as usize,
            account_id: account_id.as_str().to_string(),
            balances,
            pre_summary: pre,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: params.reason.clone(),
        })
    }
    /// The ONE writer of the panel-visible last-error slot (#180). Every
    /// internal refusal path funnels through here — directly, or via
    /// [`Core::emit_error`] — so the slot cannot be written with a message no
    /// code classifies, and cannot be skipped because one path forgot. It is
    /// also what makes the slot a *fresh* fact: each write carries the instant
    /// the failure was observed, so a reader can tell a new failure from an old
    /// banner (`tsMs` moves, `message` is the newest reason).
    fn note_error(&mut self, err: &CoreError, now_ms: i64) {
        self.last_error = Some(LastError {
            ts_ms: now_ms,
            code: err.code,
            message: err.message.clone(),
        });
    }
    /// Broadcast a structured error to Node (never swallowed) AND record it in
    /// the one panel-visible error slot.
    pub fn emit_error(&mut self, e: CoreError) {
        self.emit_error_at(e, now_ms());
    }
    /// [`Core::emit_error`] with the caller's own instant, for error paths that
    /// carry the event time (a sweep at its `ts`) rather than the time of this
    /// call.
    pub fn emit_error_at(&mut self, e: CoreError, now_ms: i64) {
        self.note_error(&e, now_ms);
        self.emit_error_event(e);
    }

    /// E25 (#331): ONE audit line per arbitrated suggestion — never folded,
    /// never blocking (a failed append warns once and the trade goes on) —
    /// plus AT MOST one `INTENT_DECISION` push per `(strategy, gate, reason)`
    /// per second (§12.3): the PUSH folds, the audit does not.
    fn record_intent_decision(
        &mut self,
        intent: &crate::arbitration::StrategyIntent,
        outcome: &crate::arbitration::Outcome,
        now_ms: i64,
    ) {
        let intent_json = serde_json::to_value(&intent.request).unwrap_or(serde_json::Value::Null);
        let record = self.intent_audit.record(
            now_ms,
            intent.request.account_id.as_str(),
            &intent.request.strategy,
            &intent_json,
            &outcome.decision,
            &outcome.gates,
            outcome.latency_us,
        );
        // The throttle key is (strategy, gate, reason); for a non-rejection the
        // "reason" slot carries the status, so an approval storm folds too.
        let (gate, key_reason) = match &outcome.decision {
            crate::arbitration::Decision::Rejected { gate, reason, .. } => (
                format!("{gate:?}").to_uppercase(),
                format!("{reason:?}").to_uppercase(),
            ),
            _ => (
                outcome
                    .gates
                    .last()
                    .map(|g| format!("{:?}", g.gate).to_uppercase())
                    .unwrap_or_else(|| "UNKNOWN".into()),
                outcome.decision.status().to_string(),
            ),
        };
        if let Some(count) =
            self.intent_audit
                .throttle(&intent.request.strategy, &gate, &key_reason, now_ms)
        {
            let push = crate::arbitration::AuditSink::push_for(&record, count);
            self.emit(Event::IntentDecision {
                account_id: record.account_id.clone(),
                strategy: push.strategy,
                intent_id: push.intent_id,
                status: push.status,
                gate: push.gate,
                detail: push.detail,
                ts_ms: push.ts_ms,
                count: push.count,
            });
        }
    }

    /// Emit an error event WITHOUT touching the slot, for a consequence whose
    /// cause is already recorded: the slot must keep showing the error an
    /// operator has to act on (the raw venue refusal), not the kernel's own
    /// reaction to it (the freeze). Everything else goes through
    /// [`Core::emit_error`].
    fn emit_error_event(&self, e: CoreError) {
        self.emit(Event::Error { error: e });
    }
    /// Broadcast a risk alert.
    pub fn emit_risk_alert(&self, code: CoreErrorCode, message: String) {
        self.emit(Event::RiskAlert { code, message });
    }

    /// Roll the daily-loss budget when the UTC day changes (P0 #173).
    ///
    /// The budget's base is the account's CASH EQUITY — the ledger's
    /// venue-reconciled balance (host `seed_balance`/`venue_free_balance` keep
    /// it honest in live, `dry_seed_balance` in dry). Only the kernel can read
    /// it: the percentage cap is relative to the day's opening equity, so a
    /// fixed dollar default stops being meaningless on a small book. Called
    /// once per tick, before the exit checks, so a position closed right after
    /// the boundary is already counted in the new day.
    fn roll_daily_budget(&mut self, now_ms: i64) {
        // `balance` is already gross of local reservations — adding `reserved`
        // here would double-count resting BUY commitments. The daily budget is
        // PER-ACCOUNT (operator verdict 2026-10-25, overriding the old §9.3
        // global reading): each account's day opens on ITS OWN ledger equity,
        // and one account's losses never burn a sibling's budget.
        let accounts: Vec<(crate::account::AccountId, Decimal)> = self
            .accounts
            .account_list()
            .iter()
            .filter(|a| a.status == crate::account::AccountStatus::Active)
            .map(|a| {
                (a.id.clone(), {
                    self.accounts
                        .ledger_of(&a.id)
                        .map(|l| l.balance())
                        .unwrap_or_default()
                })
            })
            .collect();
        for (id, equity) in accounts {
            if let Some(roll) =
                self.positions
                    .roll_daily(id.as_str(), now_ms, equity, self.equity_basis())
            {
                let message = match roll.previous_day_index {
                    Some(prev) => format!(
                        "daily loss budget: UTC day {prev} closed at ${} realized{} — day {} opens with equity ${}, cap ${}",
                        roll.previous_realized_pnl_usd,
                        if roll.previous_tripped {
                            " (TRIPPED: entries were frozen)"
                        } else {
                            ""
                        },
                        roll.day_index,
                        roll.opening_equity_usd,
                        roll.limit_usd
                    ),
                    None => format!(
                        "daily loss budget: UTC day {} opens with equity ${}, cap ${} (realized loss resets at the UTC day boundary)",
                        roll.day_index, roll.opening_equity_usd, roll.limit_usd
                    ),
                };
                if roll.previous_tripped {
                    tracing::warn!(day = roll.day_index, account = %id.as_str(), "{}", message);
                } else {
                    tracing::info!(day = roll.day_index, account = %id.as_str(), "{}", message);
                }
                self.emit_risk_alert(
                    CoreErrorCode::RiskRejected,
                    format!("[{}] {}", id.as_str(), message),
                );
            }
        }
    }

    /// Which book the equity handed to [`Self::roll_daily_budget`] came from
    /// (P1 #235): the day's percentage cap is a share of an account, and the
    /// ledger balance means a different thing in each of them.
    ///
    /// Derived from the SAME predicate `Core::new` uses to decide which book the
    /// ledger is seeded from — a locally-settling mode gets `dry_seed_balance`,
    /// live gets the venue's money — so the label cannot drift from the number
    /// it describes. A mode that settles locally is trading the simulation even
    /// when it is called read-only, and it must not be told its base came from
    /// the venue.
    fn equity_basis(&self) -> EquityBasis {
        if self.config.mode.settles_locally() {
            EquityBasis::Dry
        } else {
            EquityBasis::Live
        }
    }

    /// Announce a tripped daily-loss breaker exactly once per process (see
    /// `DailyLossState::tripped_reported`). Consumed here AND at the losing
    /// close, so the freeze is visible at the moment it happens and again after
    /// a restart restored an already-breached budget.
    fn emit_daily_trip(&mut self) {
        let trips = self.positions.take_daily_trips();
        for (account, t) in trips {
            let message = format!(
                "DAILY LOSS LIMIT REACHED for account {account}: realized ${} + unrealized ${} = net ${} against a ${} cap ({}% of the day's opening equity ${}) — new entries for this account are frozen until the next UTC day (positions still exit normally)",
                t.realized_pnl_usd,
                self.positions.unrealized_pnl_usd_for(account.as_str()),
                t.realized_pnl_usd + self.positions.unrealized_pnl_usd_for(account.as_str()),
                t.limit_usd,
                self.config.positions.max_daily_loss_equity_pct,
                t.opening_equity_usd
            );
            tracing::warn!(
                day = t.day_index,
                account = %account.as_str(),
                realized = %t.realized_pnl_usd,
                limit = %t.limit_usd,
                "{}",
                message
            );
            self.emit_risk_alert(CoreErrorCode::RiskRejected, message);
        }
    }

    /// Report every exit withheld from becoming an order (P0 #177, F6): the
    /// audit trail must show "should have triggered" instead of a silent hold.
    ///
    /// The log line names the cause instead of calling all of them a suppressed
    /// protective stop (#267): a held TRAILING stop is not a held protective
    /// stop, and a reviewer reading "protective stop suppressed" about a profit
    /// rule is being told the wrong story. The wording lives on the event
    /// (`SuppressedStopEvent::message`), so the panel and the log cannot drift.
    fn emit_suppressed_stops(&mut self) {
        for ev in self.positions.drain_suppressed_stops() {
            let message = ev.message();
            // #268 item 2: past `N x max_book_age_sec` unpriceable, this is no
            // longer a routine hold — it is a position that cannot act, and it
            // is raised to error level so it survives a warn filter. Two call
            // sites rather than a dynamic level: tracing's level is a compile-
            // time property of the callsite.
            if ev.escalated {
                tracing::error!(
                    position = %ev.position_id,
                    token = %ev.token_id,
                    cause = ?ev.cause,
                    bid = %ev.bid,
                    mid = %ev.mid,
                    stop_pct = %ev.stop_pct,
                    book_age_ms = ?ev.book_age_ms,
                    unpriceable_for_ms = ev.unpriceable_for_ms,
                    "{}",
                    message
                );
            } else {
                tracing::warn!(
                    position = %ev.position_id,
                    token = %ev.token_id,
                    cause = ?ev.cause,
                    bid = %ev.bid,
                    mid = %ev.mid,
                    stop_pct = %ev.stop_pct,
                    book_age_ms = ?ev.book_age_ms,
                    unpriceable_for_ms = ev.unpriceable_for_ms,
                    "{}",
                    message
                );
            }
            self.emit_risk_alert(CoreErrorCode::RiskRejected, message);
        }
    }

    // ── Orders ─────────────────────────────────────────────────────────────
    pub fn place(
        &mut self,
        req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
    ) -> CoreResult<(OrderId, OrderStatus)> {
        let outcome = self.place_inner(req, maker_timeout_ms, now_ms, true)?;
        Ok((outcome.order_id, outcome.status))
    }

    /// [`Core::place`] with the full outcome: the id, the status AND — when the
    /// kernel itself refused the leg after accepting it (a dry/read-only taker
    /// the book cannot fill) — the structured reason for that refusal.
    ///
    /// `orders.place` answers from this (#180) so a caller is never handed a
    /// bare `REJECTED`; everything in-tree keeps the tuple shape, which is what
    /// the strategy/engine paths use.
    pub fn place_outcome(
        &mut self,
        req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
    ) -> CoreResult<PlaceOutcome> {
        self.place_inner(req, maker_timeout_ms, now_ms, true)
    }

    /// Submit a leg that COMPLETES an already-open position rather than starting
    /// a new one — the maker→taker escalation of the same entry.
    ///
    /// The entry gates (`can_open`, breaker) exist to stop a SECOND position on
    /// an asset. When the maker leg filled only partially, the position already
    /// exists, so `can_open` rejects the escalated remainder with "Already in
    /// {asset}" and the leg is silently dropped: the venue-driven partial fill
    /// stays half-filled forever. A LIVE-only defect — dry maker fills are always
    /// full fills. The risk gate still applies to the escalated notional.
    fn place_escalated(
        &mut self,
        req: OrderRequest,
        now_ms: i64,
    ) -> CoreResult<(OrderId, OrderStatus)> {
        let outcome = self.place_inner(req, 0, now_ms, false)?;
        Ok((outcome.order_id, outcome.status))
    }

    /// Every refusal this kernel makes about an order funnels through here, so
    /// the one error slot (#180) is updated whichever gate said no — and so the
    /// slot can never be skipped because one path forgot to record.
    fn place_inner(
        &mut self,
        req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
        entry_gates: bool,
    ) -> CoreResult<PlaceOutcome> {
        let outcome = self.place_gated(req, maker_timeout_ms, now_ms, entry_gates);
        if let Err(e) = &outcome {
            self.note_error(e, now_ms);
        }
        // Replay fast path: a placed order is a live order — the escalation
        // sweep and the audit input changed.
        if outcome.is_ok() && self.replay_sched() {
            self.fast_orders_dirty = true;
        }
        outcome
    }

    /// E28 (§9.2, G5): a CLOSE is inventory-bound — the open position for a
    /// token belongs to exactly one account, and a close order naming another
    /// account would settle money into a book that never paid for the shares.
    /// Refused, never re-routed: re-routing would be the implicit-ledger
    /// accident (§9.3) under a different name. Opens are unaffected (a BUY
    /// commits its own book's cash); an exit for a token with NO open
    /// position anywhere passes here and is handled by the fill projection
    /// as before. The judgment is per-account first: the REQUEST'S OWN
    /// account holding the token is the legitimate close (the fill lands on
    /// that account's row); the refusal fires only when the request names an
    /// account that holds NOTHING while the token sits in a SIBLING book —
    /// that order would settle into the wrong book.
    fn refuse_cross_account_close(&self, req: &OrderRequest) -> CoreResult<()> {
        if req.side != Side::Sell || !crate::risk::is_close_intent(&req.internal_key) {
            return Ok(());
        }
        let own = self
            .positions
            .open_positions()
            .iter()
            .any(|p| p.token_id == req.token_id && p.account_id == req.account_id);
        if own {
            return Ok(());
        }
        if let Some(p) = self
            .positions
            .open_positions()
            .iter()
            .find(|p| p.token_id == req.token_id)
        {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "cross-account close refused: position {} on token {} belongs to \
                     account `{}`, the order names `{}`",
                    p.id, p.token_id, p.account_id, req.account_id
                ),
            ));
        }
        Ok(())
    }

    /// Account↔venue binding (first batch): an order whose token belongs to a
    /// DIFFERENT venue's live round market than the account is pinned to is
    /// refused, naming both venues. Untagged on either side passes — an
    /// unbound account (`venue == ""`, the zero-config default) trades
    /// anywhere, and an order whose token belongs to no labelled market (the
    /// engine down, a legacy corpus) carries no venue evidence to judge. The
    /// venue read is the ORDER'S EXECUTION VENUE (`execution_venue_of`):
    /// a shared token carries one row per venue, and judging on a token-only
    /// read alone would refuse on the ambiguity (or pick the wrong row) —
    /// the account's binding names its own row, the same admission blindness
    /// the engine's `scope_admits_token` resolves.
    fn refuse_cross_venue_order(&self, req: &OrderRequest) -> CoreResult<()> {
        let Some(account) = self.accounts.account(&req.account_id) else {
            // Unknown accounts are refused by the ledger lookup right after;
            // nothing venue-shaped to check here.
            return Ok(());
        };
        let bound = account.venue.as_str();
        if bound.is_empty() {
            return Ok(());
        }
        let token_venue = self.execution_venue_of(req.account_id.as_str(), &req.token_id);
        if token_venue.is_empty() || token_venue == bound {
            return Ok(());
        }
        Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(
                "cross-venue order refused: account `{}` is bound to venue `{bound}` but the \
                 order's token {} belongs to venue `{token_venue}`",
                req.account_id, req.token_id
            ),
        ))
    }

    /// The account's SUBSCRIBED round-duration families — the set of
    /// (venue, duration_sec) pairs this account is allowed to trade — as the
    /// admission gate sees it. THIS PR (B1c) the account book carries no
    /// subscription field (accounts.toml has no `rounds` key; B1d adds it),
    /// so the set is EMPTY for every account: an empty set means "no declared
    /// restriction", and the gate admits everything — today's behaviour,
    /// byte-identical. B1d assembles the real set here (the comment above the
    /// return is the single seam); hot management (subscribe/unsubscribe at
    /// runtime) lands in B2.
    fn account_duration_subscriptions(&self, account_id: &AccountId) -> HashSet<(String, i64)> {
        // Test override first (the gate's teeth must stay constructible
        // without a config surface).
        #[cfg(test)]
        if let Some(subs) = &self.test_duration_subscriptions {
            return subs.clone();
        }
        // B1d 装配：真订阅集 = 账户族声明 rounds × venue 绑定。每个声明的
        // duration 与该账户绑定的 venue 组成一对；未绑 venue（空）账户的
        // 声明作用于所有 venue —— 一行 duration 膨胀为当前 live 市场里该
        // duration 的每个 venue（抖动的 venue 集合由 live 市场承载，声明
        // 本身保持 venue 无关）。空 rounds = 未声明任何限制 ⇒ 空表 ⇒ 门
        // 全放行（回放/单族部署的逐字节不变量）。热管理（增族/移族）
        // 落在 B2。
        let Some(account) = self.accounts.account(account_id) else {
            return HashSet::new();
        };
        if account.rounds.is_empty() {
            return HashSet::new();
        }
        let bound_venue = account.venue.as_str();
        let mut out = HashSet::new();
        if !bound_venue.is_empty() {
            for &dur in &account.rounds {
                out.insert((bound_venue.to_string(), dur));
            }
            return out;
        }
        // Unbound: the declaration speaks for every venue the live book
        // currently labels with each declared duration. A venue with no
        // live market at that duration gets no pair — nothing to admit
        // there anyway.
        if let Some(e) = self.engine.as_ref() {
            for m in e.scanner().markets() {
                if m.round_duration_sec > 0 && account.rounds.contains(&m.round_duration_sec) {
                    out.insert((m.venue.clone(), m.round_duration_sec));
                }
            }
        }
        out
    }

    /// Duration-family admission gate (B1c): an order whose (venue, duration)
    /// family is NOT in the account's subscribed family set is refused with
    /// the AccountLimit vocabulary — the same admission dimension family as
    /// `refuse_cross_venue_order` (which binds the venue axis; this binds the
    /// duration axis on top), NOT a new public contract hop (#559/#568's
    /// untouched list is about new hops, not about extending the account
    /// admission chain's existing dimensions).
    ///
    /// Exemptions copy the frozen-gate precedent (the #174 hatch):
    ///  - CLOSE intents always pass — a position admitted when the family was
    ///    subscribed must never be trapped by a later unsubscribe;
    ///  - an account with NO declared family set (the zero-config / pre-B1d
    ///    shape, and today's always-shape) admits everything — an empty set
    ///    is "no restriction declared", not "subscribed to nothing";
    ///  - an order whose token belongs to no labelled live market (the engine
    ///    down, a legacy corpus) carries no family evidence to judge, so it
    ///    passes — whatever refuses it must be another gate with its own
    ///    reason (same stance the venue guard takes for untagged tokens).
    ///
    /// Placement: the `place_gated` chain calls this right AFTER
    /// `refuse_cross_venue_order` and BEFORE the risk/equity gates, so a
    /// family refusal surfaces before notional is reserved against the
    /// ledger. The call convention, the order and the semantics of every
    /// existing gate are untouched.
    fn refuse_unsubscribed_duration(&self, req: &OrderRequest) -> CoreResult<()> {
        // The #174 hatch: closing intents are never family-gated.
        if crate::risk::is_close_intent(&req.internal_key) {
            return Ok(());
        }
        let subs = self.account_duration_subscriptions(&req.account_id);
        if subs.is_empty() {
            // No declared subscription set ⇒ no restriction ⇒ admit. Today's
            // behaviour for every account until B1d wires the real set.
            return Ok(());
        }
        // The venue read is the ORDER'S EXECUTION VENUE: a shared token
        // carries one row per venue, and the placing account's binding names
        // its own row — a token-only read could not pick between the
        // candidates (the same blindness the venue guard above sheds).
        let token_venue = self.execution_venue_of(req.account_id.as_str(), &req.token_id);
        if token_venue.is_empty() {
            // No labelled market, no family evidence — same stance as the
            // venue guard's untagged-token pass.
            return Ok(());
        }
        let duration = self.duration_of_token_for_admission(&req.token_id);
        if subs.contains(&(token_venue.clone(), duration)) {
            return Ok(());
        }
        Err(CoreError::new(
            CoreErrorCode::AccountLimit,
            format!(
                "unsubscribed round family: account `{}` has no (venue, duration) subscription \
                 for (`{token_venue}`, {duration}s) — order's token {} refused (close intents \
                 are exempt)",
                req.account_id, req.token_id
            ),
        ))
    }

    /// The round-length family a token's market declares, in seconds, as the
    /// admission gate reads it. `0` = undeclared: the market carries no
    /// family fact (legacy corpus / the engine down). The gate refuses on the
    /// exact declared value — a `0` can never match a subscribed (venue,
    /// duration) pair, but the unlabelled-market exemption above already
    /// admitted the empty-venue case; a venue-tagged market without a
    /// duration declaration is a B1d concern (the subscription writer must
    /// stamp durations on every row it lists).
    fn duration_of_token_for_admission(&self, token_id: &str) -> i64 {
        self.engine
            .as_ref()
            .and_then(|e| e.market_for_token(token_id))
            .map(|m| m.round_duration_sec)
            .unwrap_or(0)
    }

    /// Test seam for the B1c gate's teeth: stages a subscription set that
    /// [`Self::account_duration_subscriptions`] returns for every account.
    /// The real per-account assembly is B1d's (the Account struct gains its
    /// rounds field there); this override exists so the gate's two verdicts
    /// (subscribed / unsubscribed) are provable TODAY, red-green, before any
    /// config surface exists. Never compiled outside tests — production reads
    /// the constant empty set.
    #[cfg(test)]
    fn stage_duration_subscriptions_for_test(&mut self, subs: HashSet<(String, i64)>) {
        self.test_duration_subscriptions = Some(subs);
    }

    fn place_gated(
        &mut self,
        mut req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
        entry_gates: bool,
    ) -> CoreResult<PlaceOutcome> {
        // An intent the venue just refused waits out its backoff locally —
        // re-POSTing at full tick rate is exactly the rejection storm this
        // guards against. Manual attempts are only paused while the backoff
        // window is running (≤ 30s), never permanently abandoned.
        if let Some(cd) = self.place_cooldowns.get(&req.internal_key)
            && now_ms < cd.next_ok_ms
        {
            return Err(crate::model::CoreError::new(
                blitzkrieg_market_api::CoreErrorCode::RiskRejected,
                format!(
                    "intent {} in rejection cooldown ({}ms left): {}",
                    req.internal_key,
                    cd.next_ok_ms - now_ms,
                    cd.last_error
                ),
            ));
        }
        // E28 Gate 2 (§9.1): the account's posture gates the place BEFORE the
        // risk gates consult equity — only `Active` places new entries; closes
        // pass every posture except `ReadOnly` (a freeze must never trap a
        // position). This is the submission-path enforcement point; the
        // arbitration pipeline carries the same check earlier for strategy
        // intents (defense in depth, one vocabulary: ACCOUNT_LIMIT).
        self.accounts
            .permits_order(&req.account_id, is_close_intent(&req.internal_key))?;
        // Account-scoped kill switch: a frozen account refuses NEW orders at
        // the submission gate (closes pass — the #174 hatch, per account).
        // Manual-orders enforcement point; strategy entries were already
        // dropped in the engine (the frozen-scope filter).
        self.account_freeze_permits(&req)?;
        // G5: a close settles into the book that PAID for the shares — a
        // close order naming another account is refused, never re-routed.
        self.refuse_cross_account_close(&req)?;
        // Account↔venue binding (first batch): an account pinned to venue V
        // may not trade a token that belongs to venue W's round market —
        // money must not leave through a book that was never wired to that
        // venue. Unbound accounts (venue == "") and orders whose token
        // belongs to no labelled live market pass untouched, so the 0.2
        // single-plugin behaviour is bit-identical. The account.switch
        // linkage (switching accounts switching the default plugin) is left
        // for the next batch — the "switch" semantics (session vs process
        // level) are still ambiguous and must not be guessed here.
        self.refuse_cross_venue_order(&req)?;
        // Duration-family admission (B1c): the same account-admission chain
        // extended to the (venue, duration) axis — an order of a round-length
        // family the account has not subscribed is refused (AccountLimit),
        // closes exempt. Until B1d wires the real subscription set no account
        // declares one, so this admits everything today — the hook is placed
        // now, in position, and takes its teeth from B1d's assembly.
        self.refuse_unsubscribed_duration(&req)?;
        // #202: the equity-relative per-order cap is a percentage of the account
        // AT SUBMISSION, so the gate is handed the live balance rather than a
        // remembered copy of it. E28: the account is the one the request names —
        // an unknown account is refused, never implicitly created.
        self.risk
            .check_with_equity(&req, self.accounts.ledger_of(&req.account_id)?.balance())?;
        // #461 Fix 2/4: the systemic nine and the pair-cost cap judge EVERY
        // entry here — manual `orders.place` included, so there is no
        // bypass (the arbitration pipeline runs the same verdict earlier for
        // strategy intents; this is the submission-path enforcement point).
        // `judge_entry`'s Reduce arm shrinks the order in place, exactly as
        // the pipeline's Modification::SizeReduced does for strategy entries;
        // its Reject arm refuses with the gate's own vocabulary.
        if req.side == Side::Buy {
            let limits = self.risk.systemic_snapshot();
            if limits.any_armed() {
                let facts = self.systemic_facts(&req.asset, now_ms);
                let bound_stop_pct = crate::exit_policy::effective_stop_pct(
                    self.config.positions.exit.stop_loss_pct,
                    0,
                    &self.config.positions.exit,
                );
                match crate::risk::limits::judge_entry(
                    limits,
                    &facts,
                    req.price,
                    req.size,
                    bound_stop_pct,
                ) {
                    crate::risk::limits::EntryJudgment::Pass => {}
                    crate::risk::limits::EntryJudgment::Reduce { approved, limit } => {
                        tracing::info!(
                            strategy = %req.strategy,
                            requested = %req.size,
                            approved = %approved,
                            limit,
                            "systemic: entry shrunk at the submission gate"
                        );
                        req.size = approved;
                    }
                    crate::risk::limits::EntryJudgment::Reject { detail, .. } => {
                        return Err(CoreError::new(CoreErrorCode::RiskRejected, detail));
                    }
                }
            }
            // The pair-cost cap: a BUY whose condition already has a
            // counterpart position may not push the pair's PER-SHARE cost
            // past the $1.00 a share-pair redeems (a fresh pair is the
            // strategy's own sub-$1.00 decision; this gate covers the
            // top-up/chase paths). The worst held leg prices the pair: a
            // position's per-share basis is `cost_usd / shares`, and one
            // share of UP plus one share of DOWN redeems exactly $1.
            // 跨账户双腿：分母只看本账户的对手腿（同一账本内赎回 $1，
            // 见 counterpart_pair_basis 的裁决注释）。
            if let Some(paid) = self.counterpart_pair_basis(
                &req.condition_id,
                &req.asset,
                &parse_direction(&req.direction),
                &req.account_id,
            ) {
                let total = paid + req.price;
                if total > Decimal::ONE {
                    return Err(CoreError::new(
                        CoreErrorCode::RiskRejected,
                        format!(
                            "pair-cost cap: counterpart leg basis {paid}/share + buying at {} = \
                             {total} > $1.00 a pair redeems — the merge would lock in a loss",
                            req.price
                        ),
                    ));
                }
            }
        }

        // Entry gates apply to opening BUY orders only; exits (SELL) are never
        // blocked by capacity, breaker or cooldowns.
        if entry_gates && req.side == Side::Buy {
            // A failing accounting audit blocks NEW entries (issue #189). It is
            // deliberately not the kill switch: that also blocks closes (#174), so
            // a drifted ledger would trap the bot in whatever it holds. Only the
            // opening side is refused, and only until an audit passes again.
            self.audit_entry_gate()?;
            // KI-10: only the ORDER'S OWN strategy's streak can block it — a
            // losing leg no longer vetoes every other strategy's entries.
            if self.breaker.is_halted(&req.strategy, now_ms) {
                return Err(CoreError::new(
                    CoreErrorCode::RiskRejected,
                    format!(
                        "breaker active until {} for strategy {}",
                        self.breaker.halted_until_ms(&req.strategy),
                        req.strategy
                    ),
                ));
            }
            let direction = parse_direction(&req.direction);
            if let Err(reason) = self.positions.can_open(
                req.account_id.as_str(),
                Some(&req.asset),
                Some(direction),
                now_ms,
            ) {
                // PAIR-COMPLETION exemption (hold-to-settlement strategies
                // only): the one-position-per-asset gate would reject the
                // second leg of a complete set, but UP + DOWN of the SAME
                // condition is market-neutral — the pair pays $1 at settlement
                // regardless of the outcome, so it is not a doubling of
                // exposure. The override is deliberately narrow: only the
                // specific "Already in {asset}" rejection, only for a strategy
                // that declared hold-to-settlement, only against a position
                // it already holds on the SAME condition — either the
                // OPPOSITE leg (the second leg of a fresh pair) or a TOP-UP
                // of a partially-filled leg toward its counterpart (the
                // gap-fill loop; the opened position itself is the evidence
                // of the incomplete pair). Same-direction stacking across
                // conditions and multi-condition exposure remain rejected,
                // and every other can_open reason (capacity, daily loss,
                // cooldowns) still applies.
                let pair_completion = reason.starts_with("Already in")
                    && self
                        .engine
                        .as_ref()
                        .is_some_and(|e| e.strategy_holds_to_settlement(&req.strategy))
                    && self.positions.open_positions().iter().any(|p| {
                        p.asset == req.asset
                            && p.condition_id == req.condition_id
                            && p.strategy == req.strategy
                            && (p.direction != direction || p.shares < req.size)
                    });
                if !pair_completion {
                    return Err(CoreError::new(CoreErrorCode::RiskRejected, reason));
                }
                tracing::info!(
                    strategy = %req.strategy,
                    asset = %req.asset,
                    direction = req.direction,
                    "pair-completion entry allowed past the one-per-asset gate (hold-to-settlement)"
                );
            }
        }

        let id = self.new_order_id();
        let buy = req.side == Side::Buy;
        let req_account = req.account_id.clone();
        let req_token = req.token_id.clone();

        // Reserve BUY notional up front (prevents over-commitment). E28: the
        // reservation commits cash in the REQUEST'S OWN book.
        if buy {
            self.accounts
                .get_mut(&req.account_id)?
                .reserve(&id, req.price * req.size)?;
        }

        // #351 order lifecycle: the decision is THIS instant; the leg arrives
        // at the venue `taker_latency_ms` later and its report lands
        // `taker_rtt_ms` after that. Zero dials = all three collapse to
        // `now_ms`, the historic immediate-fill behaviour.
        let execute_at = now_ms.saturating_add(self.config.fill_model.taker_latency_ms.max(0));
        let report_at = execute_at.saturating_add(self.config.fill_model.taker_rtt_ms.max(0));

        if let Err(e) = self.ome.submit(SubmitParams {
            decision_at_ms: now_ms,
            execute_at_ms: execute_at,
            report_at_ms: report_at,
            order_id: id.clone(),
            request: req,
            submitted_at_ms: now_ms,
            maker_timeout_ms,
            // B1b: the order's round-length family, resolved three-state off
            // the token's market and the global grid.
            round_duration_sec: self.round_family_of_token(&req_token),
        }) {
            // #537: same rollback contract as the pre-submit path — a submit
            // refusal must not strand the notional it just reserved.
            if buy && let Some(l) = self.accounts.get_mut(&req_account).ok() {
                l.release(&id);
            }
            return Err(e);
        }
        let rejection = self.place_after_submit(&id, maker_timeout_ms, now_ms)?;

        let status = self
            .ome
            .get(&id)
            .map(|o| o.status)
            .unwrap_or(OrderStatus::Pending);
        self.emit_order(&id);
        Ok(PlaceOutcome {
            order_id: id,
            status,
            rejection,
        })
    }

    /// The price a taker leg must state to guarantee a full fill of `size` on
    /// `token`, walked off the mirrored book (best levels first, no worse
    /// than `limit`), plus the volume-weighted price the walk would fill at.
    /// `None` when the book cannot cover the size within `limit` — exactly
    /// what a live FOK would be killed for — or when the book is STALE
    /// (#462): the same `max_book_age_sec` budget the exit ladder prices
    /// under also gates what this walk may authorize. A chase priced off a
    /// quote the feed stopped updating minutes ago is not a fill guarantee,
    /// it is a blind limit; the walk refuses it the same way. Books with no
    /// receive time (`received_ms <= 0`, synthetic fixtures) keep the
    /// historical "age unknown" exemption so dry tests still walk.
    fn marketable_walk(
        &self,
        side: Side,
        token: &str,
        size: Decimal,
        limit: Decimal,
        now_ms: i64,
    ) -> Option<(Decimal, Decimal)> {
        let book = self.books.get(token)?;
        if book.received_ms > 0
            && book.received_ms
                < now_ms.saturating_sub(self.config.positions.exit.max_book_age_sec * 1_000)
        {
            return None;
        }
        book.walk_marketable(side, limit, size)
    }

    /// The chase request's provenance fields, read off the token's live round
    /// market (the same engine registry the panels' Venue column and the
    /// cross-venue binding use): the round slot the missing leg belongs to,
    /// its asset, and its leg label. `None` = the token belongs to no live
    /// market right now (the engine down, a legacy corpus) — the chase caller
    /// falls back to the sweep's own defaults instead of guessing a venue
    /// fact it cannot see.
    fn books_round_slot(&self, token: &str) -> Option<i64> {
        self.engine.as_ref().and_then(|e| e.round_slot_of(token))
    }

    fn books_asset(&self, token: &str) -> Option<String> {
        self.engine
            .as_ref()
            .and_then(|e| e.asset_of(token))
            .map(|a| a.to_string())
    }

    fn books_direction(&self, token: &str) -> Option<String> {
        self.engine
            .as_ref()
            .and_then(|e| e.direction_of(token))
            .map(|d| d.as_str().to_string())
    }

    /// Why a dry/read-only taker leg could not be filled, as the structured
    /// error the submitter gets back (#180).
    ///
    /// Two different causes reach this point — nothing crosses the limit, or not
    /// enough size rests at or inside it — and they are different operational
    /// facts (a quote that moved vs a book too thin for our size), so each gets a
    /// stable leading token a log search, the panel and a gate can match on. Both
    /// carry `WOULD_CROSS`: that is exactly the code a live FOK is killed under
    /// (the venue adapter maps its "no match" / "cross" / "post-only" errors to
    /// it), and a refusal must not invent a second error model.
    fn taker_refusal(&self, order: &TrackedOrder, book: &Book) -> CoreError {
        let (crosses, depth) = book.crossing_depth(order.side, order.price);
        let best = match order.side {
            Side::Buy => book.best_ask().map(|p| format!("best ask {p}")),
            Side::Sell => book.best_bid().map(|p| format!("best bid {p}")),
        }
        .unwrap_or_else(|| {
            format!(
                "no {} rests",
                match order.side {
                    Side::Buy => "ask",
                    Side::Sell => "bid",
                }
            )
        });
        let message = match (order.side, crosses) {
            (Side::Buy, false) => format!(
                "taker_no_crossing_liquidity: no ask at or inside the {} buy limit ({best}; token {}, size {})",
                order.price, order.token_id, order.size
            ),
            (Side::Sell, false) => format!(
                "taker_no_crossing_liquidity: no bid at or inside the {} sell limit ({best}; token {}, size {})",
                order.price, order.token_id, order.size
            ),
            (Side::Buy, true) => format!(
                "taker_insufficient_depth: only {depth} of {} shares rest at or inside the {} buy limit ({best}; token {})",
                order.size, order.price, order.token_id
            ),
            (Side::Sell, true) => format!(
                "taker_insufficient_depth: only {depth} of {} shares rest at or inside the {} sell limit ({best}; token {})",
                order.size, order.price, order.token_id
            ),
        };
        CoreError::new(CoreErrorCode::WouldCross, message)
    }

    /// The dry/read-only half of submission: settle the leg locally, since there
    /// is no venue to report a fill. Returns the kernel-side refusal when the leg
    /// was rejected here, so the submitter learns WHY (#180).
    fn place_after_submit(
        &mut self,
        id: &str,
        maker_timeout_ms: i64,
        now_ms: i64,
    ) -> CoreResult<Option<CoreError>> {
        let order =
            self.ome.get(id).cloned().ok_or_else(|| {
                CoreError::new(CoreErrorCode::Internal, "order missing after submit")
            })?;

        match self.config.mode {
            // Dry and ReadOnly share the settle path: neither has a venue that
            // can report a fill, so waiting would leave the order Pending
            // forever and the ledger would never move. What separates them is
            // egress, not accounting — see `Mode::readonly`.
            Mode::Dry | Mode::ReadOnly => {
                // #351: an in-flight taker has NOT reached the venue yet — it
                // stays Pending (the same shape a LIVE order has until the
                // venue acks) and turns terminal only at its report time.
                let model = self.config.fill_model;
                let inflight = order.mode == FillPolicy::Taker
                    && (model.taker_latency_ms > 0 || model.taker_rtt_ms > 0);
                if !inflight {
                    self.ome.mark_live(id, now_ms)?;
                }
                let mut rejection = None;
                match order.mode {
                    FillPolicy::Taker => {
                        // #351: with the latency dials engaged the leg is IN
                        // FLIGHT — it matches at `execute_at_ms` against the
                        // book as it stands THEN (never the decision-time
                        // quote) and reports at `report_at_ms`. Zero dials
                        // keep the historic immediate fill below, bit-for-bit.
                        let model = self.config.fill_model;
                        if model.taker_latency_ms > 0 || model.taker_rtt_ms > 0 {
                            let arrive = order.execute_at_ms.max(now_ms);
                            self.inflight_takers.push(InflightTaker {
                                order_id: id.to_string(),
                                execute_at_ms: arrive,
                                report_at_ms: order.report_at_ms.max(arrive),
                                matched_price: None,
                                rejected_msg: None,
                            });
                        } else {
                            // Fill like a live FOK: walk the opposing side's resting
                            // levels, best first, no worse than the order's own
                            // limit, and fill at the volume-weighted price. A book
                            // that does not cross or cannot cover the size is what
                            // the venue would kill the order for — reject with the
                            // same economics instead of inventing a fill at a price
                            // nobody was offering. The walk prices the VISIBLE
                            // book; the fill model's slippage dial prices the
                            // impact of our own size beyond it (identity by
                            // default → unchanged economics).
                            let book = self.books.get(&order.token_id).cloned().unwrap_or_default();
                            match book.walk_marketable(order.side, order.price, order.size) {
                                Some((vwap, _)) => {
                                    let fill_price =
                                        self.config.fill_model.apply_slippage(order.side, vwap);
                                    // A FOK is all-or-nothing by construction, so it
                                    // is one execution of the whole size — the trade
                                    // id stays the one this path has always used.
                                    let trade_id = format!("{id}:{now_ms}");
                                    self.authoritative_fill(
                                        id, &trade_id, order.size, fill_price, false, now_ms,
                                    )?
                                }
                                None => {
                                    if order.side == Side::Buy {
                                        // E28: the refused taker's reservation is
                                        // released in the ORDER'S OWN book.
                                        if let Ok(l) = self.accounts.get_mut(&order.account_id) {
                                            l.release(id);
                                        }
                                        // E-VA: the virtual hold releases with it
                                        // (the leg never rested).
                                        self.virtual_release_order(&order);
                                    }
                                    self.ome.mark_terminal(id, OrderStatus::Rejected, now_ms)?;
                                    // RFC 568 阶段一：死亡事实（taker 拒绝）
                                    // 喂配对簿。
                                    self.pair_on_order_event(
                                        &order.internal_key,
                                        Decimal::ZERO,
                                        true,
                                    );
                                    // The refused FOK never rested: its token may
                                    // be re-attempted on a refreshed book.
                                    if let Some(engine) = self.engine.as_mut() {
                                        engine.release_pending_token_for_strategy(
                                            order.account_id.as_str(),
                                            &order.strategy,
                                            &order.token_id,
                                        );
                                    }
                                    // into the one error slot, reaches Node as an
                                    // Event::Error, lands in the log at error level and
                                    // travels back to the submitter on the
                                    // `orders.place` result. Before this, the reject
                                    // branch emitted an order update and nothing else,
                                    // so `orders.place` answered a bare REJECTED and
                                    // `engine.stats.lastError` stayed empty.
                                    let err = self.taker_refusal(&order, &book);
                                    tracing::error!(
                                        order = id,
                                        token = %order.token_id,
                                        side = ?order.side,
                                        size = %order.size,
                                        limit = %order.price,
                                        reason = %err.message,
                                        "order rejected: taker leg has no fillable liquidity"
                                    );
                                    self.emit_error_at(err.clone(), now_ms);
                                    self.emit_order(id);
                                    rejection = Some(err);
                                }
                            }
                        }
                    }
                    FillPolicy::Maker | FillPolicy::MakerThenTaker => {
                        if order.mode == FillPolicy::MakerThenTaker
                            && let Some(delay) = escalation_delay_ms(
                                maker_timeout_ms,
                                self.config.default_maker_timeout_ms,
                            )
                        {
                            self.ome.set_escalation(id, now_ms + delay)?;
                        }
                        // Fill now if the current book already crosses.
                        self.try_maker_fill(id, now_ms);
                    }
                }
                // `emit_order` after the rejection is recorded, so an event
                // consumer that reads stats on the update sees the reason.
                Ok(rejection)
            }
            Mode::Live => {
                // Pending until the venue adapter confirms; the async layer calls
                // mark_live / feeds user-WS fills. Implemented with the CLOB adapter.
                Ok(None)
            }
        }
    }

    /// Apply one authoritative (cumulative) fill and its ledger effect.
    ///
    /// `maker` is the role this execution actually had, which the dry matcher
    /// knows exactly: it either crossed a resting bid (`false`) or rested and was
    /// hit (`true`). Stating it keeps DRY and LIVE on the same authority — the
    /// execution itself — instead of DRY inferring from policy and LIVE reading
    /// the venue.
    ///
    /// `trade_id` is the execution's own identity, and `cumulative` is the size
    /// this identity has now reported — NOT an increment. Two executions of one
    /// order must therefore carry two different ids (a partial fill followed by
    /// a further partial fill is two trades), which is exactly how a venue
    /// reports them; re-reporting the same id is a revision of what was already
    /// booked, not a second trade (issue #178).
    fn authoritative_fill(
        &mut self,
        id: &str,
        trade_id: &str,
        cumulative: Decimal,
        price: Decimal,
        maker: bool,
        now_ms: i64,
    ) -> CoreResult<()> {
        let (token, side, account_id) = match self.ome.get(id) {
            Some(o) => (o.token_id.clone(), o.side, o.account_id.clone()),
            None => return Err(CoreError::new(CoreErrorCode::UnknownOrder, id)),
        };
        let fill = Fill {
            order_id: id.into(),
            trade_id: Some(trade_id.to_string()),
            token_id: token,
            account_id,
            side,
            price,
            size: cumulative,
            status: FillStatus::Confirmed,
            ts_ms: now_ms,
            tx_hash: None,
            maker: Some(maker),
        };
        if let Some(d) = self.ome.apply_fill(fill, now_ms)? {
            self.apply_delta_effects(d, now_ms);
        }
        // Event-stream completeness: a fill applied HERE is outside `place`, so
        // `place`'s trailing `emit_order` cannot cover it. Without this final
        // update the order log / backtester only ever saw the resting LIVE state
        // (the Fill event alone carries no terminal status), so a cross-filled
        // maker read as still-open to every event consumer.
        self.emit_order(id);
        Ok(())
    }

    /// Fill a resting maker order if the latest book crosses its limit.
    ///
    /// The fill is bounded by the crossing side's DEPTH, not by the order's own
    /// size (issue #183): a maker trades only against the volume that actually
    /// reaches its limit, so an order bigger than the book leaves the rest
    /// resting — partially filled — instead of pretending the whole size
    /// traded. The depth reading is the taker walk's own
    /// ([`crate::sim::Book::marketable_depth`]), so dry cannot hold two
    /// different ideas of how much is available.
    ///
    /// #464: a maker that crosses the book AT SUBMISSION is refused here, not
    /// filled. Live sends a post-only limit and the venue answers WouldCross
    /// (`map_sdk_err`, extensions/polymarket/src/venue.rs) — filling the same
    /// order at its own limit with a zero maker fee in dry handed the
    /// simulation maker economics for what live would have paid a taker fee
    /// to cross. The refusal routes through the SAME live machinery:
    /// `reject_live_result` releases the reservation, and the MakerThenTaker
    /// branch there chases at the book's real price (`escalation_guard` +
    /// `place_escalated`), so both modes converge on one path. A maker that
    /// crosses only LATER (rested first, then hit) is the honest maker fill
    /// and still fills below.
    ///
    /// #351: advance the in-flight taker legs. Runs at every clock advance —
    /// the head of [`Self::engine_on_data`] (BEFORE the incoming book is
    /// mirrored, so a match can never see the event that triggered the drain)
    /// and in [`Self::tick`]. Both call sites observe the invariant that the
    /// mirrored book only holds events ≤ the current clock, so a match is
    /// priced strictly off data the venue could have had at `execute_at_ms`
    /// or earlier — no future data, by construction.
    ///
    /// Stage 1 (match, once per leg at the first drain with
    /// `now >= execute_at_ms`): the FOK walks the CURRENT mirrored book at the
    /// order's decision-time limit; a book that no longer crosses or cannot
    /// cover the size is a kill — exactly what a live venue does to a late
    /// FOK. Stage 2 (report, `now >= report_at_ms`): the fill books with that
    /// instant (ledger/position effects land at report time), or the kill
    /// releases the BUY reservation.
    fn drain_inflight_takers(&mut self, now_ms: i64) {
        if self.inflight_takers.is_empty() {
            return;
        }
        let model = self.config.fill_model;
        let legs = std::mem::take(&mut self.inflight_takers);
        let mut remaining = Vec::new();
        for mut leg in legs {
            // Stage 1: match at the venue.
            if leg.matched_price.is_none() && leg.rejected_msg.is_none() {
                if now_ms < leg.execute_at_ms {
                    remaining.push(leg);
                    continue;
                }
                let order = match self.ome.get(&leg.order_id).cloned() {
                    Some(o) if o.status.is_live() => o,
                    // Cancelled/terminal while in flight: the venue never
                    // filled it (the cancel won the race) — drop the leg.
                    _ => continue,
                };
                let book = self.books.get(&order.token_id).cloned().unwrap_or_default();
                match book.walk_marketable(order.side, order.price, order.size) {
                    Some((vwap, _)) => {
                        leg.matched_price = Some(model.apply_slippage(order.side, vwap));
                    }
                    None => leg.rejected_msg = Some(self.taker_refusal(&order, &book).message),
                }
            }
            // Stage 2: report at the local books.
            if now_ms < leg.report_at_ms {
                remaining.push(leg);
                continue;
            }
            if let Some(price) = leg.matched_price {
                let order = match self.ome.get(&leg.order_id) {
                    // A cancel that won the race between match and report
                    // leaves the order terminal: the venue never reports a
                    // fill for an order it was told to forget.
                    Some(o) if o.status.is_live() => o.clone(),
                    Some(_) => continue,
                    None => continue,
                };
                let trade_id = format!("{}:{}", order.order_id, leg.execute_at_ms);
                if let Err(e) = self.authoritative_fill(
                    &order.order_id,
                    &trade_id,
                    order.size,
                    price,
                    false,
                    leg.report_at_ms,
                ) {
                    tracing::error!(
                        order = %order.order_id,
                        error = %e,
                        "inflight taker fill could not be applied"
                    );
                    self.emit_error_at(e, leg.report_at_ms);
                }
            } else if let Some(msg) = leg.rejected_msg.take() {
                let order = match self.ome.get(&leg.order_id) {
                    Some(o) => o.clone(),
                    None => continue,
                };
                if order.side == Side::Buy {
                    // E28: the killed FOK's reservation is released in the
                    // ORDER'S OWN book.
                    if let Ok(l) = self.accounts.get_mut(&order.account_id) {
                        l.release(&order.order_id);
                    }
                    // E-VA: the virtual hold goes with it.
                    self.virtual_release_order(&order);
                }
                let _ = self.ome.mark_terminal(
                    &order.order_id,
                    OrderStatus::Rejected,
                    leg.report_at_ms,
                );
                // RFC 568 阶段一：死亡事实（in-flight taker 被杀）喂配对簿。
                self.pair_on_order_event(&order.internal_key, Decimal::ZERO, true);
                let err = CoreError::new(CoreErrorCode::WouldCross, msg);
                tracing::error!(
                    order = %order.order_id,
                    reason = %err.message,
                    "inflight taker killed at execute_at (book no longer crosses)"
                );
                self.emit_error_at(err, leg.report_at_ms);
                self.emit_order(&order.order_id);
            }
        }
        self.inflight_takers = remaining;
    }

    /// Fill a resting maker order if the latest book crosses its limit.
    ///
    /// The fill is bounded by the crossing side's DEPTH, not by the order's own
    /// size (issue #183): a maker trades only against the volume that actually
    /// reaches its limit, so an order bigger than the book leaves the rest
    /// resting — partially filled — instead of pretending the whole size
    /// traded. The depth reading is the taker walk's own
    /// ([`crate::sim::Book::marketable_depth`]), so dry cannot hold two
    /// different ideas of how much is available.
    ///
    /// #464: a maker that crosses the book AT SUBMISSION is refused here, not
    /// filled. Live sends a post-only limit and the venue answers WouldCross
    /// (`map_sdk_err`, extensions/polymarket/src/venue.rs) — filling the same
    /// order at its own limit with a zero maker fee in dry handed the
    /// simulation maker economics for what live would have paid a taker fee
    /// to cross. The refusal routes through the SAME live machinery:
    /// `reject_live_result` releases the reservation, and the MakerThenTaker
    /// branch there chases at the book's real price (`escalation_guard` +
    /// `place_escalated`), so both modes converge on one path. A maker that
    /// crosses only LATER (rested first, then hit) is the honest maker fill
    /// and still fills below.
    fn try_maker_fill(&mut self, id: &str, now_ms: i64) {
        let Some(order) = self.ome.get(id).cloned() else {
            return;
        };
        if !order.status.is_live() || order.filled_size >= order.size {
            return;
        }
        let Some(book) = self.books.get(&order.token_id) else {
            return;
        };
        if !book.crosses(&order) {
            return;
        }
        // #464 (see above): the venue would have refused the resting order —
        // post-only may not cross. Only the SUBMISSION instant is gated, so a
        // maker that rested and is hit by a later tick keeps its fill path
        // untouched (bit-for-bit for every order that did not cross on POST).
        // CLOSE intents are exempt: live refuses their POST too (the
        // WouldCross escalation branch is entry-only), so the honest mirror
        // of live is the exit runner's own reject→backoff→retry loop, not a
        // dry refusal that would strand the position with no closer at all.
        if order.submitted_at_ms == now_ms && !is_close_intent(&order.internal_key) {
            let side_word = if order.side == Side::Buy {
                "buy"
            } else {
                "sell"
            };
            self.reject_live_result(
                id,
                Some(CoreError::new(
                    CoreErrorCode::WouldCross,
                    format!(
                        "post-only order would cross: {side_word} {} @ {} (the venue refuses \
                         this POST; a MakerThenTaker entry escalates to taker on the same book)",
                        order.size, order.price
                    ),
                )),
                now_ms,
            )
            .ok();
            return;
        }
        let depth = book.marketable_depth(order.side, order.price);
        // Fill model (P-1.2), identity by default so this path is unchanged:
        //  - latency: the order cannot be hit before the venue could have it;
        //  - fill probability: a crossing does not guarantee a fill (queue
        //    position) — a deterministic per-order draw keeps replays exact.
        let model = self.config.fill_model;
        if now_ms < model.maker_eligible_at_ms(order.submitted_at_ms) {
            return;
        }
        if !model.maker_fill_wins(&order.order_id) {
            return;
        }
        let remaining = (order.size - order.filled_size).max(Decimal::ZERO);
        let chunk = model.maker_fill_size(&order.order_id, remaining, depth);
        if chunk <= Decimal::ZERO {
            // The crossing side offers nothing to take: the order keeps resting.
            return;
        }
        // A maker still fills at its own resting limit (Node parity); only the
        // SIZE is bounded by the book. Each chunk is its own trade identity, so
        // a later chunk books on top of this one instead of revising it.
        let trade_id = format!("{id}:{now_ms}:{}", order.filled_size + chunk);
        if let Err(e) = self.authoritative_fill(id, &trade_id, chunk, order.price, true, now_ms) {
            // Through the one error slot like every other internal failure: a fill
            // the OME refuses is exactly the kind of thing an operator must see
            // (#180/#184 rather than a bare Event::Error).
            tracing::error!(order = id, error = %e, "maker fill could not be applied");
            self.emit_error_at(e, now_ms);
        }
    }

    pub fn cancel(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        let order = self
            .ome
            .get(id)
            .cloned()
            .ok_or_else(|| CoreError::new(CoreErrorCode::UnknownOrder, id))?;
        let was_live = order.status.is_live();
        if order.side == Side::Buy {
            // E28: the reservation is released in the ORDER'S OWN book — a
            // cancel must never refund another account's cash.
            if let Ok(l) = self.accounts.get_mut(&order.account_id) {
                l.release(id);
            }
            // E-VA: the same precision for the strategy's VIRTUAL pot — the
            // cancelled order's whole remaining hold leaves the book.
            self.virtual_release_order(&order);
        }
        self.ome.mark_terminal(id, OrderStatus::Cancelled, now_ms)?;
        // RFC 568 阶段一：死亡事实（撤单）喂配对簿。
        self.pair_on_order_event(&order.internal_key, Decimal::ZERO, true);
        // The token's repeat-suppression ends with the order: a cancelled
        // entry leaves no resting order behind, so a later cycle may re-attempt
        // (the strategy's own latch still bounds it to one attempt per book).
        if was_live && let Some(engine) = self.engine.as_mut() {
            engine.release_pending_token_for_strategy(
                order.account_id.as_str(),
                &order.strategy,
                &order.token_id,
            );
        }
        // Replay fast path: a live order just left the book — the escalation
        // sweep and the audit input changed.
        if was_live && self.replay_sched() {
            self.fast_orders_dirty = true;
        }
        // The venue may still hold the order resting: hand its id to the live
        // bridge for a real cancel. Nothing to cancel when the venue never
        // acknowledged the order (no id — the venue never heard of it).
        if was_live && let Some(vid) = order.venue_order_id {
            let venue = self.execution_venue_of(order.account_id.as_str(), &order.token_id);
            self.queue_venue_cancel(venue, vid);
        }
        self.emit_order(id);
        Ok(())
    }

    /// Enqueue one venue cancel (deduped by id, bounded so a pathological loop
    /// cannot grow it without bound). The live bridge drains it every tick.
    /// `venue` labels the entry for the per-venue executor drain (`""` =
    /// untagged — routes to the active plugin's drain, the single-plugin
    /// behaviour).
    fn queue_venue_cancel(&mut self, venue: String, venue_order_id: String) {
        if !self
            .pending_venue_cancels
            .iter()
            .any(|(_, id)| id == &venue_order_id)
        {
            self.pending_venue_cancels.push((venue, venue_order_id));
            if self.pending_venue_cancels.len() > 256 {
                self.pending_venue_cancels.remove(0);
            }
        }
    }

    /// Drain the venue-cancel queue: once. The bridge must forward each id as
    /// a real `DELETE /order` — see [`Self::cancel`].
    pub fn take_pending_venue_cancels(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_venue_cancels)
            .into_iter()
            .map(|(_, id)| id)
            .collect()
    }

    /// The multi-plugin drain: every cancel id whose order's round market
    /// names `venue`, plus the UNTAGGED ids when `venue` is the host's active
    /// plugin (the single-plugin fallback the orders drain uses). Take-once
    /// per entry: a sibling executor never sees an id this drain returned.
    pub fn take_pending_venue_cancels_for_venue(
        &mut self,
        venue: &str,
        untagged_fallback: bool,
    ) -> Vec<String> {
        let mut taken = Vec::new();
        let mut rest = Vec::new();
        for (v, id) in std::mem::take(&mut self.pending_venue_cancels) {
            if v == venue || (untagged_fallback && v.is_empty()) {
                taken.push(id);
            } else {
                rest.push((v, id));
            }
        }
        self.pending_venue_cancels = rest;
        taken
    }

    /// E31-c: retire the RESTING remainder of a partially filled order while
    /// keeping the already-filled part in the position book. The strategy
    /// decides WHEN; the kernel guarantees the venue-side cancel rides along.
    pub fn cancel_remaining(&mut self, order_id: &str, now_ms: i64) -> CoreResult<()> {
        self.cancel(order_id, now_ms)
    }

    /// E31-c: flatten the position behind an order — sell every share the
    /// fills have accrued so far, at that position's latest valuation.
    pub fn close_filled(&mut self, order_id: &str, now_ms: i64) -> CoreResult<usize> {
        let token = self
            .ome
            .get(order_id)
            .map(|o| o.token_id.clone())
            .ok_or_else(|| CoreError::new(CoreErrorCode::UnknownOrder, order_id.to_string()))?;
        let position = self
            .positions
            .open_positions()
            .iter()
            .find(|p| p.token_id == token)
            .map(|p| p.id.clone())
            .ok_or_else(|| {
                CoreError::new(
                    CoreErrorCode::UnknownOrder,
                    format!("no open position behind order {order_id}"),
                )
            })?;
        self.flatten(Some(&position), now_ms)
    }

    pub fn cancel_all(&mut self, token_id: Option<&str>, now_ms: i64) -> CoreResult<usize> {
        let ids: Vec<String> = self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| token_id.map(|t| o.token_id == t).unwrap_or(true))
            .map(|o| o.order_id.clone())
            .collect();
        for id in &ids {
            self.cancel(id, now_ms)?;
        }
        Ok(ids.len())
    }

    /// Force-close positions: a specific id, or all when id is None. Returns how
    /// many were closed. Exits are SELL orders; in dry mode they cross at the
    /// latest book bid, in live mode the venue bridge submits them.
    pub fn flatten(&mut self, position_id: Option<&str>, now_ms: i64) -> CoreResult<usize> {
        let targets: Vec<FlattenTarget> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| position_id.map(|id| p.id == id).unwrap_or(true))
            .map(|p| {
                (
                    p.id.clone(),
                    p.token_id.clone(),
                    p.condition_id.clone(),
                    p.shares,
                    p.strategy.clone(),
                    p.asset.clone(),
                    p.direction.as_str().to_string(),
                    p.current_price,
                    // E28 (§9.2): the manual close spends from the position's
                    // own account.
                    p.account_id.clone(),
                )
            })
            .collect();
        let mut closed = 0usize;
        for (id, token, condition, shares, strategy, asset, direction, _current, account_id) in
            targets
        {
            // Already have a live sell for this token? skip.
            if self
                .ome
                .live_for(&token, Side::Sell)
                .into_iter()
                .any(|o| o.status.is_live())
            {
                continue;
            }
            // Price the exit off the CURRENT book, not the stale valuation:
            // `current_price` is a mid ≈ what the shares are worth, not what
            // a resting bid is offering, so an FOK there never crosses live.
            // The walk names the worst bid level that covers the size. With
            // no bids at all nothing can fill — the request falls back to the
            // grid floor and the settle/venue rejects it honestly.
            let price = self
                .marketable_walk(Side::Sell, &token, shares, Decimal::new(1, 2), now_ms)
                .map(|(_, worst)| worst)
                .filter(|p| *p > Decimal::ZERO)
                .unwrap_or(Decimal::new(1, 2));
            self.note_exit_reason(&token, ExitReason::Manual);
            let order = OrderRequest {
                token_id: token,
                condition_id: condition,
                side: Side::Sell,
                mode: FillPolicy::Taker,
                price,
                size: shares,
                internal_key: format!("flatten:{id}"),
                strategy,
                asset,
                direction,
                round_slot: 0,
                // E28 (§9.2): the closing sell spends from the position's own
                // account — a cross-account flatten must never touch another
                // book.
                account_id,
            };
            if let Err(e) = self.place(order, 0, now_ms) {
                self.emit_error(e);
            } else {
                closed += 1;
            }
        }
        Ok(closed)
    }

    // ── Market data (P0 bridge from Node; P3 ingests inside the core) ───────
    pub fn book_snapshot(
        &mut self,
        token_id: &str,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
        now_ms: i64,
    ) {
        self.books.insert(
            token_id.to_string(),
            Book {
                bids,
                asks,
                received_ms: now_ms,
            },
        );
        let ids: Vec<String> = self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| o.token_id == token_id && rests_on_book(o.mode))
            .map(|o| o.order_id.clone())
            .collect();
        for id in ids {
            self.try_maker_fill(&id, now_ms);
        }
    }

    // ── Settlement & redemption (issue #175) ────────────────────────────────
    //
    // A position that survives to its market's resolution used to sit in the
    // book forever: never closed, never paid, with its cash permanently short.
    // This section closes it at the resolution's payout price and books that
    // payout as a RECEIVABLE — never as cash. The distinction is the whole
    // design: the wallet does not hold the money until the on-chain redemption
    // confirms, and crediting `balance` early would make the ledger claim cash
    // the venue does not have, which is exactly what the live leg of the
    // accounting audit reads as divergence (and halts entries over). The
    // receivable is part of `CashIdentity::total`, so the anchored identity
    // stays true at every instant: settlement moves the payout into `total`
    // against the realized PnL of the close, redemption moves it from the
    // receivable into `balance` without changing `total`.
    //
    // Booking is idempotent on two levels, because a settlement is real money:
    // an in-memory applied set (this process) and an append-only journal whose
    // replayed lines rebuild that set (every later process).

    /// Markets the core is waiting on a verdict for, as queries for the venue.
    /// The plugin drains this in live mode; dry mode answers itself in
    /// [`Core::tick`], so the same code path is exercised without a chain.
    pub fn take_settlement_queries(
        &mut self,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::SettlementQuery> {
        self.sync_settlement_watch(now_ms);
        self.settlement.take_queries(now_ms)
    }

    /// Keep the settlement watch list in step with the open book: a market is
    /// watched once a position of ours is past its expiry, and forgotten as soon
    /// as its last position leaves (settled, or closed by an ordinary exit).
    fn sync_settlement_watch(&mut self, now_ms: i64) {
        // Replay fast path: the watch list only changes when the position set
        // changes, or when the earliest open position's expiry passes (that is
        // the instant `track_markets` would first admit the market). Between
        // those, the scan re-derives the same list — skip it.
        if self.replay_sched() && !self.fast_positions_dirty && now_ms < self.fast_next_expiry_ms {
            return;
        }
        // #377 (治 #2): markets the archive promised a verdict for are settled
        // by the stream (`DataEvent::Resolution`), so the dry ladder must not
        // query them — a stamped query would wait for an answer that never
        // comes and trip the blind alert. Filtering here removes them from
        // tracking AND from the forget loop's live set (self-healing: a market
        // that slipped into the watch list is dropped on the next sync).
        let all: Vec<crate::position::OpenPosition> = self.positions.open_positions().to_vec();
        let positions: Vec<crate::position::OpenPosition> = all
            .iter()
            .filter(|p| !self.archive_verdicts.contains(&p.condition_id))
            .cloned()
            .collect();
        self.settlement.track_markets(&positions, now_ms);
        let live: HashSet<&str> = positions.iter().map(|p| p.condition_id.as_str()).collect();
        for condition_id in self.settlement.tracked_condition_ids() {
            if !live.contains(condition_id.as_str()) {
                self.settlement.forget_market(&condition_id);
            }
        }
        if self.replay_sched() {
            self.fast_positions_dirty = false;
            // The next expiry wake is the earliest expiry still IN THE
            // FUTURE. An already-expired position needs no wake: it is
            // either tracked (its re-query clock is `next_query_deadline_ms`,
            // a separate term of the settlement gate) or untrackable (the
            // `EXPIRY_TRUST_MS` refusal — re-deriving that refusal every
            // tick produces nothing). Taking the raw minimum kept this gate
            // permanently open once the earliest expiry had passed: the
            // busy-phase sample put `take_settlement_queries` at 95% of
            // tick, nearly all of it this watch-list re-derivation cloning
            // every open position per tick. Computed over the UNFILTERED
            // set: an archive-verdict position's expiry must still wake the
            // sync (its settlement event arrives around then), and letting
            // the minimum collapse to `i64::MAX` would reopen this gate on
            // every tick — the exact hot path #375 closed.
            self.fast_next_expiry_ms = all
                .iter()
                .map(|p| p.expires_at_ms)
                .filter(|t| *t > now_ms)
                .min()
                .unwrap_or(i64::MAX);
        }
    }

    /// A market's resolution arrived from the venue (live) or was synthesized by
    /// the core (dry). Closes every position held on that market exactly once and
    /// books the payout as a receivable.
    ///
    /// Loud on anything it cannot settle: a position whose token the resolution
    /// does not price stays OPEN (inventing a payout would be worse than the
    /// drift it fixes) and the market stays watched.
    pub fn on_market_resolution(
        &mut self,
        resolution: blitzkrieg_market_api::MarketResolution,
        now_ms: i64,
    ) {
        self.settlement
            .note_answer(&resolution.condition_id, resolution.resolved, now_ms);
        if !resolution.resolved {
            return; // not resolved yet — the query re-arms on its own cadence
        }
        let mut market_positions: Vec<crate::position::OpenPosition> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.condition_id == resolution.condition_id)
            .cloned()
            .collect();
        // Paying legs first: the market's claim must exist before a worthless leg
        // joins it, because a NegRisk redemption takes the whole outcome-share
        // vector in one call (a leg booked first could not be added afterwards).
        market_positions.sort_by_key(|p| {
            std::cmp::Reverse(
                resolution
                    .payout_per_share(&p.token_id)
                    .unwrap_or(Decimal::ZERO),
            )
        });
        if market_positions.is_empty() {
            self.settlement.forget_market(&resolution.condition_id);
            return;
        }
        let mut settled = 0usize;
        let mut payout_usd = Decimal::ZERO;
        let mut unpriced: Vec<String> = Vec::new();
        for pos in &market_positions {
            let Some(booking) = booking_for(pos, &resolution) else {
                unpriced.push(format!("{} (position {})", pos.token_id, pos.id));
                continue;
            };
            // Commit BEFORE closing. The journal line is what makes the close
            // replay-safe (and `book` returns None for a key it already holds, so
            // a re-delivered resolution cannot book the same position twice).
            let Some(committed) = self.settlement.book(&booking, &resolution, now_ms) else {
                continue;
            };
            payout_usd += committed.payout_usd;
            // A redemption through the CTF pays no fee, so the settlement exit is
            // a maker close: `close` charges no exit fee and the realized net is
            // `payout − entry cost − entry fee` — exactly the movement the
            // identity's receivable records.
            let closed = self.positions.close(
                &committed.position_id,
                committed.payout_per_share,
                SETTLEMENT_EXIT_REASON,
                true,
                now_ms,
            );
            match closed {
                Some(closed) => {
                    // Trade record first, then the position snapshot: recovery
                    // needs the realized record to already exist, because the
                    // audit's expected side is built from it (see
                    // `SettlementBook::recover`).
                    self.on_position_closed(&closed, now_ms);
                    self.settlement.note_closed(&committed.key, now_ms);
                    self.persist_positions();
                    settled += 1;
                    tracing::info!(
                        position = %committed.position_id,
                        condition = %committed.condition_id,
                        shares = %committed.shares,
                        payout_per_share = %committed.payout_per_share,
                        payout_usd = %committed.payout_usd,
                        winning = committed.winning,
                        source = %resolution.source,
                        "position settled at market resolution"
                    );
                }
                None => {
                    // The booking is committed but the position was not in the
                    // book (an external close raced it). Nothing to close, and
                    // the claim stands: the tokens are ours on-chain whatever the
                    // local book says.
                    tracing::warn!(
                        position = %committed.position_id,
                        "settlement booked but the position was already closed locally"
                    );
                }
            }
        }
        if settled > 0 {
            self.persist_positions();
            self.emit(Event::RiskAlert {
                code: blitzkrieg_market_api::CoreErrorCode::Internal,
                message: format!(
                    "settled {settled} position(s) on {} — {payout_usd} USDC receivable pending redemption",
                    resolution.condition_id
                ),
            });
        }
        if !unpriced.is_empty() {
            let message = format!(
                "settlement: resolution of {} (source {}) prices no outcome for {} — left open",
                resolution.condition_id,
                resolution.source,
                unpriced.join(", ")
            );
            tracing::error!("{message}");
            // Through the one error slot as well: a market that cannot be priced
            // is why positions sit unresolved, and that must be visible in
            // `engine.stats` without reading logs (#180).
            self.emit_error(CoreError::new(
                blitzkrieg_market_api::CoreErrorCode::Internal,
                message,
            ));
            return; // keep the market watched: a later resolution may price it
        }
        self.settlement.forget_market(&resolution.condition_id);
    }

    /// Claims due for a redemption attempt. The plugin drains this in live mode
    /// and reports each one back through `on_redemption_result`; dispatching
    /// stamps the claim's backoff, so a result that never arrives is retried
    /// rather than forgotten.
    pub fn take_pending_redemptions(
        &mut self,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::RedemptionRequest> {
        self.settlement.take_redemptions(now_ms)
    }

    /// #647: the per-venue redemption drain for the host's multi-plugin
    /// executor pulls. Each request carries its own venue (the claim's
    /// position-side execution-venue stamp), and this returns exactly the
    /// claims of `venue` — plus the UNTAGGED claims when `untagged_fallback`
    /// (the requesting plugin is the host's active one), matching the orders
    /// drain's single-plugin fallback. A sibling executor never sees a claim
    /// this drain handed out (the dispatch stamp is take-once).
    pub fn take_pending_redemptions_for_venue(
        &mut self,
        venue: &str,
        untagged_fallback: bool,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::RedemptionRequest> {
        self.settlement
            .take_redemptions_for_venue(venue, untagged_fallback, now_ms)
    }

    /// A redemption attempt came back.
    ///
    /// Success is the exact instant the payout becomes cash: the claim leaves the
    /// receivable and the same amount enters the ledger balance, so the audit's
    /// `total` is unchanged by it. Failure is loud, is retried on the book's
    /// backoff, and never blocks a close or a new entry — this path touches
    /// neither the risk gate nor the order manager.
    pub fn on_redemption_result(
        &mut self,
        result: blitzkrieg_market_api::RedemptionResult,
        now_ms: i64,
    ) {
        match result.failure {
            None => {
                let Some((account_id, payout)) = self.settlement.note_confirmed(
                    &result.id,
                    result.tx_hash.clone(),
                    result.block_number,
                    now_ms,
                ) else {
                    // Unknown or already-redeemed claim: crediting here is how a
                    // phantom payout would enter the books, so it does not.
                    tracing::warn!(
                        claim = %result.id,
                        "redemption success for an unknown or already-redeemed claim — ignored"
                    );
                    return;
                };
                // §9.2: the payout lands in the CLAIM's own account — the claim
                // carried `account_id` from the position at booking time, the
                // same attribution the merge path already lives by. An empty
                // account (a settlement journal written before the field
                // existed) and an unknown account both fall back to the ACTIVE
                // book, the 0.3 behaviour, loudly: the cash has no address of
                // its own, so it goes where the process funder says — visible
                // in the log, never silently swallowed.
                if account_id.is_empty() {
                    self.accounts
                        .active_ledger_mut()
                        .credit_redemption(payout, &result.id);
                } else {
                    let a = AccountId::from(account_id.as_str());
                    match self.accounts.get_mut(&a) {
                        Ok(l) => l.credit_redemption(payout, &result.id),
                        Err(e) => {
                            tracing::warn!(
                                claim = %result.id,
                                account = %account_id,
                                error = %e,
                                "redemption claim names an unconfigured account — payout credited to the active book (0.3 fallback)"
                            );
                            self.accounts
                                .active_ledger_mut()
                                .credit_redemption(payout, &result.id);
                        }
                    }
                }
                tracing::info!(
                    claim = %result.id,
                    condition = %result.condition_id,
                    account = %account_id,
                    payout_usd = %payout,
                    tx = result.tx_hash.as_deref().unwrap_or(""),
                    block = result.block_number.unwrap_or_default(),
                    "settled claim redeemed on-chain — payout credited to cash"
                );
                self.emit(Event::RiskAlert {
                    code: blitzkrieg_market_api::CoreErrorCode::Internal,
                    message: format!(
                        "redeemed settled position: +{payout} USDC on-chain (claim {}, tx {})",
                        result.id,
                        result.tx_hash.as_deref().unwrap_or("n/a")
                    ),
                });
            }
            Some(failure) => {
                let what = if failure.manual {
                    "manual redemption required"
                } else {
                    "will retry"
                };
                let message = format!(
                    "redemption of claim {} failed ({what}): {}",
                    result.id, failure.message
                );
                if !self.settlement.note_failure(&result.id, &failure, now_ms) {
                    tracing::error!(
                        claim = %result.id,
                        error = %failure.message,
                        "redemption failed for an unknown or already-redeemed claim"
                    );
                } else {
                    tracing::error!(
                        claim = %result.id,
                        manual = failure.manual,
                        error = %failure.message,
                        "redemption failed"
                    );
                }
                eprintln!("core: {message}");
                // Through the one slot as well: a claim whose collateral is stuck
                // is money the operator is waiting on (#180).
                self.emit_error(CoreError::new(
                    blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message,
                ));
            }
        }
    }

    /// Claims due for a merge attempt. The plugin drains this in live mode and
    /// reports each one back through `on_merge_result`; dispatching stamps the
    /// claim's backoff, so a result that never arrives is retried rather than
    /// forgotten. Mirrors `take_pending_redemptions`.
    pub fn take_pending_merges(&mut self, now_ms: i64) -> Vec<blitzkrieg_market_api::MergeRequest> {
        self.settlement.take_merges(now_ms)
    }

    /// #647: the per-venue merge drain for the host's multi-plugin executor
    /// pulls. Routed exactly like
    /// [`Self::take_pending_redemptions_for_venue`] — own-venue claims, plus
    /// untagged claims when `untagged_fallback` — so a second venue's executor
    /// can never drain a foreign bridge's merge.
    pub fn take_pending_merges_for_venue(
        &mut self,
        venue: &str,
        untagged_fallback: bool,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::MergeRequest> {
        self.settlement
            .take_merges_for_venue(venue, untagged_fallback, now_ms)
    }

    /// #461: a merge attempt came back. The SAME instant semantics as a
    /// redemption — confirmation is the exact moment the receivable becomes
    /// cash, on the account whose wallet held the burned pairs (the claim
    /// carries the account; E28 §9.2). An unknown claim's confirmation is
    /// ignored, never credited — crediting it is how a phantom payout enters
    /// the books.
    pub fn on_merge_result(&mut self, result: blitzkrieg_market_api::MergeResult, now_ms: i64) {
        match result.failure {
            None => {
                let Some((account_id, usd)) = self.settlement.note_merge_confirmed(
                    &result.id,
                    result.tx_hash.clone(),
                    result.block_number,
                    now_ms,
                ) else {
                    tracing::warn!(
                        claim = %result.id,
                        "merge success for an unknown or already-confirmed claim — ignored"
                    );
                    return;
                };
                match self.accounts.get_mut(&AccountId::from(account_id.as_str())) {
                    Ok(l) => {
                        l.credit_merge(usd, &result.id);
                        tracing::info!(
                            claim = %result.id,
                            condition = %result.condition_id,
                            account = %account_id,
                            usd = %usd,
                            tx = result.tx_hash.as_deref().unwrap_or(""),
                            "merge confirmed on-chain — collateral credited to cash"
                        );
                        self.emit(Event::RiskAlert {
                            code: blitzkrieg_market_api::CoreErrorCode::Internal,
                            message: format!(
                                "merge confirmed: +{usd} USDC on-chain (claim {}, tx {})",
                                result.id,
                                result.tx_hash.as_deref().unwrap_or("n/a")
                            ),
                        });
                    }
                    Err(e) => {
                        // The claim's cash has no book to land in — louder than a
                        // mint, and not silently swallowed: the receivable was
                        // already removed, so this IS the last chance to say.
                        tracing::error!(
                            account = %account_id,
                            error = %e,
                            "merge confirmation dropped: unknown account — collateral NOT credited"
                        );
                        self.emit_error(CoreError::new(
                            blitzkrieg_market_api::CoreErrorCode::Internal,
                            format!(
                                "merge {} confirmed {usd} USDC for unknown account {account_id} — cash NOT credited",
                                result.id
                            ),
                        ));
                    }
                }
            }
            Some(failure) => {
                let what = if failure.manual {
                    "manual merge required"
                } else {
                    "will retry"
                };
                let message = format!(
                    "merge of claim {} failed ({what}): {}",
                    result.id, failure.message
                );
                if !self
                    .settlement
                    .note_merge_failure(&result.id, &failure, now_ms)
                {
                    tracing::error!(
                        claim = %result.id,
                        error = %failure.message,
                        "merge failed for an unknown or already-confirmed claim"
                    );
                } else {
                    tracing::error!(
                        claim = %result.id,
                        manual = failure.manual,
                        error = %failure.message,
                        "merge failed"
                    );
                }
                eprintln!("core: {message}");
                self.emit_error(CoreError::new(
                    blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message,
                ));
            }
        }
    }

    /// Dry mode: simulate the redemption of every due claim. The claim is
    /// confirmed with a `dry-simulated` tx hash, so the ledger move is the same
    /// one a mined transaction produces and the whole path (settle → receivable →
    /// cash) is testable with no chain at all.
    ///
    /// `config.dry_redeem_fail` (test hook, off by default) makes the first N
    /// attempts of each claim fail instead. The failure travels the production
    /// path — `on_redemption_result` → `note_failure` → backoff, or a permanent
    /// stop when it is `manual` — so a gate can drive the retry clock and the
    /// receivable with no venue to fail for it. The count is per claim (its
    /// `attempts` counter, which the dispatch stamps), so the retry that follows
    /// a backoff is attempt 2 and lands.
    fn simulate_redemptions(&mut self, now_ms: i64) {
        for request in self.settlement.take_redemptions(now_ms) {
            let attempts = self
                .settlement
                .claim(&request.id)
                .map(|c| c.attempts)
                .unwrap_or(0);
            let failure = (attempts <= self.config.dry_redeem_fail).then(|| {
                blitzkrieg_market_api::RedemptionFailure {
                    message: format!(
                        "dry-simulated redemption failure (attempt {attempts} of {} injected)",
                        self.config.dry_redeem_fail
                    ),
                    manual: self.config.dry_redeem_manual,
                }
            });
            let result = blitzkrieg_market_api::RedemptionResult {
                id: request.id.clone(),
                condition_id: request.condition_id.clone(),
                tx_hash: failure.is_none().then(|| "dry-simulated".to_string()),
                block_number: None,
                failure,
                at_ms: now_ms,
            };
            self.on_redemption_result(result, now_ms);
        }
    }

    /// Dry mode: simulate the merge confirmation of every due claim — the same
    /// shape as `simulate_redemptions`, so dry mode exercises the whole merge
    /// path (burn → receivable → cash) with no chain. In dry there is no venue
    /// to fail the merge, so the first dispatch always lands; `simulate_merges`
    /// is the only consumer of the claim's dispatch stamp, and a lost tick
    /// simply retries on the book's backoff.
    fn simulate_merges(&mut self, now_ms: i64) {
        for request in self.settlement.take_merges(now_ms) {
            let result = blitzkrieg_market_api::MergeResult::success(
                &request,
                "dry-simulated",
                None,
                now_ms,
            );
            self.on_merge_result(result, now_ms);
        }
    }

    /// Answer a settlement query locally (dry/read-only modes). The rule: the
    /// token whose VALUE EVIDENCE is highest wins and pays 1, the others pay 0
    /// — and when no token's evidence clears 0.5 the resolution is WITHHELD
    /// (`None`): the query re-arms on its own cadence and the blind alert
    /// fires. A binary market past expiry always has a winner on-chain, so
    /// booking 0 for every token would silently zero a REAL winner — the
    /// bid-less-winner bug this ladder exists for. "Cannot tell" now holds the
    /// position open loudly instead of settling it at an invented 0.
    ///
    /// Evidence per token, first rung that has a price:
    ///   1. a two-sided book → its mid (the original rule, unchanged);
    ///   2. a bid-less book with a real ask → THE ASK: the buy side being
    ///      swept does not make the winner worthless. At expiry nobody sells a
    ///      sure dollar cheap, so the winner's ask sits near 1 while a loser's
    ///      sits near 0 — the old code read a one-sided mid (zeroed by
    ///      `from_levels`) and settled bid-less winners to 0;
    ///   3. an ask-less book with a bid → the bid (still a real level);
    ///   4. no usable book → the position's own last valuation.
    ///
    /// Documented as a simulation rule, not a market fact: `source` says
    /// `core-dry`, so a simulated settlement can never be read as a real one.
    /// It flows through `on_market_resolution` exactly like a venue answer —
    /// one booking path, two sources, both traceable (`core-dry` vs the
    /// venue's own).
    fn dry_resolution(
        &self,
        query: &blitzkrieg_market_api::SettlementQuery,
    ) -> Option<blitzkrieg_market_api::MarketResolution> {
        let positions = self.positions.open_positions();
        let priced: Vec<(&str, Decimal)> = query
            .token_ids
            .iter()
            .map(|t| {
                let book = self.books.get(t);
                let top_bid = book
                    .map(|b| {
                        b.bids
                            .iter()
                            .map(|(p, _)| *p)
                            .filter(|p| *p > Decimal::ZERO)
                            .max()
                            .unwrap_or(Decimal::ZERO)
                    })
                    .unwrap_or(Decimal::ZERO);
                // A real ask is < 1; the empty-side sentinel `best_ask = 1`
                // must never read as "someone offers a dollar".
                let top_ask = book.and_then(|b| {
                    b.asks
                        .iter()
                        .map(|(p, _)| *p)
                        .filter(|p| *p > Decimal::ZERO && *p < Decimal::ONE)
                        .min()
                });
                let mut value = if top_bid > Decimal::ZERO {
                    match top_ask {
                        Some(a) => (top_bid + a) / Decimal::TWO,
                        None => top_bid,
                    }
                } else {
                    top_ask.unwrap_or(Decimal::ZERO)
                };
                if value <= Decimal::ZERO {
                    value = positions
                        .iter()
                        .find(|p| &p.token_id == t)
                        .map(|p| p.current_price)
                        .unwrap_or(Decimal::ZERO);
                }
                (t.as_str(), value)
            })
            .collect();
        if priced.is_empty() {
            return None;
        }
        let (winner, best) =
            priced.iter().fold(
                ("", Decimal::ZERO),
                |acc, (t, p)| {
                    if *p > acc.1 { (*t, *p) } else { acc }
                },
            );
        // Withhold instead of "pays nobody": at expiry the only honest
        // zero-payout is a named loser, and naming one requires evidence above
        // the coin-flip. Anything darker is our view being broken — re-arm.
        if best <= Decimal::new(5, 1) {
            // Counter-evidence ladder: our leg priced below the coin-flip AND
            // the condition's OTHER token decisively bid (≥ COUNTER_DECISIVE,
            // Gamma's own DECISIVE_HIGH). At expiry nobody pays 0.99 for a
            // coin still in the air — the partner's bid IS the venue pricing
            // the verdict, so our leg is the named loser and settles at 0.
            // Without this the leg withholds forever: the venue never answers
            // an expired round's query and the position blocks capacity until
            // an operator flattens it by hand (the ETH/BTC zombie positions).
            // The evidence key is the position's stamped partner token; a
            // position without one (old snapshots) keeps the old withhold.
            let partners: Vec<String> = positions
                .iter()
                .filter(|p| query.token_ids.contains(&p.token_id))
                .filter(|p| !p.partner_token_id.is_empty())
                .map(|p| p.partner_token_id.clone())
                .filter(|t| !query.token_ids.contains(t))
                .collect();
            let counter = partners
                .iter()
                .filter_map(|t| {
                    let book = self.books.get(t)?;
                    let top_bid = book
                        .bids
                        .iter()
                        .map(|(p, _)| *p)
                        .filter(|p| *p > Decimal::ZERO)
                        .max()?;
                    (top_bid >= COUNTER_DECISIVE_HIGH).then(|| (t.clone(), top_bid))
                })
                .max_by_key(|(_, bid)| *bid);
            if let Some((partner, _)) = counter {
                let payouts = priced
                    .iter()
                    .map(|(t, _)| ((*t).to_string(), Decimal::ZERO))
                    .chain(std::iter::once((partner, Decimal::ONE)))
                    .collect();
                return Some(blitzkrieg_market_api::MarketResolution {
                    condition_id: query.condition_id.clone(),
                    resolved: true,
                    payouts,
                    neg_risk: false,
                    source: "core-dry".to_string(),
                });
            }
            return None;
        }
        let payouts = priced
            .iter()
            .map(|(t, _)| {
                let payout = if *t == winner {
                    Decimal::ONE
                } else {
                    Decimal::ZERO
                };
                ((*t).to_string(), payout)
            })
            .collect();
        Some(blitzkrieg_market_api::MarketResolution {
            condition_id: query.condition_id.clone(),
            resolved: true,
            payouts,
            // A simulated market has no venue metadata to carry.
            neg_risk: false,
            source: "core-dry".to_string(),
        })
    }

    /// One settlement round, dry modes only: the core asks itself (through the
    /// same query path the venue plugin uses) and answers from the last books it
    /// saw. Live resolutions arrive from the venue instead.
    ///
    /// The simulated redemption runs at the end of the round, so dry mode
    /// exercises the whole path (settle → receivable → cash) with no chain —
    /// `on_market_resolution` itself only ever books, whatever the mode.
    fn drive_settlement(&mut self, now_ms: i64) {
        if !self.config.mode.settles_locally() {
            return;
        }
        for query in self.take_settlement_queries(now_ms) {
            if let Some(resolution) = self.dry_resolution(&query) {
                self.on_market_resolution(resolution, now_ms);
            }
        }
        self.simulate_redemptions(now_ms);
        self.simulate_merges(now_ms);
    }

    /// Alert once when a watched market stops being answered (see
    /// [`SettlementBook::note_blind_state`]). Loud but not repeated: the panel's
    /// `blindSinceMs` is the continuous signal.
    fn alert_if_settlement_blind(&mut self, now_ms: i64) {
        let blind = self.settlement.blind_since_ms(now_ms);
        if self.settlement.note_blind_state(blind.is_some()) {
            let since = blind.unwrap_or(now_ms);
            let message = format!(
                "settlement: no market verdict since {since} — {} market(s) past expiry cannot settle until the venue answers",
                self.settlement.tracked_markets()
            );
            tracing::error!("{message}");
            // Also the one slot (#180): "settlement has been blind since X" is the
            // single most important thing an operator can know about a live book.
            self.emit_error(CoreError::new(
                blitzkrieg_market_api::CoreErrorCode::Internal,
                message,
            ));
        }
    }

    /// #684: close positions the settlement path can never resolve — a market
    /// whose expiry is long past AND whose verdict query has never been
    /// answered by the venue. The updown round list only carries future rounds,
    /// so an already-expired condition never appears in an answer: waiting is
    /// not patience, it is a ghost position forever (`positions.exit` bounces
    /// off the venue's "Insufficient position", and nothing else can clear it).
    ///
    /// The close is deliberately NOT a settlement:
    ///   * price = the position's last known mid; `0` (a dead feed) falls back
    ///     to the entry price, i.e. cost recovery — never an invented payout;
    ///   * NO redemption claim is booked (`SettlementBook::book` is not
    ///     called): with no verified verdict, redeeming a "winner" would be
    ///     redeeming an invention. The venue's own cash truth folds in through
    ///     the existing reconcile/realign sweep, which is exactly why this
    ///     price may stay conservative;
    ///   * the trade record's exit reason is `unverifiable_expiry`
    ///     ([`UNVERIFIED_EXPIRY_EXIT_REASON`]), so history can never be read
    ///     as a venue verdict.
    ///
    /// Idempotent by construction: the position leaves the open book on the
    /// first close, so the second pass finds nothing; the watch list forgets
    /// the market through [`Self::sync_settlement_watch`] once its last
    /// position is gone.
    fn run_expiry_fallback_sweep(&mut self, now_ms: i64) {
        // Only markets the venue has structurally never answered, past the
        // silence bound. `answers > 0` means the venue is ruling on this
        // condition honestly (even "not resolved yet") — keep waiting.
        let unanswered: std::collections::HashSet<String> = self
            .settlement
            .unanswered_since(EXPIRY_FALLBACK_AFTER_MS, now_ms)
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        if unanswered.is_empty() {
            return;
        }
        let targets: Vec<crate::position::OpenPosition> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| {
                unanswered.contains(&p.condition_id)
                    // Only past-expiry holdings qualify: a future expiry with an
                    // unanswered query is still (theoretically) settleable.
                    && p.expires_at_ms <= now_ms
            })
            .cloned()
            .collect();
        if targets.is_empty() {
            return;
        }
        let closed_count = targets.len();
        for pos in targets {
            // Fallback price: the last known mid; a zeroed feed recovers cost
            // via the entry price. Both are bookkeeping values — the venue's
            // real cash position is what the reconcile sweep reports.
            let price = if pos.current_price > Decimal::ZERO {
                pos.current_price
            } else {
                pos.entry_price
            };
            let closed =
                self.positions
                    .close(&pos.id, price, UNVERIFIED_EXPIRY_EXIT_REASON, true, now_ms);
            if let Some(closed) = closed {
                // The position left the open book, so the audit identity's
                // spent/received folds shrink by its flows. The settlement path
                // covers that with a receivable claim; this path books none, so
                // the fallback notional moves into cash HERE — exactly what
                // `close` priced the un-sold shares at (fee-free maker close).
                self.credit_expiry_fallback_cash(&closed, price);
                // Trade record first, then the snapshot — the same order the
                // settlement path closes with (the audit reads the record).
                self.on_position_closed(&closed, now_ms);
                tracing::warn!(
                    position = %closed.id,
                    condition = %closed.condition_id,
                    account = %closed.account_id.as_str(),
                    shares = %closed.shares,
                    fallback_price = %price,
                    "position closed at unverifiable expiry — the venue never answered this market's settlement query; \
                     no redemption claim booked (cash truth follows the reconcile sweep)"
                );
            }
        }
        self.persist_positions();
        // A market whose LAST position left via the fallback: the watch would
        // only forget it on the next drain's `sync_settlement_watch` — forget
        // it NOW (the same thing `on_market_resolution` does), so the panel's
        // `trackedMarkets` reads 0 immediately and `unanswered_since` stops
        // naming a market we no longer hold.
        for condition_id in &unanswered {
            if self
                .positions
                .open_positions()
                .iter()
                .all(|p| p.condition_id != *condition_id)
            {
                self.settlement.forget_market(condition_id);
            }
        }
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::Internal,
            message: format!(
                "expiry fallback: closed {} position(s) on market(s) the venue never answered \
                 (unverifiable-expiry, no redemption claim — reconcile sweep owns the cash truth)",
                closed_count
            ),
        });
    }

    /// #684 shared ledger leg of the fallback close: credit what `close` priced
    /// the position's un-sold shares at (`price × dust_shares`; a maker close
    /// charges no exit fee) into the POSITION's own account book. Without it the
    /// position walks out of the open book with no compensating entry and
    /// `trade_ledger_identity` drifts by exactly the closed notional. The
    /// journal row's kind is `ExpiryFallback` — never a venue sell, never a
    /// redemption: the venue's truth stays the reconcile sweep's to report.
    /// An unknown account cannot happen (the position was opened in a
    /// configured book), so a failure is logged loudly rather than folded into
    /// the active book's cash.
    fn credit_expiry_fallback_cash(
        &mut self,
        closed: &crate::position::ClosedPosition,
        price: Decimal,
    ) {
        let amount = price * closed.dust_shares;
        if amount <= Decimal::ZERO {
            return;
        }
        let account = closed.account_id.clone();
        let position_id = closed.id.clone();
        if let Err(e) = self
            .accounts
            .get_mut(&account)
            .map(|l| l.credit_expiry_fallback(amount, &position_id))
        {
            tracing::error!(
                position = %position_id,
                account = %account.as_str(),
                error = %e,
                "expiry fallback credit FAILED — cash identity will drift by {amount}"
            );
        }
    }

    /// #684: the operator's clear-out verb for one EXPIRED market's position
    /// (`positions.force_close`). The kernel-side gates, in order:
    ///   1. the position exists (unknown id → refused, nothing written);
    ///   2. it is actually past its expiry — a live round is refused with a
    ///      pointer to `positions.exit`, because a position the venue can still
    ///      settle (or an order that can still fill) is not a ghost;
    ///   3. a manual-ops audit path is configured — same gate
    ///      `system.reset_trading` sits behind: an unaudited manual close of a
    ///      real-money record must fail rather than happen silently.
    ///
    /// The close itself rides the SAME fallback shape as
    /// [`Self::run_expiry_fallback_sweep`]: last known mid, else entry price;
    /// NO redemption claim (`SettlementBook::book` is never called — the
    /// verdict is unverified, so no payout is claimed and no winner redeemed;
    /// the venue's cash truth follows the reconcile/realign sweep). The
    /// difference is provenance: this path appends a manual-ops audit line
    /// naming the operator, and the trade record reads `unverifiable_expiry`
    /// either way.
    ///
    /// Concurrency note: `&mut self` is the whole story — the Service is the
    /// lock; no second mutex is introduced (#685 standing rule).
    pub fn force_close_expired_position(
        &mut self,
        position_id: &str,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::position::ClosedPosition> {
        let pos = self
            .positions
            .open_positions()
            .iter()
            .find(|p| p.id == position_id)
            .cloned()
            .ok_or_else(|| {
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("positions.force_close: no open position \"{position_id}\""),
                )
            })?;
        if now_ms <= pos.expires_at_ms {
            return Err(CoreError::new(
                CoreErrorCode::InvalidOrderState,
                format!(
                    "positions.force_close: position {position_id}'s round is still live until {} — \
                     use positions.exit for a position the venue can still settle",
                    pos.expires_at_ms
                ),
            ));
        }
        let audit_path = self
            .config
            .manual_ops_audit_path
            .clone()
            .map(std::path::PathBuf::from);
        let Some(audit_path) = audit_path.as_deref() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "positions.force_close: no manual-ops audit path configured — \
                 an operator close with no audit trail is refused (backtests and \
                 in-process tests carry None)"
                    .to_string(),
            ));
        };
        // The same fallback price the sweep uses: last known mid, else entry
        // (cost recovery). Bookkeeping value, never an invented payout.
        let price = if pos.current_price > Decimal::ZERO {
            pos.current_price
        } else {
            pos.entry_price
        };
        let closed = self
            .positions
            .close(
                position_id,
                price,
                UNVERIFIED_EXPIRY_EXIT_REASON,
                true,
                now_ms,
            )
            .ok_or_else(|| {
                // Lost a race with another closer: refuse without writing
                // anything (no audit line for a close that did not happen).
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("positions.force_close: no open position \"{position_id}\""),
                )
            })?;
        // Trade record first, then the snapshot — the close paths' shared order.
        self.credit_expiry_fallback_cash(&closed, price);
        self.on_position_closed(&closed, now_ms);
        self.persist_positions();
        // The manual-ops audit row is the operator's fingerprint on the close:
        // who, what, at which price, on which market. `append_manual_op` (the
        // reset path's writer) fails loudly — surfaced as a refusal-shaped
        // error, though the close itself has landed by then; the warn names it.
        if let Err(e) = crate::ipc::runtime_config::append_manual_op(
            std::path::Path::new(audit_path),
            &serde_json::json!({
                "op": "positions.force_close",
                "atMs": now_ms,
                "actor": actor,
                "positionId": closed.id,
                "conditionId": closed.condition_id,
                "tokenId": closed.token_id,
                "accountId": closed.account_id.as_str(),
                "strategy": closed.strategy,
                "shares": closed.shares.to_string(),
                "price": price.to_string(),
                "exitReason": "unverifiable_expiry",
            }),
        ) {
            tracing::error!(
                position = %closed.id,
                error = %e,
                "positions.force_close: manual-ops audit write FAILED — the close landed, the audit row did not"
            );
        }
        tracing::warn!(
            position = %closed.id,
            condition = %closed.condition_id,
            actor = %actor,
            price = %price,
            "position force-closed by operator (unverifiable-expiry, no redemption claim)"
        );
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::Internal,
            message: format!(
                "operator force-closed position {} on {} at {price} (unverifiable-expiry; \
                 no redemption claim — reconcile sweep owns the cash truth)",
                closed.id, closed.condition_id
            ),
        });
        Ok(closed)
    }

    /// Finish settlements whose close never became durable (a crash between the
    /// journal commit and the close). The booking stands — the tokens are ours
    /// on-chain — so the position is closed here exactly once, at the recorded
    /// payout price.
    ///
    /// Called from [`Core::restore_positions`], before trading resumes: the
    /// trade record has to exist for the audit's expected side to balance (see
    /// `SettlementBook::recover`), and this is the only moment the books can
    /// still be repaired.
    fn recover_settlements(&mut self) -> usize {
        let stale: Vec<crate::position::OpenPosition> = self
            .settlement
            .recover(self.positions.open_positions())
            .into_iter()
            .cloned()
            .collect();
        let mut recovered = 0usize;
        for pos in stale {
            let key = settlement_key(&pos);
            let Some(record) = self.settlement.record(&key) else {
                continue;
            };
            let (payout_per_share, at_ms) = (record.payout_per_share, record.applied_at_ms);
            let closed = self.positions.close(
                &pos.id,
                payout_per_share,
                SETTLEMENT_EXIT_REASON,
                true,
                at_ms,
            );
            if let Some(closed) = closed {
                // The trade record was lost with the crash: write it now, once,
                // and mark the close durable so no later start repeats it.
                self.on_position_closed(&closed, at_ms);
                self.settlement.note_closed(&key, at_ms);
                recovered += 1;
                tracing::warn!(
                    position = %closed.id,
                    condition = %closed.condition_id,
                    payout_per_share = %payout_per_share,
                    "recovered a settlement whose close never landed — closed at the booked payout"
                );
            }
        }
        if recovered > 0 {
            self.persist_positions();
            self.emit(Event::RiskAlert {
                code: blitzkrieg_market_api::CoreErrorCode::Internal,
                message: format!(
                    "recovered {recovered} settled position(s) whose close was interrupted — redeemed claims unaffected"
                ),
            });
        }
        recovered
    }

    /// The settlement block of the engine stats: what is settled, what the chain
    /// still owes us, and anything stuck. Read by the panel through the existing
    /// `engine_stats` outlet (no new event type).
    fn settlement_view(&self, as_of_ms: i64) -> serde_json::Value {
        let claims: Vec<serde_json::Value> = self
            .settlement
            .claims()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "conditionId": c.condition_id,
                    "payoutUsd": dec_json(c.payout_usd),
                    "negRisk": c.neg_risk,
                    "shares": c.outcome_shares.iter().map(|s| dec_json(*s)).collect::<Vec<_>>(),
                    "positions": c.position_ids.len(),
                    "attempts": c.attempts,
                    "nextAttemptMs": c.next_attempt_ms,
                    "manual": c.manual,
                    "txHash": c.tx_hash,
                    "blockNumber": c.block_number,
                    "lastError": c.last_error,
                })
            })
            .collect();
        serde_json::json!({
            "settledPositions": self.settlement.applied_count(),
            "bookedThisSession": self.settlement.booked_this_session(),
            "receivableUsd": dec_json(self.settlement.receivable_usd()),
            "pendingRedemptions": self.settlement.pending_claim_count(),
            "retryableRedemptions": self.settlement.retryable_claim_count(),
            "manualRedemptions": self.settlement.manual_claim_count(),
            "redeemedClaims": self.settlement.redeemed_count(),
            "trackedMarkets": self.settlement.tracked_markets(),
            "blindSinceMs": self.settlement.blind_since_ms(as_of_ms),
            // #461: burned-pair collateral waiting on its merge receipt —
            // split from the redemption receivables so the panel can tell
            // "settled, waiting to redeem" from "merged, waiting to mine".
            "mergeReceivableUsd": dec_json(self.settlement.merge_receivable_usd()),
            "pendingMerges": self.settlement.pending_merge_count(),
            "lastError": match self.settlement.last_error() {
                Some((claim, message, manual)) => serde_json::json!({
                    "claim": claim, "message": message, "manual": manual,
                }),
                None => serde_json::Value::Null,
            },
            "claims": claims,
        })
    }

    // ── Maintenance: pending-fill retry + maker→taker escalation + exits ────
    pub fn tick(&mut self, now_ms: i64) -> CoreResult<()> {
        // #351: advance in-flight taker legs — the tick path covers rounds
        // where the clock moves without data events (the replay tail keeps
        // reporting legs after the last event). A no-op when no leg waits.
        self.drain_inflight_takers(now_ms);
        // Flush the market-data archive at most once a second, so a reader of the
        // live file stays close behind without paying a write syscall every tick
        // (P-1.3).
        if let Some(a) = self.event_archive.as_mut() {
            a.flush_if_due(now_ms, 1000);
        }
        // Issue #669: the archive's stop (disk floor / lock / io / session cap)
        // is otherwise a single journal warn line — the external operator who
        // reported it found the silent corpus loss FIVE DAYS later with a manual
        // `df`. Fold the live recording state through the last-seen slot and
        // say it OUT LOUD on the `true → false` edge, exactly once: the risk
        // alert carries the stop reason and what is being lost (backtest
        // corpus). Staying stopped is silent; the panel banner reads
        // `stats.archive` continuously. `false → true` cannot happen (a stopped
        // archive never restarts) but resets the slot defensively.
        if let Some(a) = self.event_archive.as_ref() {
            let recording = a.status().recording;
            if self.archive_recording_last && !recording {
                let message = "事件归档停录——回测语料不再记录；请清理磁盘或调整归档上限后重启会话"
                    .to_string();
                eprintln!("core: {message}");
                tracing::error!("{}", message);
                self.note_error(
                    &CoreError::new(CoreErrorCode::Internal, message.clone()),
                    now_ms,
                );
                self.emit(Event::RiskAlert {
                    code: CoreErrorCode::Internal,
                    message,
                });
            }
            self.archive_recording_last = recording;
        }

        // E29 (§12.2): the still-growing bars push at most 1/s per
        // `(symbol, interval)` — the "正在长" preview a chart draws translucent.
        // CLOSED bars are pushed by `engine_on_data` the instant they close and
        // are NEVER subject to this throttle (a swallowed close would leave the
        // chart missing that bar forever). The throttle is per bucket on the
        // CORE's clock, shared by every subscribed session.
        // Replay gate (issue #409 P0, 39.5× measured): the pass is skipped
        // wholesale under `replay_sched()`. Its events are live-panel
        // decoration and NOTHING in a replay consumes them — the backtest
        // drain matches every event it cares about by name and drops
        // `KlineUpdate` in `_ => {}`; the `next_replay_wake_ms` probe has
        // never armed a wake for this pass (its only throttle state is the
        // local 1/s fold); and `engine_on_data` pushes CLOSED bars on its
        // own path, untouched by this gate. The per-bucket 1/s fold, the
        // "one bar one push" contract and the close-vs-preview split are all
        // live semantics; dropping them in replay changes no byte of the
        // report — the A/B in docs/perf/backtest-perf-plan.md measured the
        // output bit-identical with the pass off.
        if !self.replay_sched()
            && let Some(engine) = self.engine.as_ref()
        {
            for bar in engine.kline_current_bars() {
                let key = (bar.symbol.clone(), bar.interval);
                let due = self
                    .kline_preview_last_ms
                    .get(&key)
                    .is_none_or(|last| now_ms - *last >= 1_000);
                if due {
                    self.kline_preview_last_ms.insert(key, now_ms);
                    self.emit(Event::KlineUpdate { kline: bar });
                }
            }
        }

        // Retry buffered fills (orders registered since the event arrived).
        let pending = self.ome.drain_pending(now_ms)?;
        for outcome in pending {
            self.report_fill_outcome(outcome, now_ms);
        }

        // Settlement (issue #175): a position past its market's expiry has to
        // become cash, and only the news of the market's resolution can do it.
        // Runs before the audit so the money a settlement moves is accounted in
        // the same round it happens (the ordering is free: `drive_settlement` is
        // a no-op in live mode, where resolutions arrive from the venue).
        // Replay fast path: the whole pass is a pure function of the position
        // set and the two deadline clocks (query re-query, redemption retry) —
        // skipped until the position set changes, the earliest expiry passes,
        // or a deadline arrives. A skipped tick would have dispatched nothing
        // and stamped nothing, so every dispatch keeps its exact stamp.
        // A pending merge claim joins the deadlines: its dispatch clock must
        // keep ticking even when nothing else in the pass moves.
        let settlement_pass = !self.replay_sched()
            || self.fast_positions_dirty
            || now_ms >= self.fast_next_expiry_ms
            || now_ms >= self.settlement.next_query_deadline_ms()
            || now_ms >= self.settlement.next_redemption_ms()
            || now_ms >= self.settlement.next_merge_ms();
        if settlement_pass {
            self.drive_settlement(now_ms);
            // A market we are watching whose verdict never arrives strands the
            // position: say so once per episode (the panel carries
            // `blindSinceMs` continuously, this is the alert). The blind edge
            // only changes when queries/answers move, so riding the same gate
            // delays the alert at most to the next deadline tick.
            self.alert_if_settlement_blind(now_ms);
            // #684: same gate, one step further — a market the venue will
            // STRUCTURALLY never answer (expired rounds drop out of the updown
            // list) cannot be settled by waiting. The sweep only fires when
            // `unanswered_since` names it, so a healthy book pays one
            // hash-set build per settlement pass and nothing else.
            self.run_expiry_fallback_sweep(now_ms);
        }

        // Accounting audit (issue #189): the kernel's own periodic three-way money
        // check, so drift is caught (and blocks new entries) even when nothing
        // external is polling. Cheap: sums over live orders and open positions.
        if self.config.audit_interval_sec > 0 {
            let due = self.audit.last_at_ms == 0
                || now_ms - self.audit.last_at_ms >= self.config.audit_interval_sec * 1000;
            if due {
                self.run_accounting_audit(now_ms);
            }
        }

        // Resume entries when a per-strategy breaker cooldown elapses (KI-10):
        // each strategy resumes independently and is reported by name.
        for strategy in self.breaker.maybe_resume_all(now_ms) {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::RiskRejected,
                message: format!(
                    "breaker cooldown elapsed — resuming new entries for strategy {strategy}"
                ),
            });
        }

        // 探针 10s 汇总（任务二 1-3，受控时钟）：tick 断流 / spot 失效的周期
        // 判定只由调用方传入的 `now_ms` 驱动——生产是墙钟，回放是虚拟时钟，
        // 同一函数、同一节奏，无墙钟窗口（memory: wallclock-test-flake 处方）。
        // 只观测：warn + 计数，不拦截任何东西。
        if self.probe_summary_due(now_ms) {
            self.probe_run_summary(now_ms);
        } else {
            // 非汇总 tick：汇总读过的窗口自此重新计数。
            self.probe_ticks.clear_window_if_consumed();
        }

        // Persist finalized near-miss records (blocked signals + their paths).
        if let Some(path) = self.config.near_miss_path.clone() {
            let recs = self
                .engine
                .as_mut()
                .map(|e| e.take_near_misses(now_ms))
                .unwrap_or_default();
            if !recs.is_empty()
                && let Err(e) =
                    crate::shadow::persist_near_misses(std::path::Path::new(&path), &recs)
            {
                tracing::warn!(error = %e, path = %path, "near-miss persist failed");
            }
        }

        // #173: roll the daily-loss budget at the UTC day boundary BEFORE the
        // exit checks, so a close that lands on the new day is counted in it
        // — and report the state that gates entries: the roll itself (audit
        // trail), a restored/just-tripped breaker, and #177's withheld stops.
        self.roll_daily_budget(now_ms);
        self.emit_daily_trip();

        // Evaluate exits for every open position and place a closing SELL.
        self.run_exit_checks(now_ms)?;
        // Replay fast path: the dirty-token set is consumed by this cycle's
        // valuation — clear it here (not inside run_exit_checks, whose early
        // returns would skip the clear) so it never becomes a permanent
        // over-approximation. Inserts landing between here and the next
        // tick's exit pass are events that arrive after this point by
        // construction, so nothing is lost. #390 (nofast): the same set wakes
        // the deadline-scheduled ladder, so it is cleared here on the nofast
        // path too — after `run_exit_checks` had its look.
        self.fast_dirty_tokens.clear();
        if self.replay_sched() && !self.config.backtest_fast {
            // #390 (merged maintenance): re-arm the single maintenance
            // deadline — the merged pass's own 1 s beat (report stamps are
            // quantised here in the base) plus the earliest position expiry
            // (time-to-expiry rules). Between these, a quiet grid re-observes
            // the previous run's inputs; dirty tokens wake the pass early on
            // their own.
            self.fast_next_exit_deadline_ms = self
                .positions
                .next_maintenance_deadline_ms(now_ms, EXIT_REPORT_BEAT_MS)
                .unwrap_or(i64::MAX);
        }

        // #177: a protective stop the wick guard withheld is an event, not a
        // silent no-op — review must be able to see "should have triggered".
        self.emit_suppressed_stops();

        // #457: an order resting on an EXPIRED round can never fill — close it
        // BEFORE the escalation sweep, so a dead round is never chased.
        // Live-clock only (see the sweep's own replay note).
        self.run_expiry_sweep(now_ms);
        // E-VA companion: a pair leg whose partner is gone (filled one way,
        // cancelled the other) past the landing grace window is a naked leg —
        // alert once, flatten it. Governed atomic scopes only; live path
        // only (the nofast replay loop keeps its own shape).
        self.run_pair_orphan_sweep(now_ms);
        // Escalate due maker_then_taker orders: cancel maker, cross as taker.
        // Replay fast path: the sweep over every live order runs only when
        // the order set changed, or when the earliest armed escalation
        // deadline has arrived — between those, no `escalate_at_ms` can be
        // due (arming never moves a deadline earlier than the sweep that
        // would notice it recomputes the floor).
        if !self.replay_sched() || self.fast_orders_dirty || now_ms >= self.fast_next_escalation_ms
        {
            self.run_escalation_sweep(now_ms)?;
            if self.replay_sched() {
                self.fast_orders_dirty = false;
                self.fast_next_escalation_ms = self.ome.next_escalation_ms();
            }
        }
        Ok(())
    }

    /// #461 Fix 2/4: the COMPLETE risk verdict an escalated taker leg must
    /// pass before the maker is touched. Both escalation paths (the timer
    /// sweep and the post-only WouldCross reaction) share this ONE function —
    /// there is no third vocabulary.
    ///
    /// Layers, in order:
    ///   1. `check_with_equity` with the REAL account balance — the bare
    ///      `risk.check` the paths used before passed equity 0, which
    ///      silently disabled the #202 equity-relative cap at the exact
    ///      moment the order chases the worst price on the book;
    ///   2. `judge_entry` (the nine systemic limits) when any bound is armed
    ///      — the same verdict the arbitration pipeline hands strategy
    ///      entries, so chasing cannot outrun a cap the entry obeyed;
    ///   3. the PAIR-COST hard cap: for an entry whose condition already has
    ///      a counterpart position (a maker leg that filled or is filling),
    ///      `pair_paid + chase_cost <= $1.00` — a binary pair redeems exactly
    ///      $1, so a higher total is a GUARANTEED loss, not a risk. The
    ///      "entry gate forbids" comment in `PositionManager::merge_condition`
    ///      asserted this invariant; this is the code that makes it true.
    ///
    /// Returns Err with the reason; the caller keeps the maker resting (timer
    /// path) or books the rejection (WouldCross path) — the escalation intent
    /// survives either way via the normal re-arm/backoff machinery.
    fn escalation_guard(&self, req: &OrderRequest, chase_price: Decimal) -> Result<(), String> {
        // 1. The ordinary submission gate, with the account's real equity.
        let equity = self
            .accounts
            .ledger_of(&req.account_id)
            .map(|l| l.balance())
            .unwrap_or(Decimal::ZERO);
        if let Err(e) = self.risk.check_with_equity(req, equity) {
            return Err(e.message);
        }
        // 2. The systemic nine. Armed bounds judge this entry exactly as the
        //    pipeline would judge a fresh strategy entry (the aggregates are
        //    built the same way — positions only, the incoming notional on top).
        let limits = self.risk.systemic_snapshot();
        if limits.any_armed() {
            let now_ms = blitzkrieg_market_api::net::now_ms();
            let facts = crate::risk::limits::SystemicFacts {
                open_positions: self
                    .positions
                    .open_positions()
                    .iter()
                    .filter(|p| p.expires_at_ms > now_ms)
                    .count(),
                open_exposure_usd: self
                    .positions
                    .open_positions()
                    .iter()
                    .map(|p| p.cost_usd)
                    .sum(),
                same_asset_exposure_usd: self
                    .positions
                    .open_positions()
                    .iter()
                    .filter(|p| p.asset == req.asset)
                    .map(|p| p.cost_usd)
                    .sum(),
                day_realized_usd: self.positions.daily_pnl_total(),
            };
            let bound_stop_pct = crate::exit_policy::effective_stop_pct(
                self.config.positions.exit.stop_loss_pct,
                // Time left is unknown on this path; 0 = the bound stop at its
                // tightest (no time-adjustment relief), the conservative read.
                0,
                &self.config.positions.exit,
            );
            if let crate::risk::limits::EntryJudgment::Reject { detail, .. } =
                crate::risk::limits::judge_entry(
                    limits,
                    &facts,
                    chase_price,
                    req.size,
                    bound_stop_pct,
                )
            {
                return Err(detail);
            }
        }
        // 3. The pair-cost cap. Only entries (BUY) chase; a condition with no
        //    counterpart position has nothing to sum against (a fresh pair is
        //    the strategy's own <1.00 decision). Per-share: one UP share plus
        //    one DOWN share redeems exactly $1.00. 跨账户双腿：分母只看本
        //    账户的对手腿（同一账本内赎回 $1）。
        if req.side == Side::Buy
            && let Some(paid) = self.counterpart_pair_basis(
                &req.condition_id,
                &req.asset,
                &parse_direction(&req.direction),
                &req.account_id,
            )
        {
            let total = paid + chase_price;
            if total > Decimal::ONE {
                return Err(format!(
                    "pair-cost cap: counterpart leg basis {paid}/share + chasing at \
                     {chase_price} = {total} > $1.00 a pair redeems — the merge would \
                     lock in a loss"
                ));
            }
        }
        Ok(())
    }

    /// Escalate due maker_then_taker orders: cancel the resting maker, cross
    /// the remainder as a taker. Extracted from `tick` so the replay fast
    /// path can gate the sweep (order-set dirty OR earliest deadline due)
    /// without duplicating the submission rules.
    fn run_escalation_sweep(&mut self, now_ms: i64) -> CoreResult<()> {
        let due: Vec<EscalationTarget> = self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| o.escalate_at_ms.map(|t| t <= now_ms).unwrap_or(false))
            .map(|o| {
                (
                    o.order_id.clone(),
                    o.side,
                    o.size - o.filled_size,
                    o.token_id.clone(),
                    o.condition_id.clone(),
                    o.strategy.clone(),
                    o.asset.clone(),
                    o.direction.clone(),
                    o.round_slot,
                    // The maker's own timeout, rebuilt from when it was
                    // armed: a re-armed clock waits the same interval again.
                    o.escalate_at_ms.unwrap_or(now_ms) - o.submitted_at_ms,
                    // E28 (§9.2): the taker leg spends from the maker's account.
                    o.account_id.clone(),
                )
            })
            .collect();
        for (
            id,
            side,
            remaining,
            token,
            condition,
            strategy,
            asset,
            direction,
            slot,
            timeout,
            account_id,
        ) in due
        {
            if remaining <= Decimal::ZERO {
                continue;
            }
            // Reprice the taker leg from the CURRENT book before touching the
            // maker: the resting limit is a passive price — a live FOK stated
            // there would be killed, so escalating to it just moves the
            // failure. If the book cannot cover the remaining size even at
            // the extreme of the grid, leave the maker resting and re-arm the
            // clock: no venue traffic, the entry intent stays alive.
            let cap = if side == Side::Buy {
                Decimal::new(99, 2)
            } else {
                Decimal::new(1, 2)
            };
            let Some((_, worst)) = self.marketable_walk(side, &token, remaining, cap, now_ms)
            else {
                self.ome.set_escalation(&id, now_ms + timeout)?;
                continue;
            };
            // issue 568 阶段一：定时 escalation 的追单是同一入腿事实链的
            // 续段 —— 成交/死亡事实寻址回声明腿（原腿 cancel 的死亡事实已
            // 落账；续段 key 在追单 place 前登记，其 Taker 成交照旧记账）。
            self.pair_book.bind_continuation(
                &strategy,
                account_id.as_str(),
                &token,
                &format!("{id}:escalated"),
            );
            let req = OrderRequest {
                token_id: token,
                condition_id: condition,
                side,
                mode: FillPolicy::Taker,
                price: worst,
                size: remaining,
                internal_key: format!("{id}:escalated"),
                strategy,
                asset,
                direction,
                round_slot: slot,
                // E28 (§9.2): the escalated leg spends from the maker's own
                // account.
                account_id: account_id.clone(),
            };
            // The maker is about to be cancelled to make room for this leg, so
            // the FULL risk verdict gets the first word — the same verdict the
            // submission path will apply a moment later (#461: real equity +
            // systemic limits + the pair-cost cap, one function for both
            // escalation paths). `marketable_walk` only asks whether the
            // book HAS depth at the grid's extreme; it does not ask whether
            // this core may afford it. With a per-order notional cap, the
            // affordable price is `cap / size`, and a book that only offers
            // depth above it (a one-sided 0.99 ask is the everyday case) makes
            // a naive escalation cancel the maker and THEN get refused: the
            // entry is destroyed rather than deferred, and the propagated
            // error aborts the whole tick. Skipping instead keeps the resting
            // maker alive and re-arms the clock — the same semantics as the
            // no-depth branch above — while saying so once per order.
            if let Err(reason) = self.escalation_guard(&req, worst) {
                if self.escalation_skips.insert(id.clone()) {
                    tracing::info!(
                        order = %id,
                        strategy = %req.strategy,
                        price = %worst,
                        size = %remaining,
                        reason = %reason,
                        "escalation skipped: the chase is refused by the full risk guard; maker leg kept resting"
                    );
                }
                self.ome.set_escalation(&id, now_ms + timeout)?;
                continue;
            }
            self.escalation_skips.remove(&id);
            self.cancel(&id, now_ms)?;
            // Past the gate, a placement can still fail (a reservation, a
            // venue refusal). The maker is already gone, so that entry is
            // lost — but ONE dead escalation must not abort the maintenance
            // pass: the remaining due orders still have their turn.
            if let Err(e) = self.place_escalated(req, now_ms) {
                tracing::warn!(
                    order = %id,
                    error = %e,
                    "the escalated leg was refused after the maker was cancelled"
                );
            }
        }
        // Forget the skip marks of orders that are no longer live: the set is
        // keyed by order id, and a fresh id is minted for every order.
        let ome = &self.ome;
        self.escalation_skips
            .retain(|id| ome.get(id).is_some_and(|o| o.status.is_live()));
        Ok(())
    }

    /// Check exits and submit a closing SELL for each triggered position.
    ///
    /// Two sources converge on ONE submission loop so both are subject to the
    /// same rules (live-sell dedup, `sell_shares`, risk/ledger/sign in `place`):
    ///  - the kernel's automated exit policy (`check_exits`), suppressed by
    ///    `auto_exits_enabled = false`;
    ///  - explicit strategy close intents (`StrategySignal`), which like a
    ///    manual flatten are honoured even when automated exits are disabled,
    ///    but still cannot bypass the kill switch/risk/dedup/position checks.
    fn run_exit_checks(&mut self, now_ms: i64) -> CoreResult<()> {
        // Consume this cycle's intents up front: an intent on a token with no
        // open position is dropped, never left to fire on a later position.
        let intents = std::mem::take(&mut self.strategy_exits);
        if self.positions.open_positions().is_empty() {
            return Ok(());
        }

        // MERGE interception (task 2.2): a strategy close intent tagged "merge"
        // is a pair-COLLECTION request, not a market exit. Complete UP+DOWN
        // pairs of the intent's condition are burned into their on-chain
        // collateral ($1 per pair) immediately — no order is placed, no ladder
        // prices it, the on-chain merge IS the fill, exactly like settlement's
        // redemption. Unpaired remainders stay open for settlement; an intent
        // with no complete pair behind it is dropped like every intent.
        // Runs BEFORE any book borrow: a merge is priced by construction
        // ($0.50 per leg), never off a quote.
        // Read the live books through a shared immutable borrow instead of
        // cloning the whole map per tick: the valuate/check_exits closure only
        // needs the few tokens behind open positions, not every mirrored book.
        // (Scoped before the merge interception: the closure must drop before
        // the merge's `&mut self` calls.)
        // Issue #390 (replay scheduling, marks only): the base's every-tick
        // valuate advances `current_price`/`prev_price`/`last_book_ts` before
        // its ladder runs, and consumers priced off the mark between the two
        // passes (strategy-signal exits below) see the fresh quote. The merged
        // replay pass reproduces that ordering with a mark-only refresh —
        // dirty tokens on a skip tick (only their book changed), every
        // position on a run tick — and leaves the counter ratchet to the
        // interval backfill inside the ladder.
        if !self.config.backtest_fast {
            let marks_all = !self.config.replay_scheduling
                || !self.fast_dirty_tokens.is_empty()
                || now_ms >= self.fast_next_exit_deadline_ms;
            // Issue #390: the scalar projection (BookView) instead of the
            // full snapshot — the marks refresh reads best_bid/mid_price/
            // timestamp only, so the two owned level-vector clones and the
            // token String per position per tick are gone. `from_book` does
            // the five Decimal reductions ONCE per mirrored book (the
            // closure runs once per position; identical inputs, Copy output).
            // #462: the timestamp passed is the book's own RECEIVED time —
            // stamping `now_ms` here would make every book age zero and
            // blind the freshness gate to feed outages.
            let book_fn = |token: &str| {
                self.books
                    .get(token)
                    .map(|b| crate::exit_policy::BookView::from_book(b, b.received_ms))
            };
            self.positions
                .refresh_marks(&book_fn, now_ms, marks_all, &self.fast_dirty_tokens);
        }
        let (merge_intents, sell_intents): (Vec<_>, Vec<_>) =
            intents.into_iter().partition(|i| i.reason == "merge");
        for intent in merge_intents {
            // (account, token)-keyed: two scopes can hold the same token and a
            // merge intent must burn the pair the intending scope's account owns.
            let intent_account = AccountId::from(intent.account_id.as_str());
            let Some(condition_id) = self
                .positions
                .open_positions()
                .iter()
                .find(|p| p.token_id == intent.token_id && p.account_id == intent_account)
                .map(|p| p.condition_id.clone())
            else {
                continue;
            };
            let Some(outcome) = self.positions.merge_condition(&condition_id, now_ms) else {
                tracing::info!(
                    condition = %condition_id,
                    "merge intent: no complete pair to burn (one-sided or cross-account) — legs stay open"
                );
                continue;
            };
            if outcome.pairs_merged > Decimal::ZERO {
                // #461: the burned pairs are a RECEIVABLE, not cash. On-chain
                // the CTF merge mints USDC only when the transaction mines;
                // crediting `balance` at intent time said the wallet held
                // money it did not hold — the audit's venue leg (live) or the
                // local cash identity (dry) goes red, correctly. The claim
                // rides the settlement book's journal, so a restart between
                // the burn and the receipt keeps the $1/pair.
                let neg_risk = self
                    .engine
                    .as_ref()
                    .and_then(|e| e.neg_risk_of(&intent.token_id))
                    .unwrap_or(false);
                let claim_id = self.settlement.note_merge_burned(
                    &outcome.condition_id,
                    outcome.account_id.as_str(),
                    // #647: the venue the burned pairs sit on — the legs' own
                    // execution-venue stamp, so the merge request drains to
                    // the executor that can run this CTF merge (and to no
                    // sibling's bridge).
                    &outcome.venue,
                    neg_risk,
                    outcome.pairs_merged,
                    now_ms,
                );
                tracing::info!(
                    condition = %outcome.condition_id,
                    pairs = %outcome.pairs_merged,
                    claim = %claim_id,
                    "merge burned complete pairs — collateral pending on-chain confirmation"
                );
            }
            self.persist_positions();
            for closed in &outcome.closed {
                self.on_position_closed(closed, now_ms);
            }
            tracing::info!(
                condition = %outcome.condition_id,
                pairs = %outcome.pairs_merged,
                cash_usd = %outcome.cash_credited_usd,
                unpaired = %outcome.unpaired_shares,
                "merged complete pairs into collateral"
            );
        }

        // Always re-value open positions from the latest books so the dashboard's
        // unrealized PnL / HWM move even when automated exits are disabled.
        // Replay fast path (--backtest-fast): when EVERY registered strategy
        // holds to settlement and the kernel settles locally, every ladder
        // verdict against an open position is discarded by the settlement-
        // holders filter below — the ladder is pure per-tick waste. The fast
        // path re-values only dirty tokens and skips the ladder + the E31-b
        // scan entirely (a holder never places a working sell, so the
        // residual backstop has nothing to answer between events). A mixed
        // book (any non-holder registered) keeps the full historical pass.
        let all_hold = self.config.backtest_fast
            && self.config.mode.settles_locally()
            && self
                .engine
                .as_ref()
                .is_some_and(|e| e.all_strategies_hold_to_settlement());
        // Issue #390: the scalar projection (BookView) instead of the full
        // snapshot — the ladder reads best_bid/best_ask/mid_price/depths/
        // timestamp and never the level vectors, so the two owned clones and
        // the token String per position per tick disappear; the reductions
        // run once per mirrored book and the Copy result is passed by value.
        // #462: the timestamp is the book's own RECEIVED time (see the marks
        // closure above) — the ladder must not re-arm a stale quote.
        let book_fn = |token: &str| {
            self.books
                .get(token)
                .map(|b| crate::exit_policy::BookView::from_book(b, b.received_ms))
        };
        if all_hold {
            self.positions
                .valuate_dirty_tokens(&book_fn, now_ms, &self.fast_dirty_tokens);
        } else if !self.config.replay_scheduling || self.config.backtest_fast {
            // Legacy spelling (live kernels and plain --backtest-fast without
            // all-hold): the base's every-tick valuate, unchanged.
            //
            // Issue #390 (replay scheduling): this pass is MERGED into the
            // gated ladder below — its state update there runs at the 2×
            // backfill rate (the base advances the counter ratchet twice per
            // tick: once here, once in the ladder), and the mark-only refresh
            // above carries `current_price`/`prev_price`/`last_book_ts`.
            // Running the full valuate too would double the ratchet again on
            // run ticks and is pure idempotent re-observation on skip ticks.
            self.positions.valuate(&book_fn, now_ms);
        }
        let intents = sell_intents;

        // A unit of closing work resolved against a concrete open position.
        #[derive(Clone)]
        struct ExitJob {
            position_id: String,
            token: String,
            condition_id: String,
            price: Decimal,
            reason: ExitReason,
            use_maker: bool,
            internal_key: String,
            strategy: String,
            asset: String,
            direction: String,
            /// E28 (§9.2): the account the closing SELL spends from — the
            /// position's own book, so an exit can never debit another account.
            account_id: AccountId,
            /// Issue #464: shares to sell (`None` = the whole sellable
            /// position, every path's historical behavior). A ladder step's
            /// tranche rides here from the `ExitRequest`.
            close_shares: Option<Decimal>,
        }

        let mut jobs: Vec<ExitJob> = Vec::new();
        let mut has_job: HashSet<String> = HashSet::new();

        // Hold-to-settlement strategies keep their positions to expiry: selling
        // a complete-set leg before redemption destroys the pair's riskless
        // payoff ($1 per share-pair regardless of which side wins). The policy
        // ladder therefore skips them — but only where the kernel settles
        // locally (dry/read-only), where `drive_settlement` does the redemption.
        // On a LIVE kernel the ladder keeps running until venue resolutions are
        // a proven path: a position sitting unmanaged past expiry would be
        // strictly worse than one sold early. Strategy-signal intents still
        // close such positions everywhere — an explicit "get out" is honoured.
        // Fast path: with every strategy a holder, the set is every strategy —
        // and its only consumers (the ladder loop, skipped above) are gated
        // off, so the per-tick rebuild is skipped too.
        let settlement_holders: HashSet<String> = if !all_hold && self.config.mode.settles_locally()
        {
            self.positions
                .open_positions()
                .iter()
                .map(|p| p.strategy.clone())
                .filter(|s| {
                    self.engine
                        .as_ref()
                        .is_some_and(|e| e.strategy_holds_to_settlement(s))
                })
                .collect()
        } else {
            HashSet::new()
        };
        let holds_settlement =
            |pos: &crate::position::OpenPosition| settlement_holders.contains(&pos.strategy);

        if self.config.auto_exits_enabled && !all_hold {
            // Issue #390 (replay nofast): the ladder runs on its deadline
            // schedule instead of every tick — dirty tokens (a book arrived
            // for a held position), the exit-deadline clock (expiry/
            // force-exit/throttle beats), or the 1 s report beat that keeps
            // every time-quantised alert stamp the every-tick pass produced.
            // The nofast replay observes every verdict, so the schedule below
            // is output-preserving by construction: a skipped tick's only
            // durable effect on the ladder is the counter ratchet, which the
            // interval-compensated state update backfills. Every timestamp
            // still stamps the tick that actually fires.
            // `interval_mode` is the #390 merged-maintenance spelling —
            // nofast replay ONLY. The fast path stays on the legacy
            // per-tick `valuate_one`: its all-holder filter discards every
            // ladder verdict anyway, so the merged pass buys it nothing,
            // while the per-position run ledger the merged pass maintains
            // showed up as a measurable regression (68s → 90s full corpus).
            let (ladder_run, interval_mode) = if self.config.backtest_fast {
                (true, false)
            } else if self.config.replay_scheduling {
                (
                    !self.fast_dirty_tokens.is_empty() || now_ms >= self.fast_next_exit_deadline_ms,
                    true,
                )
            } else {
                (true, false)
            };
            for req in self.positions.check_exits_interval(
                &book_fn,
                now_ms,
                ladder_run,
                interval_mode,
                self.config.tick_schedule_ms,
                &self.fast_dirty_tokens,
            ) {
                let Some(pos) = self
                    .positions
                    .open_positions()
                    .iter()
                    .find(|p| p.id == req.position_id)
                    .cloned()
                else {
                    continue;
                };
                if holds_settlement(&pos) {
                    continue;
                }
                has_job.insert(pos.id.clone());
                jobs.push(ExitJob {
                    position_id: pos.id.clone(),
                    token: pos.token_id.clone(),
                    condition_id: pos.condition_id.clone(),
                    price: req.exit_price,
                    reason: req.reason,
                    use_maker: req.use_maker,
                    internal_key: exit_intent_key("exit", &pos, &format!("{:?}", req.reason)),
                    strategy: pos.strategy.clone(),
                    asset: pos.asset.clone(),
                    direction: pos.direction.as_str().to_string(),
                    // E28 (§9.2): the automated exit spends from the position's
                    // own account.
                    account_id: pos.account_id.clone(),
                    // #464: a ladder step's tranche (None = the whole
                    // position, every no-ladder path unchanged).
                    close_shares: req.close_shares,
                });
            }
        }

        // Strategy-signal exits: resolve the token to a live position and price
        // off its latest valuation, exactly like a manual flatten. The intent's
        // OWN account is the first key (账户↔venue 解耦: two scopes can hold
        // the same token; the close must land on the intending scope's row),
        // and the job's internal key carries the account segment so two
        // accounts' exit ladders never collide in the place-cooldown/dedup
        // map. A legacy intent (no account — the pre-fanout shape) resolves
        // to `default`, the same row it always hit.
        for intent in intents {
            let intent_account = AccountId::from(intent.account_id.as_str());
            let Some(pos) = self
                .positions
                .open_positions()
                .iter()
                .find(|p| p.token_id == intent.token_id && p.account_id == intent_account)
                .cloned()
            else {
                continue;
            };
            if !has_job.insert(pos.id.clone()) {
                continue; // an automated/policy exit already closes it this cycle
            }
            let price = if pos.current_price > Decimal::ZERO {
                pos.current_price
            } else {
                Decimal::new(1, 2)
            };
            let tag: String = intent
                .reason
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .take(32)
                .collect();
            jobs.push(ExitJob {
                position_id: pos.id.clone(),
                token: pos.token_id.clone(),
                condition_id: pos.condition_id.clone(),
                price,
                reason: ExitReason::StrategySignal,
                use_maker: false,
                internal_key: exit_intent_key("exit-strategy", &pos, &tag),
                strategy: pos.strategy.clone(),
                asset: pos.asset.clone(),
                direction: pos.direction.as_str().to_string(),
                // E28 (§9.2): the strategy-signal exit spends from the
                // position's own account.
                account_id: pos.account_id.clone(),
                close_shares: None,
            });
        }

        // E31-b residual backstop: a working exit can be outgrown by its
        // position (a late entry fill lands while the sell is resting), and
        // once the sell completes the residual has NO working sell and the
        // exit policy may stay silent (e.g. the profit target already passed).
        // Any position with a sell that filled after the position opened and
        // no live sell now gets an explicit re-close of the remainder.
        // Fast path: a holder never places a working sell on these tokens,
        // so between dirty cycles there is no residual to re-close — the
        // scan is skipped with the ladder (still runs on the non-fast path
        // and whenever any non-holder strategy is registered).
        // Issue #390 (replay scheduling): the scan's inputs move only when an
        // ORDER event lands (a sell fills, a late entry fills) or when the
        // ladder actually ran (its verdicts can place/complete sells) —
        // gating it to those ticks reproduces the every-tick scan: a quiet
        // tick re-reads the same (live sells, filled-since, open positions)
        // and would emit nothing. `fast_orders_dirty` is the #388 order-set
        // wake, maintained on both replay paths.
        let residual_scan = if self.config.backtest_fast {
            true
        } else if self.config.replay_scheduling {
            self.fast_orders_dirty || now_ms >= self.fast_next_exit_deadline_ms
        } else {
            true
        };
        if self.config.auto_exits_enabled && !all_hold && residual_scan {
            // Issue #464: a ladder tranche's SELL is a PARTIAL exit — while it
            // works, the position has shares left on purpose. The residual
            // backstop below re-closes exactly such remainders, so a ladder
            // book would be re-flattened wholesale one tick after every
            // step. A position whose token has a working or recently filled
            // ladder SELL stays out of this scan until the ladder's own next
            // step (or stop) takes it the rest of the way. No-ladder runs
            // never carry tranches, so their scan is untouched.
            let ladder_tranche_tokens: HashSet<String> = jobs
                .iter()
                .filter(|j| j.close_shares.is_some())
                .map(|j| j.token.clone())
                .collect();
            for pos in self.positions.open_positions().to_vec() {
                if !has_job.insert(pos.id.clone()) {
                    continue;
                }
                if ladder_tranche_tokens.contains(&pos.token_id) {
                    continue;
                }
                let has_live_sell = self
                    .ome
                    .live_for(&pos.token_id, Side::Sell)
                    .into_iter()
                    .any(|o| o.status.is_live());
                if has_live_sell {
                    continue;
                }
                let closed_here = self
                    .ome
                    .has_filled_sell_since(&pos.token_id, pos.entered_at_ms);
                if !closed_here {
                    continue;
                }
                let price = if pos.current_price > Decimal::ZERO {
                    pos.current_price
                } else {
                    Decimal::new(1, 2)
                };
                jobs.push(ExitJob {
                    position_id: pos.id.clone(),
                    token: pos.token_id.clone(),
                    condition_id: pos.condition_id.clone(),
                    price,
                    reason: ExitReason::ForceExit,
                    use_maker: false,
                    internal_key: exit_intent_key("exit-residual", &pos, ""),
                    strategy: pos.strategy.clone(),
                    asset: pos.asset.clone(),
                    direction: pos.direction.as_str().to_string(),
                    // E28 (§9.2): the residual re-close spends from the
                    // position's own account.
                    account_id: pos.account_id.clone(),
                    close_shares: None,
                });
            }
        }

        for job in jobs {
            let already_live = self
                .ome
                .live_for(&job.token, Side::Sell)
                .into_iter()
                .any(|o| o.status.is_live());
            if already_live {
                continue;
            }
            // The venue already refused this exit a moment ago: let the
            // backoff run instead of re-POSTing every tick (the 380+-rejection
            // storm this guards against). Silent skip — the rejection was
            // already accounted for when it happened.
            if self.placement_blocked(&job.internal_key, now_ms) {
                continue;
            }
            // #464: a ladder tranche sells exactly its floor-to-grid size
            // (never more than the position still holds); `None` sells the
            // whole sellable position — every no-ladder path, unchanged.
            let size = if let Some(tranche) = job.close_shares {
                let Some(whole) = self.positions.sell_shares(&job.position_id) else {
                    continue;
                };
                tranche.min(whole)
            } else {
                let Some(size) = self.positions.sell_shares(&job.position_id) else {
                    continue;
                };
                size
            };
            let mode = if job.use_maker {
                FillPolicy::Maker
            } else {
                FillPolicy::Taker
            };
            // A FOK sell must fill in FULL at or above its limit; priced at
            // the touch it dies on the first thin tick (thin books race the
            // quote). Selling LOWER only widens the marketable range — the
            // venue still fills at the bid or better, so the slip is a floor
            // for where we agree to sell, not a price we pay.
            //
            // #496: a resting (maker) sell priced AT the bid is a crossing
            // post-only order — the venue refuses it every time, so a live TP
            // exit could never rest (the rejection storm #163 only backs off;
            // dry's close-intent exemption masked it). The resting sell goes
            // one tick ABOVE the bid, clamped to stay strictly inside the
            // ask (a price at/above the ask would cross the other side and
            // be refused too). No bid or no book: there is nothing to rest
            // against, so the job's judged price stands and the venue's own
            // refusal (or the dry book aging) reports it.
            let price = if mode == FillPolicy::Taker {
                marketable_exit_price(job.price, self.config.exit_taker_slip_ticks)
            } else {
                self.maker_exit_price(&job.token, job.price)
            };
            let order = OrderRequest {
                token_id: job.token.clone(),
                condition_id: job.condition_id,
                side: Side::Sell,
                mode,
                price,
                size,
                internal_key: job.internal_key,
                strategy: job.strategy,
                asset: job.asset,
                direction: job.direction,
                round_slot: 0,
                // E28 (§9.2): the closing SELL spends from the position's own
                // account (carried on the job from the position).
                account_id: job.account_id,
            };
            // Record the intended exit reason so a full sell fill closes with it
            // (bounded and cleaned up by the close: issue #190).
            self.note_exit_reason(&job.token, job.reason);
            if let Err(e) = self.place(order, 0, now_ms) {
                self.emit_error(e);
            }
        }
        Ok(())
    }
}

fn parse_direction(s: &str) -> SignalDirection {
    match s.to_ascii_lowercase().as_str() {
        "down" => SignalDirection::Down,
        _ => SignalDirection::Up,
    }
}

/// Instantiate ONE account scope of a registered Lua strategy from its
/// package dir (the source the default scope recorded at load: `lua:<dir>`).
/// Returns `None` when the source is not a reloadable Lua package (a dylib
/// or a vanished dir) — the scope-rebuild caller skips the name with a
/// warning, it never panics. Mirrors `load_lua_package_inner`'s adapter
/// wiring: the manifest declares (holds/exemptions) + the kernel's ONE fee
/// schedule stamped into the Lua state.
fn build_lua_scope(
    sources: &std::collections::HashMap<String, String>,
    name: &str,
    account: &str,
) -> Option<Box<dyn crate::strategies::EngineStrategy>> {
    use crate::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
    let source = sources.get(name)?;
    let dir = source.strip_prefix("lua:")?;
    let loaded = load_lua_package(std::path::Path::new(dir)).ok()?;
    if loaded.name != name {
        // The dir drifted (another package now lives there): refuse — a
        // scope must host the strategy the assignment names.
        return None;
    }
    let mut adapter = LuaEngineAdapter::scoped_new(
        loaded.strategy,
        loaded.tunables.clone(),
        account.to_string(),
    );
    adapter = adapter
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
        .declare_atomic_batch(loaded.atomic_batch)
        // RFC-645 A-1: the arbiter package's statistics declaration follows
        // the name, exactly as the default-scope load path stamps it.
        .declare_pair_book_stats(name == ARB_STRATEGY);
    let sched = crate::exit_policy::fee_schedule();
    adapter.set_fee_schedule(blitzkrieg_lua_runtime::FeeScheduleView {
        name: sched.name.to_string(),
        rate: sched.rate.to_string(),
        exponent: sched.exponent,
    });
    Some(Box::new(adapter))
}

/// Price a taker EXIT to actually fill: the limit nudged a few ticks below
/// the executable bid, floored at the venue minimum. Polymarket prices run in
/// 0.01 ticks, so a tick is 0.01.
fn marketable_exit_price(executable_bid: Decimal, slip_ticks: i64) -> Decimal {
    if executable_bid <= Decimal::ZERO {
        return executable_bid;
    }
    let slipped = executable_bid - Decimal::new(slip_ticks.max(0), 2);
    slipped.max(Decimal::new(1, 2))
}

impl Core {
    /// #496: price a RESTING (maker) exit one tick above the best bid, clamped
    /// to stay strictly below the best ask — the only shape a post-only SELL
    /// can be accepted at. A sell AT the bid crosses the book (`Book::crosses`
    /// on the sell side is `best_bid >= price`), so the venue refuses the POST
    /// every time; a sell at/above the ask crosses the other side. Without a
    /// live book there is nothing to rest against: the judged price stands and
    /// the venue's own refusal (or dry's book aging) reports it.
    fn maker_exit_price(&self, token: &str, judged: Decimal) -> Decimal {
        let tick = Decimal::new(1, 2); // 0.01 — the binary-market price grid
        let Some(book) = self.books.get(token) else {
            return judged;
        };
        let Some(bid) = book.best_bid() else {
            return judged;
        };
        let mut price = bid + tick;
        if let Some(ask) = book.best_ask()
            && ask > Decimal::ZERO
            && price >= ask
        {
            // A one-tick spread leaves no inside price: the closest legal
            // rest would be above the bid but there is no grid point strictly
            // between bid and ask. The band is too tight to rest honestly —
            // the judged price stands and the venue's answer reports it.
            if ask > bid + tick {
                price = ask - tick;
            } else {
                return judged;
            }
        }
        price.min(Decimal::new(99, 2))
    }
}

/// The narrowed surface handed to an extension when it is enabled. It carries
/// only an event sink clone and a snapshot of strategy names — deliberately NOT
/// the signer, venue client, UDS socket, order manager or any internal state.
struct CoreExtensionContext {
    tx: Option<mpsc::UnboundedSender<Event>>,
    strategies: Vec<String>,
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
fn dec_json(d: Decimal) -> serde_json::Value {
    use rust_decimal::prelude::ToPrimitive;
    serde_json::Value::from(d.to_f64().unwrap_or_default())
}

/// Feed/decision counters for P4 observability.
#[derive(Debug, Default, Clone)]
struct CoreStats {
    books: u64,
    tops: u64,
    spots: u64,
    rounds: u64,
    /// #377 (治 #2): archive verdict events replayed into the core — the
    /// count of on-chain resolutions that came from the stream instead of
    /// the dry ladder.
    resolutions: u64,
    /// #388: wallet-level cash rows credited from the stream (MAKER_REBATE /
    /// REWARD ledger rows), and the exact USD total each kind paid. The row
    /// count and the totals travel together so a report can reconcile the
    /// credit against the ledger it built.
    cashflow_rebates: u64,
    cashflow_rewards: u64,
    cashflow_rebates_usd: Decimal,
    cashflow_rewards_usd: Decimal,
    /// #427: confirmed cross-venue mapping rows replayed from the stream —
    /// the corpus-completeness anchor for the cross-venue strategies (a
    /// corpus that lost its `unified` rows reports zero instead of replaying
    /// "successfully" blind).
    unified_mappings: u64,
    evaluations: u64,
    signals: u64,
    place_rejected: u64,
    /// Entries rejected by a configured per-strategy cap (P-1.1).
    strategy_limit_rejected: u64,
    /// Orders the LIVE venue refused (place path), session-scoped.
    venue_rejected: u64,
    /// #406: entry intents refused fail-closed for a non-positive price
    /// (a prediction market has no legal quote <= 0; the ticket's sizing,
    /// policy and physics paths all divide by the price). Session-scoped.
    illegal_price_rejected: u64,
}

/// Backoff bookkeeping for one placement intent. Backoff is exponential from
/// [`REJECT_BACKOFF_BASE_MS`], capped at [`REJECT_BACKOFF_MAX_MS`]; after
/// [`MAX_PLACE_ATTEMPTS`] consecutive venue refusals the intent is no longer
/// retried at all, and if the refusals are consecutive engine-wide the trading
/// freeze trips (see `note_venue_rejection`).
#[derive(Debug, Default, Clone)]
struct RejectCooldown {
    attempts: u32,
    next_ok_ms: i64,
    last_error: String,
    /// The "abandoned after N rejections" alert for this intent was emitted.
    alerted: bool,
}

/// Base spacing between retries of a venue-rejected intent.
const REJECT_BACKOFF_BASE_MS: i64 = 2_000;
/// Retry spacing ceiling.
const REJECT_BACKOFF_MAX_MS: i64 = 30_000;
/// Attempts of ONE intent after which it stops being retried until a
/// successful placement or a restart clears the cooldown. A closing intent is
/// exempt from the cap (its backoff still applies): the position it closes is
/// unhedged exposure, and walking away leaves naked residual on the venue —
/// the "position never closes" failure class.
const MAX_PLACE_ATTEMPTS: u32 = 8;
/// Consecutive venue refusals that freeze trading (kill switch).
const FREEZE_ON_REJECTS: u32 = 5;
/// Consecutive failed reconciliation sweeps that freeze trading (E31-b): a
/// blind sweep is the failure mode that lets ghosts and orphans accumulate.
const SWEEP_FAILURE_FREEZE: u32 = 3;
/// 冻结自愈防抖：an AUTO-frozen account is only lifted by a reconcile sweep
/// after THIS long, so the flash outage that caused the freeze has to stay
/// quiet for ten minutes before the kernel trusts it again (真线 2026-10-28:
/// 断网恢复后 sweep 曾长期无处落地，冻结白挂数小时).
const FREEZE_AUTO_LIFT_MIN_MS: i64 = 600_000;
/// A benign close-intent rejection (Insufficient position) backoff-retried
/// THIS many times in a row (≈15–20 min under the exponential curve) before
/// the kernel shouts once: the venue's book and ours have diverged on one
/// position, and only a reconcile sweep (or a human) can close the gap.
const BENIGN_CLOSE_DIVERGENCE_ATTEMPTS: u32 = 40;

/// #462: how long a place-timeout order may sit pending with no venue evidence
/// (no id bound, no fold, nothing) before the reconciliation sweep retires it
/// and releases the reservation. Clears the bridge's 5 s sweep cadence plus a
/// slow POST's place timeout, so a merely-slow placement is never raced.
const PLACE_TIMEOUT_VERDICT_MS: i64 = 30_000;
/// A passing accounting audit is written to the audit log this often, so the log
/// proves the check RAN and not merely that it never failed (issue #189).
const AUDIT_HEARTBEAT_MS: i64 = 600_000;

/// The trade summary keeps realized PnL as `f64` (Node-compatible on disk); the
/// audit compares it against `Decimal` cash, so the conversion is explicit here
/// rather than an `as` cast in the comparison.
fn f64_to_decimal(v: f64) -> Decimal {
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
fn is_close_intent(internal_key: &str) -> bool {
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
fn exit_intent_key(prefix: &str, pos: &crate::position::OpenPosition, suffix: &str) -> String {
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
fn reject_backoff_ms(attempts: u32) -> i64 {
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
fn is_position_already_gone(rejection_message: &str) -> bool {
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
fn escalation_delay_ms(explicit_ms: i64, default_ms: i64) -> Option<i64> {
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
struct StrategyAccounting {
    placed: u64,
    rejected: u64,
    limit_rejected: u64,
    /// E9-c: rejection-cause buckets for this strategy. The cause label is the
    /// machine-parseable part (see `classify_rejection`), so an operator can
    /// see WHY entries bounced without grep-ing logs. Only counted buckets are
    /// serialized (empty map → absent from the row).
    rejection_causes: std::collections::BTreeMap<String, u64>,
    closed_trades: u64,
    wins: u64,
    losses: u64,
    fees_usd: Decimal,
    net_pnl_usd: Decimal,
}

/// The `strategy_accounting` map key for one (strategy, account) scope. The
/// `\u{1}` separator cannot appear in a strategy name (manifest names are
/// identifier-like), so the pair is unambiguous; the default account keeps
/// the same shape as every other account — `strategy_stats` folds the
/// account dimension out for single-name reads, so no legacy spelling is
/// preserved anywhere.
fn strategy_acc_key(strategy: &str, account: &str) -> String {
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
fn arb_name_branch_warn_once(strategy: &str) {
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
struct CycleLeg {
    internal_key: String,
    token: String,
    account: String,
    notional: Decimal,
}

/// RFC #541 挂钩 B (c)：one declared-atomic strategy's cycle legs grouped by
/// the account each spends — `(account_id, [(internal_key, notional)])`,
/// request order preserved within an account, sorted by account id so the
/// pot sweep is deterministic. The per-account pot dimension reads exactly
/// these shares: under the v2 dual-leg stamp a pair may span two accounts,
/// and each leg's notional is judged against the pot of the account it
/// spends (no pot is charged for legs it does not own).
fn per_account_shares(legs: &[CycleLeg]) -> Vec<(&str, Vec<(String, Decimal)>)> {
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
fn virtual_batch_commit_impl(
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
fn classify_rejection(err: &crate::model::CoreError) -> String {
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

/// Compact round info for the UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundView {
    pub slot: i64,
    pub age_sec: i64,
    pub time_left_sec: i64,
    pub markets: usize,
    pub can_trade: bool,
    /// Per-asset live prices (from the local book mid, falling back to the
    /// scanner's last price). For the UI Prices section.
    pub market_prices: Vec<MarketPriceView>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPriceView {
    pub asset: String,
    pub up: Decimal,
    pub down: Decimal,
}

/// 盘口深度 view (E8-c): one round asset with both token books, as the UI depth
/// chart renders them. Levels are best-first and capped per side by the caller.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetBooksView {
    /// 行自身的 venue 标签（镜像盘显示簿合成行，2026-10-25）：server 端
    /// 按它打标签，不再按 token 反查 —— 镜像行与被镜像所共享 token，
    /// `venue_of` 反查会首中被镜像所，标签就错了。
    #[serde(rename = "venue")]
    pub venue: String,
    pub asset: String,
    /// E29 (§12.2): the aggregator keys prediction-token bars by TOKEN id, so
    /// the panel needs the same key `feed_klines` was fed to ask
    /// `kline.history` for the right series.
    pub up_token_id: String,
    pub down_token_id: String,
    pub up: BookSideView,
    pub down: BookSideView,
}

/// One token's live book. Every metric is `null` when no book has arrived for
/// the token — never a zero stand-in, which the panel must not read as a real
/// quote (from_levels' empty-book sentinels stay internal to the strategy
/// layer, they are not observables).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookSideView {
    pub bids: Vec<BookLevelView>,
    pub asks: Vec<BookLevelView>,
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    pub mid_price: Option<Decimal>,
    /// Order-book imbalance (bid_depth − ask_depth) / (bid_depth + ask_depth).
    pub obi: Option<Decimal>,
    pub spread: Option<Decimal>,
    pub spread_pct: Option<Decimal>,
}

impl BookSideView {
    fn empty() -> Self {
        Self {
            bids: Vec::new(),
            asks: Vec::new(),
            best_bid: None,
            best_ask: None,
            mid_price: None,
            obi: None,
            spread: None,
            spread_pct: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookLevelView {
    pub price: Decimal,
    pub size: Decimal,
}

/// Tests for the service kernel. One file per theme; the originals were
/// moved here verbatim (pure code motion, no logic edits) to bring
/// `service.rs` under the 256KB size cap. Shared fixtures stay in this
/// parent module and reach the child files through `use super::*`.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::RiskConfig;
    use rust_decimal_macros::dec;

    fn dry_core(balance: Decimal) -> Core {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(3),
                ..Default::default()
            },
            dry_seed_balance: Decimal::from(10_000),
            // #457: pinned so a default change cannot silently arm the expiry
            // sweep in fixtures that jump the clock hours ahead.
            round_duration_sec: 900,
            ..Default::default()
        });
        c.set_balance(balance);
        c
    }

    fn order(mode: FillPolicy, price: Decimal, size: Decimal, key: &str) -> OrderRequest {
        OrderRequest {
            token_id: "tok".into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode,
            price,
            size,
            internal_key: key.into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
            account_id: crate::model::default_account_id(),
        }
    }

    // ── #458: system.reset_trading —— 三道门 + 重置闭环（直接单测） ─────────

    fn reset_params(confirm: &str) -> crate::ipc::schema::ResetTradingParams {
        crate::ipc::schema::ResetTradingParams {
            confirm: confirm.to_string(),
            reason: Some("unit".to_string()),
            account_id: None,
        }
    }

    fn reset_audit_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bk-reset-unit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn live_core(balance: Decimal) -> Core {
        let mut c = Core::new(CoreConfig {
            mode: Mode::Live,
            risk: RiskConfig {
                max_order_notional: dec!(3),
                ..Default::default()
            },
            dry_seed_balance: Decimal::from(10_000),
            ..Default::default()
        });
        c.set_balance(balance);
        c
    }

    mod account_precision_tests;
    mod audit_fix_tests;
    mod books_mirror_tests;
    mod exit_reason_table_tests;
    mod expiry_fallback_tests;
    mod fee_quote_tests;
    mod fill_model_tests;
    mod freeze_self_heal_tests;
    mod order_state_machine_462_tests;
    mod round_expiry_tests;
    mod round_family_b1b_tests;
    mod settlement_service_tests;
    mod shadow_evolution_tests;
    mod strategy_dispatch_tests;
    mod strategy_params_tests;
    mod tests_account_reset;
    mod tests_place_fill;
    mod tests_reset_trading;
    mod tests_strategy_lib;
    mod tests_subscription;
    mod tests_venue_lifecycle;
    mod trading_capability_tests;
}
