//! Split out of the parent module: pure move.
//! The cockpit frame: the root render dispatch, header, tabs, hint bar,
//! kill banner, confirm bar, command bar and log pane.

use super::evolution::render_evolution;
use super::overlays::{render_accounts, render_help, render_net_check};
use super::panes::{
    render_decisions, render_orders, render_overview, render_plugins, render_positions,
    render_trades,
};
use super::settings::render_settings;
use super::*;

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
        Tab::Orders => render_orders(f, chunks[2], app),
        Tab::Trades => render_trades(f, chunks[2], &app.snap),
        Tab::Plugins => render_plugins(f, chunks[2], app),
        Tab::Decisions => render_decisions(f, chunks[2], app),
        Tab::Evolution => render_evolution(f, chunks[2], app),
        Tab::Settings => render_settings(f, chunks[2], app),
    }
    // Kill switch engaged: the body talks with one voice until resume.
    if let Some(msg) = &app.kill_banner {
        render_kill_banner(f, chunks[2], msg);
    }
    if app.help_visible {
        render_help(f, app);
    }
    if app.net_visible {
        render_net_check(f, app);
    }
    if app.accounts_visible {
        render_accounts(f, app);
    }
    render_command_bar(f, chunks[3], app);
    if let Some(text) = &app.pending_confirmation {
        render_confirm(f, chunks[4], text);
    }
    // The log pane is always the LAST chunk: the confirm slot is a zero-height
    // chunk when nothing is pending, so indexing by confirm_h pointed the log
    // into a 0-row rect and hid it entirely.
    render_log(f, chunks[5], app);
    render_hint_bar(f, f.area(), app);
}

/// Bottom status line: the self-check stages, then one rotating hint the user
/// has not consumed yet (`?` help, `:` command, tab keys…).
fn render_hint_bar(f: &mut Frame, area: Rect, app: &App) {
    if area.height < 1 {
        return;
    }
    let line = Rect {
        y: area.bottom().saturating_sub(1),
        height: 1,
        ..area
    };
    let check = match app.check {
        CheckStage::Connecting => Span::styled("○ connecting", Style::default().fg(RED)),
        CheckStage::Handshake => Span::styled(
            "◐ connected · waiting for market data",
            Style::default().fg(WARN),
        ),
        CheckStage::Ready => Span::styled("● self-check passed", Style::default().fg(GREEN)),
    };
    // Rotate over unconsumed hints; all consumed → quiet. The rotation advances
    // every 5 s of wall time driven by the snapshot age, good enough for a hint.
    let elapsed = app.last_update.map(|t| t.elapsed().as_secs()).unwrap_or(0) as usize;
    let open: Vec<usize> = (0..HINTS.len()).filter(|i| !app.hints_used[*i]).collect();
    let hint_span = match open.first() {
        Some(&i) => {
            let idx = (elapsed / 5) % HINTS.len();
            if app.hints_used[idx] || i == idx {
                Span::styled(format!("  💡 {}", HINTS[i]), Style::default().fg(ACCENT))
            } else {
                Span::styled(format!("  💡 {}", HINTS[idx]), Style::default().fg(ACCENT))
            }
        }
        None => Span::default(),
    };
    f.render_widget(Paragraph::new(Line::from(vec![check, hint_span])), line);
}

/// Full-screen red notice while the core kill switch is engaged.
fn render_kill_banner(f: &mut Frame, area: Rect, msg: &str) {
    let inner = centered_rect(area, 60, 5);
    f.render_widget(Clear, inner);
    let p = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "TRADING HALTED — KILL SWITCH ENGAGED",
            Style::default()
                .fg(Color::Black)
                .bg(RED)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(msg.to_string(), Style::default().fg(RED))),
        Line::from(Span::styled(
            "risk.resume clears this banner (see Log for the trigger).",
            Style::default().fg(DIM),
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Risk")
            .border_style(Style::default().fg(RED).add_modifier(Modifier::BOLD)),
    );
    f.render_widget(p, inner);
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
