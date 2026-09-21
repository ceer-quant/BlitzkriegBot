//! Web adapter — renders a `UiSnapshot` to HTML (and JSON endpoints) and serves
//! it from a dependency-free HTTP/1.1 server so the browser panel can be opened
//! directly. This is the browser-facing sibling of the TUI and app adapters; it
//! contains no trading logic.
//!
//! When constructed with a [`Dispatcher`] (gateway mode) the server additionally
//! exposes the command surface:
//!
//!   GET  /                    HTML panel (auto-refresh)
//!   GET  /api/snapshot        JSON snapshot
//!   GET  /api/command?cmd=..  dispatch one command → JSON outcome
//!   POST /api/command         body = command text (or `{"cmd":"..."}`) → JSON
//!
//! Command dispatch is trading-logic-free: it starts/stops the *process* and
//! reads state. No order is ever placed from here.

use crate::core::ipc_client::IpcClient;
use crate::core::types::UiSnapshot;
use crate::gateway::Dispatcher;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

pub mod gateway_run;
pub use gateway_run::run_web_gateway;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn money(v: f64) -> String {
    format!("{}{:.2}", if v >= 0.0 { "+" } else { "-" }, v.abs())
}
fn pct(v: f64) -> String {
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
        rows.push_str(&format!(
            "<div class=card><h2>Round</h2><div class=big>#{}</div><div class=sub>{}s old · {}s left · {}</div></div>",
            r.slot, r.age_sec, r.time_left_sec,
            if r.can_trade { "TRADING" } else { "WAITING" }
        ));
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
            "prices": r.market_prices.iter().map(|m| serde_json::json!({
                "asset": m.asset, "up": m.up, "down": m.down })).collect::<Vec<_>>() })),
        "positions": s.positions.iter().map(|p| serde_json::json!({
            "asset": p.asset, "direction": p.direction, "entryPrice": p.entry_price,
            "currentPrice": p.current_price, "unrealizedPct": p.unrealized_pct,
            "strategy": p.strategy, "shares": p.shares,
            "remainingSec": p.remaining_sec })).collect::<Vec<_>>(),
        "trades": { "count": s.trades.len(), "net": s.net_pnl(), "winRate": s.win_rate() },
        // All-time totals (trades.summary): cumulative order count + net profit
        // NOT capped by the windowed history above. Older cores omit → null.
        "tradeSummary": s.trade_summary,
        // Full closed-trade rows for the history tab (snapshot already capped
        // by the trade_limit the bin passes to IpcClient::snapshot).
        "tradeRows": s.trades.iter().map(|t| serde_json::json!({
            "id": t.id, "strategy": t.strategy, "asset": t.asset,
            "direction": t.direction, "entryPrice": t.entry_price,
            "exitPrice": t.exit_price, "shares": t.shares,
            "netPnlUsd": t.net_pnl_usd, "netPnlPct": t.net_pnl_pct,
            "feesUsd": t.fees_usd, "exitReason": t.exit_reason,
            "holdTimeSec": t.hold_time_sec,
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
                "orderCounts": st.confirmed.len(),
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
        "books": s.books.iter().map(|a| serde_json::json!({
            "asset": a.asset,
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
fn panel_notices_html(lifecycle: Option<&LifecycleView>, bind_warning: Option<&str>) -> String {
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
fn relative_time(at_ms: i64) -> String {
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
fn book_side_json(side: &crate::core::types::BookSideView) -> serde_json::Value {
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

/// Minimal std-only base64 decoder for basic-auth passwords (the only place
/// the web layer needs it). Returns raw bytes; callers validate UTF-8.
fn data_encoding_free_base64(s: &str) -> Vec<u8> {
    const TBL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let Some(v) = TBL.iter().position(|t| *t == c) else {
            return out;
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}
/// Minimal parsed HTTP request.
struct HttpRequest {
    method: String,
    target: String,
    body: String,
    /// Raw header (name, value) pairs, lowercased names.
    headers: Vec<(String, String)>,
}

impl HttpRequest {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }
    /// Case-insensitive header lookup over the stored lines.
    fn header(&self, name: &str) -> Option<&str> {
        // Header case is handled by the caller storing the raw lines; keep the
        // contract tiny: headers were captured lowercased by read_request.
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    /// First `cmd` value from the query string, percent-decoded (`+` → space).
    fn query_cmd(&self) -> Option<String> {
        let q = self.target.split_once('?')?.1;
        for pair in q.split('&') {
            if let Some(v) = pair.strip_prefix("cmd=") {
                return Some(url_decode(v));
            }
        }
        None
    }
}

/// Byte-equal compare in time proportional to the expected value, so short
/// wrong guesses are not observably faster than long ones.
fn same_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        // Do not leak length via early exit: still burn a compare pass.
        let sink = a.iter().chain(b.iter()).fold(0u8, |acc, x| acc ^ x);
        return sink == !0u8; // practically never true
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// First candidate whose canonical target is an existing `index.html` file;
/// returns its parent dir.
fn resolve_panel_dir(candidates: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    for c in candidates {
        if let Ok(dir) = c.canonicalize() {
            if dir.is_file() {
                return dir.parent().map(std::path::Path::to_path_buf);
            }
        }
    }
    None
}

/// Repo-rooted path to the built Vue panel's `index.html`/assets. Relative to
/// the ui_kit crate so it holds for both `cargo run` and a repo checkout. A
/// binary built in one checkout and run from another (the `blitzkrieg`
/// launcher staged into `<repo>/target/release`) has a dead manifest path, so
/// also try two levels up from the executable — that repo shape puts the
/// panel at `<repo>/ui/webapp/webui/dist`.
fn vue_panel_dir() -> Option<std::path::PathBuf> {
    // CARGO_MANIFEST_DIR is <build>/ui/ui_kit; the built Vue app lives at
    // <build>/ui/webapp/webui/dist.
    let manifest = env!("CARGO_MANIFEST_DIR");
    let mut candidates =
        vec![std::path::Path::new(manifest).join("../webapp/webui/dist/index.html")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../../ui/webapp/webui/dist/index.html"));
        }
    }
    resolve_panel_dir(&candidates)
}

/// Serve a static asset from the Vue panel dir; `/panel/` or `/panel` (no
/// trailing file) resolves to `index.html`. System path traversal is blocked.
fn serve_vue_panel_asset(path: &str) -> Option<(Vec<u8>, &'static str)> {
    let rel = path.strip_prefix("/panel/")?;
    if rel.split('/').any(|seg| seg == ".." || seg.is_empty()) {
        return None;
    }
    serve_vue_panel_file(&std::path::PathBuf::from(rel))
}

fn serve_vue_panel() -> Option<(Vec<u8>, &'static str)> {
    serve_vue_panel_file(&std::path::PathBuf::from("index.html"))
}

fn serve_vue_panel_file(rel: &std::path::Path) -> Option<(Vec<u8>, &'static str)> {
    let dir = vue_panel_dir()?;
    let full = dir.join(rel).canonicalize().ok()?;
    // Canonical path must stay inside the panel dir.
    if !full.starts_with(&dir) {
        return None;
    }
    let body = std::fs::read(&full).ok()?;
    let ctype = match full.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    // Served as raw bytes: the panel ships PNG icons alongside its text assets,
    // and a lossy UTF-8 round-trip would mangle every byte outside ASCII (and
    // skew the Content-Length with the replacement characters it inserts).
    Some((body, ctype))
}

fn url_decode(s: &str) -> String {
    let bytes = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── Panel auth ───────────────────────────────────────────────────────────────
//
// The shape follows freqtrade's REST API — the mature peer for exactly this kind
// of program (a single-operator trading bot with a browser panel):
//
//   * **Auth is required, not optional.** freqtrade documents that every endpoint
//     except `/ping` "returns sensitive info and requires authentication". The
//     same reasoning applies with more force here: this server can start and stop
//     the trading core, so gateway mode always demands a session and refuses to
//     start without credentials from the environment, rather than serving an open
//     command surface — and rather than inventing a password of its own, which
//     the operator could neither rotate nor audit.
//   * **Loopback is not a trust boundary.** "Only reachable from localhost" does
//     not stop a page the operator happens to visit from issuing cross-site
//     requests to 127.0.0.1 — a cross-site `<img>`/`<form>` is a *simple* request
//     and gets no CORS preflight, so `<img src="…/api/command?cmd=stop">` used to
//     reach a lifecycle verb. The session requirement is what closes that; the
//     origin gate is the second layer.
//   * **Sessions expire.** freqtrade pairs a 15-minute access token with a
//     refresh token; a panel session here carries an absolute TTL plus an idle
//     timeout, and the live set is capped.
//
// Deliberate divergences: no JWT and no separate refresh endpoint (a single
// opaque, revocable, in-memory token is simpler and supports real logout, which
// a stateless JWT cannot), and credentials stay in the environment rather than a
// config file (`BLITZKRIEG_*` already owns that surface).

/// Absolute session lifetime.
const SESSION_TTL_MS: u64 = 12 * 60 * 60 * 1_000;
/// Idle timeout, enforced inside the absolute lifetime.
const SESSION_IDLE_MS: u64 = 30 * 60 * 1_000;
/// Live-session cap; the oldest is evicted past it.
const MAX_SESSIONS: usize = 64;

/// Failed logins from one client before it is locked out (#185).
///
/// Ten is the number the audit asked for and a defensible one for a password a
/// human types: it leaves room for honest typos and is far below what an online
/// guessing attack needs.
const MAX_LOGIN_FAILURES: u32 = 10;
/// First lockout (after the 10th failure). Doubles per further failure, so a
/// client that keeps going is pushed out to [`LOGIN_LOCKOUT_MAX_MS`].
const LOGIN_LOCKOUT_BASE_MS: u64 = 30_000;
/// Ceiling on the lockout: an operator who mistypes must not be locked out for
/// a day, and an attacker must not be handed a denial of service against them.
const LOGIN_LOCKOUT_MAX_MS: u64 = 15 * 60 * 1_000;
/// Bounded throttle state — at most this many clients are remembered.
const MAX_LOGIN_TRACKED: usize = 512;
/// Ceiling on the per-failure delay below.
const LOGIN_FAILURE_DELAY_MAX_MS: u64 = 400;

/// Delay applied before answering a FAILED login: 100 ms doubling to 400 ms.
///
/// This is a speed bump, not the defence — the lockout is. It exists so that a
/// fast guessing loop pays for every attempt even before the counter trips. It is
/// deliberately short and capped: this server answers one connection at a time
/// (see [`WebServer::serve`]), so a long sleep would let an attacker stall the
/// operator's own panel, which is the denial of service the throttle is meant to
/// prevent. Successful logins are never delayed.
fn login_failure_delay_ms(failures: u32) -> u64 {
    let shift = failures.saturating_sub(1).min(3);
    (100u64 << shift).min(LOGIN_FAILURE_DELAY_MAX_MS)
}

/// Lockout for a client that has failed `failures` times: 0 below the threshold,
/// then doubling from [`LOGIN_LOCKOUT_BASE_MS`], capped.
fn login_lockout_ms(failures: u32) -> u64 {
    if failures < MAX_LOGIN_FAILURES {
        return 0;
    }
    let over = u64::from(failures - MAX_LOGIN_FAILURES).min(20);
    LOGIN_LOCKOUT_BASE_MS
        .saturating_mul(1u64 << over)
        .min(LOGIN_LOCKOUT_MAX_MS)
}

fn auth_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Failed-login accounting for one client (an IP address).
#[derive(Debug, Clone, Copy, Default)]
struct LoginAttempt {
    failures: u32,
    /// Unix ms until which this client is refused without the credentials even
    /// being examined.
    locked_until_ms: u64,
    /// When this record was last touched, for bounded-map eviction.
    last_ms: u64,
}

/// The address this panel actually bound, recorded by [`WebServer::serve`].
#[derive(Debug, Clone)]
struct BindInfo {
    addr: String,
    /// Bound to a loopback interface only.
    loopback_only: bool,
}

/// One issued session. `seen_ms` refreshes on use, which is what makes idleness
/// measurable rather than merely "issued long ago".
#[derive(Debug, Clone, Copy)]
struct Session {
    issued_ms: u64,
    seen_ms: u64,
}

/// `n` bytes of OS entropy as lowercase hex.
///
/// Used for session tokens. These are bearer credentials for process control, so
/// they must come from the OS rather than a timestamp/PID mix — anything
/// predictable is forgeable by an attacker who can guess when the process
/// started. (Panel *passwords* are not generated here at all; they come from the
/// environment. See [`WebServer::require_credentials`].)
fn random_hex(n_bytes: usize) -> String {
    use std::io::Read;
    let mut bytes = vec![0u8; n_bytes];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !ok {
        // No /dev/urandom (very unlikely on unix): still produce a value, so the
        // panel stays LOCKED rather than silently open. Weak entropy is the
        // lesser failure against an unlocked command surface.
        let mut seed = auth_now_ms()
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407)
            ^ (std::process::id() as u64) << 32;
        for b in bytes.iter_mut() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (seed >> 24) as u8;
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Is this `Origin`/`Referer` value the local machine?
///
/// Accepts any loopback spelling (`127.x.y.z`, `localhost`, `[::1]`) with or
/// without a scheme, port, path or `user@` prefix. Anything else is foreign.
fn is_loopback_origin(value: &str) -> bool {
    let v = value.trim().to_ascii_lowercase();
    let rest = v
        .strip_prefix("http://")
        .or_else(|| v.strip_prefix("https://"))
        .unwrap_or(&v);
    let authority = rest.split('/').next().unwrap_or("");
    // `user@host` — the credentials are irrelevant, the host is not.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("") // [::1]:51888 → ::1
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if host == "localhost" || host == "::1" {
        return true;
    }
    // Must be a literal `127.a.b.c` — *parsed*, not prefix-matched. A bare
    // `starts_with("127.")` also accepts `127.0.0.1.evil.example`, which is a
    // hostname the attacker controls.
    let octets: Vec<&str> = host.split('.').collect();
    octets.len() == 4
        && octets[0] == "127"
        && octets[1..]
            .iter()
            .all(|o| !o.is_empty() && o.len() <= 3 && o.bytes().all(|b| b.is_ascii_digit()))
}

/// `(host, port)` of a URL-ish origin, the port default-filled by scheme
/// (`http` → 80, `https` → 443) so `http://host` equals `Host: host:80`.
/// Returns `None` when there is no authority at all. Hostnames are
/// lowercased — DNS names are case-insensitive.
fn origin_authority(candidate: &str) -> Option<(String, String)> {
    let lower = candidate.trim().to_ascii_lowercase();
    let (scheme, rest) = match lower.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("http", lower.as_str()),
    };
    let authority = rest.split('/').next()?;
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return None;
    }
    let default_port = if scheme == "https" { "443" } else { "80" };
    if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed.find(']')?;
        let host = format!("[{}]", &bracketed[..end]);
        let port = bracketed[end + 1..]
            .strip_prefix(':')
            .filter(|p| !p.is_empty())
            .unwrap_or(default_port);
        return Some((host, port.to_string()));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            Some((h.to_string(), p.to_string()))
        }
        _ => Some((authority.to_string(), default_port.to_string())),
    }
}

/// Same-origin: the `Origin`/`Referer` authority equals the request's own
/// `Host` header. This is the server-deployment branch of
/// [`WebServer::origin_allowed`] — see its docs for why a hostile page cannot
/// produce this pair.
fn is_same_origin(req: &HttpRequest, candidate: &str) -> bool {
    let Some(host) = req.header("host") else {
        return false;
    };
    let Some(referer) = origin_authority(candidate) else {
        return false;
    };
    let Some(ours) = origin_authority(&format!("http://{host}")) else {
        return false;
    };
    referer == ours
}

/// Session token from an `Authorization` header: `Bearer <token>`, or the user
/// half of a `Basic` pair (the form the E6-a panel shipped with).
fn bearer_or_basic(value: &str) -> Option<String> {
    if let Some(t) = value.strip_prefix("Bearer ") {
        return Some(t.to_string());
    }
    let b = value.strip_prefix("Basic ")?;
    let raw = data_encoding_free_base64(b);
    std::str::from_utf8(&raw)
        .ok()
        .and_then(|d| d.split(':').next().map(String::from))
}

/// A trivial, dependency-free HTTP server that always renders a fresh snapshot.
pub struct WebServer {
    snapshot_src: Arc<Mutex<IpcClient>>,
    trade_limit: usize,
    /// Present only in gateway mode (`--manage`); enables `/api/command`.
    dispatcher: Option<Arc<Mutex<Dispatcher>>>,
    /// Panel credentials (`BLITZKRIEG_PANEL_USER` / `…_PASSWORD`), read from the
    /// environment. Gateway mode refuses to start without a complete pair (see
    /// [`Self::require_credentials`]) — it never invents one.
    panel_user: Option<String>,
    panel_password: Option<String>,
    /// Whether `/api/*` demands a valid session. Armed by gateway mode.
    auth_required: bool,
    /// Non-loopback origins explicitly trusted (operator opt-in).
    allowed_origins: Vec<String>,
    /// Sessions issued by `POST /api/login`, keyed by token.
    sessions: Arc<Mutex<std::collections::BTreeMap<String, Session>>>,
    /// Failed-login state, keyed by client address (see [`LoginAttempt`]).
    login_failures: Arc<Mutex<std::collections::BTreeMap<String, LoginAttempt>>>,
    /// The address `serve()` bound, recorded so the panel can tell the operator
    /// when it is reachable beyond this machine (#185). `None` before binding.
    bind: Arc<Mutex<Option<BindInfo>>>,
}

impl WebServer {
    /// Read-only panel (no command API).
    pub fn new(client: IpcClient, trade_limit: usize) -> Self {
        Self {
            snapshot_src: Arc::new(Mutex::new(client)),
            trade_limit,
            dispatcher: None,
            panel_user: None,
            panel_password: None,
            // No lifecycle verbs on this surface, so auth stays opt-in here.
            // Gateway mode (below) arms it unconditionally.
            auth_required: false,
            allowed_origins: Vec::new(),
            sessions: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            login_failures: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            bind: Arc::new(Mutex::new(None)),
        }
    }

    /// Guarantee the panel has credentials before it serves anything.
    ///
    /// Credentials come from the environment and *only* from the environment.
    /// There is deliberately no minted/generated fallback: a password the panel
    /// invents for itself is one the operator did not choose and cannot manage
    /// (no rotation, no secret store, no audit story), and having the process
    /// print it to a terminal makes the terminal — not the credential store —
    /// the source of truth for who can stop the trading core.
    ///
    /// Returns the reason the panel must not start, when it must not.
    pub fn require_credentials(&self) -> Result<(), String> {
        if !self.auth_required {
            return Ok(()); // read-only surface, no command verbs
        }
        if self.panel_user.is_some() && self.panel_password.is_some() {
            return Ok(());
        }
        Err(
            "gateway mode requires panel credentials, but BLITZKRIEG_PANEL_USER and \
             BLITZKRIEG_PANEL_PASSWORD are not both set. A half-configured pair counts as \
             unconfigured, on purpose: guessing which half was meant is how a panel ends up \
             open. Export both and restart."
                .to_string(),
        )
    }

    /// True when a usable user/password pair is configured.
    pub fn credentials_configured(&self) -> bool {
        self.panel_user.is_some() && self.panel_password.is_some()
    }

    /// True when the panel requires a session on `/api/*`.
    pub fn auth_required(&self) -> bool {
        self.auth_required
    }

    fn token_from_query(&self, req: &HttpRequest) -> Option<String> {
        let q = req.target.split_once('?')?.1;
        for pair in q.split('&') {
            if let Some(v) = pair.strip_prefix("token=") {
                return Some(url_decode(v));
            }
        }
        None
    }

    /// Arm user/password auth from env (`BLITZKRIEG_PANEL_USER` +
    /// `BLITZKRIEG_PANEL_PASSWORD`).
    ///
    /// A half-configured pair is treated as "nothing configured" rather than as a
    /// password-less panel: guessing which half the operator meant is how a panel
    /// ends up open. Gateway mode then refuses to start, via
    /// [`Self::require_credentials`], rather than substituting a password of its
    /// own choosing.
    pub fn set_panel_credentials(&mut self, user: Option<String>, password: Option<String>) {
        let user = user.filter(|u| !u.trim().is_empty());
        let password = password.filter(|p| !p.trim().is_empty());
        if user.is_none() || password.is_none() {
            self.panel_user = None;
            self.panel_password = None;
            return;
        }
        self.panel_user = user;
        self.panel_password = password;
    }

    /// Extra origins accepted beyond loopback — the `CORS_origins` analogue in
    /// freqtrade's config. Operators who deliberately front the panel with a
    /// hostname list it here; everything else is refused.
    pub fn set_allowed_origins(&mut self, origins: Vec<String>) {
        self.allowed_origins = origins
            .into_iter()
            .map(|o| o.trim().trim_end_matches('/').to_ascii_lowercase())
            .filter(|o| !o.is_empty())
            .collect();
    }

    /// Is this request's origin trustworthy?
    ///
    /// Three things pass, in order of strength:
    ///
    /// 1. **No evidence** — no `Origin`, no `Referer`. The normal shape of
    ///    curl, of the gate scripts, and of non-browser clients. Refusing it
    ///    would buy nothing against a browser (a cross-origin `POST` always
    ///    carries `Origin`, or the opaque `null`, which none of the passing
    ///    branches accept) while breaking every CLI client.
    /// 2. **Loopback origins** — the panel on the operator's own machine.
    /// 3. **Same origin** — the evidence's authority equals the request's own
    ///    `Host` header. This is what makes the SERVER deployment work with
    ///    zero configuration: a browser pointed at `http://<server-ip>:51888`
    ///    attaches exactly that authority, and a same-origin request is not
    ///    cross-site by definition. A hostile page cannot forge the pair —
    ///    the browser fills `Host` with the server it is actually talking to,
    ///    so an attacker's page at `evil.example` produces
    ///    `Origin: evil.example` + `Host: <ours>`, which does not match.
    ///
    /// Anything else must be listed in `allowed_origins` (reverse proxies that
    /// rewrite the hostname). What actually protects the state-changing
    /// surface is the *session* requirement in [`Self::authorize`]; this gate
    /// is the second layer, and the cross-site `GET` vector (`<img src>` with
    /// no `Origin`, no readable cookie) is stopped by the session demand.
    fn origin_allowed(&self, req: &HttpRequest) -> bool {
        let Some(evidence) = req.header("origin").or_else(|| req.header("referer")) else {
            return true;
        };
        // A `Referer` is a full URL; only its origin is compared, which
        // `is_loopback_origin` handles by ignoring scheme, port and path.
        let candidate = evidence.trim().trim_end_matches('/').to_ascii_lowercase();
        if is_loopback_origin(&candidate) {
            return true;
        }
        if is_same_origin(req, &candidate) {
            return true;
        }
        self.allowed_origins
            .iter()
            .any(|a| candidate == *a || candidate.starts_with(&format!("{a}/")))
    }

    /// Issue a session token for valid panel credentials.
    fn login(&self, user: &str, password: &str) -> Option<String> {
        let (expected_user, expected_pass) =
            (self.panel_user.as_deref()?, self.panel_password.as_deref()?);
        // Constant-time compare to blunt trivial timing probes. Both halves are
        // always evaluated so a wrong user and a wrong password look alike.
        let user_ok = same_time_eq(user.as_bytes(), expected_user.as_bytes());
        let pass_ok = same_time_eq(password.as_bytes(), expected_pass.as_bytes());
        if !(user_ok && pass_ok) {
            return None;
        }
        let now = auth_now_ms();
        let token = random_hex(20); // 40 hex chars, 160 bits of OS entropy
        if let Ok(mut s) = self.sessions.lock() {
            Self::evict_expired(&mut s, now);
            if s.len() >= MAX_SESSIONS {
                // Oldest-first eviction keeps a runaway login loop from growing
                // the map without bound.
                if let Some(oldest) = s
                    .iter()
                    .min_by_key(|(_, v)| v.seen_ms)
                    .map(|(k, _)| k.clone())
                {
                    s.remove(&oldest);
                }
            }
            s.insert(
                token.clone(),
                Session {
                    issued_ms: now,
                    seen_ms: now,
                },
            );
        }
        Some(token)
    }

    fn evict_expired(sessions: &mut std::collections::BTreeMap<String, Session>, now: u64) {
        sessions.retain(|_, s| {
            now.saturating_sub(s.issued_ms) < SESSION_TTL_MS
                && now.saturating_sub(s.seen_ms) < SESSION_IDLE_MS
        });
    }

    /// Does this token name a live session? Expired ones are dropped on sight.
    fn session_valid(&self, token: &str) -> bool {
        let now = auth_now_ms();
        let Ok(mut s) = self.sessions.lock() else {
            return false;
        };
        Self::evict_expired(&mut s, now);
        match s.get_mut(token) {
            Some(entry) => {
                entry.seen_ms = now;
                true
            }
            None => false,
        }
    }

    /// Revoke a session (logout).
    fn logout(&self, token: &str) {
        if let Ok(mut s) = self.sessions.lock() {
            s.remove(token);
        }
    }

    /// Number of live sessions (tests + introspection).
    pub fn session_count(&self) -> usize {
        self.sessions.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// The client address a login attempt is charged to.
    ///
    /// The peer address, not the submitted username: charging the username would
    /// let an attacker lock the operator's own account out by guessing it, which
    /// turns a throttle into a denial of service. A connection whose peer address
    /// cannot be read shares one bucket, which is the conservative direction.
    fn login_client_key(stream: &TcpStream) -> String {
        stream
            .peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|_| "unknown".to_string())
    }

    /// Seconds this client must wait, when it is locked out.
    fn login_lockout_remaining(&self, key: &str) -> Option<u64> {
        let now = auth_now_ms();
        let guard = self.login_failures.lock().ok()?;
        let entry = guard.get(key)?;
        (entry.locked_until_ms > now).then(|| (entry.locked_until_ms - now).div_ceil(1_000))
    }

    /// Count a failed login and (past the threshold) lock the client out.
    ///
    /// Returns the new failure count, which the caller uses for the log line and
    /// the pre-response delay. It is deliberately NOT echoed to the client: the
    /// response must not depend on anything the caller supplied.
    fn note_login_failure(&self, key: &str) -> u32 {
        let now = auth_now_ms();
        let Ok(mut guard) = self.login_failures.lock() else {
            return 0;
        };
        // Bounded state: a client table that grows with every probe is its own
        // denial of service.
        if guard.len() >= MAX_LOGIN_TRACKED && !guard.contains_key(key) {
            let mut live: Vec<(String, u64)> =
                guard.iter().map(|(k, v)| (k.clone(), v.last_ms)).collect();
            live.sort_by_key(|(_, t)| *t);
            for (k, _) in live.into_iter().take(guard.len() / 4 + 1) {
                guard.remove(&k);
            }
        }
        let entry = guard.entry(key.to_string()).or_default();
        entry.failures = entry.failures.saturating_add(1);
        entry.last_ms = now;
        let lock = login_lockout_ms(entry.failures);
        if lock > 0 {
            entry.locked_until_ms = now + lock;
        }
        entry.failures
    }

    /// A successful login clears the client's record.
    fn note_login_success(&self, key: &str) {
        if let Ok(mut guard) = self.login_failures.lock() {
            guard.remove(key);
        }
    }

    /// (tracked clients, currently locked-out clients) — for the panel and tests.
    pub fn login_throttle_summary(&self) -> (usize, usize) {
        let now = auth_now_ms();
        let Ok(guard) = self.login_failures.lock() else {
            return (0, 0);
        };
        let locked = guard.values().filter(|e| e.locked_until_ms > now).count();
        (guard.len(), locked)
    }

    fn token_from_cookies(&self, req: &HttpRequest) -> Option<String> {
        let cookies = req.header("cookie")?;
        for pair in cookies.split(';') {
            let pair = pair.trim();
            if let Some(v) = pair.strip_prefix("bk_session=") {
                return Some(v.to_string());
            }
        }
        None
    }

    /// Every form this API accepts a session token in.
    fn session_token(&self, req: &HttpRequest) -> Option<String> {
        self.token_from_query(req)
            .or_else(|| self.token_from_cookies(req))
            .or_else(|| req.header("x-auth-token").map(String::from))
            .or_else(|| req.header("authorization").and_then(bearer_or_basic))
    }

    /// Gate in front of every route. Returns 200, 401 or 403.
    ///
    /// Two independent layers, in this order:
    ///
    /// 1. **Origin.** A browser's request carries `Origin` (or a `Referer`);
    ///    foreign evidence means another site is driving this request, so it is
    ///    refused — even for a caller that somehow holds a valid session. See
    ///    [`Self::origin_allowed`] for why *absent* evidence is not treated as
    ///    hostile.
    /// 2. **Session.** Required on every `/api/*` route as soon as a session can
    ///    exist (gateway mode, or credentials configured). This is the layer that
    ///    actually stops cross-site request forgery, because an HTML `<img>` or
    ///    `<form>` can issue a request but cannot attach a token — which is why
    ///    gateway mode demands one whether or not a password was configured.
    fn authorize(&self, req: &HttpRequest) -> u16 {
        // 1. Origin / CSRF gate — every route, every method, armed or not.
        if !self.origin_allowed(req) {
            return 403;
        }

        // 2. Session gate. `/api/login` and `/api/ping` are the two routes that
        //    must work without one: the first is how a session is obtained, the
        //    second is how a client tells "gateway down" apart from "session
        //    stale" — without it, a dead token is indistinguishable from an
        //    unreachable gateway, which is what left the panel stuck.
        if req.path() == "/api/login" || req.path() == "/api/ping" {
            return 200;
        }
        if !req.path().starts_with("/api/") {
            return 200; // the panel bundle itself is public; the UI gates the view
        }
        // Read-only mode with no credentials has nothing to protect: no command
        // route exists and lifecycle verbs are absent (`Dispatcher` is `None`).
        // The moment either a credential pair or gateway mode is present, a
        // session is mandatory.
        if !self.auth_required && self.panel_user.is_none() {
            return 200;
        }
        match self.session_token(req) {
            Some(t) if self.session_valid(&t) => 200,
            _ => 401,
        }
    }

    /// Liveness probe — the freqtrade `/ping` analogue, and the reason the panel
    /// can no longer get stuck.
    ///
    /// Deliberately unauthenticated, and deliberately *narrow*: it reveals only
    /// whether this process is alive and whether a session would be required. It
    /// carries no snapshot, no balance, no position. The panel uses it to tell
    /// "gateway unreachable" (no reply) apart from "my token is stale" (reply
    /// says auth is required, so a rotating token is worth spending).
    fn ping_doc(&self) -> String {
        serde_json::json!({
            "ok": true,
            "service": "blitzkrieg-panel",
            "authRequired": self.auth_required || self.panel_user.is_some(),
        })
        .to_string()
    }

    /// Panel + command API. Lifecycle verbs are gated by the dispatcher's own
    /// `lifecycle_enabled` flag.
    pub fn with_gateway(client: IpcClient, trade_limit: usize, dispatcher: Dispatcher) -> Self {
        Self {
            snapshot_src: Arc::new(Mutex::new(client)),
            trade_limit,
            dispatcher: Some(Arc::new(Mutex::new(dispatcher))),
            panel_user: None,
            panel_password: None,
            // This surface can start and stop the trading process, so it is never
            // exposed without a session.
            auth_required: true,
            allowed_origins: Vec::new(),
            sessions: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            login_failures: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            bind: Arc::new(Mutex::new(None)),
        }
    }

    /// Like [`Self::with_gateway`], but the caller KEEPS a handle to the same
    /// dispatcher.
    ///
    /// This is the form `blitzkrieg run` must use: the supervisor that spawned
    /// the core lives in that shared dispatcher, and `managed` on the wire is
    /// `supervisor.owns()`. Handing the web server a second, freshly built
    /// dispatcher would make it report `managed: false` for a core its own
    /// process spawned — the panel would then claim the core "was started by
    /// another process" and refuse 停止, even though SIGTERM handling and the
    /// core were both ours. One core, one owning dispatcher, two handles.
    pub fn with_shared_gateway(
        client: IpcClient,
        trade_limit: usize,
        dispatcher: std::sync::Arc<std::sync::Mutex<Dispatcher>>,
    ) -> Self {
        Self {
            snapshot_src: Arc::new(Mutex::new(client)),
            trade_limit,
            dispatcher: Some(dispatcher),
            panel_user: None,
            panel_password: None,
            // This surface can start and stop the trading process, so it is never
            // exposed without a session.
            auth_required: true,
            allowed_origins: Vec::new(),
            sessions: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            login_failures: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            bind: Arc::new(Mutex::new(None)),
        }
    }

    /// Serve until the process is stopped. `addr` e.g. `127.0.0.1:51888`.
    pub fn serve(&self, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        // Classify the address that was ACTUALLY bound, not the string we were
        // given: `localhost` can resolve either way, and a hostname says nothing
        // about which interface the kernel chose.
        let local = listener.local_addr().ok();
        let bound = local
            .map(|a| a.to_string())
            .unwrap_or_else(|| addr.to_string());
        let loopback_only = local.map(|a| a.ip().is_loopback()).unwrap_or(true);
        if let Ok(mut b) = self.bind.lock() {
            *b = Some(BindInfo {
                addr: bound.clone(),
                loopback_only,
            });
        }
        let console = self.dispatcher.is_some();
        println!(
            "ui_kit web adapter listening on http://{bound}/  (panel) and /api/snapshot (JSON)"
        );
        // #185: a non-loopback bind is an explicit operator choice, but it is the
        // difference between "this panel is mine" and "anyone on the network can
        // reach the process controls", so it is never allowed to be quiet.
        if !loopback_only {
            println!("{}", self.bind_warning().unwrap_or_default());
        }
        if console {
            println!(
                "  gateway: /api/command (GET ?cmd=… or POST body){}",
                if self.lifecycle_enabled() {
                    " · lifecycle ENABLED"
                } else {
                    " · read-only (pass --manage for start/stop)"
                }
            );
        }
        for stream in listener.incoming() {
            match stream {
                Ok(s) => self.handle(s),
                Err(e) => eprintln!("accept: {e}"),
            }
        }
        Ok(())
    }

    fn lifecycle_enabled(&self) -> bool {
        self.dispatcher
            .as_ref()
            .map(|d| d.lock().map(|d| d.lifecycle_enabled()).unwrap_or(false))
            .unwrap_or(false)
    }

    /// The loud warning shown when this panel is listening beyond loopback, or
    /// `None` while it is local-only.
    ///
    /// Plaintext HTTP is the load-bearing part: the panel password crosses the
    /// wire unencrypted and the panel can start and stop the trading kernel, so
    /// "reachable from the network" and "safe to reach from the network" are not
    /// the same claim. The fix for an operator who needs remote access is a
    /// tunnel (`ssh -L`), a VPN, or a TLS-terminating reverse proxy that is
    /// itself authenticated — never exposing this port directly.
    pub fn bind_warning(&self) -> Option<String> {
        let bind = self.bind.lock().ok().and_then(|b| b.clone())?;
        if bind.loopback_only {
            return None;
        }
        Some(format!(
            "\n\
             ======================================================================\n\
             ⚠  安全提示：面板监听在 {}（非本地地址）\n\
                 1) 面板是明文 HTTP，没有 TLS：登录凭据以明文在网络中传输。\n\
                 2) 面板可以启动/停止交易内核，等于把进程控制权暴露给网络。\n\
                 3) 请勿直接暴露该端口；只放在已认证的隧道/VPN 或带 TLS 的\n\
                    反向代理之后，并把可信来源写入 BLITZKRIEG_ALLOWED_ORIGINS。\n\
                 如需回到仅本机可访问：把 .env 里的 BLITZKRIEG_PANEL_ADDR 设为\n\
                 127.0.0.1:51888（默认值），或启动时传 --addr 127.0.0.1:51888。\n\
                 登录失败已限速：同一来源连续 {} 次失败后锁定（指数退避，最长 {} 分钟）。\n\
             ======================================================================\n",
            bind.addr,
            MAX_LOGIN_FAILURES,
            LOGIN_LOCKOUT_MAX_MS / 60_000
        ))
    }

    /// The address this panel actually bound, once `serve()` has it, as
    /// `host:port`. `None` before the listener exists.
    ///
    /// Exists so a caller that needs the port (tests, an embedder that passed
    /// `:0`) can read it from the listener instead of probing for a free port
    /// and racing whoever takes it in between.
    pub fn bound_addr(&self) -> Option<String> {
        self.bind
            .lock()
            .ok()
            .and_then(|b| b.as_ref().map(|i| i.addr.clone()))
    }

    /// The `security` block of the snapshot document: what a panel or an
    /// external monitor needs to see the two exposure facts without reading logs.
    fn security_doc(&self) -> serde_json::Value {
        let bind = self.bind.lock().ok().and_then(|b| b.clone());
        let (tracked, locked) = self.login_throttle_summary();
        serde_json::json!({
            // Absent before `serve()` binds: assume the safe answer is unknown,
            // but do not claim exposure we cannot demonstrate either.
            "loopbackOnly": bind.as_ref().map(|b| b.loopback_only).unwrap_or(true),
            "bind": bind.as_ref().map(|b| b.addr.clone()),
            "warning": self.bind_warning(),
            "login": {
                "maxFailures": MAX_LOGIN_FAILURES,
                "lockoutBaseSec": LOGIN_LOCKOUT_BASE_MS / 1_000,
                "lockoutMaxSec": LOGIN_LOCKOUT_MAX_MS / 1_000,
                "trackedClients": tracked,
                "lockedClients": locked,
            },
        })
    }

    /// Process-control state for the panel. `None` when this server has no
    /// dispatcher at all (pure read-only adapter).
    ///
    /// Calling this also *observes* the core (E12-c): a panel refresh is the
    /// natural heartbeat, and without one a crashed core would keep being
    /// reported as running because nothing ever asked. With the default restart
    /// policy off this only records the crash; a gateway that enabled a policy
    /// gets a replacement as a side effect of being looked at.
    fn lifecycle_view(&self) -> Option<LifecycleView> {
        let mut d = self.dispatcher.as_ref()?.lock().ok()?;
        d.pump();
        let health = d.health();
        Some(LifecycleView {
            enabled: d.lifecycle_enabled(),
            managed: health.managed,
            pid: health.pid,
            socket: d.socket_path().to_string(),
            restarts: health.restarts,
            restart_given_up: health.restart_given_up,
            last_exit: health.last_exit.as_ref().map(LifecycleExit::from),
            give_up: health.alert.as_ref().map(LifecycleGiveUp::from),
        })
    }

    fn handle(&self, mut stream: TcpStream) {
        // A client that connects and stalls must not wedge the accept loop.
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
        let Some(req) = read_request(&mut stream) else {
            return;
        };
        let target = req.path().to_string();

        // E6-a: auth + origin gate runs BEFORE any route does work.
        let status_gate = self.authorize(&req);
        if status_gate != 200 {
            let reason = if status_gate == 401 {
                "Unauthorized"
            } else {
                "Forbidden"
            };
            let head = format!(
                "HTTP/1.1 {status_gate} {reason}\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
            return;
        }

        // Set by a route that wants a header the plain (status, ctype, body)
        // tuple cannot carry — currently only `Retry-After` on a login lockout.
        let mut retry_after: Option<u64> = None;
        let (status, ctype, body): (u16, &'static str, Vec<u8>) = match (
            req.method.as_str(),
            target.as_str(),
        ) {
            ("GET", "/api/ping") | ("HEAD", "/api/ping") => {
                // Unauthenticated by design; see `ping_doc`.
                (200, "application/json", self.ping_doc().into_bytes())
            }
            ("GET", "/api/plugins") => {
                // E9-g: the registry trio the TUI Plugins page shows (strategies
                // / extensions / market plugins) in one authenticated call.
                let doc = match self.dispatcher {
                    Some(ref d) => match d.lock() {
                        Ok(mut dp) => {
                            let snap = dp.plugins_snapshot();
                            serde_json::json!({
                                "connected": snap.connected,
                                "strategies": snap.strategies,
                                "extensions": snap.extensions,
                                "marketPlugins": snap.market_plugins,
                                "marketActive": snap.market_active,
                                "lastError": snap.last_error,
                            })
                        }
                        Err(_) => serde_json::json!({"connected": false,
                            "lastError": "dispatcher poisoned"}),
                    },
                    None => {
                        // read-only mode: registry via the snapshot client
                        let mut doc = serde_json::json!({"connected": false});
                        doc["lastError"] =
                            serde_json::json!("plugin registry requires gateway (--manage)");
                        doc
                    }
                };
                (200, "application/json", doc.to_string().into_bytes())
            }
            ("GET", "/api/snapshot") => {
                let snap = self.snapshot();
                let lifecycle = self.lifecycle_view();
                (
                    200,
                    "application/json",
                    render_json_full(&snap, lifecycle.as_ref(), Some(self.security_doc()))
                        .into_bytes(),
                )
            }
            ("GET", "/api/command") | ("POST", "/api/command") => {
                let cmd = if req.method == "POST" {
                    body_to_command(&req.body)
                } else {
                    req.query_cmd().unwrap_or_default()
                };
                (200, "application/json", self.run_command(&cmd).into_bytes())
            }
            ("POST", "/api/login") => {
                // body: {"user":"…","password":"…"} → {"ok":true,"token":…,
                // "user":…} so the WebUI can store it as a session.
                //
                // #185: every failure is counted against the CLIENT ADDRESS, and a
                // client that has failed enough is refused outright. The refusal
                // happens BEFORE the credentials are examined, which is what keeps
                // the lockout from leaking whether a guessed password was right:
                // the answer is the same for every input while locked.
                let key = Self::login_client_key(&stream);
                if let Some(secs) = self.login_lockout_remaining(&key) {
                    retry_after = Some(secs);
                    eprintln!(
                        "panel: login refused — {key} is locked out for another {secs}s \
                         ({MAX_LOGIN_FAILURES} failures reached)"
                    );
                    let doc = serde_json::json!({
                        "ok": false,
                        "locked": true,
                        "retryAfterSec": secs,
                        "error": format!(
                            "登录失败次数过多，已暂时锁定，请在 {secs} 秒后重试"
                        ),
                    });
                    (
                        429,
                        "application/json; charset=utf-8",
                        serde_json::to_string(&doc).unwrap_or_default().into_bytes(),
                    )
                } else {
                    let doc = match parse_login_body(&req.body) {
                        Some((u, p)) => match self.login(&u, &p) {
                            Some(tok) => {
                                self.note_login_success(&key);
                                serde_json::json!({
                                    "ok": true, "token": tok, "user": u,
                                })
                            }
                            None => {
                                let failures = self.note_login_failure(&key);
                                // Loud, and deliberately without the submitted
                                // username or password: the log records that an
                                // attempt failed and how close the client is to
                                // the lockout, never what was tried.
                                eprintln!(
                                    "panel: failed login from {key} \
                                     ({failures}/{MAX_LOGIN_FAILURES}){}",
                                    if failures >= MAX_LOGIN_FAILURES {
                                        " — client locked out"
                                    } else {
                                        ""
                                    }
                                );
                                std::thread::sleep(std::time::Duration::from_millis(
                                    login_failure_delay_ms(failures),
                                ));
                                serde_json::json!({
                                    "ok": false, "error": "用户名或密码错误",
                                })
                            }
                        },
                        None => serde_json::json!({
                            "ok": false, "error": "请求格式错误（需要 JSON {user, password}）",
                        }),
                    };
                    let status = if doc["ok"] == serde_json::json!(true) {
                        200
                    } else {
                        401
                    };
                    (
                        status,
                        "application/json; charset=utf-8",
                        serde_json::to_string(&doc).unwrap_or_default().into_bytes(),
                    )
                }
            }
            ("GET", "/api/logout") | ("POST", "/api/logout") => {
                // Session token via any channel; revoke it. Harmless if absent.
                let token = self
                    .token_from_query(&req)
                    .or_else(|| self.token_from_cookies(&req))
                    .or_else(|| req.header("x-auth-token").map(String::from))
                    .unwrap_or_default();
                self.logout(&token);
                (200, "application/json", b"{\"ok\":true}".to_vec())
            }
            ("GET", "/") | ("GET", "/panel") | ("GET", "/panel/") => {
                // /panel is the canonical entry: the E9-g Vue app when built
                // (ui/webapp/webui/dist), else the built-in HTML panel. `/`
                // redirects so human-typed origins always land in the same place.
                if target != "/panel" && target != "/panel/" {
                    let head = "HTTP/1.1 302 Found\r\nLocation: /panel\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.flush();
                    return;
                }
                match serve_vue_panel() {
                    Some((body, ctype)) => (200, ctype, body),
                    None => {
                        let snap = self.snapshot();
                        let lifecycle = self.lifecycle_view();
                        (
                            200,
                            "text/html; charset=utf-8",
                            render_html_full(
                                &snap,
                                self.dispatcher.is_some(),
                                lifecycle.as_ref(),
                                self.bind_warning().as_deref(),
                            )
                            .into_bytes(),
                        )
                    }
                }
            }
            ("GET", path) if path.starts_with("/panel/") => {
                // Vue app assets (`/panel/assets/*.js|css`, favicon, …).
                match serve_vue_panel_asset(path) {
                    Some((body, ctype)) => (200, ctype, body),
                    None => (404, "text/plain; charset=utf-8", b"not found".to_vec()),
                }
            }
            ("GET", _) => {
                let snap = self.snapshot();
                let lifecycle = self.lifecycle_view();
                (
                    200,
                    "text/html; charset=utf-8",
                    render_html_full(
                        &snap,
                        self.dispatcher.is_some(),
                        lifecycle.as_ref(),
                        self.bind_warning().as_deref(),
                    )
                    .into_bytes(),
                )
            }
            _ => (404, "text/plain; charset=utf-8", b"not found".to_vec()),
        };

        if std::env::var("UIKIT_WEB_TRACE").is_ok() {
            eprintln!(
                "ui_kit web: {} {} -> {} bytes ({status})",
                req.method,
                target,
                body.len()
            );
        }
        let reason = match status {
            200 => "OK",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            429 => "Too Many Requests",
            _ => "Not Found",
        };
        let extra = retry_after
            .map(|s| format!("Retry-After: {s}\r\n"))
            .unwrap_or_default();
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n{extra}Connection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
        let _ = stream.flush();
    }

    fn snapshot(&self) -> UiSnapshot {
        match self.snapshot_src.lock() {
            Ok(mut c) => c.snapshot(self.trade_limit),
            Err(_) => UiSnapshot::default(),
        }
    }

    fn run_command(&self, cmd: &str) -> String {
        let Some(d) = &self.dispatcher else {
            return serde_json::json!({
                "ok": false, "action": "error",
                "message": "command API disabled; run ui_kit_web with --manage"
            })
            .to_string();
        };
        match d.lock() {
            Ok(mut d) => serde_json::to_string(&d.dispatch_line(cmd)).unwrap_or_else(|e| {
                format!("{{\"ok\":false,\"action\":\"error\",\"message\":\"serialize: {e}\"}}")
            }),
            Err(_) => "{\"ok\":false,\"action\":\"error\",\"message\":\"dispatcher poisoned\"}"
                .to_string(),
        }
    }
}

/// Parse one HTTP/1.1 request (request line + headers + optional body).
fn read_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut content_length = 0usize;
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim();
        if h.is_empty() {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if let Some((name, value)) = h.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let body = if content_length > 0 {
        let mut buf = vec![0u8; content_length.min(64 * 1024)];
        let mut read = 0;
        while read < buf.len() {
            match reader.read(&mut buf[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(_) => break,
            }
        }
        buf.truncate(read);
        String::from_utf8_lossy(&buf).into_owned()
    } else {
        String::new()
    };
    Some(HttpRequest {
        method,
        target,
        body,
        headers,
    })
}

/// POST bodies may be raw command text or `{"cmd":"..."}`.
/// Parse a login request body: JSON `{"user": “…”, “password”: “…”}` (or the
/// url-encoded `user=…&password=…` form). Returns None on malformed input.
fn parse_login_body(body: &str) -> Option<(String, String)> {
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
        let user = v.get("user")?.as_str()?.to_string();
        let password = v.get("password")?.as_str()?.to_string();
        Some((user, password))
    } else {
        let mut user = None;
        let mut password = None;
        for pair in trimmed.split('&') {
            if let Some(v) = pair.strip_prefix("user=") {
                user = Some(url_decode(v));
            } else if let Some(v) = pair.strip_prefix("password=") {
                password = Some(url_decode(v));
            }
        }
        Some((user?, password?))
    }
}

fn body_to_command(body: &str) -> String {
    let t = body.trim();
    if t.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            if let Some(c) = v.get("cmd").and_then(|c| c.as_str()) {
                return c.to_string();
            }
        }
    }
    t.to_string()
}

#[cfg(test)]
mod auth_tests;

#[cfg(test)]
mod evolution_tests;

#[cfg(test)]
mod safety_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_cmd_decodes() {
        let r = HttpRequest {
            method: "GET".into(),
            target: "/api/command?cmd=start+BTC,ETH".into(),
            body: String::new(),
            headers: Vec::new(),
        };
        assert_eq!(r.path(), "/api/command");
        assert_eq!(r.query_cmd().as_deref(), Some("start BTC,ETH"));
    }

    #[test]
    fn body_json_or_raw() {
        assert_eq!(body_to_command("{\"cmd\":\"status\"}"), "status");
        assert_eq!(body_to_command("  stop  "), "stop");
    }

    #[test]
    fn resolve_panel_dir_picks_first_existing_index_html() {
        let base = std::env::temp_dir().join(format!("bk-panel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let a = base.join("a/dist/index.html");
        let b = base.join("b/dist/index.html");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::write(&a, b"<html></html>").unwrap();
        std::fs::write(&b, b"<html></html>").unwrap();
        let missing = base.join("none/dist/index.html");
        // resolve_panel_dir canonicalizes, so expectations canonicalize too
        // (on macOS /tmp is a symlink to /private/tmp).
        assert_eq!(
            resolve_panel_dir(&[a.clone(), b.clone()]),
            Some(a.canonicalize().unwrap().parent().unwrap().to_path_buf())
        );
        assert_eq!(
            resolve_panel_dir(&[missing.clone(), b.clone()]),
            Some(b.canonicalize().unwrap().parent().unwrap().to_path_buf())
        );
        assert_eq!(resolve_panel_dir(&[missing]), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn render_html_escaping_and_console() {
        let snap = UiSnapshot::default();
        assert!(render_html(&snap).contains("Blitzkrieg UI Kit"));
        assert!(!render_html(&snap).contains("Command console"));
        assert!(render_html_with(&snap, true).contains("Command console"));
    }

    /// The panel's 启动/停止 buttons are gated on this block, so absent
    /// information must read as "cannot", never as "can".
    #[test]
    fn render_json_reports_gateway_capabilities() {
        let snap = UiSnapshot::default();
        // No dispatcher (read-only adapter): no `gateway` block at all, so a
        // consumer that defaults a missing block to `false` stays safe.
        let doc: serde_json::Value = serde_json::from_str(&render_json(&snap, None)).unwrap();
        assert!(doc.get("gateway").is_none());

        // Adopted core with `--manage`: the verbs are accepted, but stop cannot
        // act on a core this gateway did not spawn. The two flags must be
        // reported independently or the panel cannot tell them apart.
        let adopted = LifecycleView {
            enabled: true,
            managed: false,
            pid: None,
            socket: "/tmp/x.sock".into(),
            restarts: 0,
            restart_given_up: false,
            last_exit: None,
            give_up: None,
        };
        let doc: serde_json::Value =
            serde_json::from_str(&render_json(&snap, Some(&adopted))).unwrap();
        assert_eq!(doc["gateway"]["lifecycleEnabled"], serde_json::json!(true));
        assert_eq!(doc["gateway"]["managed"], serde_json::json!(false));
        assert_eq!(doc["gateway"]["corePid"], serde_json::Value::Null);
        assert_eq!(doc["gateway"]["socket"], serde_json::json!("/tmp/x.sock"));

        // Spawned core: both true, and the PID is on the wire.
        let owned = LifecycleView {
            enabled: true,
            managed: true,
            pid: Some(4242),
            socket: "/tmp/x.sock".into(),
            restarts: 2,
            restart_given_up: false,
            last_exit: Some(LifecycleExit {
                pid: 111,
                kind: "crash",
                code: None,
                signal: Some(9),
                description: "core pid 111 CRASHED (killed by signal 9)".into(),
            }),
            give_up: None,
        };
        let doc: serde_json::Value =
            serde_json::from_str(&render_json(&snap, Some(&owned))).unwrap();
        assert_eq!(doc["gateway"]["managed"], serde_json::json!(true));
        assert_eq!(doc["gateway"]["corePid"], serde_json::json!(4242));
        // E12-c: the crash must reach the panel as structured fact, not as a
        // missing pid the UI has to interpret.
        assert_eq!(doc["gateway"]["restarts"], serde_json::json!(2));
        assert_eq!(
            doc["gateway"]["lastExit"]["kind"],
            serde_json::json!("crash")
        );
        assert_eq!(doc["gateway"]["lastExit"]["signal"], serde_json::json!(9));
        assert!(doc["gateway"]["lastExit"]["description"]
            .as_str()
            .unwrap()
            .contains("CRASHED"));

        // Read-only gateway: `enabled` false even though a dispatcher exists.
        let readonly = LifecycleView {
            enabled: false,
            managed: false,
            pid: None,
            socket: "/tmp/x.sock".into(),
            restarts: 0,
            restart_given_up: false,
            last_exit: None,
            give_up: None,
        };
        let doc: serde_json::Value =
            serde_json::from_str(&render_json(&snap, Some(&readonly))).unwrap();
        assert_eq!(doc["gateway"]["lifecycleEnabled"], serde_json::json!(false));
    }

    /// #186: a supervisor that has given up must be impossible to miss.
    ///
    /// `restartGivenUp` alone tells the panel *that* the kernel is gone; the
    /// alert carries *why*, which is what an operator needs to decide what to do
    /// next. Both the JSON and the built-in HTML panel are checked here because
    /// the Vue panel reads the former and the fallback panel renders the latter.
    #[test]
    fn a_give_up_alert_reaches_the_panel_with_its_reason() {
        let snap = UiSnapshot::default();
        let give_up = LifecycleView {
            enabled: true,
            managed: true,
            pid: None,
            socket: "/tmp/x.sock".into(),
            restarts: 5,
            restart_given_up: true,
            last_exit: Some(LifecycleExit {
                pid: 111,
                kind: "crash",
                code: Some(3),
                signal: None,
                description: "core pid 111 CRASHED (exit code 3)".into(),
            }),
            give_up: Some(LifecycleGiveUp {
                attempts: 5,
                at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
                message: "内核连崩 6 次后已放弃重启（预算 5 次已用尽），进程不会自行恢复".into(),
                stderr_tail: vec!["thread 'main' panicked: no market plugin".into()],
            }),
        };

        let doc: serde_json::Value =
            serde_json::from_str(&render_json(&snap, Some(&give_up))).unwrap();
        assert_eq!(doc["gateway"]["restartGivenUp"], serde_json::json!(true));
        let alert = &doc["gateway"]["giveUp"];
        assert_eq!(alert["attempts"], serde_json::json!(5));
        assert!(
            alert["message"]
                .as_str()
                .unwrap_or_default()
                .contains("放弃重启"),
            "the panel must receive the reason, not only the boolean: {alert}"
        );
        assert_eq!(
            alert["stderrTail"][0],
            serde_json::json!("thread 'main' panicked: no market plugin"),
            "the core's own last words must survive to the panel"
        );

        let html = render_html_full(&snap, true, Some(&give_up), None);
        assert!(
            html.contains("内核已停机：已放弃重启"),
            "the built-in panel must show the give-up state"
        );
        assert!(
            html.contains("no market plugin"),
            "and the stderr tail that explains it"
        );
        assert!(html.contains("重启尝试 5 次"));

        // The healthy case renders no banner at all: a notice that is always on
        // is a notice nobody reads.
        let healthy = LifecycleView {
            give_up: None,
            restart_given_up: false,
            ..give_up
        };
        let html = render_html_full(&snap, true, Some(&healthy), None);
        assert!(!html.contains("已放弃重启"), "no alert → no banner");
    }
}
