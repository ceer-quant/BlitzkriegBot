//! Split out of the parent module: pure move.
//! The Settings tab: the version/update face above and the #364
//! execution-policy editor face below.

use super::*;

/// The evolution view: the auto-evolve switch and cycle clock on top, pending
/// proposals (each with its baseline-vs-variant 对比表) in the middle, recent
/// decisions at the bottom. The keys drive the same gateway verbs the command
/// bar accepts (`decide` / `auto-evolve` / `rollback`) — one shared backend.
/// The Settings pane (VERSIONING.md §6.2): version + build provenance + the
/// update three-state. The name `render_settings` is load-bearing — the
/// TUI/WebUI parity gate finds `fn render_<tab>` by this exact spelling.
pub(super) fn render_settings(f: &mut Frame, area: Rect, app: &App) {
    // Two stacked panes: the version/update face above, the #364 execution
    // policy face below. The split is content-driven: the top pane gets what
    // its lines need (it is short), the policy pane takes the rest.
    let rows = Layout::vertical([Constraint::Length(14), Constraint::Min(10)]).split(area);
    render_settings_version(f, rows[0], app);
    if app.policy_mode {
        render_policy(f, rows[1], app);
    } else {
        render_policy_idle(f, rows[1]);
    }
}

/// The original Settings content (VERSIONING.md §6.2): version + build
/// provenance + the update three-state.
fn render_settings_version(f: &mut Frame, area: Rect, app: &App) {
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
    // ── STAGE (#379, §7.5) — the download/verify state machine; the kernel
    // stops at `staged` and the LAUNCHER applies on the restart path. ──
    if let Some(stage) = app.snap.stage_state.as_ref() {
        let (mark, text, color) = match stage.phase.as_str() {
            "staged" => (
                "✓",
                "staged — restart applies it (launcher-side)".to_string(),
                GREEN,
            ),
            "downloading" => ("…", "downloading + verifying".to_string(), WARN),
            "failed" => (
                "✗",
                format!(
                    "failed — {}",
                    stage.detail.clone().unwrap_or_else(|| "unknown".into())
                ),
                RED,
            ),
            _ => ("·", "nothing staged".to_string(), DIM),
        };
        lines.push(Line::from(Span::styled(
            format!("  stage    {mark} {text}"),
            Style::default().fg(color),
        )));
    }
    lines.push(Line::from(""));
    if app.update_busy {
        lines.push(Line::from(Span::styled(
            "  working…",
            Style::default().fg(WARN),
        )));
    }
    lines.push(Line::from(Span::styled(
        "  [c] check now   [a] toggle auto-update (y/n)   [k] toggle check (y/n)   [s] stage   [i] install",
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

/// #364 — the execution policy face (settings, lower pane), shown while the
/// `p` sub-mode is active. Everything it
/// shows came from the kernel over IPC (`execution_policy.list/get/preview/
/// history`) — the TOML file is never read here; writes are `set`/`reset`
/// calls, so this face and the WebUI card are two views of one kernel state.
///
/// Keys: ←/→ account · [e] edit rule · [a] add · [d] delete (asks) ·
/// [↑/↓] move cursor (on a rule row: priority via [d]+re-add or the WebUI's
/// drag) · [空格] enable/disable · [s] save · [r] rollback (asks) · [q] out.
fn render_policy(f: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();

    // ── ACCOUNT chips: `defaults` is always present (index 0) ──
    let mut chips = vec!["[defaults]".to_string()];
    for a in &app.policy_accounts {
        let id = a.get("accountId").and_then(|v| v.as_str()).unwrap_or("?");
        chips.push(format!("[{id}]"));
    }
    let sel = app.policy_account_idx.min(chips.len() - 1);
    let mut chip_line: Vec<Span> = vec![Span::styled("ACCOUNT ", Style::default().fg(ACCENT))];
    for (i, c) in chips.iter().enumerate() {
        chip_line.push(if i == sel {
            Span::styled(
                c.clone(),
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(c.clone(), Style::default().fg(DIM))
        });
    }
    chip_line.push(Span::styled("  (←/→ switch)", Style::default().fg(DIM)));
    lines.push(Line::from(chip_line));
    lines.push(Line::from(""));

    // ── BASE params: the effective section's own numbers (kernel-folded) ──
    let s = app
        .policy_section
        .clone()
        .unwrap_or(serde_json::Value::Null);
    let param = |key: &str| -> String {
        s.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("—")
            .to_string()
    };
    let param_rows = [
        ("budget_ratio", "下注比例"),
        ("min_budget_usd", "最小下注"),
        ("max_budget_usd", "最大下注"),
        ("min_equity_usd", "最低权益"),
    ];
    for (i, (key, label)) in param_rows.iter().enumerate() {
        let cursor = if app.policy_focus == i { ">" } else { " " };
        lines.push(Line::from(format!(
            "  {cursor} {label:<8} {key:<18} {}",
            param(key)
        )));
    }
    let cap_cursor = if app.policy_focus == 4 { ">" } else { " " };
    lines.push(Line::from(format!(
        "  {cap_cursor} 单资产持仓上限  {}",
        s.get("max_positions_per_asset")
            .and_then(|v| v.as_u64())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".to_string())
    )));
    lines.push(Line::from(""));

    // ── RULES: name / priority / enabled / WHEN / THEN, first hit wins ──
    lines.push(Line::from(Span::styled(
        "RULES  (priority ascending — the first true `when` wins)",
        Style::default().fg(ACCENT),
    )));
    let rules = s
        .get("rules")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    if rules.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no rules — the section's defaults decide every entry)",
            Style::default().fg(DIM),
        )));
    }
    for (i, r) in rules.iter().enumerate() {
        let row = 5 + i;
        let cursor = if app.policy_focus == row { ">" } else { " " };
        let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let prio = r.get("priority").and_then(|v| v.as_u64()).unwrap_or(100);
        let enabled = r.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        let when = r.get("when").cloned().unwrap_or(serde_json::Value::Null);
        let field = when.get("field").and_then(|v| v.as_str()).unwrap_or("?");
        let op = when.get("op").and_then(|v| v.as_str()).unwrap_or("?");
        let value = match when.get("value") {
            Some(serde_json::Value::String(t)) => t.clone(),
            Some(serde_json::Value::Array(list)) => {
                let items: Vec<String> = list
                    .iter()
                    .filter_map(|x| x.as_str())
                    .map(|x| x.to_string())
                    .collect();
                items.join(",")
            }
            Some(other) => other.to_string(),
            None => "?".to_string(),
        };
        let then = r.get("then").cloned().unwrap_or(serde_json::Value::Null);
        let then_text = if then.get("action").and_then(|a| a.as_str()) == Some("skip") {
            "skip".to_string()
        } else if let Some(v) = then.get("budget_ratio").and_then(|b| b.as_str()) {
            format!("budget_ratio {v}")
        } else if let Some(v) = then.get("min_budget_usd").and_then(|b| b.as_str()) {
            format!("min_budget_usd {v}")
        } else if let Some(v) = then.get("max_budget_usd").and_then(|b| b.as_str()) {
            format!("max_budget_usd {v}")
        } else if let Some(v) = then.get("cooldown_sec").and_then(|c| c.as_i64()) {
            format!("cooldown_sec {v}")
        } else {
            "?".to_string()
        };
        let color = if enabled { FG } else { DIM };
        lines.push(Line::from(Span::styled(
            format!(
                "  {cursor} #{prio:<4} {name} [{}] WHEN {field} {op} {value} THEN {then_text}",
                if enabled { "on " } else { "off" }
            ),
            Style::default().fg(color),
        )));
    }
    lines.push(Line::from(""));

    // ── PREVIEW: the kernel's own verdicts over the recent closed trades ──
    lines.push(Line::from(Span::styled(
        "PREVIEW",
        Style::default().fg(ACCENT),
    )));
    match &app.policy_preview {
        Some(p) if !p.is_null() => {
            let considered = p.get("considered").and_then(|v| v.as_u64()).unwrap_or(0);
            let skipped = p.get("skipped").and_then(|v| v.as_u64()).unwrap_or(0);
            let avg = p
                .get("avgBudgetUsd")
                .and_then(|v| v.as_str())
                .unwrap_or("—");
            let placed = considered.saturating_sub(skipped);
            lines.push(Line::from(Span::styled(
                format!("  最近 {considered} 单：跳过 {skipped} · 放行 {placed} · 平均下注 ${avg}"),
                Style::default().fg(if skipped > 0 { WARN } else { FG }),
            )));
        }
        _ => lines.push(Line::from(Span::styled(
            "  (no preview — no policy loaded or no closed trades yet)",
            Style::default().fg(DIM),
        ))),
    }
    lines.push(Line::from(""));

    // ── ROLLBACK: the journal is the version store ──
    let version_count = app.policy_history.len();
    lines.push(Line::from(Span::styled(
        format!("HISTORY  {version_count} 条审计记录（版本号 = 记录数；[r] 回滚到上一版，先确认）"),
        Style::default().fg(ACCENT),
    )));
    lines.push(Line::from(""));

    if app.policy_busy {
        lines.push(Line::from(Span::styled(
            "  working…",
            Style::default().fg(WARN),
        )));
    }
    if app.policy_draft.is_some() {
        lines.push(Line::from(Span::styled(
            "  草稿编辑在输入栏进行：名称/值先 `:` 输入，再按提示提交（渲染见输入栏上方提示）",
            Style::default().fg(WARN),
        )));
    }
    lines.push(Line::from(Span::styled(
        "  [e] edit rule   [a] add rule   [d] delete (asks)   [空格] on/off (asks)   [s] save (asks)   [r] rollback (asks)",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        "  写路径与 WebUI 同源（execution_policy.set/reset）；两侧读到的永远是内核折叠后的生效视图",
        Style::default().fg(DIM),
    )));

    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Settings · 生效风控"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The idle pane shown when the `p` sub-mode is NOT active: an invitation
/// plus the entry key. It must not render a stale policy view as if it were
/// live — the data the editor shows is only refreshed inside the mode.
fn render_policy_idle(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            "生效风控 — 每个账户的执行策略（下注比例 / 金额上下限 / 规则 / 预览 / 版本回滚）。",
            Style::default().fg(FG),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "数据全部经 IPC 读自内核（execution_policy.*），TOML 永不出内核；写路径与 WebUI 同源，",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            "两侧读到的永远是内核折叠后的生效视图。",
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "按 p 进入编辑（←/→ 切换账户 · [e]/[a]/[d]/[空格]/[s]/[r] · q 返回）",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Settings · 生效风控"),
            )
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
