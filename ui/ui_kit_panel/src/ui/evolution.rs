//! Split out of the parent module: pure move.
//! The Evolution tab: engine/auto switches and cycle clock, pending
//! proposals with their baseline-vs-variant comparison, recent decisions.

use super::*;

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
pub(super) fn render_evolution(f: &mut Frame, area: Rect, app: &App) {
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
            blitzkrieg_ui_kit::web::money(p.baseline.net_pnl_usd),
            blitzkrieg_ui_kit::web::money(p.variant.net_pnl_usd),
            blitzkrieg_ui_kit::web::money(p.variant.net_pnl_usd - p.baseline.net_pnl_usd),
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

pub(super) fn pct(v: f64) -> String {
    format!("{:.1}%", v * 100.0)
}
pub(super) fn f2(v: f64) -> String {
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
