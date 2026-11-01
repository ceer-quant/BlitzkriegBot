use super::*;

pub(super) fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// One signed money figure shared by every adapter face (`+12.34` / `-3.00`).
/// Pure presentation — no trading meaning, no GUI dependency; the TUI panel
/// renders the same bytes from the same function (issue 719).
pub fn money(v: f64) -> String {
    format!("{}{:.2}", if v >= 0.0 { "+" } else { "-" }, v.abs())
}

/// One signed percentage figure shared by every adapter face (`+3.4%`).
/// Semantics deliberately differ from the evolution tab's fraction-scaled
/// `pct` in `ui_kit_panel` — that one multiplies by 100 first; this takes a
/// percent that already is one.
pub fn pct(v: f64) -> String {
    format!("{}{:.1}%", if v >= 0.0 { "+" } else { "-" }, v.abs())
}

/// Render the whole panel as a single self-refreshing HTML document.
pub fn render_html(s: &UiSnapshot) -> String {
    render_html_with(s, false)
}

/// As [`render_html`], optionally including the command console (gateway mode).
pub fn render_html_with(s: &UiSnapshot, console: bool) -> String {
    render_html_full(s, console, None, None)
}

/// The full built-in panel: snapshot + the two notices that must not be missed.
///
/// `lifecycle` carries the gateway's process state, which is where the give-up
/// alert lives (#186); `bind_warning` is set when this panel is listening on a
/// non-loopback address (#185). Both are rendered as red banners at the top of
/// the page — a notice an operator has to go looking for is not a notice.
pub fn render_html_full(
    s: &UiSnapshot,
    console: bool,
    lifecycle: Option<&LifecycleView>,
    bind_warning: Option<&str>,
) -> String {
    let mode = esc(s.mode());
    let mut rows = String::new();
    if let Some(r) = &s.round {
        // issue 550 / issue 559 显示面口径（TUI 三处漏网之一）：全局标量块
        // 是各 venue 最早到期的保守钟，多族并存时状态页 headline 却读它 ——
        // 显示账户绑着 kalshi 时卡上标的是 polymarket 的 slot。回落链照抄
        // webapp Overview：display_venue 的 venueRounds 行优先（helper 的
        // 缺省回落 = 第一个非空行），行整体缺席回落全局标量块 —— 单族
        // 逐字节维持现行为。下面同时补 venueRounds 行渲染（有数据才渲染）。
        let vr = s.display_venue_round();
        let (slot, age, left) = match vr {
            Some(v) => (v.slot, v.age_sec, v.time_left_sec),
            None => (r.slot, r.age_sec, r.time_left_sec),
        };
        let mut venue_lines = String::new();
        for v in &r.venue_rounds {
            let tag = if v.venue.is_empty() {
                "未分所"
            } else {
                &v.venue
            };
            venue_lines.push_str(&format!(
                "<span class=pill>{}: #{} · {}s left · {}s family</span>",
                esc(tag),
                v.slot,
                v.time_left_sec,
                v.duration_sec
            ));
        }
        if venue_lines.is_empty() {
            rows.push_str(&format!(
                "<div class=card><h2>Round</h2><div class=big>#{}</div><div class=sub>{}s old · {}s left · {}</div></div>",
                r.slot, r.age_sec, r.time_left_sec,
                if r.can_trade { "TRADING" } else { "WAITING" }
            ));
        } else {
            rows.push_str(&format!(
                "<div class=card><h2>Round</h2><div class=big>#{}</div><div class=sub>{}s old · {}s left · {}</div><div>{}{}</div></div>",
                slot,
                age,
                left,
                if r.can_trade { "TRADING" } else { "WAITING" },
                if vr.is_some() {
                    format!(
                        "<span class=pill>显示: {}</span>",
                        esc(if s.display_venue.is_empty() {
                            "全局"
                        } else {
                            &s.display_venue
                        })
                    )
                } else {
                    String::new()
                },
                venue_lines
            ));
        }
    }
    if let Some(b) = &s.balance {
        rows.push_str(&format!(
            "<div class=card><h2>Balance</h2><div class=big>${:.2}</div><div class=sub>reserved ${:.2} · avail ${:.2}</div></div>",
            b.balance, b.reserved, b.available
        ));
    }
    if let Some(st) = &s.stats {
        rows.push_str(&format!(
            "<div class=card><h2>Feed</h2><div class=big>{} books</div><div class=sub>signals {} · rejected {} · confirmed {}</div></div>",
            st.books, st.signals, st.place_rejected, st.confirmed.len()
        ));
    }
    let net = s.net_pnl();
    rows.push_str(&format!(
        "<div class=card><h2>Net PnL</h2><div class='big {}'>{}</div><div class=sub>{} trades · {:.0}% WR</div></div>",
        if net >= 0.0 { "pos" } else { "neg" },
        money(net), s.trades.len(), s.win_rate()
    ));

    let mut pos_rows = String::new();
    for p in &s.positions {
        pos_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{:.2}</td><td>{:.2}</td><td class='{}'>{}</td><td>{}s</td></tr>",
            esc(&p.asset), esc(&p.direction.to_uppercase()), p.entry_price, p.current_price,
            if p.unrealized_pct >= 0.0 { "pos" } else { "neg" }, pct(p.unrealized_pct), p.remaining_sec
        ));
    }
    if pos_rows.is_empty() {
        pos_rows.push_str("<tr><td colspan=6 class=sub>no open positions</td></tr>");
    }

    let mut trade_rows = String::new();
    for t in s.trades.iter().rev().take(25) {
        trade_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{:.2}→{:.2}</td><td class='{}'>{}</td><td>{}</td></tr>",
            esc(&t.asset),
            esc(&t.direction.to_uppercase()),
            t.entry_price,
            t.exit_price,
            if t.net_pnl_usd >= 0.0 { "pos" } else { "neg" },
            money(t.net_pnl_usd),
            esc(&t.exit_reason)
        ));
    }
    if trade_rows.is_empty() {
        trade_rows.push_str("<tr><td colspan=5 class=sub>no closed trades</td></tr>");
    }

    let price_rows = s
        .round
        .as_ref()
        .map(|r| {
            r.market_prices
                .iter()
                .map(|m| {
                    format!(
                        "<span class=pill>{}: ↑{:.2} ↓{:.2}</span>",
                        esc(&m.asset),
                        m.up,
                        m.down
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();

    let err = s
        .last_error
        .as_ref()
        .map(|e| format!("<div class=err>last error: {}</div>", esc(e)))
        .unwrap_or_default();

    let notices = panel_notices_html(lifecycle, bind_warning);

    let console_html = if console {
        r#"<div class=panel><h2>Command console</h2>
<form id=cmdform onsubmit="return runCmd(event)">
<input id=cmdinput name=cmd placeholder="status | start BTC,ETH --size 5 | stop | positions 25" autocomplete=off>
<button type=submit>Run</button>
</form>
<pre id=cmdout class=sub>try: status</pre></div>
<script>
async function runCmd(e){
 e.preventDefault();
 var v=document.getElementById('cmdinput').value;
 var out=document.getElementById('cmdout');
 out.textContent='…';
 try{
  var r=await fetch('/api/command',{method:'POST',body:v});
  var j=await r.json();
  out.textContent=(j.ok?'OK ':('ERR '))+j.action+' — '+j.message;
 }catch(err){ out.textContent='request failed: '+err; }
 return false;
}
</script>"#
    } else {
        ""
    };

    format!(
        r#"<!doctype html><html lang=en><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1">
<meta http-equiv=refresh content=3>
<title>Blitzkrieg UI Kit — Web</title>
<style>
:root{{--bg:#0d1117;--fg:#e6edf3;--mut:#8b949e;--card:#161b22;--pos:#3fb950;--neg:#f85149;--bd:#30363d}}
*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--fg);font:14px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;padding:20px}}
h1{{font-size:18px;margin:0 0 4px}}.mode{{color:var(--mut);font-size:12px;margin-bottom:16px}}
.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(190px,1fr));gap:12px;margin-bottom:20px}}
.card{{background:var(--card);border:1px solid var(--bd);border-radius:8px;padding:14px}}
.card h2{{margin:0 0 6px;font-size:11px;color:var(--mut);text-transform:uppercase;letter-spacing:.08em;font-weight:600}}
.big{{font-size:24px;font-weight:600}}.sub{{color:var(--mut);font-size:12px;margin-top:4px}}
table{{width:100%;border-collapse:collapse;margin-bottom:20px}}th,td{{text-align:left;padding:6px 8px;border-bottom:1px solid var(--bd)}}
th{{color:var(--mut);font-size:11px;text-transform:uppercase;letter-spacing:.06em}}
.pos{{color:var(--pos)}}.neg{{color:var(--neg)}}.err{{color:var(--neg);margin-top:12px}}
.pill{{display:inline-block;background:var(--card);border:1px solid var(--bd);border-radius:999px;padding:2px 10px;margin:2px;font-size:12px}}
.panel{{background:var(--card);border:1px solid var(--bd);border-radius:8px;padding:14px;margin-bottom:20px}}
.alert{{background:#3d1418;border:1px solid var(--neg);border-radius:8px;padding:14px;margin-bottom:16px}}
.alert h2{{margin:0 0 6px;font-size:14px;color:var(--neg);letter-spacing:.02em}}
.alert pre{{margin:10px 0 0;max-height:220px;overflow:auto}}
.panel h2{{margin:0 0 10px;font-size:13px}}
#cmdinput{{width:min(560px,70%);background:#0d1117;color:var(--fg);border:1px solid var(--bd);border-radius:6px;padding:8px 10px;font:inherit}}
button{{background:#21262d;color:var(--fg);border:1px solid var(--bd);border-radius:6px;padding:8px 14px;font:inherit;cursor:pointer}}
pre{{white-space:pre-wrap;margin:10px 0 0}}
</style></head><body>
<h1>Blitzkrieg UI Kit</h1>
<div class=mode>core mode: <b>{mode}</b> · connected: {} · source: UDS JSON-RPC (read-only panel){gateway_note}</div>
{notices}
<div class=grid>{rows}</div>
<div class=panel><h2>Open Positions</h2><table>
<tr><th>Asset</th><th>Dir</th><th>Entry</th><th>Cur</th><th>PnL</th><th>Left</th></tr>{pos_rows}</table></div>
<div class=panel><h2>Recent Trades</h2><table>
<tr><th>Asset</th><th>Dir</th><th>Entry→Exit</th><th>Net</th><th>Reason</th></tr>{trade_rows}</table></div>
<div class=panel><h2>Prices</h2>{price_rows}</div>
{console_html}
{err}
</body></html>"#,
        s.connected,
        rows = rows,
        pos_rows = pos_rows,
        trade_rows = trade_rows,
        price_rows = price_rows,
        mode = mode,
        gateway_note = if console {
            " · gateway: commands enabled"
        } else {
            ""
        },
        console_html = console_html,
        notices = notices,
        err = err,
    )
}

/// What this gateway can actually do about the core *process*, as opposed to
/// what it can read from it.
///
/// The panel offers 启动/停止, and both are refused in two independent
/// situations: the gateway was started without `--manage` (the verbs are
/// disabled outright), or a core is already running that this gateway did not
/// spawn, in which case [`crate::gateway::Supervisor::stop`] deliberately leaves
/// it alone. Reporting the state lets the panel mark those controls unusable and
/// say why, instead of offering a click whose only outcome is an error.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LifecycleView {
    /// `--manage` (or `UIKIT_MANAGE=1`): the gateway accepts `start`/`stop`.
    pub enabled: bool,
    /// This gateway spawned the core, so it can also stop it. False means the
    /// core was adopted — a running process owned by somebody else.
    pub managed: bool,
    /// PID of the core this gateway spawned; null when adopted.
    pub pid: Option<u32>,
    /// Socket both the reads and the lifecycle verbs act on.
    pub socket: String,
    /// How many times a core this gateway owned has been replaced after a crash.
    pub restarts: u32,
    /// The restart budget is spent; the core is down and will stay down.
    pub restart_given_up: bool,
    /// Why the last owned core stopped — `kind` separates a crash from a stop we
    /// asked for, so the panel can say which happened instead of inferring it
    /// from a missing pid.
    pub last_exit: Option<LifecycleExit>,
    /// The alert raised when the restart budget ran out (#186). Null while the
    /// supervisor is still trying: a crash it is about to replace is a restart,
    /// not an alert. Set → the kernel is DOWN and nothing will bring it back.
    pub give_up: Option<LifecycleGiveUp>,
}

/// The give-up alert, flattened for the panel.
///
/// `restart_given_up` says the core will not come back; this says *why*, which is
/// the difference between a status field and something an operator can act on.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleGiveUp {
    /// Replacements attempted before giving up.
    pub attempts: u32,
    /// Unix milliseconds when the budget was spent.
    pub at_ms: i64,
    /// Operator-readable summary (already localised by the supervisor).
    pub message: String,
    /// Last stderr lines of the core that died — the reason, when it printed one.
    pub stderr_tail: Vec<String>,
}

impl From<&crate::gateway::supervisor::GiveUpAlert> for LifecycleGiveUp {
    fn from(a: &crate::gateway::supervisor::GiveUpAlert) -> Self {
        Self {
            attempts: a.attempts,
            at_ms: a.at_ms,
            message: a.message.clone(),
            stderr_tail: a.stderr_tail.clone(),
        }
    }
}

/// A core exit, flattened for the panel.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LifecycleExit {
    pub pid: u32,
    /// "crash" | "clean".
    pub kind: &'static str,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// One-line, operator-readable form (kept so the panel need not reword it).
    pub description: String,
}

impl From<&crate::gateway::supervisor::ExitReport> for LifecycleExit {
    fn from(e: &crate::gateway::supervisor::ExitReport) -> Self {
        use crate::gateway::supervisor::ExitKind;
        Self {
            pid: e.pid,
            kind: match e.kind {
                ExitKind::Clean => "clean",
                ExitKind::Crashed => "crash",
            },
            code: e.code,
            signal: e.signal,
            description: e.describe(),
        }
    }
}

/// Render the snapshot as a stable JSON document (for programmatic consumers).
///
/// `lifecycle` is the gateway's own process-control state. It is separate from
/// the snapshot because it describes *this* gateway, not the core; a read-only
/// deployment (no dispatcher) passes `None` and the panel then knows it cannot
/// drive the lifecycle either. The `gateway` key is then OMITTED rather than set
/// to null, so a consumer cannot mistake "no gateway here" for "a gateway that
/// reports nothing".
pub fn render_json(s: &UiSnapshot, lifecycle: Option<&LifecycleView>) -> String {
    render_json_full(s, lifecycle, None)
}

/// As [`render_json`], plus a `security` block describing this server's own
/// exposure (#185) — the listen address, whether it is loopback-only, and the
/// login-throttle state. Passed separately because it describes the *server*,
/// while the snapshot describes the core; a caller with neither (tests, a
/// read-only render) passes `None` and the key is omitted.
pub fn render_json_full(
    s: &UiSnapshot,
    lifecycle: Option<&LifecycleView>,
    security: Option<serde_json::Value>,
) -> String {
    let mut doc = serde_json::json!({
        "connected": s.connected,
        "mode": s.mode(),
        "balance": s.balance.as_ref().map(|b| serde_json::json!({
            "balance": b.balance, "reserved": b.reserved, "available": b.available,
            // Starting principal (dry only) so the panel can show the
            // `principal + realized net` reconciliation instead of asserting it.
            "seed": b.seed })),
        // Wallet identity for the live-mode balance card (dry cores leave
        // these null — their cash is the local dry seed, not venue funds).
        "wallet": {
            "signer": s.ready.as_ref().and_then(|r| r.signer.clone()),
            "funder": s.ready.as_ref().and_then(|r| r.funder.clone()),
        },
        "round": s.round.as_ref().map(|r| serde_json::json!({
            "slot": r.slot, "ageSec": r.age_sec, "timeLeftSec": r.time_left_sec,
            "markets": r.markets, "canTrade": r.can_trade,
            // HFT panel needs the live UP/DOWN quotes per asset.
            // 账户↔venue 解耦（本轮）：每行还带 `venue`（插件名小写）——
            // 这里是 webui 披露过的「类型化重组剥字段」点之一：typed struct
            // 逐字段重拼，不写 venue 就到不了前端（HftPage 的 venue 过滤
            // 与 Loading 门都 feature-detect 它）。空串 = 未分所，前端降级。
            "prices": r.market_prices.iter().map(|m| serde_json::json!({
                "asset": m.asset, "up": m.up, "down": m.down,
                "venue": m.venue })).collect::<Vec<_>>(),
            // #550（症状②）：per-venue 轮次钟行 —— 倒计时卡按 active venue
            // 选行重绑（账户切换器就是交易所切换器），不再读全局保守钟的
            // 标量块。老内核没有该键 → 空表，面板走现行为（feature-detect
            // 降级同一纪律）。B1a：行还带 `durationSec`（向后兼容增量字
            // 段，老内核经 serde default 落 0）—— 多族语料下同 venue 多
            // 行，面板按 (venue, durationSec) 选行，字段在场（>0）才用。
            "venueRounds": r.venue_rounds.iter().map(|v| serde_json::json!({
                "venue": v.venue, "durationSec": v.duration_sec, "slot": v.slot,
                "ageSec": v.age_sec, "timeLeftSec": v.time_left_sec }))
                .collect::<Vec<_>>() })),
        "positions": s.positions.iter().map(|p| serde_json::json!({
            // The stable position id (hft-N) — the 持仓 card's per-row 强平
            // button flattens by it (`flatten <id>`); without it the panel
            // sent the literal `flatten undefined` and the click died silently.
            "id": p.id,
            "asset": p.asset, "direction": p.direction, "entryPrice": p.entry_price,
            "currentPrice": p.current_price, "unrealizedPct": p.unrealized_pct,
            "strategy": p.strategy, "shares": p.shares,
            // #427: the Venue column (empty = undeclared).
            "venue": p.venue,
            // E28 (§9.2): the account the position settles into (empty on a
            // pre-E28 core — the panel leaves unattributed rows visible).
            "accountId": p.account_id,
            "remainingSec": p.remaining_sec,
            // Slot matching for the panel's expiry judgment: the entry
            // order's grid slot rides the positions wire (0 = legacy
            // snapshot, panel falls back to remainingSec's sign).
            "roundSlot": p.round_slot,
            // The row's OWN family duration — the panel's slot arithmetic
            // multiplies by this, never by a page-level duration (issue 733).
            "roundDurationSec": p.round_duration_sec })).collect::<Vec<_>>(),
        // issue 456: the order book as the kernel tracks it (orders.list wire
        // shape, camelCase). The WebUI's 挂单 card renders it and derives its
        // per-row force-close button from `status`; the bulk sweep targets the
        // still-live ones whose round has expired. Absent on a dead core.
        "orders": s.orders.iter().map(|o| serde_json::json!({
            "orderId": o.order_id, "strategy": o.strategy, "asset": o.asset,
            "direction": o.direction, "tokenId": o.token_id, "side": o.side,
            "mode": o.mode, "price": o.price, "size": o.size,
            "filledSize": o.filled_size, "status": o.status,
            // E28 (§9.2): the account the order belongs to (empty = pre-E28 core).
            "accountId": o.account_id,
            // The order's venue tag (the account's binding; empty = untagged).
            "venue": o.venue,
            "roundSlot": o.round_slot })).collect::<Vec<_>>(),
        "trades": { "count": s.trades.len(), "net": s.net_pnl(), "winRate": s.win_rate() },
        // All-time totals (trades.summary): cumulative order count + net profit
        // NOT capped by the windowed history above. Older cores omit → null.
        "tradeSummary": s.trade_summary,
        // VERSIONING.md §5: version + build provenance + update three-state.
        // Older cores refuse the method → null; the card says so instead of
        // inventing a version number.
        "systemVersion": s.system_version,
        // #379: the staging state machine. The snapshot type has carried it
        // since #379, but the gateway never WROTE the key — the WebUI's
        // staging badge read `snapshot.stageState` and always saw null. The
        // snapshot fills it read-only from system.version's `stage` (never
        // the download verb); older cores → null, rendered as "cannot stage".
        "stageState": s.stage_state,
        // Full closed-trade rows for the history tab (snapshot already capped
        // by the trade_limit the bin passes to IpcClient::snapshot).
        "tradeRows": s.trades.iter().map(|t| serde_json::json!({
            "id": t.id, "strategy": t.strategy, "asset": t.asset,
            "direction": t.direction, "entryPrice": t.entry_price,
            "exitPrice": t.exit_price, "shares": t.shares,
            "netPnlUsd": t.net_pnl_usd, "netPnlPct": t.net_pnl_pct,
            "feesUsd": t.fees_usd, "exitReason": t.exit_reason,
            "holdTimeSec": t.hold_time_sec,
            // #427: the Venue column (empty = undeclared).
            "venue": t.venue,
            // E28 (§9.2): the account the trade settled in (empty = pre-E28
            // record/core; the panel leaves unattributed rows visible).
            "accountId": t.account_id,
            "entryTime": t.entry_time, "exitTime": t.exit_time })).collect::<Vec<_>>(),
        // E9-g: per-strategy accounting rows for the plugins/strategies page.
        "strategyStats": s.strategy_stats.iter().map(|r| serde_json::json!({
            "name": r.name, "enabled": r.enabled, "source": r.source,
            "ordersPlaced": r.orders_placed, "ordersRejected": r.orders_rejected,
            "limitRejected": r.limit_rejected,
            "blockedTiming": r.blocked_timing, "blockedMomentum": r.blocked_momentum,
            "gateExemptedTiming": r.gate_exempted_timing,
            "gateExemptedMomentum": r.gate_exempted_momentum,
            "gateExemptions": r.gate_exemptions,
            "closedTrades": r.closed_trades, "wins": r.wins, "losses": r.losses,
            "netPnlUsd": r.net_pnl_usd,
            "rejectionCauses": r.rejection_causes,
        })).collect::<Vec<_>>(),
        // E9-g: engine counters for the overview page (older cores omit).
        //
        // #236: the four trading-safety keys ride along with them. They are NOT
        // optional decoration — they are the only source of the panel's freeze /
        // last-error / self-check banners, and before this they were dropped a
        // layer up (a key the view type could not carry is a key no adapter can
        // render). A core that omits one yields `null`, which every consumer
        // reads as "unknown" and renders as no banner.
        //
        // Built in a block with named intermediates rather than inline: four
        // more nested `json!` calls in one literal blow past the macro recursion
        // limit, and the closure body keeps each block shallow.
        "stats": s.stats.as_ref().map(|st| {
            let last_error = st.last_error.as_ref().map(|e| {
                serde_json::json!({ "tsMs": e.ts_ms, "code": e.code, "message": e.message })
            });
            let last_venue_error = st.last_venue_error.as_ref().map(|e| {
                serde_json::json!({ "tsMs": e.ts_ms, "message": e.message })
            });
            let reconcile = st.reconcile.as_ref().map(|r| {
                serde_json::json!({
                    "consecutiveSweepFailures": r.consecutive_sweep_failures,
                    "freezeThreshold": r.freeze_threshold })
            });
            let self_check = st.self_check.as_ref().map(|c| {
                serde_json::json!({
                    "ok": c.ok, "tsMs": c.ts_ms,
                    "items": c.items.iter().map(|i| serde_json::json!({
                        "name": i.name, "ok": i.ok, "detail": i.detail,
                    })).collect::<Vec<_>>() })
            });
            // `reason` is omitted rather than nulled while trading is live, which
            // is how the kernel sends it — the panel reads a missing reason as
            // "no reason given", never as a different state.
            let trading_frozen = st.trading_frozen.as_ref().map(|f| match &f.reason {
                Some(r) => serde_json::json!({ "active": f.active, "reason": r }),
                None => serde_json::json!({ "active": f.active }),
            });
            let strategies = st
                .strategies
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "name": r.name, "enabled": r.enabled, "source": r.source,
                        "ordersPlaced": r.orders_placed, "ordersRejected": r.orders_rejected,
                        "limitRejected": r.limit_rejected,
                        "blockedTiming": r.blocked_timing, "blockedMomentum": r.blocked_momentum,
                        "gateExemptedTiming": r.gate_exempted_timing,
                        "gateExemptedMomentum": r.gate_exempted_momentum,
                        "gateExemptions": r.gate_exemptions,
                        "closedTrades": r.closed_trades, "wins": r.wins, "losses": r.losses,
                        "netPnlUsd": r.net_pnl_usd,
                        "rejectionCauses": r.rejection_causes,
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "books": st.books, "tops": st.tops, "spots": st.spots,
                "rounds": st.rounds, "evaluations": st.evaluations,
                "signals": st.signals, "placeRejected": st.place_rejected,
                // The token list itself (sorted upstream in `engine_stats_at`),
                // not just the count: the TUI's Markets card shows a
                // `trend confirmed:` preview the web panel had no access to —
                // the wire only carried `orderCounts`. The panel renders the
                // same 8-char-prefix form; raw 77-digit ids never leave the
                // kernel untruncated on screen.
                "confirmed": st.confirmed,
                "venueRejected": st.venue_rejected,
                "lastError": last_error,
                "lastVenueError": last_venue_error,
                "reconcile": reconcile,
                "selfCheck": self_check,
                "tradingFrozen": trading_frozen,
                "strategies": strategies,
            })
        }),
        "strategies": s.strategies.iter().map(|x| serde_json::json!(x.clone())).collect::<Vec<_>>(),
        "extensions": s.extensions.iter().map(|x| serde_json::json!(x.clone())).collect::<Vec<_>>(),
        "marketPlugins": s.market_plugins.iter().map(|x| serde_json::json!(x.clone())).collect::<Vec<_>>(),
        // E8-c 盘口深度: per-asset L2 depth for the HFT panel. Older cores omit
        // the verb → empty array, which the panel reads as "no depth data".
        // E29 (§12.2): the token ids must ride along — the K-line card asks
        // `kline.history` by TOKEN id, and without them the card hides for
        // every plugin (the deserializer defaults the missing keys to "").
        "books": s.books.iter().map(|a| serde_json::json!({
            "asset": a.asset,
            "upTokenId": a.up_token_id,
            "downTokenId": a.down_token_id,
            // 账户↔venue 解耦（本轮）：盘口行的 venue（token 反查所得，
            // 与 engine.books 的富化同源）——HftPage 的 venue Loading 门
            // 同时盯着 prices 与 books 两路行。
            "venue": a.venue,
            "up": book_side_json(&a.up),
            "down": book_side_json(&a.down),
        })).collect::<Vec<_>>(),
        // HFT template identity: which market plugin is driving this session
        // and its market class (prediction/spot/futures/options).
        "marketActiveName": s.market_active_name,
        "marketActiveType": s
            .market_active_name
            .as_ref()
            .and_then(|n| s.market_plugins.iter().find(|p| &p.name == n))
            .map(|p| p.kind.clone()),
        // 显示账户配置化: the display pointer the snapshot mirrors — the
        // panel's 行情页 shows THIS account (and its venue binding, empty =
        // unbound), decoupled from the connection's active account. Older
        // cores omit the fields → "" (the UI reads absence as "no pointer
        // surfaced"; a same-revision stack always carries both).
        "displayAccount": s.display_account,
        "displayVenue": s.display_venue,
        "lastError": s.last_error,
    });

    // E13: the evolution proposal workflow. The raw proposal rows carry the
    // full 对比 block (baseline vs variant metrics + knob moves); `autoEvolve`
    // is the checkbox state the panel toggles via /api/command.
    doc["evolution"] = serde_json::json!({
        "proposals": s.evolution_proposals.iter()
            .map(|p| serde_json::json!({
                "id": p.id, "strategy": p.strategy, "state": p.state,
                "reason": p.reason, "confidence": p.confidence,
                "sampleCount": p.sample_count,
                "dims": p.dims, "knobMoves": p.knob_moves(),
                "baseline": p.baseline, "variant": p.variant,
                "createdAtMs": p.created_at_ms, "expiresAtMs": p.expires_at_ms,
                "decidedBy": p.decided_by, "decidedAtMs": p.decided_at_ms,
                // #251: why it ended. Without it the ledger renders every
                // decision as one word, and "the guard refused it" reads the
                // same as "an operator said no".
                "decidedReason": p.decided_reason,
                "cycleSeq": p.cycle_seq,
            })).collect::<Vec<_>>(),
        "status": s.evolution_status,
    });

    // Process-control capabilities, inserted only when this server has a
    // dispatcher. `managed` is the load-bearing one for 停止: an adopted core
    // cannot be stopped from here even with `--manage`, so the panel must not
    // present a live stop button for it.
    if let Some(l) = lifecycle {
        doc["gateway"] = serde_json::json!({
            "lifecycleEnabled": l.enabled,
            "managed": l.managed,
            "corePid": l.pid,
            "socket": l.socket,
            // E12-c crash-recovery state. `lastExit.kind` is the load-bearing
            // field: without it the panel cannot tell "it crashed" from "we
            // stopped it", and both look like a missing pid.
            "restarts": l.restarts,
            "restartGivenUp": l.restart_given_up,
            "lastExit": l.last_exit,
            // #186: the alert behind `restartGivenUp`. A panel that only shows
            // the boolean can say "gave up" but not why, and "why" is what an
            // operator needs at 3am.
            "giveUp": l.give_up,
        });
    }

    // #185: this server's own exposure. Always present when the caller supplies
    // it, including the healthy case (`loopbackOnly: true`), so a panel can show
    // a banner from the value rather than from the absence of one.
    if let Some(sec) = security {
        doc["security"] = sec;
    }
    doc.to_string()
}

/// The red banners at the top of the built-in panel.
///
/// Two facts are worth interrupting the operator for, and both are states the
/// panel would otherwise render as "everything is fine":
///
///   * the kernel is down and the supervisor has stopped trying to bring it back
///     (#186) — the give-up alert, with the core's own last words;
///   * this panel is listening on a non-loopback address (#185), so its plaintext
///     HTTP surface is reachable from the network.
pub(super) fn panel_notices_html(
    lifecycle: Option<&LifecycleView>,
    bind_warning: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(w) = bind_warning {
        out.push_str(&format!(
            "<div class=alert><h2>⚠ 面板正在监听非本地地址</h2><div>{}</div></div>",
            esc(w)
        ));
    }
    if let Some(g) = lifecycle.and_then(|l| l.give_up.as_ref()) {
        let tail = if g.stderr_tail.is_empty() {
            "<div class=sub>内核退出前没有向 stderr 写入任何内容</div>".to_string()
        } else {
            format!(
                "<div class=sub>内核最后的 stderr 输出：</div><pre>{}</pre>",
                esc(&g.stderr_tail.join("\n"))
            )
        };
        out.push_str(&format!(
            "<div class=alert><h2>⚠ 内核已停机：已放弃重启</h2>\
             <div>{}</div>\
             <div class=sub>重启尝试 {} 次 · 发生于 {} · 交易已停止，需人工启动内核</div>{}</div>",
            esc(&g.message),
            g.attempts,
            relative_time(g.at_ms),
            tail
        ));
    }
    out
}

/// "N 秒前 / N 分钟前 / N 小时前" from a unix-millisecond stamp. Deliberately
/// relative: the panel is read live, and a relative age needs no timezone.
pub(super) fn relative_time(at_ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let secs = now.saturating_sub(at_ms) / 1000;
    if secs < 60 {
        format!("{secs} 秒前")
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else {
        format!("{} 小时前", secs / 3600)
    }
}

/// One token's book side for the JSON snapshot (E8-c). `None` metrics pass
/// through as JSON null so the panel cannot mistake them for real quotes.
pub(super) fn book_side_json(side: &crate::core::types::BookSideView) -> serde_json::Value {
    let level = |l: &crate::core::types::BookLevelView| serde_json::json!({ "price": l.price, "size": l.size });
    serde_json::json!({
        "bids": side.bids.iter().map(&level).collect::<Vec<_>>(),
        "asks": side.asks.iter().map(&level).collect::<Vec<_>>(),
        "bestBid": side.best_bid,
        "bestAsk": side.best_ask,
        "midPrice": side.mid_price,
        "obi": side.obi,
        "spread": side.spread,
        "spreadPct": side.spread_pct,
    })
}
