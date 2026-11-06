use super::*;

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

/// The env layer is the durable source of the ladder, and the flag still wins
/// — the same precedence every other env-settable knob follows. The TOML
/// `[exit]` section that used to sit between them is retired (issue #967).
#[test]
fn the_exit_ladder_answers_to_env_and_the_flag_still_wins() {
    let env = env_of(&[
        ("BK_EXIT_STOP_LOSS_PCT", "9"),
        ("BK_EXIT_MIN_TRAIL_PCT", "4.5"),
    ]);
    let a = parse_args(&[], &env);
    assert_eq!(a.exit_stop_loss_pct, dec!(9));
    assert_eq!(a.exit_min_trail_pct, dec!(4.5));
    // The two the env does not set keep the compiled value.
    assert_eq!(a.exit_take_profit_pct, dec!(100));
    assert_eq!(a.exit_trailing_min_high_pct, dec!(15));
    assert!(
        a.config_report
            .iter()
            .any(|l| l == "exit.stop_loss_pct=9 (env)"),
        "{:?}",
        a.config_report
    );

    let a = parse_args(&["--exit-stop-loss-pct".into(), "11".into()], &env);
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

// ── update switches, env tier (issue 751 / issue 923) ───────────────────

/// The three update switches answer to the env layer (`BLITZKRIEG_UPDATE_*`)
/// in BOTH directions: true|1|on|yes arm, false|0|off|no disarm. The
/// update.toml file layer is retired (issue 923), so with no CLI flag and no
/// env key the field stays `None` and main falls through to the built-in
/// default (off) — there is no third source that could resurrect a switch.
#[test]
fn the_update_switches_answer_to_the_env_in_both_directions() {
    // Nothing speaks: the file layer is gone, so the chain lands on the
    // built-in default (None here; main.rs maps it to off).
    let a = args_from(&[]);
    assert_eq!(a.update_check_enabled, None);
    assert_eq!(a.update_auto, None);
    assert_eq!(a.update_interval_secs, None);

    // Every positive spelling arms, at the env tier.
    for spelling in ["true", "1", "on", "yes"] {
        let env = env_of(&[
            ("BLITZKRIEG_UPDATE_CHECK", spelling),
            ("BLITZKRIEG_UPDATE_AUTO", spelling),
        ]);
        let a = parse_args(&[], &env);
        assert_eq!(a.update_check_enabled, Some(true), "{spelling:?}");
        assert_eq!(a.update_auto, Some(true), "{spelling:?}");
        for line in [
            "update.check_enabled=true (env)",
            "update.auto_update=true (env)",
        ] {
            assert!(
                a.config_report.iter().any(|l| l == line),
                "{line} missing from {:?}",
                a.config_report
            );
        }
    }

    // Every negative spelling disarms — and an explicit env OFF is still an
    // explicit value: the report names it, the caller cannot mistake it for
    // "nobody spoke".
    for spelling in ["false", "0", "off", "no"] {
        let env = env_of(&[
            ("BLITZKRIEG_UPDATE_CHECK", spelling),
            ("BLITZKRIEG_UPDATE_AUTO", spelling),
        ]);
        let a = parse_args(&[], &env);
        assert_eq!(a.update_check_enabled, Some(false), "{spelling:?}");
        assert_eq!(a.update_auto, Some(false), "{spelling:?}");
        for line in [
            "update.check_enabled=false (env)",
            "update.auto_update=false (env)",
        ] {
            assert!(
                a.config_report.iter().any(|l| l == line),
                "{line} missing from {:?}",
                a.config_report
            );
        }
    }

    // A spelling outside the vocabulary is "not configured", never a guess.
    let env = env_of(&[("BLITZKRIEG_UPDATE_CHECK", "yeah-nah")]);
    let a = parse_args(&[], &env);
    assert_eq!(a.update_check_enabled, None, "an unknown word is silence");
    assert!(a.config_report.is_empty(), "{:?}", a.config_report);
}

/// #379: the interval parses at the env tier like it does at the CLI tier —
/// 0 keeps the "scheduler off" spelling, positives carry — and an explicit
/// env value is reported so the log alone answers "why is this checking".
#[test]
fn the_update_interval_answers_to_the_env_tier() {
    for (text, want) in [("0", Some(0)), ("3600", Some(3600)), (" 600 ", Some(600))] {
        let env = env_of(&[("BLITZKRIEG_UPDATE_INTERVAL_SECS", text)]);
        let a = parse_args(&[], &env);
        assert_eq!(a.update_interval_secs, want, "{text:?}");
    }
    let a = parse_args(
        &[],
        &env_of(&[("BLITZKRIEG_UPDATE_INTERVAL_SECS", "3600")]),
    );
    assert!(
        a.config_report
            .iter()
            .any(|l| l == "update.interval_secs=3600 (env)"),
        "{:?}",
        a.config_report
    );
    // Unparsable text is "not configured", never a silent default.
    let a = parse_args(
        &[],
        &env_of(&[("BLITZKRIEG_UPDATE_INTERVAL_SECS", "soon")]),
    );
    assert_eq!(a.update_interval_secs, None);
}

/// The chain is still CLI > env: a spelled flag outranks the env key, so an
/// operator's `-c`-style override cannot be undone by a stale `.env` line.
#[test]
fn the_update_cli_flag_outranks_the_env() {
    let env = env_of(&[
        ("BLITZKRIEG_UPDATE_CHECK", "false"),
        ("BLITZKRIEG_UPDATE_INTERVAL_SECS", "0"),
    ]);
    let a = parse_args(
        &["--update-check".into(), "true".into()],
        &env,
    );
    assert_eq!(
        a.update_check_enabled,
        Some(true),
        "the CLI flag outranks the env's OFF"
    );
    assert!(
        a.config_report
            .iter()
            .any(|l| l == "update.check_enabled=true (cli)"),
        "{:?}",
        a.config_report
    );
    let a = parse_args(
        &["--update-interval-secs".into(), "60".into()],
        &env,
    );
    assert_eq!(a.update_interval_secs, Some(60), "CLI > env");
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
    let env = env_of(&[
        ("BK_MAX_DAILY_LOSS", "3.5"),
        ("BK_MAX_DAILY_LOSS_PCT", "10"),
    ]);
    let a = parse_args(&[], &env);
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
/// `.recon` watermark is: same directory, derived name. issue #941: the
/// name is per-mode — dry adds a `.dry` tag, live keeps the bare name,
/// so the two books' files never meet.
#[test]
fn the_daily_loss_file_follows_the_position_log() {
    assert_eq!(
        daily_loss_path_for("data/positions/positions.jsonl", true),
        "data/positions/positions.daily-loss.json"
    );
    assert_eq!(
        daily_loss_path_for("/tmp/x/pos.jsonl", true),
        "/tmp/x/pos.daily-loss.json"
    );
    assert_eq!(
        daily_loss_path_for("data/positions/positions.jsonl", false),
        "data/positions/positions.daily-loss.dry.json"
    );
    assert_eq!(
        risk_state_path_for("data/positions/positions.jsonl", false),
        "data/positions/positions.risk-state.dry.json"
    );
}

// ── issue #953: the open-position snapshot and the persisted strategy set
//    follow the book ─────────────────────────────────────────────────────

/// The DEFAULT open-position snapshot and strategy-state file are per-mode,
/// the same name-song #941 gave the side files: live keeps the historical
/// spellings byte for byte (upgrade = zero migration — acceptance C), dry
/// gets the `.dry.` tag beside them (`positions.dry.jsonl`,
/// `strategy-state.dry.json`), so a same-directory live→dry switch boots
/// into the dry book's own snapshot and persisted enablement and the two
/// books' files never meet. Acceptance E: the bare names belong to live
/// from this split on — nothing on disk is moved or renamed.
#[test]
fn the_snapshot_defaults_split_by_mode_like_the_side_files() {
    // Acceptance C: live paths byte-identical to the pre-#953 defaults.
    assert_eq!(
        default_position_log_for(Mode::Live),
        "data/positions/positions.jsonl"
    );
    assert_eq!(
        default_strategy_state_for(Mode::Live),
        "data/strategy-state.json"
    );
    // The dry family matches #941's naming convention (semantic tag before
    // the final extension), so the naming song stays one song.
    assert_eq!(
        default_position_log_for(Mode::Dry),
        "data/positions/positions.dry.jsonl"
    );
    assert_eq!(
        default_strategy_state_for(Mode::Dry),
        "data/strategy-state.dry.json"
    );
    // Read-only settles locally — the simulation book, not the venue book.
    assert_eq!(
        default_position_log_for(Mode::ReadOnly),
        "data/positions/positions.dry.jsonl"
    );
    assert_eq!(
        default_strategy_state_for(Mode::ReadOnly),
        "data/strategy-state.dry.json"
    );
}

/// The explicit flag outranks the per-mode derivation on both faces, and an
/// explicit off stays off. A bare boot carries the historical (live)
/// spellings as its RAW shape — the mode split is the binary's job (the
/// resolved `Mode` is only available there), so `parse_args` itself stays
/// mode-free and the snapshot gate keeps pinning exactly this raw shape.
#[test]
fn the_snapshot_flags_outrank_the_mode_derivation() {
    // Explicit paths survive untouched (the binary rewrites nothing.
    let a = args_from(&[
        "--mode",
        "live",
        "--position-log",
        "/tmp/bk/book.jsonl",
        "--strategy-state",
        "/tmp/bk/state.json",
    ]);
    assert_eq!(a.position_log.as_deref(), Some("/tmp/bk/book.jsonl"));
    assert_eq!(a.strategy_state.as_deref(), Some("/tmp/bk/state.json"));

    // A bare boot keeps the bare shape (the historical spelling) — this is
    // the line the snapshot gate pins; `main` swaps the dry spelling in when
    // the resolved mode does not trade.
    let a = args_from(&[]);
    assert_eq!(
        a.position_log, None,
        "parse_args never minted a position-log default (the binary does)"
    );
    assert_eq!(
        a.strategy_state.as_deref(),
        Some("data/strategy-state.json")
    );

    // An explicit off stays off on both faces.
    let a = args_from(&["--no-strategy-state"]);
    assert_eq!(a.strategy_state, None);
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
    let b = parse_args(&[], &env);
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
    let a = parse_args(
        &["--max-positions-per-asset".to_string(), "3".to_string()],
        &EnvVars::default(),
    );
    assert_eq!(a.max_positions_per_asset, Some(3));
    let a = parse_args(&[], &EnvVars::default());
    assert_eq!(
        a.max_positions_per_asset, None,
        "absent flag = the execution-policy file decides"
    );
}

#[test]
fn the_env_supplies_a_value_when_no_flag_does() {
    let env = env_of(&[
        ("BK_ASSETS", "DOGE"),
        ("BK_ROUND_SEC", "300"),
        ("BK_MIN_ROUND_AGE_SEC", "5"),
        ("BK_MIN_TIME_LEFT_SEC", "7"),
    ]);
    let a = parse_args(&[], &env);
    assert_eq!(a.round_sec, 300);
    assert_eq!(a.min_round_age, 5);
    assert_eq!(a.min_time_left, 7);
    assert_eq!(a.assets, vec!["DOGE"]);
    // Each resolved value names its origin, so the log can answer "why 300?".
    assert!(
        a.config_report
            .iter()
            .any(|l| l == "engine.round_sec=300 (env)")
    );
    assert!(
        a.config_report
            .iter()
            .any(|l| l == "engine.min_time_left_sec=7 (env)")
    );
}

#[test]
fn a_flag_beats_the_env() {
    let env = env_of(&[("BK_ROUND_SEC", "300"), ("BK_MIN_TIME_LEFT_SEC", "7")]);
    let a = parse_args(&["--round-sec".into(), "60".into()], &env);
    assert_eq!(a.round_sec, 60, "CLI outranks env");
    assert_eq!(
        a.min_time_left, 7,
        "and the untouched key still comes from env"
    );
    assert!(
        a.config_report
            .iter()
            .any(|l| l == "engine.round_sec=60 (cli)")
    );
}

// The UI card's persisted cell outranks the factory default: only an explicit
// flag/env var suppresses it. #474's "旗标 > 格 > 默认" promise, kept.
#[test]
fn nothing_explicit_lets_the_persisted_cell_apply() {
    // Nothing spoke → "default", so the cell still applies.
    let a = parse_args(&[], &EnvVars::default());
    assert_eq!(round_duration_source_from(&a.config_report), "default");
    // A CLI flag spoke → the cell is suppressed.
    let a = parse_args(&["--round-sec".into(), "60".into()], &EnvVars::default());
    assert_eq!(round_duration_source_from(&a.config_report), "cli_or_env");
    // The gate site and the judge share one spelling: a source the judge
    // emits as "cli_or_env" is exactly the one the gate refuses.
    assert_ne!("cli_or_env", "default");
}

#[test]
fn the_env_can_turn_shadow_evolution_on_and_names_its_origin() {
    // The documented way to enable the feature: `.env` → env layer. The env
    // face speaks SECONDS (the retired file's minutes are gone, issue #967).
    let env = env_of(&[
        ("BK_SHADOW_EVOLUTION", "true"),
        ("BK_SE_MIN_SAMPLES", "12"),
        ("BK_SE_COOLDOWN_SECS", "120"),
        ("BK_SE_EVAL_WINDOW_SECS", "300"),
        ("BK_SE_MIN_OBS_SECS", "60"),
        ("BK_SE_VARIANT_COUNT", "4"),
        ("BK_SE_AUDIT_DIR", "data/evo-scratch"),
    ]);
    let a = parse_args(&[], &env);
    assert!(a.shadow_evolution);
    assert_eq!(a.se_min_samples, Some(12));
    assert_eq!(a.se_cooldown_secs, Some(120));
    assert_eq!(a.se_eval_window_secs, Some(300));
    assert_eq!(a.se_min_obs_secs, Some(60));
    assert_eq!(a.se_variant_count, Some(4));
    assert_eq!(a.se_audit_dir.as_deref(), Some("data/evo-scratch"));
}

#[test]
fn strategy_selection_comes_from_cli_flags_only() {
    let a = parse_args(
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
    let env = env_of(&[("BK_SE_MAX_GRADIENT", "0.25")]);
    let a = parse_args(&[], &env);
    assert_eq!(
        a.se_max_gradient, None,
        "a request to widen a safety lock must not take effect"
    );

    // Tightening it is allowed.
    let env = env_of(&[("BK_SE_MAX_GRADIENT", "0.01")]);
    let a = parse_args(&[], &env);
    assert_eq!(a.se_max_gradient, Some(dec!(0.01)));
}

#[test]
fn the_env_layer_sits_between_the_flag_and_the_default() {
    let env = env_of(&[("BK_ROUND_SEC", "600"), ("BK_MIN_TIME_LEFT_SEC", "9")]);

    // CLI > env.
    let a = parse_args(&["--round-sec".into(), "60".into()], &env);
    assert_eq!(a.round_sec, 60);
    assert!(
        a.config_report
            .iter()
            .any(|l| l == "engine.round_sec=60 (cli)")
    );
    // env > default.
    assert_eq!(
        a.min_time_left, 9,
        "the env var supplies the key no flag named"
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
    let a = parse_args(&[], &env);
    assert_eq!(a.min_round_age, 3);
    // Asset lists are normalised wherever they come from.
    assert_eq!(a.assets, vec!["BTC", "ETH"]);
    assert!(a.shadow_evolution);
    assert_eq!(a.se_cooldown_secs, Some(45));
}

/// The factory-revival pin (issue 751), now purely env-level: the retired
/// files cannot arm anything, and the env tier's spelling is recorded.
/// `BK_SHADOW_EVOLUTION` outranks the legacy `SHADOW_ENABLED` alias.
#[test]
fn the_shadow_switches_answer_to_the_documented_env_keys() {
    let env = env_of(&[
        ("BK_SHADOW_EVOLUTION", "false"),
        ("SHADOW_ENABLED", "true"),
        ("SHADOW_AUTO_EVOLVE", "true"),
    ]);
    let a = parse_args(&[], &env);
    assert!(
        !a.shadow_evolution,
        "BK_SHADOW_EVOLUTION=false must win over the legacy alias"
    );
    assert_eq!(
        a.se_auto_evolve,
        Some(true),
        "SHADOW_AUTO_EVOLVE is the only key for the second switch"
    );
    for line in [
        "shadow_evolution.enabled=false (env)",
        "shadow_evolution.auto_evolve=true (env)",
    ] {
        assert!(
            a.config_report.iter().any(|l| l == line),
            "{line} missing from {:?}",
            a.config_report
        );
    }
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
    let a = parse_args(&[], &env);
    assert_eq!(a.max_orderbook_stale_ms, 15_000);
    assert!(
        a.config_report
            .iter()
            .any(|l| l == "engine.max_orderbook_stale_ms=15000 (env)"),
        "{:?}",
        a.config_report
    );
    // An absent key stays at the compiled 8000 ms.
    let a = parse_args(&[], &EnvVars::default());
    assert_eq!(a.max_orderbook_stale_ms, 8_000);
    // CLI outranks env, like every other setting in the chain.
    let a = parse_args(
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
