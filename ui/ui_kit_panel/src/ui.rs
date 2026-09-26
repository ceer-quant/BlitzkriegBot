//! ratatui rendering for the panel. Pure functions over `App` — no I/O.

use crate::app::{App, CheckStage, Tab, HINTS};
use blitzkrieg_ui_kit::core::types::EvolutionProposalView;
use blitzkrieg_ui_kit::gateway::command_lines;
use blitzkrieg_ui_kit::UiSnapshot;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Tabs, Wrap};

const GREEN: Color = Color::Green;
const RED: Color = Color::Red;
const DIM: Color = Color::DarkGray;
const ACCENT: Color = Color::Cyan;
/// The attention colour: noticed, but not a failure. It exists as a name because
/// the WebUI renders the same facts at the same volume (see `tui-parity`), and
/// "which tier is this fact in" is a decision the two faces have to share — a
/// bare `Color::Yellow` at the call site is how the two sides drifted apart.
const WARN: Color = Color::Yellow;

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

fn centered_rect(area: Rect, pct_x: u16, height: u16) -> Rect {
    let w = area.width * pct_x / 100;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width: w.min(area.width),
        height: height.min(area.height),
    }
}

/// The `?` overlay: every key and command in one screen.
fn render_help(f: &mut Frame, _app: &App) {
    let area = f.area();
    let mut keys = vec![
        Line::from(Span::styled(
            "KEYS",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  : /            focus command bar (then type a command, Enter runs it)"),
        Line::from("  1-5            pages: Overview / Positions / Trades / Plugins / Evolution"),
        Line::from("  Tab            next page"),
        Line::from(
            "  ↑/↓            Plugins/Evolution: move selection · Command bar: recall history",
        ),
        Line::from("  Tab (bar)      complete the command"),
        Line::from("  Enter /Esc     run / cancel"),
        Line::from("  r              refresh now"),
        Line::from("  n              network check — which path is broken, if any"),
        Line::from("  y / n          confirm or cancel a dangerous action"),
        Line::from("  a / x / d      Evolution: accept / reject / defer the selected proposal"),
        Line::from("  e / u          Evolution: toggle auto-evolve / rollback selected strategy"),
        Line::from(
            "  m              Evolution: toggle the engine itself (nothing evolves while off)",
        ),
        Line::from("  q or Ctrl-C    quit"),
        Line::from(""),
        Line::from(Span::styled(
            "COMMANDS (in the bar)",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
    ];
    for line in command_lines() {
        keys.push(Line::from(line));
    }
    keys.push(Line::from(Span::styled(
        "  start / stop need --manage",
        Style::default().fg(DIM),
    )));
    // Sized to the content: the list grows with the gateway's command table, and
    // a fixed height would silently clip its tail.
    let rect = centered_rect(area, 80, (keys.len() + 2) as u16);
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(keys).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Help — Esc to close")
                .border_style(Style::default().fg(ACCENT)),
        ),
        rect,
    );
}

/// The `n` overlay: the network self-check, one row per path.
///
/// Rendered from the report's FIELDS rather than from
/// `core::net_check::render_text`, because a table can colour a failing row and
/// a plain-text renderer cannot. The two must still agree on what each status
/// MEANS, so both call `status_label` — the shared vocabulary is the labels,
/// which is the part that would otherwise drift. They also share the COLUMNS
/// (`row_columns`), so a width change cannot land on one renderer and miss the
/// other.
fn render_net_check(f: &mut Frame, app: &App) {
    use blitzkrieg_ui_kit::core::net_check::{hint_label, row_columns, status_label};
    let area = f.area();

    let mut lines: Vec<Line> = Vec::new();
    match (&app.net_report, &app.net_error) {
        (_, Some(e)) => {
            lines.push(Line::from(Span::styled(
                "the core could not be asked:",
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(format!("  {e}")));
            lines.push(Line::from(Span::styled(
                "  (this is a CORE/IPC problem, not a network verdict — start the core, then press r)",
                Style::default().fg(DIM),
            )));
        }
        (None, None) if app.net_busy => {
            lines.push(Line::from(Span::styled(
                "probing… (resolver → TCP → TLS → one request per path; takes a few seconds)",
                Style::default().fg(ACCENT),
            )));
        }
        (None, None) => {
            lines.push(Line::from("no report yet — press r to probe"));
        }
        (Some(report), None) => {
            let (passed, total) = report.passed();
            let headline = format!(
                "{passed}/{total} paths OK — {}",
                hint_label(&report.hint_code)
            );
            lines.push(Line::from(Span::styled(
                headline,
                Style::default()
                    .fg(if report.ok { GREEN } else { RED })
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            for item in &report.items {
                let mark = if item.ok {
                    Span::styled("ok  ", Style::default().fg(GREEN))
                } else {
                    Span::styled("FAIL", Style::default().fg(RED))
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    mark,
                    Span::raw(format!(" {}  ", row_columns(item))),
                    Span::styled(
                        status_label(&item.status),
                        Style::default().fg(if item.ok { GREEN } else { RED }),
                    ),
                    Span::styled(
                        if item.fake_ip { "  [fake-ip]" } else { "" },
                        Style::default().fg(WARN),
                    ),
                ]));
                if !item.detail.trim().is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("        {}", item.detail.trim()),
                        Style::default().fg(DIM),
                    )));
                }
            }
            if report.items.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  (no probe reported — an empty report is never a pass)",
                    Style::default().fg(RED),
                )));
            }
            lines.push(Line::from(""));
            let reading = if report.hint.trim().is_empty() {
                hint_label(&report.hint_code).to_string()
            } else {
                report.hint.clone()
            };
            lines.push(Line::from(vec![
                Span::styled("reading: ", Style::default().fg(ACCENT)),
                Span::raw(reading),
            ]));
            if !report.proxy_env.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!(
                        "proxy variables set: {} (names only — a proxy URL can carry credentials)",
                        report.proxy_env.join(", ")
                    ),
                    Style::default().fg(WARN),
                )));
            }
        }
    }
    if app.net_busy && app.net_report.is_some() {
        lines.push(Line::from(Span::styled(
            "probing again… (showing the previous report)",
            Style::default().fg(ACCENT),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "r probe again · n or Esc closes · the same report is in the log pane",
        Style::default().fg(DIM),
    )));

    // Height from the CONTENT, not a fixed 20 rows (#260): the report grows a
    // row per path and a wrapped line per long detail, and a hard-coded box
    // silently cropped the tail on exactly the reports worth reading. The
    // paragraph wraps, so the count is of WRAPPED rows — `lines.len()` would
    // under-count a long detail line and crop it just as the fixed height did.
    let inner_w = (area.width * NET_OVERLAY_PCT_X / 100).saturating_sub(2);
    let mut body_rows = wrapped_rows(&lines, inner_w);
    let max_body = area.height.saturating_sub(2) as usize;
    if body_rows > max_body && max_body >= 2 {
        // The screen cannot hold it: say so rather than let the operator read a
        // truncated list as the whole report. The notice takes the last body row.
        lines.push(Line::from(Span::styled(
            format!(
                "… {} more rows below the fold — enlarge the terminal",
                body_rows - (max_body - 1)
            ),
            Style::default().fg(WARN),
        )));
        body_rows = max_body;
    }
    let rect = centered_rect(
        area,
        NET_OVERLAY_PCT_X,
        (body_rows as u16).saturating_add(2).min(area.height),
    );
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Network self-check — is it us or the venue?")
                .border_style(Style::default().fg(ACCENT)),
        ),
        rect,
    );
}

/// Width of the `n` overlay as a percentage of the screen. A constant because
/// the height calculation has to know the same inner width the paragraph will
/// wrap to.
const NET_OVERLAY_PCT_X: u16 = 86;

/// Rows `lines` occupy once wrapped to `width` columns. An empty line still
/// occupies one row; a line wider than the box wraps onto the next.
fn wrapped_rows(lines: &[Line], width: u16) -> usize {
    let width = width.max(1) as usize;
    lines
        .iter()
        .map(|l| {
            let w = l.width();
            if w == 0 {
                1
            } else {
                w.div_ceil(width)
            }
        })
        .sum()
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

/// The arbitration audit (E25 / #331, §13.4's TUI face): the SAME five columns
/// the WebUI's Decisions page shows — 时间 / 策略 / 结果 / 关卡 / 详情 — with the
/// detail column printed VERBATIM from the kernel's GateTrace. The rows are the
/// raw audit tail; no second decision model exists on this side either.
fn render_decisions(f: &mut Frame, area: Rect, app: &App) {
    let header = Row::new(["time", "strategy", "status", "gate", "detail"])
        .style(Style::default().fg(DIM).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = app
        .decisions
        .iter()
        .map(|r| {
            let status = r
                .get("decision")
                .and_then(|d| d.get("status"))
                .and_then(tui_s)
                .unwrap_or_else(|| "?".into());
            let (status_text, color) = match status.as_str() {
                "APPROVED" => ("approved", GREEN),
                "MODIFIED" => ("modified", WARN),
                "REJECTED" => ("rejected", RED),
                _ => (status.as_str(), DIM),
            };
            // REJECTED shows the rejecting gate; otherwise the last trace.
            let gate = if status == "REJECTED" {
                r.get("decision")
                    .and_then(|d| d.get("gate"))
                    .and_then(tui_s)
            } else {
                r.get("gates")
                    .and_then(|g| g.as_array())
                    .and_then(|a| a.last())
                    .and_then(|g| g.get("gate"))
                    .and_then(tui_s)
            }
            .unwrap_or_else(|| "—".into());
            let detail = if status == "REJECTED" {
                r.get("decision")
                    .and_then(|d| d.get("detail"))
                    .and_then(tui_s)
            } else {
                r.get("gates")
                    .and_then(|g| g.as_array())
                    .and_then(|a| a.last())
                    .and_then(|g| g.get("detail"))
                    .and_then(tui_s)
            }
            .unwrap_or_default();
            Row::new([
                Cell::from(
                    r.get("tsMs")
                        .and_then(tui_i64)
                        .map(ts_hms)
                        .unwrap_or_else(|| "—".into()),
                )
                .style(Style::default().fg(DIM)),
                Cell::from(r.get("strategy").and_then(tui_s).unwrap_or_default()),
                Cell::from(status_text.to_string()).style(Style::default().fg(color)),
                Cell::from(gate).style(Style::default().fg(DIM)),
                Cell::from(detail),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(18),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Fill(1),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(format!(
        "Decisions — arbitration audit ({} rows)",
        app.decisions.len()
    )));
    f.render_widget(table, area);
}

fn tui_s(v: &serde_json::Value) -> Option<String> {
    v.as_str().map(String::from)
}

fn tui_i64(v: &serde_json::Value) -> Option<i64> {
    v.as_i64()
}

fn ts_hms(ms: i64) -> String {
    let secs = ms.div_euclid(1000).rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
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

// ── Evolution tab (E13 #95) ──────────────────────────────────────────────────

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Compact human duration ("45s" / "3h" / "2d5h") between two epoch-ms stamps.
fn human_secs(secs: i64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let m = secs / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 48 {
        return format!("{h}h{}m", m % 60);
    }
    format!("{}d{}h", h / 24, h % 24)
}
fn ago(ms: i64, now: i64) -> String {
    human_secs((now - ms).max(0) / 1000)
}

/// The configured deep-round period, read off the status rather than assumed: a
/// hard-coded "72h" is wrong the moment the config file says otherwise, and it
/// was what the panel used to print over any setting (#249).
fn cycle_period(cycle_secs: i64) -> String {
    if cycle_secs <= 0 {
        "unreported".into()
    } else if cycle_secs < 3600 {
        human_secs(cycle_secs)
    } else {
        format!("{}h", cycle_secs / 3600)
    }
}
fn ttl_text(expires_at_ms: i64, now: i64) -> String {
    let left = expires_at_ms - now;
    if left <= 0 {
        "expired".into()
    } else {
        format!("{} left", human_secs(left / 1000))
    }
}
fn short_id(id: &str) -> String {
    let s: String = id.chars().take(16).collect();
    if id.chars().count() > 16 {
        format!("{s}…")
    } else {
        s
    }
}

/// The evolution view: the auto-evolve switch and cycle clock on top, pending
/// proposals (each with its baseline-vs-variant 对比表) in the middle, recent
/// decisions at the bottom. The keys drive the same gateway verbs the command
/// bar accepts (`decide` / `auto-evolve` / `rollback`) — one shared backend.
/// The Settings pane (VERSIONING.md §6.2): version + build provenance + the
/// update three-state. The name `render_settings` is load-bearing — the
/// TUI/WebUI parity gate finds `fn render_<tab>` by this exact spelling.
fn render_settings(f: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();

    // ── VERSION ── the running core's self-description (not a local guess) ──
    lines.push(Line::from(Span::styled(
        "VERSION",
        Style::default().fg(ACCENT),
    )));
    match &app.snap.system_version {
        Some(sv) => {
            lines.push(Line::from(Span::styled(
                format!("  BlitzkriegBot {}", sv.version),
                Style::default().fg(GREEN),
            )));
            lines.push(Line::from(format!(
                "  git     {} ({})",
                if sv.git_hash == "nogit" {
                    "无法指认修订号"
                } else {
                    &sv.git_hash
                },
                if sv.git_dirty { "dirty" } else { "clean" }
            )));
            lines.push(Line::from(format!("  built   {}", sv.build_date)));
            lines.push(Line::from(format!("  target  {}", sv.target)));
        }
        None => {
            lines.push(Line::from(Span::styled(
                "  (no core answered system.version — version unavailable)",
                Style::default().fg(DIM),
            )));
        }
    }
    lines.push(Line::from(""));

    // ── UPDATE ── three states, never collapsed to two (INV-3) ──
    lines.push(Line::from(Span::styled(
        "UPDATE",
        Style::default().fg(ACCENT),
    )));
    let sv = app.snap.system_version.as_ref();
    let (mark, status_text, color) = match sv.map(|v| v.update_available) {
        Some(Some(true)) => (
            "▲",
            format!(
                "{} available",
                sv.and_then(|v| v.latest_version.clone())
                    .unwrap_or_default()
            ),
            WARN,
        ),
        Some(Some(false)) => ("✓", "up to date".to_string(), GREEN),
        _ => (
            "…",
            "not checked (checks are OFF by default)".to_string(),
            DIM,
        ),
    };
    lines.push(Line::from(Span::styled(
        format!("  status   {mark} {status_text}"),
        Style::default().fg(color),
    )));
    let last = sv
        .and_then(|v| v.last_check_ms)
        .map(|ms| utc_stamp((ms / 1000) as i64))
        .unwrap_or_else(|| "—".to_string());
    lines.push(Line::from(format!("  last     {last}")));
    let auto_on = sv.map(|v| v.auto_update).unwrap_or(false);
    let check_on = sv.map(|v| v.check_enabled).unwrap_or(false);
    lines.push(Line::from(format!(
        "  auto     [{}] {}",
        if auto_on { "x" } else { " " },
        if auto_on { "on" } else { "off" }
    )));
    lines.push(Line::from(format!(
        "  check    [{}] {}",
        if check_on { "x" } else { " " },
        if check_on {
            "outbound checks allowed"
        } else {
            "outbound checks disabled"
        }
    )));
    lines.push(Line::from(""));
    if app.update_busy {
        lines.push(Line::from(Span::styled(
            "  working…",
            Style::default().fg(WARN),
        )));
    }
    lines.push(Line::from(Span::styled(
        "  [c] check now   [a] toggle auto-update (asks y/n)   [i] install (needs auto on)",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        "  unknown state is shown as \"… not checked\" — never as \"up to date\"",
        Style::default().fg(DIM),
    )));

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Settings"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Epoch seconds → `YYYY-MM-DD HH:MMZ` (UTC), for the "last check" line.
/// Same Hinnant civil-calendar conversion the build stamper uses — a date must
/// not depend on a date library or on hand-rolled month arithmetic.
fn utc_stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi) = (rem / 3600, (rem % 3600) / 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}Z")
}

fn render_evolution(f: &mut Frame, area: Rect, app: &App) {
    if !app.snap.connected {
        let msg = app
            .snap
            .last_error
            .clone()
            .unwrap_or_else(|| "core not reachable — evolution state unavailable".into());
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "○ evolution offline",
                    Style::default().fg(RED),
                )),
                Line::from(Span::styled(msg, Style::default().fg(DIM))),
                Line::from(Span::styled(
                    "Start the core, then press 5 again to re-read.",
                    Style::default().fg(DIM),
                )),
            ])
            .block(Block::default().borders(Borders::ALL).title("Evolution"))
            .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let now = now_ms();
    let rows = Layout::vertical([
        Constraint::Length(4), // switch + cycle clock
        Constraint::Min(6),    // pending list (+ focused comparison)
        Constraint::Length(8), // recent decisions
    ])
    .split(area);
    render_evo_status(f, rows[0], app, now);
    render_evo_pending(f, rows[1], app, now);
    render_evo_recent(f, rows[2], app, now);
}

fn render_evo_status(f: &mut Frame, area: Rect, app: &App, now: i64) {
    let mut lines: Vec<Line> = Vec::new();
    match &app.snap.evolution_status {
        Some(s) => {
            // Two switches, two questions (#249): whether anything is evaluated at
            // all, and who applies what qualifies. Printing only the second is how
            // this line used to promise "unattended" over an engine doing nothing.
            let (mark, text, color) = if !s.enabled {
                (
                    "○",
                    "engine OFF — nothing is evaluated, held or applied",
                    RED,
                )
            } else if s.auto_evolve {
                (
                    "●",
                    "engine ON · auto-evolve ON — variants apply unattended",
                    GREEN,
                )
            } else {
                (
                    "○",
                    "engine ON · auto-evolve OFF — every proposal waits for you",
                    ACCENT,
                )
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{mark} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    text,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                // Recorded but inert is the one state worth spelling out: the
                // auto switch is on while the engine sleeps, which is exactly the
                // combination that made the panel look like a lie.
                Span::styled(
                    if s.enabled || !s.auto_evolve {
                        "   m engine · e auto"
                    } else {
                        "   (auto-evolve ON is recorded but inert)   m engine · e auto"
                    },
                    Style::default().fg(DIM),
                ),
            ]));
            let last = if s.last_cycle_ms > 0 {
                format!("{} ago", ago(s.last_cycle_ms, now))
            } else {
                "never".into()
            };
            let next = match s.next_cycle_at_ms {
                Some(t) if t > now => format!("in {}", ago(t, now)),
                Some(_) => "due now".into(),
                None => "—".into(),
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "pending {} · deep cycle {} · round {} · last {last} · next {next}",
                    s.pending_proposals,
                    cycle_period(s.cycle_secs),
                    s.cycle_seq,
                ),
                Style::default().fg(DIM),
            )));
        }
        None => lines.push(Line::from(Span::styled(
            "evolution status unknown (older core without the proposal workflow?)",
            Style::default().fg(DIM),
        ))),
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Switch"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn evo_state_color(state: &str) -> Color {
    match state {
        "proposed" => ACCENT,
        "deferred" => WARN,
        "accepted" => GREEN,
        "rejected" => RED,
        _ => DIM,
    }
}

/// The pending-proposal pane. Every entry shows its knob moves and why the
/// evaluator held it; the focused one gets the full 对比表 (baseline vs
/// variant metrics side by side).
fn render_evo_pending(f: &mut Frame, area: Rect, app: &App, now: i64) {
    let pending = app.evo_pending();
    let mut lines: Vec<Line> = Vec::new();
    if pending.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no pending proposals — when a variant beats the incumbent on the live window, it waits here)",
            Style::default().fg(DIM),
        )));
    }
    let focus = if pending.is_empty() {
        0
    } else {
        app.evo_focus.min(pending.len() - 1)
    };
    for (i, p) in pending.iter().enumerate() {
        let focused = i == focus;
        let color = evo_state_color(&p.state);
        let cursor = if focused { "▸ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(cursor, Style::default().fg(ACCENT)),
            Span::styled(
                short_id(&p.id),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {} · ", p.strategy)),
            Span::styled(
                p.state.clone(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " · expires {} · cycle #{}",
                    ttl_text(p.expires_at_ms, now),
                    p.cycle_seq
                ),
                Style::default().fg(DIM),
            ),
        ]));
        let moves = p.knob_moves();
        if moves.is_empty() {
            lines.push(Line::from(Span::styled(
                "    knobs: —",
                Style::default().fg(DIM),
            )));
        } else {
            let txt = moves
                .iter()
                .map(|(n, f, t)| format!("{n} {f}→{t}"))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(Line::from(vec![
                Span::styled("    knobs: ", Style::default().fg(DIM)),
                Span::raw(txt),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled("    why ", Style::default().fg(DIM)),
            Span::styled(p.reason.clone(), Style::default().fg(color)),
            Span::styled(
                format!(
                    " · confidence {:.2} · samples {}",
                    p.confidence, p.sample_count
                ),
                Style::default().fg(DIM),
            ),
        ]));
        if focused {
            push_comparison(&mut lines, p);
        }
        lines.push(Line::from(Span::raw("")));
    }
    lines.push(Line::from(Span::styled(
        "a accept (asks y/n) · x reject · d defer · m engine · e auto-evolve · u rollback selected strategy · ↑/↓ select",
        Style::default().fg(DIM),
    )));
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Pending Proposals ({})", pending.len())),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Full baseline-vs-variant table for the focused proposal — the 对比 the
/// operator reads before accepting: every metric side by side, delta colored
/// by whether it favors the variant.
fn push_comparison(lines: &mut Vec<Line<'static>>, p: &EvolutionProposalView) {
    lines.push(Line::from(Span::styled(
        "    ── baseline → variant ──",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    let rows: Vec<(&str, String, String, String, Option<bool>)> = vec![
        (
            "trades",
            format!("{}", p.baseline.closed),
            format!("{}", p.variant.closed),
            String::new(),
            None,
        ),
        (
            "win rate",
            pct(p.baseline.win_rate),
            pct(p.variant.win_rate),
            format!(
                "{:+.1}pp",
                (p.variant.win_rate - p.baseline.win_rate) * 100.0
            ),
            Some(p.variant.win_rate > p.baseline.win_rate),
        ),
        (
            "profit factor",
            f2(p.baseline.profit_factor),
            f2(p.variant.profit_factor),
            format!("{:+.2}", p.variant.profit_factor - p.baseline.profit_factor),
            Some(p.variant.profit_factor > p.baseline.profit_factor),
        ),
        (
            "payoff",
            f2(p.baseline.payoff),
            f2(p.variant.payoff),
            format!("{:+.2}", p.variant.payoff - p.baseline.payoff),
            Some(p.variant.payoff > p.baseline.payoff),
        ),
        (
            "net",
            signed(p.baseline.net_pnl_usd, ""),
            signed(p.variant.net_pnl_usd, ""),
            signed(p.variant.net_pnl_usd - p.baseline.net_pnl_usd, ""),
            Some(p.variant.net_pnl_usd > p.baseline.net_pnl_usd),
        ),
    ];
    for (name, base, var, delta, better) in rows {
        let delta_style = match better {
            Some(true) => Style::default().fg(GREEN),
            Some(false) => Style::default().fg(RED),
            None => Style::default().fg(DIM),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("      {name:<14}"), Style::default().fg(DIM)),
            Span::raw(format!("{base:>10}")),
            Span::styled(" → ", Style::default().fg(DIM)),
            Span::styled(
                format!("{var:<10}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(delta, delta_style),
        ]));
    }
}

fn pct(v: f64) -> String {
    format!("{:.1}%", v * 100.0)
}
fn f2(v: f64) -> String {
    format!("{v:.2}")
}

/// The decided tail (newest first), so an operator can see what already
/// happened without paging through history.
fn render_evo_recent(f: &mut Frame, area: Rect, app: &App, now: i64) {
    let mut decided: Vec<&EvolutionProposalView> = app
        .snap
        .evolution_proposals
        .iter()
        .filter(|p| !p.is_pending())
        .collect();
    decided.reverse(); // store order is oldest-first
    decided.truncate(6);
    let mut lines: Vec<Line> = Vec::new();
    if decided.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no decided proposals yet)",
            Style::default().fg(DIM),
        )));
    }
    for p in decided {
        let color = evo_state_color(&p.state);
        let by = p.decided_by.as_deref().unwrap_or("—");
        let when = p
            .decided_at_ms
            .map(|t| ago(t, now))
            .unwrap_or_else(|| "—".into());
        lines.push(Line::from(vec![
            Span::styled(short_id(&p.id), Style::default().fg(DIM)),
            Span::styled(format!(" {} · ", p.strategy), Style::default().fg(DIM)),
            Span::styled(
                p.state.clone(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" by {by} · {when} ago"), Style::default().fg(DIM)),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Recent Decisions"),
        ),
        area,
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> Line<'static> {
        Line::from(text.to_string())
    }

    /// The `n` overlay sized itself from a fixed 20 rows, so a report that grew
    /// past it lost its tail silently (#260). The height now comes from the
    /// content, and the content WRAPS — counting `lines.len()` would under-count
    /// a long detail line and crop it exactly as the fixed height did.
    #[test]
    fn the_overlay_height_counts_wrapped_rows_not_just_lines() {
        assert_eq!(wrapped_rows(&[], 40), 0, "no content, no rows");

        // An empty line is still a row: the overlay's spacers must be counted.
        assert_eq!(wrapped_rows(&[line("")], 40), 1);
        assert_eq!(wrapped_rows(&[line(""), line("")], 40), 2);

        // A line that fits is one row, whatever it is made of.
        assert_eq!(wrapped_rows(&[line("0123456789")], 10), 1);
        // One column too narrow wraps onto a second row.
        assert_eq!(wrapped_rows(&[line("0123456789X")], 10), 2);
        assert_eq!(wrapped_rows(&[line(&"a".repeat(21))], 10), 3);

        // Width is DISPLAY width, not a character count and not a byte count: a
        // CJK glyph is two columns and three bytes, so six glyphs fill a
        // 12-column box exactly and an eighth wraps. Counting chars or bytes
        // would mis-size the box for every Chinese detail line — the same silent
        // crop, by another route.
        let six_glyphs = "市场".repeat(3); // 6 glyphs, 12 columns, 18 bytes
        assert_eq!(wrapped_rows(&[line(&six_glyphs)], 12), 1);
        assert_eq!(wrapped_rows(&[line(&six_glyphs)], 13), 1);
        assert_eq!(wrapped_rows(&[line(&"市场".repeat(4))], 12), 2); // 8 glyphs = 16 columns

        // A zero-width box would divide by zero; it degrades to "one row per
        // character" rather than panicking inside the render loop.
        assert_eq!(wrapped_rows(&[line("abc")], 0), 3);
    }

    /// A four-path report is the shape the panel ships with; it must fit in the
    /// height the overlay asks for, with room for the headline and the footer.
    #[test]
    fn a_four_path_report_fits_the_computed_height() {
        let mut lines = vec![line("4/4 paths OK — all network paths OK"), line("")];
        for name in ["venue-rest", "venue-ws", "discovery", "spot-ws"] {
            lines.push(line(&format!(
                "  ok   {}  {}.example  12 ms  ok",
                name, name
            )));
        }
        lines.push(line(""));
        lines.push(line("reading: venue-rest answered in 12 ms"));
        lines.push(line(""));
        lines.push(line(
            "r probe again · n or Esc closes · the same report is in the log pane",
        ));

        // Comfortably narrower than any real terminal: nothing here wraps.
        let rows = wrapped_rows(&lines, 100);
        assert_eq!(rows, lines.len(), "a report that fits must not be padded");
        assert!(
            rows + 2 <= 20,
            "the shipped four-path report outgrew the old box"
        );
    }
}
