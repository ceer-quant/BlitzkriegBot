//! `blitzkrieg doctor` — the environment-difference reading, assembled from
//! read-only IPC answers and rendered in ONE place.
//!
//! The probes themselves live in the core (`blitzkrieg_core::probe`, surfaced
//! through `engine.stats.probe`); what this module owns is the reading: which
//! questions to ask the serving core, how each answer becomes a line an
//! operator can act on, and which answers are BAD enough to fail the doctor.
//! The split mirrors `net_check.rs` (data contract here, measurement in the
//! core), and for the same reason: the TUI, the CLI subcommand and a pasted
//! issue must all read the same report, and two renderers drift.
//!
//! Read-only by construction — every question it asks is an observation verb
//! (`core.ready`, `engine.stats`, `account.list`); there is no mode where the
//! doctor could place an order or flip a switch. Exit semantics match
//! `net-check`: 0 everything within tolerance · 1 something needs a look ·
//! 2 the doctor could not run (no core answered).

use serde_json::Value;

/// One finding of the report: what was asked, what came back, and whether it
/// is worth an operator's attention. `level` is the whole verdict vocabulary —
/// `ok` is quiet, `warn` means "look when convenient", `bad` fails the doctor.
#[derive(Debug)]
pub struct Finding {
    pub section: &'static str,
    pub label: String,
    pub detail: String,
    pub level: Level,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Bad,
}

impl Level {
    fn mark(self) -> &'static str {
        match self {
            Level::Ok => "ok  ",
            Level::Warn => "WARN",
            Level::Bad => "BAD ",
        }
    }
}

/// A whole report: the findings, plus the identity of the core that answered
/// (the report must say WHO answered — a doctor reading against a stale core
/// from yesterday is worse than no report).
pub struct DoctorReport {
    pub core_version: String,
    pub core_mode: String,
    pub core_pid: Option<u64>,
    pub findings: Vec<Finding>,
}

impl DoctorReport {
    pub fn ok(&self) -> bool {
        self.findings.iter().all(|f| f.level != Level::Bad)
    }

    pub fn warn_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.level == Level::Warn)
            .count()
    }

    pub fn bad_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.level == Level::Bad)
            .count()
    }

    /// The report as JSON — what `blitzkrieg doctor --json` prints. A
    /// `Serialize` failure is returned rather than papered over: a script
    /// reading this wants to know it got a report.
    pub fn render_json(&self) -> Result<String, String> {
        let findings: Vec<Value> = self
            .findings
            .iter()
            .map(|f| {
                serde_json::json!({
                    "section": f.section,
                    "level": match f.level { Level::Ok => "ok", Level::Warn => "warn", Level::Bad => "bad" },
                    "label": f.label,
                    "detail": f.detail,
                })
            })
            .collect();
        let doc = serde_json::json!({
            "core": {
                "version": self.core_version,
                "mode": self.core_mode,
                "pid": self.core_pid,
            },
            "ok": self.ok(),
            "warnCount": self.warn_count(),
            "badCount": self.bad_count(),
            "findings": findings,
        });
        serde_json::to_string_pretty(&doc)
            .map_err(|e| format!("the report is not serializable: {e}"))
    }

    /// The report as plain text: findings grouped by section, then the one-line
    /// reading. Text and not ANSI colour for the same reason as `net-check`:
    /// the same string goes to a terminal, a log line and a pasted issue.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "core: {} ({}) pid {}\n",
            self.core_version,
            self.core_mode,
            self.core_pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "?".to_string())
        ));
        out.push('\n');
        let mut last_section = "";
        for f in &self.findings {
            if f.section != last_section {
                out.push_str(&format!("── {} ──\n", f.section));
                last_section = f.section;
            }
            out.push_str(&format!(
                "  [{}] {} — {}\n",
                f.level.mark(),
                f.label,
                f.detail
            ));
        }
        out.push('\n');
        if self.ok() && self.warn_count() == 0 {
            out.push_str("reading: everything within tolerance\n");
        } else if self.ok() {
            out.push_str(&format!(
                "reading: {0} warning{1} to look at when convenient — nothing blocks trading\n",
                self.warn_count(),
                if self.warn_count() == 1 { "" } else { "s" }
            ));
        } else {
            out.push_str(&format!(
                "reading: {0} finding{1} need attention (see BAD lines)\n",
                self.bad_count(),
                if self.bad_count() == 1 { "s" } else { "" }
            ));
        }
        out
    }
}

/// Ask every question and compose the report. Each source degrades
/// independently: a core that predates one verb yields a WARN finding saying
/// what could not be read (a doctor that crashed on an old core would be a
/// diagnostic that fails exactly when it is needed most), never a crash.
pub fn compose(ready: &Value, stats: &Value, account_list: Result<&Value, String>) -> DoctorReport {
    let mut findings = Vec::new();

    // ── identity ──
    let core_version = ready
        .get("build")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let core_mode = ready
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let core_pid = ready.get("pid").and_then(|v| v.as_u64());

    // ── mode: live deserves to be stated, readonly is worth a note ──
    match core_mode.as_str() {
        "live" => findings.push(Finding {
            section: "core",
            label: "mode".to_string(),
            detail: "LIVE — orders route to the venue for real".to_string(),
            level: Level::Ok,
        }),
        "readonly" => findings.push(Finding {
            section: "core",
            label: "mode".to_string(),
            detail: "read-only — order egress is structurally absent".to_string(),
            level: Level::Warn,
        }),
        other => findings.push(Finding {
            section: "core",
            label: "mode".to_string(),
            detail: format!("{other} (local settlement, no venue orders)"),
            level: Level::Ok,
        }),
    }

    // ── kill switch (engine.stats.tradingFrozen) ──
    match stats.get("tradingFrozen") {
        Some(f) if f.get("active").and_then(|v| v.as_bool()) == Some(true) => {
            let reason = f
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("no reason given");
            findings.push(Finding {
                section: "core",
                label: "kill switch".to_string(),
                detail: format!("DOWN — {reason}"),
                level: Level::Bad,
            });
        }
        _ => findings.push(Finding {
            section: "core",
            label: "kill switch".to_string(),
            detail: "up (trading allowed)".to_string(),
            level: Level::Ok,
        }),
    }

    // ── the three environment probes (engine.stats.probe) ──
    let probe = stats.get("probe").cloned().unwrap_or(Value::Null);
    if probe.is_null() {
        findings.push(Finding {
            section: "probes",
            label: "environment probes".to_string(),
            detail: "this core does not report engine.stats.probe — older build; \
                     upgrade the kernel to read clock/feed health here"
                .to_string(),
            level: Level::Warn,
        });
    } else {
        compose_ntp(&probe, &mut findings);
        compose_ticks(&probe, &mut findings);
        compose_spot(&probe, &mut findings);
    }

    // ── K-line aggregator drops (engine.stats.kline) ──
    match stats.get("kline") {
        Some(k) if !k.is_null() => {
            let dropped = k
                .get("droppedOutOfOrder")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let no_data = k.get("noDataBars").and_then(|v| v.as_u64()).unwrap_or(0);
            let closed = k.get("closedBars").and_then(|v| v.as_u64()).unwrap_or(0);
            findings.push(if dropped > 0 {
                Finding {
                    section: "kline",
                    label: "aggregator".to_string(),
                    detail: format!(
                        "{closed} closed bars; {dropped} tick(s) dropped out of order, \
                         {no_data} empty bucket(s) skipped — the chart tail can lag reality"
                    ),
                    level: Level::Warn,
                }
            } else {
                Finding {
                    section: "kline",
                    label: "aggregator".to_string(),
                    detail: format!(
                        "{closed} closed bars, zero drops, {no_data} empty bucket(s) skipped"
                    ),
                    level: Level::Ok,
                }
            });
        }
        _ => findings.push(Finding {
            section: "kline",
            label: "aggregator".to_string(),
            detail: "no K-line aggregator on this core (E29 off or older build)".to_string(),
            level: Level::Warn,
        }),
    }

    // ── per-strategy rejections (engine.stats.strategies) ──
    compose_strategy_rejections(stats, &mut findings);

    // ── account-scoped kill book (engine.stats.accountFreezes) ──
    compose_account_freezes(stats, &mut findings);

    // ── accounts (account.list) ──
    match account_list {
        Ok(v) => compose_accounts(v, &mut findings),
        Err(e) => findings.push(Finding {
            section: "accounts",
            label: "account book".to_string(),
            detail: format!("account.list could not be read: {e}"),
            level: Level::Warn,
        }),
    }

    DoctorReport {
        core_version,
        core_mode,
        core_pid,
        findings,
    }
}

/// NTP: the clock-offset probe's last measurement.
fn compose_ntp(probe: &Value, out: &mut Vec<Finding>) {
    let ntp = &probe["ntp"];
    if ntp.is_null() {
        return;
    }
    let synced = ntp.get("synced").and_then(|v| v.as_bool()) == Some(true);
    let skew_red = ntp.get("skewRed").and_then(|v| v.as_bool()) == Some(true);
    let failures = ntp.get("failures").and_then(|v| v.as_u64()).unwrap_or(0);
    let offset = ntp.get("offsetMs").and_then(|v| v.as_i64());
    match (synced, offset) {
        (true, Some(ms)) if skew_red => out.push(Finding {
            section: "probes",
            label: "clock (NTP)".to_string(),
            detail: format!(
                "offset {ms:+} ms — past the 100 ms red line; sizing/round math \
                 silently disagrees with the venue. Check the host's time sync."
            ),
            level: Level::Bad,
        }),
        (true, Some(ms)) => out.push(Finding {
            section: "probes",
            label: "clock (NTP)".to_string(),
            detail: format!("offset {ms:+} ms (within tolerance)"),
            level: Level::Ok,
        }),
        (true, None) => out.push(Finding {
            section: "probes",
            label: "clock (NTP)".to_string(),
            detail: "synced but no offset reported".to_string(),
            level: Level::Ok,
        }),
        (false, _) => out.push(Finding {
            section: "probes",
            label: "clock (NTP)".to_string(),
            detail: format!(
                "no successful measurement yet ({failures} failed attempt(s)) — \
                 a blocked UDP egress or an offline resolver looks like this"
            ),
            level: if failures > 0 { Level::Warn } else { Level::Ok },
        }),
    }
}

/// Tick health: feed flow per window.
fn compose_ticks(probe: &Value, out: &mut Vec<Finding>) {
    let ticks = &probe["tickHealth"];
    if ticks.is_null() {
        return;
    }
    let zero: Vec<String> = ticks
        .get("zeroTickTokens")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let silence = ticks
        .get("maxSilenceMs")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let breaks = ticks
        .get("streamBreakWarnings")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let window = ticks
        .get("windowTicks")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if !zero.is_empty() {
        out.push(Finding {
            section: "probes",
            label: "feed (tick health)".to_string(),
            detail: format!(
                "{} token(s) with ZERO ticks in the last window: {} — a stalled \
                 venue feed or a listing the plugin stopped polling",
                zero.len(),
                zero.join(", ")
            ),
            level: Level::Warn,
        });
    }
    if silence >= 60_000 {
        out.push(Finding {
            section: "probes",
            label: "feed (tick health)".to_string(),
            detail: format!(
                "longest token silence {silence} ms (>= the 60 s warn budget), \
                 {breaks} stream-break warning(s) this session"
            ),
            level: Level::Warn,
        });
    } else if zero.is_empty() {
        out.push(Finding {
            section: "probes",
            label: "feed (tick health)".to_string(),
            detail: format!(
                "flowing — {window} tick(s) in the current window, longest \
                 silence {silence} ms, {breaks} break warning(s) this session"
            ),
            level: Level::Ok,
        });
    }
}

/// Spot: signal-source staleness.
fn compose_spot(probe: &Value, out: &mut Vec<Finding>) {
    let spot = &probe["spot"];
    if spot.is_null() {
        return;
    }
    let stale = spot.get("staleMs").and_then(|v| v.as_i64());
    let warnings = spot
        .get("staleWarnings")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    match stale {
        Some(ms) if ms >= 60_000 => out.push(Finding {
            section: "probes",
            label: "spot signal".to_string(),
            detail: format!(
                "stale for {ms} ms (>= the 60 s budget, {warnings} warning(s) \
                 this session) — trend/momentum signals price off an old mark"
            ),
            level: Level::Warn,
        }),
        Some(ms) => out.push(Finding {
            section: "probes",
            label: "spot signal".to_string(),
            detail: format!("fresh ({ms} ms since the last update)"),
            level: Level::Ok,
        }),
        None => out.push(Finding {
            section: "probes",
            label: "spot signal".to_string(),
            detail: "no spot update seen yet this session".to_string(),
            level: Level::Warn,
        }),
    }
}

/// Strategies that are being REFUSED entries — a silent cap misconfiguration
/// is exactly the "why is it not trading?" question the doctor exists for.
fn compose_strategy_rejections(stats: &Value, out: &mut Vec<Finding>) {
    let strategies = match stats.get("strategies").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return,
    };
    for s in strategies {
        let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let limit_rejected = s.get("limitRejected").and_then(|v| v.as_u64()).unwrap_or(0);
        let rejected = s.get("rejected").and_then(|v| v.as_u64()).unwrap_or(0);
        let placed = s.get("ordersPlaced").and_then(|v| v.as_u64()).unwrap_or(0);
        if limit_rejected > 0 || rejected > 0 {
            let causes: Vec<String> = s
                .get("rejectionCauses")
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| format!("{k}×{}", v.as_u64().unwrap_or(0)))
                        .collect()
                })
                .unwrap_or_default();
            out.push(Finding {
                section: "strategies",
                label: name.to_string(),
                detail: format!(
                    "{placed} placed · {rejected} refused{limp}{}",
                    if causes.is_empty() {
                        String::new()
                    } else {
                        format!(" · causes: {}", causes.join(", "))
                    },
                    limp = if limit_rejected > 0 {
                        format!(" ({limit_rejected} by a limit)")
                    } else {
                        String::new()
                    },
                ),
                level: Level::Warn,
            });
        }
    }
}

/// The account book: trading switches and venue bindings off their defaults.
fn compose_accounts(list: &Value, out: &mut Vec<Finding>) {
    let accounts = match list.get("accounts").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => {
            // The 1.1 envelope wraps rows differently — try a bare array too.
            match list.as_array() {
                Some(a) if !a.is_empty() => a,
                _ => {
                    out.push(Finding {
                        section: "accounts",
                        label: "account book".to_string(),
                        detail: "account.list returned an unreadable shape".to_string(),
                        level: Level::Warn,
                    });
                    return;
                }
            }
        }
    };
    for a in accounts {
        let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let trading = a
            .get("tradingEnabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if !trading {
            out.push(Finding {
                section: "accounts",
                label: id.to_string(),
                detail: "trading switch OFF — entries refused, exits still managed".to_string(),
                level: Level::Warn,
            });
        }
    }
}

/// The account-scoped kill book (`engine.stats.accountFreezes`): one BAD
/// finding per frozen account, naming the reason. BAD, not WARN — a freeze
/// is the kernel's own verdict that something on this venue went wrong; the
/// operator is expected to look NOW (unlike a trading switch, which is a
/// deliberate operator act).
fn compose_account_freezes(stats: &Value, out: &mut Vec<Finding>) {
    let Some(book) = stats.get("accountFreezes").and_then(|v| v.as_object()) else {
        return; // old core without the surface — silent, like other seams
    };
    if book.is_empty() {
        return;
    }
    for (id, reason) in book {
        out.push(Finding {
            section: "accounts",
            label: id.clone(),
            detail: format!(
                "FROZEN (account-scoped kill switch) — {reason}; new orders refused, exits flow"
            ),
            level: Level::Bad,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The ready payload's identity fields, as the core serialises them.
    fn ready() -> Value {
        json!({
            "version": "1.1",
            "mode": "dry",
            "pid": 4242,
            "build": "0.4.1+gd0edf706",
        })
    }

    /// A quiet engine.stats: probes armed and healthy, no frozen flag, a
    /// clean aggregator, no strategy rows. Individual tests overlay their
    /// finding.
    fn quiet_stats() -> Value {
        json!({
            "probe": {
                "ntp": { "synced": true, "offsetMs": 5, "skewRed": false, "failures": 0, "lastCheckMs": 1 },
                "tickHealth": { "windowTicks": 40, "zeroTickTokens": [], "maxSilenceMs": 1_200, "streamBreakWarnings": 0, "lastSummaryMs": 1 },
                "spot": { "lastSeenMs": 1, "staleMs": 300, "staleWarnings": 0 },
            },
            "kline": { "symbols": 3, "liveBars": 3, "closedBars": 120, "droppedOutOfOrder": 0, "noDataBars": 0, "intervals": [] },
            "tradingFrozen": null,
            "strategies": [],
        })
    }

    fn accounts(value: Value) -> Result<Value, String> {
        Ok(value)
    }

    fn compose_now(stats: Value, account_list: Result<Value, String>) -> DoctorReport {
        let ok = account_list.as_ref().map_err(|e| e.clone());
        compose(&ready(), &stats, ok)
    }

    /// The all-clear report: every level ok, text says so in words.
    #[test]
    fn a_quiet_core_reads_as_within_tolerance() {
        let r = compose_now(quiet_stats(), accounts(json!({ "accounts": [] })));
        assert!(r.ok(), "{:#?}", r.findings);
        assert_eq!(r.bad_count(), 0);
        assert_eq!(r.warn_count(), 0);
        let text = r.render_text();
        assert!(text.contains("everything within tolerance"), "{text}");
        assert!(text.contains("0.4.1+gd0edf706"), "{text}");
        assert!(text.contains("dry"), "{text}");
    }

    /// A clock past the red line fails the doctor and says why it matters.
    #[test]
    fn a_red_line_clock_offset_is_bad() {
        let mut stats = quiet_stats();
        stats["probe"]["ntp"] = json!({
            "synced": true, "offsetMs": 150, "skewRed": true, "failures": 0, "lastCheckMs": 1,
        });
        let r = compose_now(stats, accounts(json!({ "accounts": [] })));
        assert!(!r.ok());
        let f = r
            .findings
            .iter()
            .find(|f| f.label.contains("clock"))
            .unwrap();
        assert_eq!(f.level, Level::Bad);
        assert!(
            f.detail.contains("150") && f.detail.contains("100 ms"),
            "{}",
            f.detail
        );
        let text = r.render_text();
        assert!(text.contains("[BAD ]"), "{text}");
        assert!(text.contains("need attention"), "{text}");
    }

    /// A clock inside tolerance is a quiet ok line carrying the signed offset.
    #[test]
    fn a_within_tolerance_offset_is_ok() {
        let mut stats = quiet_stats();
        stats["probe"]["ntp"]["offsetMs"] = json!(10);
        stats["probe"]["ntp"]["synced"] = json!(true);
        let r = compose_now(stats, accounts(json!({ "accounts": [] })));
        assert!(r.ok(), "{:#?}", r.findings);
        let f = r
            .findings
            .iter()
            .find(|f| f.label.contains("clock"))
            .unwrap();
        assert_eq!(f.level, Level::Ok);
        assert!(f.detail.contains("+10"), "{}", f.detail);
    }

    /// A frozen account fails the doctor as BAD, naming the account and the
    /// kernel's own reason. An empty freeze book stays quiet.
    #[test]
    fn a_frozen_account_is_bad_with_reason() {
        let mut stats = quiet_stats();
        stats["accountFreezes"] = json!({
            "probe2": "5 consecutive venue rejections on account probe2; last: auth failed",
        });
        let r = compose_now(stats, accounts(json!({ "accounts": [] })));
        assert!(!r.ok());
        assert_eq!(r.bad_count(), 1, "{:#?}", r.findings);
        let f = r
            .findings
            .iter()
            .find(|f| f.section == "accounts" && f.label == "probe2")
            .unwrap();
        assert_eq!(f.level, Level::Bad);
        assert!(f.detail.contains("FROZEN"), "{}", f.detail);
        assert!(
            f.detail.contains("5 consecutive venue rejections"),
            "the kernel's reason rides along: {}",
            f.detail
        );
        // An empty book (or an old core without the key) stays quiet.
        let mut stats = quiet_stats();
        stats["accountFreezes"] = json!({});
        assert!(compose_now(stats, accounts(json!({ "accounts": [] }))).ok());
    }

    /// A silent token (zero ticks in the window) is a WARN naming the token.
    #[test]
    fn zero_tick_tokens_are_named() {
        let mut stats = quiet_stats();
        stats["probe"]["tickHealth"]["zeroTickTokens"] = json!(["btc_up", "eth_down"]);
        let r = compose_now(stats, accounts(json!({ "accounts": [] })));
        assert!(
            r.ok(),
            "a silent token warns, never fails: {:#?}",
            r.findings
        );
        let f = r
            .findings
            .iter()
            .find(|f| f.label.contains("tick health"))
            .unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(
            f.detail.contains("btc_up") && f.detail.contains("eth_down"),
            "{}",
            f.detail
        );
    }

    /// A frozen kill switch fails the doctor and carries the reason.
    #[test]
    fn a_down_kill_switch_is_bad() {
        let mut stats = quiet_stats();
        stats["tradingFrozen"] = json!({ "active": true, "reason": "daily loss budget spent" });
        let r = compose_now(stats, accounts(json!({ "accounts": [] })));
        assert!(!r.ok());
        let f = r
            .findings
            .iter()
            .find(|f| f.label.contains("kill switch"))
            .unwrap();
        assert_eq!(f.level, Level::Bad);
        assert!(f.detail.contains("daily loss budget spent"), "{}", f.detail);
    }

    /// Out-of-order K-line drops warn (the chart tail can lag reality); a
    /// clean aggregator is quiet-ok with the bar count.
    #[test]
    fn kline_drops_warn_and_a_clean_aggregator_is_ok() {
        let mut stats = quiet_stats();
        stats["kline"] =
            json!({ "symbols": 3, "closedBars": 120, "droppedOutOfOrder": 7, "noDataBars": 2 });
        let r = compose_now(stats.clone(), accounts(json!({ "accounts": [] })));
        let f = r.findings.iter().find(|f| f.section == "kline").unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(
            f.detail.contains("7") && f.detail.contains("120"),
            "{}",
            f.detail
        );

        stats["kline"]["droppedOutOfOrder"] = json!(0);
        let r = compose_now(stats, accounts(json!({ "accounts": [] })));
        let f = r.findings.iter().find(|f| f.section == "kline").unwrap();
        assert_eq!(f.level, Level::Ok);
    }

    /// A strategy with limit refusals is a WARN naming the cause bucket —
    /// the silent cap misconfiguration the doctor exists to surface.
    #[test]
    fn limit_rejected_strategies_surface_with_causes() {
        let mut stats = quiet_stats();
        stats["strategies"] = json!([
            { "name": "spread_arb", "ordersPlaced": 3, "rejected": 4, "limitRejected": 2,
              "rejectionCauses": { "limit.virtualCap": 2 } },
        ]);
        let r = compose_now(stats, accounts(json!({ "accounts": [] })));
        let f = r
            .findings
            .iter()
            .find(|f| f.section == "strategies")
            .unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(f.label.contains("spread_arb"), "{}", f.label);
        assert!(f.detail.contains("limit.virtualCap×2"), "{}", f.detail);
    }

    /// An account with its trading switch off warns — that is a chosen
    /// posture, not a fault, so it must not fail the doctor.
    #[test]
    fn a_trading_off_account_warns_without_failing() {
        let list = json!({ "accounts": [
            { "id": "probe2", "tradingEnabled": false },
            { "id": "default", "tradingEnabled": true },
        ]});
        let r = compose_now(quiet_stats(), accounts(list));
        assert!(
            r.ok(),
            "a chosen switch is a warn, not a fault: {:#?}",
            r.findings
        );
        let f = r.findings.iter().find(|f| f.section == "accounts").unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(f.label == "probe2", "{}", f.label);
        assert!(f.detail.contains("exits still managed"), "{}", f.detail);
    }

    /// A core too old to know the probes at all degrades to a WARN that says
    /// to upgrade — the doctor must not crash on the core it most needs to
    /// read.
    #[test]
    fn a_probeless_core_warns_instead_of_crashing() {
        let mut stats = quiet_stats();
        stats.as_object_mut().unwrap().remove("probe");
        let r = compose_now(stats, accounts(json!({ "accounts": [] })));
        let f = r.findings.iter().find(|f| f.section == "probes").unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(f.detail.contains("older build"), "{}", f.detail);
    }

    /// A core with no K-line aggregator (E29 off or older build) degrades to
    /// a WARN — never a crash, never a silent ok.
    #[test]
    fn a_klineless_core_warns_instead_of_crashing() {
        let mut stats = quiet_stats();
        stats["kline"] = json!(null);
        let r = compose_now(stats, accounts(json!({ "accounts": [] })));
        let f = r.findings.iter().find(|f| f.section == "kline").unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(f.detail.contains("aggregator"), "{}", f.detail);
    }

    /// An unreadable account.list degrades to a WARN naming the verb.
    #[test]
    fn an_unreadable_account_list_is_a_warn_naming_the_verb() {
        let r = compose_now(
            quiet_stats(),
            Err("rpc -32601: method not found".to_string()),
        );
        let f = r.findings.iter().find(|f| f.section == "accounts").unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(f.detail.contains("account.list"), "{}", f.detail);
    }

    /// The JSON report carries the verdict counts and the core identity —
    /// the shape `--json` consumers script against.
    #[test]
    fn the_json_report_carries_counts_and_identity() {
        let mut stats = quiet_stats();
        stats["probe"]["ntp"] = json!({
            "synced": true, "offsetMs": 150, "skewRed": true, "failures": 0, "lastCheckMs": 1,
        });
        let r = compose_now(stats, accounts(json!({ "accounts": [] })));
        let doc: Value = serde_json::from_str(&r.render_json().unwrap()).unwrap();
        assert_eq!(doc["ok"], json!(false));
        assert_eq!(doc["badCount"], json!(1));
        assert_eq!(doc["core"]["pid"], json!(4242));
        assert_eq!(doc["core"]["version"], json!("0.4.1+gd0edf706"));
        assert_eq!(doc["findings"][0]["level"], json!("ok"));
        assert!(doc["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["level"] == json!("bad") && f["section"] == json!("probes")));
    }
}
