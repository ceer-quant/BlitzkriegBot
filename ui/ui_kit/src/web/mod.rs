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
        "lastError": s.last_error,
    })
    .to_string()
}

/// Minimal parsed HTTP request.
struct HttpRequest {
    method: String,
    target: String,
    body: String,
}

impl HttpRequest {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
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
}

impl WebServer {
    /// Read-only panel (no command API).
    pub fn new(client: IpcClient, trade_limit: usize) -> Self {
        Self {
            snapshot_src: Arc::new(Mutex::new(client)),
            trade_limit,
            dispatcher: None,
        }
    }

    /// Panel + command API. Lifecycle verbs are gated by the dispatcher's own
    /// `lifecycle_enabled` flag.
    pub fn with_gateway(client: IpcClient, trade_limit: usize, dispatcher: Dispatcher) -> Self {
        Self {
            snapshot_src: Arc::new(Mutex::new(client)),
            trade_limit,
            dispatcher: Some(Arc::new(Mutex::new(dispatcher))),
        }
    }

    /// Serve until the process is stopped. `addr` e.g. `127.0.0.1:18888`.
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

        let (status, ctype, body) = match (req.method.as_str(), target.as_str()) {
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
    })
}

/// POST bodies may be raw command text or `{"cmd":"..."}`.
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
mod tests {
    use super::*;

    #[test]
    fn query_cmd_decodes() {
        let r = HttpRequest {
            method: "GET".into(),
            target: "/api/command?cmd=start+BTC,ETH".into(),
            body: String::new(),
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
