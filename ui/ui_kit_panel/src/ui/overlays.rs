//! Split out of the parent module: pure move.
//! Full-screen overlays: `?` help, the `n` network self-check, the `A`
//! account switch.

use super::*;

/// The `?` overlay: every key and command in one screen.
pub(super) fn render_help(f: &mut Frame, _app: &App) {
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
        Line::from("  A (shift-a)    accounts — switch this session's default account"),
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
pub(super) fn render_net_check(f: &mut Frame, app: &App) {
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
pub(super) fn wrapped_rows(lines: &[Line], width: u16) -> usize {
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

/// 账户↔venue 解耦（本轮）：the `A` overlay — the account book the kernel
/// answered with, one row per account, cursor on the switchable row. Rendered
/// straight off the raw `account.list` reply (no second model — the same
/// discipline as the policy face); the venue binding and the strategy
/// enablement set ride the row so the operator can see WHO they are about to
/// become before pressing Enter. Enter confirms through the same y/n bar as
/// the other panel writes; Esc/A closes.
pub(super) fn render_accounts(f: &mut Frame, app: &App) {
    let area = f.area();
    let mut lines: Vec<Line> = Vec::new();
    let active = app
        .accounts_doc
        .as_ref()
        .and_then(|d| d.get("active"))
        .and_then(|a| a.as_str())
        .unwrap_or("")
        .to_string();
    let rows: Vec<&serde_json::Value> = app
        .accounts_doc
        .as_ref()
        .and_then(|d| d.get("accounts"))
        .and_then(|a| a.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    match app.accounts_doc.as_ref() {
        None => {
            lines.push(Line::from(Span::styled(
                "loading the account book… (or the core refused — check the log pane)",
                Style::default().fg(ACCENT),
            )));
        }
        Some(_) if rows.is_empty() => {
            lines.push(Line::from(Span::styled(
                "the account book is empty (this should not happen — `default` always exists)",
                Style::default().fg(RED),
            )));
        }
        Some(_) => {
            lines.push(Line::from(Span::styled(
                format!("session default: {active}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            for (i, row) in rows.iter().enumerate() {
                let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let status = row.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                let venue = row.get("venue").and_then(|v| v.as_str()).unwrap_or("");
                let cursor = if i == app.accounts_focus.min(rows.len().saturating_sub(1)) {
                    Span::styled("▸ ", Style::default().fg(ACCENT))
                } else {
                    Span::raw("  ")
                };
                let is_active = id == active;
                let mark = if is_active {
                    Span::styled("● ", Style::default().fg(GREEN))
                } else {
                    Span::raw("  ")
                };
                let status_span = if status == "active" {
                    Span::styled(format!("{status:<9}"), Style::default().fg(GREEN))
                } else {
                    Span::styled(format!("{status:<9}"), Style::default().fg(WARN))
                };
                let mut spans = vec![
                    cursor,
                    mark,
                    Span::styled(
                        format!("{id:<12}"),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("{name:<14}")),
                    status_span,
                ];
                // The venue binding: 空 = 未绑定（可交易任意交易所）— rendered
                // as the dash, never as a fake venue name.
                spans.push(Span::styled(
                    format!("venue: {}", if venue.is_empty() { "—" } else { venue }),
                    Style::default().fg(DIM),
                ));
                let strategies: Vec<&str> = row
                    .get("strategies")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|s| s.as_str()).collect())
                    .unwrap_or_default();
                spans.push(Span::styled(
                    if strategies.is_empty() {
                        " · strategies: 全局继承".to_string()
                    } else {
                        format!(" · strategies: {}", strategies.join(","))
                    },
                    Style::default().fg(DIM),
                ));
                lines.push(Line::from(spans));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ select · Enter switch (asks y/n) · A or Esc closes",
        Style::default().fg(DIM),
    )));

    let pct_x = 86u16;
    let inner_w = (area.width * pct_x / 100).saturating_sub(2);
    let body_rows = wrapped_rows(&lines, inner_w);
    let rect = centered_rect(
        area,
        pct_x,
        (body_rows as u16).saturating_add(2).min(area.height),
    );
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Accounts — switch this session's default (A / Esc to close)")
                .border_style(Style::default().fg(ACCENT)),
        ),
        rect,
    );
}
