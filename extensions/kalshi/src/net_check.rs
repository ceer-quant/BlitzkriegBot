//! Network self-check for the Kalshi plugin (#424).
//!
//! Same staged probe as the Polymarket extension (resolver → TCP → TLS/request,
//! read-only, credential-free, safe to run while trading): each path the plugin
//! depends on is probed and reported per stage so a failure names the stage
//! that broke.
//!
//! SSRF rule: a probe target can only exist as [`Target::Probe`] — and that
//! variant is constructed exclusively by [`env_target`], which runs
//! [`validate_probe_url`] first. A URL that fails validation becomes a
//! [`Target::Rejected`] row and is never turned into a request. The dial
//! function [`probe_one`] matches on the enum, so no code path exists where an
//! unvalidated string reaches the HTTP client.
//!
//! Paths:
//!   * `venue-rest` — `KALSHI_API_URL` (default the production REST root):
//!     orderbook polling, discovery, and in live mode orders + balance.
//!   * `auth-reachability` — deliberately NOT probed: a net check that fails
//!     whenever RSA credentials are missing would be indistinguishable from
//!     one failing because the network is down.

use crate::rest::PRODUCTION_BASE_URL;
use blitzkrieg_market_api::{
    NetCheckItem, NetCheckReport,
    net::{finish, is_fake_ip, now_ms, proxy_env_names},
};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

/// Per-stage budgets (same discipline as the Polymarket probe).
const DNS_TIMEOUT: Duration = Duration::from_secs(3);
const TCP_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

/// The paths this plugin depends on, in report order.
const LABELS: [&str; 1] = ["venue-rest"];

/// One path to probe, or the reason its URL was refused before it could
/// become a connection attempt. `Probe` is only constructed AFTER
/// [`validate_probe_url`] accepts the URL — see [`env_target`].
enum Target {
    Probe {
        name: &'static str,
        url: String,
    },
    Rejected {
        name: &'static str,
        raw: String,
        reason: String,
    },
}

/// Acceptance rule for a probe target: http(s) scheme, public non-reserved
/// host. Mirrors the Polymarket extension's `validate_host` (reserved names +
/// IPv4 private/loopback/link-local blocks + IPv6 loopback/ULA/link-local/
/// NAT64). A rejected URL becomes a report row, never a dial.
pub(crate) fn validate_probe_url(raw: &str) -> Result<String, String> {
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| format!("KALSHI_API_URL is not a URL: {raw}"))?;
    if !matches!(scheme, "https" | "http" | "wss" | "ws") {
        return Err(format!(
            "KALSHI_API_URL must be http(s)/ws(s), got scheme {scheme:?}"
        ));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_part = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = if let Some(inner) = host_part.strip_prefix('[') {
        inner
            .split_once(']')
            .map(|(h, _)| h)
            .unwrap_or(inner)
            .to_ascii_lowercase()
    } else {
        host_part
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    if host.is_empty() {
        return Err("KALSHI_API_URL has no host".to_string());
    }
    let reserved_name = host == "localhost"
        || host == "local"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".internal.invalid");
    if reserved_name {
        return Err(format!("KALSHI_API_URL points at a reserved host: {host}"));
    }
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        let o = ip.octets();
        let blocked = o[0] == 0
            || o[0] == 10
            || o[0] == 127
            || (o[0] == 100 && (64..=127).contains(&o[1]))
            || (o[0] == 169 && o[1] == 254)
            || (o[0] == 172 && (16..=31).contains(&o[1]))
            || (o[0] == 192 && o[1] == 168)
            || o[0] >= 224;
        if blocked {
            return Err(format!(
                "KALSHI_API_URL points at a loopback/private/reserved address: {ip}"
            ));
        }
    }
    if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
        let s = ip.segments();
        let blocked = s == [0, 0, 0, 0, 0, 0, 0, 1]
            || s == [0, 0, 0, 0, 0, 0, 0, 0]
            || (s[0] & 0xfe00) == 0xfc00
            || (s[0] & 0xffc0) == 0xfe80
            || s[..6] == [0, 0, 0, 0, 0, 0xffff];
        if blocked {
            return Err(format!(
                "KALSHI_API_URL points at a loopback/link-local address: {ip}"
            ));
        }
    }
    Ok(raw.to_owned())
}

/// The ONLY constructor of [`Target::Probe`]: the URL is validated here, and a
/// failure produces [`Target::Rejected`] instead — no caller can bypass it
/// because `Target`'s fields are private to this module.
fn env_target(name: &'static str, raw: String) -> Target {
    match validate_probe_url(&raw) {
        Ok(url) => Target::Probe { name, url },
        Err(reason) => Target::Rejected { name, raw, reason },
    }
}

/// Probe every path, concurrently, in report order.
pub async fn probe() -> NetCheckReport {
    let raw = std::env::var("KALSHI_API_URL").unwrap_or_else(|_| PRODUCTION_BASE_URL.to_string());
    let target = env_target(LABELS[0], raw);
    let mut set = tokio::task::JoinSet::new();
    set.spawn(async move { probe_one(&target).await });
    let mut slots: Vec<Option<NetCheckItem>> = (0..LABELS.len()).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(item) => slots[0] = Some(item),
            Err(e) => eprintln!("kalshi-extension: net probe task failed: {e}"),
        }
    }
    let items: Vec<NetCheckItem> = slots
        .into_iter()
        .enumerate()
        .map(|(i, slot)| {
            slot.unwrap_or_else(|| NetCheckItem {
                name: LABELS[i].to_string(),
                target: LABELS[i].to_string(),
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

/// The URL as a report may print it: userinfo removed.
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
    if let Some(inner) = host_part.strip_prefix('[') {
        let (host, tail) = inner.split_once(']')?;
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

/// Name the stage a transport error came from (same vocabulary as the
/// Polymarket probe — these are the messages this deployment produces).
fn classify_transport(msg: &str, timed_out: bool) -> &'static str {
    let m = msg.to_ascii_lowercase();
    if timed_out || m.contains("timed out") || m.contains("no answer") {
        return "timeout";
    }
    if m.contains("certificate") || m.contains("unknown issuer") || m.contains("invalid peer") {
        return "tls_cert";
    }
    if m.contains("handshake") || m.contains("tls") || m.contains("eof") || m.contains("alert") {
        return "tls_error";
    }
    if m.contains("refused") {
        return "tcp_refused";
    }
    "transport_error"
}

/// One staged probe: resolver → TCP → TLS + one cheap request. Receives only
/// a post-validation [`Target`].
async fn probe_one(t: &Target) -> NetCheckItem {
    let (name, url) = match t {
        Target::Probe { name, url } => (*name, url.as_str()),
        Target::Rejected { name, raw, reason } => {
            return NetCheckItem {
                name: (*name).to_string(),
                target: redact_userinfo(raw),
                ok: false,
                status: "rejected".to_string(),
                addrs: Vec::new(),
                fake_ip: false,
                ms: 0,
                detail: reason.clone(),
            };
        }
    };
    let target = redact_userinfo(url);
    let Some((host, port)) = host_port(url) else {
        let detail = format!("no host readable out of {target}");
        return NetCheckItem {
            name: name.to_string(),
            target,
            ok: false,
            status: "rejected".to_string(),
            addrs: Vec::new(),
            fake_ip: false,
            ms: 0,
            detail,
        };
    };

    // ── 1. Resolver ─────────────────────────────────────────────────────────
    // `host` is cloned up front: the resolver borrows it only for the lookup,
    // and every later row builder takes ownership.
    let host_str = host.as_str().to_owned();
    let dns_start = Instant::now();
    let resolved = tokio::time::timeout(
        DNS_TIMEOUT,
        tokio::net::lookup_host((host_str.as_str(), port)),
    )
    .await;
    let addrs: Vec<SocketAddr> = match resolved {
        Ok(Ok(it)) => it.collect(),
        Ok(Err(e)) => {
            return item(
                name,
                host,
                false,
                "dns_failed",
                Vec::new(),
                false,
                dns_start.elapsed().as_millis() as i64,
                format!("resolver error: {e}"),
            );
        }
        Err(_) => {
            return item(
                name,
                host,
                false,
                "dns_failed",
                Vec::new(),
                false,
                dns_start.elapsed().as_millis() as i64,
                format!("resolver did not answer within {}s", DNS_TIMEOUT.as_secs()),
            );
        }
    };
    if addrs.is_empty() {
        return item(
            name,
            host,
            false,
            "dns_failed",
            Vec::new(),
            false,
            dns_start.elapsed().as_millis() as i64,
            "resolver returned no address".to_string(),
        );
    }
    let dns_ms = dns_start.elapsed().as_millis() as i64;
    let shown: Vec<String> = addrs.iter().map(|a| a.ip().to_string()).collect();
    let fake_ip = addrs.iter().all(|a| is_fake_ip(&a.ip()));

    // ── 2. TCP ──────────────────────────────────────────────────────────────
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
        return item(
            name,
            host,
            false,
            status,
            shown,
            fake_ip,
            tcp_ms,
            format!("tcp failed after {tcp_ms}ms: {tcp_err}"),
        );
    }

    // ── 3. TLS + one request ────────────────────────────────────────────────
    let req_start = Instant::now();
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return item(
                name,
                host,
                false,
                "transport_error",
                shown,
                fake_ip,
                req_start.elapsed().as_millis() as i64,
                format!("client build: {e}"),
            );
        }
    };
    let outcome = client.get(url).send().await;
    let ms = req_start.elapsed().as_millis() as i64;
    match outcome {
        Ok(resp) => {
            let code = resp.status().as_u16();
            // Any status below 500 is the venue talking, not the network
            // failing; a 5xx is the edge reporting it cannot serve.
            let (status, ok) = if code >= 500 {
                ("http_error", false)
            } else {
                ("ok", true)
            };
            item(
                name,
                host,
                ok,
                status,
                shown,
                fake_ip,
                ms,
                format!("http {code} in {ms}ms (dns {dns_ms}ms)"),
            )
        }
        Err(e) => {
            let timed_out = e.is_timeout();
            let status = classify_transport(&e.to_string(), timed_out);
            item(
                name,
                host,
                false,
                status,
                shown,
                fake_ip,
                ms,
                format!("tls/http failed after {ms}ms: {e}"),
            )
        }
    }
}

/// Assemble one report row.
#[allow(clippy::too_many_arguments)]
fn item(
    name: &str,
    target: String,
    ok: bool,
    status: &str,
    addrs: Vec<String>,
    fake_ip: bool,
    ms: i64,
    detail: String,
) -> NetCheckItem {
    NetCheckItem {
        name: name.to_string(),
        target,
        ok,
        status: status.to_string(),
        addrs,
        fake_ip,
        ms,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_reads_the_forms_this_plugin_uses() {
        assert_eq!(
            host_port("https://api.elections.kalshi.com"),
            Some(("api.elections.kalshi.com".to_string(), 443))
        );
        assert_eq!(host_port("ftp://host.example"), None, "only http(s)/ws(s)");
        assert_eq!(host_port("not a url"), None);
    }

    #[test]
    fn userinfo_is_redacted_from_reported_targets() {
        assert_eq!(
            redact_userinfo("https://user:pw@api.example.com/trade-api"),
            "https://api.example.com/trade-api"
        );
        assert_eq!(
            redact_userinfo("https://api.example.com"),
            "https://api.example.com"
        );
    }

    #[test]
    fn transport_errors_name_their_stage() {
        assert_eq!(classify_transport("tls handshake eof", false), "tls_error");
        assert_eq!(
            classify_transport("invalid peer certificate: UnknownIssuer", false),
            "tls_cert"
        );
        assert_eq!(
            classify_transport("Connection refused (os error 61)", false),
            "tcp_refused"
        );
        assert_eq!(
            classify_transport("198.18.0.12: no answer within 3s", false),
            "timeout"
        );
        assert_eq!(classify_transport("whatever", true), "timeout");
    }

    /// The SSRF gate: a probe target the venue would refuse never becomes a
    /// dial. Reserved names, loopback/private IPv4 and IPv6 all rejected —
    /// and through `env_target` they materialize as `Rejected`, not `Probe`.
    #[test]
    fn probe_urls_must_be_public_hosts() {
        for (raw, why) in [
            ("http://127.0.0.1:8080", "loopback v4"),
            ("http://localhost:9000", "loopback name"),
            ("http://10.1.2.3", "private v4"),
            ("http://192.168.1.1", "private v4"),
            ("http://172.20.0.5", "private v4"),
            ("http://169.254.1.1", "link-local"),
            ("http://[::1]:9000", "loopback v6"),
            ("http://[fe80::1]", "link-local v6"),
            ("http://[fd00::1]", "ula v6"),
            ("file:///etc/passwd", "not http(s)"),
            ("https://metadata.internal", "reserved name"),
            ("https://host.example.local", "reserved suffix"),
        ] {
            assert!(
                validate_probe_url(raw).is_err(),
                "{why}: {raw} must be rejected"
            );
            assert!(
                matches!(
                    env_target("venue-rest", raw.to_string()),
                    Target::Rejected { .. }
                ),
                "{why}: {raw} must not become a probe target"
            );
        }
        for raw in [
            "https://api.elections.kalshi.com",
            "https://api.elections.kalshi.com/trade-api/v2",
        ] {
            assert!(validate_probe_url(raw).is_ok(), "{raw} must be accepted");
        }
    }
}
