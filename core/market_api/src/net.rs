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
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::OnceLock;
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
    // 28.0.0.0/8 — the alternate range Clash/mihomo document as `fake-ip-range`
    // (the block the owner's resolver returned after Clash was stopped).
    o[0] == 28 || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
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

/// Variables whose values this process rewrites for `BK_PROXY=none|<url>`.
/// `BK_PROXY` itself is never cleared: it is the operator's instruction.
const PROXY_ASSIGN_VARS: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

/// How this process dials. `BK_PROXY` selects it; unset is [`ProxyMode::Auto`].
#[derive(Clone, PartialEq, Eq)]
pub enum ProxyMode {
    /// Honor `HTTPS_PROXY` / `ALL_PROXY` (and the lowercase names), same as reqwest.
    Auto,
    /// Ignore every proxy variable and dial direct.
    Direct,
    /// Use this URL for HTTP and HTTPS. May contain userinfo; never log it.
    Explicit(String),
}

impl fmt::Debug for ProxyMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("Auto"),
            Self::Direct => f.write_str("Direct"),
            Self::Explicit(_) => f.write_str("Explicit(<redacted>)"),
        }
    }
}

impl ProxyMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Direct => "none",
            Self::Explicit(_) => "explicit",
        }
    }
}

/// `BK_PROXY=auto|none|off|direct|<url>`. An empty value is `auto`.
///
/// The error never echoes `raw`: a URL can carry a password.
pub fn parse_proxy_mode(raw: Option<&str>) -> Result<ProxyMode, String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(ProxyMode::Auto);
    };
    if raw.eq_ignore_ascii_case("auto") {
        return Ok(ProxyMode::Auto);
    }
    if raw.eq_ignore_ascii_case("none")
        || raw.eq_ignore_ascii_case("off")
        || raw.eq_ignore_ascii_case("direct")
    {
        return Ok(ProxyMode::Direct);
    }
    if parse_proxy_endpoint(raw).is_err() {
        return Err(
            "BK_PROXY must be auto, none, off, direct, or an http/https/socks5/socks5h URL \
             (the value is not shown)"
                .to_string(),
        );
    }
    Ok(ProxyMode::Explicit(raw.to_string()))
}

/// A proxy endpoint safe to print: `scheme://host:port`, never userinfo.
#[derive(Clone)]
pub struct ProxyEndpoint {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    username: String,
    password: String,
}

impl fmt::Debug for ProxyEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyEndpoint")
            .field("display", &self.display())
            .finish()
    }
}

impl ProxyEndpoint {
    /// `scheme://host:port` with IPv6 hosts bracketed. No userinfo.
    pub fn display(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{}://{host}:{}", self.scheme, self.port)
    }

    /// Address `TcpStream::connect` accepts.
    pub fn dial_addr(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Username and password, when the URL had userinfo.
    pub fn basic_userinfo(&self) -> Option<(&str, &str)> {
        if self.username.is_empty() && self.password.is_empty() {
            None
        } else {
            Some((self.username.as_str(), self.password.as_str()))
        }
    }
}

/// What a reqwest builder should do. `Explicit`'s URL may contain credentials.
#[derive(Clone)]
pub enum ClientProxy {
    FromEnvironment,
    Disable,
    Explicit(String),
}

impl fmt::Debug for ClientProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FromEnvironment => f.write_str("FromEnvironment"),
            Self::Disable => f.write_str("Disable"),
            Self::Explicit(_) => f.write_str("Explicit(<redacted>)"),
        }
    }
}

/// The proxy decision for one process, with secrets kept off [`Debug`].
#[derive(Clone)]
pub struct ProxyView {
    pub policy: &'static str,
    /// Variable NAMES that were set, plus `BK_PROXY` when the operator set it.
    pub names: Vec<String>,
    /// The proxy the environment (or `BK_PROXY=<url>`) named. Redacted.
    pub configured: Option<ProxyEndpoint>,
    /// The proxy REST and our WebSocket stack will actually dial.
    pub active: Option<ProxyEndpoint>,
    /// A proxy value was present and was not a supported URL.
    pub invalid: bool,
    explicit_url: Option<String>,
}

impl fmt::Debug for ProxyView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyView")
            .field("policy", &self.policy)
            .field("names", &self.names)
            .field("configured", &self.configured.as_ref().map(|e| e.display()))
            .field("active", &self.active.as_ref().map(|e| e.display()))
            .field("invalid", &self.invalid)
            .finish()
    }
}

impl ProxyView {
    pub fn client_proxy(&self) -> ClientProxy {
        if self.invalid || self.policy == "none" || self.active.is_none() {
            // `auto` with no proxy selected keeps reqwest's own env lookup,
            // which is a no-op when nothing is set and matches NO_PROXY=*.
            if self.policy == "auto" && !self.invalid && self.active.is_none() {
                return ClientProxy::FromEnvironment;
            }
            return ClientProxy::Disable;
        }
        if let Some(url) = &self.explicit_url {
            return ClientProxy::Explicit(url.clone());
        }
        ClientProxy::FromEnvironment
    }
}

/// Resolve `mode` against an explicit environment list (tests pass this; the
/// process path reads the real environment).
pub fn resolve_proxy_with(mode: &ProxyMode, env: &[(&str, &str)]) -> Result<ProxyView, String> {
    let mut names: Vec<String> = PROXY_ENV_VARS
        .iter()
        .filter(|name| env_value(env, name).is_some())
        .map(|name| (*name).to_string())
        .collect();
    if env_value(env, "BK_PROXY").is_some() && !names.iter().any(|n| n == "BK_PROXY") {
        names.push("BK_PROXY".to_string());
    }
    let bypass = no_proxy_star(env);
    let (from_env, env_invalid) = https_proxy_from_env(env);
    let (configured, invalid, explicit_url, active) = match mode {
        ProxyMode::Explicit(url) => {
            let ep = parse_proxy_endpoint(url).map_err(|_| {
                "BK_PROXY must be auto, none, off, direct, or an http/https/socks5/socks5h URL \
                 (the value is not shown)"
                    .to_string()
            })?;
            (Some(ep.clone()), false, Some(url.clone()), Some(ep))
        }
        ProxyMode::Direct => (from_env, env_invalid, None, None),
        ProxyMode::Auto => {
            let active = if bypass { None } else { from_env.clone() };
            (from_env, env_invalid, None, active)
        }
    };
    Ok(ProxyView {
        policy: mode.label(),
        names,
        configured,
        active,
        invalid,
        explicit_url,
    })
}

fn env_value<'a>(env: &'a [(&str, &str)], name: &str) -> Option<&'a str> {
    env.iter().find_map(|(k, v)| {
        if !k.eq_ignore_ascii_case(name) {
            return None;
        }
        let t = v.trim();
        if t.is_empty() { None } else { Some(t) }
    })
}

fn first_env<'a>(env: &'a [(&str, &str)], keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| env_value(env, k))
}

fn no_proxy_star(env: &[(&str, &str)]) -> bool {
    first_env(env, &["NO_PROXY", "no_proxy"]).is_some_and(|v| v.split(',').any(|p| p.trim() == "*"))
}

/// The proxy reqwest would use for an `https://` URL: `HTTPS_PROXY`, then
/// `https_proxy`, then `ALL_PROXY`, then `all_proxy`. `HTTP_PROXY` alone does
/// not intercept HTTPS. An unparseable value is skipped the way hyper-util
/// skips it, and reported as `invalid` when nothing usable remains.
fn https_proxy_from_env(env: &[(&str, &str)]) -> (Option<ProxyEndpoint>, bool) {
    let https = first_env(env, &["HTTPS_PROXY", "https_proxy"]);
    let all = first_env(env, &["ALL_PROXY", "all_proxy"]);
    match parse_optional(https) {
        Some(Ok(ep)) => (Some(ep), false),
        Some(Err(_)) => match parse_optional(all) {
            Some(Ok(ep)) => (Some(ep), false),
            Some(Err(_)) => (None, true),
            None => (None, true),
        },
        None => match parse_optional(all) {
            Some(Ok(ep)) => (Some(ep), false),
            Some(Err(_)) => (None, true),
            None => (None, false),
        },
    }
}

fn parse_optional(raw: Option<&str>) -> Option<Result<ProxyEndpoint, &'static str>> {
    raw.map(parse_proxy_endpoint)
}

pub fn parse_proxy_endpoint(raw: &str) -> Result<ProxyEndpoint, &'static str> {
    let raw = raw.trim();
    let Some((scheme, rest)) = raw.split_once("://") else {
        return Err("proxy URL has no scheme");
    };
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "socks5" | "socks5h") {
        return Err("proxy scheme is not http, https, socks5, or socks5h");
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err("proxy URL has no host");
    }
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, authority),
    };
    if hostport.is_empty() {
        return Err("proxy URL has no host");
    }
    let (username, password) = match userinfo {
        Some(u) => {
            let (user, pass) = u.split_once(':').unwrap_or((u, ""));
            (percent_decode(user), percent_decode(pass))
        }
        None => (String::new(), String::new()),
    };
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => 1080,
    };
    let (host, port) = split_host_port(hostport, default_port)?;
    if host.is_empty() {
        return Err("proxy URL has no host");
    }
    Ok(ProxyEndpoint {
        scheme,
        host,
        port,
        username,
        password,
    })
}

fn split_host_port(hostport: &str, default_port: u16) -> Result<(String, u16), &'static str> {
    if let Some(rest) = hostport.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            return Err("proxy URL has a bad IPv6 host");
        };
        if host.is_empty() {
            return Err("proxy URL has no host");
        }
        let port = if let Some(p) = tail.strip_prefix(':') {
            if p.is_empty() {
                return Err("proxy URL has an empty port");
            }
            p.parse().map_err(|_| "proxy URL has a bad port")?
        } else if tail.is_empty() {
            default_port
        } else {
            return Err("proxy URL has a bad IPv6 host");
        };
        return Ok((host.to_string(), port));
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !host.contains(':') && !port.is_empty() => Ok((
            host.to_string(),
            port.parse().map_err(|_| "proxy URL has a bad port")?,
        )),
        Some(_) => Err("proxy URL has a bad host"),
        None => Ok((hostport.to_string(), default_port)),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One line for the boot log. `None` when nothing about proxies is worth saying.
/// Never contains a credential.
pub fn boot_line(view: &ProxyView) -> Option<String> {
    if view.policy == "auto"
        && view.names.is_empty()
        && !view.invalid
        && view.configured.is_none()
        && view.active.is_none()
    {
        return None;
    }
    let proxy = view
        .active
        .as_ref()
        .or(view.configured.as_ref())
        .map(|e| e.display())
        .unwrap_or_else(|| "none".to_string());
    let use_ = if view.invalid {
        "rejected URL, dialing direct"
    } else if view.active.is_some() {
        "in use"
    } else if view.policy == "none" {
        "ignored, dialing direct"
    } else {
        "not used for https"
    };
    let names = if view.names.is_empty() {
        "none".to_string()
    } else {
        view.names.join(", ")
    };
    Some(format!(
        "BK_PROXY={policy}: proxy {proxy} ({use_}); variables: {names} (names only)",
        policy = view.policy
    ))
}

static PROXY_SNAPSHOT: OnceLock<ProxyView> = OnceLock::new();

fn process_env() -> Vec<(String, String)> {
    let mut keys: Vec<&str> = PROXY_ENV_VARS.to_vec();
    keys.push("BK_PROXY");
    keys.iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(|v| ((*name).to_string(), v))
        })
        .collect()
}

fn resolve_process() -> Result<ProxyView, String> {
    let owned = process_env();
    let pairs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mode = parse_proxy_mode(env_value(&pairs, "BK_PROXY"))?;
    resolve_proxy_with(&mode, &pairs)
}

/// Read the proxy decision. After [`install_process_proxy_policy`] this is the
/// snapshot taken BEFORE env vars were rewritten, so a report can still name
/// the dead proxy `BK_PROXY=none` cleared.
pub fn proxy_view() -> Result<ProxyView, String> {
    if let Some(view) = PROXY_SNAPSHOT.get() {
        return Ok(view.clone());
    }
    resolve_process()
}

pub fn client_proxy() -> Result<ClientProxy, String> {
    Ok(proxy_view()?.client_proxy())
}

/// Apply `BK_PROXY` to the process environment so clients we do not build
/// (the venue SDK's reqwest) follow the same decision. Idempotent.
///
/// `none` removes `HTTP(S)_PROXY` / `ALL_PROXY`. An explicit URL is written
/// into those variables and is never logged. `auto` leaves the environment
/// alone.
pub fn install_process_proxy_policy() -> Result<(), String> {
    if PROXY_SNAPSHOT.get().is_some() {
        return Ok(());
    }
    let view = resolve_process()?;
    match view.policy {
        "none" => {
            for name in PROXY_ASSIGN_VARS {
                // SAFETY: one call from process startup, before other threads
                // read the proxy environment. The OnceLock above makes a
                // second call return without mutating. Nothing here is logged.
                unsafe { std::env::remove_var(name) };
            }
        }
        "explicit" => {
            let url = view
                .explicit_url
                .clone()
                .ok_or_else(|| "BK_PROXY explicit URL missing".to_string())?;
            for name in PROXY_ASSIGN_VARS {
                // SAFETY: same single-threaded startup window as `remove_var`.
                // `url` is the operator's BK_PROXY value and is not logged.
                unsafe { std::env::set_var(name, &url) };
            }
        }
        _ => {}
    }
    let _ = PROXY_SNAPSHOT.set(view);
    Ok(())
}

/// `Display` plus each `source()`, with `user:password@` stripped from any URL.
pub fn error_chain(err: &dyn std::error::Error) -> String {
    let mut parts = Vec::new();
    let mut cur = Some(err);
    let mut guard = 0;
    while let Some(e) = cur {
        if guard >= 8 {
            break;
        }
        let text = redact_secrets(&e.to_string());
        if parts.last() != Some(&text) {
            parts.push(text);
        }
        cur = e.source();
        guard += 1;
    }
    parts.join(": ")
}

/// Strip `scheme://userinfo@` down to `scheme://host` anywhere in `input`.
pub fn redact_secrets(input: &str) -> String {
    let mut rest = input;
    let mut out = String::with_capacity(input.len());
    while let Some(idx) = rest.find("://") {
        out.push_str(&rest[..idx + 3]);
        rest = &rest[idx + 3..];
        let end = rest
            .find(|c: char| {
                c.is_whitespace()
                    || matches!(
                        c,
                        '/' | '?' | '#' | ')' | ']' | '"' | '\'' | ',' | ';' | '>'
                    )
            })
            .unwrap_or(rest.len());
        let authority = &rest[..end];
        match authority.rsplit_once('@') {
            Some((_, host)) => out.push_str(host),
            None => out.push_str(authority),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Facts the report's hint is allowed to print. Built by [`annotate`].
#[derive(Clone, Debug)]
pub struct ProxyObservation {
    /// `false` for the historical `finish` path (no proxy TCP probe).
    pub probed: bool,
    pub policy: &'static str,
    pub display: String,
    pub in_use: bool,
    pub invalid: bool,
    pub tcp_error: String,
    pub tcp_ms: i64,
}

impl ProxyObservation {
    pub fn legacy() -> Self {
        Self {
            probed: false,
            policy: "auto",
            display: String::new(),
            in_use: false,
            invalid: false,
            tcp_error: String::new(),
            tcp_ms: 0,
        }
    }
}

/// Status of a proxy that is configured and not accepting TCP.
pub const PROXY_UNREACHABLE: &str = "proxy_unreachable";

/// Turn a TCP probe of the configured proxy (`None` when there was nothing to
/// dial) into the hint observation and, when there is something to say, a
/// `proxy` row. The row's `target` is the redacted endpoint.
pub fn annotate(
    view: &ProxyView,
    dial: Option<Result<i64, String>>,
) -> (ProxyObservation, Option<NetCheckItem>) {
    let (tcp_error, tcp_ms) = match dial {
        Some(Ok(ms)) => (String::new(), ms),
        Some(Err(e)) => (redact_secrets(&e), 0),
        None => (String::new(), 0),
    };
    let display = if view.active.is_some() {
        view.active
            .as_ref()
            .map(|e| e.display())
            .unwrap_or_default()
    } else {
        view.configured
            .as_ref()
            .map(|e| e.display())
            .unwrap_or_default()
    };
    let obs = ProxyObservation {
        probed: true,
        policy: view.policy,
        display,
        in_use: view.active.is_some() && !view.invalid,
        invalid: view.invalid,
        tcp_error,
        tcp_ms,
    };
    let item = proxy_row(&obs, &view.names);
    (obs, item)
}

fn proxy_row(obs: &ProxyObservation, names: &[String]) -> Option<NetCheckItem> {
    if obs.display.is_empty() && !obs.invalid {
        return None;
    }
    let vars = if names.is_empty() {
        "the proxy variables".to_string()
    } else {
        names.join(", ")
    };
    let (ok, status, detail) = if obs.invalid {
        (
            true,
            "ok",
            "a proxy value is not an http, https, socks5, or socks5h URL; the value is not shown. REST is dialing direct".to_string(),
        )
    } else if obs.in_use && !obs.tcp_error.is_empty() {
        (
            false,
            PROXY_UNREACHABLE,
            format!(
                "proxy configured but unreachable at {} ({}). reqwest dials it before any venue I/O, so REST fails in about 0ms with only \"error sending request\" — the refusal is on the error source chain. Set BK_PROXY=none, or unset {vars}",
                obs.display, obs.tcp_error
            ),
        )
    } else if obs.in_use {
        (
            true,
            "ok",
            format!("proxy reachable at {} (tcp {}ms)", obs.display, obs.tcp_ms),
        )
    } else if obs.policy == "none" {
        let extra = if obs.tcp_error.is_empty() {
            String::new()
        } else {
            format!(
                "; {} is not listening ({}) and is not used",
                obs.display, obs.tcp_error
            )
        };
        (
            true,
            "ok",
            format!("BK_PROXY=none dials direct and ignores {vars}{extra}"),
        )
    } else {
        (
            true,
            "ok",
            format!("proxy {} is not used for https", obs.display),
        )
    };
    Some(NetCheckItem {
        name: "proxy".to_string(),
        target: if obs.display.is_empty() {
            "proxy".to_string()
        } else {
            obs.display.clone()
        },
        ok,
        status: status.to_string(),
        addrs: Vec::new(),
        fake_ip: false,
        ms: obs.tcp_ms,
        detail,
    })
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
pub fn finish(report: NetCheckReport, proxy: Vec<String>) -> NetCheckReport {
    finish_with(report, proxy, &ProxyObservation::legacy())
}

/// [`finish`] plus the proxy TCP probe. The `proxy` row is not a venue path:
/// fake-IP and "N of M" counts ignore it, but a failing proxy row still fails
/// the report.
pub fn finish_with(
    mut report: NetCheckReport,
    proxy: Vec<String>,
    obs: &ProxyObservation,
) -> NetCheckReport {
    let venue = venue_items(&report.items);
    report.ok = !report.items.is_empty() && report.items.iter().all(|i| i.ok);
    let failing_all: Vec<&NetCheckItem> = report.items.iter().filter(|i| !i.ok).collect();
    let failing_venue: Vec<&NetCheckItem> = venue.iter().copied().filter(|i| !i.ok).collect();
    report.hint_code = hint_code(&venue, &failing_all, &failing_venue, &proxy, obs).to_string();
    report.hint = hint_text(&venue, &failing_all, &failing_venue, &proxy, obs);
    report.proxy_env = proxy;
    report
}

fn venue_items(items: &[NetCheckItem]) -> Vec<&NetCheckItem> {
    items.iter().filter(|i| i.name != "proxy").collect()
}

/// The most specific explanation the report supports, in the order that decides
/// what an operator does next.
///
/// Deliberately a priority and not a set: the UIs print ONE headline, and a
/// headline that hedges ("possibly TLS, possibly DNS, possibly a proxy") is the
/// thing this report exists to replace. The supporting sentences all appear in
/// [`hint_text`], so nothing the priority hides is lost.
fn hint_code(
    venue: &[&NetCheckItem],
    failing_all: &[&NetCheckItem],
    failing_venue: &[&NetCheckItem],
    proxy: &[String],
    obs: &ProxyObservation,
) -> &'static str {
    if venue.is_empty() || venue.iter().all(|i| i.status == UNSUPPORTED) {
        return UNSUPPORTED;
    }
    if failing_all.is_empty() {
        return "ok";
    }
    if (obs.probed && obs.in_use && !obs.tcp_error.is_empty())
        || failing_all.iter().any(|i| i.status == PROXY_UNREACHABLE)
    {
        return PROXY_UNREACHABLE;
    }
    if failing_venue.iter().any(|i| is_tls_status(&i.status)) {
        return "tls_blocked";
    }
    if !failing_venue.is_empty() && failing_venue.iter().all(|i| i.status == "dns_failed") {
        return "dns_failed";
    }
    if proxy_is_headline(obs, proxy) {
        return "proxy_env";
    }
    if venue.iter().all(|i| i.fake_ip) {
        return "fake_ip";
    }
    "partial"
}

fn proxy_is_headline(obs: &ProxyObservation, proxy: &[String]) -> bool {
    if !obs.probed {
        return !proxy.is_empty();
    }
    obs.in_use && !obs.display.is_empty()
}

/// Status of a plugin that implements no probe at all.
pub const UNSUPPORTED: &str = "unsupported";

/// Whether a probe status means "TCP completed, TLS did not".
pub fn is_tls_status(status: &str) -> bool {
    matches!(status, "tls_error" | "tls_cert")
}

/// The full reading: every applicable sentence, in the same order as the codes.
fn hint_text(
    venue: &[&NetCheckItem],
    failing_all: &[&NetCheckItem],
    failing_venue: &[&NetCheckItem],
    proxy: &[String],
    obs: &ProxyObservation,
) -> String {
    if venue.is_empty() {
        return "no probe was run".to_string();
    }
    if venue.iter().all(|i| i.status == UNSUPPORTED) {
        return "this build's market plugin implements no network probe".to_string();
    }
    if failing_all.is_empty() {
        let mut s = format!("all {} network paths answered", venue.len());
        // A green report still carries its context. These are the sentences an
        // operator wants to have read BEFORE the failure a tunneled resolver
        // eventually causes, and this function's contract is that no supporting
        // sentence is lost to the headline — on a passing report the headline has
        // nothing to say at all, so it is the only branch where that would
        // actually hide them.
        for note in [fake_ip_note(venue), proxy_context(proxy, obs)] {
            if !note.is_empty() {
                s.push_str("; ");
                s.push_str(&note);
            }
        }
        return s;
    }
    let mut notes: Vec<String> = Vec::new();
    if let Some(note) = unreachable_note(obs, proxy) {
        notes.push(note);
    }
    let tls: Vec<&str> = failing_venue
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
    let dns: Vec<&str> = failing_venue
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
    let fake_ip = fake_ip_note(venue);
    if !fake_ip.is_empty() {
        notes.push(fake_ip);
    }
    let proxy_note = proxy_context(proxy, obs);
    if !proxy_note.is_empty() && unreachable_note(obs, proxy).is_none() {
        notes.push(proxy_note);
    }
    let other: Vec<&str> = failing_venue
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
            venue.len(),
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
fn fake_ip_note(venue: &[&NetCheckItem]) -> String {
    let fakes: Vec<&&NetCheckItem> = venue.iter().filter(|i| i.fake_ip).collect();
    if fakes.is_empty() {
        return String::new();
    }
    let addrs: Vec<String> = fakes.iter().flat_map(|i| i.addrs.iter().cloned()).collect();
    if addrs.is_empty() {
        return String::new();
    }
    let lead = if venue.iter().all(|i| i.fake_ip) {
        format!(
            "every address came from a proxy fake-IP range ({})",
            addrs.join(", ")
        )
    } else {
        let targets: Vec<&str> = fakes.iter().map(|i| i.target.as_str()).collect();
        format!(
            "DNS returned proxy fake-IP addresses ({}) for {}",
            addrs.join(", "),
            targets.join(", ")
        )
    };
    format!(
        "{lead}: 198.18.0.0/15, 28.0.0.0/8 and fc00::/7 are Clash/Surge markers, \
         often a stale resolver cache after the tunnel stops. On macOS flush with \
         `sudo dscacheutil -flushcache` and `sudo killall -HUP mDNSResponder`"
    )
}

/// The proxy sentence, or nothing when the process sees no proxy variable.
///
/// Returns the sentence WITHOUT a leading separator: [`hint_text`] joins its
/// notes with `"; "`, so a note that carried one of its own was printed as
/// "…answered; ; proxy variables…" on every failing report from a box that had
/// one set.
fn unreachable_note(obs: &ProxyObservation, proxy: &[String]) -> Option<String> {
    if !(obs.probed && obs.in_use && !obs.tcp_error.is_empty()) {
        return None;
    }
    let vars = if proxy.is_empty() {
        "HTTP_PROXY, HTTPS_PROXY, ALL_PROXY".to_string()
    } else {
        proxy.join(", ")
    };
    Some(format!(
        "proxy configured but unreachable at {} ({}). reqwest dials that proxy before any \
         venue I/O and its Display is only \"error sending request\"; the refusal is on the \
         error source chain, which is why REST fails in about 0ms while a WebSocket that \
         ignores those variables can still connect. Variables still set: {vars}. Set \
         BK_PROXY=none, or unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy \
         all_proxy, so REST dials direct",
        obs.display, obs.tcp_error
    ))
}

fn proxy_context(proxy: &[String], obs: &ProxyObservation) -> String {
    if !obs.probed {
        return proxy_note(proxy);
    }
    if obs.invalid {
        return "a proxy value is not an http, https, socks5, or socks5h URL; the value is \
                not shown. Set BK_PROXY=none to dial direct, or set BK_PROXY to a supported URL"
            .to_string();
    }
    if obs.probed && obs.in_use && !obs.tcp_error.is_empty() {
        // The unreachable sentence already names it.
        return String::new();
    }
    if obs.in_use {
        return format!(
            "REST and WebSocket use {} (variables: {})",
            obs.display,
            if proxy.is_empty() {
                "none".to_string()
            } else {
                proxy.join(", ")
            }
        );
    }
    if obs.policy == "none" && (!obs.display.is_empty() || !proxy.is_empty()) {
        let vars = if proxy.is_empty() {
            "the proxy variables".to_string()
        } else {
            proxy.join(", ")
        };
        let dead = if obs.tcp_error.is_empty() {
            String::new()
        } else {
            format!(
                "; {} is not listening ({}) and is not used",
                obs.display, obs.tcp_error
            )
        };
        return format!("BK_PROXY=none dials direct and ignores {vars}{dead}");
    }
    String::new()
}

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
    fn clash_28_slash_8_is_a_fake_ip_and_neighbouring_blocks_are_not() {
        for addr in ["28.0.0.1", "28.0.2.41", "28.255.255.255", "fc00::22a"] {
            let ip: IpAddr = addr.parse().expect("test literal parses");
            assert!(is_fake_ip(&ip), "{addr}");
        }
        for addr in ["27.255.255.255", "29.0.0.1", "128.0.0.1"] {
            let ip: IpAddr = addr.parse().expect("test literal parses");
            assert!(!is_fake_ip(&ip), "{addr}");
        }
    }

    #[test]
    fn bk_proxy_modes_and_https_selection_never_keep_credentials() {
        assert_eq!(parse_proxy_mode(None).unwrap().label(), "auto");
        assert_eq!(parse_proxy_mode(Some(" Off ")).unwrap().label(), "none");
        assert!(parse_proxy_mode(Some("ftp://127.0.0.1:1")).is_err());
        let err = parse_proxy_mode(Some("http://user:pw@127.0.0.1:1")).unwrap();
        let rendered = format!("{err:?}");
        assert!(!rendered.contains("pw"), "{rendered}");

        let view = resolve_proxy_with(
            &ProxyMode::Auto,
            &[
                ("HTTPS_PROXY", "http://user:pw@127.0.0.1:7890"),
                ("ALL_PROXY", "socks5://127.0.0.1:1"),
                ("HTTP_PROXY", "http://127.0.0.1:9"),
            ],
        )
        .unwrap();
        let shown = view.active.as_ref().unwrap().display();
        assert_eq!(shown, "http://127.0.0.1:7890");
        assert!(!format!("{view:?}").contains("pw"), "{view:?}");
        assert!(matches!(view.client_proxy(), ClientProxy::FromEnvironment));

        let only_http =
            resolve_proxy_with(&ProxyMode::Auto, &[("HTTP_PROXY", "http://127.0.0.1:9")]).unwrap();
        assert!(
            only_http.active.is_none(),
            "HTTP_PROXY does not intercept https"
        );

        let star = resolve_proxy_with(
            &ProxyMode::Auto,
            &[("ALL_PROXY", "socks5://127.0.0.1:7890"), ("NO_PROXY", "*")],
        )
        .unwrap();
        assert!(star.active.is_none(), "NO_PROXY=* bypasses the proxy");

        let direct = resolve_proxy_with(
            &ProxyMode::Direct,
            &[("ALL_PROXY", "http://user:pw@127.0.0.1:7890")],
        )
        .unwrap();
        assert!(direct.active.is_none());
        assert_eq!(
            direct.configured.as_ref().unwrap().display(),
            "http://127.0.0.1:7890"
        );
        assert!(matches!(direct.client_proxy(), ClientProxy::Disable));
        let line = boot_line(&direct).unwrap();
        assert!(line.contains("BK_PROXY=none"), "{line}");
        assert!(!line.contains("pw") && !line.contains("user:"), "{line}");
    }

    #[test]
    fn a_dead_in_use_proxy_is_the_headline_and_fake_ips_stay_in_the_hint() {
        let obs = ProxyObservation {
            probed: true,
            policy: "auto",
            display: "http://127.0.0.1:7890".into(),
            in_use: true,
            invalid: false,
            tcp_error: "Connection refused (os error 61)".into(),
            tcp_ms: 0,
        };
        let r = finish_with(
            NetCheckReport {
                ok: false,
                ts_ms: 0,
                hint_code: String::new(),
                hint: String::new(),
                proxy_env: Vec::new(),
                items: vec![item("venue-rest", false, "transport_error", &["28.0.2.41"])],
            },
            vec!["ALL_PROXY".into(), "HTTPS_PROXY".into()],
            &obs,
        );
        assert_eq!(r.hint_code, "proxy_unreachable");
        assert!(r.hint.contains("unreachable"), "{}", r.hint);
        assert!(r.hint.contains("http://127.0.0.1:7890"), "{}", r.hint);
        assert!(r.hint.contains("BK_PROXY=none"), "{}", r.hint);
        assert!(r.hint.contains("28.0.2.41"), "{}", r.hint);
        assert!(r.hint.contains("dscacheutil"), "{}", r.hint);
        assert!(!r.hint.contains("pw"), "{}", r.hint);
    }

    #[test]
    fn bk_proxy_none_does_not_blame_the_proxy_variable() {
        let obs = ProxyObservation {
            probed: true,
            policy: "none",
            display: "http://127.0.0.1:7890".into(),
            in_use: false,
            invalid: false,
            tcp_error: "Connection refused (os error 61)".into(),
            tcp_ms: 0,
        };
        let r = finish_with(
            NetCheckReport {
                ok: false,
                ts_ms: 0,
                hint_code: String::new(),
                hint: String::new(),
                proxy_env: Vec::new(),
                items: vec![item("venue-rest", true, "ok", &["1.1.1.1"])],
            },
            vec!["ALL_PROXY".into()],
            &obs,
        );
        assert!(r.ok);
        assert_eq!(r.hint_code, "ok");
        assert!(r.hint.contains("BK_PROXY=none"), "{}", r.hint);
        assert!(r.hint.contains("not listening"), "{}", r.hint);
    }

    #[test]
    fn error_chain_walks_sources_and_strips_userinfo() {
        #[derive(Debug)]
        struct Leaf;
        impl std::fmt::Display for Leaf {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("Connection refused (os error 61)")
            }
        }
        impl std::error::Error for Leaf {}
        static LEAF: Leaf = Leaf;
        #[derive(Debug)]
        struct Top;
        impl std::fmt::Display for Top {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("error sending request for url (https://user:pw@clob.example/)")
            }
        }
        impl std::error::Error for Top {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&LEAF)
            }
        }
        let chain = error_chain(&Top);
        assert!(chain.contains("error sending request"), "{chain}");
        assert!(chain.contains("Connection refused"), "{chain}");
        assert!(!chain.contains("pw"), "{chain}");
        assert!(chain.contains("clob.example"), "{chain}");
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
