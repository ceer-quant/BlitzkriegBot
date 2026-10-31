use super::*;

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
        sizing.contains("absolute size_usd (default") && sizing.contains("NO equity-relative cap")
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
        got.contains("shadow_evolution_echo[0]=shadow evolution: evaluator ON at startup (toml)"),
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
