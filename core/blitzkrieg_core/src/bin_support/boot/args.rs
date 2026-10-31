// #780 — `parse_args`, moved verbatim out of `boot.rs` so no file exceeds
// the 1500-line review budget. Its match tail (replay/latency/dry-redeem
// flags and the `--config*` no-ops) was extracted, block-for-block, into
// `replay_flags.rs`; no flag string, help line or error message changed.
use blitzkrieg_core::model::Mode;
use blitzkrieg_core::position::PositionConfig;
use rust_decimal::Decimal;
use std::str::FromStr;

use super::replay_flags::parse_replay_and_noop_flags;
use super::{
    Args, DEFAULT_ASSETS, DEFAULT_MIN_ROUND_AGE_SEC, DEFAULT_MIN_TIME_LEFT_SEC, DEFAULT_ROUND_SEC,
    EnvVars, bad_value, default_lua_strategy_dir, default_socket, default_strategy_dir,
    parse_backtest_knob, parse_exit_pct, split_assets, stale_budget_rejection,
};

pub(crate) fn parse_args(
    file: &blitzkrieg_core::config::FileConfig,
    argv: &[String],
    env: &EnvVars,
) -> Args {
    let mut socket = default_socket();
    let mut mode = Mode::Dry;
    let mut readonly = false;
    let mut tick_ms = 50u64;
    let mut seed_balance = Decimal::from(10_000);
    let mut max_order_notional = Decimal::from(100);
    let mut max_open_notional = Decimal::ZERO;
    let mut min_shares: Option<Decimal> = None;
    let mut max_shares: Option<Decimal> = None;
    // #202: the equity-relative sizing and per-order cap. Both tracked as Option
    // so CLI > env > default resolution can tell "the operator spoke" from
    // "nobody did", and both default to 0 (off) — the shipped absolute
    // behaviour, untouched.
    let mut size_pct: Option<Decimal> = None;
    let mut max_order_notional_pct: Option<Decimal> = None;
    let mut markets: Vec<String> = Vec::new();
    let mut auto_exits = true;
    let mut max_positions: usize = 2;
    let mut max_positions_per_asset: Option<u32> = None;
    // #173: the daily-loss budget, tracked as Option so CLI > env > default
    // resolution can tell "the operator spoke" from "nobody did".
    let mut max_daily_loss: Option<Decimal> = None;
    let mut max_daily_loss_pct: Option<Decimal> = None;
    let mut strategy_limits: Vec<String> = Vec::new();
    let mut virtual_limits: Vec<String> = Vec::new();
    let mut enable_strategy: Vec<String> = Vec::new();
    let mut disable_strategy: Vec<String> = Vec::new();
    let mut strategy_state: Option<String> = None;
    let mut no_strategy_state = false;
    let mut strategy_dir: Option<String> = None;
    let mut no_strategy_dir = false;
    let mut lua_strategy_dir: Option<String> = None;
    let mut no_lua_strategy_dir = false;
    let mut engine = false;
    // File-settable settings are collected as Option so the precedence chain can
    // resolve them at the end; a concrete default would erase the "was this flag
    // given?" question that TOML-vs-CLI depends on.
    let mut min_round_age: Option<i64> = None;
    let mut min_time_left: Option<i64> = None;
    let mut trend_confirm: i64 = 60;
    let mut trend_floor_ms: i64 = 10_000;
    let mut spread_arb_entry_factor: Option<Decimal> = None;
    let mut spread_arb_min_obi: Option<Decimal> = None;
    let mut spread_arb_max_spread_pct: Option<Decimal> = None;
    let mut spread_arb_dip_max_pct: Option<Decimal> = None;
    let mut spread_arb_bounce_min_pct: Option<Decimal> = None;
    let mut spread_arb_bounce_window_sec: Option<i64> = None;
    // E17/#264: the four engine-level exit knobs, tracked as Option so the
    // CLI > TOML > default chain can tell "the operator spoke" from "nobody did".
    let mut exit_take_profit_pct: Option<Decimal> = None;
    let mut exit_stop_loss_pct: Option<Decimal> = None;
    let mut exit_trailing_min_high_pct: Option<Decimal> = None;
    let mut exit_min_trail_pct: Option<Decimal> = None;
    let mut exit_force_sec: Option<i64> = None;
    let mut allow_zero_strategies = false;
    let mut feed_ws = false;
    let mut net_check = false;
    let mut replay: Option<String> = None;
    let mut replay_near_miss: Option<String> = None;
    let mut round_sec: Option<i64> = None;
    let mut near_miss_path: Option<String> = None;
    let mut trade_log: Option<String> = None;
    let mut no_trade_log = false;
    let mut order_log: Option<String> = None;
    let mut no_order_log = false;
    let mut position_log: Option<String> = None;
    let mut no_position_log = false;
    let mut no_ledger_journal = false;
    let mut no_intent_audit = false;
    let mut allow_shared_data = false;
    let mut market_plugin: Option<String> = None;
    let mut discovery = true;
    let mut shadow_evolution_flag = false;
    let mut se_min_samples: Option<u32> = None;
    let mut se_cooldown_secs: Option<i64> = None;
    let mut se_min_obs_secs: Option<i64> = None;
    let mut se_auto_evolve: Option<bool> = None;
    let mut update_check_enabled: Option<bool> = None;
    let mut update_auto: Option<bool> = None;
    let mut update_interval_secs: Option<i64> = None;
    let mut se_cycle_secs: Option<i64> = None;
    let mut se_ttl_secs: Option<i64> = None;
    let mut se_deep_dims: Option<usize> = None;
    let mut assets_arg: Option<String> = None;
    let mut event_archive: Option<String> = None;
    // Tuning is tracked as Option so the always-on default can supply its own
    // values without clobbering an explicit flag (and vice versa).
    let mut event_archive_max_mb: Option<u64> = None;
    let mut event_archive_rotate_mb: Option<u64> = None;
    let mut event_archive_min_free_mb: Option<u64> = None;
    let mut no_event_archive = false;
    let mut entry_maker_timeout_ms: i64 = 5000;
    let mut backtest: Option<String> = None;
    let mut backtest_report: Option<String> = None;
    let mut backtest_tick_ms: i64 = 50;
    let mut backtest_tail_ms: i64 = 0;
    let mut backtest_knobs: Vec<(String, String, Decimal)> = Vec::new();
    // #351: the replay mode — `mine` (zero latency, friction as configured),
    // `verify` (one real-latency run), `sweep` (latency ladder). Empty = the
    // historic single-run behaviour, byte-identical CLI surface.
    let mut backtest_mode = String::new();
    let mut verify_latency_ms: i64 = 286;
    // #203: replay-only fee schedule (see `Args::fee_model`).
    let mut fee_model: Option<blitzkrieg_core::exit_policy::FeeSchedule> = None;
    // Replay-only maker-rebate model (see `Args::maker_rebate`).
    let mut maker_rebate = false;
    // Replay maintenance-gating fast path (see `Args::backtest_fast`).
    let mut backtest_fast = false;
    let mut regime_eval: Option<String> = None;
    let mut regime_report: Option<String> = None;
    let mut regime_token: Option<String> = None;
    let mut regime_max_tokens: usize = 3;
    let mut regime_window_sec: i64 = 300;
    let mut regime_min_trend_ticks = Decimal::new(3, 0);
    let mut regime_min_efficiency = Decimal::new(5, 1);
    let mut regime_volatile_mad_ticks = Decimal::new(15, 1);
    let mut regime_confirmations: u32 = 2;
    // #352: on-chain pull knobs (offline subcommand, like --regime-eval).
    let mut onchain_pull: Option<String> = None;
    let mut onchain_start: Option<String> = None;
    let mut onchain_end: Option<String> = None;
    let mut onchain_asset: Option<String> = None;
    let mut onchain_market: Option<String> = None;
    let mut onchain_fills: Option<String> = None;
    let mut onchain_out_dir: Option<String> = None;
    // #361: the offline blueprint compiler (`--blueprint-compile <json>`),
    // plus its optional output override (`--blueprint-out <path>`).
    let mut blueprint_compile: Option<String> = None;
    let mut blueprint_out: Option<String> = None;
    let mut slippage_ticks: u32 = 0;
    let mut latency_ms: i64 = 0;
    let mut taker_latency_ms: i64 = 0;
    let mut taker_rtt_ms: i64 = 0;
    let mut fill_prob_bps: Option<u32> = None;
    let mut maker_depth_share_bps: Option<u32> = None;
    // #205: the orderbook freshness budget, tracked as Option so CLI > env >
    // default resolution can tell "the operator spoke" from "nobody did".
    let mut max_orderbook_stale_ms: Option<i64> = None;
    // 任务二 1-3: the NTP clock-offset probe. Host None = nobody spoke; the
    // empty string is a DOCUMENTED "off" spelling (path_value!'s empty-is-off
    // semantics), so a deployment can disarm the probe without a rebuild.
    let mut ntp_host: Option<String> = None;
    let mut ntp_interval_secs: Option<i64> = None;
    // Dry-mode redemption test hooks (#175 gate): fail the first N simulated
    // redemption attempts of each claim, optionally as a `manual` failure that
    // stops the retries for good. 0 = production behaviour.
    let mut dry_redeem_fail: u32 = 0;
    let mut dry_redeem_manual = false;
    // #380: the explicit policy-file override. `None` = the shipped default,
    // resolved against the install root (see `resolve_execution_policy_path`).
    let mut execution_policy: Option<String> = None;

    // #228: the explicit opt-out from "an unknown argument stops the boot". Read
    // as a pre-scan rather than as an arm below, because argv order must not
    // decide whether `--allow-unknown-args` applies to an argument that comes
    // BEFORE it — an escape hatch whose validity depends on where it was written
    // is a trap, not an escape hatch.
    let allow_unknown = argv
        .iter()
        .any(|a| a == blitzkrieg_core::cli::ALLOW_UNKNOWN_ARGS);

    let mut it = argv.iter().cloned();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--socket" => socket = next_value!(it, "--socket"),
            "--mode" => {
                let raw = next_value!(it, "--mode");
                mode = match raw.as_str() {
                    "live" => Mode::Live,
                    "dry" => Mode::Dry,
                    _ => bad_value(
                        "--mode",
                        &raw,
                        "want dry|live — a typo here would run the OPPOSITE mode of the one you named",
                    ),
                }
            }
            "--readonly" => readonly = true,
            "--tick-ms" => {
                tick_ms = num_value!(it, "--tick-ms", u64, "want a whole number of milliseconds")
            }
            "--seed-balance" => {
                seed_balance = dec_value!(it, "--seed-balance", "want a USD amount (e.g. 10000)")
            }
            "--max-order-notional" => {
                max_order_notional =
                    dec_value!(it, "--max-order-notional", "want a USD amount (e.g. 100)")
            }
            "--max-open-notional-usd" => {
                max_open_notional = dec_value!(
                    it,
                    "--max-open-notional-usd",
                    "want a USD amount (0 = no cap)"
                )
            }
            // #202: per-entry budget as a share of the account, and the
            // per-order notional bound in the same unit. Both are percentages
            // (20 = 20%), both default to 0 = off.
            "--size-pct" => {
                size_pct = Some(dec_value!(
                    it,
                    "--size-pct",
                    "want a percentage of equity (20 = 20%, 0 = off)"
                ))
            }
            "--max-order-notional-pct" => {
                max_order_notional_pct = Some(dec_value!(
                    it,
                    "--max-order-notional-pct",
                    "want a percentage of equity (0 = off)"
                ))
            }
            "--min-shares" => {
                min_shares = Some(dec_value!(it, "--min-shares", "want a share count"))
            }
            "--max-shares" => {
                max_shares = Some(dec_value!(it, "--max-shares", "want a share count"))
            }
            "--no-auto-exits" => auto_exits = false,
            "--engine" => engine = true,
            "--feed-ws" => feed_ws = true,
            "--replay" => replay = Some(path_value!(it, "--replay")),
            "--replay-near-miss" => replay_near_miss = Some(path_value!(it, "--replay-near-miss")),
            "--net-check" => net_check = true,
            "--near-miss-path" => near_miss_path = Some(path_value!(it, "--near-miss-path")),
            "--trade-log" => trade_log = Some(path_value!(it, "--trade-log")),
            "--no-trade-log" => no_trade_log = true,
            "--order-log" => order_log = Some(path_value!(it, "--order-log")),
            "--no-order-log" => no_order_log = true,
            "--position-log" => position_log = Some(path_value!(it, "--position-log")),
            "--no-position-log" => no_position_log = true,
            "--no-ledger-journal" => no_ledger_journal = true,
            "--no-intent-audit" => no_intent_audit = true,
            "--allow-shared-data" => allow_shared_data = true,
            "--market-plugin" => market_plugin = Some(path_value!(it, "--market-plugin")),
            "--no-discovery" => discovery = false,
            "--shadow-evolution" => shadow_evolution_flag = true,
            "--se-min-samples" => {
                se_min_samples = Some(num_value!(
                    it,
                    "--se-min-samples",
                    u32,
                    "want a sample count"
                ))
            }
            "--se-cooldown-secs" => {
                se_cooldown_secs = Some(num_value!(
                    it,
                    "--se-cooldown-secs",
                    i64,
                    "want a number of seconds"
                ))
            }
            "--se-min-obs-secs" => {
                se_min_obs_secs = Some(num_value!(
                    it,
                    "--se-min-obs-secs",
                    i64,
                    "want a number of seconds"
                ))
            }
            "--se-auto-evolve" => se_auto_evolve = Some(bool_value!(it, "--se-auto-evolve")),
            "--se-cycle-secs" => {
                se_cycle_secs = Some(num_value!(
                    it,
                    "--se-cycle-secs",
                    i64,
                    "want a number of seconds"
                ))
            }
            // Update switches (VERSIONING.md §7): explicit `true|false` values,
            // like --se-auto-evolve, so a caller can turn OFF as well as on.
            "--update-check" => update_check_enabled = Some(bool_value!(it, "--update-check")),
            "--update-auto" => update_auto = Some(bool_value!(it, "--update-auto")),
            // #379: the automatic-check interval in seconds. 0 explicitly
            // disables the scheduler; a negative value is refused here (the
            // old `.filter(>=0)` silently dropped it into the default).
            "--update-interval-secs" => {
                let v = num_value!(
                    it,
                    "--update-interval-secs",
                    i64,
                    "want a number of seconds (0 = off)"
                );
                if v < 0 {
                    bad_value(
                        "--update-interval-secs",
                        &v.to_string(),
                        "want a non-negative number of seconds (0 = off)",
                    );
                }
                update_interval_secs = Some(v);
            }
            "--se-ttl-secs" => {
                se_ttl_secs = Some(num_value!(
                    it,
                    "--se-ttl-secs",
                    i64,
                    "want a number of seconds"
                ))
            }
            "--se-deep-dims" => {
                se_deep_dims = Some(num_value!(
                    it,
                    "--se-deep-dims",
                    usize,
                    "want a dimension count"
                ))
            }
            "--strategy-limit" => {
                let v = path_value!(it, "--strategy-limit");
                strategy_limits.push(v);
            }
            // E-VA: the virtual-account pot for strategies that declare
            // `atomic_batch` — `name:max_batch_usd:acct1,acct2` (repeatable;
            // "-" = no cap, the flag is then dropped with a warning because an
            // uncapped pot would silently disarm the whole atomic posture).
            "--virtual-limit" => {
                let v = path_value!(it, "--virtual-limit");
                virtual_limits.push(v);
            }
            "--enable-strategy" => {
                let v = path_value!(it, "--enable-strategy");
                if !v.trim().is_empty() {
                    enable_strategy.push(v);
                }
            }
            "--disable-strategy" => {
                let v = path_value!(it, "--disable-strategy");
                if !v.trim().is_empty() {
                    disable_strategy.push(v);
                }
            }
            // #265: the explicit opt-out from "an explicit request that resolved
            // to nothing refuses the boot". Named here (and in the flag table) so
            // it is never mistaken for an unknown argument; it changes nothing
            // about what starts enabled.
            "--allow-zero-strategies" => allow_zero_strategies = true,
            "--strategy-state" => {
                strategy_state =
                    Some(path_value!(it, "--strategy-state")).filter(|v| !v.trim().is_empty());
            }
            "--no-strategy-state" => no_strategy_state = true,
            "--strategy-dir" => {
                strategy_dir =
                    Some(path_value!(it, "--strategy-dir")).filter(|v| !v.trim().is_empty())
            }
            "--no-strategy-dir" => no_strategy_dir = true,
            // E30 (§6.4): the Lua package scan root (`none`/empty = off).
            "--lua-strategy-dir" => {
                lua_strategy_dir =
                    Some(path_value!(it, "--lua-strategy-dir")).filter(|v| !v.trim().is_empty());
            }
            "--no-lua-strategy-dir" => no_lua_strategy_dir = true,
            "--assets" => assets_arg = Some(path_value!(it, "--assets")),
            "--round-sec" => {
                round_sec = Some(num_value!(
                    it,
                    "--round-sec",
                    i64,
                    "want a number of seconds"
                ))
            }
            "--min-round-age" => {
                min_round_age = Some(num_value!(
                    it,
                    "--min-round-age",
                    i64,
                    "want a number of seconds"
                ))
            }
            "--min-time-left" => {
                min_time_left = Some(num_value!(
                    it,
                    "--min-time-left",
                    i64,
                    "want a number of seconds"
                ))
            }
            "--trend-confirm-sec" => {
                trend_confirm =
                    num_value!(it, "--trend-confirm-sec", i64, "want a number of seconds")
            }
            "--trend-window-floor-ms" => {
                trend_floor_ms = num_value!(
                    it,
                    "--trend-window-floor-ms",
                    i64,
                    "want a number of milliseconds"
                )
            }
            "--spread-arb-entry-factor" => {
                spread_arb_entry_factor = Some(dec_value!(
                    it,
                    "--spread-arb-entry-factor",
                    "want a decimal factor (e.g. 1.2)"
                ))
            }
            "--spread-arb-min-obi" => {
                spread_arb_min_obi = Some(dec_value!(
                    it,
                    "--spread-arb-min-obi",
                    "want a decimal between 0 and 1"
                ))
            }
            "--spread-arb-max-spread-pct" => {
                spread_arb_max_spread_pct = Some(dec_value!(
                    it,
                    "--spread-arb-max-spread-pct",
                    "want a decimal percentage"
                ))
            }
            "--spread-arb-dip-max-pct" => {
                spread_arb_dip_max_pct = Some(dec_value!(
                    it,
                    "--spread-arb-dip-max-pct",
                    "want a decimal percentage"
                ))
            }
            "--spread-arb-bounce-min-pct" => {
                spread_arb_bounce_min_pct = Some(dec_value!(
                    it,
                    "--spread-arb-bounce-min-pct",
                    "want a decimal percentage"
                ))
            }
            "--spread-arb-bounce-window-sec" => {
                spread_arb_bounce_window_sec = Some(num_value!(
                    it,
                    "--spread-arb-bounce-window-sec",
                    i64,
                    "want a number of seconds"
                ))
            }
            // E17/#264: the engine-level exit ladder. Parsed strictly — a value
            // the engine cannot honour is a startup error, never a silent
            // fallback to the shipped ladder (see `parse_exit_pct`).
            "--exit-take-profit-pct" => {
                let raw = next_value!(it, "--exit-take-profit-pct");
                match parse_exit_pct(&raw) {
                    Ok(v) => exit_take_profit_pct = Some(v),
                    Err(e) => bad_value("--exit-take-profit-pct", &raw, &e),
                }
            }
            "--exit-stop-loss-pct" => {
                let raw = next_value!(it, "--exit-stop-loss-pct");
                match parse_exit_pct(&raw) {
                    Ok(v) => exit_stop_loss_pct = Some(v),
                    Err(e) => bad_value("--exit-stop-loss-pct", &raw, &e),
                }
            }
            "--exit-trailing-min-high-pct" => {
                let raw = next_value!(it, "--exit-trailing-min-high-pct");
                match parse_exit_pct(&raw) {
                    Ok(v) => exit_trailing_min_high_pct = Some(v),
                    Err(e) => bad_value("--exit-trailing-min-high-pct", &raw, &e),
                }
            }
            "--exit-min-trail-pct" => {
                let raw = next_value!(it, "--exit-min-trail-pct");
                match parse_exit_pct(&raw) {
                    Ok(v) => exit_min_trail_pct = Some(v),
                    Err(e) => bad_value("--exit-min-trail-pct", &raw, &e),
                }
            }
            "--exit-force-sec" => {
                exit_force_sec = Some(num_value!(
                    it,
                    "--exit-force-sec",
                    i64,
                    "want a whole number of seconds (0 = off)"
                ));
            }
            "--regime-eval" => regime_eval = Some(path_value!(it, "--regime-eval")),
            "--regime-report" => regime_report = Some(path_value!(it, "--regime-report")),
            "--regime-token" => regime_token = Some(path_value!(it, "--regime-token")),
            // #352: the on-chain pull subcommand and its knobs.
            "--onchain-pull" => onchain_pull = Some(path_value!(it, "--onchain-pull")),
            "--onchain-start" => onchain_start = Some(path_value!(it, "--onchain-start")),
            "--onchain-end" => onchain_end = Some(path_value!(it, "--onchain-end")),
            "--onchain-asset" => onchain_asset = Some(path_value!(it, "--onchain-asset")),
            "--onchain-market" => onchain_market = Some(path_value!(it, "--onchain-market")),
            "--onchain-fills" => onchain_fills = Some(path_value!(it, "--onchain-fills")),
            "--onchain-out-dir" => onchain_out_dir = Some(path_value!(it, "--onchain-out-dir")),
            // #361: the offline blueprint compiler and its output knob.
            "--blueprint-compile" => {
                blueprint_compile = Some(path_value!(it, "--blueprint-compile"))
            }
            "--blueprint-out" => blueprint_out = Some(path_value!(it, "--blueprint-out")),
            "--regime-max-tokens" => {
                regime_max_tokens =
                    num_value!(it, "--regime-max-tokens", usize, "want a token count")
            }
            "--regime-window-sec" => {
                regime_window_sec =
                    num_value!(it, "--regime-window-sec", i64, "want a number of seconds")
            }
            "--regime-min-trend-ticks" => {
                regime_min_trend_ticks = dec_value!(
                    it,
                    "--regime-min-trend-ticks",
                    "want a decimal number of ticks"
                )
            }
            "--regime-min-efficiency" => {
                regime_min_efficiency = dec_value!(
                    it,
                    "--regime-min-efficiency",
                    "want a decimal between 0 and 1"
                )
            }
            "--regime-volatile-mad-ticks" => {
                regime_volatile_mad_ticks = dec_value!(
                    it,
                    "--regime-volatile-mad-ticks",
                    "want a decimal number of ticks"
                )
            }
            "--regime-confirmations" => {
                regime_confirmations = num_value!(
                    it,
                    "--regime-confirmations",
                    u32,
                    "want a confirmation count"
                )
            }
            "--max-positions" => {
                max_positions = num_value!(it, "--max-positions", usize, "want a position count")
            }
            // #363: per-asset entries (the @Almach replay needs > 1). Absent
            // = the execution-policy file decides; 0 is refused at boot
            // (it would refuse every entry).
            "--max-positions-per-asset" => {
                max_positions_per_asset = Some(num_value!(
                    it,
                    "--max-positions-per-asset",
                    u32,
                    "want a position count"
                ))
            }
            // #380: an explicit policy file wins over the install-root
            // resolution of the shipped default. A relative spelling is
            // honoured verbatim (resolved against the process cwd, like any
            // explicit path) — the install-root lookup is for the DEFAULT
            // only, never a second-guessing of the operator's choice.
            "--execution-policy" => {
                execution_policy = Some(path_value!(it, "--execution-policy"));
            }
            // #173: absolute USD cap for the day's realized loss (0 = none).
            "--max-daily-loss" => {
                max_daily_loss = Some(dec_value!(
                    it,
                    "--max-daily-loss",
                    "want a USD amount (0 = no budget)"
                ))
            }
            // #173: cap as a percentage of the day's opening cash equity.
            "--max-daily-loss-pct" => {
                max_daily_loss_pct = Some(dec_value!(
                    it,
                    "--max-daily-loss-pct",
                    "want a percentage (0 = off)"
                ))
            }
            "--market" => {
                if let Some(m) = it.next() {
                    markets.push(m);
                }
            }
            "--event-archive" => event_archive = Some(path_value!(it, "--event-archive")),
            "--no-event-archive" => no_event_archive = true,
            "--event-archive-max-mb" => {
                event_archive_max_mb = Some(num_value!(
                    it,
                    "--event-archive-max-mb",
                    u64,
                    "want a size in MiB (0 = unlimited)"
                ))
            }
            "--event-archive-rotate-mb" => {
                event_archive_rotate_mb = Some(num_value!(
                    it,
                    "--event-archive-rotate-mb",
                    u64,
                    "want a size in MiB (0 = never rotate)"
                ))
            }
            "--event-archive-min-free-mb" => {
                event_archive_min_free_mb = Some(num_value!(
                    it,
                    "--event-archive-min-free-mb",
                    u64,
                    "want a size in MiB (0 = no guard)"
                ))
            }
            // Strict, like `--max-orderbook-stale-ms` below: a value the engine
            // cannot honour is a startup error, never a silent fallback. The
            // silent fallback here was worse than usual — a typo produced the
            // default clock, and an operator who believed they had DISARMED the
            // maker→taker escalation was running it.
            "--entry-maker-timeout-ms" => {
                entry_maker_timeout_ms = num_value!(
                    it,
                    "--entry-maker-timeout-ms",
                    i64,
                    "want a whole number of milliseconds (0 uses the 5000 ms default, a negative value never escalates)"
                );
            }
            // #205: how stale an orderbook may be before the engine refuses to
            // price off it. Parsed strictly — a value the engine cannot honour is
            // a startup error, never a silent fallback: an operator who believes
            // they set a budget must not be running a different one.
            "--max-orderbook-stale-ms" => {
                max_orderbook_stale_ms = Some(num_value!(
                    it,
                    "--max-orderbook-stale-ms",
                    i64,
                    "want a whole number of milliseconds (0 disables the freshness check)"
                ));
            }
            // 任务二 1-3: the NTP clock-offset probe. The empty string is the
            // documented "off" spelling (zero network tasks spawned — the
            // replay-safe posture); a NEGATIVE interval is refused, an explicit
            // 0 disables the periodic loop (a single boot-time probe only).
            "--ntp-host" => {
                ntp_host = Some(path_value!(it, "--ntp-host").trim().to_string());
            }
            "--ntp-interval-secs" => {
                let v = num_value!(
                    it,
                    "--ntp-interval-secs",
                    i64,
                    "want a number of seconds (0 = off)"
                );
                if v < 0 {
                    bad_value(
                        "--ntp-interval-secs",
                        &v.to_string(),
                        "want a non-negative number of seconds (0 = off)",
                    );
                }
                ntp_interval_secs = Some(v);
            }
            "--backtest" => backtest = Some(path_value!(it, "--backtest")),
            "--backtest-report" => backtest_report = Some(path_value!(it, "--backtest-report")),
            "--backtest-tick-ms" => {
                backtest_tick_ms = num_value!(
                    it,
                    "--backtest-tick-ms",
                    i64,
                    "want a whole number of milliseconds"
                )
            }
            "--backtest-tail-ms" => {
                backtest_tail_ms = num_value!(
                    it,
                    "--backtest-tail-ms",
                    i64,
                    "want a whole number of milliseconds"
                )
            }
            // `<strategy>:<knob>=<value>`; repeatable. A replay-only counterfactual.
            "--backtest-knob" => {
                let spec = next_value!(it, "--backtest-knob");
                match parse_backtest_knob(&spec) {
                    Ok(k) => backtest_knobs.push(k),
                    Err(e) => bad_value("--backtest-knob", &spec, &e),
                }
            }
            // #203: which taker-fee schedule a REPLAY charges. Not a live knob —
            // see `Args::fee_model`.
            "--fee-model" => {
                let name = next_value!(it, "--fee-model");
                match blitzkrieg_core::exit_policy::fee_schedule_by_name(&name) {
                    Some(s) => fee_model = Some(s),
                    None => bad_value(
                        "--fee-model",
                        &name,
                        &format!(
                            "unknown schedule; known: {}",
                            blitzkrieg_core::exit_policy::FEE_SCHEDULE_NAMES.join(", ")
                        ),
                    ),
                }
            }
            "--slippage-ticks" => {
                slippage_ticks = num_value!(it, "--slippage-ticks", u32, "want a tick count")
            }
            // Replay maintenance-gating fast path: a flag, no value. Not a live
            // knob — see `Args::backtest_fast` (refusal below, with --fee-model).
            "--backtest-fast" => backtest_fast = true,
            // Maker-rebate model: a flag, no value. Replay-only like
            // --backtest-fast (refused below, same shape).
            "--maker-rebate" => maker_rebate = true,
            // #228: the fallback REFUSES. It used to print
            // `ignoring unknown argument: <flag>` and start the core anyway,
            // which turned a misspelled `--readonly` into a write-mode boot and a
            // misspelled `--max-order-notional` into the shipped cap — a silent
            // downgrade of exactly the flags that exist to bound what this
            // process may do. The refusal is decided by `cli::classify_unknown_arg`
            // (unit-tested, with the "did you mean" spelling and, for a safety
            // flag, what leaving it out would have meant); this arm only prints
            // and exits. `--allow-unknown-args` opts out, and never for a
            // near-miss of a safety flag.
            other => {
                if !parse_replay_and_noop_flags(
                    other,
                    &mut it,
                    &mut backtest_mode,
                    &mut verify_latency_ms,
                    &mut latency_ms,
                    &mut taker_latency_ms,
                    &mut taker_rtt_ms,
                    &mut fill_prob_bps,
                    &mut maker_depth_share_bps,
                    &mut dry_redeem_fail,
                    &mut dry_redeem_manual,
                ) {
                    match blitzkrieg_core::cli::classify_unknown_arg(other, allow_unknown) {
                        blitzkrieg_core::cli::UnknownArg::Reject(msg) => {
                            eprintln!("{msg}");
                            std::process::exit(2);
                        }
                        blitzkrieg_core::cli::UnknownArg::Ignore(msg) => eprintln!("{msg}"),
                    }
                }
            }
        }
    }

    // ── Resolve the file-settable settings: CLI > env > TOML > default ──────
    use blitzkrieg_core::config::{Sourced, pick};
    let mut report: Vec<String> = Vec::new();

    macro_rules! resolve {
        ($label:literal, $cli:expr, $env:literal, $toml:expr, $default:expr) => {{
            let s: Sourced<_> = pick($cli, env.num($env), $toml, $default);
            if s.is_explicit() {
                report.push(format!("{}={} ({})", $label, s.value, s.source.as_str()));
            }
            s.value
        }};
    }
    // Same, for values that are not Display (the asset list).
    macro_rules! resolve_shown {
        ($label:literal, $cli:expr, $env:expr, $toml:expr, $default:expr) => {{
            let s = pick($cli, $env, $toml, $default);
            if s.is_explicit() {
                report.push(format!("{}={:?} ({})", $label, s.value, s.source.as_str()));
            }
            s.value
        }};
    }
    // For settings whose "not given" state is itself `None`. The override has to
    // be wrapped one level so `pick` can tell "a layer supplied None" from
    // "no layer spoke" — resolving those two is the whole job of this layer.
    macro_rules! resolve_opt {
        ($label:literal, $cli:expr, $env:expr, $toml:expr) => {{
            let s = pick($cli.map(Some), $env.map(Some), $toml.map(Some), None);
            if s.is_explicit() {
                let shown = s
                    .value
                    .as_ref()
                    .map(|v| format!("{v:?}"))
                    .unwrap_or_else(|| "none".to_string());
                report.push(format!("{}={} ({})", $label, shown, s.source.as_str()));
            }
            s.value
        }};
    }

    let round_sec = resolve!(
        "engine.round_sec",
        round_sec,
        "BK_ROUND_SEC",
        file.round_sec,
        DEFAULT_ROUND_SEC
    );
    let min_round_age = resolve!(
        "engine.min_round_age_sec",
        min_round_age,
        "BK_MIN_ROUND_AGE_SEC",
        file.min_round_age_sec,
        DEFAULT_MIN_ROUND_AGE_SEC
    );
    let min_time_left = resolve!(
        "engine.min_time_left_sec",
        min_time_left,
        "BK_MIN_TIME_LEFT_SEC",
        file.min_time_left_sec,
        DEFAULT_MIN_TIME_LEFT_SEC
    );
    let assets: Vec<String> = resolve_shown!(
        "engine.assets",
        assets_arg.as_deref().map(split_assets),
        env.text("BK_ASSETS").map(|v| split_assets(&v)),
        file.assets.clone(),
        DEFAULT_ASSETS
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
    );

    // Shadow Evolution. The file speaks in MINUTES for the two windows; the
    // kernel speaks in seconds, so the conversion happens exactly here and
    // nowhere else.
    let se_eval_window_secs = resolve_opt!(
        "shadow_evolution.evaluation_window_secs",
        env.num::<i64>("BK_SE_EVAL_WINDOW_SECS"),
        None::<i64>,
        file.shadow.evaluation_window_minutes.map(|m| m * 60)
    );
    let se_min_obs_secs = resolve_opt!(
        "shadow_evolution.min_observation_secs",
        se_min_obs_secs,
        None::<i64>,
        file.shadow.min_observation_minutes.map(|m| m * 60)
    );
    let se_cooldown_secs = resolve_opt!(
        "shadow_evolution.cooldown_secs",
        se_cooldown_secs,
        env.num::<i64>("BK_SE_COOLDOWN_SECS"),
        file.shadow.cooldown_minutes.map(|m| m * 60)
    );
    let se_min_samples = resolve_opt!(
        "shadow_evolution.min_sample_count",
        se_min_samples,
        env.num::<u32>("BK_SE_MIN_SAMPLES"),
        file.shadow.min_sample_count
    );
    let se_variant_count = resolve_opt!(
        "shadow_evolution.variant_count",
        None::<usize>,
        env.num::<usize>("BK_SE_VARIANT_COUNT"),
        file.shadow.variant_count
    );
    let se_audit_dir = resolve_opt!(
        "shadow_evolution.audit_dir",
        None::<String>,
        env.text("BK_SE_AUDIT_DIR"),
        file.shadow.audit_dir.clone()
    );
    let se_min_win_rate = resolve_opt!(
        "shadow_evolution.min_win_rate_improvement",
        None::<Decimal>,
        env.num::<Decimal>("BK_SE_MIN_WIN_RATE"),
        file.shadow.min_win_rate_improvement
    );
    let se_min_profit_factor = resolve_opt!(
        "shadow_evolution.min_profit_factor_improvement",
        None::<Decimal>,
        env.num::<Decimal>("BK_SE_MIN_PROFIT_FACTOR"),
        file.shadow.min_profit_factor_improvement
    );
    // Lock 2 is a safety lock, not a tuning surface: a file may tighten the
    // per-step gradient, never widen it. A value above the ceiling is refused
    // outright rather than clamped — silently ignoring a request to loosen a
    // safety lock is the kind of thing an operator must be told about.
    let se_max_gradient = match pick(
        None::<Decimal>,
        env.num::<Decimal>("BK_SE_MAX_GRADIENT"),
        file.shadow.max_gradient,
        blitzkrieg_core::config::MAX_GRADIENT_CEILING,
    ) {
        s if !s.is_explicit() => None,
        s if s.value <= blitzkrieg_core::config::MAX_GRADIENT_CEILING
            && s.value > Decimal::ZERO =>
        {
            report.push(format!(
                "shadow_evolution.max_gradient={} ({})",
                s.value,
                s.source.as_str()
            ));
            Some(s.value)
        }
        s => {
            eprintln!(
                "blitzkrieg-core: ignoring max_gradient {}: must be > 0 and <= {} \
                 (Lock 2 is not widenable)",
                s.value,
                blitzkrieg_core::config::MAX_GRADIENT_CEILING
            );
            None
        }
    };
    // E13 proposal workflow. The file speaks in MINUTES for the two clocks
    // (same convention as the other windows); the kernel speaks in seconds.
    let se_auto_evolve = resolve_opt!(
        "shadow_evolution.auto_evolve",
        se_auto_evolve,
        env.text("BK_SE_AUTO_EVOLVE")
            .or_else(|| env.text("SHADOW_AUTO_EVOLVE"))
            .and_then(|v| match v.trim() {
                "true" | "1" | "on" | "yes" => Some(true),
                "false" | "0" | "off" | "no" => Some(false),
                _ => None,
            }),
        file.shadow.auto_evolve
    );
    // VERSIONING.md §7: CLI > env > update.toml > built-in default (off).
    let update_check_enabled = resolve_opt!(
        "update.check_enabled",
        update_check_enabled,
        env.text("BLITZKRIEG_UPDATE_CHECK")
            .and_then(|v| match v.trim() {
                "true" | "1" | "on" | "yes" => Some(true),
                "false" | "0" | "off" | "no" => Some(false),
                _ => None,
            }),
        file.update.check_enabled
    );
    let update_auto = resolve_opt!(
        "update.auto_update",
        update_auto,
        env.text("BLITZKRIEG_UPDATE_AUTO")
            .and_then(|v| match v.trim() {
                "true" | "1" | "on" | "yes" => Some(true),
                "false" | "0" | "off" | "no" => Some(false),
                _ => None,
            }),
        file.update.auto_update
    );
    // #379: the automatic-check clock. Same chain; 0 = scheduler off. A
    // persisted interval is NOT used here by design — the clock is
    // restart-to-change configuration, so a UI toggle cannot silently arm it.
    let update_interval_secs = resolve_opt!(
        "update.interval_secs",
        update_interval_secs,
        env.text("BLITZKRIEG_UPDATE_INTERVAL_SECS")
            .and_then(|v| v.trim().parse::<i64>().ok()),
        file.update.interval_secs
    )
    .filter(|s| *s >= 0);
    let se_cycle_secs = resolve_opt!(
        "shadow_evolution.evolution_cycle_secs",
        se_cycle_secs,
        env.num::<i64>("BK_SE_CYCLE_SECS"),
        file.shadow.evolution_cycle_minutes.map(|m| m * 60)
    );
    let se_ttl_secs = resolve_opt!(
        "shadow_evolution.proposal_ttl_secs",
        se_ttl_secs,
        env.num::<i64>("BK_SE_TTL_SECS"),
        file.shadow.proposal_ttl_minutes.map(|m| m * 60)
    );
    let se_deep_dims = resolve_opt!(
        "shadow_evolution.deep_dims",
        se_deep_dims,
        env.num::<usize>("BK_SE_DEEP_DIMS"),
        file.shadow.deep_dims
    );

    // ── #173: the daily-loss breaker's budget ───────────────────────────────
    // Resolved CLI > env > compiled default, then echoed unconditionally below.
    // The default is RELATIVE (`max_daily_loss_equity_pct`), because an absolute
    // default cannot be meaningful on every account size: the old hard-coded
    // 200 USD silently disabled the breaker on a 4.8 USDC live book (P0-2).
    let daily_loss_defaults = PositionConfig::default();
    let daily_loss_usd = pick(
        max_daily_loss,
        env.num::<Decimal>("BK_MAX_DAILY_LOSS"),
        // E26 (§4.2): the [risk] drawdown key IS this budget's TOML face —
        // one budget, resolved through the one chain, never a second counter.
        file.risk.max_daily_drawdown_usd,
        daily_loss_defaults.max_daily_loss_usd,
    );
    let daily_loss_pct = pick(
        max_daily_loss_pct,
        env.num::<Decimal>("BK_MAX_DAILY_LOSS_PCT"),
        None,
        daily_loss_defaults.max_daily_loss_equity_pct,
    );
    if daily_loss_usd.is_explicit() {
        report.push(format!(
            "positions.max_daily_loss_usd={} ({})",
            daily_loss_usd.value,
            daily_loss_usd.source.as_str()
        ));
    }
    if daily_loss_pct.is_explicit() {
        report.push(format!(
            "positions.max_daily_loss_equity_pct={} ({})",
            daily_loss_pct.value,
            daily_loss_pct.source.as_str()
        ));
    }
    let mut daily_loss_echo = vec![format!(
        "daily loss breaker: absolute cap ${} ({}), relative cap {}% of the day's opening cash equity ({}); the TIGHTER applies, 0 = off",
        daily_loss_usd.value,
        daily_loss_usd.source.as_str(),
        daily_loss_pct.value,
        daily_loss_pct.source.as_str()
    )];
    if daily_loss_usd.value <= Decimal::ZERO && daily_loss_pct.value <= Decimal::ZERO {
        daily_loss_echo.push(
            "daily loss breaker: DISABLED (both caps are 0) — pass --max-daily-loss <usd> or \
             --max-daily-loss-pct <pct> to arm it"
                .to_string(),
        );
    }

    // ── #202: the account-relative sizing / per-order bound ─────────────────
    // Same treatment as the daily-loss budget above, and for the same reason: a
    // bound that is only in the source is not a bound anyone can verify against
    // the account it runs on. `size_pct` also changes ORDER SIZES, so the boot
    // log has to say whether the absolute path or the equity path is in force.
    let size_pct = pick(
        size_pct,
        env.num::<Decimal>("BK_SIZE_PCT"),
        None,
        Decimal::ZERO,
    );
    let notional_pct = pick(
        max_order_notional_pct,
        env.num::<Decimal>("BK_MAX_ORDER_NOTIONAL_PCT"),
        None,
        Decimal::ZERO,
    );
    if size_pct.is_explicit() {
        report.push(format!(
            "engine.size_pct={} ({})",
            size_pct.value,
            size_pct.source.as_str()
        ));
    }
    if notional_pct.is_explicit() {
        report.push(format!(
            "risk.max_order_notional_pct={} ({})",
            notional_pct.value,
            notional_pct.source.as_str()
        ));
    }
    let mut sizing_echo = vec![
        format!(
            "order sizing: per-entry budget {} (0 = off: the absolute size_usd path, order sizes unchanged), \
             per-order notional cap {}% of the account's cash equity (0 = off)",
            if size_pct.value > Decimal::ZERO {
                format!(
                    "{}% of cash equity ({}; replaces size_usd)",
                    size_pct.value,
                    size_pct.source.as_str()
                )
            } else {
                format!(
                    "absolute size_usd ({}; --size-pct/BK_SIZE_PCT off)",
                    size_pct.source.as_str()
                )
            },
            notional_pct.value,
        ),
        format!(
            "per-order bounds in force: absolute cap ${}, plus {} (relative cap ON: an over-cap order is REJECTED, never truncated, and closing intents are exempt)",
            max_order_notional,
            if notional_pct.value > Decimal::ZERO {
                format!("{}% of the account's cash equity", notional_pct.value)
            } else {
                "NO equity-relative cap".to_string()
            }
        ),
    ];
    if notional_pct.value <= Decimal::ZERO {
        sizing_echo.push(
            "per-order bounds in force: this account has NO bound as a share of itself — one order may commit \
             the whole of `max_shares × max_price` (10 shares × 1.00 = 10.00 USD by default, 208% of a 4.8 USDC \
             book). Pass --max-order-notional-pct <pct> or run `node scripts/risk-sizing-check.mjs` to see the number"
                .to_string(),
        );
    }

    // ── #205: the orderbook freshness budget ────────────────────────────────
    // How long after the feed goes quiet the engine stops pricing off its books.
    // A runtime setting because it is a venue/ops property, not a code property:
    // a slow poller or a venue that batches updates needs a wider budget, and a
    // deployment tightening it must not need a rebuild to be obeyed.
    let stale = pick(
        max_orderbook_stale_ms,
        env.num::<i64>("BK_MAX_ORDERBOOK_STALE_MS"),
        None,
        blitzkrieg_core::engine::DEFAULT_MAX_ORDERBOOK_STALE_MS,
    );
    // Invalid values fail the boot loudly instead of falling back: a silently
    // substituted budget is the failure this knob exists to remove.
    if let Some(why) = stale_budget_rejection(stale.value) {
        eprintln!(
            "blitzkrieg-core: {} gives the orderbook freshness budget {why}",
            stale.source.as_str()
        );
        std::process::exit(2);
    }
    if stale.is_explicit() {
        report.push(format!(
            "engine.max_orderbook_stale_ms={} ({})",
            stale.value,
            stale.source.as_str()
        ));
    }
    let orderbook_stale_echo = vec![if stale.value > 0 {
        format!(
            "orderbook freshness: the engine refuses to price off a book older than {} ms ({}); past that it \
             stops taking entries until the feed catches up (closing intents are never blocked by this)",
            stale.value,
            stale.source.as_str()
        )
    } else {
        format!(
            "orderbook freshness: CHECK DISABLED ({}; any book age is accepted) — the engine keeps pricing off \
             the last book it saw however old it is. Pass --max-orderbook-stale-ms <ms> to arm it",
            stale.source.as_str()
        )
    }];

    // ── 任务二 1-3: the NTP clock-offset probe ──────────────────────────────
    // CLI > env > default. The empty string is the documented "off" spelling:
    // `probe::should_spawn_ntp` refuses it, so a configured-off probe spawns
    // ZERO network tasks (the replay-safe posture). An explicit --ntp-host ""
    // and an absent host are pinned apart by the snapshot test.
    let ntp = pick(
        ntp_host,
        env.text("BK_NTP_HOST"),
        None::<String>,
        blitzkrieg_core::probe::DEFAULT_NTP_HOST.to_string(),
    );
    let ntp_interval = pick(
        ntp_interval_secs,
        env.num::<i64>("BK_NTP_INTERVAL_SECS"),
        None::<i64>,
        blitzkrieg_core::probe::DEFAULT_NTP_INTERVAL_SECS,
    );
    if ntp_interval.value < 0 {
        // Parse arms already refuse a negative CLI value; a negative from the
        // env layer lands here (same fail-loud posture, never a silent default).
        eprintln!(
            "blitzkrieg-core: BK_NTP_INTERVAL_SECS={} is negative — the NTP probe \
             interval cannot be negative (0 = off)",
            ntp_interval.value
        );
        std::process::exit(2);
    }
    let ntp_echo = vec![if blitzkrieg_core::probe::should_spawn_ntp(&ntp.value)
        && ntp_interval.value > 0
    {
        format!(
            "environment probes: NTP clock-offset probe ON — every {} s against \
             {} ({}/{}); |offset| >= 50 ms warns, >= 100 ms flags probe.ntp.skewRed",
            ntp_interval.value,
            ntp.value,
            ntp.source.as_str(),
            ntp_interval.source.as_str()
        )
    } else {
        format!(
            "environment probes: NTP clock-offset probe OFF (host={}, interval={}s) — \
             zero network tasks spawned; stats probe.ntp stays cold",
            if ntp.value.trim().is_empty() {
                "(empty = off)"
            } else {
                ntp.value.as_str()
            },
            ntp_interval.value
        )
    }];

    // ── E17/#264: the engine-level exit ladder ──────────────────────────────
    // Same chain as every other file-settable knob (CLI > TOML > compiled
    // default). No env layer: `--spread-arb-*`, the sibling this pipeline
    // mirrors, has none either, and the durable layer for a ladder is the file.
    // The defaults are read off `ExitConfig::default()` rather than re-typed, so
    // "unchanged" is a property of the code, not of a comment.
    let exit_defaults = blitzkrieg_core::exit_policy::ExitConfig::default();
    let exit_knob = |report: &mut Vec<String>,
                     label: &str,
                     cli: Option<Decimal>,
                     toml: Option<Decimal>,
                     default: Decimal| {
        let s = pick(cli, None::<Decimal>, toml, default);
        if s.is_explicit() {
            report.push(format!("{label}={} ({})", s.value, s.source.as_str()));
        }
        s.value
    };
    let exit_take_profit_pct = exit_knob(
        &mut report,
        "exit.take_profit_pct",
        exit_take_profit_pct,
        file.exit.take_profit_pct,
        exit_defaults.take_profit_pct,
    );
    let exit_stop_loss_pct = exit_knob(
        &mut report,
        "exit.stop_loss_pct",
        exit_stop_loss_pct,
        file.exit.stop_loss_pct,
        exit_defaults.stop_loss_pct,
    );
    let exit_trailing_min_high_pct = exit_knob(
        &mut report,
        "exit.trailing_min_high_pct",
        exit_trailing_min_high_pct,
        file.exit.trailing_min_high_pct,
        exit_defaults.trailing_min_high_pct,
    );
    let exit_min_trail_pct = exit_knob(
        &mut report,
        "exit.min_trail_pct",
        exit_min_trail_pct,
        file.exit.min_trail_pct,
        exit_defaults.min_trail_pct,
    );
    // force_exit_sec is an INT knob (seconds), so the Decimal-typed `exit_knob`
    // closure above does not fit; the chain is the same CLI > TOML > default.
    let exit_force_sec = {
        let s = pick(
            exit_force_sec,
            None::<i64>,
            file.exit.force_exit_sec,
            exit_defaults.force_exit_sec,
        );
        if s.is_explicit() {
            report.push(format!(
                "exit.force_exit_sec={} ({})",
                s.value,
                s.source.as_str()
            ));
        }
        s.value
    };

    // ── #269: is the evolution engine running? ──────────────────────────────
    // The one shadow-evolution setting whose origin was never printed, and the
    // question the switch-conflict warning is about ("the file says off" is only
    // a kill switch if you can see whether it took). Echoed unconditionally, like
    // the breaker's budget and the freshness budget above: a boot log must answer
    // it even for a run that configured nothing.
    let se_engine = pick(
        None::<bool>,
        env.num::<bool>("BK_SHADOW_EVOLUTION")
            .or_else(|| env.num::<bool>("SHADOW_ENABLED")),
        file.shadow.enabled,
        false,
    );
    let shadow_evolution_enabled = shadow_evolution_flag || se_engine.value;
    if se_engine.is_explicit() {
        report.push(format!(
            "shadow_evolution.enabled={} ({})",
            se_engine.value,
            se_engine.source.as_str()
        ));
    }
    let shadow_evolution_echo = vec![format!(
        "shadow evolution: evaluator {} at startup ({}); the persisted runtime switch in \
         state.json wins over it, and any disagreement is reported below",
        if shadow_evolution_enabled {
            "ON"
        } else {
            "OFF"
        },
        if shadow_evolution_flag {
            "from the --shadow-evolution flag"
        } else {
            se_engine.source.as_str()
        },
    )];

    Args {
        socket,
        mode,
        readonly,
        tick_ms,
        seed_balance,
        max_order_notional,
        max_open_notional,
        min_shares,
        max_shares,
        size_pct: size_pct.value,
        max_order_notional_pct: notional_pct.value,
        sizing_echo,
        max_orderbook_stale_ms: stale.value,
        orderbook_stale_echo,
        ntp_host: ntp.value,
        ntp_interval_secs: ntp_interval.value,
        markets,
        auto_exits,
        max_positions,
        max_positions_per_asset,
        execution_policy,
        daily_loss_usd: daily_loss_usd.value,
        daily_loss_pct: daily_loss_pct.value,
        daily_loss_source: daily_loss_usd.source,
        daily_loss_echo,
        engine,
        min_round_age,
        min_time_left,
        trend_confirm,
        trend_floor_ms,
        spread_arb_entry_factor,
        spread_arb_min_obi,
        spread_arb_max_spread_pct,
        spread_arb_dip_max_pct,
        spread_arb_bounce_min_pct,
        spread_arb_bounce_window_sec,
        exit_take_profit_pct,
        exit_stop_loss_pct,
        exit_trailing_min_high_pct,
        exit_min_trail_pct,
        exit_force_sec,
        feed_ws,
        net_check,
        replay,
        replay_near_miss,
        round_sec,
        near_miss_path,
        trade_log,
        no_trade_log,
        order_log,
        no_order_log,
        position_log,
        no_position_log,
        ledger_journal_dir: if no_ledger_journal {
            None
        } else {
            Some("data/ledger".to_string())
        },
        no_intent_audit,
        allow_shared_data,
        market_plugin,
        discovery,
        // Resolved once, above, so the boot echo cannot disagree with what the
        // service is actually handed.
        shadow_evolution: shadow_evolution_enabled,
        assets,
        se_min_samples,
        se_cooldown_secs,
        se_min_obs_secs,
        se_eval_window_secs,
        se_max_gradient,
        se_variant_count,
        se_audit_dir,
        se_min_win_rate,
        se_min_profit_factor,
        se_auto_evolve,
        se_cycle_secs,
        se_ttl_secs,
        se_deep_dims,
        update_check_enabled,
        update_auto,
        update_interval_secs,
        ntp_echo,
        shadow_evolution_echo,
        strategy_limits,
        virtual_limits,
        enable_strategy,
        disable_strategy,
        allow_zero_strategies,
        strategy_state: if no_strategy_state {
            None
        } else {
            Some(strategy_state.unwrap_or_else(|| "data/strategy-state.json".to_string()))
        },
        strategy_dir: if no_strategy_dir {
            None
        } else {
            strategy_dir
                .or_else(|| env.text("BK_STRATEGY_DIR"))
                .or_else(default_strategy_dir)
        },
        lua_strategy_dir: if no_lua_strategy_dir {
            None
        } else {
            lua_strategy_dir
                .or_else(|| env.text("BK_LUA_STRATEGY_DIR"))
                .or_else(default_lua_strategy_dir)
        },
        event_archive,
        no_event_archive,
        event_archive_max_mb,
        event_archive_rotate_mb,
        event_archive_min_free_mb,
        entry_maker_timeout_ms,
        backtest,
        backtest_report,
        backtest_tick_ms,
        backtest_tail_ms,
        backtest_knobs,
        backtest_mode,
        verify_latency_ms,
        fee_model,
        maker_rebate,
        backtest_fast,
        regime_eval,
        regime_report,
        regime_token,
        regime_max_tokens,
        regime_window_sec,
        regime_min_trend_ticks,
        regime_min_efficiency,
        regime_volatile_mad_ticks,
        regime_confirmations,
        onchain_pull,
        onchain_start,
        onchain_end,
        onchain_asset,
        onchain_market,
        onchain_fills,
        onchain_out_dir,
        blueprint_compile,
        blueprint_out,
        slippage_ticks,
        latency_ms,
        taker_latency_ms,
        taker_rtt_ms,
        fill_prob_bps,
        maker_depth_share_bps,
        dry_redeem_fail,
        dry_redeem_manual,
        config_report: report,
    }
}
