#![cfg(test)]

//! #684 两层修复：LIVE 模式下持有到期的轮次持仓永远无法结算 —— 场地的
//! updown 列表只载未来 ~20h 的未到期轮次，已过期 conditionId 永远不会出现
//! 在结算应答里。
//!
//! 第一层（内核自动）：查询零应答且过期超过 [`EXPIRY_FALLBACK_AFTER_MS`]
//! （2h）→ 按兜底价（最近已知 mid，为 0 则入场价=成本回收）平仓入历史，
//! trade record 带 `unverifiable_expiry` 标签，**不建 redemption claim** ——
//! 裁决未经验证，赎回一个发明出来的赢家是错的；场地现金真值由既有
//! reconcile/realign sweep 跟平。
//!
//! 第二层（操作员）：`positions.force_close` —— 持仓存在 + 已过期 +
//! manual-ops 审计路径三道门；审计行、warn、RiskAlert；同样不建 claim。

use super::*;
use crate::model::{FillPolicy, OrderStatus, Side};
use crate::position::OpenParams;
use rust_decimal_macros::dec;

/// 900s 网格上 slot 1 的终点：`place` 在 now=1、round_slot=1 落进
/// [900s, 1800s)，持仓到期即此 —— 所有判定时刻都锚在这个真实网格值上。
const EXPIRES_MS: i64 = 1_800_000;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bk-expiry-fallback-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 一个 LIVE 形态的 dry 核（dry 让 tick 自驱结算 pass；判定逻辑两层共享），
/// 持久日志全部落私有目录，manual-ops 审计可选 —— 第二层测试用它。
fn fallback_core(dir: &std::path::Path, with_audit: bool) -> Core {
    let path = |name: &str| Some(dir.join(name).to_string_lossy().to_string());
    let mut c = Core::new(CoreConfig {
        mode: Mode::Dry,
        round_duration_sec: 900,
        risk: crate::risk::RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        position_log_path: path("positions.jsonl"),
        trade_log_path: path("trades.jsonl"),
        settlement_log_path: path("settlements.jsonl"),
        order_log_path: path("orders.jsonl"),
        manual_ops_audit_path: with_audit.then(|| path("manual-ops.jsonl")).flatten(),
        ..Default::default()
    });
    c.set_balance(dec!(1000));
    c
}

fn entry_order(price: Decimal, size: Decimal) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price,
        size,
        internal_key: "k1".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

/// 开一个 5 股 @0.40 的持仓，市场在 [`EXPIRES_MS`] 过期，入场时现货记录在案。
/// 返回开好的持仓快照。
fn open_expiring_position(c: &mut Core) -> crate::position::OpenPosition {
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let (_id, status) = c.place(entry_order(dec!(0.40), dec!(5)), 0, 1).unwrap();
    assert_eq!(status, OrderStatus::Filled);
    let p = c.positions().open_positions()[0].clone();
    assert_eq!(p.expires_at_ms, EXPIRES_MS);
    p
}

/// 把 watch 表直接注入一条「已发出、零应答、first_sent_ms = sent」的查询。
/// 不走 take_queries 的真实节拍（那是用例 7 的活），判定用的可观测状态
/// 与线上完全一致：answers == 0 && first_sent_ms > 0。
fn stage_unanswered_query(c: &mut Core, condition_id: &str, token_id: &str, first_sent_ms: i64) {
    let positions: Vec<crate::position::OpenPosition> = c
        .positions()
        .open_positions()
        .iter()
        .filter(|p| p.condition_id == condition_id)
        .cloned()
        .collect();
    assert!(!positions.is_empty(), "fixture needs the position first");
    c.settlement.track_markets(&positions, first_sent_ms);
    c.settlement.take_queries(first_sent_ms);
    // take_queries stamps first_sent_ms = first_sent_ms exactly (fresh query);
    // unanswered_since needs now > first_sent, hence the +1.
    let unanswered = c.settlement.unanswered_since(0, first_sent_ms + 1);
    assert!(
        unanswered.iter().any(|(cid, _)| cid == condition_id),
        "fixture: the query must stage as unanswered: {unanswered:?}"
    );
    let _ = token_id;
}

/// 用例 1：过期超过 2h + 查询零应答 → 兜底平仓，trade record 带
/// `unverifiable_expiry`，无 redemption claim，审计锚全程为真。
#[test]
fn an_expired_never_answered_position_falls_back_closed() {
    let dir = scratch("fallback-closes");
    let mut c = fallback_core(&dir, false);
    open_expiring_position(&mut c);

    // 过期瞬间 watch 接管（真路径），查询在过期后 100ms 首次发出、零应答。
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);
    assert_eq!(c.positions().open_positions().len(), 1, "still held");

    // 2h 静默界限之后的第一拍：兜底 sweep 平仓。
    let bound = sent + super::EXPIRY_FALLBACK_AFTER_MS;
    c.tick(bound + 1_000).unwrap();

    assert!(
        c.positions().open_positions().is_empty(),
        "the ghost position is closed by the fallback sweep"
    );
    let closed = &c.positions().closed_positions()[0];
    // 标签：不是 settlement —— 没有裁决就永远不能读成裁决。
    assert_eq!(
        closed.exit_reason,
        crate::model::ExitReason::UnverifiableExpiry,
        "the record must carry the unverifiable-expiry label"
    );
    // 兜底价 = 最后已知 mid（入场后没有新报价 → 入场价 0.40 = 成本回收）。
    assert_eq!(closed.exit_price, dec!(0.40));
    // 无 redemption claim：结算簿应收为 0、无待赎回。
    let stats = c.engine_stats_at(bound + 2_000)["settlement"].clone();
    assert_eq!(
        stats["receivableUsd"],
        serde_json::json!(0.0),
        "no claim may exist behind an unverified close"
    );
    assert_eq!(stats["pendingRedemptions"], serde_json::json!(0));
    assert!(c.take_pending_redemptions(i64::MAX / 2).is_empty());
    // 结算日志（journal）里没有为它记过 settlement —— book 从未被调用。
    let journal = std::fs::read_to_string(dir.join("settlements.jsonl")).unwrap_or_default();
    assert!(
        !journal.contains("\"kind\":\"settlement\""),
        "the fallback must never book a settlement: {journal}"
    );
    // trade record 自带标签（面板/审计读的就是这一行）。
    let trades = c.recent_trades(10);
    assert_eq!(trades.len(), 1);
    assert_eq!(
        trades[0]["exitReason"],
        serde_json::json!("unverifiable_expiry")
    );
    // 记账恒等式在兜底平仓后依然成立。
    let audit = c.run_accounting_audit(bound + 3_000);
    assert!(audit.ok, "audit after fallback close: {}", audit.summary());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 用例 2：未到 2h 界限 → 不平。等待仍是正确姿态（场地理论上还能裁决）。
#[test]
fn inside_the_silence_bound_the_position_stays_open() {
    let dir = scratch("inside-bound");
    let mut c = fallback_core(&dir, false);
    open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);

    // 界限前 1s：什么都不发生。
    c.tick(sent + super::EXPIRY_FALLBACK_AFTER_MS - 1_000)
        .unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "before the bound the fallback must not fire"
    );
    assert!(c.positions().closed_positions().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 用例 3：场地应答过（answers > 0）即使过期很久 → 不平（还在诚实等
/// resolved=true）。"not resolved yet" 的应答也算数 —— 场地活着。
#[test]
fn an_answered_market_is_never_fallback_closed() {
    let dir = scratch("answered-stays");
    let mut c = fallback_core(&dir, false);
    open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);

    // 场地应答过一次（未到期，resolved=false —— 正是 updown 载未来轮次的
    // 场地会给出的诚实回答）。answers 从 0 变 1。
    c.settlement.note_answer("cond", false, sent + 100);
    assert!(c.settlement.unanswered_since(0, sent + 200).is_empty());

    // 远超 2h：只有零应答的 condition 才进兜底，这里必须原地不动。
    c.tick(sent + super::EXPIRY_FALLBACK_AFTER_MS + 60_000)
        .unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "an answered market keeps waiting for its verdict — the fallback never touches it"
    );
    assert!(c.positions().closed_positions().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 用例 4：兜底后 settlement.tracked_markets() 归零 —— 最后一个持仓离场，
/// watch 通过 sync_settlement_watch 自然忘掉该市场。
#[test]
fn after_the_fallback_the_market_leaves_the_watch_list() {
    let dir = scratch("watch-forgets");
    let mut c = fallback_core(&dir, false);
    open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", "tok", sent);
    let stats = c.engine_stats_at(sent + 100)["settlement"].clone();
    assert_eq!(
        stats["trackedMarkets"],
        serde_json::json!(1),
        "precondition: watched"
    );

    let bound = sent + super::EXPIRY_FALLBACK_AFTER_MS;
    c.tick(bound + 1_000).unwrap();
    assert!(c.positions().open_positions().is_empty());
    let stats = c.engine_stats_at(bound + 2_000)["settlement"].clone();
    assert_eq!(
        stats["trackedMarkets"],
        serde_json::json!(0),
        "the watch forgets a market with nothing left to settle"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 用例 5：`positions.force_close` 三道门 —— 过期持仓平 + 审计行存在 +
/// RiskAlert；活轮持仓拒；无审计路径拒。平仓同样不建 claim。
#[test]
fn force_close_gates_and_audit() {
    let dir = scratch("force-close");
    let audit = dir.join("manual-ops.jsonl");

    // (a) 无审计路径 → 拒（回测/进程内测试不下药）。
    let mut c = fallback_core(&dir, false);
    let p = open_expiring_position(&mut c);
    let err = c
        .force_close_expired_position(&p.id, "uid:op", EXPIRES_MS + 1_000)
        .expect_err("no audit path = refused");
    assert_eq!(err.code, CoreErrorCode::InvalidParams, "{err}");
    assert!(err.message.contains("no manual-ops audit path"), "{err}");
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "refusal moves nothing"
    );
    drop(c);

    // (b) 活轮持仓 → 拒（那是 positions.exit 的活）。
    let mut c = fallback_core(&dir, true);
    let p = open_expiring_position(&mut c);
    let err = c
        .force_close_expired_position(&p.id, "uid:op", 1_500)
        .expect_err("a live round is refused");
    assert_eq!(err.code, CoreErrorCode::InvalidOrderState, "{err}");
    assert!(err.message.contains("still live"), "{err}");
    assert!(
        err.message.contains("positions.exit"),
        "the refusal points at the right verb"
    );
    assert_eq!(c.positions().open_positions().len(), 1);

    // (c) 过期持仓 + 审计路径在 → 平。兜底价 = 最后 mid；无 claim；审计行在。
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    let closed = c
        .force_close_expired_position(&p.id, "uid:op", EXPIRES_MS + 1_000)
        .expect("expired + audited = closed");
    assert_eq!(
        closed.exit_reason,
        crate::model::ExitReason::UnverifiableExpiry
    );
    assert_eq!(
        closed.exit_price,
        dec!(0.40),
        "last known mid (the entry book)"
    );
    assert!(c.positions().open_positions().is_empty());
    assert!(
        c.take_pending_redemptions(EXPIRES_MS + 1_100).is_empty(),
        "no claim is booked"
    );
    assert_eq!(
        c.engine_stats_at(EXPIRES_MS + 1_200)["settlement"]["receivableUsd"],
        serde_json::json!(0.0)
    );
    // 审计行：op/actor/position/价格全名。
    let text = std::fs::read_to_string(&audit).unwrap();
    let rec: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(rec["op"], serde_json::json!("positions.force_close"));
    assert_eq!(rec["actor"], serde_json::json!("uid:op"));
    assert_eq!(rec["positionId"], serde_json::json!(closed.id));
    assert_eq!(rec["conditionId"], serde_json::json!("cond"));
    assert_eq!(rec["exitReason"], serde_json::json!("unverifiable_expiry"));
    // 操作员听得见响（RiskAlert）。
    let mut alerts = 0;
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            assert!(message.contains("force-closed"), "{message}");
            alerts += 1;
        }
    }
    assert!(alerts >= 1, "the operator hears the manual close");
    // 记账恒等式成立。
    let report = c.run_accounting_audit(EXPIRES_MS + 2_000);
    assert!(report.ok, "audit after force-close: {}", report.summary());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 用例 6：幂等 —— 已平仓的 position_id 再调 force_close → 拒
/// （InvalidParams），不重复记账（只有一条 trade record）。
#[test]
fn force_close_is_idempotent_on_an_already_closed_position() {
    let dir = scratch("force-close-idem");
    let mut c = fallback_core(&dir, true);
    let p = open_expiring_position(&mut c);
    c.force_close_expired_position(&p.id, "uid:op", EXPIRES_MS + 1_000)
        .expect("first close lands");
    assert_eq!(c.positions().closed_positions().len(), 1);

    let err = c
        .force_close_expired_position(&p.id, "uid:op", EXPIRES_MS + 2_000)
        .expect_err("a second close is refused");
    assert_eq!(err.code, CoreErrorCode::InvalidParams, "{err}");
    assert!(err.message.contains("no open position"), "{err}");
    assert_eq!(
        c.positions().closed_positions().len(),
        1,
        "no double booking"
    );
    assert_eq!(c.trade_summary(None)["totalTrades"], serde_json::json!(1));
    // 未知 id 同拒。
    let err = c
        .force_close_expired_position("hft-ghost", "uid:op", EXPIRES_MS + 3_000)
        .expect_err("unknown id is refused");
    assert_eq!(err.code, CoreErrorCode::InvalidParams);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 补充：兜底价的第二档 —— 持仓快照的标记价为 0（恢复自一份喂死时的
/// positions.jsonl 快照；F6 语义下活路径永不把 stale 标记打成 0，所以这一档
/// 只能从恢复形态抵达）时回退入场价（成本回收）。
#[test]
fn the_zero_mid_fallback_recovers_cost_via_the_entry_price() {
    let dir = scratch("zero-mid");
    let mut c = fallback_core(&dir, false);
    let p = open_expiring_position(&mut c);
    // 一份喂死时落盘的快照：current_price = 0，flows 已记录开仓现金流。
    let mut dead = p.clone();
    dead.current_price = dec!(0);
    dead.prev_price = dec!(0);
    c.positions.restore_open(vec![dead]);
    assert_eq!(
        c.positions().open_positions()[0].current_price,
        dec!(0),
        "precondition: the snapshot carries a dead mark"
    );
    // 真路径：过期后 watch 接管、查询打点。
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    assert_eq!(c.settlement.unanswered_since(0, sent + 100).len(), 1);

    let bound = sent + super::EXPIRY_FALLBACK_AFTER_MS;
    c.tick(bound + 1_000).unwrap();
    assert!(c.positions().open_positions().is_empty());
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(
        closed.exit_reason,
        crate::model::ExitReason::UnverifiableExpiry
    );
    assert_eq!(
        closed.exit_price,
        dec!(0.40),
        "mark 0 falls back to the entry price (cost recovery)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// OpenParams 构造的补充面：test-seam 开出的旧快照（entered > expires +
/// EXPIRY_TRUST_MS）不进 watch（track_markets 拒收），因此兜底也永远不碰它
/// —— 那条路是 #175 的发明防护，本修复不越它半步。
#[test]
fn an_untrackable_position_is_outside_the_fallback() {
    let dir = scratch("untrackable");
    let mut c = fallback_core(&dir, false);
    c.positions.open(
        OpenParams {
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: crate::model::SignalDirection::Up,
            token_id: "tok".into(),
            condition_id: "cond".into(),
            account_id: crate::model::default_account_id(),
            venue: String::new(),
            partner_token_id: String::new(),
            entry_price: dec!(0.40),
            // expires 在入场前 10 分钟 —— 不是市场终点（EXPIRY_TRUST_MS 防护）。
            expires_at_ms: 1_000,
            was_maker: false,
            target_exit_price: None,
            round_duration_sec: 0,
        },
        700_000,
    );
    c.tick(3_000_000).unwrap();
    assert_eq!(
        c.settlement.tracked_markets(),
        0,
        "the untrackable position never enters the watch"
    );
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "and the fallback never closes what it does not watch"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
