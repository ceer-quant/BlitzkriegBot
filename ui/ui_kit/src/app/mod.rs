//! App adapter — the native-app (Tauri/egui) seam.
//!
//! We do not pull a GUI toolkit into the workspace (that would drag in a large
//! platform dependency and cannot be verified headlessly). Instead this adapter
//! defines the *view-model boundary* the future native panel renders, plus a
//! headless renderer, so the same snapshot the web/TUI adapters use can be
//! driven by a native frontend without change.
//!
//! The contract: a frontend calls `AppViewModel::refresh` (via the shared IPC
//! client), which returns the render-ready `AppView`. All of it is plain data —
//! the native shell only draws it.

use crate::core::ipc_client::IpcClient;
use crate::core::types::UiSnapshot;

/// A flat, render-ready view model for a native panel.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AppView {
    pub connected: bool,
    pub mode: String,
    pub headline: String,
    pub balance: f64,
    pub round_slot: i64,
    pub time_left_sec: i64,
    pub can_trade: bool,
    pub open_positions: usize,
    pub trades: usize,
    pub win_rate: f64,
    pub net_pnl: f64,
    pub rows: Vec<AppRow>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AppRow {
    pub asset: String,
    pub direction: String,
    pub entry: f64,
    pub current: f64,
    pub unrealized_pct: f64,
}

impl AppView {
    pub fn from_snapshot(s: &UiSnapshot) -> Self {
        let mut v = AppView {
            connected: s.connected,
            mode: s.mode().to_string(),
            balance: s.balance.as_ref().map(|b| b.balance).unwrap_or(0.0),
            round_slot: s.round.as_ref().map(|r| r.slot).unwrap_or(0),
            time_left_sec: s.round.as_ref().map(|r| r.time_left_sec).unwrap_or(0),
            can_trade: s.round.as_ref().map(|r| r.can_trade).unwrap_or(false),
            open_positions: s.positions.len(),
            trades: s.trades.len(),
            win_rate: s.win_rate(),
            net_pnl: s.net_pnl(),
            last_error: s.last_error.clone(),
            ..Default::default()
        };
        v.headline = format!(
            "{} · {} · round #{} {}s left",
            v.mode.to_uppercase(),
            if v.connected {
                "connected"
            } else {
                "DISCONNECTED"
            },
            v.round_slot,
            v.time_left_sec
        );
        for p in &s.positions {
            v.rows.push(AppRow {
                asset: p.asset.clone(),
                direction: p.direction.clone(),
                entry: p.entry_price,
                current: p.current_price,
                unrealized_pct: p.unrealized_pct,
            });
        }
        v
    }
}

/// Owns the IPC connection and the latest view model for a native frontend.
pub struct AppViewModel {
    client: IpcClient,
    trade_limit: usize,
    view: AppView,
}

impl AppViewModel {
    pub fn new(client: IpcClient, trade_limit: usize) -> Self {
        Self {
            client,
            trade_limit,
            view: AppView::default(),
        }
    }

    /// Pull a fresh snapshot and update the view model. Returns the new view.
    pub fn refresh(&mut self) -> &AppView {
        let snap = self.client.snapshot(self.trade_limit);
        self.view = AppView::from_snapshot(&snap);
        &self.view
    }
}

/// Headless renderer: prints the view model the way a native panel would lay it
/// out. Used to verify the app adapter without a GUI toolkit.
pub fn render_headless(v: &AppView) -> String {
    let mut o = String::new();
    o.push_str("┌─ Blitzkrieg UI Kit — App (headless) ─────────────\n");
    o.push_str(&format!("│ {}\n", v.headline));
    o.push_str(&format!(
        "│ balance ${:.2} · open {} · trades {} · WR {:.0}% · net {:+.2}\n",
        v.balance, v.open_positions, v.trades, v.win_rate, v.net_pnl
    ));
    if v.rows.is_empty() {
        o.push_str("│ (no open positions)\n");
    } else {
        for r in &v.rows {
            o.push_str(&format!(
                "│ {} {} {:.2}→{:.2} {:+.1}%\n",
                r.asset,
                r.direction.to_uppercase(),
                r.entry,
                r.current,
                r.unrealized_pct
            ));
        }
    }
    if let Some(e) = &v.last_error {
        o.push_str(&format!("│ last error: {e}\n"));
    }
    o.push_str("└──────────────────────────────────────────────────\n");
    o
}
