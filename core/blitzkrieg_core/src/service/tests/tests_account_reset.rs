#![cfg(test)]

use super::*;
use rust_decimal_macros::dec;

// ── issue 561: 账户维重置隔离（重置 B 不动其他账户）────────────────────

/// 账户维重置的双账户 fixture：default + paper 两个账簿、私有记录文件
/// 与审计路径（与上方 reset 测试家族同一 cwd 隔离纪律）。返回
/// (core, scratch dir) —— 调用方负责清理 dir。
fn two_account_reset_fixture(tag: &str) -> (Core, std::path::PathBuf) {
    let dir = reset_audit_dir(tag);
    let log = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());
    let book = crate::account::AccountLedgers::new(
        vec![
            crate::account::Account {
                id: crate::account::AccountId::from("default"),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
            crate::account::Account {
                id: crate::account::AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("default"),
    );
    let c = Core::with_account_book(
        CoreConfig {
            mode: Mode::Dry,
            manual_ops_audit_path: Some(
                dir.join("manual-ops.jsonl").to_string_lossy().into_owned(),
            ),
            trade_log_path: log("trades.jsonl"),
            order_log_path: log("orders.jsonl"),
            position_log_path: log("positions.jsonl"),
            dry_seed_balance: Decimal::from(1_000),
            ..Default::default()
        },
        book,
    );
    (c, dir)
}

/// 场地现金真值必须落进绑定该 venue 的账户，而不是「active 账户」——
/// 单账户时代的路由曾把 PolySimulator 的 100 USDC 写到 default 账户上
/// （default 真 13.59 被冲掉，poly 面板还停在 journal 回放的 1000）。
#[tokio::test]
async fn venue_cash_realign_routes_to_the_bound_account_not_active() {
    let (mut c, dir) = two_account_reset_fixture("venue-cash-route");
    // paper 账户绑 poly 模拟盘（journal 回放出的练习金）；default 是
    // active、揣着自己的真钱。
    c.accounts
        .set_venue(
            &crate::account::AccountId::from("paper"),
            "polysimulator",
            0,
        )
        .expect("venue binding lands");
    c.accounts
        .get_mut(&crate::account::AccountId::from("paper"))
        .unwrap()
        .set_balance(Decimal::from(1_000));
    c.accounts
        .get_mut(&crate::account::AccountId::from("default"))
        .unwrap()
        .set_balance(Decimal::new(1359, 2));
    // PolySimulator 场地报 100：落在 paper 的簿上，default 分文不动。
    c.realign_venue_cash("polysimulator", Decimal::from(100));
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("paper"))
            .unwrap()
            .balance(),
        Decimal::from(100)
    );
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("default"))
            .unwrap()
            .balance(),
        Decimal::new(1359, 2)
    );
    // 无绑定的 venue（多账户部署）：拒绝 + 告警，分文不落任何簿（#646
    // —— 旧 fallback 曾把 7 写进 active 的 default 簿）。
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.realign_venue_cash("ghost-venue", Decimal::from(7));
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("default"))
            .unwrap()
            .balance(),
        Decimal::new(1359, 2),
        "an unbound venue's report must not touch the active book"
    );
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("paper"))
            .unwrap()
            .balance(),
        Decimal::from(100),
        "an unbound venue's report must not touch any sibling book"
    );
    let mut alerts = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            alerts.push(message);
        }
    }
    assert!(
        alerts.iter().any(|m| m.contains("ghost-venue")
            && m.contains("refused")
            && m.contains("no account bound")),
        "the refusal must surface as a risk alert: {alerts:?}"
    );
    // fold 路径同样拒绝（drift killer 的周期性入口）。
    c.fold_venue_free("ghost-venue", Decimal::from(9));
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("default"))
            .unwrap()
            .balance(),
        Decimal::new(1359, 2),
        "the fold path must refuse an unbound venue too"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// #646 回归钉：default 单账户部署（零配置形态 —— accounts.toml 缺失时
/// `load_account_ledgers` 返回的同一本 `single_default` 簿）的行为与改前
/// 逐字节一致：无绑定 venue 的余额上报仍落 default/active 那本唯一的簿，
/// fold 的 drift killer 仍自动校正，且不得触发 #646 的拒绝告警。
#[tokio::test]
async fn single_account_deployment_keeps_the_legacy_venue_cash_fallback() {
    let dir = reset_audit_dir("venue-cash-single-default");
    let log = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());
    let mut c = Core::with_account_book(
        CoreConfig {
            mode: Mode::Live,
            manual_ops_audit_path: Some(
                dir.join("manual-ops.jsonl").to_string_lossy().into_owned(),
            ),
            trade_log_path: log("trades.jsonl"),
            order_log_path: log("orders.jsonl"),
            position_log_path: log("positions.jsonl"),
            dry_seed_balance: Decimal::from(1_000),
            ..Default::default()
        },
        crate::account::AccountLedgers::single_default(0),
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    // 无绑定的 venue 上报：单账户形态回落 active（= default = 唯一簿），
    // 与旧 fallback 落点相同；venue_free 槽照旧入账（Live 分支原样）。
    c.realign_venue_cash("polysimulator", Decimal::from(100));
    assert_eq!(c.ledger().balance(), Decimal::from(100));
    // fold drift killer：无 resting、余额对不上时仍自动校正（旧语义）。
    c.fold_venue_free("polysimulator", Decimal::from(90));
    assert_eq!(c.ledger().balance(), Decimal::from(90));
    // 全程零告警 —— 拒绝路径在单账户形态下绝不触发。
    let mut alerts = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            alerts.push(message);
        }
    }
    assert!(
        alerts.is_empty(),
        "the single-account fallback must stay silent: {alerts:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}
/// 场地报 100、active 是 default（13.59），单槽 venue_free 让
/// `venue_free_cash` 检查比出 drift 86.403094（= 100 − 13.59，逐位吻合）
/// 并封锁新开仓。分槽后兄弟 venue 的现金不再是这本书的「真相」。
#[test]
fn audit_venue_report_scopes_to_the_active_accounts_venue() {
    let dir = reset_audit_dir("audit-venue-scope");
    let log = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());
    let book = crate::account::AccountLedgers::new(
        vec![
            crate::account::Account {
                id: crate::account::AccountId::from("default"),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: "polymarket".into(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
            crate::account::Account {
                id: crate::account::AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: "polysimulator".into(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("default"),
    );
    let mut c = Core::with_account_book(
        CoreConfig {
            mode: Mode::Live,
            manual_ops_audit_path: Some(
                dir.join("manual-ops.jsonl").to_string_lossy().into_owned(),
            ),
            trade_log_path: log("trades.jsonl"),
            order_log_path: log("orders.jsonl"),
            position_log_path: log("positions.jsonl"),
            dry_seed_balance: Decimal::from(1_000),
            ..Default::default()
        },
        book,
    );
    c.accounts
        .get_mut(&crate::account::AccountId::from("default"))
        .unwrap()
        .set_balance(Decimal::new(1359, 2));
    // poly 场地报 100（route 到 paper 的簿）。
    c.realign_venue_cash("polysimulator", Decimal::from(100));
    // active = default（绑 polymarket）：poly 的报告不可比 —— 无 venue 报告。
    let input = c.build_audit_input(now_ms());
    assert_eq!(
        input.venue_free, None,
        "sibling venue's cash leaked into default's audit"
    );
    // 切 active 到 paper（绑 polysimulator）：poly 的报告可比。
    c.accounts
        .switch(&crate::account::AccountId::from("paper"))
        .expect("switch to paper");
    let input = c.build_audit_input(now_ms());
    assert_eq!(input.venue_free, Some(Decimal::from(100)));
    // polymarket 场地报它自己的真值：只进 default 的槽，paper 的审计仍看 100。
    c.realign_venue_cash("polymarket", Decimal::new(1359, 2));
    let input = c.build_audit_input(now_ms());
    assert_eq!(input.venue_free, Some(Decimal::from(100)));
    let _ = std::fs::remove_dir_all(dir);
}

/// 审计身份（realized/receivable/spent/received/seen_trades）只折 active
/// 账户自己的行——真线事故链的第三处：poly 账户平仓后，全局 summary 的
/// realized 曾把它的盈亏算进 default 的 trade_ledger_identity。
#[test]
fn audit_identity_scopes_to_the_active_account() {
    let (mut c, dir) = two_account_reset_fixture("audit-identity-scope");
    // paper 账户经生产平仓路径实现 -2（TradeDb/结算认领/持仓三处同折）。
    seed_closed_trade(&mut c, "paper", "poly-1", Decimal::from(-2));
    // active = default：paper 的盈亏与认领不得进入 default 的身份。
    let input = c.build_audit_input(now_ms());
    assert_eq!(
        input.identity.realized,
        Decimal::ZERO,
        "sibling realized leaked"
    );
    assert_eq!(
        input.identity.receivable,
        Decimal::ZERO,
        "sibling receivable leaked"
    );
    assert_eq!(input.seen_trades, 0, "sibling trade count leaked");
    // 切 active 到 paper：它自己的行可见（realized = -2）。
    c.accounts
        .switch(&crate::account::AccountId::from("paper"))
        .expect("switch");
    let input = c.build_audit_input(now_ms());
    let expected_realized = f64_to_decimal(
        c.trade_db
            .as_ref()
            .unwrap()
            .summary_for("paper")
            .total_net_pnl,
    );
    assert!(expected_realized < Decimal::ZERO);
    assert_eq!(input.identity.realized, expected_realized);
    assert_eq!(input.seen_trades, 1);
    // receivable 与该账户自己的结算簿一致（认领在，但只属于 paper）。
    assert_eq!(
        input.identity.receivable,
        c.settlement.receivable_usd_for("paper") + c.settlement.merge_receivable_usd_for("paper")
    );
    // default 的结算簿分文未动。
    assert_eq!(c.settlement.receivable_usd_for("default"), Decimal::ZERO);
    let _ = std::fs::remove_dir_all(dir);
}

/// 铺一个已平仓交易到指定账户（生产路径：open → close() →
/// on_position_closed，TradeDb/positions.closed/strategy_accounting 三处
/// 各折一次 —— 与 9833 行注释的幂等契约一致）。
fn seed_closed_trade(c: &mut Core, account: &str, id: &str, net: Decimal) {
    use crate::position::{CashFlows, OpenPosition};
    // net = −8.69 形态：entry 0.5 × 10 股（cost 5），exit 价补足差额。
    let exit_price = Decimal::new(5, 1) + net / Decimal::from(10);
    let held = OpenPosition {
        id: id.into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: crate::model::SignalDirection::Up,
        token_id: "tok".into(),
        condition_id: "cond".into(),
        entry_price: Decimal::ONE / Decimal::from(2),
        current_price: exit_price,
        prev_price: Decimal::ONE / Decimal::from(2),
        shares: Decimal::from(10),
        cost_usd: Decimal::from(5),
        was_maker_entry: true,
        entry_fee_pct: Decimal::ZERO,
        entry_role: crate::model::OrderRole::Maker,
        exit_role: crate::model::OrderRole::Taker,
        target_exit_price: None,
        entered_at_ms: 0,
        expires_at_ms: 0,
        account_id: crate::account::AccountId::from(account),
        venue: "polymarket".into(),
        partner_token_id: String::new(),
        last_book_ts: 0,
        round_duration_sec: 0,
        state: crate::exit_policy::ExitState::new(Decimal::ONE / Decimal::from(2), 0),
        flows: CashFlows::default(),
    };
    c.positions.restore_open(vec![held]);
    let closed = c
        .positions
        .close(
            id,
            exit_price,
            crate::model::ExitReason::TakeProfit,
            false,
            1,
        )
        .expect("the held position closes");
    c.on_position_closed(&closed, 1);
}

/// (a) 隔离性主用例：重置 paper 不动 default 的任何记录 —— 成交史、
/// 策略盈亏切片、余额、closed 内存行全部原样；paper 自身三处归零。
#[test]
fn account_scoped_reset_leaves_other_accounts_untouched() {
    let (mut c, dir) = two_account_reset_fixture("scoped-isolation");
    c.set_balance(Decimal::from(1_000));
    // 两本账各有真实历史：default 赚 7，paper 亏 8.69。
    seed_closed_trade(&mut c, "default", "hft-d1", Decimal::from(7));
    seed_closed_trade(
        &mut c,
        "paper",
        "hft-p1",
        Decimal::from(-8) - Decimal::new(69, 2),
    );
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("paper"))
            .unwrap()
            .balance(),
        Decimal::from(1_000),
        "fixture books start on the seed"
    );

    let r = c
        .reset_trading(
            &crate::ipc::schema::ResetTradingParams {
                confirm: "RESET".into(),
                reason: Some("unit".into()),
                account_id: Some("paper".into()),
            },
            "uid:t",
            3,
        )
        .unwrap();
    assert_eq!(r.account_id, "paper", "the reply names the reset scope");
    assert_eq!(
        r.balances,
        vec![("paper".to_string(), Decimal::from(1_000))],
        "only the target account's balance rides the reply"
    );

    // paper：余额回种子、tradeSummary（账户维读数）归零、closed 内存行清空。
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("paper"))
            .unwrap()
            .balance(),
        Decimal::from(1_000)
    );
    let paper_summary = c.trade_summary(Some("paper"));
    assert_eq!(paper_summary["totalTrades"], serde_json::json!(0));
    assert!(
        c.positions()
            .closed_positions()
            .iter()
            .all(|x| x.account_id.as_str() != "paper"),
        "the in-memory closed rows of the reset account are gone — \
         the 991.31 revival path (issue 561)"
    );
    let paper_stats = c.strategy_stats_for_account("paper");
    assert!(
        paper_stats
            .iter()
            .all(|row| row["closedTrades"] == serde_json::json!(0)),
        "paper's per-account PnL slice is zeroed: {paper_stats:?}"
    );

    // default：一字不动 —— 成交史、策略切片、closed 行、余额。
    assert_eq!(
        c.trade_summary(Some("default"))["totalTrades"],
        serde_json::json!(1),
        "default's trade history survives a paper reset"
    );
    assert_eq!(
        c.trade_summary(Some("default"))["totalNetPnl"],
        serde_json::json!(7.0)
    );
    assert!(
        c.positions()
            .closed_positions()
            .iter()
            .any(|x| x.account_id.as_str() == "default"),
        "default's in-memory closed row survives"
    );
    let default_stats = c.strategy_stats_for_account("default");
    assert!(
        default_stats
            .iter()
            .any(|row| row["closedTrades"] == serde_json::json!(1)),
        "default's PnL slice survives: {default_stats:?}"
    );
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("default"))
            .unwrap()
            .balance(),
        Decimal::from(1_000)
    );
    drop(c);
    let _ = std::fs::remove_dir_all(dir);
}

/// (b) 缺省账户解析：省略 accountId 的重置落在进程 display 格（不经
/// IPC 派发层的直连兜底），回执点名它。
#[test]
fn account_scoped_reset_defaults_to_the_display_account() {
    let (mut c, dir) = two_account_reset_fixture("scoped-default");
    c.set_balance(Decimal::from(1_000));
    seed_closed_trade(&mut c, "default", "hft-d1", Decimal::from(7));

    let r = c.reset_trading(&reset_params("RESET"), "uid:t", 3).unwrap();
    assert_eq!(
        r.account_id, "default",
        "no explicit id = the display account (the process default here)"
    );
    assert_eq!(
        c.trade_summary(Some("default"))["totalTrades"],
        serde_json::json!(0),
        "the display account was wiped"
    );
    drop(c);
    let _ = std::fs::remove_dir_all(dir);
}

/// (c) 未知账户显式拒绝 —— 静默的空清洗比拒绝更糟。
#[test]
fn account_scoped_reset_refuses_an_unknown_account() {
    let (mut c, dir) = two_account_reset_fixture("scoped-unknown");
    c.set_balance(Decimal::from(1_000));
    let err = c
        .reset_trading(
            &crate::ipc::schema::ResetTradingParams {
                confirm: "RESET".into(),
                reason: Some("unit".into()),
                account_id: Some("ghost".into()),
            },
            "uid:t",
            3,
        )
        .expect_err("an unknown scope is refused");
    assert_eq!(err.code, CoreErrorCode::InvalidParams);
    assert!(
        err.message.contains("unknown account"),
        "the refusal names the scope: {err}"
    );
    drop(c);
    let _ = std::fs::remove_dir_all(dir);
}

/// (d) 三道门在账户维同样竖着：显式 accountId + 错误确认词照样红；
/// Live 模式连账户维重置也整个拒绝（不因范围变小而放水）。
#[test]
fn account_scoped_reset_keeps_the_three_gates() {
    let (mut c, dir) = two_account_reset_fixture("scoped-gates");
    c.set_balance(Decimal::from(1_000));
    let err = c
        .reset_trading(
            &crate::ipc::schema::ResetTradingParams {
                confirm: "reset".into(),
                reason: Some("unit".into()),
                account_id: Some("paper".into()),
            },
            "uid:t",
            3,
        )
        .expect_err("the confirm word is scoped too");
    assert_eq!(err.code, CoreErrorCode::InvalidParams);
    drop(c);
    let _ = std::fs::remove_dir_all(dir);

    let mut live = live_core(dec!(10));
    let err = live
        .reset_trading(
            &crate::ipc::schema::ResetTradingParams {
                confirm: "RESET".into(),
                reason: Some("unit".into()),
                account_id: Some("paper".into()),
            },
            "uid:t",
            3,
        )
        .expect_err("live refuses even an account-scoped wipe");
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
}

/// (e) 裁决 3 端到端：挂单 + 已平仓亏损 → 账户维重置 → 「真实余额 =
/// 本金 + 净利」读数回到本金。recent_trades（trade 文件空后兜底读
/// closed 内存行）不再复活已抹掉的净利 —— 991.31 复活链全断。
#[test]
fn account_scoped_reset_breaks_the_balance_revival_chain() {
    let (mut c, dir) = two_account_reset_fixture("scoped-revival");
    c.set_balance(Decimal::from(1_000));
    seed_closed_trade(
        &mut c,
        "default",
        "hft-d1",
        Decimal::from(-8) - Decimal::new(69, 2),
    );
    // The pre-reset panel reading: 1000 + (−8.69) = 991.31.
    let pre = c.trade_summary(Some("default"));
    assert_eq!(pre["totalNetPnl"], serde_json::json!(-8.69));

    c.reset_trading(&reset_params("RESET"), "uid:t", 3).unwrap();

    // The revival chain is cut at every link: the trade file is empty
    // AND the in-memory closed fallback no longer names the account.
    let post = c.trade_summary(Some("default"));
    assert_eq!(
        post["totalNetPnl"],
        serde_json::json!(0.0),
        "net stays zero after the reset — the balance reads seed, not seed + revived net"
    );
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("default"))
            .unwrap()
            .balance(),
        Decimal::from(1_000)
    );
    drop(c);
    let _ = std::fs::remove_dir_all(dir);
}

/// (f) journal 端到端（裁决 3 的 boot 半环）：有持久 journal 的账户被
/// 重置后，磁盘上只剩一行 Reset 行，一次全新回放恢复的 balance == 种子
/// —— 与 ResetTradingResult 自检跨 boot 一致；其他账户的 journal 原样。
#[test]
fn account_scoped_reset_rewrites_only_the_target_journal() {
    let dir = reset_audit_dir("scoped-journal");
    let journal_dir = dir.join("ledger");
    std::fs::create_dir_all(&journal_dir).expect("journal dir");
    let log = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());
    let book = crate::account::AccountLedgers::new(
        vec![
            crate::account::Account {
                id: crate::account::AccountId::from("default"),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
            crate::account::Account {
                id: crate::account::AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("default"),
    );
    let mut c = Core::with_account_book(
        CoreConfig {
            mode: Mode::Dry,
            manual_ops_audit_path: Some(
                dir.join("manual-ops.jsonl").to_string_lossy().into_owned(),
            ),
            trade_log_path: log("trades.jsonl"),
            order_log_path: log("orders.jsonl"),
            position_log_path: log("positions.jsonl"),
            ledger_journal_dir: Some(journal_dir.to_string_lossy().into_owned()),
            dry_seed_balance: Decimal::from(1_000),
            ..Default::default()
        },
        book,
    );
    // Attach journals by hand (the boot path's job): both accounts get a
    // durable trail, default accumulates fills, paper stays on its seed.
    let default_path = journal_dir.join("default.journal.jsonl");
    let paper_path = journal_dir.join("paper.journal.jsonl");
    c.accounts
        .get_mut(&crate::account::AccountId::from("default"))
        .unwrap()
        .set_journal_path(default_path.clone());
    c.accounts
        .get_mut(&crate::account::AccountId::from("paper"))
        .unwrap()
        .set_journal_path(paper_path.clone());
    {
        let l = c
            .accounts
            .get_mut(&crate::account::AccountId::from("default"))
            .unwrap();
        l.seed(Decimal::from(1_000));
        l.settle_buy_fill("o1", Decimal::from(30), Decimal::ZERO);
        l.settle_sell_fill(Decimal::from(21), Decimal::ONE);
    }

    c.reset_trading(
        &crate::ipc::schema::ResetTradingParams {
            confirm: "RESET".into(),
            reason: Some("unit".into()),
            account_id: Some("default".into()),
        },
        "uid:t",
        3,
    )
    .unwrap();

    // default's journal: exactly one Reset row, replay == seed.
    let rows: Vec<crate::ledger::JournalEntry> = crate::jsonl::load(&default_path, "test journal");
    assert_eq!(
        rows.len(),
        1,
        "the reset journal is exactly the reset row: {rows:?}"
    );
    assert_eq!(rows[0].balance_after, Decimal::from(1_000));
    let mut replayed = crate::ledger::Ledger::new();
    replayed.replay_journal(crate::jsonl::load::<crate::ledger::JournalEntry>(
        &default_path,
        "test journal",
    ));
    assert_eq!(
        replayed.balance(),
        Decimal::from(1_000),
        "boot replay over the rewritten journal lands on the seed"
    );
    // paper's journal: untouched by default's reset. The boot's seed_all
    // put a Seed row here (dry 模式整簿注资，回放契约的一部分)；重置只
    // 重写目标账户的文件 —— 这行 Seed 原样留在盘上。
    let paper_rows: Vec<crate::ledger::JournalEntry> =
        crate::jsonl::load(&paper_path, "test journal");
    assert_eq!(
        paper_rows.len(),
        1,
        "a sibling account's journal is not the reset's business (its \
         boot Seed row stays exactly as the boot wrote it)"
    );
    assert_eq!(paper_rows[0].kind, crate::ledger::JournalKind::Seed);
    drop(c);
    let _ = std::fs::remove_dir_all(dir);
}
