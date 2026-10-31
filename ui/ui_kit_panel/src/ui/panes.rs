//! Split out of the parent module: pure move.
//! The data-tab renderers: overview, positions, orders, trades, decisions
//! and plugins.

use super::*;

fn color_of(v: f64) -> Color {
    if v >= 0.0 {
        GREEN
    } else {
        RED
    }
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

pub(super) fn render_overview(f: &mut Frame, area: Rect, s: &UiSnapshot) {
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
            blitzkrieg_ui_kit::web::money(net),
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

pub(super) fn render_positions(f: &mut Frame, area: Rect, s: &UiSnapshot) {
    let header = Row::new(vec![
        "Asset", "Dir", "Venue", "Strategy", "Entry", "Cur", "PnL", "Left",
    ])
    .style(Style::default().fg(DIM).add_modifier(Modifier::BOLD));
    let widths = [
        Constraint::Length(7),
        Constraint::Length(5),
        Constraint::Length(9),
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
            // #427: the venue is DATA the row carries — a cross-venue pair
            // shows one leg per venue. Undeclared renders as a dash.
            let venue = if p.venue.is_empty() {
                "—".to_string()
            } else {
                p.venue.clone()
            };
            Row::new(vec![
                Cell::from(p.asset.clone()),
                Cell::from(p.direction.to_uppercase()),
                Cell::from(venue),
                Cell::from(p.strategy.clone()),
                Cell::from(format!("{:.2}", p.entry_price)),
                Cell::from(format!("{:.2}", p.current_price)),
                Cell::from(Span::styled(
                    blitzkrieg_ui_kit::web::pct(p.unrealized_pct),
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

/// issue 456: the kernel's order book, with the cursor highlight and the
/// force-close hint. Every row carries its `status` verbatim from
/// `orders.list`; the [F] key sends `force-order <id>` for the highlighted
/// row — the kernel still refuses anything whose round could still fill.
pub(super) fn render_orders(f: &mut Frame, area: Rect, app: &App) {
    let header = Row::new(vec![
        "Id", "Asset", "Dir", "Side", "Mode", "Strategy", "Price", "Size", "Filled", "Status",
        "Round",
    ])
    .style(Style::default().fg(DIM).add_modifier(Modifier::BOLD));
    let widths = [
        Constraint::Length(14),
        Constraint::Length(8),
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(11),
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Length(7),
        Constraint::Length(14),
        Constraint::Length(7),
    ];
    let rows: Vec<Row> = app
        .snap
        .orders
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let selected = i
                == app
                    .orders_focus
                    .min(app.snap.orders.len().saturating_sub(1));
            let id = short_id(&o.order_id);
            Row::new(vec![
                Cell::from(id),
                Cell::from(o.asset.clone()),
                Cell::from(o.direction.to_uppercase()),
                Cell::from(o.side.clone()),
                Cell::from(o.mode.clone()),
                Cell::from(o.strategy.clone()),
                Cell::from(format!("{:.3}", o.price)),
                Cell::from(format!("{:.1}", o.size)),
                Cell::from(format!("{:.1}", o.filled_size)),
                Cell::from(Span::styled(
                    o.status.clone(),
                    Style::default().fg(if o.status == "FORCE_CLOSED" {
                        Color::Yellow
                    } else {
                        color_of(0.0)
                    }),
                )),
                Cell::from(format!("#{}", o.round_slot)),
            ])
            .style(if selected {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            })
        })
        .collect();
    let table = Table::new(rows, widths).header(header).block(
        Block::default().borders(Borders::ALL).title(format!(
            "Orders ({}) — ↑/↓ select · [F] force-close ghost",
            app.snap.orders.len()
        )),
    );
    f.render_widget(table, area);
}

pub(super) fn render_trades(f: &mut Frame, area: Rect, s: &UiSnapshot) {
    let header = Row::new(vec![
        "Asset",
        "Dir",
        "Venue",
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
        Constraint::Length(9),
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
            // #427: the venue is DATA the row carries; undeclared is a dash.
            let venue = if t.venue.is_empty() {
                "—".to_string()
            } else {
                t.venue.clone()
            };
            Row::new(vec![
                Cell::from(t.asset.clone()),
                Cell::from(t.direction.to_uppercase()),
                Cell::from(venue),
                Cell::from(t.strategy.clone()),
                Cell::from(format!("{:.2}→{:.2}", t.entry_price, t.exit_price)),
                Cell::from(Span::styled(
                    blitzkrieg_ui_kit::web::money(t.net_pnl_usd),
                    Style::default().fg(color_of(t.net_pnl_usd)),
                )),
                Cell::from(Span::styled(
                    blitzkrieg_ui_kit::web::pct(t.net_pnl_pct),
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
pub(super) fn render_decisions(f: &mut Frame, area: Rect, app: &App) {
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
pub(super) fn render_plugins(f: &mut Frame, area: Rect, app: &App) {
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
