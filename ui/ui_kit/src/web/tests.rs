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

/// issue 719: `money`/`pct` are now the shared signed figures BOTH faces
/// render (web HTML + the TUI panel's PnL columns), so the wire-visible
/// bytes are pinned here — a format change on either side fails this test
/// before it can silently diverge the two faces.
#[test]
fn shared_signed_formatters_render_the_pinned_bytes() {
    assert_eq!(money(12.341), "+12.34");
    assert_eq!(money(-12.341), "-12.34");
    assert_eq!(money(0.0), "+0.00");
    assert_eq!(pct(3.44), "+3.4%");
    assert_eq!(pct(-3.44), "-3.4%");
    assert_eq!(pct(0.0), "+0.0%");
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
fn exe_sibling_webui_shape_resolves() {
    // issue 487: the release-archive shape — <stage>/bin/<exe> with the
    // bundled panel at <stage>/webui/ — must resolve with NO repo around.
    let base = std::env::temp_dir().join(format!("bk-panel-arch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let stage = base.join("stage");
    std::fs::create_dir_all(stage.join("bin")).unwrap();
    std::fs::create_dir_all(stage.join("webui")).unwrap();
    std::fs::write(stage.join("webui/index.html"), b"<html></html>").unwrap();
    let candidate = stage.join("bin").join("../webui/index.html");
    assert_eq!(
        resolve_panel_dir(&[candidate]),
        Some(stage.join("webui").canonicalize().unwrap())
    );
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
    let doc: serde_json::Value = serde_json::from_str(&render_json(&snap, Some(&adopted))).unwrap();
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
    let doc: serde_json::Value = serde_json::from_str(&render_json(&snap, Some(&owned))).unwrap();
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

/// The update half of the snapshot contract: `stageState` is ON the wire
/// (it never was — the WebUI's staging badge read a key the gateway never
/// wrote), and `systemVersion` carries the failure reason + the staging
/// state so a poller never needs the download verb. A default snapshot
/// renders both as null/absent, which every consumer reads as "unknown".
#[test]
fn the_snapshot_carries_the_stage_state_and_check_error() {
    use crate::core::types::{StageStateView, SystemVersionView};

    let mut snap = UiSnapshot::default();
    let doc: serde_json::Value = serde_json::from_str(&render_json(&snap, None)).unwrap();
    // Default snapshot: the key EXISTS (older cores → null, "cannot stage"),
    // and systemVersion is absent (nothing answered).
    assert!(
        doc.get("stageState").is_some(),
        "stageState must be a wire key even when the core cannot stage"
    );
    assert_eq!(doc["stageState"], serde_json::Value::Null);

    // A snapshot with version+stage data carries both through, verbatim.
    snap.system_version = Some(SystemVersionView {
        version: "0.4.0".into(),
        last_check_error: Some("GET releases/latest: HTTP 403".into()),
        stage: Some(StageStateView {
            phase: "staged".into(),
            version: Some("0.4.1".into()),
            ..StageStateView::default()
        }),
        ..SystemVersionView::default()
    });
    snap.stage_state = snap.system_version.as_ref().and_then(|v| v.stage.clone());
    let doc: serde_json::Value = serde_json::from_str(&render_json(&snap, None)).unwrap();
    assert_eq!(
        doc["systemVersion"]["lastCheckError"],
        serde_json::json!("GET releases/latest: HTTP 403"),
        "the failure reason must reach the panel verbatim"
    );
    assert_eq!(doc["stageState"]["phase"], serde_json::json!("staged"));
    assert_eq!(doc["stageState"]["version"], serde_json::json!("0.4.1"));
}

/// issue 456: the snapshot carries the kernel's order book (`orders`) so
/// the WebUI's 挂单 card can render it without a second round-trip. The
/// wire shape mirrors `orders.list`'s TrackedOrder rows (camelCase) —
/// `status` is what the card's per-row force-close gate reads, and a
/// default snapshot yields the key as an empty array, never absent.
#[test]
fn the_snapshot_carries_the_order_book() {
    let mut snap = UiSnapshot::default();
    let doc: serde_json::Value = serde_json::from_str(&render_json(&snap, None)).unwrap();
    assert_eq!(doc["orders"], serde_json::json!([]));

    snap.orders = vec![crate::core::types::OrderView {
        order_id: "dry_7".into(),
        strategy: "spread_arb".into(),
        asset: "BTC_UP".into(),
        direction: "up".into(),
        token_id: "tok-1".into(),
        side: "buy".into(),
        mode: "maker".into(),
        price: 0.39,
        size: 5.0,
        filled_size: 0.0,
        status: "LIVE".into(),
        round_slot: 123,
        account_id: "default".into(),
        venue: "polymarket".into(),
    }];
    let doc: serde_json::Value = serde_json::from_str(&render_json(&snap, None)).unwrap();
    let row = &doc["orders"][0];
    assert_eq!(row["orderId"], serde_json::json!("dry_7"));
    assert_eq!(row["status"], serde_json::json!("LIVE"));
    assert_eq!(row["roundSlot"], serde_json::json!(123));
    assert_eq!(row["price"], serde_json::json!(0.39));
    // E28: the account rides along so the panel can filter rows per account.
    assert_eq!(row["accountId"], serde_json::json!("default"));
    // The venue tag rides along too (empty = untagged, filter keeps the row).
    assert_eq!(row["venue"], serde_json::json!("polymarket"));
    assert_eq!(doc["orders"].as_array().map(|a| a.len()), Some(1));
}

/// E28 (§9.2) backward compatibility: a core older than the account book
/// omits `accountId` on every row wire — the views must still parse
/// (serde default = empty string) and the gateway must emit the empty
/// string rather than drop the key, so the panel can tell "unattributed
/// row, show it" apart from "attributed row, filter on it".
#[test]
fn a_pre_e28_core_without_account_ids_still_parses_and_renders() {
    // Raw `positions.list` / `trades.history` / `orders.list` shapes as a
    // pre-E28 core answers them: no accountId anywhere.
    let pos: crate::core::types::PositionView = serde_json::from_value(serde_json::json!({
        "id": "p1", "asset": "BTC_UP", "direction": "up", "tokenId": "tok-1",
        "entryPrice": 0.4, "currentPrice": 0.5, "shares": 10, "unrealizedPct": 25.0
    }))
    .expect("a pre-E28 position row must parse without accountId");
    assert_eq!(pos.account_id, "", "missing accountId defaults to empty");

    let trade: crate::core::types::TradeView = serde_json::from_value(serde_json::json!({
        "id": "hft-1", "asset": "BTC_UP", "direction": "up",
        "entryPrice": 0.4, "exitPrice": 0.5, "shares": 10, "netPnlUsd": 1.0
    }))
    .expect("a pre-E28 trade row must parse without accountId");
    assert_eq!(trade.account_id, "");

    let order: crate::core::types::OrderView = serde_json::from_value(serde_json::json!({
        "orderId": "dry_1", "asset": "BTC_UP", "direction": "up", "tokenId": "tok-1",
        "side": "buy", "price": 0.4, "size": 10, "status": "LIVE"
    }))
    .expect("a pre-E28 order row must parse without accountId");
    assert_eq!(order.account_id, "");

    // And a NEW-core row (accountId present) parses through the same views.
    let attributed: crate::core::types::PositionView = serde_json::from_value(serde_json::json!({
        "id": "p2", "asset": "BTC_UP", "direction": "up", "tokenId": "tok-1",
        "entryPrice": 0.4, "currentPrice": 0.5, "shares": 10,
        "unrealizedPct": 25.0, "accountId": "paper"
    }))
    .expect("an E28 position row parses with accountId");
    assert_eq!(attributed.account_id, "paper");

    // The gateway's snapshot mapping forwards whatever it parsed: empty
    // stays empty (the panel's "show unattributed" signal), "paper" stays
    // "paper".
    let snap = UiSnapshot {
        positions: vec![pos, attributed],
        ..UiSnapshot::default()
    };
    let doc: serde_json::Value = serde_json::from_str(&render_json(&snap, None)).unwrap();
    let rows = doc["positions"].as_array().expect("positions array");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["accountId"], serde_json::json!(""));
    assert_eq!(rows[1]["accountId"], serde_json::json!("paper"));
    // The id rides through too (#678 follow-up): the 持仓 card's per-row
    // 强平 flattens by it. A json!-literal omission here is exactly how
    // the panel ended up sending the literal `flatten undefined` — the
    // core always wrote the field, the gateway dropped it in re-assembly.
    assert_eq!(rows[0]["id"], serde_json::json!("p1"));
    assert_eq!(rows[1]["id"], serde_json::json!("p2"));
}

/// 账户↔venue 解耦（本轮）：the typed re-assembly in `render_json_full`
/// must NOT strip the new `venue` fields — this is exactly the
/// "网关类型化重组剥字段" failure the WebUI disclosed: a struct field the
/// json! literal omits never reaches the browser, no matter what the core
/// sent. Both quote paths are pinned: `round.prices` (per market row) and
/// `books` (per asset row). A pre-venue core deserialises into empty
/// strings, which the panel renders as 未分所 — never filters on.
#[test]
fn the_snapshot_carries_the_venue_tag_on_quote_rows() {
    use crate::core::types::{
        AssetBooksView, BookSideView, MarketPriceView, RoundView, VenueRoundView,
    };

    let snap = UiSnapshot {
        round: Some(RoundView {
            slot: 7,
            age_sec: 1,
            time_left_sec: 299,
            markets: 2,
            can_trade: true,
            market_prices: vec![
                MarketPriceView {
                    asset: "BTC".into(),
                    up: 0.61,
                    down: 0.39,
                    venue: "polymarket".into(),
                },
                MarketPriceView {
                    asset: "BTC".into(),
                    up: 0.60,
                    down: 0.40,
                    venue: "kalshi".into(),
                },
            ],
            // #550（症状②）：the per-venue round clock rows travel the
            // same wire — one row per venue that has spoken. B1a: the
            // rows carry `durationSec` (the row's round-length family;
            // 0 = a pre-B1a core's undeclared default).
            venue_rounds: vec![
                VenueRoundView {
                    venue: "polymarket".into(),
                    duration_sec: 900,
                    slot: 7,
                    age_sec: 1,
                    time_left_sec: 299,
                },
                VenueRoundView {
                    venue: "kalshi".into(),
                    duration_sec: 3600,
                    slot: 7,
                    age_sec: 5,
                    time_left_sec: 295,
                },
            ],
        }),
        books: vec![AssetBooksView {
            asset: "BTC".into(),
            up_token_id: "tok-up".into(),
            down_token_id: "tok-down".into(),
            venue: "kalshi".into(),
            up: BookSideView::default(),
            down: BookSideView::default(),
        }],
        ..UiSnapshot::default()
    };
    let doc: serde_json::Value = serde_json::from_str(&render_json(&snap, None)).unwrap();
    let prices = doc["round"]["prices"].as_array().expect("prices array");
    assert_eq!(prices.len(), 2, "same asset, two venues → two rows");
    assert_eq!(prices[0]["venue"], serde_json::json!("polymarket"));
    assert_eq!(prices[1]["venue"], serde_json::json!("kalshi"));
    assert_eq!(doc["books"][0]["venue"], serde_json::json!("kalshi"));
    // The per-venue clock rows reach the panel verbatim (the countdown
    // re-binds to the active venue's own row).
    let vr = doc["round"]["venueRounds"]
        .as_array()
        .expect("venueRounds array");
    assert_eq!(vr.len(), 2, "one row per venue that has spoken");
    assert_eq!(vr[0]["venue"], serde_json::json!("polymarket"));
    assert_eq!(vr[1]["timeLeftSec"], serde_json::json!(295));
    // B1a: the typed re-assembly must not strip `durationSec` — the same
    // "网关类型化重组剥字段" failure class the `venue` fields pinned
    // above. The multi-family panel selects its row by (venue, duration).
    assert_eq!(vr[0]["durationSec"], serde_json::json!(900));
    assert_eq!(vr[1]["durationSec"], serde_json::json!(3600));

    // Backward compatibility: a core that never wrote `venue` parses into
    // the empty-string default and the wire still carries the key.
    let legacy: RoundView = serde_json::from_value(serde_json::json!({
        "slot": 1, "ageSec": 0, "timeLeftSec": 60, "markets": 1,
        "canTrade": false,
        "marketPrices": [{ "asset": "ETH", "up": 0.5, "down": 0.5 }]
    }))
    .expect("a pre-venue round row must parse without venue");
    assert_eq!(legacy.market_prices[0].venue, "");
    let snap = UiSnapshot {
        round: Some(legacy),
        ..UiSnapshot::default()
    };
    let doc: serde_json::Value = serde_json::from_str(&render_json(&snap, None)).unwrap();
    assert_eq!(
        doc["round"]["prices"][0]["venue"],
        serde_json::json!(""),
        "untagged rows reach the panel as 未分所, never as a missing key"
    );
    // An old core that never wrote venueRounds degrades to an EMPTY array
    // on the wire (the panel's feature-detect keeps the legacy countdown).
    assert_eq!(
        doc["round"]["venueRounds"],
        serde_json::json!([]),
        "pre-#550 cores degrade to an empty venueRounds, never a missing key"
    );
}

/// issue 550 / issue 559 显示面口径（TUI 状态页漏网）：the built-in HTML
/// panel's Round card must follow the display account's own venue round row
/// when `venue_rounds` is populated, render one pill per spoken row, and
/// stay byte-for-byte on the legacy single-family card when the rows are
/// absent (pre-issue-550 core → empty serde default).
#[test]
fn the_round_card_follows_the_display_venue_row_and_lists_every_family() {
    use crate::core::types::{RoundView, VenueRoundView};

    let round = |rows: Vec<VenueRoundView>| UiSnapshot {
        round: Some(RoundView {
            slot: 7,
            age_sec: 1,
            time_left_sec: 299,
            markets: 2,
            can_trade: true,
            venue_rounds: rows,
            ..RoundView::default()
        }),
        ..UiSnapshot::default()
    };

    // Multi-family: display venue kalshi's own 3600s clock wins the
    // headline, and both families render as pills beneath it.
    let mut s = round(vec![
        VenueRoundView {
            venue: "polymarket".into(),
            duration_sec: 900,
            slot: 7,
            age_sec: 1,
            time_left_sec: 299,
        },
        VenueRoundView {
            venue: "kalshi".into(),
            duration_sec: 3600,
            slot: 3,
            age_sec: 5,
            time_left_sec: 295,
        },
    ]);
    s.display_venue = "kalshi".into();
    let html = render_html(&s);
    assert!(
        html.contains("<div class=big>#3</div>"),
        "the headline must read the display venue's own slot: {html}"
    );
    assert!(
        html.contains("5s old · 295s left"),
        "the headline must read the display venue's own age/timeLeft: {html}"
    );
    assert!(
        html.contains("<span class=pill>显示: kalshi</span>"),
        "the card must disclose which venue's clock the headline follows: {html}"
    );
    assert!(
        html.contains("<span class=pill>polymarket: #7 · 299s left · 900s family</span>"),
        "every spoken family renders a pill: {html}"
    );
    assert!(
        html.contains("<span class=pill>kalshi: #3 · 295s left · 3600s family</span>"),
        "every spoken family renders a pill: {html}"
    );

    // Single family, no display pointer: the fallback row is the venue's
    // own row, the headline still follows it (same numbers a legacy card
    // showed when that family was the only clock).
    s.display_venue = String::new();
    let html = render_html(&round(vec![VenueRoundView {
        venue: "polymarket".into(),
        duration_sec: 900,
        slot: 7,
        age_sec: 1,
        time_left_sec: 299,
    }]));
    assert!(
        html.contains("<div class=big>#7</div>") && html.contains("1s old · 299s left"),
        "a single spoken family keeps the same headline numbers: {html}"
    );

    // Legacy byte-for-byte equivalence: without venue_rounds the card is
    // the exact pre-change markup (global scalar block, no pills).
    let legacy_html = render_html(&round(Vec::new()));
    assert!(
        legacy_html.contains(
            "<div class=card><h2>Round</h2><div class=big>#7</div><div class=sub>1s old · 299s left · TRADING</div></div>"
        ),
        "the legacy card markup must not change when no venue rows exist: {legacy_html}"
    );
    assert!(
        !legacy_html.contains("class=pill>显示"),
        "no display pill without venue rows: {legacy_html}"
    );
    assert!(
        !legacy_html.contains("s family"),
        "no family pills without venue rows: {legacy_html}"
    );
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

    let doc: serde_json::Value = serde_json::from_str(&render_json(&snap, Some(&give_up))).unwrap();
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
