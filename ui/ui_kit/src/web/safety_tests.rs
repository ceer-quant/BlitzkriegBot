//! #236 — the panel-facing half of the seam, tested where both ends are visible.
//!
//! The kernel-side half (a real dry core's `engine_stats()` payload landing in
//! `EngineStatsView`) is pinned by `core/blitzkrieg_core/tests/
//! engine_stats_view_seam.rs`. This file pins the other direction: the view
//! reaching `/api/snapshot` as the camelCase JSON the panel's `EngineStats`
//! TypeScript interface reads.
//!
//! Both halves matter because the defect was a chain, not a point: the kernel
//! emitted the keys, the view type could not carry them, and the renderer only
//! forwards what the view carries. Fixing the type alone would still have left
//! the banners dark if `render_json_full` had not forwarded them, and that is
//! exactly the kind of gap a struct-only test cannot see.

use crate::core::types::{
    AccountGateView, ArchiveView, BlockedCounters, DailyLossAccountView, DailyLossView,
    EngineStatsView, LastErrorView, LegacyVenueErrorView, ReconcileView, SelfCheckItemView,
    SelfCheckView, SettlementView, StrategyStatsRow, TradingFrozenView, UiSnapshot,
};
use crate::web::render_json_full;

/// A frozen kernel with a refusal behind it: the exact state in which the panel
/// used to draw nothing at all.
fn frozen_snapshot() -> UiSnapshot {
    UiSnapshot {
        connected: true,
        stats: Some(EngineStatsView {
            books: 12,
            venue_rejected: 5,
            last_error: Some(LastErrorView {
                ts_ms: 1_000_000_000_000,
                code: "NOT_AUTHENTICATED".into(),
                message: "auth failed".into(),
            }),
            last_venue_error: Some(LegacyVenueErrorView {
                ts_ms: 1_000_000_000_000,
                message: "NotAuthenticated: auth failed".into(),
            }),
            reconcile: Some(ReconcileView {
                consecutive_sweep_failures: 2,
                freeze_threshold: 3,
            }),
            self_check: Some(SelfCheckView {
                ok: false,
                ts_ms: 1_000_000_000_000,
                items: vec![SelfCheckItemView {
                    name: "venue_balance".into(),
                    ok: false,
                    detail: "HTTP 401 unauthorized".into(),
                }],
            }),
            trading_frozen: Some(TradingFrozenView {
                active: true,
                reason: Some("5 consecutive venue rejections; last: auth failed".into()),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn snapshot_json(s: &UiSnapshot) -> serde_json::Value {
    let body = render_json_full(s, None, None);
    serde_json::from_str(&body).expect("/api/snapshot is JSON")
}

/// Every key the panel's banner block reads, in the spelling the panel reads it.
#[test]
fn the_snapshot_forwards_the_trading_safety_block() {
    let doc = snapshot_json(&frozen_snapshot());
    let st = &doc["stats"];

    assert_eq!(st["books"], 12, "the counters beside them are unchanged");
    assert_eq!(st["venueRejected"], 5, "refusal count reaches the panel");
    // issue #1199 R2: the empty-database fact rides the same block — `false`
    // on the default view (no claim), and a `true` view round-trips it (the
    // panel agent reads this key).
    assert_eq!(st["dbEmpty"], false, "the default view makes no claim");
    assert_eq!(st["lastError"]["tsMs"], 1_000_000_000_000i64);
    assert_eq!(st["lastError"]["code"], "NOT_AUTHENTICATED");
    assert_eq!(st["lastError"]["message"], "auth failed");
    assert_eq!(
        st["lastVenueError"]["message"], "NotAuthenticated: auth failed",
        "the legacy spelling is still forwarded for a panel that has not migrated"
    );
    assert_eq!(st["reconcile"]["consecutiveSweepFailures"], 2);
    assert_eq!(st["reconcile"]["freezeThreshold"], 3);
    assert_eq!(st["selfCheck"]["ok"], false);
    assert_eq!(st["selfCheck"]["items"][0]["name"], "venue_balance");
    assert_eq!(
        st["selfCheck"]["items"][0]["detail"],
        "HTTP 401 unauthorized"
    );
    assert_eq!(st["tradingFrozen"]["active"], true);
    assert_eq!(
        st["tradingFrozen"]["reason"],
        "5 consecutive venue rejections; last: auth failed"
    );
}

/// The reason key is omitted, not nulled, while trading is live — the kernel's
/// own shape, and what the panel's `reason || '交易已被冻结'` fallback reads.
#[test]
fn a_live_kernel_renders_no_banner() {
    let s = UiSnapshot {
        connected: true,
        stats: Some(EngineStatsView {
            trading_frozen: Some(TradingFrozenView {
                active: false,
                reason: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let doc = snapshot_json(&s);
    let st = &doc["stats"];

    assert_eq!(st["tradingFrozen"]["active"], false);
    assert!(
        st["tradingFrozen"].get("reason").is_none(),
        "no reason key while trading is live, got: {}",
        st["tradingFrozen"]
    );
    assert!(st["lastError"].is_null(), "nothing has failed");
    assert!(st["selfCheck"].is_null(), "no self-check has run");
    assert!(st["lastVenueError"].is_null());
    assert_eq!(st["venueRejected"], 0);
    assert_eq!(st["dbEmpty"], false, "no claim on a live default view");
}

/// issue #1199 R2: a `true` empty-database view round-trips through the
/// snapshot — the panel agent's source for the empty-database banner.
#[test]
fn an_empty_database_view_round_trips_through_the_snapshot() {
    let s = UiSnapshot {
        connected: true,
        stats: Some(EngineStatsView {
            db_empty: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let doc = snapshot_json(&s);
    assert_eq!(doc["stats"]["dbEmpty"], true);
}

/// An older core: the view carries only what the old keys provided, and the
/// snapshot renders `null` for the rest. The panel reads that as "no banner" —
/// the graceful degradation this fix must not break in either direction.
#[test]
fn an_old_core_renders_nulls_and_the_panel_shows_no_banner() {
    let s = UiSnapshot {
        connected: true,
        // What `IpcClient::stats` produces from a core whose payload has only
        // the ten original keys.
        stats: Some(EngineStatsView {
            books: 1,
            ..Default::default()
        }),
        ..Default::default()
    };
    let doc = snapshot_json(&s);
    let st = &doc["stats"];

    assert_eq!(st["books"], 1);
    assert!(st["venueRejected"].is_number(), "the counter degrades to 0");
    assert!(st["lastError"].is_null());
    assert!(st["lastVenueError"].is_null());
    assert!(st["selfCheck"].is_null());
    assert!(st["tradingFrozen"].is_null());
    assert!(st["reconcile"].is_null());
}

// ── §6 结构体键 ⊆ 网关输出键 ────────────────────────────────────────────
//
// "结构体收了、网关没转发" 的缺陷：`EngineStatsView` 承载了字段，`render.rs`
// 的 `stats` 块却没把它写进 `/api/snapshot`。前端据此读空、安全 banner
// 不亮，而仅过 struct 的测试看不见这个断层。本条测试在 CI 里把它变成
// 红线：每条已转发字段的 camelCase 键都必须出现在 output["stats"] 里。

/// `render.rs` 的 `stats` 块实际转发到 `/api/snapshot` 的 camelCase 键。
/// 新增转发字段时请同步加入本表，否则新字段的“网关没转发”不会被本条
/// CI 断言捕获。
const FORWARDED_STATS_KEYS: &[&str] = &[
    "books",
    "tops",
    "spots",
    "rounds",
    "evaluations",
    "signals",
    "placeRejected",
    "confirmed",
    "venueRejected",
    "lastError",
    "lastVenueError",
    "reconcile",
    "tradingFrozen",
    "accountGates",
    "strategies",
    "dbEmpty",
    "settlement",
    "dailyLoss",
    "archive",
    "selfCheck",
];

/// 豁免表：结构体里携带、但当前**不**在网关输出里的字段。
///
/// 每一条都是“还在漏”的证据——有意为之（暂无前端消费者），故**不**
/// 纳入 `FORWARDED_STATS_KEYS ⊆ 输出键` 的红线，也不断言输出里缺
/// 它们：若某天字段被转发了，请把它从本表移出并加入
/// `FORWARDED_STATS_KEYS`，让覆盖断言自己收紧一格，而不要默默扩大
/// 豁免。
const EXEMPT: &[(&str, &str)] = &[
    (
        "illegalPriceRejected",
        "#406 入口价拒绝计数：内核发、struct 承载，但当前没有前端消费者，\
         render.rs 暂不转发；不作为覆盖红线。",
    ),
    (
        "blocked",
        "Gate 计数 tallies（`BlockedCounters`）：前端已通过 `accountGates`\
         读取 per-account 闸状态，该汇总计数暂无前端消费者，render.rs\
         暂不转发；不作为覆盖红线。",
    ),
];

/// 全字段置满的 EngineStatsView：全部可选字段为 Some（含
/// settlement/daily_loss/archive/account_gates/db_empty）；豁免表里的两字段
/// 亦置非缺省值，作为“struct 里有、网关输出里没有”的当前快照。
fn full_stats_view() -> EngineStatsView {
    // 优先反序列化内核干栈快照 fixture；该 fixture 缺失（或反序列化
    // 失败）时，退回在测试内直接构造，保证测试在任意检出上都可跑。
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../core/blitzkrieg_core/tests/fixtures/engine_stats_dry_frozen.json",
    );
    if let Ok(text) = std::fs::read_to_string(fixture) {
        if let Ok(view) = serde_json::from_str::<EngineStatsView>(&text) {
            return view;
        }
    }
    EngineStatsView {
        books: 7,
        tops: 3,
        spots: 9,
        rounds: 41,
        evaluations: 1_234,
        signals: 55,
        place_rejected: 2,
        illegal_price_rejected: 1, // EXEMPT
        blocked: BlockedCounters {
            timing: 4,
            momentum: 1,
        }, // EXEMPT
        confirmed: vec!["0123456789abcdef".into()],
        strategies: vec![StrategyStatsRow {
            name: "grid_btc_usdt".into(),
            enabled: true,
            source: "kernel".into(),
            orders_placed: 17,
            orders_rejected: 2,
            limit_rejected: 0,
            blocked_timing: 0,
            blocked_momentum: 0,
            gate_exempted_timing: 0,
            gate_exempted_momentum: 0,
            closed_trades: 6,
            wins: 4,
            losses: 2,
            net_pnl_usd: 12.5,
            rejection_causes: None,
            gate_exemptions: vec![],
        }],
        venue_rejected: 3,
        last_error: Some(LastErrorView {
            ts_ms: 1_000_000_000_000,
            code: "RISK_REJECTED".into(),
            message: "daily loss limit tripped".into(),
        }),
        last_venue_error: Some(LegacyVenueErrorView {
            ts_ms: 1_000_000_000_000,
            message: "RISK_REJECTED: daily loss limit tripped".into(),
        }),
        reconcile: Some(ReconcileView {
            consecutive_sweep_failures: 1,
            freeze_threshold: 3,
        }),
        settlement: SettlementView {
            receivable_usd: Some(42.5),
            fallback_receivable_usd: Some(10.0),
            pending_fallback_claims: 1,
            oldest_fallback_claim_age_ms: Some(1_700_000_000_000.0),
            unverified_expiry_policy: "receivable".into(),
        },
        daily_loss: DailyLossView {
            account: "acc-1".into(),
            day_index: Some(0),
            realized_pnl_usd: -150.0,
            limit_usd: -200.0,
            opening_equity_usd: 10_000.0,
            equity_basis: Some("seed".into()),
            tripped: true,
            tripped_at_ms: Some(1_000_000_000_000),
            any_tripped: true,
            accounts: vec![DailyLossAccountView {
                account: "acc-1".into(),
                realized_pnl_usd: -150.0,
                tripped: true,
                day_index: Some(0),
                reset_at_ms: Some(1_000_086_400_000),
            }],
            suppressed_stops: 0,
        },
        archive: Some(ArchiveView {
            path: "/data/archive/events.jsonl".into(),
            events: 1_000,
            bytes: 64_000,
            dropped: 0,
            recording: true,
            rotate_bytes: 1_000_000,
            segment_bytes: 250_000,
            segments: 1,
            free_bytes: 10_000_000_000,
            stopped_reason: None,
        }),
        self_check: Some(SelfCheckView {
            ok: true,
            ts_ms: 1_000_000_000_000,
            items: vec![SelfCheckItemView {
                name: "venue_balance".into(),
                ok: true,
                detail: "ok".into(),
            }],
        }),
        trading_frozen: Some(TradingFrozenView {
            active: false,
            reason: None,
        }),
        account_gates: vec![AccountGateView {
            account: "acc-1".into(),
            blocked: true,
            reason: Some("daily loss tripped".into()),
            reason_kind: Some("dailyLoss".into()),
            since_ms: Some(1_000_000_000_000),
            expires_at_ms: None,
            actor: Some("kernel".into()),
        }],
        db_empty: true,
    }
}

/// §6：每条已转发字段的 camelCase 键都必须出现在 output["stats"] 里。
#[test]
fn snapshot_stats_keys_are_subset_of_output_keys() {
    let s = UiSnapshot {
        connected: true,
        stats: Some(full_stats_view()),
        ..Default::default()
    };
    let doc = snapshot_json(&s);
    let st = &doc["stats"];

    // 豁免表与转发清单互斥：豁免的是“暂不转发”的字段，不得同时出现在
    // 两边；否则覆盖断言会默默放过一条该被锁死的键。
    for (key, _) in EXEMPT {
        assert!(
            !FORWARDED_STATS_KEYS.contains(key),
            "EXEMPT 字段 `{key}` 不得同时列在 FORWARDED_STATS_KEYS 里",
        );
    }

    // 结构体键 ⊆ 输出键：每条已转发字段的 camelCase 键都必须出现在 stats 块里。
    for key in FORWARDED_STATS_KEYS {
        assert!(
            st.get(*key).is_some(),
            "转发字段 `{key}` 缺失于 /api/snapshot output[\"stats\"]：\
             EngineStatsView 里有该字段，render.rs 却没把它写进 wire \
            （“结构体收了、网关没转发”）。got: {st}",
        );
    }

    // 逆向 spot check：置满的可选字段确实随快照到达，非 null。
    assert_eq!(st["dbEmpty"], true, "db_empty 须随快照到达");
    assert_eq!(
        st["settlement"]["pendingFallbackClaims"], 1,
        "settlement 块须整块转发"
    );
    assert_eq!(st["dailyLoss"]["tripped"], true, "dailyLoss 块须整块转发");
    assert_eq!(
        st["dailyLoss"]["accounts"][0]["tripped"], true,
        "dailyLoss.accounts 行须转发"
    );
    assert_eq!(st["archive"]["recording"], true, "archive 块须整块转发");
    assert_eq!(
        st["accountGates"][0]["account"], "acc-1",
        "accountGates 数组须转发"
    );
    assert_eq!(
        st["accountGates"][0]["blocked"], true,
        "accountGates 条目须转发"
    );
    assert_eq!(
        st["reconcile"]["consecutiveSweepFailures"], 1,
        "reconcile 块须转发"
    );
    assert_eq!(st["tradingFrozen"]["active"], false, "tradingFrozen 须转发");
    assert_eq!(st["selfCheck"]["ok"], true, "selfCheck 须转发");
    assert_eq!(st["venueRejected"], 3, "venueRejected 须转发");
    assert_eq!(st["placeRejected"], 2, "placeRejected 须转发");
}
