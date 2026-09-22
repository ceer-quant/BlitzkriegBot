//! Network self-check (`net.check` / `--net-check` / `blitzkrieg net-check`).
//!
//! One probe per path this stack needs, walked in order — resolver → TCP → TLS →
//! one cheap request — so a failure names the stage that broke instead of
//! reporting "unreachable" and leaving an operator to guess between a poisoned
//! resolver, a blocked route and an intercepting middlebox.
//!
//! Read-only and credential-free by construction: no API key, no signature, no
//! order, no state. It is meant to be run while the bot trades — the moment an
//! operator actually needs it — and a diagnostic that can move money or mutate
//! venue state is one nobody can safely reach for.
//!
//! The four paths this plugin depends on at runtime:
//!
//!   * `venue-rest` — `CLOB_API_URL`: orderbook polling (the data it trades on),
//!     and in live mode order placement, the balance self-check and the sweep.
//!     One host carries most of the risk, which is why it is first.
//!   * `discovery`  — Gamma: how a round's markets are found at all.
//!   * `spot-ws`    — Binance spot: the momentum filter's reference price.
//!   * `venue-ws`   — `POLYMARKET_WS_URL`: the authenticated fill stream (live
//!     mode only, but a silent one is a silent ledger). Probed at the endpoint
//!     the SDK's client actually dials, not at the configured base — see
//!     [`user_stream_url`].
//!
//! Deliberately NOT probed: anything requiring credentials. A net check that
//! failed whenever a key was missing would be indistinguishable from one that
//! failed because the network was down — the exact confusion it exists to
//! remove. Authenticated reachability is the trading self-check's job.

use crate::venue::{
    DEFAULT_CLOB_URL, DEFAULT_WS_URL, GAMMA_URL, validate_venue_host, validate_ws_host,
};
use blitzkrieg_market_api::{
    NetCheckItem, NetCheckReport,
    net::{finish, is_fake_ip, now_ms, proxy_env_names},
};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

/// Binance spot: the momentum filter's reference price stream.
const BINANCE_SPOT_URL: &str = "wss://stream.binance.com:9443/ws/btcusdt@trade";

/// Per-stage budgets. Generous enough for a slow-but-working path, short enough
/// that the whole report is back before an operator gives up on it: the four
/// probes run concurrently, so the wall clock is one target's worst case.
const DNS_TIMEOUT: Duration = Duration::from_secs(3);
const TCP_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

/// The sweep transport impersonates a browser for the same reason the sweep
/// does: Cloudflare fronts the CLOB and treats unknown custom user agents
/// differently per endpoint class. A probe challenged where the sweep is not
/// would report a problem the bot does not have.
const PROBE_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

enum Kind {
    /// HTTP(S): TLS handshake, then one request.
    Http,
    /// WebSocket: the handshake itself is the proof (HTTP 101).
    Ws,
}

/// One path to probe, or the reason its URL was refused before it could become
/// a connection attempt.
enum Target {
    Probe {
        name: &'static str,
        url: String,
        kind: Kind,
    },
    Rejected {
        name: &'static str,
        raw: String,
        reason: String,
    },
}

impl Target {
    /// Only the tests read the name back out of a variant — `probe_one`
    /// destructures what it needs — but the row order is a contract worth
    /// pinning, so the accessor stays test-only instead of being duplicated.
    #[cfg(test)]
    fn name(&self) -> &'static str {
        match self {
            Target::Probe { name, .. } | Target::Rejected { name, .. } => name,
        }
    }
}

/// Probe every path, concurrently. Always answers with a report: a failed probe
/// is data, not an error, and no input makes this return something less useful
/// than "here is what each path did".
pub async fn probe() -> NetCheckReport {
    let targets = targets();
    // Concurrent, but the report keeps the target order: a reader comparing two
    // runs needs the rows in the same place even when one probe hangs.
    let mut set = tokio::task::JoinSet::new();
    for (i, t) in targets.into_iter().enumerate() {
        set.spawn(async move { (i, probe_one(&t).await) });
    }
    let mut slots: Vec<Option<NetCheckItem>> = (0..set.len()).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((i, item)) => slots[i] = Some(item),
            // A probe that panicked is a failed path, not a failed report.
            Err(e) => eprintln!("polymarket-extension: net probe task failed: {e}"),
        }
    }
    let items: Vec<NetCheckItem> = slots
        .into_iter()
        .enumerate()
        .map(|(i, slot)| {
            slot.unwrap_or_else(|| NetCheckItem {
                name: LABELS[i].0.to_string(),
                target: LABELS[i].0.to_string(),
                ok: false,
                status: "transport_error".to_string(),
                addrs: Vec::new(),
                fake_ip: false,
                ms: 0,
                detail: "probe task did not complete".to_string(),
            })
        })
        .collect();
    finish(
        NetCheckReport {
            ok: false,
            ts_ms: now_ms(),
            hint_code: String::new(),
            hint: String::new(),
            proxy_env: Vec::new(),
            items,
        },
        proxy_env_names(),
    )
}

/// Row order, so the placeholder above can name the path it stands for.
const LABELS: [(&str, &str); 4] = [
    ("venue-rest", "CLOB REST"),
    ("discovery", "Gamma discovery"),
    ("spot-ws", "Binance spot"),
    ("venue-ws", "CLOB user stream"),
];

/// The paths to probe, with the two env-configurable URLs validated before they
/// can become an outbound request.
///
/// A URL the venue itself would refuse is reported as `rejected` rather than
/// dialled: `CLOB_API_URL` and `POLYMARKET_WS_URL` come from the process
/// environment, so the probe obeys the same rule the venue does — an unvalidated
/// URL is a request to wherever the environment points.
fn targets() -> Vec<Target> {
    let clob = std::env::var("CLOB_API_URL").unwrap_or_else(|_| DEFAULT_CLOB_URL.to_string());
    let ws = std::env::var("POLYMARKET_WS_URL").unwrap_or_else(|_| DEFAULT_WS_URL.to_string());
    vec![
        env_target("venue-rest", clob, validate_venue_host, Kind::Http),
        Target::Probe {
            name: "discovery",
            url: GAMMA_URL.to_string(),
            kind: Kind::Http,
        },
        Target::Probe {
            name: "spot-ws",
            url: BINANCE_SPOT_URL.to_string(),
            kind: Kind::Ws,
        },
        user_stream_target(env_target("venue-ws", ws, validate_ws_host, Kind::Ws)),
    ]
}

/// Rewrite the `venue-ws` row to the endpoint the client dials.
///
/// Validation still decides whether this URL may be dialled at all: a rejected
/// row keeps its reason, and only an accepted one gets its path rewritten — the
/// host is the same host either way.
fn user_stream_target(t: Target) -> Target {
    match t {
        Target::Probe { name, url, kind } => Target::Probe {
            name,
            url: user_stream_url(&url),
            kind,
        },
        rejected => rejected,
    }
}

/// The SDK's own two steps, mirrored: strip a trailing channel suffix off the
/// base, then append the user channel's path (`normalize_base_endpoint` and
/// `channel_endpoint` in the SDK's `clob/ws/client.rs`).
///
/// `POLYMARKET_WS_URL` is a BASE — the client appends its channel path. A probe
/// against the base alone dials `/`, which the CLOB edge answers with a 404 from
/// its CDN: a red row that says nothing about the stream and everything about
/// the probe asking the wrong question. Measured against the real edge, the
/// difference is the whole verdict — `/` answers 404 while `/ws/user` completes
/// the `101` upgrade.
fn user_stream_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    let base = trimmed
        .strip_suffix("/ws/market")
        .or_else(|| trimmed.strip_suffix("/ws/user"))
        .or_else(|| trimmed.strip_suffix("/ws"))
        .unwrap_or(trimmed);
    format!("{base}/ws/user")
}

/// Either the validated URL, or a row that fails with the validator's own
/// reason — which already names the variable and the host it refused.
fn env_target(
    name: &'static str,
    raw: String,
    validate: fn(&str) -> anyhow::Result<String>,
    kind: Kind,
) -> Target {
    match validate(&raw) {
        Ok(url) => Target::Probe { name, url, kind },
        Err(e) => Target::Rejected {
            name,
            raw,
            reason: e.to_string(),
        },
    }
}

/// What the earlier stages learned about one path, so a late failure can be
/// reported WITH the stages that worked rather than as a bare "failed".
struct Trail {
    name: &'static str,
    target: String,
    addrs: Vec<String>,
    fake_ip: bool,
    notes: Vec<String>,
}

impl Trail {
    fn note(mut self, note: String) -> Self {
        self.notes.push(note);
        self
    }

    fn done(&self, ok: bool, status: &str, ms: i64, detail: String) -> NetCheckItem {
        let mut notes = self.notes.join(" · ");
        if !notes.is_empty() {
            notes.push_str(" · ");
        }
        NetCheckItem {
            name: self.name.to_string(),
            target: self.target.clone(),
            ok,
            status: status.to_string(),
            addrs: self.addrs.clone(),
            fake_ip: self.fake_ip,
            ms,
            detail: format!("{notes}{detail}"),
        }
    }
}

async fn probe_one(t: &Target) -> NetCheckItem {
    let (name, url, kind) = match t {
        Target::Probe { name, url, kind } => (*name, url.as_str(), kind),
        Target::Rejected { name, raw, reason } => {
            let trail = Trail {
                name,
                target: redact_userinfo(raw),
                addrs: Vec::new(),
                fake_ip: false,
                notes: Vec::new(),
            };
            return trail.done(false, "rejected", 0, reason.clone());
        }
    };

    let (host, port) = match host_port(url) {
        Some(hp) => hp,
        None => {
            let trail = Trail {
                name,
                target: redact_userinfo(url),
                addrs: Vec::new(),
                fake_ip: false,
                notes: Vec::new(),
            };
            return trail.done(
                false,
                "rejected",
                0,
                format!("no host readable out of {}", redact_userinfo(url)),
            );
        }
    };

    // ── 1. Resolver ─────────────────────────────────────────────────────────
    let dns_start = Instant::now();
    let resolved =
        tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host.as_str(), port))).await;
    let empty = Trail {
        name,
        target: host.clone(),
        addrs: Vec::new(),
        fake_ip: false,
        notes: Vec::new(),
    };
    let addrs: Vec<SocketAddr> = match resolved {
        Ok(Ok(it)) => it.collect(),
        Ok(Err(e)) => {
            let ms = dns_start.elapsed().as_millis() as i64;
            return empty.done(
                false,
                "dns_failed",
                ms,
                format!("resolver error after {ms}ms: {e}"),
            );
        }
        Err(_) => {
            let ms = dns_start.elapsed().as_millis() as i64;
            return empty.done(
                false,
                "dns_failed",
                ms,
                format!("resolver did not answer within {}s", DNS_TIMEOUT.as_secs()),
            );
        }
    };
    if addrs.is_empty() {
        return empty.done(
            false,
            "dns_failed",
            dns_start.elapsed().as_millis() as i64,
            "resolver returned no address".to_string(),
        );
    }
    let dns_ms = dns_start.elapsed().as_millis() as i64;
    let shown: Vec<String> = addrs.iter().map(|a| a.ip().to_string()).collect();
    let fake_ip = addrs.iter().all(|a| is_fake_ip(&a.ip()));
    let trail = Trail {
        name,
        target: host.clone(),
        addrs: shown,
        fake_ip,
        notes: Vec::new(),
    }
    .note(format!(
        "dns {}{} ({dns_ms}ms)",
        trail_addrs(addrs.as_slice()),
        if fake_ip { " — fake-IP range" } else { "" }
    ));

    // ── 2. TCP ──────────────────────────────────────────────────────────────
    // Every resolved address is tried: a name that answers with a fake-IP marker
    // and a real address at once is exactly the mixed case worth diagnosing, and
    // stopping at the first failure would hide which of the two is reachable.
    let tcp_start = Instant::now();
    let mut tcp_err = String::new();
    let mut timed_out = false;
    let mut connected = false;
    for addr in &addrs {
        match tokio::time::timeout(TCP_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                drop(stream);
                connected = true;
                break;
            }
            Ok(Err(e)) => tcp_err = format!("{addr}: {e}"),
            Err(_) => {
                tcp_err = format!("{addr}: no answer within {}s", TCP_TIMEOUT.as_secs());
                timed_out = true;
                break;
            }
        }
    }
    let tcp_ms = tcp_start.elapsed().as_millis() as i64;
    if !connected {
        let status = classify_transport(&tcp_err, timed_out);
        return trail.done(
            false,
            status,
            tcp_ms,
            format!("tcp failed after {tcp_ms}ms: {tcp_err}"),
        );
    }
    let trail = trail.note(format!("tcp {tcp_ms}ms"));

    // ── 3. TLS + one request ────────────────────────────────────────────────
    let req_start = Instant::now();
    let outcome = match kind {
        Kind::Http => http_probe(url).await,
        Kind::Ws => ws_probe(url).await,
    };
    let ms = req_start.elapsed().as_millis() as i64;
    match outcome {
        Ok(note) => trail.done(true, "ok", ms, format!("{note} in {ms}ms")),
        Err(e) => {
            let status = classify_transport(&e, false);
            trail.done(
                false,
                status,
                ms,
                format!("tls/http failed after {ms}ms: {e}"),
            )
        }
    }
}

fn trail_addrs(addrs: &[SocketAddr]) -> String {
    addrs
        .iter()
        .map(|a| a.ip().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// One GET. Any status below 500 counts as a working path: this probe answers
/// "is the path there", and a 403 or a 404 is the venue talking, not the network
/// failing. A 5xx is the edge or the origin reporting it cannot serve, which is
/// worth failing on.
async fn http_probe(url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(PROBE_UA)
        .build()
        .map_err(|e| format!("client build: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| transport_message(&e))?;
    let code = resp.status().as_u16();
    let (status, ok) = classify_http(code);
    if ok {
        Ok(format!("http {code}"))
    } else {
        Err(format!("http {code} ({status})"))
    }
}

/// The WebSocket handshake is the proof: a completed `101` means DNS, TCP, TLS
/// and the venue's edge all worked — the same stack the feed and the fill stream
/// use. The socket is dropped immediately and nothing is sent.
async fn ws_probe(url: &str) -> Result<String, String> {
    match tokio::time::timeout(REQUEST_TIMEOUT, tokio_tungstenite::connect_async(url)).await {
        Ok(Ok((stream, resp))) => {
            let code = resp.status().as_u16();
            drop(stream);
            Ok(format!("ws handshake {code}"))
        }
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!(
            "no handshake within {}s",
            REQUEST_TIMEOUT.as_secs()
        )),
    }
}

/// reqwest flattens its error chain into one message; keep it, and keep the
/// timeout flag distinct — a timeout and a broken handshake are different
/// diagnoses.
fn transport_message(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        format!("timed out: {e}")
    } else {
        e.to_string()
    }
}

/// Name the stage a transport error came from.
///
/// String matching, deliberately: the errors cross several TLS stacks (rustls
/// under reqwest, rustls under tungstenite, the SDK's own client) and the only
/// vocabulary they share is their message text. The sets stay narrow and are
/// tested against the messages this deployment has actually produced — a
/// misclassification here is a wrong diagnosis on screen.
fn classify_transport(msg: &str, timed_out: bool) -> &'static str {
    let m = msg.to_ascii_lowercase();
    if timed_out || m.contains("timed out") || m.contains("no answer") || m.contains("no handshake")
    {
        return "timeout";
    }
    if m.contains("certificate") || m.contains("unknown issuer") || m.contains("invalid peer") {
        return "tls_cert";
    }
    if m.contains("handshake")
        || m.contains("tls")
        || m.contains("eof")
        || m.contains("alert")
        || m.contains("connection closed")
    {
        return "tls_error";
    }
    if m.contains("refused") {
        return "tcp_refused";
    }
    "transport_error"
}

/// Any HTTP status a working edge can produce. See [`http_probe`].
fn classify_http(code: u16) -> (&'static str, bool) {
    if code >= 500 {
        ("http_error", false)
    } else {
        ("ok", true)
    }
}

/// The URL as a report may print it: userinfo removed.
///
/// The validators accept `https://user:pw@host` and hand the string back as
/// written, so a credential placed in `CLOB_API_URL` or `POLYMARKET_WS_URL`
/// would otherwise be printed by the overlay, the web card and the JSON CLI —
/// the three surfaces whose documented contract is that they never carry one.
/// Only the display is rewritten: the probe still dials exactly what the
/// environment configured, and `host_port` already reads its host past the `@`.
fn redact_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://{host}{tail}"),
        None => url.to_string(),
    }
}

/// `(host, port)` of a URL, with the scheme's default port when it names none.
fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme {
        "https" | "wss" => 443,
        "http" | "ws" => 80,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_part = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if let Some(rest) = host_part.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return Some((host.to_ascii_lowercase(), port));
    }
    match host_part.split_once(':') {
        Some((host, port)) => Some((host.to_ascii_lowercase(), port.parse().ok()?)),
        None if host_part.is_empty() => None,
        None => Some((host_part.to_ascii_lowercase(), default_port)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_reads_every_url_form_this_plugin_uses() {
        for (url, host, port) in [
            ("https://clob.polymarket.com", "clob.polymarket.com", 443),
            ("https://clob.polymarket.com/", "clob.polymarket.com", 443),
            (
                "https://gamma-api.polymarket.com",
                "gamma-api.polymarket.com",
                443,
            ),
            (
                "wss://ws-subscriptions-clob.polymarket.com",
                "ws-subscriptions-clob.polymarket.com",
                443,
            ),
            (
                "wss://stream.binance.com:9443/ws/btcusdt@trade",
                "stream.binance.com",
                9443,
            ),
            ("http://127.0.0.1:8080/base?x=1", "127.0.0.1", 8080),
            ("ws://[::1]:9000/x", "::1", 9000),
            ("https://user:pw@host.example/p", "host.example", 443),
        ] {
            assert_eq!(
                host_port(url),
                Some((host.to_string(), port)),
                "{url} must read as {host}:{port}"
            );
        }
        assert_eq!(host_port("ftp://host.example"), None, "only http(s)/ws(s)");
        assert_eq!(host_port("not a url"), None);
    }

    /// The messages this deployment has actually produced. `tls handshake eof`
    /// is the one the CLOB/Gamma outage writes; a report that called it
    /// `transport_error` would send an operator to the wrong layer.
    #[test]
    fn transport_errors_are_attributed_to_the_stage_that_produced_them() {
        for (msg, expected) in [
            ("tls handshake eof", "tls_error"),
            ("invalid peer certificate: UnknownIssuer", "tls_cert"),
            ("Connection refused (os error 61)", "tcp_refused"),
            ("198.18.0.12: no answer within 3s", "timeout"),
            ("operation timed out", "timeout"),
            ("connection closed via error", "tls_error"),
            ("something else entirely", "transport_error"),
        ] {
            assert_eq!(
                classify_transport(msg, false),
                expected,
                "{msg} must classify as {expected}"
            );
        }
        assert_eq!(
            classify_transport("whatever", true),
            "timeout",
            "the timeout flag outranks the message"
        );
    }

    /// A completed exchange is a working path whatever the venue said, except
    /// for the one class that means "cannot serve".
    #[test]
    fn http_status_is_a_path_verdict_not_a_venue_verdict() {
        for (code, status, ok) in [
            (200, "ok", true),
            (301, "ok", true),
            (403, "ok", true),
            (404, "ok", true),
            (429, "ok", true),
            (500, "http_error", false),
            (502, "http_error", false),
            (503, "http_error", false),
        ] {
            assert_eq!(classify_http(code), (status, ok), "{code}");
        }
    }

    /// The four paths this plugin depends on, each with its kind.
    #[test]
    fn targets_cover_the_paths_this_venue_needs() {
        let ts = targets();
        let names: Vec<&str> = ts.iter().map(Target::name).collect();
        assert_eq!(
            names,
            vec!["venue-rest", "discovery", "spot-ws", "venue-ws"]
        );
        assert_eq!(LABELS.len(), ts.len(), "the placeholder rows line up");
        match &ts[0] {
            Target::Probe { kind, .. } => assert!(matches!(kind, Kind::Http)),
            _ => panic!("the default CLOB URL must be accepted"),
        }
        match &ts[2] {
            Target::Probe { url, kind, .. } => {
                assert!(matches!(kind, Kind::Ws));
                assert_eq!(url, BINANCE_SPOT_URL);
            }
            _ => panic!("spot-ws is a fixed URL"),
        }
    }

    /// The probe asks the question the venue call asks: `POLYMARKET_WS_URL` is a
    /// base, and the user channel lives at `<base>/ws/user`. Suffix for suffix,
    /// this is the SDK's own rewriting — a base that arrives with a channel
    /// suffix already on it must not end up with two.
    #[test]
    fn the_user_stream_is_probed_at_the_sdk_dialled_path() {
        for (base, want) in [
            (
                "wss://ws-subscriptions-clob.polymarket.com",
                "wss://ws-subscriptions-clob.polymarket.com/ws/user",
            ),
            ("wss://h.example/", "wss://h.example/ws/user"),
            ("wss://h.example/ws", "wss://h.example/ws/user"),
            ("wss://h.example/ws/market", "wss://h.example/ws/user"),
            ("wss://h.example/ws/user", "wss://h.example/ws/user"),
            ("wss://user:pw@h.example", "wss://user:pw@h.example/ws/user"),
        ] {
            assert_eq!(user_stream_url(base), want, "{base}");
        }
    }

    /// …and the row the report prints is that endpoint, not the base.
    #[test]
    fn the_venue_ws_row_is_the_user_channel() {
        match &targets()[3] {
            Target::Probe { url, kind, .. } => {
                assert!(matches!(kind, Kind::Ws));
                assert_eq!(url, &user_stream_url(DEFAULT_WS_URL));
                assert!(url.ends_with("/ws/user"), "{url}");
            }
            _ => panic!("the default ws URL must be accepted"),
        }
    }

    /// The report's `target` never carries a credential, whatever the
    /// environment put in the URL.
    #[test]
    fn a_credentialed_url_is_printed_without_its_userinfo() {
        assert_eq!(
            redact_userinfo("https://user:pw@clob.example/base?x=1"),
            "https://clob.example/base?x=1"
        );
        assert_eq!(
            redact_userinfo("wss://token@ws.example"),
            "wss://ws.example"
        );
        assert_eq!(
            redact_userinfo("https://clob.example/"),
            "https://clob.example/",
            "a URL without userinfo is unchanged"
        );
        assert_eq!(
            redact_userinfo("user:pw@not-a-url"),
            "user:pw@not-a-url",
            "nothing is stripped from a string that has no scheme"
        );
    }

    /// An env URL that points at a private address never becomes a connection
    /// attempt: it becomes a `rejected` row carrying the validator's reason.
    #[test]
    fn a_private_env_url_is_reported_not_dialled() {
        let t = env_target(
            "venue-rest",
            "http://192.168.1.10:8080".to_string(),
            validate_venue_host,
            Kind::Http,
        );
        match t {
            Target::Rejected { reason, .. } => {
                assert!(reason.contains("private"), "{reason}");
            }
            Target::Probe { url, .. } => panic!("a private host must not be dialled: {url}"),
        }
    }

    /// Both env URLs go through a validator, and both defaults pass it.
    #[test]
    fn the_shipped_defaults_are_accepted_by_their_validators() {
        assert!(validate_venue_host(DEFAULT_CLOB_URL).is_ok());
        assert!(validate_ws_host(DEFAULT_WS_URL).is_ok());
    }
}
