//! The order executor: the market-plugin component that turns the core's
//! locally-accepted orders into real PolySimulator orders and reconciles the
//! venue's own state back (#454).
//!
//! The venue has no private push channel the extension consumes (its user WS
//! needs JWT rotation and the free tier allows one connection), so the
//! executor is a REST polling actor on the same shape as the kalshi
//! executor: one tokio task owns a credentialed [`PolySimRest`], drains the
//! host's pending orders / cancels every tick, and runs a periodic
//! reconciliation sweep (open orders + trades + orphan cleanup + free-balance
//! sync). Everything the core sees crosses the `MarketHost` seam; all
//! PolySimulator knowledge stays here.
//!
//! Unit conventions at the boundary:
//!   * price — the core speaks 0..1 probabilities; this venue quotes dollars
//!     0..1 too, so prices pass through. An off-tick limit is answered with
//!     `INVALID_ORDER_MIN_TICK_SIZE` rather than silently reinterpreted.
//!   * order form — the venue's NATIVE place is the `{"order":{tokenId,
//!     side, makerAmount, takerAmount}}` envelope with 1e6-scaled integer
//!     strings (price = maker/taker for BUY, inverted for SELL); outcome,
//!     quantity and time-in-force do not exist as fields — the venue derives
//!     them from the token and the amounts (verified live 2026-10-25: the
//!     flat form is answered `VALIDATION_FAILED: missing order`).
//!   * fills — `GET /v1/data/trades` reports PM-shape rows whose legs split
//!     across `taker_order_id` (the marketable side) and `maker_orders` (the
//!     resting ids consumed). OUR fill books under whichever of the two names
//!     our venue order id; the venue order id is an INTEGER everywhere,
//!     stringified at this seam.
//!   * fill realism (#876) — the venue answers a marketable order with
//!     `status:"matched"` and lists it FILLED instantly: no queue, no
//!     round-trip, and its trade rows report `fee_rate_bps: 0` (free
//!     trading). The adapter does not fold that optimism into the ledger:
//!     a just-placed order stays non-terminal (reported OPEN) for one sweep
//!     cycle and its fill confirms at the NEXT sweep — the existing sweep
//!     cadence models the queue the venue lacks, no new timer — and the fee
//!     a fill carries follows the official crypto schedule through its
//!     maker/taker role: taker `shares · 0.07 · p · (1−p)` USD, maker 0
//!     (`TAKER_FEE_RATE` / `official_taker_fee_usd`). An order whose ack
//!     said `live` (rested) books its sweep-confirmed fill as MAKER; only a
//!     `matched` ack (we crossed) books taker.
//!   * ledger seed (#877 → #909 follow-up) — the figure the executor seeds
//!     and re-aligns the kernel ledger with is the venue's reported CASH.
//!     The #877 replay seeded the account EQUITY (`total_value` = cash +
//!     open positions) so the replay equity would match the website's —
//!     but the kernel channels it feeds (`seed_balance` → the ledger's
//!     spendable `balance`; `venue_free_balance` → `fold_venue_free`)
//!     are CASH books: an equity seed over-approved the reserve gate and
//!     an equity re-alignment folded the position value back in as
//!     phantom cash on every no-order sweep (independent post-merge
//!     acceptance, PR #909). The equity stays read-only telemetry: the
//!     ready/sweep log lines still name cash + positions = equity in the
//!     website's own identity. A report without `total_value`, or with
//!     one below cash (impossible while a long position holds value),
//!     still refuses the field loudly (see `ledger_equity`).

use crate::into_core_error;
use crate::rest::{Credentials, PolySimRest, RestConfig};
use crate::settlement::{resolution_by_single_market, resolution_for};
use crate::types::{BalanceBody, PlaceOrderBody, UpDownRow};
use blitzkrieg_bk_contract::{
    CoreError, CoreErrorCode, CoreResult, ExecutorConfig, FillStatus, MarketFill, MarketHost,
    PendingOrder, ReconcileSnapshot, SelfCheckItem, SelfCheckReport, Side, VenueTradeInfo,
    net::now_ms,
};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{MissedTickBehavior, interval};

/// Executor tick: pending-order/cancel drain cadence. The venue is REST and
/// per-user serializes order writes; a faster tick buys nothing.
const EXECUTOR_TICK_MS: u64 = 500;
/// Reconciliation sweep every 10 ticks (~5s), the sibling executors' cadence.
const SWEEP_EVERY_TICKS: u64 = 10;
/// Consecutive sweep failures before the host freezes trading (E31-b: the
/// safety net being blind is worse than it being slow).
const SWEEP_FAILURE_FREEZE: u32 = 3;
/// A venue rejection streak that triggers a direct capability probe instead
/// of letting the freeze rest on inference.
const PLACE_FAILURE_PROBE: u32 = 3;

/// The fill-confirmation window (#876): one sweep period. A placement whose
/// fill the sweep observes INSIDE this window does not book yet — the order
/// stays non-terminal (reported OPEN) and the fill confirms at the NEXT
/// sweep. The venue matches marketable orders instantly (`status:"matched"`,
/// FILLED within the same tick); booking that at the same tick's sweep would
/// fold the venue's missing queue straight into the ledger. The existing
/// sweep cadence models the delay; deliberately no new timer.
const FILL_CONFIRM_MS: i64 = (EXECUTOR_TICK_MS * SWEEP_EVERY_TICKS) as i64;
/// How long a placement record outlives its confirmation window. The record
/// carries the fill's role (maker/taker read off the place ack), so it must
/// be there whenever the venue's FILLED list surfaces the order — bounded,
/// not forever: past this horizon an unknown-provenance fill books with
/// `maker: None` and the core falls back to the order's own fill policy.
const PLACEMENT_TTL_MS: i64 = 600_000;
/// The official crypto taker fee rate — `fee = shares · 0.07 · p · (1−p)`
/// USD, maker side 0 (Polymarket's published Crypto schedule; the same curve
/// the kernel pins as `official`). THE fee source of this adapter, in one
/// place: the venue's own trade rows report `fee_rate_bps: 0` (live
/// 2026-10-26) and that wire field is parsed but NEVER booked (#876).
/// `from_parts` because `Decimal::new` is not `const`: 7 at scale 2 = 0.07.
const TAKER_FEE_RATE: Decimal = Decimal::from_parts(7, 0, 0, false, 2);

/// One placement the executor submitted this process: the venue id plus the
/// place ack's own fill verdict. The verdict decides the fee semantics of
/// the fill the sweep later confirms — the official schedule's maker/taker
/// split is decided at PLACE time on this venue, whose FILLED list carries
/// no role at all.
#[derive(Debug, Clone)]
struct Placement {
    /// `Some(true)` = the ack said `matched` (we crossed, taker);
    /// `Some(false)` = the ack said `live` (rested, maker when hit);
    /// `None` = the ack carried no readable status — the role stays with the
    /// core's fill-policy fallback.
    crossed: Option<bool>,
    placed_ms: i64,
}

/// Whether `p` is still inside its fill-confirmation window at `now_ms`:
/// less than one sweep period since the place ack. Inside the window the
/// order reports OPEN and its fill (whichever channel carries it first)
/// waits for the next sweep.
fn in_confirm_window(p: &Placement, now_ms: i64) -> bool {
    now_ms - p.placed_ms < FILL_CONFIRM_MS
}

/// The official crypto taker fee in USD: `shares · 0.07 · p · (1−p)`.
/// A price outside `(0, 1)` (and a non-positive size) charges nothing — the
/// same bounds the kernel's own schedule guards, so a fee can never come out
/// negative (a rebate) on a mis-shaped row.
fn official_taker_fee_usd(price: Decimal, shares: Decimal) -> Decimal {
    if price <= Decimal::ZERO || price >= Decimal::ONE || shares <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    shares * TAKER_FEE_RATE * price * (Decimal::ONE - price)
}

/// The place ack's own fill verdict: `matched` = the venue crossed the order
/// at place time (taker), `live` = it rested (maker when the sweep confirms
/// its fill), anything else = the ack does not say (`None`).
fn ack_crossed(status: Option<&str>) -> Option<bool> {
    match status {
        Some("matched") => Some(true),
        Some("live") => Some(false),
        _ => None,
    }
}

/// The fill's maker flag from the placement's ack verdict — the official fee
/// semantics of this adapter: an order that RESTED books its sweep-confirmed
/// fill as maker (official maker fee = 0); only an order the ack reported
/// `matched` (we crossed) books taker. `None` (no readable ack, or an order
/// this process never placed — restart recovery) leaves the role to the
/// core's fill-policy fallback, which is what `MarketFill::maker = None`
/// means at the seam.
fn fill_role(crossed: Option<bool>) -> Option<bool> {
    crossed.map(|c| !c)
}

/// The figure the ledger is seeded with at boot and re-aligned with on every
/// sweep (#877): the venue's own account EQUITY — available cash plus the
/// market value of open positions, exactly the account-equity view the
/// website displays — when the balance report carries `total_value`, and the
/// cash floor (loudly degraded) when it does not.
///
/// Why not `balance` alone: seeding the ledger with bare cash left the
/// replay equity systematically short by the open-position value — the #877
/// identity (cash $111.62 + positions $20.00 = equity $131.62; 20 of the
/// +31.6% points were position valuation) never closed on the kernel side.
/// Why `unrealized_pnl` never enters the arithmetic: it is quoted against
/// the ENTRY cost (equity = cash + cost + unrealized) and the cost basis is
/// not on the balance body — adding it to cash would double-count once the
/// position closes. It rides the evidence line instead. Why not
/// `GET /v1/account/positions`: [`crate::types::PositionRow`] carries only
/// `avg_entry_price`, so a sum of its rows prices open positions at COST — a
/// different scope than the website's market-value equity, for a second wire
/// dependency; `total_value` IS the website's own number.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LedgerEquity {
    /// The wire's `total_value`: the venue's own equity view (cash + open
    /// positions). `unrealized` is the wire's `unrealized_pnl`, evidence
    /// only.
    VenueReported {
        equity: Decimal,
        cash: Decimal,
        unrealized: Option<Decimal>,
    },
    /// No usable `total_value` (an older website build may omit the field,
    /// or report one below cash — impossible while a long position holds
    /// value, so such a field is refused): the ledger falls back to cash
    /// only, and the caller SAYS SO — a silent cash-only read is exactly the
    /// #877 blind spot this type exists to close. Fail-closed floor: cash
    /// understates equity by at most the open-position value; it never
    /// overstates the money the venue can actually spend.
    CashFloor { cash: Decimal, reason: &'static str },
}

impl LedgerEquity {
    /// The number that goes into the LEDGER — the spendable CASH
    /// (post-#909-follow-up: the equity figure over-approved the reserve gate
    /// and double-counted the open position through `fold_venue_free`; the
    /// equity stays on the evidence line instead).
    fn figure(&self) -> Decimal {
        match self {
            Self::VenueReported { cash, .. } => *cash,
            Self::CashFloor { cash, .. } => *cash,
        }
    }

    /// The degradation reason when this figure is the cash floor — the
    /// caller logs it, at the boot seed and on every sweep: a degraded read
    /// must be visible, never silent.
    fn degraded(&self) -> Option<&'static str> {
        match self {
            Self::VenueReported { .. } => None,
            Self::CashFloor { reason, .. } => Some(reason),
        }
    }
}

impl std::fmt::Display for LedgerEquity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The website's identity on one line: cash + positions = equity
            // (the #877 screenshot: 111.62 + 20.00 = 131.62).
            Self::VenueReported {
                equity,
                cash,
                unrealized,
            } => {
                let positions = *equity - *cash;
                let un = match unrealized {
                    Some(u) => u.to_string(),
                    None => String::from("n/a"),
                };
                write!(
                    f,
                    "cash={cash} positions={positions} unrealized={un} equity={equity}"
                )
            }
            Self::CashFloor { cash, reason } => write!(
                f,
                "cash={cash} equity={cash} (DEGRADED: {reason} — open positions count 0 toward the ledger; issue #877)"
            ),
        }
    }
}

/// The ledger figure off one balance report, post-#909-follow-up. Reality
/// discovered by the independent post-merge acceptance (PR #909 comment
/// 6597853873): the kernel's cash channels this figure feeds are SPENDABLE
/// cash — `seed_balance` seeds the ledger's `balance` the reserve gate
/// (`available() = balance − reserved`) spends, and `venue_free_balance`
/// folds into the same book through `fold_venue_free`, which also reconciles
/// the venue's post-settlement cash balance (`orders.rs:
/// settle_buy_fill` debits cash while the position book books the position
/// at cost). Feeding the EQUITY there let the kernel over-approve up to the
/// open-position value (+17.9% on #877's screenshot: approve $131.62 while
/// only $111.62 is real cash) and double-counted the position every
/// no-order sweep (equity moved as the price moved → `fold_venue_free` read
/// it as cash that appeared → `report_venue_cash_move` flagged a phantom
/// injection the recon's `ledger_realigned` exemption then absorbed,
/// keeping `trade_ledger_identity` green — fail-open).
///
/// So the ledger now takes the CASH figure, and the website's equity stays
/// what it always was to the kernel: read-only telemetry (the evidence line
/// names cash + positions + equity on one line, exactly the website's
/// identity). The #877 replay-equity view is a REPORT concern (the dry-run
/// report prices open positions from the position book), not a ledger-cash
/// truth — an equity-in-cash-channel seed was the bug, not the fix.
///
/// `total_value` is still parsed and audit-checked (equity < cash is
/// IMPOSSIBLE while a long position holds value → the field is inconsistent
/// → refuse, keep the loud floor log at boot/every sweep) — but its figure
/// no longer enters the ledger.
///
/// Pure so the mapping is testable without a client.
fn ledger_equity(b: &BalanceBody) -> LedgerEquity {
    match b.total_value {
        Some(equity) if equity >= b.balance => LedgerEquity::VenueReported {
            equity,
            cash: b.balance,
            unrealized: b.unrealized_pnl,
        },
        Some(_) => LedgerEquity::CashFloor {
            cash: b.balance,
            reason: "total_value below available cash (impossible while a long position holds value) — field refused",
        },
        None => LedgerEquity::CashFloor {
            cash: b.balance,
            reason: "no total_value on the balance report (older website build?)",
        },
    }
}

/// Build the credentialed client, or `None` when the environment carries no
/// API key — the executor is then INERT (dry runs). Same contract as the
/// sibling executors' `spawn_if_configured`.
fn credentials_from_env() -> Option<Credentials> {
    let api_key = std::env::var("POLYSIM_API_KEY").ok()?;
    if api_key.is_empty() {
        return None;
    }
    Some(Credentials { api_key })
}

fn rest_config() -> RestConfig {
    let mut config = RestConfig::default();
    if std::env::var("POLYSIM_NO_PROXY").is_ok_and(|v| v == "1" || v == "true") {
        config.no_proxy = true;
    }
    config
}

/// Spawn the executor. Inert (Ok(None)) without credentials; otherwise a
/// background task runs until the process ends.
pub async fn spawn_if_configured(
    host: Arc<dyn MarketHost>,
    _config: ExecutorConfig,
) -> CoreResult<Option<tokio::task::JoinHandle<()>>> {
    let Some(credentials) = credentials_from_env() else {
        return Ok(None);
    };
    let client = Arc::new(
        PolySimRest::new(rest_config(), Some(credentials)).map_err(|e| {
            CoreError::new(
                CoreErrorCode::VenueError,
                format!("polysimulator client: {e}"),
            )
        })?,
    );
    let handle = tokio::spawn(executor_loop(host, client));
    Ok(Some(handle))
}

/// Test-only entry for the executor integration suite: drive the REAL
/// production loop against a mock server with an explicit client (env-var
/// plumbing stays out of the test's control, and no credential is fixed in
/// source). Not part of the public surface.
#[doc(hidden)]
pub fn executor_tests_only_spawn(host: Arc<dyn MarketHost>, client: Arc<PolySimRest>) {
    tokio::spawn(executor_loop(host, client));
}

/// One capability probe against the paths trading actually uses: balance
/// (auth), open orders and a one-page trades query. A failed item means
/// order egress cannot work either.
/// One capability probe against the paths trading actually uses: balance
/// (auth), open orders and a one-page trades query. A failed item means
/// order egress cannot work either.
async fn self_check(client: &PolySimRest) -> SelfCheckReport {
    // Each probe's failure reason rides in the detail: a freeze whose only
    // text is "trades failed" sent an operator hunting blind (the venue's
    // "Rate limit exceeded. Retry after 1s." is a different story than an
    // auth rejection, and #184 says the reason goes on the line).
    let balance = client.balance().await;
    let orders = client.open_orders().await;
    let trades = client.trades().await;
    let items = vec![
        SelfCheckItem {
            name: "balance".into(),
            ok: balance.is_ok(),
            detail: match balance {
                Ok(_) => "balance endpoint answered".into(),
                Err(e) => format!("balance failed: {e}"),
            },
        },
        SelfCheckItem {
            name: "orders".into(),
            ok: orders.is_ok(),
            detail: match orders {
                Ok(_) => "open orders answered".into(),
                Err(e) => format!("open orders failed: {e}"),
            },
        },
        SelfCheckItem {
            name: "trades".into(),
            ok: trades.is_ok(),
            detail: match trades {
                Ok(_) => "trades answered".into(),
                Err(e) => format!("trades failed: {e}"),
            },
        },
    ];
    SelfCheckReport {
        ok: items.iter().all(|i| i.ok),
        ts_ms: now_ms(),
        items,
    }
}

pub(crate) async fn executor_loop(host: Arc<dyn MarketHost>, client: Arc<PolySimRest>) {
    let mut tick = interval(Duration::from_millis(EXECUTOR_TICK_MS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut since_sweep: u64 = 0;
    let mut sweep_failures: u32 = 0;
    let mut place_failures: u32 = 0;
    // Trade rows already reported to the host (dedup by trade id so a sweep
    // overlap never double-books).
    let mut seen_trades: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Orders this process placed, keyed by venue id: the place ack's fill
    // verdict (maker/taker at place time) + when. Serves the orphan sweep's
    // grace window AND the #876 fill-confirmation window (a just-placed
    // order stays non-terminal for one sweep cycle; its fill confirms at the
    // next sweep, with the role the ack decided).
    let mut placements: std::collections::HashMap<String, Placement> =
        std::collections::HashMap::new();
    const ORPHAN_GRACE_MS: i64 = 30_000;
    // Token id → the venue's own outcome word for it ("Up"/"Down"), watched
    // from every trade row the reconciles sweep sees. This is the POLARITY
    // side of the single-market verdict fallback: that endpoint names the
    // winning outcome word but not the token ids, so the trade row's outcome
    // label is the only honest (venue-sourced) mapping we have.
    let mut outcome_polarity: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    // Seed the ledger from the venue so the reserve gate reflects real
    // money. #877: the seed is the venue's account EQUITY (cash + open
    // positions — the website's own identity), not bare cash: a cash-only
    // seed left the replay equity systematically short by the open-position
    // value. A report without a usable `total_value` degrades LOUDLY to the
    // cash floor — never a silent zero for the positions.
    match client.balance().await {
        Ok(b) => {
            let equity = ledger_equity(&b);
            // `equity`'s Display IS the website's identity (cash + positions =
            // equity); the ledger takes the CASH figure and the view stays
            // telemetry in this one line (#909 follow-up).
            eprintln!(
                "polysimulator-extension: executor ready {equity} (ledger cash={})",
                equity.figure()
            );
            host.seed_balance("polysimulator", equity.figure()).await;
        }
        Err(e) => eprintln!("polysimulator-extension: live balance fetch failed: {e}"),
    }
    let report = self_check(&client).await;
    if !report.ok {
        eprintln!("polysimulator-extension: startup self-check FAILED");
    }
    host.on_self_check("polysimulator", report).await;

    // Startup orphan sweep: anything resting at the venue the core does not
    // know about is cancelled before trading resumes.
    match client.open_orders().await {
        Ok(open) => {
            let known: std::collections::HashSet<String> =
                host.known_venue_order_ids().await.into_iter().collect();
            for o in &open {
                let vid = o.venue_id();
                if known.contains(&vid) {
                    continue;
                }
                if let Err(e) = client.cancel_order(&vid).await {
                    eprintln!("polysimulator-extension: orphan cancel {vid}: {e}");
                } else {
                    host.note_orphan_cancelled(&vid).await;
                }
            }
        }
        Err(e) => eprintln!("polysimulator-extension: startup orphan sweep failed: {e}"),
    }

    loop {
        tick.tick().await;

        // 1) Cancels first (same serialization rule as the sibling executors:
        //    the old order is gone before a replacement is submitted).
        let mut cancel_strikes = 0u32;
        for id in host.take_pending_cancels("polysimulator").await {
            match client.cancel_order(&id).await {
                Ok(_) => host.note_orphan_cancelled(&id).await,
                Err(e) => {
                    cancel_strikes += 1;
                    eprintln!("polysimulator-extension: venue cancel failed for {id}: {e}");
                    host.report_error(into_core_error(&e)).await;
                }
            }
        }

        // 2) Submit locally-accepted orders. `take_pending_orders` routes by
        //    this plugin's venue name — with multiple executors running, each
        //    drains only ITS venue's orders.
        if cancel_strikes == 0 {
            for order in host.take_pending_orders("polysimulator").await {
                match place(&client, &order).await {
                    Ok((venue_id, crossed)) => {
                        place_failures = 0;
                        placements.insert(
                            venue_id.clone(),
                            Placement {
                                crossed,
                                placed_ms: now_ms(),
                            },
                        );
                        host.on_order_accepted(&order.core_order_id, &venue_id)
                            .await;
                    }
                    Err(e) if e.code == CoreErrorCode::Timeout => {
                        place_failures += 1;
                        host.on_place_timeout(&order.core_order_id).await;
                        host.report_error(e).await;
                    }
                    Err(e) => {
                        place_failures += 1;
                        host.on_order_rejected(&order.core_order_id, Some(e.clone()))
                            .await;
                        host.report_error(e).await;
                        if place_failures >= PLACE_FAILURE_PROBE {
                            place_failures = 0;
                            host.on_self_check("polysimulator", self_check(&client).await)
                                .await;
                        }
                    }
                }
            }
        }

        // 3) Periodic reconciliation sweep.
        since_sweep += 1;
        if since_sweep < SWEEP_EVERY_TICKS {
            continue;
        }
        since_sweep = 0;
        match sweep(
            &host,
            &client,
            &mut seen_trades,
            &mut placements,
            &mut outcome_polarity,
            ORPHAN_GRACE_MS,
        )
        .await
        {
            Ok(()) => sweep_failures = 0,
            Err(e) => {
                sweep_failures += 1;
                eprintln!("polysimulator-extension: sweep failed ({sweep_failures}): {e}");
                if sweep_failures >= SWEEP_FAILURE_FREEZE {
                    host.on_reconcile_failed("polysimulator", e).await;
                } else {
                    host.report_error(e).await;
                }
            }
        }

        // 4) Settlements (root cause C): the core asks what happened to every
        //    market it still holds a position in; without an answer a
        //    held-to-expiry position stays a ghost (proceeds 0, expiry past),
        //    the exit ladder keeps re-emitting SELLs for a dead token, the
        //    venue rejects with "Insufficient position", and the rejection
        //    streak freezes the account. The updown list already carries the
        //    verdict: resolved=true with terminal 1/0 outcome prices. One
        //    fetch per sweep cycle, only when there ARE queries; a query the
        //    list does not name or has not resolved yet answers nothing — the
        //    core re-asks on its own cadence, and payouts are never invented.
        //    Full page on purpose: settlement needs the whole list as its
        //    bottom line.
        let queries = host.take_settlement_queries().await;
        if queries.is_empty() {
            continue;
        }
        match client.updown_markets(None, None).await {
            Ok(rows) => {
                for query in &queries {
                    match resolution_for(query, &rows) {
                        Some(resolution) => {
                            eprintln!(
                                "polysimulator-extension: settlement {} payouts={:?}",
                                resolution.condition_id, resolution.payouts
                            );
                            host.on_market_resolution(resolution).await;
                        }
                        // The list carries only rounds ending in the future;
                        // an already-resolved condition rotates out within an
                        // hour, so every query the list does not name falls
                        // to the single-market verdict endpoint — the same
                        // discipline the list path follows: unreadable or
                        // not-yet-terminal answers NOTHING and never fabricates.
                        None => match resolution_by_single_market(
                            query,
                            client.as_ref(),
                            &outcome_polarity,
                        )
                        .await
                        {
                            Ok(Some(resolution)) => {
                                eprintln!(
                                    "polysimulator-extension: settlement (single-market verdict) {} payouts={:?}",
                                    resolution.condition_id, resolution.payouts
                                );
                                host.on_market_resolution(resolution).await;
                            }
                            Ok(None) => {}
                            Err(e) => eprintln!(
                                "polysimulator-extension: settlement single-market query {} failed: {e}",
                                query.condition_id
                            ),
                        },
                    }
                }
            }
            // Loud, but never fatal: a failed fetch must not stop trading,
            // and the core alerts on its own once it has waited too long.
            Err(e) => eprintln!(
                "polysimulator-extension: settlement query ({}) fetch failed: {e}",
                queries.len()
            ),
        }
    }
}

/// The venue's native body for one core order: side/amounts derived per the
/// live contract (price = maker/taker BUY, taker/maker SELL; 1e6 scaling).
/// Split from `place` so the scaling math is testable without a client.
fn place_body_for(side: &Side, price: Decimal, shares: Decimal, token_id: &str) -> PlaceOrderBody {
    let scale = Decimal::from(1_000_000u64);
    let (maker_amount, taker_amount) = match side {
        Side::Buy => (price * shares, shares),
        Side::Sell => (shares, price * shares),
    };
    PlaceOrderBody {
        token_id: token_id.to_string(),
        side: match side {
            Side::Buy => "BUY".to_string(),
            Side::Sell => "SELL".to_string(),
        },
        maker_amount: (maker_amount * scale).trunc().to_string(),
        taker_amount: (taker_amount * scale).trunc().to_string(),
    }
}

/// Submit one order. The venue's native form is (tokenId, side, makerAmount,
/// takerAmount) — price = maker/taker for BUY and taker/maker for SELL,
/// amounts 1e6-scaled integer strings. The core's `core_order_id` cannot ride
/// along usefully: the venue accepts a `client_order_id` field but neither
/// echoes nor dedupes on it (verified live 2026-10-25), so a timeout is
/// reconciled through the orders list, never by blind retry. Outcome,
/// quantity and time-in-force do not exist as fields — the venue derives them
/// from the token and the amounts.
///
/// Returns the venue id PLUS the ack's own fill verdict (`matched` → we
/// crossed → taker; `live` → rested → maker; unreadable → unknown). The
/// verdict is decided ONCE here: the venue's FILLED list carries no role, so
/// this is what the sweep-confirmed fill later books from (#876).
async fn place(client: &PolySimRest, order: &PendingOrder) -> CoreResult<(String, Option<bool>)> {
    if order.size < Decimal::ONE {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            "polysimulator order size rounds to zero",
        ));
    }
    let shares = order.size.trunc();
    let body = place_body_for(&order.side, order.price, shares, &order.token_id);
    let ack = client
        .place_order(&body)
        .await
        .map_err(|e| into_core_error(&e))?;
    Ok((ack.venue_id(), ack_crossed(ack.status.as_deref())))
}

/// Attribute one trade row to our order ids: a row is OURS when our venue id
/// appears as the taker (we crossed) or inside the maker set (someone crossed
/// our resting order). Returns the venue order id that books the fill plus
/// whether it was the maker leg — the fill's maker flag is decided by WHICH
/// leg matched, not by which fields happen to be present.
fn our_leg(
    row: &crate::types::PmTradeRow,
    our_ids: &std::collections::HashSet<String>,
) -> Option<(String, bool)> {
    if let Some(taker) = &row.taker_order_id
        && our_ids.contains(taker)
    {
        return Some((taker.clone(), false));
    }
    if let Some(makers) = &row.maker_orders {
        for m in makers {
            if our_ids.contains(m) {
                return Some((m.clone(), true));
            }
        }
    }
    None
}

/// Re-derive the core token id from a trade row: the row's `asset_id` IS the
/// CLOB token id on this venue (same id the descriptors minted).
fn row_token_id(row: &crate::types::PmTradeRow) -> Option<String> {
    row.asset_id.clone().filter(|s| !s.is_empty())
}

/// Map one trade row onto the seam type. A row whose price or shape cannot be
/// read is skipped by the caller with a log line — never forwarded as garbage.
///
/// The row's `fee_rate_bps` is parsed for wire fidelity and deliberately NOT
/// read here: the live venue reports it as 0 on every row (free trading),
/// which is exactly the optimism #876 corrects. The fee a fill carries is
/// decided by the executor — the maker/taker role per the official crypto
/// schedule (taker `shares · 0.07 · p · (1−p)`, maker 0) — and the kernel's
/// charge path books from that role. See `official_taker_fee_usd`.
fn to_venue_trade(
    row: &crate::types::PmTradeRow,
    venue_order_id: String,
    maker: bool,
) -> Option<VenueTradeInfo> {
    let price = row.price?;
    let size = row.size?;
    if size <= Decimal::ZERO || price <= Decimal::ZERO || price >= Decimal::ONE {
        return None;
    }
    let side = match row.side.as_deref().map(str::to_ascii_uppercase).as_deref() {
        Some("SELL") => Side::Sell,
        _ => Side::Buy,
    };
    Some(VenueTradeInfo {
        venue_order_id,
        trade_id: row.id.clone(),
        token_id: row_token_id(row)?,
        side,
        size,
        price,
        // Live venue (2026-10-26) reports match_time in SECONDS since epoch
        // (e.g. 1792995878 for a 2026-10-26 fill); every ts_ms consumer treats
        // this field as MILLISECONDS. Normalize seconds → ms at the boundary;
        // a value already in ms passes through unchanged. Without this the
        // kernel's external-fill watermark (kept in ms) never advances past
        // these fills and they re-fold as external fills on every sweep.
        ts_ms: match row.match_time {
            Some(t) if t >= 1_000_000_000_000 => t,
            Some(t) => t.saturating_mul(1_000),
            None => now_ms(),
        },
        tx_hash: row.transaction_hash.clone(),
        maker: Some(maker),
    })
}

fn to_fill(order_id: &str, trade: &VenueTradeInfo) -> MarketFill {
    MarketFill {
        order_id: order_id.to_string(),
        trade_id: Some(trade.trade_id.clone()),
        token_id: trade.token_id.clone(),
        side: trade.side,
        price: trade.price,
        size: trade.size,
        status: FillStatus::Matched,
        ts_ms: trade.ts_ms,
        tx_hash: trade.tx_hash.clone(),
        maker: trade.maker,
    }
}

/// One reconciliation pass: open orders + new trades + orphan cleanup + free
/// balance. Any transport failure aborts the pass so the failure counter sees
/// a coherent "sweep did not complete", not a half-swept ledger.
///
/// #876 fill-confirmation semantics: a placement still inside its window
/// ([`in_confirm_window`]) reports OPEN and its fill — whichever channel
/// first carries it — waits for the NEXT sweep. The kernel's reconcile folds
/// a snapshot's trades BEFORE its ghost pass, so a deferred fill with the
/// order still reported open is exactly the safe order: the venue's instant
/// `matched` never books at the same tick's sweep.
async fn sweep(
    host: &Arc<dyn MarketHost>,
    client: &PolySimRest,
    seen_trades: &mut std::collections::HashSet<String>,
    placements: &mut std::collections::HashMap<String, Placement>,
    outcome_polarity: &mut std::collections::HashMap<String, String>,
    orphan_grace_ms: i64,
) -> CoreResult<()> {
    let open = client
        .open_orders()
        .await
        .map_err(|e| into_core_error(&e))?;
    let trades = client.trades().await.map_err(|e| into_core_error(&e))?;

    // Orphan sweep: a venue-open id the core has never heard of gets cancelled
    // (a POST accepted after our timeout, or a resting order whose cancel
    // failed). Fresh placements ride out a grace window.
    let known: std::collections::HashSet<String> =
        host.known_venue_order_ids().await.into_iter().collect();
    let now = now_ms();
    placements.retain(|_, p| now - p.placed_ms < PLACEMENT_TTL_MS);
    for o in &open {
        let vid = o.venue_id();
        if known.contains(&vid)
            || placements
                .get(&vid)
                .is_some_and(|p| now - p.placed_ms < orphan_grace_ms)
        {
            continue;
        }
        match client.cancel_order(&vid).await {
            Ok(_) => {
                eprintln!("polysimulator-extension: cancelled orphan order {vid}");
                host.note_orphan_cancelled(&vid).await;
            }
            Err(e) => {
                eprintln!("polysimulator-extension: failed to cancel orphan {vid}: {e}");
                host.report_error(CoreError::new(
                    CoreErrorCode::VenueError,
                    format!("orphan cancel failed for {vid}: {e}"),
                ))
                .await;
            }
        }
    }

    // Trades → reconcile snapshot; the core matches them to its own orders
    // and books what it owns. OUR fills are attributed through both leg
    // sides (taker + maker); rows that are not ours and rows we cannot read
    // are silent or logged, never forwarded as garbage. A row attributed to
    // an order still inside its confirmation window waits for the next sweep
    // — and is deliberately NOT marked seen, so the same row books then.
    let mut matched = Vec::new();
    for row in &trades {
        if seen_trades.contains(&row.id) {
            continue;
        }
        // Polarity watch (the single-market verdict's counterpart): the trade
        // row's own `outcome` label names which side of the market this
        // TOKEN is. Every readable row feeds the map, ours or not — the
        // verdict endpoint will pair against whichever token the settlement
        // query carries.
        match (&row.asset_id, &row.outcome) {
            (Some(token), Some(word)) if !token.is_empty() && !word.is_empty() => {
                outcome_polarity.insert(token.clone(), word.clone());
            }
            _ => {}
        }
        if let Some((vid, maker)) = our_leg(row, &known) {
            if placements
                .get(&vid)
                .is_some_and(|p| in_confirm_window(p, now))
            {
                eprintln!(
                    "polysimulator-extension: trade row {} for {vid} deferred — inside the fill-confirmation window, books at the next sweep",
                    row.id
                );
                continue;
            }
            seen_trades.insert(row.id.clone());
            match to_venue_trade(row, vid, maker) {
                Some(trade) => matched.push(trade),
                None => eprintln!(
                    "polysimulator-extension: unreadable trade row {} (market {}) — skipped",
                    row.id, row.market
                ),
            }
        } else {
            // Mark seen regardless: an unreadable row must not be re-parsed
            // every sweep (it would spam the log forever).
            seen_trades.insert(row.id.clone());
        }
    }

    // FILLED-orders attribution (live venue 2026-10-26): the trade rows' leg
    // fields NEVER name our orders here — `taker_order_id` repeats the trade's
    // own id and `maker_orders` is absent — so the stream above cannot match a
    // single fill on this venue. The FILLED orders list does: each row names
    // its own order id, executed price and fill time. Every FILLED row whose
    // id the core knows becomes a synthetic reconcile trade; dedup key
    // `fill:{order_id}` (a limit order fills once at `fill_price`, so the
    // order id alone identifies the fill event).
    //
    // #876: a FILLED row for an order inside its confirmation window waits
    // for the next sweep (unmarked, so it books then), and the fill books
    // with the role the PLACE ack decided — `live` → maker (official maker
    // fee 0), `matched` → taker. The venue's FILLED list carries no role, so
    // "everything taker" was exactly its missing-queue optimism. An order
    // this process never placed (restart recovery) has no record: the role
    // stays `None` and the core falls back to the order's own fill policy.
    if let Ok(filled) = client.filled_orders().await {
        for o in &filled {
            let vid = o.venue_id();
            let key = format!("fill:{}", vid);
            if !known.contains(&vid) || seen_trades.contains(&key) {
                continue;
            }
            if placements
                .get(&vid)
                .is_some_and(|p| in_confirm_window(p, now))
            {
                eprintln!(
                    "polysimulator-extension: FILLED order {vid} deferred — inside the fill-confirmation window, books at the next sweep"
                );
                continue;
            }
            seen_trades.insert(key);
            // The record is consumed with the row it decided: booked or
            // skipped-as-unreadable either way, the order is terminal at the
            // venue and must not keep reporting OPEN.
            let crossed = placements.remove(&vid).and_then(|p| p.crossed);
            let (Some(price), Some(size)) = (o.fill_price, o.quantity) else {
                eprintln!(
                    "polysimulator-extension: FILLED order {} lacks fill_price/quantity — skipped",
                    vid
                );
                continue;
            };
            if size <= Decimal::ZERO || price <= Decimal::ZERO || price >= Decimal::ONE {
                eprintln!(
                    "polysimulator-extension: FILLED order {} out-of-band price/size ({price}×{size}) — skipped",
                    vid
                );
                continue;
            }
            let ts_ms = o
                .filled_at
                .as_deref()
                .and_then(crate::types::iso_to_ms)
                .unwrap_or_else(now_ms);
            matched.push(VenueTradeInfo {
                venue_order_id: vid,
                trade_id: format!("fill:{}", o.venue_id()),
                token_id: o.market_id.clone(),
                side: match o.side.as_deref().map(str::to_ascii_uppercase).as_deref() {
                    Some("SELL") => Side::Sell,
                    _ => Side::Buy,
                },
                size,
                price,
                ts_ms,
                tx_hash: None,
                maker: fill_role(crossed),
            });
        }
    }

    // The snapshot's open list: the venue's own PENDING rows PLUS every
    // placement still inside its confirmation window (#876) — a just-placed
    // order stays non-terminal (OPEN/resting) toward the core even when the
    // venue has already matched it, until the next sweep confirms FILLED.
    let mut open_ids: Vec<String> = open.iter().map(|o| o.venue_id()).collect();
    for (vid, p) in placements.iter() {
        if in_confirm_window(p, now) && !open_ids.iter().any(|listed| listed == vid) {
            open_ids.push(vid.clone());
        }
    }

    // Fee evidence (#876): the fee a fill carries under the official crypto
    // schedule, on the record next to the fill row. The maker/taker role is
    // the fee-bearing field the kernel's charge path books from (maker 0);
    // the venue's own `fee_rate_bps: 0` is never used.
    for trade in &matched {
        let (role, fee) = match trade.maker {
            Some(false) => ("taker", official_taker_fee_usd(trade.price, trade.size)),
            Some(true) => ("maker", Decimal::ZERO),
            None => ("unknown (core fill-policy fallback)", Decimal::ZERO),
        };
        eprintln!(
            "polysimulator-extension: fill {} {} {}@{} role={role} official_taker_fee_usd={fee}",
            trade.trade_id,
            match trade.side {
                Side::Buy => "BUY",
                Side::Sell => "SELL",
            },
            trade.size,
            trade.price
        );
    }

    let snapshot = ReconcileSnapshot {
        open_order_ids: open_ids,
        trades: matched.clone(),
        now_ms: now_ms(),
    };
    host.on_reconcile(snapshot).await;
    for trade in &matched {
        host.on_fill(to_fill(&trade.venue_order_id, trade)).await;
    }

    // (#918, #909 follow-up) The ledger takes the CASH figure collected below —
    // NOT the equity view, and not the venue's `total_value`. The comment block
    // that preceded it said "re-align the ledger's equity view … the venue's
    // own total_value", which is the retired contract: folding open positions
    // back into the cash book pumped phantom cash into the account on every
    // no-order sweep. `ledger_equity` still reports the degradation to a cash
    // floor when the venue does not state a cash figure. The kernel folds this
    // through its reserved-aware venue-cash semantics (`fold_venue_free`):
    // resting commitments still surface as drift warnings, never auto-corrected.
    match client.balance().await {
        Ok(b) => {
            let equity = ledger_equity(&b);
            if let Some(reason) = equity.degraded() {
                eprintln!("polysimulator-extension: balance sync degraded to cash only: {reason}");
            }
            // The ledger takes the CASH figure (#909 follow-up): the equity
            // view over-approved the reserve gate and folded the open
            // position back in as phantom cash on every no-order sweep.
            host.venue_free_balance("polysimulator", equity.figure())
                .await;
        }
        Err(e) => host.report_error(into_core_error(&e)).await,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_place_body_prices_scale_to_1e6_integer_strings() {
        // BUY 5 shares at 0.435 → maker = 0.435·5 = 2.175 USDC = 2175000;
        // taker = 5 shares = 5000000. price = maker/taker = 0.435.
        let body = place_body_for(
            &Side::Buy,
            Decimal::from_str("0.435").unwrap(),
            Decimal::from(5),
            "tok-1",
        );
        assert_eq!(body.token_id, "tok-1");
        assert_eq!(body.side, "BUY");
        assert_eq!(body.maker_amount, "2175000");
        assert_eq!(body.taker_amount, "5000000");

        // SELL 5 shares at 0.90 → the pair inverts (price = taker/maker).
        let body = place_body_for(
            &Side::Sell,
            Decimal::from_str("0.90").unwrap(),
            Decimal::from(5),
            "tok-2",
        );
        assert_eq!(body.side, "SELL");
        assert_eq!(body.maker_amount, "5000000");
        assert_eq!(body.taker_amount, "4500000");
    }

    fn trade_row(
        id: &str,
        taker: Option<&str>,
        makers: Option<Vec<&str>>,
        side: &str,
        price: Option<&str>,
        asset: Option<&str>,
    ) -> crate::types::PmTradeRow {
        crate::types::PmTradeRow {
            id: id.to_string(),
            market: "0xabc".to_string(),
            asset_id: asset.map(|s| s.to_string()),
            side: Some(side.to_string()),
            outcome: Some("Yes".to_string()),
            price: price.map(|p| Decimal::from_str(p).unwrap()),
            size: Some(Decimal::from(10)),
            fee_rate_bps: Some(0),
            maker_orders: makers.map(|v| v.iter().map(|s| s.to_string()).collect()),
            taker_order_id: taker.map(|s| s.to_string()),
            transaction_hash: None,
            bucket_index: None,
            last_update: None,
            match_time: Some(1_773_583_200_000),
            owner: None,
            status: Some("MATCHED".to_string()),
            trader_side: None,
        }
    }
    use std::str::FromStr;

    #[test]
    fn trade_rows_attribute_through_both_leg_sides() {
        let our_ids: std::collections::HashSet<String> =
            ["41", "77"].iter().map(|s| s.to_string()).collect();
        // Taker leg is ours.
        let row = trade_row(
            "tx-1",
            Some("41"),
            Some(vec!["12"]),
            "BUY",
            Some("0.55"),
            Some("71321"),
        );
        assert_eq!(our_leg(&row, &our_ids), Some(("41".into(), false)));
        let trade = to_venue_trade(&row, "41".into(), false).expect("taker row maps");
        assert_eq!(trade.token_id, "71321");
        assert_eq!(trade.side, Side::Buy);
        assert_eq!(trade.price, Decimal::from_str("0.55").unwrap());
        assert_eq!(trade.maker, Some(false), "a taker-leg fill is not maker");

        // Maker leg is ours (our resting id inside maker_orders).
        let row_maker = trade_row(
            "tx-1",
            Some("9999"),
            Some(vec!["12", "77"]),
            "SELL",
            Some("0.55"),
            Some("71321"),
        );
        assert_eq!(our_leg(&row_maker, &our_ids), Some(("77".into(), true)));
        let trade = to_venue_trade(&row_maker, "77".into(), true).expect("maker row maps");
        assert_eq!(trade.side, Side::Sell);
        assert_eq!(trade.maker, Some(true), "a maker-leg fill is maker");

        // Neither leg is ours: not attributed.
        let row_other = trade_row(
            "tx-1",
            Some("9999"),
            Some(vec!["12"]),
            "BUY",
            Some("0.55"),
            Some("71321"),
        );
        assert_eq!(our_leg(&row_other, &our_ids), None);
    }

    #[test]
    fn unreadable_trade_rows_map_to_none_not_garbage() {
        let base = trade_row("tx-2", Some("41"), None, "BUY", None, Some("71321"));
        assert!(
            to_venue_trade(&base, "41".into(), false).is_none(),
            "no price"
        );

        let zero = trade_row("tx-2", Some("41"), None, "BUY", Some("0.55"), Some("71321"))
            .pipe_zero_size();
        assert!(
            to_venue_trade(&zero, "41".into(), false).is_none(),
            "zero size"
        );

        let out_of_band = trade_row("tx-2", Some("41"), None, "BUY", Some("1.0"), Some("71321"));
        assert!(
            to_venue_trade(&out_of_band, "41".into(), false).is_none(),
            "price 1.0 is not a tradeable probability"
        );

        let no_token = trade_row("tx-2", Some("41"), None, "BUY", Some("0.55"), None);
        assert!(
            to_venue_trade(&no_token, "41".into(), false).is_none(),
            "no asset_id"
        );
    }

    #[test]
    fn match_time_in_seconds_is_normalized_to_ms() {
        // Live venue (2026-10-26) reports match_time in SECONDS since epoch;
        // ts_ms consumers (incl. the kernel's external-fill watermark) expect
        // MILLISECONDS. A seconds-valued row must be scaled ×1000, and an
        // already-millisecond value must pass through untouched.
        let mut row = trade_row("tx-3", Some("41"), None, "BUY", Some("0.55"), Some("71321"));

        row.match_time = Some(1_792_995_878); // seconds: 2026-10-26T06:24:38Z
        let trade = to_venue_trade(&row, "41".into(), false).expect("seconds row maps");
        assert_eq!(trade.ts_ms, 1_792_995_878_000, "seconds scaled to ms");

        row.match_time = Some(1_773_583_200_000); // already ms
        let trade = to_venue_trade(&row, "41".into(), false).expect("ms row maps");
        assert_eq!(trade.ts_ms, 1_773_583_200_000, "ms passes through");

        row.match_time = None;
        let now = now_ms();
        let trade = to_venue_trade(&row, "41".into(), false).expect("absent row maps");
        assert!(trade.ts_ms >= now, "absent match_time falls back to now_ms");
    }

    /// Test helper: rebuild a row with a zero size (struct has no builder).
    trait ZeroSize {
        fn pipe_zero_size(self) -> crate::types::PmTradeRow;
    }
    impl ZeroSize for crate::types::PmTradeRow {
        fn pipe_zero_size(mut self) -> crate::types::PmTradeRow {
            self.size = Some(Decimal::ZERO);
            self
        }
    }

    // ── #876: official fee schedule ─────────────────────────────────────

    /// The taker fee is the OFFICIAL crypto formula `shares · 0.07 · p ·
    /// (1−p)` — not the venue's `fee_rate_bps: 0`, not the legacy kernel
    /// curve. Boundaries charge nothing (never a negative fee).
    #[test]
    fn official_taker_fee_is_the_published_crypto_formula() {
        let dec = |s: &str| Decimal::from_str(s).unwrap();
        // 10 shares @ 0.5 → 10 · 0.07 · 0.25 = 0.175 USD.
        assert_eq!(
            official_taker_fee_usd(dec("0.5"), Decimal::from(10)),
            dec("0.175")
        );
        // 5 shares @ 0.435 → 5 · 0.07 · 0.435 · 0.565 = 0.08602125 USD.
        assert_eq!(
            official_taker_fee_usd(dec("0.435"), Decimal::from(5)),
            dec("0.08602125")
        );
        // Boundaries: a price at 0 or 1 (and non-positive inputs) is not a
        // tradeable probability — no fee, never a rebate.
        assert_eq!(
            official_taker_fee_usd(Decimal::ZERO, Decimal::from(10)),
            Decimal::ZERO
        );
        assert_eq!(
            official_taker_fee_usd(Decimal::ONE, Decimal::from(10)),
            Decimal::ZERO
        );
        assert_eq!(
            official_taker_fee_usd(dec("0.5"), Decimal::ZERO),
            Decimal::ZERO
        );
    }

    /// The maker side of the official schedule is ZERO: a fill whose role
    /// books maker carries no fee. The role comes from the place ack —
    /// `live` (rested) → maker, `matched` (crossed) → taker, unreadable →
    /// None (the core's fill-policy fallback).
    #[test]
    fn maker_fee_is_zero_and_the_role_is_decided_by_the_place_ack() {
        // Ack verdicts…
        assert_eq!(ack_crossed(Some("matched")), Some(true), "crossed at place");
        assert_eq!(ack_crossed(Some("live")), Some(false), "rested");
        assert_eq!(ack_crossed(None), None, "unreadable ack stays unknown");
        assert_eq!(
            ack_crossed(Some("pending")),
            None,
            "an unknown status says nothing"
        );

        // …become fill roles.
        assert_eq!(fill_role(Some(true)), Some(false), "matched → taker");
        assert_eq!(fill_role(Some(false)), Some(true), "rested → maker (fee 0)");
        assert_eq!(fill_role(None), None, "unknown stays unknown");
        // A maker-role fill's fee under this schedule is exactly zero — the
        // evidence line books Decimal::ZERO for it (the schedule's maker
        // side), never the taker formula.
        let maker_fee = match fill_role(Some(false)) {
            Some(true) => Decimal::ZERO,
            _ => official_taker_fee_usd(Decimal::from_str("0.5").unwrap(), Decimal::from(10)),
        };
        assert_eq!(maker_fee, Decimal::ZERO, "official maker fee is 0");
    }

    /// The confirmation window: inside one sweep period the fill waits
    /// (order reports OPEN); at/after it the next sweep books. The window
    /// is the sweep cadence — one sweep period, no new timer.
    #[test]
    fn fill_confirmation_window_is_one_sweep_period() {
        assert_eq!(
            FILL_CONFIRM_MS,
            (EXECUTOR_TICK_MS * SWEEP_EVERY_TICKS) as i64
        );
        let placed = Placement {
            crossed: Some(false),
            placed_ms: 10_000,
        };
        assert!(
            in_confirm_window(&placed, 10_000),
            "same instant: in window"
        );
        assert!(
            in_confirm_window(&placed, 10_000 + FILL_CONFIRM_MS - 1),
            "just under one sweep period: still in window"
        );
        assert!(
            !in_confirm_window(&placed, 10_000 + FILL_CONFIRM_MS),
            "at one sweep period: the next sweep may confirm FILLED"
        );
    }

    // ── #877: the ledger seed is the venue's equity, not bare cash ──────

    fn balance_body(
        balance: &str,
        total_value: Option<&str>,
        unrealized: Option<&str>,
    ) -> BalanceBody {
        BalanceBody {
            balance: Decimal::from_str(balance).unwrap(),
            currency: Some("USDC".to_string()),
            starting_balance: None,
            unrealized_pnl: unrealized.map(|u| Decimal::from_str(u).unwrap()),
            total_value: total_value.map(|t| Decimal::from_str(t).unwrap()),
        }
    }

    /// The #877 screenshot identity, in one balance body: cash 111.62 +
    /// positions 20.00 = equity 131.62. Post-#909-follow-up the LEDGER figure
    /// is the spendable CASH (111.62 — the reserve gate must never see the
    /// position value) while the website's equity identity rides the evidence
    /// line read-only (cash, positions component, unrealized, equity).
    #[test]
    fn ledger_seed_takes_cash_and_keeps_equity_on_the_evidence_line() {
        let eq = ledger_equity(&balance_body("111.62", Some("131.62"), Some("0.00")));
        assert_eq!(
            eq.figure(),
            Decimal::from_str("111.62").unwrap(),
            "the ledger is a cash book (#909 acceptance: an equity figure over-approved the reserve gate by the position value)"
        );
        assert_eq!(
            eq,
            LedgerEquity::VenueReported {
                equity: Decimal::from_str("131.62").unwrap(),
                cash: Decimal::from_str("111.62").unwrap(),
                unrealized: Some(Decimal::ZERO),
            }
        );
        assert!(eq.degraded().is_none(), "a reported equity is not degraded");
        let rendered = eq.to_string();
        for needle in [
            "cash=111.62",
            "positions=20.00",
            "unrealized=0.00",
            "equity=131.62",
        ] {
            assert!(
                rendered.contains(needle),
                "{needle} missing from: {rendered}"
            );
        }
    }

    /// `unrealized_pnl` never enters the arithmetic: it is quoted against
    /// entry cost (equity = cash + cost + unrealized, cost basis not on this
    /// body). It rides the evidence line only — and the ledger figure is the
    /// cash either way.
    #[test]
    fn unrealized_pnl_never_enters_the_arithmetic() {
        let eq = ledger_equity(&balance_body("111.62", Some("131.62"), Some("5.00")));
        assert_eq!(
            eq.figure(),
            Decimal::from_str("111.62").unwrap(),
            "the ledger takes the cash figure; unrealized_pnl must not be added on top"
        );
        assert!(
            eq.to_string().contains("unrealized=5.00"),
            "unrealized stays visible on the evidence line"
        );
    }

    /// An older website build without `total_value`: the figure falls back
    /// to the cash floor — and the degradation reason rides to the caller's
    /// log line, never silently zeroing the positions (#877's blind spot).
    #[test]
    fn ledger_seed_degrades_to_the_cash_floor_without_total_value() {
        let eq = ledger_equity(&balance_body("111.62", None, None));
        assert_eq!(eq.figure(), Decimal::from_str("111.62").unwrap());
        assert!(
            eq.degraded().is_some(),
            "the cash floor must be visible as a DEGRADED read, not a silent default"
        );
        let rendered = eq.to_string();
        assert!(rendered.contains("DEGRADED"), "{rendered}");
        assert!(rendered.contains("issue #877"), "{rendered}");
        assert!(rendered.contains("cash=111.62"), "{rendered}");
    }

    /// A long-only venue's equity can never sit below its available cash —
    /// a `total_value` that does is a wire inconsistency: refused
    /// (fail-closed), the figure falls back to the cash floor loudly.
    #[test]
    fn ledger_seed_refuses_a_total_value_below_cash() {
        let eq = ledger_equity(&balance_body("111.62", Some("90.00"), None));
        assert_eq!(eq.figure(), Decimal::from_str("111.62").unwrap());
        assert!(
            eq.degraded().is_some(),
            "the refused field must degrade visibly, not pass through"
        );
    }

    /// Flat account (no open positions): `total_value` == balance — the
    /// venue-reported path holds with a zero positions component, no
    /// degradation.
    #[test]
    fn flat_account_reports_equity_equal_to_cash_without_degradation() {
        let eq = ledger_equity(&balance_body("100.00", Some("100.00"), Some("0.00")));
        assert!(eq.degraded().is_none());
        assert_eq!(eq.figure(), Decimal::from_str("100.00").unwrap());
        assert!(eq.to_string().contains("positions=0.00"));
    }
}
