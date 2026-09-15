//! ratatui rendering for the panel. Pure functions over `App` — no I/O.

use crate::app::{App, Tab};
use blitzkrieg_ui_kit::UiSnapshot;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs, Wrap};

const GREEN: Color = Color::Green;
const RED: Color = Color::Red;
const DIM: Color = Color::DarkGray;
const ACCENT: Color = Color::Cyan;

fn signed(v: f64, unit: &str) -> String {
    format!("{}{:.2}{}", if v >= 0.0 { "+" } else { "-" }, v.abs(), unit)
}
fn signed_pct(v: f64) -> String {
    format!("{}{:.1}%", if v >= 0.0 { "+" } else { "-" }, v.abs())
}
fn color_of(v: f64) -> Color {
    if v >= 0.0 {
        GREEN
    } else {
        RED
    }
}

pub fn render(f: &mut Frame, app: &App) {
    let confirm_h = if app.pending_confirmation.is_some() {
        3
    } else {
        0
    };
    let chunks = Layout::vertical([
        Constraint::Length(3),         // header
        Constraint::Length(3),         // tabs
        Constraint::Min(8),            // body
        Constraint::Length(3),         // command bar
        Constraint::Length(confirm_h), // confirm bar (only when pending)
        Constraint::Length(6),         // log
    ])
    .split(f.area());

    render_header(f, chunks[0], app);
    render_tabs(f, chunks[1], app);
    match app.tab {
        Tab::Overview => render_overview(f, chunks[2], &app.snap),
        Tab::Positions => render_positions(f, chunks[2], &app.snap),
        Tab::Trades => render_trades(f, chunks[2], &app.snap),
        Tab::Plugins => render_plugins(f, chunks[2], app),
    }
    render_command_bar(f, chunks[3], app);
    if let Some(text) = &app.pending_confirmation {
        render_confirm(f, chunks[4], text);
    }
    // The log pane is always the LAST chunk: the confirm slot is a zero-height
    // chunk when nothing is pending, so indexing by confirm_h pointed the log
    // into a 0-row rect and hid it entirely.
    render_log(f, chunks[5], app);
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let s = &app.snap;
    let conn = if s.connected {
        Span::styled("● connected", Style::default().fg(GREEN))
    } else {
        Span::styled("○ offline", Style::default().fg(RED))
    };
    let mode = s.mode();
    let mode_span = if mode == "live" {
        Span::styled(
            "LIVE",
            Style::default()
                .fg(Color::Black)
                .bg(RED)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("DRY", Style::default().fg(Color::Black).bg(ACCENT))
    };
    let owner = if app.managed {
        Span::styled(
            format!(
                "managed pid {}",
                app.pid.map(|p| p.to_string()).unwrap_or_default()
            ),
            Style::default().fg(ACCENT),
        )
    } else if s.connected {
        Span::styled("adopted (not owned)", Style::default().fg(DIM))
    } else {
        Span::styled("not running", Style::default().fg(DIM))
    };
    let age = app
        .last_update
        .map(|t| format!("{}s ago", t.elapsed().as_secs()))
        .unwrap_or_else(|| "—".into());
    let gate = if app.lifecycle_enabled {
        Span::styled("lifecycle:ON", Style::default().fg(GREEN))
    } else {
        Span::styled("lifecycle:off", Style::default().fg(DIM))
    };

    let line1 = Line::from(vec![
        Span::styled(
            "Blitzkrieg Panel",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        mode_span,
        Span::raw("  "),
        conn,
        Span::raw("  "),
        owner,
        Span::raw("  "),
        gate,
    ]);
    let line2 = Line::from(vec![
        Span::styled("socket: ", Style::default().fg(DIM)),
        Span::raw(app.socket.clone()),
        Span::styled("   updated ", Style::default().fg(DIM)),
        Span::raw(age),
    ]);
    let p = Paragraph::new(vec![line1, line2])
        .block(Block::default().borders(Borders::ALL).title("Status"));
    f.render_widget(p, area);
}

fn render_tabs(f: &mut Frame, area: Rect, app: &App) {
    let tabs = Tabs::new(Tab::titles())
        .select(app.tab.index())
        .block(Block::default().borders(Borders::ALL).title("View"))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, area);
}

fn stat(title: &str, big: String, sub: String, big_style: Style) -> Paragraph<'static> {
    Paragraph::new(vec![
        Line::from(Span::styled(big, big_style.add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(sub, Style::default().fg(DIM))),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(title.to_string()),
    )
}

fn render_overview(f: &mut Frame, area: Rect, s: &UiSnapshot) {
    let rows = Layout::vertical([Constraint::Length(5), Constraint::Min(4)]).split(area);
    let cards = Layout::horizontal([
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
    ])
    .split(rows[0]);

    // Round
    let (big, sub, style) = match &s.round {
        Some(r) => (
            format!("#{}", r.slot),
            format!(
                "{}s old · {}s left · {}",
                r.age_sec,
                r.time_left_sec,
                if r.can_trade { "TRADING" } else { "WAITING" }
            ),
            Style::default().fg(if r.can_trade { GREEN } else { DIM }),
        ),
        None => (
            "—".into(),
            "engine not running".into(),
            Style::default().fg(DIM),
        ),
    };
    f.render_widget(stat("Round", big, sub, style), cards[0]);

    // Balance
    let (big, sub) = match &s.balance {
        Some(b) => (
            format!("${:.2}", b.balance),
            format!("reserved ${:.2} · avail ${:.2}", b.reserved, b.available),
        ),
        None => ("—".into(), "no balance".into()),
    };
    f.render_widget(
        stat("Balance", big, sub, Style::default().fg(ACCENT)),
        cards[1],
    );

    // Feed
    let (big, sub) = match &s.stats {
        Some(st) => (
            format!("{} books", st.books),
            format!(
                "signals {} · rejected {} · confirmed {}",
                st.signals,
                st.place_rejected,
                st.confirmed.len()
            ),
        ),
        None => ("—".into(), "no feed stats".into()),
    };
    f.render_widget(
        stat("Feed", big, sub, Style::default().fg(ACCENT)),
        cards[2],
    );

    // Net PnL
    let net = s.net_pnl();
    f.render_widget(
        stat(
            "Net PnL",
            signed(net, ""),
            format!("{} trades · {:.0}% WR", s.trades.len(), s.win_rate()),
            Style::default().fg(color_of(net)),
        ),
        cards[3],
    );

    // Prices
    let body = rows[1];
    let mut lines: Vec<Line> = Vec::new();
    if let Some(r) = &s.round {
        if r.market_prices.is_empty() {
            lines.push(Line::from(Span::styled(
                "(no market prices yet)",
                Style::default().fg(DIM),
            )));
        } else {
            for m in &r.market_prices {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:<5}", m.asset),
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  UP ", Style::default().fg(DIM)),
                    Span::raw(format!("{:.2}", m.up)),
                    Span::styled("   DOWN ", Style::default().fg(DIM)),
                    Span::raw(format!("{:.2}", m.down)),
                    Span::styled("   spread ", Style::default().fg(DIM)),
                    Span::raw(format!("{:.1}c", (m.up + m.down - 1.0) * 100.0)),
                ]));
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            "(engine not running)",
            Style::default().fg(DIM),
        )));
    }
    if let Some(st) = &s.stats {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "feed: books={} tops={} spots={} rounds={} evaluations={} · blocked timing={} momentum={}",
                st.books, st.tops, st.spots, st.rounds, st.evaluations, st.blocked.timing, st.blocked.momentum
            ),
            Style::default().fg(DIM),
        )));
        if !st.confirmed.is_empty() {
            // `confirmed` holds raw token IDs (77-digit decimal strings) — useless
            // to print in full. Show the count and a short prefix per token.
            let preview: Vec<String> = st
                .confirmed
                .iter()
                .take(6)
                .map(|t| format!("{}…", &t[..t.len().min(8)]))
                .collect();
            let more = if st.confirmed.len() > 6 {
                format!(" (+{})", st.confirmed.len() - 6)
            } else {
                String::new()
            };
            lines.push(Line::from(vec![
                Span::styled("trend confirmed: ", Style::default().fg(DIM)),
                Span::raw(format!("{}{}", preview.join(", "), more)),
            ]));
        }
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Markets"))
            .wrap(Wrap { trim: false }),
        body,
    );
}

fn render_positions(f: &mut Frame, area: Rect, s: &UiSnapshot) {
    let header = Row::new(vec![
        "Asset", "Dir", "Strategy", "Entry", "Cur", "PnL", "Left",
    ])
    .style(Style::default().fg(DIM).add_modifier(Modifier::BOLD));
    let widths = [
        Constraint::Length(7),
        Constraint::Length(5),
        Constraint::Length(11),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Length(9),
        Constraint::Length(7),
    ];
    let rows: Vec<Row> = s
        .positions
        .iter()
        .map(|p| {
            Row::new(vec![
                Cell::from(p.asset.clone()),
                Cell::from(p.direction.to_uppercase()),
                Cell::from(p.strategy.clone()),
                Cell::from(format!("{:.2}", p.entry_price)),
                Cell::from(format!("{:.2}", p.current_price)),
                Cell::from(Span::styled(
                    signed_pct(p.unrealized_pct),
                    Style::default().fg(color_of(p.unrealized_pct)),
                )),
                Cell::from(format!("{}s", p.remaining_sec)),
            ])
        })
        .collect();
    let table = Table::new(rows, widths).header(header).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Open Positions ({})", s.positions.len())),
    );
    f.render_widget(table, area);
}

fn render_trades(f: &mut Frame, area: Rect, s: &UiSnapshot) {
    let header = Row::new(vec![
        "Asset",
        "Dir",
        "Strategy",
        "Entry→Exit",
        "Net",
        "Net%",
        "Hold",
        "Reason",
    ])
    .style(Style::default().fg(DIM).add_modifier(Modifier::BOLD));
    let widths = [
        Constraint::Length(7),
        Constraint::Length(5),
        Constraint::Length(11),
        Constraint::Length(14),
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Min(10),
    ];
    // Newest first, cap the visible list.
    let rows: Vec<Row> = s
        .trades
        .iter()
        .rev()
        .take(200)
        .map(|t| {
            Row::new(vec![
                Cell::from(t.asset.clone()),
                Cell::from(t.direction.to_uppercase()),
                Cell::from(t.strategy.clone()),
                Cell::from(format!("{:.2}→{:.2}", t.entry_price, t.exit_price)),
                Cell::from(Span::styled(
                    signed(t.net_pnl_usd, ""),
                    Style::default().fg(color_of(t.net_pnl_usd)),
                )),
                Cell::from(Span::styled(
                    signed_pct(t.net_pnl_pct),
                    Style::default().fg(color_of(t.net_pnl_pct)),
                )),
                Cell::from(format!("{}s", t.hold_time_sec)),
                Cell::from(t.exit_reason.clone()),
            ])
        })
        .collect();
    let table = Table::new(rows, widths).header(header).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Closed Trades ({})", s.trades.len())),
    );
    f.render_widget(table, area);
}

/// The plugin-manager view (E5-a): strategies / market plugins / extensions in
/// three columns. Strategies and extensions are toggleable at the cursor row
/// (`Enter`); market plugins are view-only here (registry-side `enabled` is a
/// core-market config choice, not a runtime toggle).
fn render_plugins(f: &mut Frame, area: Rect, app: &App) {
    if !app.snap.connected {
        let msg = app
            .snap
            .last_error
            .clone()
            .unwrap_or_else(|| "core not reachable — plugin registry unavailable".into());
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "○ plugin registry offline",
                    Style::default().fg(RED),
                )),
                Line::from(Span::styled(msg, Style::default().fg(DIM))),
                Line::from(Span::styled(
                    "Start the core, then press 4 again to re-read.",
                    Style::default().fg(DIM),
                )),
            ])
            .block(Block::default().borders(Borders::ALL).title("Plugins"))
            .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let cols = Layout::horizontal([
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
    ])
    .split(area);

    // Cursor is flattened: strategies first, then extensions.
    let n_strat = app.snap.strategies.len();
    let n_ext = app.snap.extensions.len();
    let total = n_strat + n_ext;
    let focus = if total == 0 {
        0
    } else {
        app.plugin_focus.min(total - 1)
    };

    // Strategies
    let mut lines: Vec<Line> = Vec::new();
    if n_strat == 0 {
        lines.push(Line::from(Span::styled("(none)", Style::default().fg(DIM))));
    } else {
        for (i, s) in app.snap.strategies.iter().enumerate() {
            let focused = focus < n_strat && focus == i;
            lines.push(plugin_line(&s.name, s.enabled, focused));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter toggle · ↑/↓ move · disabled →",
        Style::default().fg(DIM),
    )));
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Strategies"))
            .wrap(Wrap { trim: false }),
        cols[0],
    );

    // Market plugins (view-only)
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("registry active: {}", app.snap.market_active),
        Style::default().fg(if app.snap.market_active { GREEN } else { DIM }),
    )));
    lines.push(Line::from(""));
    if app.snap.market_plugins.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no plugins)",
            Style::default().fg(DIM),
        )));
    } else {
        for p in &app.snap.market_plugins {
            let color = if p.active {
                GREEN
            } else if p.enabled {
                ACCENT
            } else {
                DIM
            };
            lines.push(Line::from(vec![
                Span::styled(
                    if p.active { "▸ " } else { "  " },
                    Style::default().fg(color),
                ),
                Span::styled(
                    format!("{:<14}", p.name),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {}", p.kind), Style::default().fg(DIM)),
            ]));
            let mut caps = String::from("      ");
            if p.has_data_feed {
                caps.push_str("feed ");
            }
            if p.has_discovery {
                caps.push_str("discovery ");
            }
            if p.has_executor {
                caps.push_str("executor");
            }
            lines.push(Line::from(Span::styled(caps, Style::default().fg(DIM))));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "view-only (market config in core)",
        Style::default().fg(DIM),
    )));
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Market Plugins"),
            )
            .wrap(Wrap { trim: false }),
        cols[1],
    );

    // Extensions
    let mut lines: Vec<Line> = Vec::new();
    if n_ext == 0 {
        lines.push(Line::from(Span::styled("(none)", Style::default().fg(DIM))));
    } else {
        for (i, e) in app.snap.extensions.iter().enumerate() {
            let on = e.state == "enabled";
            let focused = focus >= n_strat && focus - n_strat == i;
            lines.push(plugin_line(&e.name, on, focused));
            if !e.kind.is_empty() && e.kind != e.name {
                lines.push(Line::from(Span::styled(
                    format!("    {}", e.kind),
                    Style::default().fg(DIM),
                )));
            }
        }
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Extensions ({})", n_ext)),
            )
            .wrap(Wrap { trim: false }),
        cols[2],
    );
}

fn plugin_line(name: &str, on: bool, focused: bool) -> Line<'static> {
    let mark = if on { "[on ]" } else { "[off]" };
    let (mark_style, name_style) = if on {
        (Style::default().fg(GREEN), Style::default())
    } else {
        (Style::default().fg(DIM), Style::default().fg(DIM))
    };
    let name_style = if focused {
        name_style.bg(Color::Black).add_modifier(Modifier::BOLD)
    } else {
        name_style
    };
    let cursor = if focused { "▸ " } else { "  " };
    Line::from(vec![
        Span::styled(cursor, Style::default().fg(ACCENT)),
        Span::styled(format!("{:>5} ", mark), mark_style),
        Span::styled(name.to_string(), name_style),
    ])
}

fn render_confirm(f: &mut Frame, area: Rect, text: &str) {
    let p = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("⚠ ", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
            Span::styled(text, Style::default().fg(RED).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(Span::styled(
            "y = confirm  ·  any other key = cancel  (disabling may strand open positions)",
            Style::default().fg(DIM),
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Confirm")
            .border_style(Style::default().fg(RED)),
    );
    f.render_widget(p, area);
}

fn render_command_bar(f: &mut Frame, area: Rect, app: &App) {
    let title = if app.input_active {
        "Command (Enter run · Esc cancel)"
    } else {
        "Command"
    };
    let body = if app.input_active {
        Line::from(vec![
            Span::styled(": ", Style::default().fg(ACCENT)),
            Span::raw(app.input.clone()),
            Span::styled("█", Style::default().fg(ACCENT)),
        ])
    } else {
        Line::from(Span::styled(
            "press : to type a command   (status · start BTC,ETH --dry-run · stop · positions 25)",
            Style::default().fg(DIM),
        ))
    };
    f.render_widget(
        Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn render_log(f: &mut Frame, area: Rect, app: &App) {
    let visible: Vec<Line> = app
        .logs
        .iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
        .rev()
        .map(|l| Line::from(l.clone()))
        .collect();
    let body = if visible.is_empty() {
        vec![Line::from(Span::styled(
            "(no commands run yet)",
            Style::default().fg(DIM),
        ))]
    } else {
        visible
    };
    f.render_widget(
        Paragraph::new(body)
            .block(Block::default().borders(Borders::ALL).title("Log"))
            .wrap(Wrap { trim: false }),
        area,
    );
}
