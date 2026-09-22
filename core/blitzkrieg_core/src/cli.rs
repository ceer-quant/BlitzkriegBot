//! The core binary's command line, as one table (#228).
//!
//! Why this is a module with a table instead of a bare `match`: until #228 the
//! parser's fallback arm printed `ignoring unknown argument: <flag>` and then
//! STARTED THE CORE ANYWAY. For a binary whose arguments carry safety semantics
//! that is the wrong default in one specific and expensive way — an operator who
//! types `--redonly` (or `--read-only`, or `--max-notinal 50`) does not get an
//! error, they get a core running in WRITE mode under the shipped caps, because
//! the flag they believed they set is simply not in force. The same fallback is
//! why `--version` against a stale binary started a trading process: twice,
//! during the #228 audit.
//!
//! So the accepted set lives here, once, and three readers share it:
//!
//!   * `--help` prints it — every flag with its value form and its meaning, so
//!     an operator can check a spelling instead of guessing at one;
//!   * the parser's fallback arm asks [`classify_unknown_arg`] and refuses to
//!     boot (exit 2) on anything it does not recognise, naming the closest
//!     known flag when there is one;
//!   * a test below reads `main.rs` with `include_str!` and asserts that the
//!     literal flags its `match` accepts are EXACTLY the non-early flags in
//!     [`FLAGS`] — so a flag added to the parser without a line here fails the
//!     tests instead of quietly missing from `--help`.
//!
//! `--allow-unknown-args` is the explicit opt-out for a caller that must pass a
//! flag this build predates. It does NOT cover a near-miss of a safety flag: the
//! difference between `--readonly` and `--redonly` is the difference between
//! observing and trading, and no escape hatch should be able to make that
//! ambiguous.

/// One accepted flag, as `--help` prints it.
pub struct FlagSpec {
    /// Exact spelling, `--` included. This is the string the parser matches.
    pub flag: &'static str,
    /// Value form for `--help` (`"<path>"`, `"<n>"`, …); empty for a switch.
    pub value: &'static str,
    /// One sentence: what the flag means, and its default when it has one.
    pub help: &'static str,
    /// What a MISPRONOUNCED spelling of this flag silently costs the run. Only
    /// safety-relevant flags have one; the text is shown when an unknown
    /// argument is a near-miss of this flag, because "unknown argument" alone
    /// does not tell an operator that the command they just typed would have
    /// started a materially less guarded core.
    pub risk: Option<&'static str>,
    /// Answered in `main` before `parse_args` (no config, no socket, no boot).
    /// These are `--help`/`--version`; the parser never sees them.
    pub early: bool,
    /// Short spelling, if any (`-h` for `--help`).
    pub alias: Option<&'static str>,
}

/// The one flag that turns an unrecognised argument back into a warning (#228).
/// Read by `main`'s parser as a pre-scan, and listed in [`FLAGS`] so `--help`
/// states the opt-out and its limit (see [`classify_unknown_arg`]).
pub const ALLOW_UNKNOWN_ARGS: &str = "--allow-unknown-args";

/// A switch that changes a limit or a mode the operator believes is in force.
const fn risky(
    flag: &'static str,
    value: &'static str,
    help: &'static str,
    risk: &'static str,
) -> FlagSpec {
    FlagSpec {
        flag,
        value,
        help,
        risk: Some(risk),
        early: false,
        alias: None,
    }
}

/// A plain flag.
const fn flag(flag: &'static str, value: &'static str, help: &'static str) -> FlagSpec {
    FlagSpec {
        flag,
        value,
        help,
        risk: None,
        early: false,
        alias: None,
    }
}

/// A flag answered before the parser runs, with its short spelling.
const fn early(flag: &'static str, alias: &'static str, help: &'static str) -> FlagSpec {
    FlagSpec {
        flag,
        value: "",
        help,
        risk: None,
        early: true,
        alias: Some(alias),
    }
}

/// Every argument the core accepts. Order is the order `--help` prints.
pub const FLAGS: &[FlagSpec] = &[
    early(
        "--help",
        "-h",
        "Print this list and exit 0 — no config, no socket, no boot.",
    ),
    early(
        "--version",
        "-V",
        "Print the built revision (`<semver>+g<sha>`) and exit 0.",
    ),
    flag(
        ALLOW_UNKNOWN_ARGS,
        "",
        "Start even when an argument is not recognised, ignoring it (loudly). \
         Never covers a near-miss of a safety flag; see the note above the list.",
    ),
    flag(
        "--socket",
        "<path>",
        "Unix socket to serve the JSON-RPC core on (default `$TMPDIR/blitzkrieg-core-$USER.sock`).",
    ),
    risky(
        "--mode",
        "<dry|live>",
        "`dry` simulates fills locally, `live` routes orders to the venue (default `dry`).",
        "the core would run DRY: a live request silently becomes a simulation, and \
         every order it reports is a local fill",
    ),
    risky(
        "--readonly",
        "",
        "Never send an order: the venue egress is not constructed at all. Outranks \
         `--mode live` and `DRY_RUN=false`.",
        "the core is in WRITE mode by default, so an order it decides to place is \
         placed for real once the mode is live",
    ),
    flag("--tick-ms", "<n>", "Engine tick period in ms (default 50)."),
    flag(
        "--seed-balance",
        "<usd>",
        "Dry-mode starting cash in USD (default 10000).",
    ),
    flag(
        "--engine",
        "",
        "Run the self-driving engine loop, not just serve RPC commands.",
    ),
    flag(
        "--feed-ws",
        "",
        "Pull market data natively instead of expecting the client to feed it.",
    ),
    flag(
        "--no-discovery",
        "",
        "Do not discover markets at startup (the client feeds them).",
    ),
    flag(
        "--market-plugin",
        "<name>",
        "Market extension to load (default: the shipped one).",
    ),
    flag(
        "--allow-shared-data",
        "",
        "Acknowledge a second core writing the same data directory; without it the \
         #199 latch refuses the boot (exit 1). For fixtures only.",
    ),
    flag(
        "--max-orderbook-stale-ms",
        "<n>",
        "Refuse to price off an orderbook older than this (ms); 0 disables the check. \
         A negative value, a non-number or anything past 600000 is a startup error.",
    ),
    flag(
        "--entry-maker-timeout-ms",
        "<n>",
        "How long a maker entry may rest before escalation to taker (default 5000).",
    ),
    risky(
        "--max-order-notional",
        "<usd>",
        "Absolute cap on ONE order's notional in USD (default 100).",
        "the cap is the compiled default (100 USD), not the number you meant to set",
    ),
    risky(
        "--max-open-notional-usd",
        "<usd>",
        "Cap on total open notional in USD (default 0 = no cap).",
        "total open notional is UNCAPPED (the default is 0), which is the opposite \
         of what a dropped limit flag is usually believed to mean",
    ),
    risky(
        "--size-pct",
        "<pct>",
        "Size each entry as this percentage of account equity (default 0 = off, the \
         absolute lot path).",
        "entries size from the absolute share lot instead of a share of the account",
    ),
    risky(
        "--max-order-notional-pct",
        "<pct>",
        "Cap one order at this percentage of account equity (default 0 = off).",
        "one order is bounded only by the absolute cap — with the shipped 0 the \
         relative bound is OFF",
    ),
    risky(
        "--min-shares",
        "<n>",
        "Lower bound of the per-entry share lot (default 10).",
        "the lot floor is the compiled default (10 shares)",
    ),
    risky(
        "--max-shares",
        "<n>",
        "Upper bound of the per-entry share lot (default 10).",
        "the lot ceiling is the compiled default (10 shares)",
    ),
    risky(
        "--max-positions",
        "<n>",
        "How many positions may be open at once (default 2).",
        "the position count is the compiled default (2)",
    ),
    risky(
        "--no-auto-exits",
        "",
        "Disable the kernel's automatic exit management (stops, targets).",
        "the kernel manages exits itself (stops and targets fire on their own)",
    ),
    risky(
        "--max-daily-loss",
        "<usd>",
        "Stop for the day after this much realized loss (0 = no budget).",
        "the daily breaker's absolute budget is its default (0 = no budget at all)",
    ),
    risky(
        "--max-daily-loss-pct",
        "<pct>",
        "Daily realized-loss budget as a percentage of the day's opening equity \
         (0 = off).",
        "no percentage budget arms the daily breaker (default 0 = off); the effective \
         cap is the tighter of the two budgets, so dropping this one loosens it",
    ),
    flag(
        "--assets",
        "<BTC,ETH>",
        "Comma-separated assets to trade (default: the built-in list).",
    ),
    flag(
        "--market",
        "<slug>",
        "Repeatable: pin one specific market to consider.",
    ),
    flag(
        "--round-sec",
        "<n>",
        "Round length in seconds (default: discovered from the venue).",
    ),
    flag(
        "--min-round-age",
        "<n>",
        "Do not enter a market younger than this many seconds.",
    ),
    flag(
        "--min-time-left",
        "<n>",
        "Do not enter a market with less than this many seconds left.",
    ),
    flag(
        "--trend-confirm-sec",
        "<n>",
        "Trend confirmation window in seconds (default 60).",
    ),
    flag(
        "--trend-window-floor-ms",
        "<n>",
        "Floor for the trend window in ms (default 10000).",
    ),
    flag(
        "--spread-arb-entry-factor",
        "<x>",
        "Spread-arb entry factor (default: the strategy's own).",
    ),
    flag(
        "--spread-arb-min-obi",
        "<x>",
        "Spread-arb minimum orderbook imbalance to enter (default: the strategy's own).",
    ),
    flag(
        "--spread-arb-max-spread-pct",
        "<x>",
        "Spread-arb maximum spread to enter, in percent (default: the strategy's own).",
    ),
    flag(
        "--spread-arb-dip-max-pct",
        "<x>",
        "Spread-arb dip ceiling, in percent (default: the strategy's own).",
    ),
    flag(
        "--spread-arb-bounce-min-pct",
        "<x>",
        "Spread-arb minimum bounce, in percent (default: the strategy's own).",
    ),
    flag(
        "--spread-arb-bounce-window-sec",
        "<n>",
        "Spread-arb bounce measurement window, seconds (default: the strategy's own).",
    ),
    flag(
        "--strategy-limit",
        "<name:max_open:max_notional_usd>",
        "Repeatable: per-strategy open-position and notional caps (`-` = no cap on \
         that segment).",
    ),
    flag(
        "--enable-strategy",
        "<name>",
        "Repeatable: switch one strategy ON at startup (an unknown name is a warning, \
         not fatal).",
    ),
    flag(
        "--disable-strategy",
        "<name>",
        "Repeatable: switch one strategy OFF at startup; applied after \
         --enable-strategy, so an explicit off wins.",
    ),
    flag(
        "--strategy-state",
        "<path|none>",
        "Where the enabled-strategy set is persisted (default \
         `data/strategy-state.json`; `none` = do not persist).",
    ),
    flag(
        "--no-strategy-state",
        "",
        "Do not persist the enabled-strategy set.",
    ),
    flag(
        "--strategy-dir",
        "<path|none>",
        "Directory scanned for external strategy cdylibs, every library found is \
         loaded (default `user_layer/strategies`; `none` = load none).",
    ),
    flag("--no-strategy-dir", "", "Load no external strategies."),
    flag(
        "--shadow-evolution",
        "",
        "Run the shadow-evolution loop alongside trading.",
    ),
    flag(
        "--se-min-samples",
        "<n>",
        "Shadow evolution: minimum samples before a variant may be judged.",
    ),
    flag(
        "--se-cooldown-secs",
        "<n>",
        "Shadow evolution: cooldown between evolution cycles, seconds.",
    ),
    flag(
        "--se-min-obs-secs",
        "<n>",
        "Shadow evolution: minimum observation time, seconds.",
    ),
    flag(
        "--se-auto-evolve",
        "<true|false>",
        "Shadow evolution: E13 proposal workflow on/off.",
    ),
    flag(
        "--se-cycle-secs",
        "<n>",
        "Shadow evolution: E13 cycle period, seconds.",
    ),
    flag(
        "--se-ttl-secs",
        "<n>",
        "Shadow evolution: E13 proposal lifetime, seconds.",
    ),
    flag(
        "--se-deep-dims",
        "<n>",
        "Shadow evolution: deep-search dimensions.",
    ),
    flag(
        "--replay",
        "<archive.jsonl>",
        "Replay a captured event archive through the core (offline).",
    ),
    flag(
        "--replay-near-miss",
        "<archive.jsonl>",
        "Replay an archive through the entry gate's near-miss path.",
    ),
    flag(
        "--near-miss-path",
        "<path>",
        "Where engine near-misses are recorded (default \
         `data/shadow/near-miss.jsonl` when --engine is on).",
    ),
    risky(
        "--trade-log",
        "<path>",
        "Trade ledger JSONL (default `data/trades/trades.jsonl`).",
        "trades go to the DEFAULT ledger path (`data/trades/trades.jsonl`), not the \
         file you named",
    ),
    risky(
        "--no-trade-log",
        "",
        "Do not persist trades at all.",
        "trades ARE persisted — to the default ledger path — which is the accounting \
         truth the risk limits are computed from",
    ),
    risky(
        "--order-log",
        "<path>",
        "Order ledger JSONL for crash recovery (default `data/orders/orders.jsonl`).",
        "orders go to the DEFAULT ledger path (`data/orders/orders.jsonl`)",
    ),
    risky(
        "--no-order-log",
        "",
        "Do not persist orders.",
        "orders ARE persisted — to the default ledger path",
    ),
    risky(
        "--position-log",
        "<path>",
        "Open-position snapshot (default `data/positions/positions.jsonl`).",
        "positions go to the DEFAULT path (`data/positions/positions.jsonl`)",
    ),
    risky(
        "--no-position-log",
        "",
        "Do not persist positions.",
        "positions ARE persisted — to the default path",
    ),
    flag(
        "--event-archive",
        "<path>",
        "Mirror every market-data event into this JSONL archive.",
    ),
    flag(
        "--no-event-archive",
        "",
        "Disable market-data capture (it is ON by default for an engine session).",
    ),
    flag(
        "--event-archive-max-mb",
        "<n>",
        "Stop recording at this archive size in MiB (0 = unlimited).",
    ),
    flag(
        "--event-archive-rotate-mb",
        "<n>",
        "Rotate into a new UTC-stamped segment every this many MiB (0 = never).",
    ),
    flag(
        "--event-archive-min-free-mb",
        "<n>",
        "Stop recording below this free-space floor in MiB (0 = no guard).",
    ),
    flag(
        "--dry-redeem-fail",
        "<n>",
        "Dry-mode test hook: the first N simulated redemptions of each claim fail \
         (0 = production behaviour).",
    ),
    flag(
        "--dry-redeem-manual",
        "",
        "Dry-mode test hook: report those injected failures as `manual`, stopping \
         the automatic retries.",
    ),
    flag(
        "--slippage-ticks",
        "<n>",
        "Fill model: taker slippage in ticks (default 0).",
    ),
    flag(
        "--latency-ms",
        "<n>",
        "Fill model: maker latency in ms (default 0).",
    ),
    flag(
        "--fill-prob-bps",
        "<n>",
        "Fill model: maker fill probability in bps of 10000 (default: untouched).",
    ),
    flag(
        "--maker-depth-share-bps",
        "<n>",
        "Fill model: share of crossing depth a resting maker takes, bps of 10000 \
         (default: untouched).",
    ),
    flag(
        "--backtest",
        "<archive.jsonl>",
        "Offline replay of an archive with a report (forces dry, starts no feed and \
         writes no log).",
    ),
    flag(
        "--backtest-report",
        "<path>",
        "Write the backtest report as JSON here (default: stdout only).",
    ),
    flag(
        "--backtest-tick-ms",
        "<n>",
        "Virtual-clock step between maintenance cycles in a replay (default 50).",
    ),
    flag(
        "--backtest-tail-ms",
        "<n>",
        "Keep the replay clock running this long after the last event (default 0).",
    ),
    flag(
        "--backtest-knob",
        "<strategy:knob=value>",
        "Repeatable: counterfactual knob override handed to the replayed strategy.",
    ),
    flag(
        "--fee-model",
        "<legacy_quadratic|official>",
        "Replay-only: which taker-fee schedule the replay charges (asking for it \
         without --backtest is refused).",
    ),
    flag(
        "--regime-eval",
        "<archive.jsonl>",
        "Label market regimes over an archive and score the online state machine \
         against them.",
    ),
    flag(
        "--regime-report",
        "<path>",
        "Write the regime report as `<path>.json` + `.md` (default: stdout).",
    ),
    flag(
        "--regime-token",
        "<token>",
        "Evaluate exactly this token instead of the most-active defaults.",
    ),
    flag(
        "--regime-max-tokens",
        "<n>",
        "How many of the most active tokens to evaluate (default 3).",
    ),
    flag(
        "--regime-window-sec",
        "<n>",
        "Regime window in seconds, shared by the label rule and the machine \
         (default 300).",
    ),
    flag(
        "--regime-min-trend-ticks",
        "<x>",
        "Trend bar: minimum net move in ticks (default 3).",
    ),
    flag(
        "--regime-min-efficiency",
        "<x>",
        "Trend bar: minimum |net| / path efficiency (default 0.5).",
    ),
    flag(
        "--regime-volatile-mad-ticks",
        "<x>",
        "Volatile bar: mean absolute per-step move, in ticks (default 1.5).",
    ),
    flag(
        "--regime-confirmations",
        "<n>",
        "Confirmation hysteresis before the regime machine switches state \
         (default 2).",
    ),
    flag(
        "--net-check",
        "",
        "Probe every network path this venue uses (resolver, TCP, TLS, one cheap \
         request each) and print ONE JSON report; exit 1 when a probe failed. Read-only: \
         no socket, no ledger, no order, no credential.",
    ),
    flag(
        "--config",
        "<path>",
        "TOML config file (default `user_layer/configs/default.toml`; the `=` form \
         `--config=<path>` is also accepted).",
    ),
    flag(
        "--no-config",
        "",
        "Ignore the TOML config file entirely (same as `BK_CONFIG=none`).",
    ),
];

/// The flag form a value-taking flag must be written as today: a separate
/// argument. Only `--config=<path>` accepts the `=` form, so `--mode=live` is an
/// unknown argument — and this is the sentence that tells an operator why.
const JOINED_VALUE_NOTE: &str = "note: a flag takes its value as a separate argument (`--mode live`); \
                                 only --config=<path> accepts the `=value` form";

/// The full `--help` text, stdout-ready.
pub fn help_text() -> String {
    let mut s = String::new();
    s.push_str(
        "blitzkrieg-core — the market-agnostic trading core, served over a Unix socket.\n\n",
    );
    s.push_str("usage: blitzkrieg-core [flag [value]]...\n\n");
    s.push_str(
        "An argument this build does not recognise STOPS the boot (exit 2) instead of\n\
         being ignored: a misspelled --readonly used to start a core in write mode, and a\n\
         misspelled --max-order-notional one under the shipped cap (#228). Every flag is\n\
         listed below with its exact spelling. --allow-unknown-args is the explicit\n\
         opt-out, and it still refuses a near-miss of a safety flag.\n\n",
    );
    s.push_str("flags:\n");
    for spec in FLAGS {
        match spec.alias {
            Some(a) => s.push_str(&format!("  {}, {a}{}\n", spec.flag, value_form(spec))),
            None => s.push_str(&format!("  {}{}\n", spec.flag, value_form(spec))),
        }
        s.push_str(&format!("      {}\n", spec.help));
    }
    s.push_str(
        "\nexit codes: 0 — --help/--version; 1 — boot refused (another core owns the data\n\
         directory, #199) or --net-check found a failing probe; 2 — bad value, unknown argument,\n\
         or an argument that is only valid with another mode (e.g. --fee-model without --backtest).\n",
    );
    s
}

fn value_form(spec: &FlagSpec) -> String {
    if spec.value.is_empty() {
        String::new()
    } else {
        format!(" {}", spec.value)
    }
}

/// Whether `token` is an argument the parser handles.
///
/// The parser's own `match` is the authority (the flag list above is checked
/// against it by a test); this is for callers that need the question answered
/// without running the parser. `--config=<path>` is the one joined-value form.
pub fn is_known_flag(token: &str) -> bool {
    token.starts_with("--config=") || FLAGS.iter().any(|f| f.flag == token)
}

/// The known flag `token` most likely meant, or `None` when nothing is close.
///
/// Three rules, because the mistakes operators actually make are of three kinds.
/// Each carries a rank and the candidates are compared by (rank, distance,
/// length difference): a stronger kind always beats a weaker one, a
/// distance-1 misspelling beats a distance-2 one, and between two equally good
/// candidates the one closest in length wins. Ties fall to the earlier table
/// entry, so the answer is deterministic — a wrong suggestion is worse than
/// none, and an unstable one is worse than both.
///
///   * rank 0 — a truncated flag: a longer known spelling starts with the token
///     (`--no-trade` for `--no-trade-log`, `--max-order-notional-p` for
///     `--max-order-notional-pct`). Only from 6 characters up, so `--max` is not
///     a truncation of anything.
///   * rank 1 — a misspelling: edit distance ≤ 2 on the dashes-stripped text
///     (`--redonly`, `--read-only`, `--no-auto-exit`).
///   * rank 2 — a dropped word or letters: the token is a SUBSEQUENCE of a known
///     flag with the same first three characters (`--max-notinal` for
///     `--max-order-notional`). A subsequence, not a bag of characters — a bag
///     would match `--no-trade` against `--no-strategy-dir`, which is a
///     different flag, and a suggestion that points at a different limit is its
///     own small hazard.
pub fn suggestion(token: &str) -> Option<&'static FlagSpec> {
    let bare = token.trim_start_matches('-');
    if bare.is_empty() {
        return None;
    }
    let bare_len = bare.chars().count();
    let first3: String = bare.chars().take(3).collect();
    let mut best: Option<((u32, u32, usize), &'static FlagSpec)> = None;
    for spec in FLAGS {
        let known = spec.flag.trim_start_matches('-');
        let known_first3: String = known.chars().take(3).collect();
        let distance = edit_distance(bare, known);
        let score = if bare_len >= 6 && known.len() > bare.len() && known.starts_with(bare) {
            (0, 0, known.len() - bare.len())
        } else if distance <= 2 {
            (1, distance, known.len().abs_diff(bare.len()))
        } else if bare_len >= 6 && first3 == known_first3 && is_subsequence(bare, known) {
            (2, 0, known.len().abs_diff(bare.len()))
        } else {
            continue;
        };
        if best.is_none_or(|(b, _)| score < b) {
            best = Some((score, spec));
        }
    }
    best.map(|(_, spec)| spec)
}

/// Levenshtein distance over characters, the plain O(n·m) table.
fn edit_distance(a: &str, b: &str) -> u32 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<u32> = (0..=b.len() as u32).collect();
    let mut cur = vec![0u32; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i as u32 + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = u32::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Whether `needle`'s characters occur in `haystack` in the same ORDER (the
/// definition of "a word was dropped out of the middle of this flag").
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut rest = haystack.chars();
    needle.chars().all(|c| rest.any(|h| h == c))
}

/// What to do with an argument the parser does not recognise.
pub enum UnknownArg {
    /// Refuse to start: the caller prints this on stderr and exits 2.
    Reject(String),
    /// `--allow-unknown-args` is in effect and the argument is not a near-miss
    /// of a safety flag: the caller warns and continues.
    Ignore(String),
}

/// Decide what an unrecognised argument means, and say it in the operator's
/// terms (#228).
///
/// The one rule that overrides everything else: a near-miss of a flag with a
/// [`FlagSpec::risk`] note is refused even under `--allow-unknown-args`. An
/// escape hatch exists so a caller can pass a flag a newer build added; it must
/// never be the thing that turns `--redonly` back into a silent write-mode boot.
pub fn classify_unknown_arg(token: &str, allow_unknown: bool) -> UnknownArg {
    let prefix = "blitzkrieg-core: ";
    let mut lines = vec![format!(
        "{prefix}{}: {token}",
        if token.starts_with('-') {
            "unknown argument"
        } else {
            "unexpected argument (the core takes no positional arguments)"
        }
    )];
    let found = suggestion(token);
    if let Some(spec) = found {
        lines.push(format!("{prefix}did you mean {}?", spec.flag));
    }
    if token.contains('=') && !token.starts_with("--config=") {
        lines.push(format!("{prefix}{JOINED_VALUE_NOTE}"));
    }
    match found.and_then(|spec| spec.risk.map(|risk| (spec, risk))) {
        Some((spec, risk)) => reject_reason(
            lines,
            format!(
                "had this been {}, the core would have started WITHOUT it — {risk}",
                spec.flag
            ),
        ),
        None if allow_unknown => {
            lines.push(format!(
                "{prefix}ignoring it: {} was given, so the core starts without it — anything it \
                 was meant to set is NOT in force.",
                ALLOW_UNKNOWN_ARGS
            ));
            UnknownArg::Ignore(lines.join("\n"))
        }
        None => reject_reason(
            lines,
            "an argument this core does not understand is never silently ignored: the boot \
             it would have produced is not the boot you asked for"
                .to_string(),
        ),
    }
}

/// The refusal, with the reason for it and the two ways out: fix the spelling
/// (the `--help` list has the exact ones), or opt in with
/// `--allow-unknown-args` — which is unavailable when the near-miss is a
/// safety flag, because that is the case the refusal exists for.
fn reject_reason(mut lines: Vec<String>, reason: String) -> UnknownArg {
    let prefix = "blitzkrieg-core: ";
    lines.push(format!("{prefix}refusing to start: {reason}."));
    lines.push(format!(
        "{prefix}run `blitzkrieg-core --help` for every accepted flag; \
         --allow-unknown-args starts anyway (it never covers a safety flag)."
    ));
    UnknownArg::Reject(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flags `main.rs`'s parse loop matches literally, read out of its
    /// source. The loop is located by two markers rather than by line numbers,
    /// so the extraction survives edits above it; a moved marker fails the test
    /// with the reason instead of silently comparing two empty sets.
    fn flags_matched_by_the_parser() -> Vec<String> {
        const SRC: &str = include_str!("main.rs");
        const START: &str = "let mut it = argv.iter().cloned();";
        const END: &str = "// ── Resolve the file-settable settings";
        let start = SRC
            .find(START)
            .unwrap_or_else(|| panic!("parse loop marker not found in main.rs: {START}"));
        let end = SRC
            .find(END)
            .expect("the resolve-section marker is missing from main.rs");
        assert!(end > start, "the parse loop markers are out of order");
        let mut out = Vec::new();
        for line in SRC[start..end].lines() {
            let trimmed = line.trim_start();
            // The escape hatch's arm names the const instead of repeating the
            // spelling, so the const is read here rather than skipped: the point
            // of the comparison is the SET of accepted spellings, and the hatch
            // is one of them.
            if trimmed.starts_with("blitzkrieg_core::cli::ALLOW_UNKNOWN_ARGS") {
                out.push(ALLOW_UNKNOWN_ARGS.to_string());
                continue;
            }
            if !trimmed.starts_with("\"--") {
                continue;
            }
            let literal = &trimmed[1..];
            let Some(close) = literal.find('"') else {
                continue;
            };
            out.push(literal[..close].to_string());
        }
        assert!(
            out.len() > 50,
            "the extraction found {} flags, which is far too few to be the parse loop",
            out.len()
        );
        out
    }

    fn flag_names_in_help() -> Vec<String> {
        let help = help_text();
        let mut out = Vec::new();
        for line in help.lines() {
            // An entry line is exactly two spaces of indent and starts a flag;
            // descriptions are indented six and never start with `--`.
            let rest = match line.strip_prefix("  --") {
                Some(r) => r,
                None => continue,
            };
            let name = rest
                .split([',', ' '])
                .next()
                .expect("split always yields one item");
            out.push(format!("--{name}"));
        }
        out
    }

    /// Acceptance criterion #3 of #228: every flag the parser accepts is in
    /// `--help`, and neither list has an entry the other lacks. The parser half
    /// is the direction that rots silently — a new flag in the `match` with no
    /// line in [`FLAGS`] (a sibling of the #228 defect: a spelling an operator
    /// cannot check) — and the `--help` half catches a flag removed from the
    /// parser but left in the table, which would advertise an argument that is
    /// now REFUSED.
    #[test]
    fn help_lists_exactly_the_flags_the_parser_accepts() {
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v.dedup();
            v
        };
        let parser = sorted(flags_matched_by_the_parser());
        let non_early = sorted(
            FLAGS
                .iter()
                .filter(|f| !f.early)
                .map(|f| f.flag.to_string())
                .collect(),
        );
        assert_eq!(
            parser, non_early,
            "the parser and the --help table disagree; a flag accepted by one and not the \
             other is the #228 defect in a new spelling"
        );
        let all = sorted(FLAGS.iter().map(|f| f.flag.to_string()).collect());
        assert_eq!(
            sorted(flag_names_in_help()),
            all,
            "--help does not list exactly the flags in the table"
        );
    }

    /// The short spellings `main` accepts must be in the list too, or `-h` works
    /// and nothing says so.
    #[test]
    fn help_prints_the_short_spellings() {
        let help = help_text();
        for spec in FLAGS.iter().filter(|f| f.early) {
            let alias = spec.alias.expect("early flags carry an alias");
            assert!(
                help.contains(&format!("{}, {alias}", spec.flag)),
                "--help omits the short spelling {alias}"
            );
        }
    }

    /// The acceptance case from #228: `--redonly` must be refused AND must name
    /// `--readonly`, because "unknown argument" alone does not tell an operator
    /// that the command they typed would have started a WRITE-mode core.
    #[test]
    fn a_misspelled_readonly_is_refused_with_the_spelling_that_was_meant() {
        for typo in ["--redonly", "--read-only", "--readonlyy", "--read_onl"] {
            match classify_unknown_arg(typo, false) {
                UnknownArg::Reject(msg) => {
                    assert!(msg.contains(typo), "{typo}: the argument is named: {msg}");
                    assert!(
                        msg.contains("did you mean --readonly?"),
                        "{typo} must be pointed at --readonly: {msg}"
                    );
                    assert!(
                        msg.contains("WRITE mode"),
                        "{typo} must state the consequence: {msg}"
                    );
                }
                UnknownArg::Ignore(msg) => panic!("{typo} must be a refusal, got: {msg}"),
            }
        }
    }

    /// The escape hatch is for a flag a newer build added, never for a
    /// misspelled safety flag: `--allow-unknown-args --redonly` must still fail
    /// to boot. Without this property the hatch would re-open #228 exactly.
    #[test]
    fn the_escape_hatch_does_not_cover_a_safety_flag_near_miss() {
        for typo in [
            "--redonly",
            "--read-only",
            "--no-auto-exit",
            "--max-notinal",
        ] {
            match classify_unknown_arg(typo, true) {
                UnknownArg::Reject(msg) => assert!(
                    msg.contains("--allow-unknown-args starts anyway"),
                    "{typo}: the refusal says what the hatch does not do: {msg}"
                ),
                UnknownArg::Ignore(msg) => panic!("{typo} must not be ignorable: {msg}"),
            }
        }
        // The benign case is what the hatch is for.
        match classify_unknown_arg("--an-experiment", true) {
            UnknownArg::Ignore(msg) => {
                assert!(msg.contains("--an-experiment"), "{msg}");
                assert!(msg.contains("ignoring it"), "{msg}");
                assert!(msg.contains(ALLOW_UNKNOWN_ARGS), "{msg}");
                assert!(msg.contains("NOT in force"), "{msg}");
            }
            UnknownArg::Reject(msg) => panic!("the hatch must ignore a benign unknown: {msg}"),
        }
    }

    /// Limits the audit called out by name: a dropped/renamed word must still
    /// reach the flag the operator meant, and the message must say what the
    /// silent version would have run with.
    #[test]
    fn a_misspelled_limit_names_the_limit_and_the_consequence() {
        for (typo, meant, consequence) in [
            ("--max-notinal", "--max-order-notional", "100 USD"),
            ("--max-order-notional-p", "--max-order-notional-pct", "OFF"),
            ("--max-open-notional", "--max-open-notional-usd", "UNCAPPED"),
            ("--max-daily-losses", "--max-daily-loss", "no budget"),
        ] {
            match classify_unknown_arg(typo, false) {
                UnknownArg::Reject(msg) => {
                    assert!(
                        msg.contains(&format!("did you mean {meant}?")),
                        "{typo} -> {meant}: {msg}"
                    );
                    assert!(
                        msg.contains(consequence),
                        "{typo} must state what the silent run would have used ({consequence}): {msg}"
                    );
                }
                UnknownArg::Ignore(msg) => panic!("{typo} must be refused: {msg}"),
            }
        }
    }

    /// An argument that resembles nothing gets the plain refusal: no invented
    /// suggestion, and the way to the list is named.
    #[test]
    fn an_unrelated_argument_is_refused_without_a_suggestion() {
        for token in [
            "--definitely-not-a-flag",
            "--xyzzy",
            "--",
            "start",
            "--mode=live",
        ] {
            match classify_unknown_arg(token, false) {
                UnknownArg::Reject(msg) => {
                    assert!(!msg.contains("did you mean"), "{token}: {msg}");
                    assert!(
                        msg.contains("--help"),
                        "{token}: the list is named as the way out: {msg}"
                    );
                    assert!(msg.contains(token.trim()), "{token} is named: {msg}");
                }
                UnknownArg::Ignore(msg) => panic!("{token} must be refused: {msg}"),
            }
        }
    }

    /// `--mode=live` is the one joined-value mistake worth its own sentence:
    /// `--config=<path>` is the only flag that takes `=`, so this is a real
    /// operator habit that lands on the unknown-argument path.
    #[test]
    fn a_joined_value_is_told_to_use_a_separate_argument() {
        let UnknownArg::Reject(msg) = classify_unknown_arg("--mode=live", false) else {
            panic!("--mode=live is not an accepted spelling");
        };
        assert!(msg.contains("separate argument"), "{msg}");
        assert!(msg.contains("--mode live"), "{msg}");
    }

    /// The suggester itself: close enough to help, far enough away to stay
    /// quiet. A wrong suggestion is worse than none, so the negative cases are
    /// part of the contract.
    #[test]
    fn suggestions_are_close_matches_only() {
        for (token, meant) in [
            ("--redonly", "--readonly"),
            ("--read-only", "--readonly"),
            ("--no-auto-exit", "--no-auto-exits"),
            ("--max-notinal", "--max-order-notional"),
            ("--no-trade", "--no-trade-log"),
            ("--trade-loggs", "--trade-log"),
            ("--hel", "--help"),
            ("--verison", "--version"),
            ("--se-min-sampl", "--se-min-samples"),
            ("--strateg-dir", "--strategy-dir"),
            ("--engin", "--engine"),
        ] {
            assert_eq!(
                suggestion(token).map(|s| s.flag),
                Some(meant),
                "{token} should suggest {meant}"
            );
        }
        for token in [
            "--definitely-not-a-flag",
            "--xyzzy",
            "--read",
            "--max",
            "--a",
            "",
            "-",
            "--",
        ] {
            assert!(
                suggestion(token).is_none(),
                "{token} must not produce a suggestion (got {:?})",
                suggestion(token).map(|s| s.flag)
            );
        }
    }

    /// The table's own invariants: the set-equality test above cannot see a
    /// duplicate row or an empty description.
    #[test]
    fn the_table_is_well_formed() {
        for spec in FLAGS {
            assert!(
                spec.flag.starts_with("--"),
                "{}: not a long flag",
                spec.flag
            );
            assert!(
                spec.flag.len() > 2
                    && spec.flag[2..]
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{}: unexpected spelling",
                spec.flag
            );
            assert!(!spec.help.is_empty(), "{}: no help text", spec.flag);
            assert!(
                spec.value.is_empty() || spec.value.starts_with('<'),
                "{}: value form must be `<...>`",
                spec.flag
            );
            if let Some(a) = spec.alias {
                assert!(
                    a.starts_with('-') && !a.starts_with("--"),
                    "{}: {a} is not a short flag",
                    spec.flag
                );
                assert!(
                    spec.early,
                    "{}: only the early flags have aliases",
                    spec.flag
                );
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for spec in FLAGS {
            assert!(seen.insert(spec.flag), "{} is listed twice", spec.flag);
        }
        // The safety set #228 is about: these must carry a consequence.
        for name in [
            "--readonly",
            "--mode",
            "--no-auto-exits",
            "--max-order-notional",
            "--max-order-notional-pct",
            "--max-open-notional-usd",
            "--size-pct",
            "--max-daily-loss",
            "--max-daily-loss-pct",
            "--no-trade-log",
        ] {
            let spec = FLAGS
                .iter()
                .find(|s| s.flag == name)
                .unwrap_or_else(|| panic!("{name} is not in the table"));
            assert!(
                spec.risk.is_some(),
                "{name} is safety-relevant and must say what a typo costs"
            );
        }
    }

    /// `is_known_flag` answers for the joined `--config=` form too, which the
    /// table cannot hold as a literal.
    #[test]
    fn the_config_joined_form_is_known() {
        assert!(is_known_flag("--config"));
        assert!(is_known_flag("--config=/tmp/x.toml"));
        assert!(!is_known_flag("--config-file"));
        for spec in FLAGS {
            assert!(is_known_flag(spec.flag), "{} is not known", spec.flag);
        }
        assert!(!is_known_flag("--redonly"));
    }

    /// `--help` is the list the audit asked for: every flag, with a sentence,
    /// plus the exit-code contract.
    #[test]
    fn help_is_a_list_not_a_stub() {
        let help = help_text();
        assert!(help.contains("usage: blitzkrieg-core"));
        assert!(help.starts_with("blitzkrieg-core —"));
        for line in help.lines() {
            if let Some(rest) = line.strip_prefix("      ") {
                assert!(
                    rest.chars().count() > 10,
                    "description too short to be one: {rest}"
                );
            }
        }
        assert!(help.contains("exit codes: 0 — --help/--version"));
        assert!(help.ends_with('\n'));
    }
}
