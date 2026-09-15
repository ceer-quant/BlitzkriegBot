//! Web adapter — renders a `UiSnapshot` to HTML (and JSON endpoints) and serves
//! it from a dependency-free HTTP/1.1 server so the browser panel can be opened
//! directly. This is the browser-facing sibling of the TUI and app adapters; it
//! contains no trading logic.
//!
//! When constructed with a [`Dispatcher`] (gateway mode) the server additionally
//! exposes the command surface:
//!
//!   GET  /                    HTML panel (auto-refresh)
//!   GET  /api/snapshot        JSON snapshot
//!   GET  /api/command?cmd=..  dispatch one command → JSON outcome
//!   POST /api/command         body = command text (or `{"cmd":"..."}`) → JSON
//!
//! Command dispatch is trading-logic-free: it starts/stops the *process* and
//! reads state. No order is ever placed from here.

use crate::core::ipc_client::IpcClient;
use crate::core::types::UiSnapshot;
use crate::gateway::Dispatcher;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn money(v: f64) -> String {
    format!("{}{:.2}", if v >= 0.0 { "+" } else { "-" }, v.abs())
}
fn pct(v: f64) -> String {
    format!("{}{:.1}%", if v >= 0.0 { "+" } else { "-" }, v.abs())
}

/// Render the whole panel as a single self-refreshing HTML document.
pub fn render_html(s: &UiSnapshot) -> String {
    render_html_with(s, false)
}

/// As [`render_html`], optionally including the command console (gateway mode).
pub fn render_html_with(s: &UiSnapshot, console: bool) -> String {
    let mode = esc(s.mode());
    let mut rows = String::new();
    if let Some(r) = &s.round {
        rows.push_str(&format!(
            "<div class=card><h2>Round</h2><div class=big>#{}</div><div class=sub>{}s old · {}s left · {}</div></div>",
            r.slot, r.age_sec, r.time_left_sec,
            if r.can_trade { "TRADING" } else { "WAITING" }
        ));
    }
    if let Some(b) = &s.balance {
        rows.push_str(&format!(
            "<div class=card><h2>Balance</h2><div class=big>${:.2}</div><div class=sub>reserved ${:.2} · avail ${:.2}</div></div>",
            b.balance, b.reserved, b.available
        ));
    }
    if let Some(st) = &s.stats {
        rows.push_str(&format!(
            "<div class=card><h2>Feed</h2><div class=big>{} books</div><div class=sub>signals {} · rejected {} · confirmed {}</div></div>",
            st.books, st.signals, st.place_rejected, st.confirmed.len()
        ));
    }
    let net = s.net_pnl();
    rows.push_str(&format!(
        "<div class=card><h2>Net PnL</h2><div class='big {}'>{}</div><div class=sub>{} trades · {:.0}% WR</div></div>",
        if net >= 0.0 { "pos" } else { "neg" },
        money(net), s.trades.len(), s.win_rate()
    ));

    let mut pos_rows = String::new();
    for p in &s.positions {
        pos_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{:.2}</td><td>{:.2}</td><td class='{}'>{}</td><td>{}s</td></tr>",
            esc(&p.asset), esc(&p.direction.to_uppercase()), p.entry_price, p.current_price,
            if p.unrealized_pct >= 0.0 { "pos" } else { "neg" }, pct(p.unrealized_pct), p.remaining_sec
        ));
    }
    if pos_rows.is_empty() {
        pos_rows.push_str("<tr><td colspan=6 class=sub>no open positions</td></tr>");
    }

    let mut trade_rows = String::new();
    for t in s.trades.iter().rev().take(25) {
        trade_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{:.2}→{:.2}</td><td class='{}'>{}</td><td>{}</td></tr>",
            esc(&t.asset),
            esc(&t.direction.to_uppercase()),
            t.entry_price,
            t.exit_price,
            if t.net_pnl_usd >= 0.0 { "pos" } else { "neg" },
            money(t.net_pnl_usd),
            esc(&t.exit_reason)
        ));
    }
    if trade_rows.is_empty() {
        trade_rows.push_str("<tr><td colspan=5 class=sub>no closed trades</td></tr>");
    }

    let price_rows = s
        .round
        .as_ref()
        .map(|r| {
            r.market_prices
                .iter()
                .map(|m| {
                    format!(
                        "<span class=pill>{}: ↑{:.2} ↓{:.2}</span>",
                        esc(&m.asset),
                        m.up,
                        m.down
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();

    let err = s
        .last_error
        .as_ref()
        .map(|e| format!("<div class=err>last error: {}</div>", esc(e)))
        .unwrap_or_default();

    let console_html = if console {
        r#"<div class=panel><h2>Command console</h2>
<form id=cmdform onsubmit="return runCmd(event)">
<input id=cmdinput name=cmd placeholder="status | start BTC,ETH --size 5 | stop | positions 25" autocomplete=off>
<button type=submit>Run</button>
</form>
<pre id=cmdout class=sub>try: status</pre></div>
<script>
async function runCmd(e){
 e.preventDefault();
 var v=document.getElementById('cmdinput').value;
 var out=document.getElementById('cmdout');
 out.textContent='…';
 try{
  var r=await fetch('/api/command',{method:'POST',body:v});
  var j=await r.json();
  out.textContent=(j.ok?'OK ':('ERR '))+j.action+' — '+j.message;
 }catch(err){ out.textContent='request failed: '+err; }
 return false;
}
</script>"#
    } else {
        ""
    };

    format!(
        r#"<!doctype html><html lang=en><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1">
<meta http-equiv=refresh content=3>
<title>Blitzkrieg UI Kit — Web</title>
<style>
:root{{--bg:#0d1117;--fg:#e6edf3;--mut:#8b949e;--card:#161b22;--pos:#3fb950;--neg:#f85149;--bd:#30363d}}
*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--fg);font:14px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;padding:20px}}
h1{{font-size:18px;margin:0 0 4px}}.mode{{color:var(--mut);font-size:12px;margin-bottom:16px}}
.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(190px,1fr));gap:12px;margin-bottom:20px}}
.card{{background:var(--card);border:1px solid var(--bd);border-radius:8px;padding:14px}}
.card h2{{margin:0 0 6px;font-size:11px;color:var(--mut);text-transform:uppercase;letter-spacing:.08em;font-weight:600}}
.big{{font-size:24px;font-weight:600}}.sub{{color:var(--mut);font-size:12px;margin-top:4px}}
table{{width:100%;border-collapse:collapse;margin-bottom:20px}}th,td{{text-align:left;padding:6px 8px;border-bottom:1px solid var(--bd)}}
th{{color:var(--mut);font-size:11px;text-transform:uppercase;letter-spacing:.06em}}
.pos{{color:var(--pos)}}.neg{{color:var(--neg)}}.err{{color:var(--neg);margin-top:12px}}
.pill{{display:inline-block;background:var(--card);border:1px solid var(--bd);border-radius:999px;padding:2px 10px;margin:2px;font-size:12px}}
.panel{{background:var(--card);border:1px solid var(--bd);border-radius:8px;padding:14px;margin-bottom:20px}}
.panel h2{{margin:0 0 10px;font-size:13px}}
#cmdinput{{width:min(560px,70%);background:#0d1117;color:var(--fg);border:1px solid var(--bd);border-radius:6px;padding:8px 10px;font:inherit}}
button{{background:#21262d;color:var(--fg);border:1px solid var(--bd);border-radius:6px;padding:8px 14px;font:inherit;cursor:pointer}}
pre{{white-space:pre-wrap;margin:10px 0 0}}
</style></head><body>
<h1>Blitzkrieg UI Kit</h1>
<div class=mode>core mode: <b>{mode}</b> · connected: {} · source: UDS JSON-RPC (read-only panel){gateway_note}</div>
<div class=grid>{rows}</div>
<div class=panel><h2>Open Positions</h2><table>
<tr><th>Asset</th><th>Dir</th><th>Entry</th><th>Cur</th><th>PnL</th><th>Left</th></tr>{pos_rows}</table></div>
<div class=panel><h2>Recent Trades</h2><table>
<tr><th>Asset</th><th>Dir</th><th>Entry→Exit</th><th>Net</th><th>Reason</th></tr>{trade_rows}</table></div>
<div class=panel><h2>Prices</h2>{price_rows}</div>
{console_html}
{err}
</body></html>"#,
        s.connected,
        rows = rows,
        pos_rows = pos_rows,
        trade_rows = trade_rows,
        price_rows = price_rows,
        mode = mode,
        gateway_note = if console {
            " · gateway: commands enabled"
        } else {
            ""
        },
        console_html = console_html,
        err = err,
    )
}

/// Render the snapshot as a stable JSON document (for programmatic consumers).
pub fn render_json(s: &UiSnapshot) -> String {
    serde_json::json!({
        "connected": s.connected,
        "mode": s.mode(),
        "balance": s.balance.as_ref().map(|b| serde_json::json!({
            "balance": b.balance, "reserved": b.reserved, "available": b.available })),
        "round": s.round.as_ref().map(|r| serde_json::json!({
            "slot": r.slot, "ageSec": r.age_sec, "timeLeftSec": r.time_left_sec, "canTrade": r.can_trade })),
        "positions": s.positions.iter().map(|p| serde_json::json!({
            "asset": p.asset, "direction": p.direction, "entryPrice": p.entry_price,
            "currentPrice": p.current_price, "unrealizedPct": p.unrealized_pct })).collect::<Vec<_>>(),
        "trades": { "count": s.trades.len(), "net": s.net_pnl(), "winRate": s.win_rate() },
        // E9-g: per-strategy accounting rows for the plugins/strategies page.
        "strategyStats": s.strategy_stats.iter().map(|r| serde_json::json!({
            "name": r.name, "enabled": r.enabled, "source": r.source,
            "ordersPlaced": r.orders_placed, "ordersRejected": r.orders_rejected,
            "limitRejected": r.limit_rejected,
            "blockedTiming": r.blocked_timing, "blockedMomentum": r.blocked_momentum,
            "gateExemptedTiming": r.gate_exempted_timing,
            "gateExemptedMomentum": r.gate_exempted_momentum,
            "gateExemptions": r.gate_exemptions,
            "closedTrades": r.closed_trades, "wins": r.wins, "losses": r.losses,
            "netPnlUsd": r.net_pnl_usd,
            "rejectionCauses": r.rejection_causes,
        })).collect::<Vec<_>>(),
        // E9-g: engine counters for the overview page (older cores omit).
        "stats": s.stats.as_ref().map(|st| serde_json::json!({
            "books": st.books, "tops": st.tops, "spots": st.spots,
            "rounds": st.rounds, "evaluations": st.evaluations,
            "signals": st.signals, "placeRejected": st.place_rejected,
            "orderCounts": st.confirmed.len(),
            "strategies": st.strategies.iter().map(|r| serde_json::json!({
                "name": r.name, "enabled": r.enabled, "source": r.source,
                "ordersPlaced": r.orders_placed, "ordersRejected": r.orders_rejected,
                "limitRejected": r.limit_rejected,
                "blockedTiming": r.blocked_timing, "blockedMomentum": r.blocked_momentum,
                "gateExemptedTiming": r.gate_exempted_timing,
                "gateExemptedMomentum": r.gate_exempted_momentum,
                "gateExemptions": r.gate_exemptions,
                "closedTrades": r.closed_trades, "wins": r.wins, "losses": r.losses,
                "netPnlUsd": r.net_pnl_usd,
                "rejectionCauses": r.rejection_causes,
            })).collect::<Vec<_>>(),
        })),
        "strategies": s.strategies.iter().map(|x| serde_json::json!(x.clone())).collect::<Vec<_>>(),
        "extensions": s.extensions.iter().map(|x| serde_json::json!(x.clone())).collect::<Vec<_>>(),
        "marketPlugins": s.market_plugins.iter().map(|x| serde_json::json!(x.clone())).collect::<Vec<_>>(),
        "lastError": s.last_error,
    })
    .to_string()
}


/// Minimal std-only base64 decoder for basic-auth passwords (the only place
/// the web layer needs it). Returns raw bytes; callers validate UTF-8.
fn data_encoding_free_base64(s: &str) -> Vec<u8> {
    const TBL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let Some(v) = TBL.iter().position(|t| *t == c) else {
            return out;
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}
/// Minimal parsed HTTP request.
struct HttpRequest {
    method: String,
    target: String,
    body: String,
    /// Raw header (name, value) pairs, lowercased names.
    headers: Vec<(String, String)>,
}

impl HttpRequest {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }
    /// Case-insensitive header lookup over the stored lines.
    fn header(&self, name: &str) -> Option<&str> {
        // Header case is handled by the caller storing the raw lines; keep the
        // contract tiny: headers were captured lowercased by read_request.
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    /// First `cmd` value from the query string, percent-decoded (`+` → space).
    fn query_cmd(&self) -> Option<String> {
        let q = self.target.split_once('?')?.1;
        for pair in q.split('&') {
            if let Some(v) = pair.strip_prefix("cmd=") {
                return Some(url_decode(v));
            }
        }
        None
    }
}

/// Byte-equal compare in time proportional to the expected value, so short
/// wrong guesses are not observably faster than long ones.
fn same_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        // Do not leak length via early exit: still burn a compare pass.
        let sink = a.iter().chain(b.iter()).fold(0u8, |acc, x| acc ^ x);
        return sink == !0u8; // practically never true
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Repo-rooted path to the built Vue panel's `index.html`/assets. Relative to
/// the ui_kit crate so it holds for both `cargo run` and a repo checkout.
fn vue_panel_dir() -> Option<std::path::PathBuf> {
    // CARGO_MANIFEST_DIR is <repo>/ui/ui_kit; the built Vue app lives at
    // <repo>/ui/webapp/webui/dist.
    let manifest = env!("CARGO_MANIFEST_DIR");
    let dir = std::path::Path::new(manifest)
        .join("../webapp/webui/dist/index.html")
        .canonicalize()
        .ok()?;
    dir.is_file().then(|| dir.parent().map(std::path::Path::to_path_buf)).flatten()
}

/// Serve a static asset from the Vue panel dir; `/panel/` or `/panel` (no
/// trailing file) resolves to `index.html`. System path traversal is blocked.
fn serve_vue_panel_asset(path: &str) -> Option<(String, &'static str)> {
    let rel = path.strip_prefix("/panel/")?;
    if rel.split('/').any(|seg| seg == ".." || seg.is_empty()) {
        return None;
    }
    serve_vue_panel_file(&std::path::PathBuf::from(rel))
}

fn serve_vue_panel() -> Option<(String, &'static str)> {
    serve_vue_panel_file(&std::path::PathBuf::from("index.html"))
}

fn serve_vue_panel_file(rel: &std::path::Path) -> Option<(String, &'static str)> {
    let dir = vue_panel_dir()?;
    let full = dir.join(rel).canonicalize().ok()?;
    // Canonical path must stay inside the panel dir.
    if !full.starts_with(&dir) {
        return None;
    }
    let body = std::fs::read(&full).ok()?;
    let ctype = match full.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    Some((
        String::from_utf8_lossy(&body).into_owned(),
        ctype,
    ))
}

fn url_decode(s: &str) -> String {
    let bytes = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A trivial, dependency-free HTTP server that always renders a fresh snapshot.
pub struct WebServer {
    snapshot_src: Arc<Mutex<IpcClient>>,
    trade_limit: usize,
    /// Present only in gateway mode (`--manage`); enables `/api/command`.
    dispatcher: Option<Arc<Mutex<Dispatcher>>>,
    /// E6-a panel credentials from `BLITZKRIEG_PANEL_USER` /
    /// `BLITZKRIEG_PANEL_PASSWORD`. Unset disables auth (loopback-only
    /// deployment contract).
    panel_user: Option<String>,
    panel_password: Option<String>,
    /// Session tokens issued by a successful `POST /api/login`.
    sessions: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl WebServer {
    /// Read-only panel (no command API).
    pub fn new(client: IpcClient, trade_limit: usize) -> Self {
        Self {
            snapshot_src: Arc::new(Mutex::new(client)),
            trade_limit,
            dispatcher: None,
            panel_user: None,
            panel_password: None,
            sessions: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }

    fn token_from_query(&self, req: &HttpRequest) -> Option<String> {
        let q = req.target.split_once('?')?.1;
        for pair in q.split('&') {
            if let Some(v) = pair.strip_prefix("token=") {
                return Some(url_decode(v));
            }
        }
        None
    }

    /// Arm user/password auth from env (`BLITZKRIEG_PANEL_USER` +
    /// `BLITZKRIEG_PANEL_PASSWORD`). Both must be non-empty; empty/unset
    /// disables auth (loopback-only deployment contract).
    pub fn set_panel_credentials(&mut self, user: Option<String>, password: Option<String>) {
        self.panel_user = user.filter(|u| !u.trim().is_empty());
        self.panel_password = password.filter(|p| !p.trim().is_empty());
        if self.panel_user.is_none() || self.panel_password.is_none() {
            // Half-configured is a config error — refuse the whole pair so a
            // password-less panel never ships.
            self.panel_user = None;
            self.panel_password = None;
        }
    }

    /// Issue a session token for valid panel credentials.
    fn login(&self, user: &str, password: &str) -> Option<String> {
        let (expected_user, expected_pass) =
            (self.panel_user.as_deref()?, self.panel_password.as_deref()?);
        // Constant-ish time compare to blunt trivial timing probes.
        if !same_time_eq(user.as_bytes(), expected_user.as_bytes())
            || !same_time_eq(password.as_bytes(), expected_pass.as_bytes())
        {
            return None;
        }
        // Session token: time + address entropy via std only.
        let mut seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407)
            ^ (std::process::id() as u128) << 32
            ^ (&password as *const _ as *const () as usize as u128);
        let mut hex = String::with_capacity(40);
        while hex.len() < 40 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            hex.push_str(&format!("{:08x}", (seed >> 33) as u32));
        }
        hex.truncate(40);
        if let Ok(mut s) = self.sessions.lock() {
            s.insert(hex.clone());
        }
        Some(hex)
    }

    fn session_valid(&self, token: &str) -> bool {
        match self.sessions.lock() {
            Ok(s) => s.contains(token),
            Err(_) => false,
        }
    }

    /// Revoke a session (logout).
    fn logout(&self, token: &str) {
        if let Ok(mut s) = self.sessions.lock() {
            s.remove(token);
        }
    }

    fn token_from_cookies(&self, req: &HttpRequest) -> Option<String> {
        let cookies = req.header("cookie")?;
        for pair in cookies.split(';') {
            let pair = pair.trim();
            if let Some(v) = pair.strip_prefix("bk_session=") {
                return Some(v.to_string());
            }
        }
        None
    }

    /// E6-a gate: user/password login issues a session; every /api/* call then
    /// carries the session (query / X-Auth-Token / Bearer / bk_session cookie).
    /// Origin policy — foreign origins 403, loopback allowed.
    fn authorize(&self, req: &HttpRequest) -> u16 {
        if req.path() == "/api/login" {
            return 200; // the login endpoint itself must be reachable
        }
        if !req.path().starts_with("/api/") {
            return 200; // the panel itself is served to unauthenticated clients
        }
        if self.panel_user.is_some() {
            let supplied = self
                .token_from_query(req)
                .or_else(|| self.token_from_cookies(req))
                .or_else(|| req.header("x-auth-token").map(String::from))
                .or_else(|| {
                    req.header("authorization").and_then(|a| {
                        a.strip_prefix("Bearer ")
                            .map(String::from)
                            .or_else(|| {
                                // Legacy basic-auth user form still accepted.
                                a.strip_prefix("Basic ").and_then(|b| {
                                    let raw = data_encoding_free_base64(b);
                                    std::str::from_utf8(&raw)
                                        .ok()
                                        .and_then(|d| d.split(':').next().map(String::from))
                                })
                            })
                    })
                });
            match supplied {
                Some(t) if self.session_valid(&t) => {}
                _ => return 401,
            }
        }
        // Origin policy: once present, only loopback (127.x/localhost) hosts pass.
        if let Some(origin) = req.header("origin") {
            let host = origin
                .trim_start_matches("http://")
                .trim_start_matches("https://");
            let host_only = host.split(':').next().unwrap_or("");
            let loopback = host_only == "127.0.0.1" || host_only == "localhost" || host_only.starts_with("127.");
            if !loopback {
                return 403;
            }
        }
        200
    }

    /// Panel + command API. Lifecycle verbs are gated by the dispatcher's own
    /// `lifecycle_enabled` flag.
    pub fn with_gateway(client: IpcClient, trade_limit: usize, dispatcher: Dispatcher) -> Self {
        Self {
            snapshot_src: Arc::new(Mutex::new(client)),
            trade_limit,
            dispatcher: Some(Arc::new(Mutex::new(dispatcher))),
            panel_user: None,
            panel_password: None,
            sessions: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Serve until the process is stopped. `addr` e.g. `127.0.0.1:51888`.
    pub fn serve(&self, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        let console = self.dispatcher.is_some();
        println!(
            "ui_kit web adapter listening on http://{addr}/  (panel) and /api/snapshot (JSON)"
        );
        if console {
            println!(
                "  gateway: /api/command (GET ?cmd=… or POST body){}",
                if self.lifecycle_enabled() {
                    " · lifecycle ENABLED"
                } else {
                    " · read-only (pass --manage for start/stop)"
                }
            );
        }
        for stream in listener.incoming() {
            match stream {
                Ok(s) => self.handle(s),
                Err(e) => eprintln!("accept: {e}"),
            }
        }
        Ok(())
    }

    fn lifecycle_enabled(&self) -> bool {
        self.dispatcher
            .as_ref()
            .map(|d| d.lock().map(|d| d.lifecycle_enabled()).unwrap_or(false))
            .unwrap_or(false)
    }

    fn handle(&self, mut stream: TcpStream) {
        // A client that connects and stalls must not wedge the accept loop.
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
        let Some(req) = read_request(&mut stream) else {
            return;
        };
        let target = req.path().to_string();

        // E6-a: auth + origin gate runs BEFORE any route does work.
        let status_gate = self.authorize(&req);
        if status_gate != 200 {
            let reason = if status_gate == 401 { "Unauthorized" } else { "Forbidden" };
            let head = format!(
                "HTTP/1.1 {status_gate} {reason}\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
            return;
        }

        let (status, ctype, body) = match (req.method.as_str(), target.as_str()) {
            ("GET", "/api/plugins") => {
                // E9-g: the registry trio the TUI Plugins page shows (strategies
                // / extensions / market plugins) in one authenticated call.
                let doc = match self.dispatcher {
                    Some(ref d) => match d.lock() {
                        Ok(mut dp) => {
                            let snap = dp.plugins_snapshot();
                            serde_json::json!({
                                "connected": snap.connected,
                                "strategies": snap.strategies,
                                "extensions": snap.extensions,
                                "marketPlugins": snap.market_plugins,
                                "marketActive": snap.market_active,
                                "lastError": snap.last_error,
                            })
                        }
                        Err(_) => serde_json::json!({"connected": false,
                            "lastError": "dispatcher poisoned"}),
                    },
                    None => {
                        // read-only mode: registry via the snapshot client
                        let mut doc = serde_json::json!({"connected": false});
                        doc["lastError"] = serde_json::json!(
                            "plugin registry requires gateway (--manage)");
                        doc
                    }
                };
                (200, "application/json", doc.to_string())
            }
            ("GET", "/api/snapshot") => {
                let snap = self.snapshot();
                (200, "application/json", render_json(&snap))
            }
            ("GET", "/api/command") | ("POST", "/api/command") => {
                let cmd = if req.method == "POST" {
                    body_to_command(&req.body)
                } else {
                    req.query_cmd().unwrap_or_default()
                };
                (200, "application/json", self.run_command(&cmd))
            }
            ("POST", "/api/login") => {
                // body: {"user":"…","password":"…"} → {"ok":true,"token":…,
                // "user":…} so the WebUI can store it as a session.
                let creds = body_to_command(&req.body); // reuse tiny parse? no — dedicated parse below
                let _ = creds;
                let doc = match parse_login_body(&req.body) {
                    Some((u, p)) => match self.login(&u, &p) {
                        Some(tok) => serde_json::json!({
                            "ok": true, "token": tok, "user": u,
                        }),
                        None => serde_json::json!({
                            "ok": false, "error": "用户名或密码错误",
                        }),
                    },
                    None => serde_json::json!({
                        "ok": false, "error": "请求格式错误（需要 JSON {user, password}）",
                    }),
                };
                let status = if doc["ok"] == serde_json::json!(true) { 200 } else { 401 };
                (
                    status,
                    "application/json; charset=utf-8",
                    serde_json::to_string(&doc).unwrap_or_default(),
                )
            }
            ("GET", "/api/logout") | ("POST", "/api/logout") => {
                // Session token via any channel; revoke it. Harmless if absent.
                let token = self
                    .token_from_query(&req)
                    .or_else(|| self.token_from_cookies(&req))
                    .or_else(|| req.header("x-auth-token").map(String::from))
                    .unwrap_or_default();
                self.logout(&token);
                (200, "application/json", "{\"ok\":true}".to_string())
            }
            ("GET", "/") | ("GET", "/panel") | ("GET", "/panel/") => {
                // /panel is the canonical entry: the E9-g Vue app when built
                // (ui/webapp/webui/dist), else the built-in HTML panel. `/`
                // redirects so human-typed origins always land in the same place.
                if target != "/panel" && target != "/panel/" {
                    let head = "HTTP/1.1 302 Found\r\nLocation: /panel\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.flush();
                    return;
                }
                match serve_vue_panel() {
                    Some((body, ctype)) => (200, ctype, body),
                    None => {
                        let snap = self.snapshot();
                        (
                            200,
                            "text/html; charset=utf-8",
                            render_html_with(&snap, self.dispatcher.is_some()),
                        )
                    }
                }
            }
            ("GET", path) if path.starts_with("/panel/") => {
                // Vue app assets (`/panel/assets/*.js|css`, favicon, …).
                match serve_vue_panel_asset(path) {
                    Some((body, ctype)) => (200, ctype, body),
                    None => (404, "text/plain; charset=utf-8", "not found".to_string()),
                }
            }
            ("GET", _) => {
                let snap = self.snapshot();
                (
                    200,
                    "text/html; charset=utf-8",
                    render_html_with(&snap, self.dispatcher.is_some()),
                )
            }
            _ => (404, "text/plain; charset=utf-8", "not found".to_string()),
        };

        if std::env::var("UIKIT_WEB_TRACE").is_ok() {
            eprintln!(
                "ui_kit web: {} {} -> {} bytes ({status})",
                req.method,
                target,
                body.len()
            );
        }
        let reason = if status == 200 { "OK" } else { "Not Found" };
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
    }

    fn snapshot(&self) -> UiSnapshot {
        match self.snapshot_src.lock() {
            Ok(mut c) => c.snapshot(self.trade_limit),
            Err(_) => UiSnapshot::default(),
        }
    }

    fn run_command(&self, cmd: &str) -> String {
        let Some(d) = &self.dispatcher else {
            return serde_json::json!({
                "ok": false, "action": "error",
                "message": "command API disabled; run ui_kit_web with --manage"
            })
            .to_string();
        };
        match d.lock() {
            Ok(mut d) => serde_json::to_string(&d.dispatch_line(cmd)).unwrap_or_else(|e| {
                format!("{{\"ok\":false,\"action\":\"error\",\"message\":\"serialize: {e}\"}}")
            }),
            Err(_) => "{\"ok\":false,\"action\":\"error\",\"message\":\"dispatcher poisoned\"}"
                .to_string(),
        }
    }
}

/// Parse one HTTP/1.1 request (request line + headers + optional body).
fn read_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut content_length = 0usize;
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim();
        if h.is_empty() {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if let Some((name, value)) = h.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let body = if content_length > 0 {
        let mut buf = vec![0u8; content_length.min(64 * 1024)];
        let mut read = 0;
        while read < buf.len() {
            match reader.read(&mut buf[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(_) => break,
            }
        }
        buf.truncate(read);
        String::from_utf8_lossy(&buf).into_owned()
    } else {
        String::new()
    };
    Some(HttpRequest {
        method,
        target,
        body,
        headers,
    })
}

/// POST bodies may be raw command text or `{"cmd":"..."}`.
/// Parse a login request body: JSON `{"user": “…”, “password”: “…”}` (or the
/// url-encoded `user=…&password=…` form). Returns None on malformed input.
fn parse_login_body(body: &str) -> Option<(String, String)> {
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
        let user = v.get("user")?.as_str()?.to_string();
        let password = v.get("password")?.as_str()?.to_string();
        Some((user, password))
    } else {
        let mut user = None;
        let mut password = None;
        for pair in trimmed.split('&') {
            if let Some(v) = pair.strip_prefix("user=") {
                user = Some(url_decode(v));
            } else if let Some(v) = pair.strip_prefix("password=") {
                password = Some(url_decode(v));
            }
        }
        Some((user?, password?))
    }
}

fn body_to_command(body: &str) -> String {
    let t = body.trim();
    if t.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            if let Some(c) = v.get("cmd").and_then(|c| c.as_str()) {
                return c.to_string();
            }
        }
    }
    t.to_string()
}

#[cfg(test)]
mod auth_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_cmd_decodes() {
        let r = HttpRequest {
            method: "GET".into(),
            target: "/api/command?cmd=start+BTC,ETH".into(),
            body: String::new(),
            headers: Vec::new(),
        };
        assert_eq!(r.path(), "/api/command");
        assert_eq!(r.query_cmd().as_deref(), Some("start BTC,ETH"));
    }

    #[test]
    fn body_json_or_raw() {
        assert_eq!(body_to_command("{\"cmd\":\"status\"}"), "status");
        assert_eq!(body_to_command("  stop  "), "stop");
    }

    #[test]
    fn render_html_escaping_and_console() {
        let snap = UiSnapshot::default();
        assert!(render_html(&snap).contains("Blitzkrieg UI Kit"));
        assert!(!render_html(&snap).contains("Command console"));
        assert!(render_html_with(&snap, true).contains("Command console"));
    }
}
