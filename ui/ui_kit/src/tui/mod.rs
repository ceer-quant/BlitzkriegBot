//! TUI adapter — renders a `UiSnapshot` to ANSI text and runs a polling loop.
//! The terminal sibling of the web/app adapters; no trading logic.

use crate::core::ipc_client::IpcClient;
use crate::core::types::UiSnapshot;
use std::io::Write;
use std::time::Duration;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const CLEAR: &str = "\x1b[2J\x1b[H";

fn signed(v: f64, unit: &str) -> String {
    format!("{}{:.2}{}", if v >= 0.0 { "+" } else { "-" }, v.abs(), unit)
}

/// Render the snapshot as an ANSI screen (no trailing newline management).
pub fn render(s: &UiSnapshot) -> String {
    let mut o = String::new();
    o.push_str(CLEAR);
    o.push_str(&format!("{BOLD}{CYAN}Blitzkrieg UI Kit — TUI{RESET}\n"));
    o.push_str(&format!(
        "{DIM}mode={} · connected={} · source=UDS JSON-RPC (read-only){RESET}\n\n",
        s.mode(),
        s.connected
    ));

    if let Some(r) = &s.round {
        o.push_str(&format!(
            "  Round {BOLD}#{}{RESET}  {}s old  {BOLD}{}s left{RESET}  [{}]\n",
            r.slot,
            r.age_sec,
            r.time_left_sec,
            if r.can_trade { format!("{GREEN}TRADING{RESET}") } else { format!("{DIM}WAITING{RESET}") }
        ));
    } else {
        o.push_str(&format!("  {DIM}round: (engine not running){RESET}\n"));
    }
    if let Some(b) = &s.balance {
        o.push_str(&format!(
            "  Balance ${:.2}   reserved ${:.2}   available ${:.2}\n",
            b.balance, b.reserved, b.available
        ));
    }
    let net = s.net_pnl();
    let col = if net >= 0.0 { GREEN } else { RED };
    o.push_str(&format!(
        "  Trades {}   WR {:.0}%   Net {col}{}{RESET}\n",
        s.trades.len(),
        s.win_rate(),
        signed(net, "")
    ));
    if let Some(st) = &s.stats {
        o.push_str(&format!(
            "  {DIM}books={} signals={} rejected={} confirmed={}{RESET}\n",
            st.books,
            st.signals,
            st.place_rejected,
            st.confirmed.len()
        ));
    }

    o.push_str(&format!("\n{BOLD}Open Positions{RESET}\n"));
    if s.positions.is_empty() {
        o.push_str(&format!("  {DIM}(none){RESET}\n"));
    } else {
        o.push_str(&format!("  {DIM}{:<6} {:<5} {:>6} {:>6} {:>8} {:>6}{RESET}\n", "ASSET", "DIR", "ENTRY", "CUR", "PNL", "LEFT"));
        for p in &s.positions {
            let c = if p.unrealized_pct >= 0.0 { GREEN } else { RED };
            o.push_str(&format!(
                "  {:<6} {:<5} {:>6.2} {:>6.2} {c}{:>8}{RESET} {:>5}s\n",
                p.asset,
                p.direction.to_uppercase(),
                p.entry_price,
                p.current_price,
                signed(p.unrealized_pct, "%"),
                p.remaining_sec
            ));
        }
    }

    o.push_str(&format!("\n{BOLD}Recent Trades{RESET}\n"));
    if s.trades.is_empty() {
        o.push_str(&format!("  {DIM}(none){RESET}\n"));
    } else {
        for t in s.trades.iter().rev().take(10) {
            let c = if t.net_pnl_usd >= 0.0 { GREEN } else { RED };
            o.push_str(&format!(
                "  {:<6} {:<5} {:.2}→{:.2}  {c}{:>9}{RESET}  {}\n",
                t.asset,
                t.direction.to_uppercase(),
                t.entry_price,
                t.exit_price,
                signed(t.net_pnl_usd, "$"),
                t.exit_reason
            ));
        }
    }

    if let Some(r) = &s.round {
        if !r.market_prices.is_empty() {
            o.push_str(&format!("\n{BOLD}Prices{RESET}\n  "));
            for m in &r.market_prices {
                o.push_str(&format!("{} ↑{:.2} ↓{:.2}   ", m.asset, m.up, m.down));
            }
            o.push('\n');
        }
    }
    if let Some(e) = &s.last_error {
        o.push_str(&format!("\n{RED}last error: {e}{RESET}\n"));
    }
    o.push_str(&format!("\n{DIM}Ctrl-C to exit{RESET}\n"));
    o
}

/// Poll the core every `interval` and redraw. Runs until the process is stopped.
pub fn run(client: &mut IpcClient, interval: Duration, trade_limit: usize) -> std::io::Result<()> {
    loop {
        let snap = client.snapshot(trade_limit);
        print!("{}", render(&snap));
        std::io::stdout().flush()?;
        std::thread::sleep(interval);
    }
}
