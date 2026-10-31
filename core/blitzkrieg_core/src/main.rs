//! blitzkrieg-core entrypoint.
//!
//! Serves the trading core over a Unix-domain-socket JSON-RPC 2.0 channel.
//! Node spawns this process and owns its lifecycle; credentials are read from
//! the environment inside the process (never supplied by Node).
//!
//! Usage:
//!   blitzkrieg-core --socket <path> [--mode dry|live] [--tick-ms 50]
//!                   [--readonly]
//!                   [--seed-balance 10000] [--max-order-notional 100]
//!                   [--min-shares 10] [--max-shares 10]
//!                   [--market-plugin <name>]
//!                   [--trade-log <path>] [--no-trade-log]
//!                   [--order-log <path>] [--no-order-log]
//!                   [--position-log <path>] [--no-position-log]
//!                   [--allow-shared-data]
//!                   [--near-miss-path <path>]
//!                   [--event-archive <path>] [--no-event-archive]
//!                   [--event-archive-max-mb 0]
//!                   [--event-archive-rotate-mb 256]
//!                   [--event-archive-min-free-mb 5120]
//!                   [--entry-maker-timeout-ms 5000]
//!                   [--max-orderbook-stale-ms 8000]
//!                   [--slippage-ticks 0] [--latency-ms 0] [--fill-prob-bps 10000]
//!                   [--maker-depth-share-bps 10000]
//!                   [--dry-redeem-fail 0] [--dry-redeem-manual]
//!                   [--net-check]
//!                   [--blueprint-compile <blueprint.json> [--blueprint-out <path>]]
//!
//! `--max-orderbook-stale-ms <ms>` (env `BK_MAX_ORDERBOOK_STALE_MS`) is how old
//! an orderbook may be before the engine refuses to price off it — the knob that
//! decides how long after the feed goes quiet the bot stops taking entries.
//! Default 8000 (the value every deployment has always run with). `0` disables
//! the check entirely (any book age is accepted); a negative value, a
//! non-numeric value or anything past the 600000 ms ceiling is a startup error
//! (exit 2), never a silent fallback. Closing intents are never blocked by it.
//!
//! `--readonly` is a promise, not a convention: the venue order egress is never
//! constructed, so no code path in this process can send an order. It outranks
//! `--mode live` / `DRY_RUN=false` (giving both is not an error — the read-only
//! promise simply wins) and settles orders like dry, since a ledger waiting on a
//! venue that was never started would never move.
//!
//! #199 — the data-directory latch. The ledgers ARE the accounting truth the
//! risk limits rest on, so a second writer is not a logging accident: it breaks
//! the trade/order/position identities `max_daily_loss_usd`, the loss breakers
//! and the reconcile audit are computed from, and polluted rows are
//! indistinguishable from real ones. At startup this process therefore claims
//! the parent directory of every EFFECTIVE ledger path (`--trade-log`,
//! `--order-log`, `--position-log`) by writing `.core-lock` — `{pid,
//! started_at, socket, mode, version}` — into it, and REFUSES TO START (exit 1,
//! not one byte written anywhere) if a record is already there and its pid is
//! alive. A record whose pid is gone is taken over, and the boot banner says so.
//! The banner always prints the absolute path of every file this process may
//! write, so "where did that run write?" is answered at boot rather than
//! reconstructed from a cwd afterwards. This is the DATA conflict; #196's socket
//! probe is the separate SOCKET conflict (who is listening). `--allow-shared-data`
//! (or `BLITZKRIEG_ALLOW_SHARED_DATA=1`) is the fixture/backtest escape hatch:
//! the refusal is disabled and the banner states loudly that the directory has
//! two writers. Never use it for a deployment.
//!
//! Strategy selection (repeatable; the kernel ships ZERO enabled strategies, so
//! a new strategy never changes what a running session trades until it is
//! named):
//!                   [--enable-strategy <name>] [--disable-strategy <name>]
//!                   [--allow-zero-strategies]
//!   An explicit `--enable-strategy` that resolves to NOTHING is a refusal, not a
//!   warning (#265): the kernel would otherwise boot with a healthy banner and
//!   trade nothing because the library it was told to run never loaded. The
//!   opt-out is explicit (`--allow-zero-strategies`), the same rule as an unknown
//!   argument.
//!
//! Exit ladder (E17/#264; engine-level — exits belong to the kernel, not to a
//! strategy dylib):
//!                   [--exit-take-profit-pct <pct>] [--exit-stop-loss-pct <pct>]
//!                   [--exit-trailing-min-high-pct <pct>]
//!                   [--exit-min-trail-pct <pct>]
//!   Resolved CLI > `[exit]` in the config file > the shipped `ExitConfig`
//!   default, so leaving all four out is byte-for-byte the ladder that was
//!   compiled in before these flags existed.
//!
//! Market-data capture is ON by default for an engine session (`--no-event-archive`
//! disables it): the events behind a past stop-out only exist if recording was
//! already running when it happened.
//!
//! Backtest (offline; forces dry mode, starts no feeds and writes no logs):
//!   blitzkrieg-core --backtest <archive.jsonl> [--backtest-report <path>]
//!                   [--backtest-tick-ms 50] [--backtest-tail-ms 0] [model flags]
//!                   [--backtest-knob <strategy>:<knob>=<value>]...
//!                   [--fee-model <legacy_quadratic|official>] [strategy knobs:
//!                    --trend-confirm-sec --spread-arb-entry-factor
//!                    --spread-arb-min-obi --spread-arb-max-spread-pct
//!                    --spread-arb-dip-max-pct --spread-arb-bounce-min-pct
//!                    --spread-arb-bounce-window-sec
//!                    --exit-take-profit-pct --exit-stop-loss-pct
//!                    --exit-trailing-min-high-pct --exit-min-trail-pct]
//!   The CoreConfig knobs flow through CoreConfig → engine_config, the same
//!   mapping the live server uses, so a sweep is a pure CLI variation with no
//!   rebuild (E15). `--backtest-knob` is the other half: a knob a strategy
//!   declares but CoreConfig does not carry (the cdylibs' own parameters) is
//!   handed to the replayed strategy through the Shadow Evolution hot-param
//!   registry — the same cell, the same `on_hot_params` push as a live
//!   evolution — so a counterfactual arm differs from the shipped one by exactly
//!   that value (see `scripts/mean-reversion-gate-evidence.mjs`).
//!   `--fee-model` is the fee-side counterpart (#203): the same corpus replayed
//!   under a different taker-fee schedule, so "what would the published crypto
//!   rate cost this strategy" is a measurement rather than an argument (see
//!   `scripts/fee-model-sensitivity-check.mjs`).
//!
//! Network self-check (a diagnostic, not a mode — no socket, no ledger, no
//! order, no credential, and it is answered before the #199 latch, so it is safe
//! to run against a live deployment):
//!   blitzkrieg-core --net-check
//!   Answers the question behind a bot that has gone quiet — "is it us or the
//!   venue?" — by probing the SAME hosts the live paths use (venue REST,
//!   discovery, spot stream, venue user stream) through resolver → TCP → TLS →
//!   one cheap request each, and printing ONE JSON `NetCheckReport` on stdout.
//!   The wording is deliberately absent from the core: the launcher, the TUI and
//!   the WebUI render that struct in Chinese, so the core keeps a
//!   language-neutral data contract. Exit 0 when every probe passed, 1 when any
//!   failed — and the report is printed either way, so a failure is readable
//!   rather than inferred from the code. A `CLOB_API_URL` / `POLYMARKET_WS_URL`
//!   override pointing at a loopback or private address is reported as
//!   `rejected` instead of being dialled, and proxy-related environment
//!   variables are NAMED, never printed with their values (a proxy URL may carry
//!   credentials).
//!
//! Env (live): POLYMARKET_PRIVATE_KEY, POLYMARKET_FUNDER_ADDRESS, CLOB_API_URL.
//!
//! Blueprint compile (#361; offline, like --net-check — answered before the
//! #199 latch, no socket, no ledger, no order):
//!   blitzkrieg-core --blueprint-compile <blueprint.json> [--blueprint-out <path>]
//!   Compiles a version-1 blueprint JSON decision graph (8 node types, three
//!   validation stages: structure → semantics → codegen) into a human-readable
//!   Lua strategy body carrying `on_tick(tick)` + `declare_modes()`. Errors
//!   name the node id and the reason. The default output is `strategy.lua`
//!   beside the blueprint.

use blitzkrieg_core::ipc::server;
use blitzkrieg_core::model::Mode;
use blitzkrieg_core::position::PositionConfig;
use blitzkrieg_core::risk::RiskConfig;
use blitzkrieg_core::service::CoreConfig;
use rust_decimal::Decimal;

// #699 — main.rs 体积拆分：boot/subcommands/tests 三列纯代码搬移。
// 子文件经 `mod` 挂载在 bin 根；下列 `use` 重导出让 main() 的裸名引用
// 路径与拆分前完全一致（零逻辑改动）。
mod bin_support;
#[cfg(test)]
mod bin_tests;

use bin_support::boot::{
    ConfigChoice, EnvVars, config_choice, daily_loss_path_for, parse_args, parse_strategy_limits,
    parse_virtual_limits, resolve_event_archive, resolved_policy_path, round_duration_source_from,
    run_net_check,
};
use bin_support::subcommands::{
    run_backtest, run_blueprint_compile, run_mode_backtest, run_regime_eval, run_replay,
    run_replay_near_miss,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // #184: INFO by default, not ERROR — `EnvFilter::from_default_env()` with no
    // RUST_LOG kept only ERROR, so a deployed run's warn/error breadcrumbs never
    // reached the run log while the process looked healthy. An explicit RUST_LOG
    // still wins outright. `logging` is the one spelling of that rule; the
    // effective level is echoed below so the run log states it.
    tracing_subscriber::fmt()
        .with_env_filter(blitzkrieg_core::logging::log_filter())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    eprintln!(
        "blitzkrieg-core: log level {} (source: {})",
        blitzkrieg_core::logging::effective_level(),
        blitzkrieg_core::logging::level_source()
    );

    // Configuration is loaded BEFORE the arguments, because the arguments are
    // resolved against it (CLI > env > TOML > default).
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // `--version` is answered before anything else is touched: it must work with
    // no config file, no socket, no data/ and no git, and must not boot a core.
    // The string carries the built revision (`<semver>+g<sha>`, #179), which is
    // what lets a gate compare the binary under test against the commit it is
    // checking instead of trusting whatever is on disk (#172).
    if argv.iter().any(|a| a == "--version" || a == "-V") {
        println!("{}", blitzkrieg_core::ipc::build_info::version_string());
        return Ok(());
    }

    // #228: `--help` is answered here, beside `--version` and for the same
    // reason — it must work with no config file, no socket, no data/ and no git,
    // and it must NEVER boot a core. It reads the accepted set out of
    // `blitzkrieg_core::cli::FLAGS`, the same table the parser's fallback arm
    // consults, so the list an operator checks a spelling against is the list the
    // parser enforces (a test in that module pins the two together). `--version`
    // is checked first, so a command with both keeps the pre-#228 answer.
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", blitzkrieg_core::cli::help_text());
        return Ok(());
    }

    let env = EnvVars::from_process();
    let choice = config_choice(&argv, &env);
    let file = match &choice {
        ConfigChoice::Off => blitzkrieg_core::config::FileConfig::default(),
        ConfigChoice::Path(p) => {
            blitzkrieg_core::config::FileConfig::load(Some(std::path::Path::new(p)))
        }
    };
    for w in &file.warnings {
        eprintln!("blitzkrieg-core: config warning: {w}");
    }
    // An unknown key is the exact failure mode this layer exists to prevent
    // (KI-11), so it is loud — but never fatal.
    for k in &file.unknown_keys {
        eprintln!("blitzkrieg-core: config key not understood (ignored): {k}");
    }

    let args = parse_args(&file, &argv, &env);

    // Say where the non-default settings came from. A declarative config layer is
    // only auditable if the effective value and its origin are both observable.
    if let Some(p) = &file.path {
        eprintln!("blitzkrieg-core: config file {}", p.display());
    }
    for line in &args.config_report {
        eprintln!("blitzkrieg-core: config {line}");
    }
    // #173: the daily-loss breaker's effective budget, echoed whether or not any
    // layer configured it — a run that set nothing must still say what protects
    // it (the old default was silently uncapped for a small book).
    for line in &args.daily_loss_echo {
        eprintln!("blitzkrieg-core: {line}");
    }
    // #202: what one order may commit on THIS account. Printed unconditionally
    // for the same reason as the breaker's budget above: the answer must not
    // depend on someone having thought to configure (or grep) it.
    for line in &args.sizing_echo {
        eprintln!("blitzkrieg-core: {line}");
    }
    // #205: the freshness budget in force, printed unconditionally for the same
    // reason: "how long after the feed goes quiet does this bot stop trading?"
    // must be answerable from the boot log of a run that set no flags.
    for line in &args.orderbook_stale_echo {
        eprintln!("blitzkrieg-core: {line}");
    }
    // 任务二 1-3: the NTP probe posture, printed unconditionally — the answer
    // to "is this run watching the system clock, and against whom?" belongs in
    // the boot log, not in a grep of the flags that happen to be set.
    for line in &args.ntp_echo {
        eprintln!("blitzkrieg-core: {line}");
    }
    // E26 (§4): the systemic risk limits in force, resolved from the config
    // file's [risk] section — this Epic's only wired input — and echoed
    // unconditionally like every other budget above: the factory posture
    // (all nine off, §4.1) must be visible in the boot log, not greppable.
    // The set is then handed to the core through `RiskConfig`, the one
    // hand-off `RiskGate::new` already makes.
    let systemic = blitzkrieg_core::risk::limits::SystemicRiskLimits::from_file(&file.risk);
    // E26 (§4.4): the drawdown bound on the readout names the TRUE layer that
    // armed the budget. The resolver already picked CLI > env > [risk] > the
    // compiled default for the daily-loss breaker — the matrix must agree
    // with what it prints, not silently claim `default` (or `toml`) for a
    // number a flag overrode.
    let systemic = {
        let mut s = systemic;
        s.account.max_daily_drawdown_usd = if args.daily_loss_usd > Decimal::ZERO {
            blitzkrieg_core::risk::limits::Bound::new(
                args.daily_loss_usd,
                args.daily_loss_source.into(),
            )
        } else {
            blitzkrieg_core::risk::limits::Bound::off()
        };
        s
    };
    let explicit_ladder: Vec<blitzkrieg_core::arbitration::LadderStep> =
        file.risk.ladder.iter().map(|row| row.to_step()).collect();
    for line in systemic.report_lines() {
        eprintln!("blitzkrieg-core: {line}");
    }
    // #269: same reason again — "is the evolution evaluator running?" is the
    // question an incident responder asks first, and it used to be answerable
    // only from the IPC status, never from the boot log.
    for line in &args.shadow_evolution_echo {
        eprintln!("blitzkrieg-core: {line}");
    }

    // Which code this process is (#179). On stderr and deliberately NOT through
    // `tracing`: the filter is configurable (`RUST_LOG=error` is a legitimate
    // operator choice, #184), so a provenance line sent to the log layer could be
    // absent from exactly the runs later asked "which build was that?".
    eprintln!(
        "blitzkrieg-core: {}",
        blitzkrieg_core::ipc::build_info::provenance_line()
    );

    // A diagnostic, not a mode: answered before the mode is decided, before any
    // data directory is claimed (it writes nothing), and before the replay /
    // backtest runners — so it needs no socket, no ledger and no `data/`, and is
    // safe to run against a live deployment. It reads the parsed args only, which
    // is why it sits here and not beside the other one-shot runners: those are
    // reached after `CoreConfig` is built, and `CoreConfig` consumes the very
    // field (`market_plugin`) this needs.
    if args.net_check {
        std::process::exit(run_net_check(args.market_plugin.as_deref()).await);
    }

    // DRY_RUN env honours the existing convention when --mode is not explicit.
    let mode = if std::env::args().any(|a| a == "--mode") {
        args.mode
    } else if std::env::var("DRY_RUN")
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        Mode::Live
    } else {
        Mode::Dry
    };

    // `--readonly` is decided LAST and unconditionally, so nothing above can
    // outrank it. Deciding it in the same chain would make the promise depend on
    // clause order — exactly the kind of thing that reads correct and is not.
    let mode = if args.readonly {
        if mode == Mode::Live {
            eprintln!(
                "blitzkrieg-core: --readonly given with a live mode request; \
                 refusing egress (orders will settle locally, no venue is started)"
            );
        }
        Mode::ReadOnly
    } else {
        mode
    };

    // When the self-driving engine is on, record near-misses to disk by default
    // so the entry-gate question can be evaluated offline.
    let near_miss_path = if args.engine {
        args.near_miss_path
            .clone()
            .or_else(|| Some("data/shadow/near-miss.jsonl".to_string()))
    } else {
        args.near_miss_path.clone()
    };

    // Trade log destination. The DEFAULT is derived from the resolved run
    // mode (`crate::trade_db::default_trade_log_path`): live derives
    // `data/trades/trades-live.jsonl`, dry/readonly keep the historical
    // `data/trades/trades.jsonl` (dry tooling + the dry-era rows stay put).
    // `--trade-log <path>` overrides both (tests pin their own file) and
    // `--no-trade-log` disables persistence entirely.
    let trade_log_path =
        if args.no_trade_log {
            None
        } else {
            Some(args.trade_log.clone().unwrap_or_else(|| {
                blitzkrieg_core::trade_db::default_trade_log_path(mode).to_string()
            }))
        };

    // Order log: durable tracked-order storage for crash recovery + orphan sweep.
    let order_log_path = if args.no_order_log {
        None
    } else {
        Some(
            args.order_log
                .clone()
                .unwrap_or_else(|| "data/orders/orders.jsonl".to_string()),
        )
    };

    // Position log: durable snapshot of the OPEN position book (crash recovery).
    let position_log_path = if args.no_position_log {
        None
    } else {
        Some(
            args.position_log
                .clone()
                .unwrap_or_else(|| "data/positions/positions.jsonl".to_string()),
        )
    };

    // Per-order share sizing. Both default to 10 (fixed lot) when neither flag is
    // given. Clamp min<=max so a stray flag order cannot invert the range.
    let (min_shares, max_shares) = {
        let mn = args.min_shares.unwrap_or_else(|| Decimal::from(10));
        let mx = args.max_shares.unwrap_or_else(|| Decimal::from(10));
        if mn > mx {
            eprintln!(
                "blitzkrieg-core: --min-shares {mn} > --max-shares {mx}; clamping min to max"
            );
            (mx, mx)
        } else {
            (mn, mx)
        }
    };

    // Market-data capture is ON by default for an engine session. The data for a
    // past stop-out only exists if recording was already running when it happened,
    // which makes "opt in" the wrong default for the one thing that cannot be
    // reconstructed after the fact. Rotation + a free-space floor are what make
    // leaving it on safe, so the default supplies those too.
    let (
        event_archive_path,
        event_archive_max_mb,
        event_archive_rotate_mb,
        event_archive_min_free_mb,
    ) = resolve_event_archive(
        args.engine,
        args.no_event_archive,
        args.event_archive.as_deref(),
        args.event_archive_max_mb,
        args.event_archive_rotate_mb,
        args.event_archive_min_free_mb,
    );

    // What starts enabled is the OPERATOR'S intent, never a hardcoded name:
    // the persisted set (runtime toggles + earlier boot flags) first, then the
    // explicit CLI adds on top. `--disable-strategy` still wins over both — it
    // is applied after them in `install_engine`. A fresh checkout (no state
    // file) boots with ZERO strategies enabled; the panel's toggles then build
    // the set and it persists from the first change on.
    let mut enabled_strategies = args
        .strategy_state
        .as_deref()
        .map(|p| blitzkrieg_core::strategy_state::load(std::path::Path::new(p)))
        .unwrap_or_default();
    for s in &args.enable_strategy {
        if !enabled_strategies.contains(s) {
            enabled_strategies.push(s.clone());
        }
    }

    // #173: the daily-loss budget's durable side file follows the position log
    // (the struct literal below consumes `position_log_path`).
    let daily_pnl_path = position_log_path.as_deref().map(daily_loss_path_for);

    // #380: WHERE the execution-policy file lives, settled BEFORE the core is
    // constructed. An explicit --execution-policy wins; the shipped default is
    // resolved against the install root (exe ancestors) then the cwd (cwd
    // ancestors), and the outcome is echoed above the provenance line — the
    // boot log states the effective file, the base that decided, or (loudly)
    // that no policy was found and the kernel runs the neutral defaults.
    let resolved_policy = resolved_policy_path(args.execution_policy.as_deref());
    let execution_policy_path = match &resolved_policy {
        blitzkrieg_core::execution_policy::ResolvedPolicyPath::Explicit(p) => {
            Some(p.to_string_lossy().into_owned())
        }
        blitzkrieg_core::execution_policy::ResolvedPolicyPath::Default(r) => {
            Some(r.path.to_string_lossy().into_owned())
        }
    };

    let mut config = CoreConfig {
        mode,
        // Replay maintenance-gating fast path: replay-only, refuse-guarded in
        // the CLI parser (mirrors --fee-model). Default false = the historic
        // every-tick behaviour, byte-for-byte.
        backtest_fast: args.backtest_fast,
        default_maker_timeout_ms: 5000,
        risk: RiskConfig {
            max_order_notional: args.max_order_notional,
            // #202: the same bound as a share of the account (0 = off).
            max_order_notional_pct: args.max_order_notional_pct,
            max_open_notional_usd: args.max_open_notional,
            // E26 (§4): the systemic limits resolved from the config file's
            // [risk] section (factory = all off, §4.1), plus an explicit
            // [[risk.ladder]] when the operator opted in (§4.3).
            systemic,
            explicit_ladder: explicit_ladder.clone(),
            ..Default::default()
        },
        dry_seed_balance: args.seed_balance,
        // #205: the validated freshness budget (0 = check off).
        max_orderbook_stale_ms: args.max_orderbook_stale_ms,
        // 任务二 1-3: the NTP probe's boot configuration. The server spawns the
        // measurement loop only for a non-empty host with a positive interval;
        // a replay never reaches that code path (production-only by placement).
        ntp_host: args.ntp_host.clone(),
        ntp_interval_secs: args.ntp_interval_secs,
        strategy_limits: parse_strategy_limits(&args.strategy_limits),
        // E-VA: the virtual pots backing every strategy that declared
        // `atomic_batch`. Malformed entries are dropped with a warning; a pot
        // with no cap (`-`) is dropped too — an armed-but-uncapped pot would
        // silently flip the atomic posture on while enforcing nothing.
        virtual_limits: parse_virtual_limits(&args.virtual_limits),
        enabled_strategies,
        disabled_strategies: args.disable_strategy,
        // #265: what THIS invocation asked for by flag (the persisted set is
        // merged in above but is not an explicit request — a name that no longer
        // resolves there stays a warning). The startup self-check refuses the
        // boot when this list resolves to nothing and `--allow-zero-strategies`
        // was not given.
        requested_strategies: args.enable_strategy.clone(),
        allow_zero_strategies: args.allow_zero_strategies,
        strategy_state_path: args.strategy_state,
        strategy_dir: args.strategy_dir,
        lua_strategy_dir: args.lua_strategy_dir,
        markets: args.markets,
        auto_exits_enabled: args.auto_exits,
        intent_audit_enabled: !args.no_intent_audit,
        engine_enabled: args.engine,
        min_shares,
        max_shares,
        // #202: equity-relative per-entry budget (0 = the absolute size_usd path,
        // i.e. every deployment's current order sizes, unchanged).
        size_pct: args.size_pct,
        near_miss_path,
        trade_log_path,
        order_log_path,
        position_log_path,
        ledger_journal_dir: args.ledger_journal_dir.clone(),
        discovery_enabled: args.discovery,
        update_check_enabled: args.update_check_enabled.unwrap_or(false),
        update_auto: args.update_auto.unwrap_or(false),
        update_interval_secs: args.update_interval_secs.unwrap_or(0).max(0) as u64,
        shadow_evolution_enabled: args.shadow_evolution,
        shadow_evolution_tuning: if args.se_min_samples.is_some()
            || args.se_cooldown_secs.is_some()
            || args.se_min_obs_secs.is_some()
            || args.se_eval_window_secs.is_some()
            || args.se_max_gradient.is_some()
            || args.se_variant_count.is_some()
            || args.se_audit_dir.is_some()
            || args.se_min_win_rate.is_some()
            || args.se_min_profit_factor.is_some()
            || args.se_auto_evolve.is_some()
            || args.se_cycle_secs.is_some()
            || args.se_ttl_secs.is_some()
            || args.se_deep_dims.is_some()
        {
            Some(blitzkrieg_core::service::ShadowEvolutionTuning {
                min_sample_count: args.se_min_samples,
                cooldown_secs: args.se_cooldown_secs,
                min_observation_secs: args.se_min_obs_secs,
                evaluation_window_secs: args.se_eval_window_secs,
                max_gradient: args.se_max_gradient,
                variant_count: args.se_variant_count,
                min_win_rate_improvement: args.se_min_win_rate,
                min_profit_factor_improvement: args.se_min_profit_factor,
                audit_dir: args.se_audit_dir.clone(),
                auto_evolve: args.se_auto_evolve,
                evolution_cycle_secs: args.se_cycle_secs,
                proposal_ttl_secs: args.se_ttl_secs,
                deep_dims: args.se_deep_dims,
            })
        } else {
            None
        },
        assets: args.assets.clone(),
        min_round_age_sec: args.min_round_age,
        trend_confirm_sec: args.trend_confirm,
        trend_window_floor_ms: args.trend_floor_ms,
        spread_arb_trend_entry_factor: args.spread_arb_entry_factor,
        spread_arb_entry_min_obi: args.spread_arb_min_obi,
        spread_arb_entry_max_spread_pct: args.spread_arb_max_spread_pct,
        spread_arb_entry_dip_max_pct: args.spread_arb_dip_max_pct,
        spread_arb_entry_bounce_min_pct: args.spread_arb_bounce_min_pct,
        spread_arb_entry_bounce_window_sec: args.spread_arb_bounce_window_sec,
        feed_ws_enabled: args.feed_ws,
        market_plugin: args.market_plugin,
        round_duration_sec: args.round_sec,
        // The round-duration UI card's persisted cell (data/runtime/config.json)
        // outranks the factory TOML and the compiled default; only an explicit
        // CLI flag or BK_* env var beats it (the bit-exact gate baselines run
        // with those flags, so they can never be hijacked by the card). The
        // config_report names the winning layer: "(cli)"/"(env)" suppress the
        // cell below; "(toml)" does not — the cell still applies (the site
        // further down reads it whenever the source is not "cli_or_env").
        round_duration_source: round_duration_source_from(&args.config_report),
        // The durable cell the round-duration UI card writes (cwd-relative,
        // like every other `data/` writer). Always on for a real process —
        // the backtester builds its own CoreConfig with None.
        runtime_config_path: Some("data/runtime/config.json".to_string()),
        // The single persistent credential home (`market.credentials.set`
        // upserts here). cwd-relative — the same file the launcher loads at
        // start (`ui_kit_panel::env_file` reads `<cwd>/.env`), so a UI write
        // and the launcher read never disagree about which file is the truth.
        // Always on for a real process; the backtester builds None and the
        // credentials write path refuses.
        env_file_path: Some(".env".to_string()),
        // ④ 统一映射账本（cwd-relative）。confirmed 的跨所映射跨重启存活；
        // 自动配对只提议、操作员 confirm 才写这里。回测/测试 None = 纯内存。
        unified_ledger_path: Some("data/runtime/unified-mapping.json".to_string()),
        // #458: where manual destructive operations audit themselves. Always
        // on for a real process; the backtester/tests carry None and refuse.
        manual_ops_audit_path: Some("data/audit/manual-ops.jsonl".to_string()),
        // The account book's config source (E28 §9.5, cwd-relative like every
        // other `data/`-family writer). `Core::new` already LOADS the book
        // from this exact path — without naming it here too, every runtime
        // write face (`account.venue.set`, and the panel's account
        // create/remove) refuses with "persistence is disabled". The
        // backtester zeroes it again (backtest.rs), so a replay can never
        // write the operator's file.
        accounts_config_path: Some(
            blitzkrieg_core::account::config::ACCOUNTS_CONFIG_PATH.to_string(),
        ),
        // #363: the EXPLICIT flag only (None = let the policy file decide).
        max_positions_per_asset: args.max_positions_per_asset,
        positions: PositionConfig {
            max_positions: args.max_positions,
            // #363: `None` = the flag is absent → the policy file's value (or
            // the compiled 1) flows through in service.rs; `Some` = the flag
            // wins over the file. 1 is the historical one-per-asset gate.
            max_positions_per_asset: args.max_positions_per_asset.unwrap_or(1),
            // #173: the budget comes from the CLI/env, and its durable side file
            // is derived from the position log (the same convention as the
            // `.recon` watermark beside it) so a restart resumes the SAME day's
            // realized loss instead of laundering it.
            max_daily_loss_usd: args.daily_loss_usd,
            max_daily_loss_equity_pct: args.daily_loss_pct,
            daily_pnl_path: daily_pnl_path.clone(),
            exit: blitzkrieg_core::exit_policy::ExitConfig {
                min_time_left_sec: args.min_time_left,
                // E17/#264: the engine-level exit-ladder knobs. Their defaults
                // are `ExitConfig::default()`'s own values, so a run that sets
                // none of them is exactly the pre-flag kernel — which since the
                // @almach calibration means force_exit OFF (0), trailing OFF.
                take_profit_pct: args.exit_take_profit_pct,
                stop_loss_pct: args.exit_stop_loss_pct,
                trailing_min_high_pct: args.exit_trailing_min_high_pct,
                min_trail_pct: args.exit_min_trail_pct,
                force_exit_sec: args.exit_force_sec,
                ..Default::default()
            },
            // Issue #464: the operator's `[[risk.ladder]]` also drives the
            // per-position exit overlay. The arbitration face keeps its own
            // copy (Gate 4's trace); both read the same file rows, so there is
            // exactly one source. Empty when the operator opted out — the
            // exit sweep is then byte-identical to its historical behavior.
            exit_ladder: explicit_ladder.clone(),
            ..Default::default()
        },
        entry_maker_timeout_ms: args.entry_maker_timeout_ms,
        // #380: the RESOLVED policy path (absolute when the install root or
        // the cwd supplied it) — the library reads what it is handed, the
        // install-root logic lives above, once, where the boot log can name
        // the decision.
        execution_policy_path,
        fill_model: blitzkrieg_core::sim::FillModel {
            taker_slippage_ticks: args.slippage_ticks,
            maker_latency_ms: args.latency_ms,
            maker_fill_prob_bps: args.fill_prob_bps.unwrap_or(10_000),
            maker_depth_share_bps: args.maker_depth_share_bps.unwrap_or(10_000),
            taker_latency_ms: args.taker_latency_ms,
            taker_rtt_ms: args.taker_rtt_ms,
        },
        event_archive_path,
        event_archive_max_mb,
        event_archive_rotate_mb,
        event_archive_min_free_mb,
        // #175 gate hooks: dry-mode redemption failure injection, off unless the
        // flag is given (see `CoreConfig::dry_redeem_fail`).
        dry_redeem_fail: args.dry_redeem_fail,
        dry_redeem_manual: args.dry_redeem_manual,
        ..Default::default()
    };

    if let Some(path) = &args.replay {
        run_replay(std::path::Path::new(path));
        return Ok(());
    }
    if let Some(path) = &args.replay_near_miss {
        run_replay_near_miss(std::path::Path::new(path));
        return Ok(());
    }
    if let Some(wallet) = &args.onchain_pull {
        // #352: the offline on-chain pull — Polymarket Data/Gamma/CLOB APIs →
        // the backtest JSONL event stream under data/onchain. Like
        // --regime-eval it needs no mode, no feeds and never trades; unlike
        // it, the pull is async (page-sized progress on stderr, never a
        // blocking loop without feedback). #648 C: the kernel keeps only this
        // operator entry point — the implementation lives in the
        // `blitzkrieg-onchain-pull` crate and is called unchanged.
        let (Some(start), Some(end)) = (&args.onchain_start, &args.onchain_end) else {
            eprintln!(
                "blitzkrieg-core: --onchain-pull needs --onchain-start and --onchain-end \
                 (YYYY-MM-DD or epoch seconds, UTC)"
            );
            std::process::exit(2);
        };
        let start_sec = match blitzkrieg_onchain_pull::parse_time_arg(start) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("blitzkrieg-core: --onchain-start: {e}");
                std::process::exit(2);
            }
        };
        let end_sec = match blitzkrieg_onchain_pull::parse_time_arg(end) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("blitzkrieg-core: --onchain-end: {e}");
                std::process::exit(2);
            }
        };
        // The end date is inclusive: a `--onchain-end 2026-10-01` covers the
        // whole day, not the midnight instant. (An epoch arg is the instant
        // itself — no day to expand.)
        let end_is_date = end.trim().parse::<i64>().is_err();
        let end_inclusive_sec = if end_is_date {
            end_sec + 86_400 - 1
        } else {
            end_sec
        };
        let out_dir = args
            .onchain_out_dir
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("data/onchain"));
        let req = blitzkrieg_onchain_pull::PullRequest {
            wallet: wallet.clone(),
            start_ms: start_sec * 1000,
            end_ms: end_inclusive_sec * 1000,
            asset: args.onchain_asset.clone(),
            market: args.onchain_market.clone(),
            out_dir,
            page_limit: 500,
            fill_source: match &args.onchain_fills {
                Some(path) => blitzkrieg_onchain_pull::FillSource::ActivityFile(
                    std::path::PathBuf::from(path),
                ),
                None => Default::default(),
            },
        };
        let fetch = blitzkrieg_onchain_pull::HttpFetcher::new(150);
        let outcome = blitzkrieg_onchain_pull::pull_and_convert(
            &fetch,
            &req,
            &|p: &blitzkrieg_onchain_pull::PullProgress| {
                eprintln!("onchain {}: {} — {}", p.phase, p.fetched, p.detail);
            },
        )
        .await;
        match outcome {
            Ok(o) => {
                println!(
                    "onchain: {} trades → {} events\n  trades:  {}\n  events:  {}\n  manifest: {}{}",
                    o.trades,
                    o.events,
                    o.trades_path.display(),
                    o.events_path.display(),
                    o.manifest_path.display(),
                    if o.cache_hit {
                        "\n  (cache hit, sha256 verified)"
                    } else {
                        ""
                    },
                );
                return Ok(());
            }
            Err(e) => {
                eprintln!("blitzkrieg-core: onchain pull failed: {e}");
                std::process::exit(2);
            }
        }
    }

    if let Some(path) = &args.regime_eval {
        // Offline MarketRegime evaluation: independent of the engine — it
        // needs no mode, no feeds and never trades.
        run_regime_eval(
            path,
            args.regime_report.as_deref(),
            blitzkrieg_core::regime_eval::RegimeEvalArgs {
                token: args.regime_token.clone(),
                max_tokens: args.regime_max_tokens,
                config: strategy_logic::MarketRegimeConfig {
                    window_ms: args.regime_window_sec * 1_000,
                    trend_net_ticks: args.regime_min_trend_ticks,
                    trend_min_efficiency: args.regime_min_efficiency,
                    volatile_mad_ticks: args.regime_volatile_mad_ticks,
                    confirmations: args.regime_confirmations,
                    ..strategy_logic::MarketRegimeConfig::default()
                },
            },
        );
        return Ok(());
    }

    if let Some(path) = &args.blueprint_compile {
        // #361: the offline blueprint compiler — JSON decision graph → Lua
        // strategy body. Like --regime-eval it needs no mode, no feed, no
        // ledger and never trades; it also never touches the data latch (the
        // branch returns before it).
        run_blueprint_compile(path, args.blueprint_out.as_deref());
        return Ok(());
    }

    if let Some(path) = &args.backtest {
        // #203: install the counterfactual fee schedule BEFORE the core is built,
        // so every fill the replay charges is priced under it. Set-once: a failed
        // install means a schedule was already chosen, which cannot happen here
        // (this branch runs once) and must not be silent if it somehow does.
        if let Some(schedule) = args.fee_model
            && let Err(e) = blitzkrieg_core::exit_policy::set_fee_schedule(schedule)
        {
            eprintln!("blitzkrieg-core: --fee-model {}: {e}", schedule.name);
            std::process::exit(2);
        }
        // Offline replay: never re-record into (or read from) the file being
        // replayed — an appended-to-archive would feed the source its own tail.
        let mut cfg = config.clone();
        cfg.event_archive_path = None;
        cfg.event_archive_max_mb = 0;
        cfg.event_archive_rotate_mb = 0;
        cfg.event_archive_min_free_mb = 0;
        // A replay never writes (or replays) the durable cash journals — the
        // corpus is the only input, the report the only output.
        cfg.ledger_journal_dir = None;
        // Maker-rebate model: the replay declares it where the money is booked
        // (same posture as `fee_schedule_replay`), so every MAKER fill the
        // replay settles earns the modeled pool share.
        cfg.model_maker_rebate = args.maker_rebate;
        // #351: an explicit mode takes the mode runner (mine / verify /
        // sweep); an empty mode keeps the historic single-run shape so every
        // existing gate and script is byte-identical.
        if args.backtest_mode.is_empty() {
            run_backtest(
                path,
                args.backtest_report.as_deref(),
                cfg,
                args.backtest_tick_ms,
                args.backtest_tail_ms,
                args.backtest_knobs,
            );
        } else {
            run_mode_backtest(
                path,
                args.backtest_report.as_deref(),
                cfg,
                args.backtest_tick_ms,
                args.backtest_tail_ms,
                args.backtest_knobs,
                &args.backtest_mode,
                args.verify_latency_ms,
            );
        }
        return Ok(());
    }
    if args.blueprint_out.is_some() && args.blueprint_compile.is_none() {
        eprintln!(
            "blitzkrieg-core: --blueprint-out only applies to a compile; pass \
             --blueprint-compile <blueprint.json>"
        );
        std::process::exit(2);
    }
    if !args.backtest_knobs.is_empty() {
        eprintln!(
            "blitzkrieg-core: --backtest-knob only applies to a replay; pass --backtest <archive.jsonl>"
        );
        std::process::exit(2);
    }
    if let Some(schedule) = args.fee_model {
        eprintln!(
            "blitzkrieg-core: --fee-model {} only applies to a replay; pass --backtest <archive.jsonl> \
             (the schedule a live run charges is not a per-invocation choice, #203)",
            schedule.name
        );
        std::process::exit(2);
    }
    if args.backtest_fast {
        eprintln!(
            "blitzkrieg-core: --backtest-fast only applies to a replay; pass --backtest <archive.jsonl> \
             (a live kernel's tick cadence is not a per-invocation choice)"
        );
        std::process::exit(2);
    }
    if args.maker_rebate {
        eprintln!(
            "blitzkrieg-core: --maker-rebate only applies to a replay; pass --backtest <archive.jsonl> \
             (a live core must not credit cash the venue has not reported)"
        );
        std::process::exit(2);
    }

    // ───────────────────────── #199: the data-directory latch ────────────────
    // Everything above this line is read-only or in-memory, and every offline
    // mode (--replay, --regime-eval, --backtest) has already returned. The
    // serving path is where the ledgers are first touched — `server::run` →
    // `Core::new` reads them, restore and the first append write them — so the
    // latch is taken HERE. That placement is the whole reason a refused boot
    // writes nothing: no lock file, no ledger line, no side file, anywhere.
    let mode_str = match mode {
        Mode::Dry => "dry",
        Mode::Live => "live",
        Mode::ReadOnly => "readonly",
    };
    // The escape hatch. `BLITZKRIEG_ALLOW_SHARED_DATA` is spelled with the
    // issue's prefix rather than the `BK_*` namespace `EnvVars` collects,
    // because it is the one knob a fixture author copies out of the issue text.
    // An unparseable value is treated as UNSET (fail closed): a typo must never
    // be what silently disables the guard.
    let allow_shared_data = args.allow_shared_data
        || match std::env::var("BLITZKRIEG_ALLOW_SHARED_DATA") {
            Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" | "" => false,
                other => {
                    eprintln!(
                        "blitzkrieg-core: BLITZKRIEG_ALLOW_SHARED_DATA={other:?} is not a \
                         boolean; treating it as UNSET (the data directories stay guarded)"
                    );
                    false
                }
            },
            Err(_) => false,
        };

    // The latch goes on the parent directory of each EFFECTIVE ledger path, not
    // on a hardcoded `data/`: `data/` is only this process's cwd-relative
    // default, so a deployment with `--trade-log /mnt/ledger/trades.jsonl` would
    // be guarded in the wrong place by a fixed path. Three directories under the
    // defaults, one when a harness points all three logs at one temp directory
    // (and two fixtures in two temp directories never see each other), none when
    // every ledger is disabled. `data_lock`'s module docs carry the full
    // argument, including why #196's socket probe is not a substitute.
    let ledger_logs: [(&str, Option<&str>); 3] = [
        ("trade-log", config.trade_log_path.as_deref()),
        ("order-log", config.order_log_path.as_deref()),
        ("position-log", config.position_log_path.as_deref()),
    ];
    let anchors = blitzkrieg_core::data_lock::ledger_anchors(&ledger_logs);

    // "Where did this process write?" answered at boot, in absolute terms,
    // without knowing the cwd it was started from — the incident behind #199 was
    // found by hand, days later, by comparing files nobody had been told were
    // being written. Absolute but deliberately NOT canonicalized: the files may
    // not exist yet, and on macOS canonicalizing `/tmp` would print a path the
    // operator never typed.
    for (label, path) in [
        ("trade-log", config.trade_log_path.as_deref()),
        ("order-log", config.order_log_path.as_deref()),
        ("position-log", config.position_log_path.as_deref()),
        ("near-miss-log", config.near_miss_path.as_deref()),
        ("event-archive", config.event_archive_path.as_deref()),
        ("strategy-state", config.strategy_state_path.as_deref()),
        ("shadow-evolution-audit", args.se_audit_dir.as_deref()),
    ] {
        match path {
            Some(p) => {
                let abs = blitzkrieg_core::data_lock::absolute(std::path::Path::new(p));
                if abs.as_path() == std::path::Path::new(p) {
                    eprintln!("blitzkrieg-core: write path {label}: {}", abs.display());
                } else {
                    eprintln!(
                        "blitzkrieg-core: write path {label}: {} (from {p})",
                        abs.display()
                    );
                }
            }
            None => eprintln!("blitzkrieg-core: write path {label}: (disabled)"),
        }
    }
    // The side files that follow those paths are where a "the core only writes
    // trades.jsonl" assumption goes wrong: the trade summary, the fill/audit/
    // settlement journals and the daily-loss state all live in the same
    // directories, so a latch on the log file alone would not describe the
    // footprint. Naming them here keeps the banner honest about the set. The
    // mode-split ledger renames the live trade summary, so the banner spells
    // the rule instead of one filename.
    eprintln!(
        "blitzkrieg-core: side files: <trade-log stem>.summary.json beside the trade log \
         (the historical `trades.jsonl` keeps `summary.json`); applied-fills.jsonl / \
         reconcile.jsonl / settlements.jsonl beside the order log; <position-log>.daily-loss.json \
         beside the position log"
    );

    let lock_now = blitzkrieg_market_api::net::now_ms();
    let record =
        blitzkrieg_core::data_lock::LockRecord::for_this_process(&args.socket, mode_str, lock_now);
    // A refusal propagates out of `main` as a non-zero exit with the operator
    // text `DataDirsBusy` renders (occupant pid / socket / mode / start time,
    // per busy directory, plus the remedy). The anchors were all decided before
    // this call and the call writes only what it decided it owns, so a refusal
    // leaves the filesystem exactly as it found it.
    let data_lock =
        blitzkrieg_core::data_lock::DataLock::acquire(&anchors, &record, allow_shared_data)?;
    if anchors.is_empty() {
        eprintln!(
            "blitzkrieg-core: data lock: nothing to claim (every ledger is disabled by \
             --no-trade-log / --no-order-log / --no-position-log)"
        );
    } else {
        let dirs = data_lock
            .held()
            .iter()
            .map(|h| h.dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "blitzkrieg-core: data lock: pid {} claimed {} data director{}: {}",
            record.pid,
            anchors.len(),
            if anchors.len() == 1 { "y" } else { "ies" },
            dirs
        );
    }
    if allow_shared_data {
        // Printed even when no live owner was found: the flag is the promise
        // that this run may share, and the log reader (not the operator who
        // typed it) is who needs to know a guard was switched off.
        eprintln!(
            "blitzkrieg-core: WARNING SHARED DATA MODE: --allow-shared-data \
             (BLITZKRIEG_ALLOW_SHARED_DATA=1) is set, so a LIVE core writing the same data \
             directories will NOT refuse this one. Two writers break the trade/order/position \
             identities the daily-loss breaker and the reconcile audit rest on. Fixtures and \
             backtests only — never a deployment."
        );
    }
    for line in data_lock.banner(lock_now) {
        eprintln!("blitzkrieg-core: {line}");
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    // Keep the sender alive for the process lifetime; shutdown is driven by
    // SIGTERM/SIGINT inside the server. The handle is available for an embedded
    // caller that wants programmatic shutdown.
    let _shutdown_tx = tx;

    // The round-duration admission set: the market plugins' DECLARED union
    // (`supported_round_secs`), reported by the registry — which durations may
    // be armed is venue knowledge, so the kernel hardcodes no list; this is a
    // report of what the compiled-in plugins claim. Computed once here and
    // stored in the config (every admission consumer reads it from there); a
    // runtime enable flip stays within the registered set, so the union cannot
    // change after boot. An EMPTY union refuses every duration (fail-closed).
    config.allowed_round_secs = {
        let registry = blitzkrieg_core::market::registry::MarketPluginRegistry::new();
        blitzkrieg_core::market::register_builtin_markets(&registry);
        config.credential_vars = registry.credential_vars();
        // The labels travel with the durations (issue 648): the plugins'
        // declared `sec → label` vocabulary, so the readout renders exactly
        // the names the plugins claim (was the kernel's hardcoded
        // `duration_label_for` table).
        config.round_labels = registry.round_labels();
        registry.allowed_round_secs()
    };
    eprintln!(
        "blitzkrieg-core: declared round durations {:?} (market plugins' \
         supported_round_secs union)",
        config.allowed_round_secs
    );

    // Issue 751: RUNTIME_ROUND_DURATION_SEC env key outranks the JSON cell.
    // The JSON cell (data/runtime/config.json) is kept as a migration fallback.
    if config.round_duration_source != "cli_or_env" {
        if let Some(sec) = std::env::var("RUNTIME_ROUND_DURATION_SEC")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
        {
            if sec != config.round_duration_sec {
                eprintln!(
                    "blitzkrieg-core: round duration {}s -> {}s (RUNTIME_ROUND_DURATION_SEC env)",
                    config.round_duration_sec, sec
                );
                config.round_duration_sec = sec;
                config.round_duration_source = "runtime";
            }
        } else if let Some(cell) = config.runtime_config_path.as_deref().and_then(|p| {
            blitzkrieg_core::ipc::runtime_config::load_round_duration_from(
                std::path::Path::new(p),
                &config.allowed_round_secs,
            )
        }) && cell.duration_sec != config.round_duration_sec
        {
            eprintln!(
                "blitzkrieg-core: round duration {}s -> {}s (persisted UI setting, \
                 data/runtime/config.json; an explicit --round-sec or BK_ROUND_SEC outranks it)",
                config.round_duration_sec, cell.duration_sec
            );
            config.round_duration_sec = cell.duration_sec;
            config.round_duration_source = "runtime";
        }
    }

    // Issue 751: RUNTIME_MARKET_PLUGIN env key outranks the JSON cell.
    if let Some(plugin) = std::env::var("RUNTIME_MARKET_PLUGIN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        eprintln!(
            "blitzkrieg-core: market plugin {} (RUNTIME_MARKET_PLUGIN env)",
            plugin
        );
        // The --market-plugin flag already outranks everything; this env key
        // only applies when the flag is absent (handled in the parser).
    }

    // The legacy credentials cell (`market.credentials.set`,
    // data/runtime/config.json) is retired: `.env` is the single persistent
    // credential home (operator ruling). One shot at boot — drain any legacy
    // cell values into the configured `.env`, then strip the key so a later
    // sibling-cell write cannot resurrect it. The launcher applies `.env` at
    // the next start (this process already carries its own load), so the
    // migration is next-session by construction, exactly like the old cell.
    // Values never appear in the log; the migrated KEY names do.
    if let (Some(config_path), Some(env_path)) = (
        config.runtime_config_path.as_deref(),
        config.env_file_path.as_deref(),
    ) {
        match blitzkrieg_core::ipc::runtime_config::migrate_credentials_cell_to_env(
            std::path::Path::new(config_path),
            std::path::Path::new(env_path),
        ) {
            Ok(0) => {}
            Ok(_) => eprintln!(
                "blitzkrieg-core: legacy credentials cell migrated into {env_path} — \
                 restart (the launcher re-reads .env) to apply; market.credentials.set \
                 now writes .env directly"
            ),
            Err(e) => eprintln!(
                "blitzkrieg-core: legacy credentials cell migration failed: {e} — \
                 the cell stays in place; market.credentials.set writes .env directly"
            ),
        }
    }

    server::run(args.socket.clone(), config, args.tick_ms, rx).await?;
    eprintln!("blitzkrieg-core stopped");
    Ok(())
}
