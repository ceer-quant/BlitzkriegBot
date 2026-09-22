//! Network self-check rendering — the ONE place a `NetCheckReportView` becomes
//! text.
//!
//! Why this is a shared module rather than a formatting call at each call site:
//! the same report is read by the TUI overlay (`n`), by `blitzkrieg net-check`
//! on the command line, and by the browser panel — and the browser panel splits
//! the labels out into Vue components while the first two print the table
//! below. Two renderers drift; one cannot. The Chinese wording of the WebUI is
//! its own surface (it splits per field), so nothing here needs translating for
//! that: what is shared is the DATA reading — which status means what, and which
//! hint code summarises which report.
//!
//! Two rules the labels obey, both about not lying to the operator:
//!   * an UNKNOWN status is echoed verbatim, never mapped to something prettier
//!     or dropped — a core that grows a status must not be rendered as blank;
//!   * `unsupported` and `rejected` never read as a failure of the network. One
//!     means "this build has no probe for that path", the other "the endpoint
//!     was refused by policy before any dial" — an operator who reads either as
//!     "the venue is down" goes hunting for the wrong problem.

use crate::core::types::{NetCheckItemView, NetCheckReportView};

/// Human label for one probe's `status`. Unknown values are echoed.
pub fn status_label(status: &str) -> String {
    let label = match status {
        "ok" => "ok",
        "dns_failed" => "DNS failed",
        "tcp_refused" => "TCP refused",
        "tcp_timeout" => "TCP timeout",
        "tls_cert" => "TLS cert rejected",
        "tls_error" => "TLS handshake failed",
        "timeout" => "timeout",
        "http_error" => "HTTP error",
        "transport_error" => "transport error",
        "rejected" => "refused by policy",
        "unsupported" => "no probe",
        other => return other.to_string(),
    };
    label.to_string()
}

/// Heading for a whole report's `hint_code`, used when the composer wants a
/// short line above the table. Unknown codes fall back to the core's own
/// `hint` sentence, which is always present.
pub fn hint_label(code: &str) -> &'static str {
    match code {
        "ok" => "all network paths OK",
        "tls_blocked" => "TLS blocked before the request",
        "dns_failed" => "name resolution failed",
        "proxy_env" => "a proxy is configured for this process",
        "fake_ip" => "the resolver is answering with fake IPs",
        "partial" => "some paths failed",
        "unsupported" => "this build exposes no network probe",
        _ => "network self-check",
    }
}

/// Parse the report out of the core's `--net-check` stdout.
///
/// The same JSON the `net.check` IPC method returns, read here with the same
/// types, so a one-shot probe prints exactly what a running panel would show.
/// The slice between the first `{` and the last `}` is what gets parsed: the
/// core may have logged a line before the report, and a diagnostic that fails
/// to print because of an unrelated banner is worse than no diagnostic. A
/// payload that still does not parse is returned as text for the caller to
/// print — the operator needs the raw bytes to see what actually came back.
pub fn parse_report(stdout: &str) -> Result<NetCheckReportView, String> {
    let start = stdout.find('{');
    let end = stdout.rfind('}');
    let (Some(start), Some(end)) = (start, end) else {
        return Err(format!(
            "core --net-check printed no JSON report: {}",
            truncate(stdout.trim(), 200)
        ));
    };
    if end < start {
        return Err("core --net-check printed a truncated report".to_string());
    }
    serde_json::from_str(&stdout[start..=end])
        .map_err(|e| format!("core --net-check printed a report this build cannot read: {e}"))
}

/// The report as JSON — what `blitzkrieg net-check --json` prints, i.e. the same
/// document the core emits and the panels parse.
///
/// A `Serialize` failure is returned rather than papered over with an empty
/// object: a script reading this wants to know it got a report, not to be handed
/// something that parses and says nothing.
pub fn render_json(report: &NetCheckReportView) -> Result<String, String> {
    serde_json::to_string_pretty(report).map_err(|e| format!("the report is not serializable: {e}"))
}

/// The report as a plain-text table: one line per probe, then the core's own
/// reading, then the proxy variable NAMES.
///
/// Text and not ANSI colour on purpose: the same string goes to a terminal, to
/// the TUI's overlay (which paints its own background) and to a log line, and a
/// report pasted into an issue must stay readable.
pub fn render_text(report: &NetCheckReportView) -> String {
    let (passed, total) = report.passed();
    let mut out = format!(
        "network self-check: {}/{} paths OK — {}\n",
        passed,
        total,
        hint_label(&report.hint_code)
    );
    for item in &report.items {
        out.push_str(&render_item(item));
        out.push('\n');
    }
    if report.items.is_empty() {
        out.push_str("  (no probe reported — an empty report is never a pass)\n");
    }
    let hint = if report.hint.trim().is_empty() {
        hint_label(&report.hint_code).to_string()
    } else {
        report.hint.clone()
    };
    out.push_str(&format!("reading: {hint}\n"));
    if !report.proxy_env.is_empty() {
        out.push_str(&format!(
            "proxy variables set: {} (names only — values may carry credentials)\n",
            report.proxy_env.join(", ")
        ));
    }
    out
}

/// Column widths of the one-line-per-path table.
///
/// Single source on purpose: the plain-text renderer below and the TUI overlay
/// both lay a row out from these, and while each hard-coded its own format
/// string, changing one width silently mis-aligned the other — a table that
/// reads differently in the log pane than in the overlay is a table nobody can
/// compare across the two (#260).
pub const COL_NAME: usize = 11;
pub const COL_TARGET: usize = 34;
pub const COL_MS: usize = 7;

/// One row's fixed-width columns — name, truncated target, right-aligned ms —
/// with no leading mark and no trailing status. Shared so the two renderers
/// cannot disagree about where the columns are.
pub fn row_columns(item: &NetCheckItemView) -> String {
    format!(
        "{:<COL_NAME$} {:<COL_TARGET$} {:>COL_MS$} ms",
        item.name,
        truncate(&item.target, COL_TARGET),
        item.ms
    )
}

/// One table row. The detail is appended only when it adds something the status
/// does not already say, so a passing row stays one short line.
fn render_item(item: &NetCheckItemView) -> String {
    let mark = if item.ok { "ok  " } else { "FAIL" };
    let mut line = format!(
        "  {mark} {}  {}",
        row_columns(item),
        status_label(&item.status)
    );
    if item.fake_ip {
        line.push_str(" [fake-ip]");
    }
    let detail = item.detail.trim();
    if !detail.is_empty() && !line.trim_end().ends_with(detail) {
        line.push_str(&format!(" — {detail}"));
    }
    line
}

/// Truncate to `max` characters with a trailing ellipsis, counting CHARACTERS
/// (a byte index would panic on a non-ASCII endpoint).
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, status: &str, ok: bool) -> NetCheckItemView {
        NetCheckItemView {
            name: name.into(),
            target: format!("{name}.example"),
            ok,
            status: status.into(),
            addrs: vec![],
            fake_ip: false,
            ms: 12,
            detail: String::new(),
        }
    }

    fn report(ok: bool, items: Vec<NetCheckItemView>) -> NetCheckReportView {
        NetCheckReportView {
            ok,
            ts_ms: 1,
            hint_code: if ok { "ok" } else { "partial" }.into(),
            hint: "venue-rest answered in 12 ms".into(),
            proxy_env: vec![],
            items,
        }
    }

    #[test]
    fn every_status_the_core_emits_has_a_label() {
        for (status, expected) in [
            ("ok", "ok"),
            ("dns_failed", "DNS failed"),
            ("tcp_refused", "TCP refused"),
            ("tcp_timeout", "TCP timeout"),
            ("tls_cert", "TLS cert rejected"),
            ("tls_error", "TLS handshake failed"),
            ("timeout", "timeout"),
            ("http_error", "HTTP error"),
            ("transport_error", "transport error"),
            ("rejected", "refused by policy"),
            ("unsupported", "no probe"),
        ] {
            assert_eq!(status_label(status), expected, "{status}");
        }
    }

    /// An unknown status is echoed. Mapping it to "" (or to "ok") would hide a
    /// core-side addition behind a blank cell, which is the one outcome an
    /// operator cannot act on.
    #[test]
    fn an_unknown_status_is_echoed_not_hidden() {
        assert_eq!(status_label("quic_reset"), "quic_reset");
    }

    /// The two statuses that are NOT network failures must not be rendered as
    /// failures: `unsupported` is a build fact, `rejected` is a policy refusal
    /// taken before any dial, and calling either "down" sends the operator
    /// after the wrong problem.
    #[test]
    fn non_network_statuses_say_what_they_are() {
        assert_eq!(status_label("unsupported"), "no probe");
        assert_eq!(status_label("rejected"), "refused by policy");
        assert!(!status_label("unsupported").contains("fail"));
        assert!(!status_label("rejected").contains("fail"));
    }

    #[test]
    fn the_table_carries_the_count_the_hint_and_the_proxy_names() {
        let mut r = report(
            false,
            vec![
                item("venue-rest", "ok", true),
                item("venue-ws", "tls_error", false),
            ],
        );
        r.proxy_env = vec!["HTTPS_PROXY".into(), "https_proxy".into()];
        let text = render_text(&r);
        assert!(text.contains("1/2 paths OK"), "{text}");
        assert!(text.contains("TLS handshake failed"), "{text}");
        assert!(text.contains("ok   venue-rest"), "{text}");
        assert!(text.contains("FAIL venue-ws"), "{text}"); // FAIL keeps its width
        assert!(
            text.contains("reading: venue-rest answered in 12 ms"),
            "{text}"
        );
        assert!(
            text.contains("HTTPS_PROXY, https_proxy") && text.contains("names only"),
            "{text}"
        );
    }

    /// An empty report must read as "no evidence", not as "all clear": the count
    /// line says 0/0 and the body says so in words.
    #[test]
    fn an_empty_report_is_not_a_pass() {
        let text = render_text(&report(false, vec![]));
        assert!(text.contains("0/0 paths OK"), "{text}");
        assert!(text.contains("never a pass"), "{text}");
    }

    /// `unsupported` reports (a build with no venue probe) keep `ok == false`
    /// and say why, rather than rendering an empty table.
    #[test]
    fn an_unsupported_report_says_so() {
        let mut r = report(false, vec![item("venue", "unsupported", false)]);
        r.hint_code = "unsupported".into();
        r.hint = "market plugin `none` implements no network probe".into();
        let text = render_text(&r);
        assert!(text.contains("no probe"), "{text}");
        assert!(text.contains("no network probe"), "{text}");
        assert!(!text.contains("paths OK — all"), "{text}");
    }

    /// The hint the core composes is what gets printed; a missing one falls back
    /// to the code's label instead of printing an empty "reading:".
    #[test]
    fn a_missing_hint_falls_back_to_the_code_label() {
        let mut r = report(false, vec![item("venue-rest", "dns_failed", false)]);
        r.hint = String::new();
        r.hint_code = "dns_failed".into();
        let text = render_text(&r);
        assert!(text.contains("reading: name resolution failed"), "{text}");
    }

    #[test]
    fn targets_are_truncated_by_characters_not_bytes() {
        assert_eq!(truncate("short", COL_TARGET), "short");
        let long = "a".repeat(40);
        assert_eq!(truncate(&long, COL_TARGET).chars().count(), COL_TARGET);
        // A multi-byte target must not panic or split a character.
        let wide = "市场".repeat(20);
        let cut = truncate(&wide, 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'));
    }

    /// The columns have ONE source, so a width change cannot land on one
    /// renderer and miss the other: both `render_text` and the TUI overlay build
    /// their row from `row_columns`, and this pins what that helper promises —
    /// the declared widths, and the truncation to `COL_TARGET`.
    #[test]
    fn the_table_columns_have_one_source() {
        let mut it = item("venue-rest", "ok", true);
        it.ms = 1234;
        let row = row_columns(&it);

        // name left-aligned in COL_NAME, then a space, then the target column.
        let (name_col, rest) = row.split_at(COL_NAME);
        assert_eq!(name_col, "venue-rest ", "name is padded to COL_NAME");
        assert_eq!(&rest[..1], " ", "one separator after the name column");
        let (target_col, rest) = rest[1..].split_at(COL_TARGET);
        assert_eq!(
            target_col.trim_end(),
            it.target.as_str(),
            "target column is COL_TARGET"
        );
        assert_eq!(&rest[..1], " ", "one separator after the target column");
        // ms is right-aligned in COL_MS, then the literal unit.
        let (ms_col, unit) = rest[1..].split_at(COL_MS);
        assert_eq!(ms_col, "   1234", "ms is right-aligned to COL_MS");
        assert_eq!(unit, " ms");

        // An over-long target is truncated to exactly the column, so the columns
        // after it do not shift.
        let mut long = item("venue-rest", "ok", true);
        long.target = "a".repeat(COL_TARGET * 2);
        let long_row = row_columns(&long);
        assert_eq!(long_row.chars().count(), row.chars().count());
        assert!(
            long_row.contains('…'),
            "a clipped target says so: {long_row}"
        );

        // And the text renderer really is built from it: same columns, with the
        // mark in front and the status behind.
        let line = render_item(&it);
        assert_eq!(
            line,
            format!("  ok   {}  {}", row, status_label(&it.status)),
            "render_item must lay out from row_columns"
        );
    }

    /// A detail that already appears in the line is not repeated.
    #[test]
    fn the_detail_is_appended_only_when_it_adds_something() {
        let mut it = item("venue-rest", "dns_failed", false);
        it.detail = "no addresses returned".into();
        assert!(render_item(&it).ends_with("— no addresses returned"));
        it.detail = String::new();
        assert!(!render_item(&it).contains(" — "));
    }

    /// `blitzkrieg net-check` reads the core's one-shot stdout, which may carry
    /// a log line before the report. Both must survive the round trip.
    #[test]
    fn the_core_stdout_is_parsed_even_with_a_leading_log_line() {
        let json = r#"{"ok":true,"tsMs":1,"hintCode":"ok","hint":"venue-rest answered in 12 ms","proxyEnv":[],"items":[{"name":"venue-rest","target":"https://clob.example","ok":true,"status":"ok","addrs":["1.2.3.4:443"],"fakeIp":false,"ms":12,"detail":""}]}"#;
        let report = parse_report(&format!("core: warming up\n{json}\n")).expect("parses");
        assert!(report.ok);
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].name, "venue-rest");
        assert_eq!(report.items[0].addrs, vec!["1.2.3.4:443".to_string()]);
    }

    /// No JSON at all is an error carrying the raw bytes — never a silent
    /// empty report, which would render as "0/0 paths OK".
    #[test]
    fn a_report_that_never_arrived_is_an_error_with_the_bytes() {
        let err = parse_report("blitzkrieg-core: starting\n").unwrap_err();
        assert!(err.contains("no JSON report"), "{err}");
        assert!(err.contains("blitzkrieg-core: starting"), "{err}");
    }

    /// `--json` prints the same document the core emits: it must survive a round
    /// trip through the parser every consumer uses.
    #[test]
    fn the_json_rendering_round_trips() {
        let r = report(true, vec![item("venue-rest", "ok", true)]);
        let json = render_json(&r).expect("serializes");
        assert!(json.contains("\"hintCode\""), "{json}");
        assert!(json.contains("\"fakeIp\""), "{json}");
        let back = parse_report(&json).expect("parses");
        assert_eq!(back.ok, r.ok);
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.items[0].status, "ok");
    }
}
