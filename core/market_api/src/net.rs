//! The network self-check's shared, dependency-free pieces (`net.check` /
//! `--net-check` / `blitzkrieg net-check`).
//!
//! The probe itself belongs to the venue extension: only it knows which hosts a
//! venue needs, and only it has an HTTP stack. What lives here is everything the
//! two callers must agree on — how a resolved address is judged, which proxy
//! variables this process can see, and what the report as a whole says. Those
//! rules are in one place so the CLI, the TUI and the WebUI explain the same
//! failure the same way instead of each inventing a reading of the same numbers.

use crate::types::{NetCheckItem, NetCheckReport};
use std::net::{IpAddr, Ipv4Addr};
use std::time::{SystemTime, UNIX_EPOCH};

/// Proxy variables that route a process's outbound traffic through something it
/// never chose.
///
/// Reported by NAME, never by value: a proxy URL can carry credentials
/// (`http://user:pass@host:3128`) and this report is printed, logged, rendered
/// in a panel and pasted into bug reports. The `NO_PROXY` pair is included
/// because it can undo the others for a specific host, and an operator reading
/// "HTTPS_PROXY is set" needs to know the exemption exists.
pub const PROXY_ENV_VARS: [&str; 8] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

/// Whether `addr` is inside the range a proxy's fake-IP DNS hands out.
///
/// Inside such a range the address says nothing about the internet: the resolver
/// answered with a marker its tunnel is expected to route. It is reported as a
/// FACT (`NetCheckItem::fake_ip`) and not as a failure, because a tunnel that
/// routes the marker works exactly as well as a real address does — when the
/// tunnel does not route it, the failure surfaces one stage later, at TLS, where
/// the report can name it precisely.
pub fn is_fake_ip(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => is_fake_ipv4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_fake_ipv4(&v4),
            // fc00::/7 — unique local addresses, what the same proxies use as
            // their IPv6 fake range.
            None => (v6.segments()[0] & 0xfe00) == 0xfc00,
        },
    }
}

fn is_fake_ipv4(v4: &Ipv4Addr) -> bool {
    let o = v4.octets();
    // 198.18.0.0/15 — RFC 2544's benchmarking block, which Clash, Surge, Stash
    // and their relatives reuse for fake-IP answers.
    o[0] == 198 && (o[1] == 18 || o[1] == 19)
}

/// Names of the proxy variables visible to THIS process, in the order of
/// [`PROXY_ENV_VARS`] (so the output is stable across runs and platforms).
pub fn proxy_env_names() -> Vec<String> {
    PROXY_ENV_VARS
        .iter()
        .filter(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
        .map(|name| (*name).to_string())
        .collect()
}

/// Wall-clock milliseconds since the epoch (0 when the clock is before it).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Fill in the report-level fields ([`NetCheckReport::ok`], `hint_code`,
/// `hint`) from the probe items and the visible proxy variables.
///
/// The rules live here rather than in the extension so that a second venue gets
/// the same reading for free, and so the mapping is unit-testable without a
/// network.
pub fn finish(mut report: NetCheckReport, proxy: Vec<String>) -> NetCheckReport {
    report.ok = !report.items.is_empty() && report.items.iter().all(|i| i.ok);
    let failing: Vec<&NetCheckItem> = report.items.iter().filter(|i| !i.ok).collect();
    report.hint_code = hint_code(&report.items, &failing, &proxy).to_string();
    report.hint = hint_text(&report.items, &failing, &proxy);
    report.proxy_env = proxy;
    report
}

/// The most specific explanation the report supports, in the order that decides
/// what an operator does next.
///
/// Deliberately a priority and not a set: the UIs print ONE headline, and a
/// headline that hedges ("possibly TLS, possibly DNS, possibly a proxy") is the
/// thing this report exists to replace. The supporting sentences all appear in
/// [`hint_text`], so nothing the priority hides is lost.
fn hint_code(items: &[NetCheckItem], failing: &[&NetCheckItem], proxy: &[String]) -> &'static str {
    if items.is_empty() || items.iter().all(|i| i.status == UNSUPPORTED) {
        return UNSUPPORTED;
    }
    if failing.is_empty() {
        return "ok";
    }
    if failing.iter().any(|i| is_tls_status(&i.status)) {
        return "tls_blocked";
    }
    if failing.iter().all(|i| i.status == "dns_failed") {
        return "dns_failed";
    }
    if !proxy.is_empty() {
        return "proxy_env";
    }
    if items.iter().all(|i| i.fake_ip) {
        return "fake_ip";
    }
    "partial"
}

/// Status of a plugin that implements no probe at all.
pub const UNSUPPORTED: &str = "unsupported";

/// Whether a probe status means "TCP completed, TLS did not".
pub fn is_tls_status(status: &str) -> bool {
    matches!(status, "tls_error" | "tls_cert")
}

/// The full reading: every applicable sentence, in the same order as the codes.
fn hint_text(items: &[NetCheckItem], failing: &[&NetCheckItem], proxy: &[String]) -> String {
    if items.is_empty() {
        return "no probe was run".to_string();
    }
    if items.iter().all(|i| i.status == UNSUPPORTED) {
        return "this build's market plugin implements no network probe".to_string();
    }
    if failing.is_empty() {
        let mut s = format!("all {} network paths answered", items.len());
        // A green report still carries its context. These are the sentences an
        // operator wants to have read BEFORE the failure a tunneled resolver
        // eventually causes, and this function's contract is that no supporting
        // sentence is lost to the headline — on a passing report the headline has
        // nothing to say at all, so it is the only branch where that would
        // actually hide them.
        for note in [fake_ip_note(items), proxy_note(proxy)] {
            if !note.is_empty() {
                s.push_str("; ");
                s.push_str(&note);
            }
        }
        return s;
    }
    let mut notes: Vec<String> = Vec::new();
    let tls: Vec<&str> = failing
        .iter()
        .filter(|i| is_tls_status(&i.status))
        .map(|i| i.target.as_str())
        .collect();
    if !tls.is_empty() {
        notes.push(format!(
            "TCP connects to {} but TLS never completes: the connection is being \
             intercepted, or the host is blocked by name (SNI)",
            tls.join(", ")
        ));
    }
    let dns: Vec<&str> = failing
        .iter()
        .filter(|i| i.status == "dns_failed")
        .map(|i| i.target.as_str())
        .collect();
    if !dns.is_empty() {
        notes.push(format!(
            "the resolver returned no address for {}",
            dns.join(", ")
        ));
    }
    let fake_ip = fake_ip_note(items);
    if !fake_ip.is_empty() {
        notes.push(fake_ip);
    }
    if !proxy.is_empty() {
        notes.push(proxy_note(proxy));
    }
    let other: Vec<&str> = failing
        .iter()
        .filter(|i| {
            !is_tls_status(&i.status) && i.status != "dns_failed" && i.status != UNSUPPORTED
        })
        .map(|i| i.target.as_str())
        .collect();
    if !other.is_empty() {
        // `other.len()`, not `failing.len()`: this sentence introduces the group
        // it lists, and a count of every failure beside a list of some of them
        // reads as a contradiction ("4 of 4 paths did not answer: <two hosts>"
        // was the real output on a box where the other two failed at TLS, which
        // the sentence above had already named).
        notes.push(format!(
            "{} of {} paths did not answer: {}",
            other.len(),
            items.len(),
            other.join(", ")
        ));
    }
    if notes.is_empty() {
        // Only `unsupported` items are failing: the hint_code already says so.
        return "no usable probe result".to_string();
    }
    notes.join("; ")
}

/// The fake-IP sentence, or nothing when the resolver answered with real
/// addresses.
///
/// Shared by both branches on purpose: a report from a resolver that answers on
/// behalf of a tunnel is still a report about that tunnel whether the paths came
/// back green or not — that tunnel is what decides whether the next connection
/// leaves the machine at all.
fn fake_ip_note(items: &[NetCheckItem]) -> String {
    if items.is_empty() || !items.iter().all(|i| i.fake_ip) {
        return String::new();
    }
    let addrs: Vec<String> = items
        .iter()
        .filter(|i| i.fake_ip)
        .flat_map(|i| i.addrs.iter().cloned())
        .collect();
    if addrs.is_empty() {
        return String::new();
    }
    format!(
        "every address came from a proxy fake-IP range ({}): the resolver answers \
         on behalf of a tunnel, and reachability depends on whether that tunnel \
         routes the connection",
        addrs.join(", ")
    )
}

/// The proxy sentence, or nothing when the process sees no proxy variable.
///
/// Returns the sentence WITHOUT a leading separator: [`hint_text`] joins its
/// notes with `"; "`, so a note that carried one of its own was printed as
/// "…answered; ; proxy variables…" on every failing report from a box that had
/// one set.
fn proxy_note(proxy: &[String]) -> String {
    if proxy.is_empty() {
        return String::new();
    }
    format!(
        "proxy variables are visible to this process ({}), so outbound traffic may be \
         going somewhere the trading stack never chose — the venue paths fail closed on \
         refused traffic rather than retrying around it",
        proxy.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, ok: bool, status: &str, addrs: &[&str]) -> NetCheckItem {
        NetCheckItem {
            name: name.into(),
            target: format!("{name}.example"),
            ok,
            status: status.into(),
            addrs: addrs.iter().map(|a| (*a).to_string()).collect(),
            fake_ip: !addrs.is_empty()
                && addrs
                    .iter()
                    .all(|a| a.parse::<IpAddr>().is_ok_and(|i| is_fake_ip(&i))),
            ms: 1,
            detail: String::new(),
        }
    }

    fn report(items: Vec<NetCheckItem>, proxy: &[&str]) -> NetCheckReport {
        finish(
            NetCheckReport {
                ok: false,
                ts_ms: 0,
                hint_code: String::new(),
                hint: String::new(),
                proxy_env: Vec::new(),
                items,
            },
            proxy.iter().map(|s| (*s).to_string()).collect(),
        )
    }

    // ── fake-IP recognition ────────────────────────────────────────────────

    #[test]
    fn fake_ip_ranges_are_recognised() {
        for addr in [
            "198.18.0.1",
            "198.18.255.254",
            "198.19.0.1",
            "fc00::1",
            "fd12:3456:789a::1",
            "::ffff:198.18.0.7",
        ] {
            let ip: IpAddr = addr.parse().expect("test literal parses");
            assert!(is_fake_ip(&ip), "{addr} is a fake-IP marker");
        }
    }

    #[test]
    fn real_addresses_are_not_fake_ips() {
        for addr in [
            "1.1.1.1",
            "104.18.32.7",
            "198.17.0.1",
            "198.20.0.1",
            "2606:4700::1111",
            "::1",
            "127.0.0.1",
        ] {
            let ip: IpAddr = addr.parse().expect("test literal parses");
            assert!(!is_fake_ip(&ip), "{addr} is a real address");
        }
    }

    // ── the report-level reading ───────────────────────────────────────────

    #[test]
    fn all_paths_answering_is_ok_and_says_so() {
        let r = report(
            vec![
                item("venue-rest", true, "ok", &["1.1.1.1"]),
                item("discovery", true, "ok", &["1.1.1.1"]),
            ],
            &[],
        );
        assert!(r.ok);
        assert_eq!(r.hint_code, "ok");
        assert!(r.hint.contains("all 2 network paths"));
    }

    #[test]
    fn a_green_report_with_a_visible_proxy_stays_ok_but_names_it() {
        let r = report(
            vec![item("venue-rest", true, "ok", &["1.1.1.1"])],
            &["HTTPS_PROXY"],
        );
        assert!(r.ok, "a proxy alone does not fail a reachable path");
        assert_eq!(r.hint_code, "ok");
        assert!(r.hint.contains("HTTPS_PROXY"), "{}", r.hint);
    }

    /// A green report from a tunneled resolver still says so. That tunnel is
    /// what decides whether the next connection leaves the machine at all, so it
    /// is context an operator should not have to rediscover from the failure it
    /// eventually causes.
    #[test]
    fn a_green_report_through_a_fake_ip_resolver_still_names_the_tunnel() {
        let r = report(
            vec![
                item("venue-rest", true, "ok", &["198.18.0.12"]),
                item("spot-ws", true, "ok", &["198.18.0.9"]),
            ],
            &[],
        );
        assert!(r.ok);
        assert_eq!(r.hint_code, "ok");
        assert!(r.hint.contains("fake-IP range"), "{}", r.hint);
        assert!(r.hint.contains("198.18.0.12"), "{}", r.hint);
    }

    /// One separator per note: joining is the caller's job, so a note must not
    /// arrive with one of its own ("…answered; ; proxy variables…").
    #[test]
    fn notes_are_joined_exactly_once() {
        let r = report(
            vec![item("venue-rest", false, "tcp_refused", &["1.1.1.1"])],
            &["ALL_PROXY"],
        );
        assert!(r.hint.contains("ALL_PROXY"), "{}", r.hint);
        assert!(!r.hint.contains("; ;"), "{}", r.hint);
        assert!(!r.hint.starts_with("; "), "{}", r.hint);
    }

    /// The priority that matters for this deployment: a completed TCP connect
    /// whose TLS dies is named as TLS, even though every address is a fake-IP
    /// marker and a proxy variable is set. Those two are context; the blocked
    /// handshake is the verdict.
    #[test]
    fn tls_failure_outranks_the_context_that_surrounds_it() {
        let r = report(
            vec![
                item("venue-rest", false, "tls_error", &["198.18.0.12"]),
                item("discovery", false, "tls_error", &["198.18.0.12"]),
            ],
            &["HTTPS_PROXY"],
        );
        assert!(!r.ok);
        assert_eq!(r.hint_code, "tls_blocked");
        assert!(r.hint.contains("TLS never completes"), "{}", r.hint);
        assert!(r.hint.contains("fake-IP"), "context is kept: {}", r.hint);
        assert!(
            r.hint.contains("HTTPS_PROXY"),
            "context is kept: {}",
            r.hint
        );
    }

    #[test]
    fn a_resolver_that_answers_nothing_is_dns_failed() {
        let r = report(
            vec![
                item("venue-rest", false, "dns_failed", &[]),
                item("discovery", false, "dns_failed", &[]),
            ],
            &[],
        );
        assert_eq!(r.hint_code, "dns_failed");
        assert!(r.hint.contains("no address"), "{}", r.hint);
    }

    #[test]
    fn a_refused_proxy_is_the_headline_when_nothing_else_explains_the_failure() {
        let r = report(
            vec![item("venue-rest", false, "tcp_refused", &["1.1.1.1"])],
            &["ALL_PROXY"],
        );
        assert_eq!(r.hint_code, "proxy_env");
        assert!(r.hint.contains("ALL_PROXY"), "{}", r.hint);
    }

    #[test]
    fn every_address_from_a_fake_range_is_reported_as_such() {
        let r = report(
            vec![item("spot-ws", false, "timeout", &["198.18.0.9"])],
            &[],
        );
        assert_eq!(r.hint_code, "fake_ip");
        assert!(r.hint.contains("fake-IP range"), "{}", r.hint);
    }

    #[test]
    fn a_mixed_failure_without_a_single_cause_is_partial() {
        let r = report(
            vec![
                item("venue-rest", false, "tcp_refused", &["1.1.1.1"]),
                item("discovery", true, "ok", &["1.1.1.1"]),
            ],
            &[],
        );
        assert_eq!(r.hint_code, "partial");
        assert!(r.hint.contains("1 of 2 paths"), "{}", r.hint);
    }

    /// The "did not answer" sentence counts the group it lists, not every
    /// failure. The live output that made this a test read "4 of 4 paths did not
    /// answer: clob.polymarket.com, gamma-api.polymarket.com" — the other two
    /// had failed at TLS and were already named by the sentence before, so the
    /// headline contradicted its own evidence.
    #[test]
    fn the_did_not_answer_count_matches_the_hosts_it_lists() {
        let r = report(
            vec![
                item("venue-rest", false, "transport_error", &["198.18.0.26"]),
                item("discovery", false, "transport_error", &["198.18.0.21"]),
                item("spot-ws", false, "tls_error", &["198.18.0.72"]),
                item("venue-ws", false, "tls_error", &["198.18.0.158"]),
            ],
            &[],
        );
        assert_eq!(r.hint_code, "tls_blocked");
        assert!(r.hint.contains("2 of 4 paths did not answer"), "{}", r.hint);
        assert!(!r.hint.contains("4 of 4 paths"), "{}", r.hint);
        // Both groups are still named — the count narrowed, the picture did not:
        // the TLS pair in the first sentence, the silent pair in the second.
        assert!(r.hint.contains("spot-ws.example"), "{}", r.hint);
        assert!(r.hint.contains("venue-rest.example"), "{}", r.hint);
    }

    /// A plugin that implements no probe must never read as a healthy one: the
    /// report is `ok=false` with `unsupported`, which is what the default trait
    /// method produces.
    #[test]
    fn an_unsupported_probe_is_never_a_green_light() {
        let r = report(vec![item("venue", false, "unsupported", &[])], &[]);
        assert!(!r.ok);
        assert_eq!(r.hint_code, "unsupported");
        let empty = report(Vec::new(), &[]);
        assert!(!empty.ok, "an empty report is not a passing one");
        assert_eq!(empty.hint_code, "unsupported");
    }

    #[test]
    fn the_proxy_scan_reports_names_only() {
        // The value is never read into the report; the function's contract is
        // "names", and this pins that a credential-bearing URL cannot leak.
        let names = proxy_env_names();
        for n in &names {
            assert!(
                PROXY_ENV_VARS.contains(&n.as_str()),
                "{n} is not one of the known variables"
            );
        }
    }
}
