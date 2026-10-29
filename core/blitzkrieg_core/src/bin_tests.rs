// #699 — the bin's `#[cfg(test)] mod tests`, moved here verbatim so
// `main.rs` stays inside its review budget. Declared from the bin root
// (`#[path]` keeps it beside `main.rs`). The explicit imports below stand in
// for the old `use super::*` pull of the root's private imports (a glob does
// not see those across the split), so every test keeps resolving unchanged.
use super::bin_support::boot::{
    Args, ConfigChoice, DEFAULT_ASSETS, DEFAULT_EVENT_ARCHIVE, DEFAULT_MIN_ROUND_AGE_SEC,
    DEFAULT_MIN_TIME_LEFT_SEC, DEFAULT_ROUND_SEC, EnvVars, config_choice, daily_loss_path_for,
    parse_args, parse_exit_pct, parse_strategy_limits, parse_virtual_limits, prescan_config,
    resolve_event_archive, round_duration_source_from, stale_budget_rejection,
};
use blitzkrieg_core::model::Mode;
use rust_decimal::Decimal;

mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    // ── #351 反向验收 · the honest-friction floor is enforced ───────────────

    /// The spec's floor: "mine 模式拒绝 slippage=0（fail-closed）". A sweep
    /// that asked for a frictionless run must be refused at the knob, not run
    /// and quietly print fantasy PnL.
    #[test]
    fn mine_mode_refuses_zero_slippage() {
        let base = blitzkrieg_core::sim::FillModel {
            taker_slippage_ticks: 0,
            ..Default::default()
        };
        let err = blitzkrieg_core::backtest::mode_rung_model("mine", base, 0)
            .expect_err("zero slippage must die");
        assert!(
            err.contains("refuses zero taker slippage"),
            "the refusal must name the floor: {err}"
        );
        // The floor is mode-independent: verify and sweep run on the same
        // honest-friction contract.
        assert!(blitzkrieg_core::backtest::mode_rung_model("verify", base, 0).is_err());
        assert!(blitzkrieg_core::backtest::mode_rung_model("sweep", base, 0).is_err());
    }

    /// The other side of the knob contract: mine zeroes the latency dials
    /// (0 decision→venue distance, real slippage/fees kept from `base`), while
    /// verify/sweep stamp the rung onto every latency.
    #[test]
    fn mode_rung_model_wires_the_latency_dials_per_mode() {
        let base = blitzkrieg_core::sim::FillModel {
            taker_slippage_ticks: 1,
            taker_latency_ms: 7,
            taker_rtt_ms: 11,
            maker_latency_ms: 13,
            ..Default::default()
        };
        let mine = blitzkrieg_core::backtest::mode_rung_model("mine", base, 0)
            .expect("mine above the floor");
        assert_eq!(mine.taker_latency_ms, 0);
        assert_eq!(mine.taker_rtt_ms, 0);
        assert_eq!(mine.maker_latency_ms, 0);
        assert_eq!(
            mine.taker_slippage_ticks, 1,
            "mine keeps the real friction; latency is the only dial it turns"
        );
        let verify = blitzkrieg_core::backtest::mode_rung_model("verify", base, 286)
            .expect("verify above the floor");
        assert_eq!(verify.taker_latency_ms, 286);
        assert_eq!(verify.taker_rtt_ms, 286);
        assert_eq!(verify.maker_latency_ms, 286);
        assert_eq!(verify.taker_slippage_ticks, 1);
    }

    // ── E2-a: the --strategy-limit grammar ──────────────────────────────────

    fn parse_one(flag: &str) -> Option<blitzkrieg_core::service::StrategyLimit> {
        parse_strategy_limits(&[flag.to_string()])
            .into_iter()
            .next()
            .map(|(_, l)| l)
    }

    #[test]
    fn three_segment_form_still_parses_as_caps_only() {
        let l = parse_one("spread_arb:0:-").expect("legacy form must still parse");
        assert_eq!(l.max_open_positions, Some(0));
        assert_eq!(l.max_open_notional_usd, None);
        assert!(l.size_usd.is_none() && l.min_shares.is_none() && l.max_shares.is_none());
        assert!(
            !l.sizing().overrides_anything(),
            "caps-only must not masquerade as a sizing override"
        );

        let l2 = parse_one("spread_arb:-:12.5").expect("blank cap segment = uncapped");
        assert_eq!(l2.max_open_positions, None);
        assert_eq!(l2.max_open_notional_usd, Some(dec!(12.5)));
    }

    #[test]
    fn six_segment_form_parses_per_strategy_sizing() {
        let l = parse_one("spread_arb:2:20:1.5:4:8").expect("extended form must parse");
        assert_eq!(l.max_open_positions, Some(2));
        assert_eq!(l.max_open_notional_usd, Some(dec!(20)));
        assert_eq!(l.size_usd, Some(dec!(1.5)));
        assert_eq!(l.min_shares, Some(dec!(4)));
        assert_eq!(l.max_shares, Some(dec!(8)));
        assert!(l.sizing().overrides_anything());

        // "-" inherits the global per dimension, independently of the others.
        let l2 = parse_one("x:-:-:-:3:-").expect("blank sizing segments inherit the globals");
        assert_eq!(l2.size_usd, None);
        assert_eq!(l2.min_shares, Some(dec!(3)));
        assert_eq!(l2.max_shares, None);
        assert!(
            l2.sizing().overrides_anything(),
            "one set dimension is enough"
        );
    }

    #[test]
    fn seven_segment_form_parses_allocation_weight() {
        // E16/#98: the 7th segment is the leg's allocation weight.
        let l = parse_one("spread_arb:2:20:1.5:4:8:0.5").expect("weight form must parse");
        assert_eq!(l.size_weight, Some(dec!(0.5)));
        assert!(l.sizing().overrides_anything());
        // "-" = unweighted; an absent 7th segment is the 6-segment form.
        let l2 = parse_one("spread_arb:2:20:1.5:4:8:-").expect("blank weight = unweighted");
        assert_eq!(l2.size_weight, None);
        // Bad weight drops the whole flag.
        assert!(parse_one("spread_arb:0:-:1:2:3:x").is_none());
    }

    #[test]
    fn malformed_strategy_limits_are_dropped_not_half_applied() {
        for bad in [
            "spread_arb:0",               // too few segments
            "spread_arb:0:-:1:2:3:4:5:6", // too many
            ":0:-",                       // no name
            "spread_arb:x:-",             // bad position cap
            "spread_arb:0:abc",           // bad notional
            "spread_arb:0:-:abc:1:2",     // bad size_usd
            "spread_arb:0:-:1:abc:2",     // bad min_shares
            "spread_arb:0:-:1:2:abc",     // bad max_shares
            "spread_arb:0:-:1:2:3:4:x",   // bad size_pct (#202)
        ] {
            assert!(parse_one(bad).is_none(), "must be ignored: {bad}");
        }
        assert!(parse_strategy_limits(&[]).is_empty());
    }

    #[test]
    fn repeated_flags_keep_the_last_entry_per_name() {
        let m = parse_strategy_limits(&[
            "a:1:-".to_string(),
            "b:2:-:1:1:1".to_string(),
            "a:5:-".to_string(),
        ]);
        assert_eq!(m.len(), 2);
        assert_eq!(m["a"].max_open_positions, Some(5), "last flag wins");
        assert_eq!(m["b"].min_shares, Some(dec!(1)));
    }

    // ── E-VA: the --virtual-limit grammar ───────────────────────────────────

    #[test]
    fn virtual_limit_parses_name_cap_accounts() {
        let m = parse_virtual_limits(&["cross_venue_arb:500:default,paper".to_string()]);
        assert_eq!(m.len(), 1);
        let pot = m.get("cross_venue_arb").expect("the named pot");
        assert_eq!(pot.max_batch_notional_usd, Some(dec!(500)));
        assert_eq!(
            pot.accounts,
            vec!["default".to_string(), "paper".to_string()],
            "comma-separated accounts split and kept in order"
        );
    }

    #[test]
    fn virtual_limit_trims_and_drops_blank_segments() {
        // Whitespace around every segment is trimmed; empty account slots
        // (a trailing comma, doubled commas) drop out rather than become an
        // account named "".
        let m = parse_virtual_limits(&[" s : 12.5 : a , , b, ".to_string()]);
        let pot = m.get("s").expect("trimmed pot");
        assert_eq!(pot.max_batch_notional_usd, Some(dec!(12.5)));
        assert_eq!(pot.accounts, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn malformed_virtual_limits_are_dropped_not_half_applied() {
        for bad in [
            "cross_venue_arb:500",           // too few segments
            "cross_venue_arb:500:a,b:extra", // too many
            ":500:default",                  // no name
            "cross_venue_arb:abc:default",   // bad cap
            "cross_venue_arb:-:default",     // no cap = disarmed pot, dropped
            "cross_venue_arb:500:,",         // no accounts
            "cross_venue_arb:500:",          // empty account segment
        ] {
            let m = parse_virtual_limits(&[bad.to_string()]);
            assert!(m.is_empty(), "must be ignored, not half-applied: {bad}");
        }
        // An empty flag list parses to an empty map (the zero-config posture).
        assert!(parse_virtual_limits(&[]).is_empty());
    }

    #[test]
    fn repeated_virtual_limit_flags_keep_the_last_per_name() {
        let m = parse_virtual_limits(&[
            "a:10:default".to_string(),
            "b:20:paper".to_string(),
            "a:99:default,paper".to_string(),
        ]);
        assert_eq!(m.len(), 2, "one pot per strategy name");
        let a = m.get("a").expect("pot a");
        assert_eq!(a.max_batch_notional_usd, Some(dec!(99)), "last flag wins");
        assert_eq!(a.accounts, vec!["default".to_string(), "paper".to_string()]);
        assert_eq!(
            m.get("b").expect("pot b").max_batch_notional_usd,
            Some(dec!(20))
        );
    }

    #[test]
    fn engine_session_records_by_default() {
        let (path, max, rotate, min_free) =
            resolve_event_archive(true, false, None, None, None, None);
        assert_eq!(path.as_deref(), Some(DEFAULT_EVENT_ARCHIVE));
        assert_eq!(
            max, 0,
            "no session cap: rotation + the space floor bound the volume"
        );
        assert_eq!(rotate, 256);
        assert_eq!(min_free, 5120);
    }

    #[test]
    fn non_engine_session_does_not_record() {
        // The plain trading core (no --engine) has no market-data consumers, so
        // there is nothing to archive; the default must not leave an empty file.
        let (path, _, rotate, _) = resolve_event_archive(false, false, None, None, None, None);
        assert_eq!(path, None);
        assert_eq!(rotate, 0);
    }

    #[test]
    fn explicit_path_opts_in_without_engine() {
        let (path, ..) =
            resolve_event_archive(false, false, Some("/tmp/x.jsonl"), None, None, None);
        assert_eq!(path.as_deref(), Some("/tmp/x.jsonl"));
    }

    #[test]
    fn explicit_tuning_beats_the_default() {
        let (path, max, rotate, min_free) =
            resolve_event_archive(true, false, None, Some(64), Some(8), Some(100));
        assert_eq!(path.as_deref(), Some(DEFAULT_EVENT_ARCHIVE));
        assert_eq!((max, rotate, min_free), (64, 8, 100));
    }

    #[test]
    fn explicit_zero_tuning_is_honoured_not_defaulted() {
        // 0 is a meaningful request (no cap / never rotate / no space guard) and
        // must survive; only an ABSENT flag falls back to the default.
        let (_, max, rotate, min_free) =
            resolve_event_archive(true, false, None, Some(0), Some(0), Some(0));
        assert_eq!((max, rotate, min_free), (0, 0, 0));
    }

    #[test]
    fn no_event_archive_disables_capture_even_with_a_path() {
        let (path, max, rotate, min_free) = resolve_event_archive(
            true,
            true,
            Some("/tmp/x.jsonl"),
            Some(64),
            Some(8),
            Some(100),
        );
        assert_eq!((path, max, rotate, min_free), (None, 0, 0, 0));
    }

    // ── KI-11: the file-config source and its precedence ────────────────────

    /// Parse an argv with no config file and no env overrides in play.
    fn args_from(flags: &[&str]) -> Args {
        let argv: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &argv,
            &EnvVars::default(),
        )
    }

    /// An env layer built from literal pairs, so the env tier of the precedence
    /// chain is testable without touching the process environment.
    fn env_of(pairs: &[(&str, &str)]) -> EnvVars {
        EnvVars {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// Write `text` to a scratch `default.toml` and load it the way the kernel
    /// does (the loader consults the file system, so a temp file is the honest
    /// way to exercise it).
    fn file_with(text: &str) -> blitzkrieg_core::config::FileConfig {
        let dir = std::env::temp_dir().join(format!(
            "bk-main-cfg-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("default.toml");
        std::fs::write(&path, text).unwrap();
        let cfg = blitzkrieg_core::config::FileConfig::load(Some(&path));
        std::fs::remove_dir_all(&dir).ok();
        cfg
    }

    #[test]
    fn defaults_hold_without_a_file_or_flags() {
        let a = args_from(&[]);
        assert_eq!(a.round_sec, 900);
        assert_eq!(a.min_round_age, 30);
        assert_eq!(a.min_time_left, 180);
        assert_eq!(a.assets, vec!["BTC", "ETH", "SOL", "XRP"]);
        assert!(
            a.config_report.is_empty(),
            "nothing is explicit, so nothing is reported: {:?}",
            a.config_report
        );
    }

    #[test]
    fn the_compiled_defaults_carry_the_former_factory_file_values() {
        // env single-source guard: the [engine] rows deleted from the shipped
        // default.toml (2026-10-23) resolved to these values. If a change moves
        // a compiled default, it must be an EXPLICIT act paired with a .env /
        // BK_* rollout — never a silent drift hidden behind "the file still
        // says 900" (it no longer says anything). 900 (15m), not 300 (5m): a
        // file-driven 5m is a measured regression (MIGRATION_LOG §12) that
        // `scripts/soak-health.sh` alarms on.
        assert_eq!(DEFAULT_ASSETS, ["BTC", "ETH", "SOL", "XRP"]);
        assert_eq!(DEFAULT_ROUND_SEC, 900);
        assert_eq!(DEFAULT_MIN_ROUND_AGE_SEC, 30);
        assert_eq!(DEFAULT_MIN_TIME_LEFT_SEC, 180);
    }

    // ── E17/#264: the engine-level exit ladder ──────────────────────────────

    /// The property the ticket turns on: the four flags exist, and leaving them
    /// out changes NOTHING. `min_trail_pct` is the one the ticket names
    /// explicitly (8, not the 5 the source branch used), so it is asserted
    /// against the shipped `ExitConfig` rather than a literal repeated here.
    #[test]
    fn the_exit_ladder_defaults_to_the_compiled_one() {
        let a = args_from(&[]);
        let d = blitzkrieg_core::exit_policy::ExitConfig::default();
        assert_eq!(a.exit_take_profit_pct, d.take_profit_pct);
        assert_eq!(a.exit_stop_loss_pct, d.stop_loss_pct);
        assert_eq!(a.exit_trailing_min_high_pct, d.trailing_min_high_pct);
        assert_eq!(a.exit_min_trail_pct, d.min_trail_pct);
        // Spelled out once, because "unchanged" is the acceptance criterion.
        assert_eq!(a.exit_take_profit_pct, dec!(100));
        assert_eq!(a.exit_stop_loss_pct, dec!(12));
        assert_eq!(a.exit_trailing_min_high_pct, dec!(15));
        assert_eq!(a.exit_min_trail_pct, dec!(8));
        assert!(
            a.config_report.is_empty(),
            "nothing was configured, so nothing is reported: {:?}",
            a.config_report
        );
    }

    #[test]
    fn an_exit_flag_overrides_the_ladder_and_is_reported() {
        let a = args_from(&[
            "--exit-take-profit-pct",
            "20",
            "--exit-stop-loss-pct",
            "6",
            "--exit-trailing-min-high-pct",
            "10",
            "--exit-min-trail-pct",
            "5",
        ]);
        assert_eq!(a.exit_take_profit_pct, dec!(20));
        assert_eq!(a.exit_stop_loss_pct, dec!(6));
        assert_eq!(a.exit_trailing_min_high_pct, dec!(10));
        assert_eq!(a.exit_min_trail_pct, dec!(5));
        for line in [
            "exit.take_profit_pct=20 (cli)",
            "exit.stop_loss_pct=6 (cli)",
            "exit.trailing_min_high_pct=10 (cli)",
            "exit.min_trail_pct=5 (cli)",
        ] {
            assert!(
                a.config_report.iter().any(|l| l == line),
                "{line} missing from {:?}",
                a.config_report
            );
        }
    }

    /// The file is the second source, and the flag still wins — the same
    /// precedence every other file-settable knob follows.
    #[test]
    fn the_exit_section_of_the_config_file_is_read_and_outranked_by_the_flag() {
        let file = file_with(
            r#"
            [exit]
            stop_loss_pct = 9
            min_trail_pct = "4.5"
            "#,
        );
        let argv: Vec<String> = Vec::new();
        let a = parse_args(&file, &argv, &EnvVars::default());
        assert_eq!(a.exit_stop_loss_pct, dec!(9));
        assert_eq!(a.exit_min_trail_pct, dec!(4.5));
        // The two the file does not mention keep the compiled value.
        assert_eq!(a.exit_take_profit_pct, dec!(100));
        assert_eq!(a.exit_trailing_min_high_pct, dec!(15));
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "exit.stop_loss_pct=9 (toml)"),
            "{:?}",
            a.config_report
        );

        let a = parse_args(
            &file,
            &["--exit-stop-loss-pct".into(), "11".into()],
            &EnvVars::default(),
        );
        assert_eq!(a.exit_stop_loss_pct, dec!(11));
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "exit.stop_loss_pct=11 (cli)"),
            "{:?}",
            a.config_report
        );
    }

    /// A decimal the kernel cannot read is a startup error, never a silent
    /// fallback to the shipped ladder (`parse_exit_pct` is the pure half; the
    /// `exit(2)` half is the arm that calls it).
    #[test]
    fn a_malformed_exit_value_is_rejected_purely() {
        assert_eq!(parse_exit_pct("8"), Ok(dec!(8)));
        assert_eq!(parse_exit_pct(" 8.5 "), Ok(dec!(8.5)));
        assert_eq!(parse_exit_pct("-3"), Ok(dec!(-3)));
        for bad in ["", "eight", "1x2", "8%"] {
            assert!(
                parse_exit_pct(bad).is_err(),
                "{bad:?} must not parse as a percentage"
            );
        }
        let err = parse_exit_pct("eight").expect_err("not a decimal");
        assert!(err.contains("decimal percentage"), "{err}");
    }

    // ── #441: a value the kernel cannot honour is a startup error, never a
    //    silent fallback to the compiled default ────────────────────────────
    //
    // The parser's own exit(2) cannot be observed from an in-process test, so
    // the property under test here is the one that a fallback would break:
    // a well-spelled flag with a garbage value must never silently resolve to
    // the compiled default. Where the old arms returned the default, the new
    // arms terminate — so ANY return from parse_args with a garbage value is
    // already a defect, and the valid-value arms below double as proof the
    // strictness did not eat legitimate spellings (trims, negatives where the
    // knob means "off", explicit zero).

    /// A numeric knob parses the value it is given, trimmed — not the default.
    #[test]
    fn numeric_flags_carry_the_value_they_are_given() {
        let a = args_from(&["--tick-ms", "120"]);
        assert_eq!(a.tick_ms, 120);
        let a = args_from(&["--max-positions", "9"]);
        assert_eq!(a.max_positions, 9);
        let a = args_from(&["--regime-confirmations", "5"]);
        assert_eq!(a.regime_confirmations, 5);
        let a = args_from(&["--event-archive-rotate-mb", "64"]);
        assert_eq!(a.event_archive_rotate_mb, Some(64));
        let a = args_from(&["--entry-maker-timeout-ms", "-1"]);
        assert_eq!(a.entry_maker_timeout_ms, -1, "negative = never escalate");
        let a = args_from(&["--max-orderbook-stale-ms", "0"]);
        assert_eq!(a.max_orderbook_stale_ms, 0, "0 = freshness check off");
    }

    /// The decimal knobs keep their exact decimal — the sizing and limit
    /// family where a fallback would quietly re-arm the shipped cap.
    #[test]
    fn decimal_flags_carry_the_value_they_are_given() {
        let a = args_from(&["--seed-balance", " 2500.5 "]);
        assert_eq!(a.seed_balance, dec!(2500.5));
        let a = args_from(&["--max-order-notional", "33.25"]);
        assert_eq!(a.max_order_notional, dec!(33.25));
        let a = args_from(&["--size-pct", "20"]);
        assert_eq!(a.size_pct, dec!(20));
        let a = args_from(&["--spread-arb-entry-factor", "1.2"]);
        assert_eq!(a.spread_arb_entry_factor, Some(dec!(1.2)));
        let a = args_from(&["--regime-min-efficiency", "0.55"]);
        assert_eq!(a.regime_min_efficiency, dec!(0.55));
    }

    /// The mode is only ever what the operator spelled: `--mode live` is live,
    /// `--mode dry` is dry — there is no spelling that falls back to dry.
    #[test]
    fn the_mode_is_only_ever_the_spelled_one() {
        assert!(matches!(args_from(&["--mode", "live"]).mode, Mode::Live));
        assert!(matches!(args_from(&["--mode", "dry"]).mode, Mode::Dry));
    }

    /// The boolean switches accept the explicit true|false vocabulary and
    /// carry it (an explicit OFF must stay off — the whole point of the
    /// `true|false` spelling).
    #[test]
    fn boolean_switches_carry_their_explicit_value() {
        let a = args_from(&["--se-auto-evolve", "false"]);
        assert_eq!(a.se_auto_evolve, Some(false));
        let a = args_from(&["--update-check", "true"]);
        assert_eq!(a.update_check_enabled, Some(true));
        let a = args_from(&["--update-check", "0"]);
        assert_eq!(a.update_check_enabled, Some(false));
        let a = args_from(&["--se-auto-evolve", "on"]);
        assert_eq!(a.se_auto_evolve, Some(true));
    }

    /// The update interval keeps 0 (scheduler off) and positive values; the
    /// old `.filter(>=0)` silently DROPPED a negative into the default — the
    /// negative is now the caller's problem at the parse arm, so any value
    /// that reaches Args is one the scheduler can honour.
    #[test]
    fn the_update_interval_keeps_explicit_zero_and_positives() {
        let a = args_from(&["--update-interval-secs", "0"]);
        assert_eq!(a.update_interval_secs, Some(0));
        let a = args_from(&["--update-interval-secs", "3600"]);
        assert_eq!(a.update_interval_secs, Some(3600));
    }

    // 任务二 1-3: the NTP probe flags. The empty host string is the documented
    // "off" spelling (zero network tasks — the replay-safe posture); an
    // explicit 0 interval disarms the periodic loop; a negative interval is a
    // startup error, never a silent default.
    #[test]
    fn the_ntp_flags_keep_their_off_spellings_and_defaults() {
        let a = args_from(&[]);
        assert_eq!(a.ntp_host, "pool.ntp.org");
        assert_eq!(a.ntp_interval_secs, 300);
        let a = args_from(&["--ntp-host", ""]);
        assert_eq!(a.ntp_host, "", "empty host = probe off");
        assert!(
            !blitzkrieg_core::probe::should_spawn_ntp(&a.ntp_host),
            "an empty host must disarm the probe entirely (replay safety)"
        );
        let a = args_from(&["--ntp-host", "time.apple.com"]);
        assert_eq!(a.ntp_host, "time.apple.com");
        let a = args_from(&["--ntp-interval-secs", "0"]);
        assert_eq!(a.ntp_interval_secs, 0, "0 = the periodic loop is off");
        let a = args_from(&["--ntp-interval-secs", "60"]);
        assert_eq!(a.ntp_interval_secs, 60);
        // Whitespace host trims to empty = off, same as the explicit "".
        let a = args_from(&["--ntp-host", "   "]);
        assert!(!blitzkrieg_core::probe::should_spawn_ntp(&a.ntp_host));
    }

    // The negative `--ntp-interval-secs` refusal is a `bad_value` exit(2) in
    // the parse arm — the parser's own exit cannot be observed from an
    // in-process test (same posture as the #441 strict knobs above): any value
    // that REACHED Args with a negative sign would already be the defect, and
    // the valid-value arms above prove the strictness did not eat the
    // legitimate spellings (explicit zero = off, positives).

    /// Backtest mode: the three documented spellings pass through, and a bare
    /// `--backtest-mode` (no value) keeps the historic empty-mode single-run
    /// shape — that default is documented, not a fallback a typo can hit
    /// (a typo'd value is refused, the missing value is the default).
    #[test]
    fn backtest_mode_passes_the_three_documented_spellings() {
        for mode in ["mine", "verify", "sweep"] {
            let a = args_from(&["--backtest-mode", mode]);
            assert_eq!(a.backtest_mode, mode);
        }
        let a = args_from(&["--backtest-mode"]);
        assert_eq!(a.backtest_mode, "", "missing value = the historic default");
    }

    // ── #265: the zero-strategy refusal and its opt-out ─────────────────────

    /// The flag is parsed, defaults to OFF (an ordinary zero-strategy boot is
    /// not refused), and the request list is what the operator named — the
    /// persisted set is merged in elsewhere and is not an explicit request.
    #[test]
    fn allow_zero_strategies_is_parsed_and_off_by_default() {
        let a = args_from(&[]);
        assert!(!a.allow_zero_strategies);
        assert!(a.enable_strategy.is_empty());
        let a = args_from(&[
            "--enable-strategy",
            "alpha_strategy",
            "--allow-zero-strategies",
        ]);
        assert!(a.allow_zero_strategies);
        assert_eq!(a.enable_strategy, vec!["alpha_strategy".to_string()]);
    }

    // ── #173: the daily-loss breaker's CLI/env surface ─────────────────────

    /// The compiled default is RELATIVE: an absolute default is meaningless
    /// across account sizes (the old 200 USD was no protection at all on a
    /// 4.8 USDC book), so the percentage carries the default protection.
    #[test]
    fn the_daily_loss_budget_defaults_to_a_relative_cap_and_is_always_echoed() {
        let a = args_from(&[]);
        assert_eq!(
            a.daily_loss_usd,
            Decimal::ZERO,
            "no absolute cap by default"
        );
        assert_eq!(a.daily_loss_pct, dec!(20));
        assert!(
            a.config_report.is_empty(),
            "the echo is not part of config_report: {:?}",
            a.config_report
        );
        let echo = a.daily_loss_echo.join("\n");
        assert!(echo.contains("relative cap 20%"), "{echo}");
        assert!(echo.contains("default"), "{echo}");
    }

    #[test]
    fn a_max_daily_loss_flag_overrides_the_budget_and_is_reported() {
        let a = args_from(&["--max-daily-loss", "5"]);
        assert_eq!(a.daily_loss_usd, dec!(5));
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "positions.max_daily_loss_usd=5 (cli)"),
            "{:?}",
            a.config_report
        );
        // The echo reports the effective configuration whether or not it moved.
        assert!(
            a.daily_loss_echo
                .join("\n")
                .contains("absolute cap $5 (cli)")
        );
    }

    #[test]
    fn the_daily_loss_budget_is_settable_from_the_environment() {
        let args = ["--no-config".to_string()];
        let env = env_of(&[
            ("BK_MAX_DAILY_LOSS", "3.5"),
            ("BK_MAX_DAILY_LOSS_PCT", "10"),
        ]);
        let a = parse_args(&blitzkrieg_core::config::FileConfig::default(), &args, &env);
        assert_eq!(a.daily_loss_usd, dec!(3.5));
        assert_eq!(a.daily_loss_pct, dec!(10));
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "positions.max_daily_loss_usd=3.5 (env)"),
            "{:?}",
            a.config_report
        );
        // CLI outranks env, like every other setting in the chain.
        let a = parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &["--max-daily-loss".into(), "9".into()],
            &env,
        );
        assert_eq!(a.daily_loss_usd, dec!(9));
    }

    /// 0/0 is the one configuration that protects nothing, so it says so out loud.
    #[test]
    fn a_disabled_daily_loss_budget_is_announced() {
        let a = args_from(&["--max-daily-loss", "0", "--max-daily-loss-pct", "0"]);
        assert!(a.daily_loss_echo.join("\n").contains("DISABLED"));
    }

    /// The budget's side file is a sibling of the position log, the way the
    /// `.recon` watermark is: same directory, derived name.
    #[test]
    fn the_daily_loss_file_follows_the_position_log() {
        assert_eq!(
            daily_loss_path_for("data/positions/positions.jsonl"),
            "data/positions/positions.daily-loss.json"
        );
        assert_eq!(
            daily_loss_path_for("/tmp/x/pos.jsonl"),
            "/tmp/x/pos.daily-loss.json"
        );
    }

    // ── #202: the equity-relative sizing / per-order bound ──────────────────

    /// Both knobs default to OFF — the shipped absolute path, order sizes
    /// unchanged — and the boot log says so, including the number the operator
    /// would otherwise have to derive: the widest ticket the defaults allow as a
    /// share of the live 4.8 USDC book.
    #[test]
    fn the_equity_knobs_default_to_off_and_the_echo_states_the_worst_case() {
        let a = args_from(&[]);
        assert_eq!(a.size_pct, Decimal::ZERO);
        assert_eq!(a.max_order_notional_pct, Decimal::ZERO);
        assert!(
            !a.config_report
                .iter()
                .any(|l| l.contains("size_pct") || l.contains("notional_pct")),
            "nothing explicit, nothing reported: {:?}",
            a.config_report
        );
        let echo = a.sizing_echo.join("\n");
        assert!(echo.contains("absolute size_usd"), "{echo}");
        assert!(
            echo.contains("(default; --size-pct/BK_SIZE_PCT off)"),
            "{echo}"
        );
        assert!(echo.contains("NO equity-relative cap"), "{echo}");
        assert!(echo.contains("208% of a 4.8 USDC book"), "{echo}");
        assert!(echo.contains("scripts/risk-sizing-check.mjs"), "{echo}");
    }

    #[test]
    fn the_equity_knobs_are_settable_by_flag_and_by_env() {
        let a = args_from(&["--size-pct", "20", "--max-order-notional-pct", "20"]);
        assert_eq!(a.size_pct, dec!(20));
        assert_eq!(a.max_order_notional_pct, dec!(20));
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.size_pct=20 (cli)"),
            "{:?}",
            a.config_report
        );
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "risk.max_order_notional_pct=20 (cli)"),
            "{:?}",
            a.config_report
        );
        let echo = a.sizing_echo.join("\n");
        assert!(
            echo.contains("20% of cash equity (cli; replaces size_usd)"),
            "{echo}"
        );
        assert!(echo.contains("REJECTED, never truncated"), "{echo}");

        // Same chain as every other knob: env supplies, CLI outranks.
        let env = env_of(&[("BK_SIZE_PCT", "7.5"), ("BK_MAX_ORDER_NOTIONAL_PCT", "15")]);
        let b = parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &["--no-config".to_string()],
            &env,
        );
        assert_eq!(b.size_pct, dec!(7.5));
        assert_eq!(b.max_order_notional_pct, dec!(15));
        assert!(
            b.config_report
                .iter()
                .any(|l| l == "engine.size_pct=7.5 (env)"),
            "{:?}",
            b.config_report
        );
        let c = parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &["--size-pct".to_string(), "9".to_string()],
            &env,
        );
        assert_eq!(c.size_pct, dec!(9), "CLI must outrank the environment");
        assert_eq!(
            c.max_order_notional_pct,
            dec!(15),
            "untouched env knob stays"
        );
    }

    #[test]
    fn eight_segment_form_parses_the_legs_own_equity_pct() {
        // #202: the 8th segment is the leg's own share of the account.
        let l = parse_one("spread_arb:2:20:1.5:4:8:0.5:10").expect("8-segment form must parse");
        assert_eq!(l.size_weight, Some(dec!(0.5)));
        assert_eq!(l.size_pct, Some(dec!(10)));
        assert!(l.sizing().overrides_anything());
        // The weight slot stays mandatory, so a percentage can never be read as
        // a weight ("-" skips the weight, the same convention as every segment).
        let l2 = parse_one("spread_arb:2:20:1.5:4:8:-:10").expect("blank weight = unweighted");
        assert_eq!(l2.size_weight, None);
        assert_eq!(l2.size_pct, Some(dec!(10)));
        // The 7- and 6-segment forms keep their own meaning: no percentage.
        assert_eq!(
            parse_one("spread_arb:2:20:1.5:4:8:0.5").unwrap().size_pct,
            None
        );
        assert_eq!(parse_one("spread_arb:2:20:1.5:4:8").unwrap().size_pct, None);
    }

    /// #363: the per-asset cap parses from the flag and stays `None` (the
    /// policy file decides) when the flag is absent. Lockstep with cli.rs's
    /// FLAGS entry is pinned by the same crate test that pins every other
    /// flag; this pins the PARSER half.
    #[test]
    fn the_per_asset_position_cap_parses_and_defaults_to_none() {
        let file = file_with("");
        let a = parse_args(
            &file,
            &["--max-positions-per-asset".to_string(), "3".to_string()],
            &EnvVars::default(),
        );
        assert_eq!(a.max_positions_per_asset, Some(3));
        let a = parse_args(&file, &[], &EnvVars::default());
        assert_eq!(
            a.max_positions_per_asset, None,
            "absent flag = the execution-policy file decides"
        );
    }

    #[test]
    fn the_file_supplies_a_value_when_no_flag_does() {
        let file = file_with(
            r#"
            [engine]
            assets = ["DOGE"]
            round_sec = 300
            min_round_age_sec = 5
            min_time_left_sec = 7
            "#,
        );
        let a = parse_args(&file, &[], &EnvVars::default());
        assert_eq!(a.round_sec, 300);
        assert_eq!(a.min_round_age, 5);
        assert_eq!(a.min_time_left, 7);
        assert_eq!(a.assets, vec!["DOGE"]);
        // Each resolved value names its origin, so the log can answer "why 300?".
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.round_sec=300 (toml)")
        );
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.min_time_left_sec=7 (toml)")
        );
    }

    #[test]
    fn a_flag_beats_the_file() {
        let file = file_with(
            r#"
            [engine]
            round_sec = 300
            min_time_left_sec = 7
            "#,
        );
        let a = parse_args(
            &file,
            &["--round-sec".into(), "60".into()],
            &EnvVars::default(),
        );
        assert_eq!(a.round_sec, 60, "CLI outranks TOML");
        assert_eq!(
            a.min_time_left, 7,
            "and the untouched key still comes from TOML"
        );
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.round_sec=60 (cli)")
        );
    }

    // The UI card's persisted cell outranks the factory TOML: only an explicit
    // flag/env var suppresses it. #474's "旗标 > 格 > 默认" promise, kept.
    #[test]
    fn a_toml_round_sec_does_not_veto_the_persisted_cell() {
        // TOML spoke → the cell still applies (the boot site tests
        // `!= "cli_or_env"`, and "default" is what TOML resolves to here).
        let file = file_with(
            r#"
            [engine]
            round_sec = 300
            "#,
        );
        let a = parse_args(&file, &[], &EnvVars::default());
        assert_eq!(round_duration_source_from(&a.config_report), "default");
        // A CLI flag spoke → the cell is suppressed.
        let a = parse_args(
            &file,
            &["--round-sec".into(), "60".into()],
            &EnvVars::default(),
        );
        assert_eq!(round_duration_source_from(&a.config_report), "cli_or_env");
        // The gate site and the judge share one spelling: a source the judge
        // emits as "cli_or_env" is exactly the one the gate refuses.
        assert_ne!("cli_or_env", "default");
    }

    #[test]
    fn the_file_can_turn_shadow_evolution_on_and_the_flag_is_a_no_op() {
        // The documented way to enable the feature: edit the file. Before KI-11
        // this had no effect at all.
        let file = file_with(
            r#"
            [shadow_evolution]
            enabled = true
            min_sample_count = 12
            cooldown_minutes = 2
            evaluation_window_minutes = 5
            min_observation_minutes = 1
            variant_count = 4
            audit_dir = "data/evo-scratch"
            "#,
        );
        let a = parse_args(&file, &[], &EnvVars::default());
        assert!(a.shadow_evolution);
        assert_eq!(a.se_min_samples, Some(12));
        // Minutes in the file, seconds in the kernel.
        assert_eq!(a.se_cooldown_secs, Some(120));
        assert_eq!(a.se_eval_window_secs, Some(300));
        assert_eq!(a.se_min_obs_secs, Some(60));
        assert_eq!(a.se_variant_count, Some(4));
        assert_eq!(a.se_audit_dir.as_deref(), Some("data/evo-scratch"));
    }

    #[test]
    fn a_strategy_section_is_reported_as_dead_config_not_silently_ignored() {
        // PR-B removed `[strategy] active`: strategies are never named in kernel
        // config any more (they are auto-loaded from strategy_dir and enabled on
        // load). An old file must not silently look like it did something.
        let file = file_with(
            r#"
            [strategy]
            active = ["spread_arb", "trend_follow"]
            "#,
        );
        assert!(
            file.unknown_keys.iter().any(|k| k == "strategy"),
            "the dead section must be reported: {:?}",
            file.unknown_keys
        );
        let a = parse_args(&file, &[], &EnvVars::default());
        assert!(
            a.enable_strategy.is_empty(),
            "the kernel must not invent an enable list from the file: {:?}",
            a.enable_strategy
        );
    }

    #[test]
    fn strategy_selection_comes_from_cli_flags_only() {
        let a = parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &[
                "--enable-strategy".into(),
                "alpha_strategy".into(),
                "--disable-strategy".into(),
                "spread_arb".into(),
            ],
            &EnvVars::default(),
        );
        assert_eq!(a.enable_strategy, vec!["alpha_strategy"]);
        assert_eq!(a.disable_strategy, vec!["spread_arb"]);
    }

    #[test]
    fn a_gradient_above_lock_1_is_refused_not_silently_applied() {
        let file = file_with(
            r#"
            [shadow_evolution]
            max_gradient = 0.25
            "#,
        );
        let a = parse_args(&file, &[], &EnvVars::default());
        assert_eq!(
            a.se_max_gradient, None,
            "a request to widen a safety lock must not take effect"
        );

        // Tightening it is allowed.
        let file = file_with(
            r#"
            [shadow_evolution]
            max_gradient = 0.01
            "#,
        );
        let a = parse_args(&file, &[], &EnvVars::default());
        assert_eq!(a.se_max_gradient, Some(dec!(0.01)));
    }

    #[test]
    fn a_broken_file_does_not_stop_startup() {
        // The parse_args half: a FileConfig that failed to load is all-defaults,
        // and the kernel still comes up on CLI + built-in values.
        let a = args_from(&["--round-sec", "120"]);
        assert_eq!(a.round_sec, 120);
        assert_eq!(a.assets, vec!["BTC", "ETH", "SOL", "XRP"]);
    }

    #[test]
    fn prescan_finds_the_config_flag_in_every_documented_spelling() {
        let v = |s: &[&str]| -> Vec<String> { s.iter().map(|x| x.to_string()).collect() };
        match prescan_config(&v(&["--config", "a.toml"])) {
            Some(ConfigChoice::Path(p)) => assert_eq!(p, "a.toml"),
            _ => panic!("--config <path> must parse"),
        }
        match prescan_config(&v(&["--config=b.toml"])) {
            Some(ConfigChoice::Path(p)) => assert_eq!(p, "b.toml"),
            _ => panic!("--config=<path> must parse"),
        }
        assert!(matches!(
            prescan_config(&v(&["--no-config"])),
            Some(ConfigChoice::Off)
        ));
        // Bare `--config` with nothing usable after it means "no file", not "the
        // next flag is my filename".
        assert!(matches!(
            prescan_config(&v(&["--config="])),
            Some(ConfigChoice::Off)
        ));
        assert!(matches!(
            prescan_config(&v(&["--config", ""])),
            Some(ConfigChoice::Off)
        ));
        assert!(prescan_config(&v(&["--engine"])).is_none());
    }

    #[test]
    fn the_env_layer_sits_between_the_flag_and_the_file() {
        let file = file_with(
            r#"
            [engine]
            round_sec = 300
            min_time_left_sec = 7
            "#,
        );
        let env = env_of(&[("BK_ROUND_SEC", "600"), ("BK_MIN_TIME_LEFT_SEC", "9")]);

        // CLI > env.
        let a = parse_args(&file, &["--round-sec".into(), "60".into()], &env);
        assert_eq!(a.round_sec, 60);
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.round_sec=60 (cli)")
        );
        // env > TOML.
        assert_eq!(
            a.min_time_left, 9,
            "the env var outranks the file for a key no flag named"
        );
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.min_time_left_sec=9 (env)")
        );
    }

    #[test]
    fn the_env_layer_alone_can_supply_a_value() {
        let env = env_of(&[
            ("BK_MIN_ROUND_AGE_SEC", "3"),
            ("BK_ASSETS", "btc, eth"),
            ("BK_SHADOW_EVOLUTION", "true"),
            ("BK_SE_COOLDOWN_SECS", "45"),
        ]);
        let a = parse_args(&blitzkrieg_core::config::FileConfig::default(), &[], &env);
        assert_eq!(a.min_round_age, 3);
        // Asset lists are normalised wherever they come from.
        assert_eq!(a.assets, vec!["BTC", "ETH"]);
        assert!(a.shadow_evolution);
        assert_eq!(a.se_cooldown_secs, Some(45));
    }

    #[test]
    fn bk_config_none_turns_the_file_off() {
        let env = env_of(&[("BK_CONFIG", "none")]);
        assert!(matches!(config_choice(&[], &env), ConfigChoice::Off));
        // Without the variable the shipped default is used.
        assert!(matches!(
            config_choice(&[], &EnvVars::default()),
            ConfigChoice::Path(p) if p == blitzkrieg_core::config::DEFAULT_CONFIG_PATH
        ));
    }

    #[test]
    fn a_config_path_that_does_not_exist_is_a_silent_no_op() {
        let file = blitzkrieg_core::config::FileConfig::load(Some(std::path::Path::new(
            "/nonexistent/bk.toml",
        )));
        let a = parse_args(&file, &[], &EnvVars::default());
        assert_eq!(a.round_sec, 900);
        assert!(a.config_report.is_empty());
    }

    // ── #205: the orderbook freshness budget is a runtime knob ──────────────

    /// The default must be the value the kernel has always hardcoded: making the
    /// knob configurable must not move it.
    #[test]
    fn the_orderbook_staleness_default_is_the_historic_eight_seconds() {
        let a = args_from(&[]);
        assert_eq!(a.max_orderbook_stale_ms, 8_000);
        assert_eq!(
            a.max_orderbook_stale_ms,
            blitzkrieg_core::engine::DEFAULT_MAX_ORDERBOOK_STALE_MS
        );
        assert!(
            a.config_report.is_empty(),
            "an unset knob is not a configured value: {:?}",
            a.config_report
        );
        assert!(
            a.orderbook_stale_echo.join(" ").contains("8000 ms"),
            "the boot line must state the effective budget: {:?}",
            a.orderbook_stale_echo
        );
        assert!(a.orderbook_stale_echo.join(" ").contains("default"));
    }

    /// CLI and env both reach it, CLI wins, and the boot line names the tier that
    /// supplied the value — the same shape as `--size-pct` / `BK_SIZE_PCT`.
    #[test]
    fn the_orderbook_staleness_budget_is_configurable_from_cli_and_env() {
        let a = args_from(&["--max-orderbook-stale-ms", "12000"]);
        assert_eq!(a.max_orderbook_stale_ms, 12_000);
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.max_orderbook_stale_ms=12000 (cli)"),
            "{:?}",
            a.config_report
        );
        assert!(a.orderbook_stale_echo.join(" ").contains("12000 ms (cli)"));

        let env = env_of(&[("BK_MAX_ORDERBOOK_STALE_MS", "15000")]);
        let a = parse_args(&blitzkrieg_core::config::FileConfig::default(), &[], &env);
        assert_eq!(a.max_orderbook_stale_ms, 15_000);
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.max_orderbook_stale_ms=15000 (env)"),
            "{:?}",
            a.config_report
        );
        // CLI outranks env, like every other setting in the chain.
        let a = parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &["--max-orderbook-stale-ms".into(), "9000".into()],
            &env,
        );
        assert_eq!(a.max_orderbook_stale_ms, 9_000);
    }

    /// `0` is the supported OFF switch and must be announced as such rather than
    /// rendered as "a 0 ms budget" (which would read as "refuse everything").
    #[test]
    fn a_disabled_orderbook_staleness_check_is_announced() {
        let a = args_from(&["--max-orderbook-stale-ms", "0"]);
        assert_eq!(a.max_orderbook_stale_ms, 0);
        let echo = a.orderbook_stale_echo.join(" ");
        assert!(echo.contains("CHECK DISABLED"), "{echo}");
        assert!(echo.contains("cli"), "the tier is named: {echo}");
        assert!(
            stale_budget_rejection(0).is_none(),
            "0 is legal, not invalid"
        );
    }

    /// Invalid values are rejected by a rule the boot path enforces with exit 2;
    /// this pins the rule itself (the exit is exercised end-to-end from the
    /// shell). A negative budget has no meaning, and anything past the ceiling is
    /// a unit mistake (`8` for 8 seconds, `8_000_000` for 8000 ms).
    #[test]
    fn an_out_of_band_orderbook_staleness_budget_is_rejected() {
        for bad in [-1i64, -8_000, 600_001, 8_000_000] {
            assert!(
                stale_budget_rejection(bad).is_some(),
                "{bad} must be rejected"
            );
        }
        for ok in [0i64, 1, 8_000, 60_000, 600_000] {
            assert!(
                stale_budget_rejection(ok).is_none(),
                "{ok} must be accepted"
            );
        }
        let why = stale_budget_rejection(-5).unwrap();
        assert!(why.contains("0"), "the fix is named: {why}");
        let why = stale_budget_rejection(600_001).unwrap();
        assert!(why.contains("600000"), "the ceiling is named: {why}");
    }

    /// A value that is not a number at all fails the boot the same way, rather
    /// than being silently dropped: the flag parser rejects it outright.
    #[test]
    fn a_non_numeric_orderbook_staleness_budget_is_rejected_at_parse() {
        // `parse_args` exits the process on a bad value, so this asserts the
        // parse rule through the helper the arm uses.
        assert!("8s".trim().parse::<i64>().is_err());
        assert!("".trim().parse::<i64>().is_err());
        assert!(" 8000 ".trim().parse::<i64>().is_ok());
    }

    // ── #439 candidate 1: the args snapshot gate ─────────────────────────────
    //
    // `parse_args` is a published surface (every script and panel flag string
    // rides it), and its complexity is NOT the argv→value matching — that part a
    // parser crate could do — but the contract around it: the CLI > env > TOML >
    // default chain, the five boot-echo wordings, the #228 near-miss refusals,
    // the #441 strict-value exits. The audit's replacement candidate (clap) was
    // rejected on exactly that contract cost (#439 comment). Whatever edits this
    // function next — a refactor, a new knob, or an actual parser swap — must be
    // provably behavior-identical, so the gate renders EVERY `Args` field plus
    // the report/echo lines as text and pins the renderings. A field added to
    // `Args` without a line here is a compile error in `snapshot` itself; a
    // behavior change shows up as a diff.

    /// The snapshot: every `Args` field, one `key=value` line each, in struct
    /// order, followed by the five boot-observable line groups. Deterministic:
    /// no process environment, no clock, no paths resolved outside the input.
    fn snapshot(a: &Args) -> String {
        let mut s = String::new();
        macro_rules! line {
            ($key:literal, $val:expr) => {
                s.push_str(&format!("{}={}\n", $key, $val));
            };
        }
        // Scalars and Options render with their Debug form so a change in TYPE
        // shows up too; Vectors render indexed so reordering is visible.
        // `socket` resolves from $TMPDIR/$USER, so only its SUFFIX (the
        // canonical name) is pinned — the path prefix is the machine's, not the
        // parser's, business.
        let sock = a.socket.rsplit('/').next().unwrap_or(&a.socket).to_string();
        // The canonical name embeds the invoking user (`blitzkrieg-core-<user>`),
        // so the user segment is stripped too — only the `blitzkrieg-core` prefix
        // is machine-independent.
        let canon = if sock.starts_with("blitzkrieg-core-") && sock.ends_with(".sock") {
            "blitzkrieg-core"
        } else {
            &sock
        };
        line!("socket", format!("{canon} (suffix)"));
        line!("mode", format!("{:?}", a.mode));
        line!("readonly", a.readonly.to_string());
        line!("tick_ms", a.tick_ms.to_string());
        line!("seed_balance", a.seed_balance.to_string());
        line!("max_order_notional", a.max_order_notional.to_string());
        line!("max_open_notional", a.max_open_notional.to_string());
        line!("min_shares", format!("{:?}", a.min_shares));
        line!("max_shares", format!("{:?}", a.max_shares));
        line!("size_pct", a.size_pct.to_string());
        line!(
            "max_order_notional_pct",
            a.max_order_notional_pct.to_string()
        );
        vec_lines(&mut s, "sizing_echo", &a.sizing_echo);
        vec_lines(&mut s, "markets", &a.markets);
        line!("auto_exits", a.auto_exits.to_string());
        line!("max_positions", a.max_positions.to_string());
        line!(
            "max_positions_per_asset",
            format!("{:?}", a.max_positions_per_asset)
        );
        line!("execution_policy", format!("{:?}", a.execution_policy));
        line!("daily_loss_usd", a.daily_loss_usd.to_string());
        line!("daily_loss_pct", a.daily_loss_pct.to_string());
        line!(
            "daily_loss_source",
            a.daily_loss_source.as_str().to_string()
        );
        vec_lines(&mut s, "daily_loss_echo", &a.daily_loss_echo);
        line!("engine", a.engine.to_string());
        line!("min_round_age", a.min_round_age.to_string());
        line!("min_time_left", a.min_time_left.to_string());
        line!("trend_confirm", a.trend_confirm.to_string());
        line!("trend_floor_ms", a.trend_floor_ms.to_string());
        line!(
            "spread_arb_entry_factor",
            format!("{:?}", a.spread_arb_entry_factor)
        );
        line!("spread_arb_min_obi", format!("{:?}", a.spread_arb_min_obi));
        line!(
            "spread_arb_max_spread_pct",
            format!("{:?}", a.spread_arb_max_spread_pct)
        );
        line!(
            "spread_arb_dip_max_pct",
            format!("{:?}", a.spread_arb_dip_max_pct)
        );
        line!(
            "spread_arb_bounce_min_pct",
            format!("{:?}", a.spread_arb_bounce_min_pct)
        );
        line!(
            "spread_arb_bounce_window_sec",
            format!("{:?}", a.spread_arb_bounce_window_sec)
        );
        line!("exit_take_profit_pct", a.exit_take_profit_pct.to_string());
        line!("exit_stop_loss_pct", a.exit_stop_loss_pct.to_string());
        line!(
            "exit_trailing_min_high_pct",
            a.exit_trailing_min_high_pct.to_string()
        );
        line!("exit_min_trail_pct", a.exit_min_trail_pct.to_string());
        line!("exit_force_sec", a.exit_force_sec.to_string());
        line!("feed_ws", a.feed_ws.to_string());
        line!("net_check", a.net_check.to_string());
        line!("replay", format!("{:?}", a.replay));
        line!("replay_near_miss", format!("{:?}", a.replay_near_miss));
        line!("round_sec", a.round_sec.to_string());
        line!("near_miss_path", format!("{:?}", a.near_miss_path));
        line!("trade_log", format!("{:?}", a.trade_log));
        line!("no_trade_log", a.no_trade_log.to_string());
        line!("order_log", format!("{:?}", a.order_log));
        line!("no_order_log", a.no_order_log.to_string());
        line!("position_log", format!("{:?}", a.position_log));
        line!("no_position_log", a.no_position_log.to_string());
        line!("ledger_journal_dir", format!("{:?}", a.ledger_journal_dir));
        line!("no_intent_audit", a.no_intent_audit.to_string());
        line!("allow_shared_data", a.allow_shared_data.to_string());
        line!("market_plugin", format!("{:?}", a.market_plugin));
        line!("discovery", a.discovery.to_string());
        line!("shadow_evolution", a.shadow_evolution.to_string());
        vec_lines(&mut s, "assets", &a.assets);
        line!("se_min_samples", format!("{:?}", a.se_min_samples));
        line!("se_cooldown_secs", format!("{:?}", a.se_cooldown_secs));
        line!("se_min_obs_secs", format!("{:?}", a.se_min_obs_secs));
        line!(
            "se_eval_window_secs",
            format!("{:?}", a.se_eval_window_secs)
        );
        line!("se_max_gradient", format!("{:?}", a.se_max_gradient));
        line!("se_variant_count", format!("{:?}", a.se_variant_count));
        line!("se_audit_dir", format!("{:?}", a.se_audit_dir));
        line!("se_min_win_rate", format!("{:?}", a.se_min_win_rate));
        line!(
            "se_min_profit_factor",
            format!("{:?}", a.se_min_profit_factor)
        );
        line!("se_auto_evolve", format!("{:?}", a.se_auto_evolve));
        line!("se_cycle_secs", format!("{:?}", a.se_cycle_secs));
        line!("se_ttl_secs", format!("{:?}", a.se_ttl_secs));
        line!("se_deep_dims", format!("{:?}", a.se_deep_dims));
        line!(
            "update_check_enabled",
            format!("{:?}", a.update_check_enabled)
        );
        line!("update_auto", format!("{:?}", a.update_auto));
        line!(
            "update_interval_secs",
            format!("{:?}", a.update_interval_secs)
        );
        vec_lines(&mut s, "strategy_limits", &a.strategy_limits);
        vec_lines(&mut s, "virtual_limits", &a.virtual_limits);
        vec_lines(&mut s, "enable_strategy", &a.enable_strategy);
        vec_lines(&mut s, "disable_strategy", &a.disable_strategy);
        line!("allow_zero_strategies", a.allow_zero_strategies.to_string());
        line!("strategy_state", format!("{:?}", a.strategy_state));
        // `strategy_dir`/`lua_strategy_dir` resolve against the checkout (an
        // absolute path the repo layout decides); only the trailing segment the
        // parser's own default logic names is pinned, not the checkout's mount
        // point. An explicit `--strategy-dir` still lands here verbatim.
        for (key, v) in [
            ("strategy_dir", &a.strategy_dir),
            ("lua_strategy_dir", &a.lua_strategy_dir),
        ] {
            let shown = match v {
                Some(p) => {
                    let tail = p.rsplit('/').next().unwrap_or(p);
                    format!("Some(…/{tail})")
                }
                None => "None".to_string(),
            };
            s.push_str(&format!("{key}={shown}\n"));
        }
        line!("event_archive", format!("{:?}", a.event_archive));
        line!("no_event_archive", a.no_event_archive.to_string());
        line!(
            "event_archive_max_mb",
            format!("{:?}", a.event_archive_max_mb)
        );
        line!(
            "event_archive_rotate_mb",
            format!("{:?}", a.event_archive_rotate_mb)
        );
        line!(
            "event_archive_min_free_mb",
            format!("{:?}", a.event_archive_min_free_mb)
        );
        line!(
            "entry_maker_timeout_ms",
            a.entry_maker_timeout_ms.to_string()
        );
        line!(
            "max_orderbook_stale_ms",
            a.max_orderbook_stale_ms.to_string()
        );
        vec_lines(&mut s, "orderbook_stale_echo", &a.orderbook_stale_echo);
        line!("ntp_host", format!("{:?}", a.ntp_host));
        line!("ntp_interval_secs", a.ntp_interval_secs.to_string());
        vec_lines(&mut s, "ntp_echo", &a.ntp_echo);
        line!("backtest", format!("{:?}", a.backtest));
        line!("backtest_report", format!("{:?}", a.backtest_report));
        line!("backtest_tick_ms", a.backtest_tick_ms.to_string());
        line!("backtest_tail_ms", a.backtest_tail_ms.to_string());
        for (i, (name, knob, v)) in a.backtest_knobs.iter().enumerate() {
            s.push_str(&format!("backtest_knobs[{i}]={name}:{knob}={v}\n"));
        }
        line!("backtest_mode", format!("{:?}", a.backtest_mode));
        line!("verify_latency_ms", a.verify_latency_ms.to_string());
        line!(
            "fee_model",
            match a.fee_model {
                Some(f) => format!("{:?}", f),
                None => "None".to_string(),
            }
        );
        line!("backtest_fast", a.backtest_fast.to_string());
        line!("regime_eval", format!("{:?}", a.regime_eval));
        line!("regime_report", format!("{:?}", a.regime_report));
        line!("regime_token", format!("{:?}", a.regime_token));
        line!("regime_max_tokens", a.regime_max_tokens.to_string());
        line!("regime_window_sec", a.regime_window_sec.to_string());
        line!(
            "regime_min_trend_ticks",
            a.regime_min_trend_ticks.to_string()
        );
        line!("regime_min_efficiency", a.regime_min_efficiency.to_string());
        line!(
            "regime_volatile_mad_ticks",
            a.regime_volatile_mad_ticks.to_string()
        );
        line!("regime_confirmations", a.regime_confirmations.to_string());
        line!("onchain_pull", format!("{:?}", a.onchain_pull));
        line!("onchain_start", format!("{:?}", a.onchain_start));
        line!("onchain_end", format!("{:?}", a.onchain_end));
        line!("onchain_asset", format!("{:?}", a.onchain_asset));
        line!("onchain_market", format!("{:?}", a.onchain_market));
        line!("onchain_fills", format!("{:?}", a.onchain_fills));
        line!("onchain_out_dir", format!("{:?}", a.onchain_out_dir));
        line!("blueprint_compile", format!("{:?}", a.blueprint_compile));
        line!("blueprint_out", format!("{:?}", a.blueprint_out));
        line!("slippage_ticks", a.slippage_ticks.to_string());
        line!("latency_ms", a.latency_ms.to_string());
        line!("taker_latency_ms", a.taker_latency_ms.to_string());
        line!("taker_rtt_ms", a.taker_rtt_ms.to_string());
        line!("fill_prob_bps", format!("{:?}", a.fill_prob_bps));
        line!(
            "maker_depth_share_bps",
            format!("{:?}", a.maker_depth_share_bps)
        );
        line!("dry_redeem_fail", a.dry_redeem_fail.to_string());
        line!("dry_redeem_manual", a.dry_redeem_manual.to_string());
        vec_lines(&mut s, "config_report", &a.config_report);
        vec_lines(&mut s, "shadow_evolution_echo", &a.shadow_evolution_echo);
        s
    }

    fn vec_lines(s: &mut String, key: &str, v: &[String]) {
        for (i, item) in v.iter().enumerate() {
            s.push_str(&format!("{key}[{i}]={item}\n"));
        }
    }

    /// The default boot (no flags, no file, no env): the shipped posture, pinned
    /// line for line. This is the snapshot every flag-specific test diffs
    /// against, so a new knob's default lands HERE first.
    #[test]
    fn snapshot_the_default_boot_is_pinned() {
        let a = args_from(&[]);
        let want = "\
socket=blitzkrieg-core (suffix)\n\
mode=Dry\n\
readonly=false\n\
tick_ms=50\n\
seed_balance=10000\n\
max_order_notional=100\n\
max_open_notional=0\n\
min_shares=None\n\
max_shares=None\n\
size_pct=0\n\
max_order_notional_pct=0\n\
sizing_echo[0]=order sizing: per-entry budget absolute size_usd (default; --size-pct/BK_SIZE_PCT off) (0 = off: the absolute size_usd path, order sizes unchanged), per-order notional cap 0% of the account's cash equity (0 = off)\n\
sizing_echo[1]=per-order bounds in force: absolute cap $100, plus NO equity-relative cap (relative cap ON: an over-cap order is REJECTED, never truncated, and closing intents are exempt)\n\
sizing_echo[2]=per-order bounds in force: this account has NO bound as a share of itself — one order may commit the whole of `max_shares × max_price` (10 shares × 1.00 = 10.00 USD by default, 208% of a 4.8 USDC book). Pass --max-order-notional-pct <pct> or run `node scripts/risk-sizing-check.mjs` to see the number\n\
auto_exits=true\n\
max_positions=2\n\
max_positions_per_asset=None\n\
execution_policy=None\n\
daily_loss_usd=0\n\
daily_loss_pct=20\n\
daily_loss_source=default\n\
daily_loss_echo[0]=daily loss breaker: absolute cap $0 (default), relative cap 20% of the day's opening cash equity (default); the TIGHTER applies, 0 = off\n\
engine=false\n\
min_round_age=30\n\
min_time_left=180\n\
trend_confirm=60\n\
trend_floor_ms=10000\n\
spread_arb_entry_factor=None\n\
spread_arb_min_obi=None\n\
spread_arb_max_spread_pct=None\n\
spread_arb_dip_max_pct=None\n\
spread_arb_bounce_min_pct=None\n\
spread_arb_bounce_window_sec=None\n\
exit_take_profit_pct=100\n\
exit_stop_loss_pct=12\n\
exit_trailing_min_high_pct=15\n\
exit_min_trail_pct=8\n\
exit_force_sec=0\n\
feed_ws=false\n\
net_check=false\n\
replay=None\n\
replay_near_miss=None\n\
round_sec=900\n\
near_miss_path=None\n\
trade_log=None\n\
no_trade_log=false\n\
order_log=None\n\
no_order_log=false\n\
position_log=None\n\
no_position_log=false\n\
ledger_journal_dir=Some(\"data/ledger\")\n\
no_intent_audit=false\n\
allow_shared_data=false\n\
market_plugin=None\n\
discovery=true\n\
shadow_evolution=false\n\
assets[0]=BTC\n\
assets[1]=ETH\n\
assets[2]=SOL\n\
assets[3]=XRP\n\
se_min_samples=None\n\
se_cooldown_secs=None\n\
se_min_obs_secs=None\n\
se_eval_window_secs=None\n\
se_max_gradient=None\n\
se_variant_count=None\n\
se_audit_dir=None\n\
se_min_win_rate=None\n\
se_min_profit_factor=None\n\
se_auto_evolve=None\n\
se_cycle_secs=None\n\
se_ttl_secs=None\n\
se_deep_dims=None\n\
update_check_enabled=None\n\
update_auto=None\n\
update_interval_secs=None\n\
allow_zero_strategies=false\n\
strategy_state=Some(\"data/strategy-state.json\")\n\
strategy_dir=Some(…/strategies)\n\
lua_strategy_dir=Some(…/strategies_lua)\n\
event_archive=None\n\
no_event_archive=false\n\
event_archive_max_mb=None\n\
event_archive_rotate_mb=None\n\
event_archive_min_free_mb=None\n\
entry_maker_timeout_ms=5000\n\
max_orderbook_stale_ms=8000\n\
orderbook_stale_echo[0]=orderbook freshness: the engine refuses to price off a book older than 8000 ms (default); past that it stops taking entries until the feed catches up (closing intents are never blocked by this)\n\
ntp_host=\"pool.ntp.org\"\n\
ntp_interval_secs=300\n\
ntp_echo[0]=environment probes: NTP clock-offset probe ON — every 300 s against pool.ntp.org (default/default); |offset| >= 50 ms warns, >= 100 ms flags probe.ntp.skewRed\n\
backtest=None\n\
backtest_report=None\n\
backtest_tick_ms=50\n\
backtest_tail_ms=0\n\
backtest_mode=\"\"\n\
verify_latency_ms=286\n\
fee_model=None\n\
backtest_fast=false\n\
regime_eval=None\n\
regime_report=None\n\
regime_token=None\n\
regime_max_tokens=3\n\
regime_window_sec=300\n\
regime_min_trend_ticks=3\n\
regime_min_efficiency=0.5\n\
regime_volatile_mad_ticks=1.5\n\
regime_confirmations=2\n\
onchain_pull=None\n\
onchain_start=None\n\
onchain_end=None\n\
onchain_asset=None\n\
onchain_market=None\n\
onchain_fills=None\n\
onchain_out_dir=None\n\
blueprint_compile=None\n\
blueprint_out=None\n\
slippage_ticks=0\n\
latency_ms=0\n\
taker_latency_ms=0\n\
taker_rtt_ms=0\n\
fill_prob_bps=None\n\
maker_depth_share_bps=None\n\
dry_redeem_fail=0\n\
dry_redeem_manual=false\n\
shadow_evolution_echo[0]=shadow evolution: evaluator OFF at startup (default); the persisted runtime switch in state.json wins over it, and any disagreement is reported below\n";
        let got = snapshot(&a);
        // Diff line-by-line so a failure NAMES the drifted field instead of
        // dumping 120 lines at the reader.
        let mut diffs: Vec<String> = Vec::new();
        for (i, (g, w)) in got.lines().zip(want.lines()).enumerate() {
            if g != w {
                diffs.push(format!("line {}: got {g:?}, want {w:?}", i + 1));
            }
        }
        assert!(
            diffs.is_empty() && got.lines().count() == want.lines().count(),
            "the default-boot snapshot drifted:\n{}",
            diffs.join("\n")
        );
        // Nothing was configured, so the report is empty and the echoes are the
        // shipped posture — pinned here because the boot log prints them on
        // EVERY run, including runs that set nothing.
        assert!(a.config_report.is_empty(), "{:?}", a.config_report);
        let sizing = a.sizing_echo.join(" | ");
        assert!(
            sizing.contains("absolute size_usd (default")
                && sizing.contains("NO equity-relative cap")
        );
        let breaker = a.daily_loss_echo.join(" | ");
        assert!(
            breaker.contains("absolute cap $0") && breaker.contains("the TIGHTER applies, 0 = off")
        );
        let stale = a.orderbook_stale_echo.join(" | ");
        assert!(stale.contains("older than 8000 ms") && stale.contains("(default)"));
        let se = a.shadow_evolution_echo.join(" | ");
        assert!(se.contains("OFF") && se.contains("default"));
    }

    // ── #439 candidate 1: the snapshot as a flag-competition gate ────────────
    // The default boot pins the shipped posture; these pin HOW values move. Each
    // test diffs the snapshot against the default and asserts the changed lines
    // are EXACTLY the ones the flag owns — a knob that leaks into a neighbor's
    // line (or a precedence bug that keeps a line at its loser's value) fails
    // here with the offending lines named. The same diff-based shape keeps the
    // gate readable as flags multiply: no test grows a second 120-line literal.

    /// The logical change-set between two snapshots: one entry per snapshot key
    /// whose value(s) moved, in the snapshot's own key order, as
    /// `(key, old, new)`. Index suffixes on vector keys (`assets[0]`) are
    /// collapsed, so a set that shrinks or grows diffs as ONE line instead of
    /// index churn — `assets: BTC, ETH, SOL, XRP -> DOGE`, not four deletions.
    fn diff_keys(base: &str, other: &str) -> Vec<(String, String, String)> {
        fn parse(s: &str) -> Vec<(String, Vec<String>)> {
            let mut order: Vec<String> = Vec::new();
            let mut map: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for line in s.lines() {
                let (k, v) = line.split_once('=').expect("snapshot lines are key=value");
                let logical = k.split('[').next().unwrap_or(k).to_string();
                if !map.contains_key(&logical) {
                    order.push(logical.clone());
                }
                map.entry(logical).or_default().push(v.to_string());
            }
            order
                .into_iter()
                .map(|k| {
                    let v = map.remove(&k).unwrap_or_default();
                    (k, v)
                })
                .collect()
        }
        let a = parse(base);
        let b = parse(other);
        let mut out: Vec<(String, String, String)> = Vec::new();
        for (k, av) in &a {
            let bv = b.iter().find(|(bk, _)| bk == k).map(|(_, v)| v.clone());
            if bv.as_ref() != Some(av) {
                let old = if av.is_empty() {
                    "(none)".to_string()
                } else {
                    av.join("; ")
                };
                let new = match &bv {
                    Some(v) if !v.is_empty() => v.join("; "),
                    Some(_) => "(none)".to_string(),
                    None => "<absent>".to_string(),
                };
                out.push((k.clone(), old, new));
            }
        }
        for (k, bv) in &b {
            if !a.iter().any(|(ak, _)| ak == k) {
                out.push((k.clone(), "<absent>".to_string(), bv.join("; ")));
            }
        }
        out
    }

    /// The keys that moved, in snapshot order — the exact footprint of an argv.
    fn changed_keys(d: &[(String, String, String)]) -> Vec<&str> {
        d.iter().map(|(k, _, _)| k.as_str()).collect()
    }

    /// The `(old, new)` pair for one moved key; panics if it did not move.
    fn moved<'a>(d: &'a [(String, String, String)], key: &str) -> (&'a str, &'a str) {
        let (_, o, n) = d
            .iter()
            .find(|(k, _, _)| k == key)
            .unwrap_or_else(|| panic!("key {key} did not move in the diff"));
        (o.as_str(), n.as_str())
    }

    #[test]
    fn snapshot_single_flag_moves_exactly_its_own_lines() {
        let base = snapshot(&args_from(&[]));
        let a = args_from(&[
            "--readonly",
            "--max-positions",
            "5",
            "--exit-force-sec",
            "120",
        ]);
        let d = diff_keys(&base, &snapshot(&a));
        // Exactly the keys this argv owns — a knob that leaks into a neighbor's
        // line (or a precedence bug that keeps a line at its loser's value)
        // fails here with the offending key named.
        assert_eq!(
            changed_keys(&d),
            [
                "readonly",
                "max_positions",
                "exit_force_sec",
                "config_report"
            ]
        );
        assert_eq!(moved(&d, "readonly"), ("false", "true"));
        assert_eq!(moved(&d, "max_positions"), ("2", "5"));
        assert_eq!(moved(&d, "exit_force_sec"), ("0", "120"));
        // The explicit value is announced in the report — the report is the
        // only other thing this argv may touch.
        assert_eq!(
            moved(&d, "config_report").1,
            "exit.force_exit_sec=120 (cli)"
        );
    }

    #[test]
    fn snapshot_env_layer_is_visible_and_cli_outranks_it() {
        let env = env_of(&[("BK_ROUND_SEC", "300"), ("BK_SIZE_PCT", "7.5")]);
        let base = snapshot(&parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &["--no-config".to_string()],
            &EnvVars::default(),
        ));
        // env supplies: the value lines AND the report's (env) lines move, and
        // nothing else does.
        let a = parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &["--no-config".to_string()],
            &env,
        );
        let d = diff_keys(&base, &snapshot(&a));
        assert_eq!(
            changed_keys(&d),
            ["size_pct", "sizing_echo", "round_sec", "config_report"],
            "the env layer must move ONLY the keys it owns"
        );
        assert_eq!(moved(&d, "round_sec"), ("900", "300"));
        assert_eq!(moved(&d, "size_pct"), ("0", "7.5"));
        assert_eq!(
            moved(&d, "config_report").1,
            "engine.round_sec=300 (env); engine.size_pct=7.5 (env)"
        );
        let echo = moved(&d, "sizing_echo").1;
        assert!(
            echo.starts_with(
                "order sizing: per-entry budget 7.5% of cash equity (env; replaces size_usd)"
            ),
            "{echo}"
        );

        // CLI outranks env: the value flips to (cli) and the report names the
        // winner per knob — round_sec from cli, size_pct still from env.
        let c = parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &[
                "--no-config".to_string(),
                "--round-sec".to_string(),
                "60".to_string(),
            ],
            &env,
        );
        let d = diff_keys(&base, &snapshot(&c));
        assert_eq!(
            changed_keys(&d),
            ["size_pct", "sizing_echo", "round_sec", "config_report"]
        );
        assert_eq!(moved(&d, "round_sec"), ("900", "60"));
        assert_eq!(moved(&d, "size_pct"), ("0", "7.5"));
        assert_eq!(
            moved(&d, "config_report").1,
            "engine.round_sec=60 (cli); engine.size_pct=7.5 (env)"
        );
        // The sizing echo follows size_pct (env), not the CLI knob.
        assert!(
            moved(&d, "sizing_echo")
                .1
                .starts_with("order sizing: per-entry budget 7.5% of cash equity (env;"),
            "the sizing echo must track size_pct from env: {:?}",
            moved(&d, "sizing_echo")
        );
    }

    #[test]
    fn snapshot_toml_layer_is_visible_and_cli_outranks_it() {
        let file = file_with(
            r#"
            [engine]
            round_sec = 300
            assets = ["DOGE"]
            "#,
        );
        let base = snapshot(&parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &[],
            &EnvVars::default(),
        ));
        let a = parse_args(&file, &[], &EnvVars::default());
        let d = diff_keys(&base, &snapshot(&a));
        assert_eq!(
            changed_keys(&d),
            ["round_sec", "assets", "config_report"],
            "the TOML layer must move ONLY the keys it owns"
        );
        assert_eq!(moved(&d, "round_sec"), ("900", "300"));
        assert_eq!(
            moved(&d, "assets"),
            ("BTC; ETH; SOL; XRP", "DOGE"),
            "the whole vector diffs as one line, not index churn"
        );
        assert_eq!(
            moved(&d, "config_report").1,
            "engine.round_sec=300 (toml); engine.assets=[\"DOGE\"] (toml)"
        );

        // CLI outranks TOML; the untouched TOML key (assets) stays.
        let c = parse_args(
            &file,
            &["--round-sec".to_string(), "60".to_string()],
            &EnvVars::default(),
        );
        let d = diff_keys(&base, &snapshot(&c));
        assert_eq!(
            changed_keys(&d),
            ["round_sec", "assets", "config_report"],
            "the CLI win must not disturb the TOML-owned key's footprint"
        );
        assert_eq!(moved(&d, "round_sec"), ("900", "60"));
        assert_eq!(
            moved(&d, "config_report").1,
            "engine.round_sec=60 (cli); engine.assets=[\"DOGE\"] (toml)"
        );
    }

    #[test]
    fn snapshot_echo_wording_tracks_the_value_across_layers() {
        // Each echo has a value-dependent branch; this pins the branch, not just
        // the number, for each layer that can move the value.
        // 1) the daily-loss breaker arms itself:
        let a = args_from(&["--max-daily-loss", "5"]);
        let got = snapshot(&a);
        assert!(
            got.contains("daily_loss_echo[0]=daily loss breaker: absolute cap $5 (cli)"),
            "{got}"
        );
        // 2) the per-order bound names the equity-relative cap:
        let a = args_from(&["--max-order-notional-pct", "20"]);
        let got = snapshot(&a);
        assert!(
            got.contains(
                "per-order bounds in force: absolute cap $100, plus 20% of the account's cash equity"
            ),
            "{got}"
        );
        assert!(
            !got.contains("NO equity-relative cap"),
            "an armed relative cap must retire the NO-cap wording"
        );
        // 3) disabling the freshness check flips the echo's branch:
        let a = args_from(&["--max-orderbook-stale-ms", "0"]);
        let got = snapshot(&a);
        assert!(
            got.contains("orderbook_stale_echo[0]=orderbook freshness: CHECK DISABLED (cli;"),
            "{got}"
        );
        assert!(got.contains("max_orderbook_stale_ms=0"), "{got}");
        // 4) the shadow-evolution evaluator echoes ON when the file arms it:
        let file = file_with("[shadow_evolution]\nenabled = true\n");
        let a = parse_args(&file, &[], &EnvVars::default());
        let got = snapshot(&a);
        assert!(
            got.contains(
                "shadow_evolution_echo[0]=shadow evolution: evaluator ON at startup (toml)"
            ),
            "{got}"
        );
        assert!(got.contains("shadow_evolution=true"), "{got}");
    }

    #[test]
    fn snapshot_backtest_knobs_render_indexed_and_ordered() {
        // The knob list renders in argv order; the indexed form makes a
        // reorder or a rename a named diff instead of a silent swap.
        let a = args_from(&[
            "--backtest-knob",
            "spread_arb:exit_take_profit_pct=30",
            "--backtest-knob",
            "mad_dog:entry_dip_max_pct=9",
        ]);
        let got = snapshot(&a);
        assert!(
            got.contains("backtest_knobs[0]=spread_arb:exit_take_profit_pct=30"),
            "{got}"
        );
        assert!(
            got.contains("backtest_knobs[1]=mad_dog:entry_dip_max_pct=9"),
            "{got}"
        );
        assert!(!got.contains("backtest_knobs[2]"), "{got}");
    }
}
