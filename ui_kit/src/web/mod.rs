//! Web adapter — renders a `UiSnapshot` to HTML (and a JSON endpoint) and serves
//! it from a dependency-free HTTP/1.1 server so the browser panel can be opened
//! directly. This is the browser-facing sibling of the TUI and app adapters; it
//! contains no trading logic.

use crate::core::ipc_client::IpcClient;
use crate::core::types::UiSnapshot;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn money(v: f64) -> String {
    format!("{}{:.2}", if v >= 0.0 { "+" } else { "-" }, v.abs())
}
fn pct(v: f64) -> String {
    format!("{}{:.1}%", if v >= 0.0 { "+" } else { "-" }, v.abs())
}

/// Render the whole panel as a single self-refreshing HTML document.
pub fn render_html(s: &UiSnapshot) -> String {
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
            esc(&t.asset), esc(&t.direction.to_uppercase()), t.entry_price, t.exit_price,
            if t.net_pnl_usd >= 0.0 { "pos" } else { "neg" }, money(t.net_pnl_usd), esc(&t.exit_reason)
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
                .map(|m| format!("<span class=pill>{}: ↑{:.2} ↓{:.2}</span>", esc(&m.asset), m.up, m.down))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();

    let err = s
        .last_error
        .as_ref()
        .map(|e| format!("<div class=err>last error: {}</div>", esc(e)))
        .unwrap_or_default();

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
</style></head><body>
<h1>Blitzkrieg UI Kit</h1>
<div class=mode>core mode: <b>{mode}</b> · connected: {} · source: UDS JSON-RPC (read-only)</div>
<div class=grid>{rows}</div>
<div class=panel><h2>Open Positions</h2><table>
<tr><th>Asset</th><th>Dir</th><th>Entry</th><th>Cur</th><th>PnL</th><th>Left</th></tr>{pos_rows}</table></div>
<div class=panel><h2>Recent Trades</h2><table>
<tr><th>Asset</th><th>Dir</th><th>Entry→Exit</th><th>Net</th><th>Reason</th></tr>{trade_rows}</table></div>
<div class=panel><h2>Prices</h2>{price_rows}</div>
{err}
</body></html>"#,
        s.connected,
        rows = rows,
        pos_rows = pos_rows,
        trade_rows = trade_rows,
        price_rows = price_rows,
        mode = mode,
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

/// A trivial, dependency-free HTTP server that always renders a fresh snapshot.
pub struct WebServer {
    snapshot_src: Arc<Mutex<IpcClient>>,
    trade_limit: usize,
}

impl WebServer {
    pub fn new(client: IpcClient, trade_limit: usize) -> Self {
        Self { snapshot_src: Arc::new(Mutex::new(client)), trade_limit }
    }

    /// Serve until the process is stopped. `addr` e.g. `127.0.0.1:18888`.
    pub fn serve(&self, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        println!("ui_kit web adapter listening on http://{addr}/  (panel) and /api/snapshot (JSON)");
        for stream in listener.incoming() {
            match stream {
                Ok(s) => self.handle(s),
                Err(e) => eprintln!("accept: {e}"),
            }
        }
        Ok(())
    }

    fn handle(&self, mut stream: TcpStream) {
        // A client that connects and stalls must not wedge the accept loop.
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        });
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        // Drain headers.
        loop {
            let mut h = String::new();
            match reader.read_line(&mut h) {
                Ok(0) => break,
                Ok(_) => {
                    if h.trim().is_empty() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        let snap = {
            let mut c = match self.snapshot_src.lock() {
                Ok(c) => c,
                Err(_) => return,
            };
            c.snapshot(self.trade_limit)
        };
        let (ctype, body) = if line.contains("/api/snapshot") {
            ("application/json", render_json(&snap))
        } else {
            ("text/html; charset=utf-8", render_html(&snap))
        };
        if std::env::var("UIKIT_WEB_TRACE").is_ok() {
            eprintln!("ui_kit web: {} -> {} bytes (connected={})", line.trim(), body.len(), snap.connected);
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
    }
}
