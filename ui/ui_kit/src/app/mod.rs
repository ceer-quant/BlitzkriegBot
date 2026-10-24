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
        // issue 550 / issue 559 显示面口径（TUI headline 漏网）：全局标量块
        // 是各 venue 最早到期的保守钟，多族并存时原生面板 headline 却读它
        // —— 显示账户绑着 kalshi 时 headline 标的是 polymarket 的 slot。
        // 回落链共用 `UiSnapshot::display_venue_round`：displayVenue 行优先
        // → 第一个非空行 → 全局标量块（单族逐字节现行为）。
        let (round_slot, time_left_sec) = match s.display_venue_round() {
            Some(v) => (v.slot, v.time_left_sec),
            None => (
                s.round.as_ref().map(|r| r.slot).unwrap_or(0),
                s.round.as_ref().map(|r| r.time_left_sec).unwrap_or(0),
            ),
        };
        let mut v = AppView {
            connected: s.connected,
            mode: s.mode().to_string(),
            balance: s.balance.as_ref().map(|b| b.balance).unwrap_or(0.0),
            round_slot,
            time_left_sec,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{RoundView, VenueRoundView};

    /// issue 550 / issue 559 显示面口径（TUI headline 漏网）：the native
    /// panel's headline (and `round_slot`/`time_left_sec`) must follow the
    /// display account's own venue round row when `venue_rounds` is
    /// populated, and stay byte-for-byte on the global scalar block when the
    /// rows are absent (pre-issue-550 core → empty serde default).
    #[test]
    fn the_headline_follows_the_display_venue_row_then_the_global_block() {
        let snap = |rows: Vec<VenueRoundView>, display_venue: &str| UiSnapshot {
            connected: true,
            round: Some(RoundView {
                slot: 7,
                age_sec: 1,
                time_left_sec: 299,
                markets: 2,
                can_trade: true,
                venue_rounds: rows,
                ..RoundView::default()
            }),
            display_venue: display_venue.into(),
            ..UiSnapshot::default()
        };

        // Multi-family: display venue kalshi's own 3600s clock wins the
        // headline (slot AND time left), not the conservative earliest-expiry
        // global block.
        let v = AppView::from_snapshot(&snap(
            vec![
                VenueRoundView {
                    venue: "polymarket".into(),
                    duration_sec: 900,
                    slot: 7,
                    age_sec: 1,
                    time_left_sec: 299,
                },
                VenueRoundView {
                    venue: "kalshi".into(),
                    duration_sec: 3600,
                    slot: 3,
                    age_sec: 5,
                    time_left_sec: 295,
                },
            ],
            "kalshi",
        ));
        assert_eq!(
            v.round_slot, 3,
            "the headline slot is the display venue's own"
        );
        assert_eq!(
            v.time_left_sec, 295,
            "the headline countdown is the display venue's own"
        );
        assert!(
            v.headline.contains("round #3 295s left"),
            "the rendered headline reads the display venue's row: {}",
            v.headline
        );

        // No display pointer (older core): falls back to the first spoken row —
        // with a single family that is the same clock a legacy headline showed.
        let v = AppView::from_snapshot(&snap(
            vec![VenueRoundView {
                venue: "polymarket".into(),
                duration_sec: 900,
                slot: 7,
                age_sec: 1,
                time_left_sec: 299,
            }],
            "",
        ));
        assert_eq!(v.round_slot, 7);
        assert_eq!(v.time_left_sec, 299);

        // Legacy equivalence: without venue_rounds the headline fields are the
        // global scalar block, exactly as before this change.
        let v = AppView::from_snapshot(&snap(Vec::new(), "kalshi"));
        assert_eq!(v.round_slot, 7);
        assert_eq!(v.time_left_sec, 299);
        assert!(
            v.headline.contains("round #7 299s left"),
            "the legacy headline text is byte-for-byte unchanged: {}",
            v.headline
        );
    }
}
