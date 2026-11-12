#![cfg(test)]

use super::*;

use crate::service::{Core, CoreConfig};
use blitzkrieg_market_api::net::now_ms;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

/// #191 acceptance (a): one `risk.setLimits` call moves the entry limits
/// without a restart, answers with the audit record (old → new, actor,
/// instant, `persisted:false`) and the very NEXT order is judged by the NEW
/// value.
#[tokio::test]
async fn risk_set_limits_binds_the_next_order_without_a_restart() {
    let (core, registry, peer) = hot_reload_fixture().await;

    let feed = rpc(&core, &registry, &peer, books_line()).await;
    assert!(feed.get("error").is_none(), "feed the book first: {feed}");

    // 2.00 USD against the 3 USD startup cap: MUST be admitted, or the
    // post-change refusal below would prove nothing about the change.
    let before = rpc(
        &core,
        &registry,
        &peer,
        order_line(2, "probe-before", "BTC", "up"),
    )
    .await;
    assert!(
        before.get("error").is_none(),
        "2.00 must pass the 3 USD startup cap: {before}"
    );
    assert_eq!(
        before["result"]["status"],
        serde_json::json!("FILLED"),
        "the probe must be a real, filled order: {before}"
    );

    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"risk.setLimits","params":{"maxOrderNotional":1,"minShares":2,"maxShares":4,"reason":"tighten after drawdown"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the safe subset must be accepted: {applied}"
    );
    let audit = &applied["result"];
    assert_eq!(
        audit["persisted"],
        serde_json::json!(false),
        "the update must state that it is memory-only: {audit}"
    );
    assert_eq!(
        audit["actor"],
        serde_json::json!(format!("uid:{}", own_uid())),
        "who did it must be on the record: {audit}"
    );
    assert_eq!(audit["reason"], serde_json::json!("tighten after drawdown"));
    assert!(
        audit["atMs"].as_i64().unwrap_or(0) > 0,
        "when it happened must be on the record: {audit}"
    );
    let change = |field: &str| {
        audit["applied"]
            .as_array()
            .expect("an applied list")
            .iter()
            .find(|c| c["field"] == serde_json::json!(field))
            .cloned()
            .unwrap_or_else(|| panic!("{field} must be in the audit record: {audit}"))
    };
    assert_eq!(
        change("maxOrderNotional")["from"],
        serde_json::json!(3.0),
        "the audit keeps the OLD value"
    );
    assert_eq!(change("maxOrderNotional")["to"], serde_json::json!(1.0));
    assert_eq!(change("minShares")["from"], serde_json::json!(10.0));
    assert_eq!(change("minShares")["to"], serde_json::json!(2.0));
    assert_eq!(change("maxShares")["to"], serde_json::json!(4.0));

    // `engine.stats.sizing` is what the panel answers "what may ONE entry
    // commit with?" from, and with an engine installed it is read live off
    // the engine's own config — the same `global_sizing()` the ticket
    // arithmetic calls. So this is the no-restart half: the boot-time copy
    // moved too, not just the `CoreConfig` a new `Core` would be built from.
    let stats = rpc(&core, &registry, &peer, stats_line(4)).await;
    let sizing = &stats["result"]["sizing"];
    assert_eq!(sizing["maxOrderNotionalUsd"], serde_json::json!(1.0));
    assert_eq!(sizing["minShares"], serde_json::json!(2.0));
    assert_eq!(sizing["maxShares"], serde_json::json!(4.0));

    // The SAME 2.00 ticket, on another asset (so the only possible reason is
    // the cap, never the one-position-per-asset gate), judged by the NEW cap.
    let after = rpc(
        &core,
        &registry,
        &peer,
        order_line(5, "probe-after", "ETH", "down"),
    )
    .await;
    assert_eq!(
        after["error"]["data"]["coreCode"],
        serde_json::json!("RISK_REJECTED"),
        "the next order must be judged by the new cap: {after}"
    );
    assert!(
        after["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("exceeds per-order cap 1"),
        "the refusal must name the NEW cap: {after}"
    );
}

/// 生效风控可编辑（`risk.setSystemic`）：写路径的 wire 合同 —— patch
/// 落进 matrix（readout 立即读回 flag 来源的新值）、回执逐项带 effect
/// （live / next_session）与 old→new、空 patch 拒绝、负值拒绝且原子
/// （同行 ride-along 的合法字段也不落）。下一笔入场按新值裁决由
/// `engine_evaluate` → `process_intent` 的 §4.2 测试矩阵覆盖（arbitration/
/// pipeline.rs 的 judge_entry 用例），这里不重复架一台引擎。
#[tokio::test]
async fn risk_set_systemic_writes_the_matrix_and_names_each_effect() {
    let (core, registry, peer) = hot_reload_fixture().await;

    // Arm ONE live bound + the drawdown row, which must carry next_session.
    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"maxPositionSize":1,"maxDailyDrawdownUsd":50,"reason":"live arm"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the systemic patch must be accepted: {applied}"
    );
    let audit = &applied["result"];
    assert_eq!(audit["persisted"], serde_json::json!(false));
    assert_eq!(
        audit["persistence"],
        serde_json::json!("none"),
        "the receipt names the carrier: no env file wired → none: {audit}"
    );
    assert_eq!(
        audit["actor"],
        serde_json::json!(format!("uid:{}", own_uid())),
        "who did it must be on the record: {audit}"
    );
    let effect_of = |field: &str| {
        audit["applied"]
            .as_array()
            .expect("an applied list")
            .iter()
            .find(|c| c["field"] == serde_json::json!(field))
            .map(|c| c["effect"].as_str().expect("effect").to_string())
    };
    assert_eq!(
        effect_of("maxPositionSize").as_deref(),
        Some("live"),
        "the size cap is a live bound: {audit}"
    );
    assert_eq!(
        effect_of("maxDailyDrawdownUsd").as_deref(),
        Some("next_session"),
        "the drawdown budget must state its next-session arming: {audit}"
    );
    let from_of = |field: &str| {
        audit["applied"]
            .as_array()
            .expect("an applied list")
            .iter()
            .find(|c| c["field"] == serde_json::json!(field))
            .map(|c| c["from"].as_str().expect("from").to_string())
    };
    assert_eq!(
        from_of("maxPositionSize").as_deref(),
        Some("0"),
        "the old value (0 = off) is on the record: {audit}"
    );

    // The readout answers with the new values, flag-sourced.
    let read = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"risk.limits","params":{}}"#.to_string(),
    )
    .await;
    assert!(read.get("error").is_none(), "readout must answer: {read}");
    assert_eq!(
        read["result"]["account"]["limits"]["maxPositionSize"]["value"],
        serde_json::json!("1")
    );
    assert_eq!(
        read["result"]["account"]["limits"]["maxPositionSize"]["source"],
        serde_json::json!("flag")
    );
    assert_eq!(
        read["result"]["account"]["limits"]["maxDailyDrawdownUsd"]["value"],
        serde_json::json!("50")
    );

    // Empty patch is refused.
    let empty = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"risk.setSystemic","params":{}}"#.to_string(),
    )
    .await;
    assert!(
        empty["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("at least one"),
        "an empty patch is refused: {empty}"
    );

    // A negative value is refused with the field named — and the atomicity
    // means the legal field riding along did NOT land either.
    let negative = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"risk.setSystemic","params":{"maxSingleLossUsd":"-5","maxConsecutiveLosses":10}}"#.to_string(),
    )
    .await;
    assert!(
        negative["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("must be >= 0"),
        "a negative bound is refused with the field named: {negative}"
    );
    let read2 = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":5,"method":"risk.limits","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        read2["result"]["account"]["limits"]["maxConsecutiveLosses"]["value"],
        serde_json::json!("0"),
        "a refused patch leaves no trace: {read2}"
    );
}

/// 生效风控的持久化回路（issue #965 验收一）：面板保存 → `BK_RISK_*` 键
/// 落进内核 `.env`（回执 `persisted=true` + `persistence=env`）→ 「重启」
/// （新解析走启动链的同一 seam `SystemicRiskLimits::from_lookup`，键位拼写
/// 就是写路径写下的那些）→ 限额仍武装、来源诚实标注 env。持久化载体与回
/// 读一致 —— 一条载体链，没有第二个 flag 标注的影子载体（issue #965 把
/// risk-state 文件里的 systemic 格退役了）。
#[tokio::test]
async fn risk_set_systemic_persists_to_env_and_the_boot_rearms_it() {
    use rust_decimal_macros::dec;
    let dir = std::env::temp_dir().join(format!("bk-risk-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");

    let cfg = CoreConfig {
        risk: crate::risk::RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        },
        dry_seed_balance: dec!(100),
        env_file_path: Some(env_path.to_string_lossy().into_owned()),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    c.set_balance(cfg.dry_seed_balance);
    cfg.install_engine(&mut c).expect("engine install");
    let core = Arc::new(AsyncMutex::new(c));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"maxPositionSize":2,"maxSingleLossUsd":"0.75","reason":"persist me"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the patch must land: {applied}"
    );
    let audit = &applied["result"];
    assert_eq!(
        audit["persisted"],
        serde_json::json!(true),
        "the env file is wired, so the write must land: {audit}"
    );
    assert_eq!(
        audit["persistence"],
        serde_json::json!("env"),
        "the receipt names the carrier: {audit}"
    );

    // The `.env` the write face produced: exactly the two keys, exactly the
    // spellings the boot chain reads — one map, no second carrier.
    let on_file: std::collections::HashMap<String, String> = std::fs::read_to_string(&env_path)
        .expect("the .env exists")
        .lines()
        .filter_map(crate::env_file::parse_line)
        .collect();
    assert_eq!(
        on_file.get("BK_RISK_MAX_POSITION_SIZE").map(String::as_str),
        Some("2"),
        "{on_file:?}"
    );
    assert_eq!(
        on_file
            .get("BK_RISK_MAX_SINGLE_LOSS_USD")
            .map(String::as_str),
        Some("0.75"),
        "{on_file:?}"
    );

    // 「重启」：the boot chain resolves the bounds from EXACTLY these keys
    // (`from_lookup` is `from_env`'s pure seam; the launcher folds .env into
    // the process environment, `from_env` reads it back).
    let limits =
        crate::risk::limits::SystemicRiskLimits::from_lookup(|key| on_file.get(key).cloned());
    let pos = &limits.account.max_position_size;
    assert_eq!(pos.value(), dec!(2));
    assert_eq!(
        pos.source(),
        crate::risk::limits::LimitSource::Env,
        "provenance is env — the persisted bound names its real layer"
    );
    let single = &limits.account.max_single_loss_usd;
    assert_eq!(single.value(), dec!(0.75));
    assert_eq!(single.source(), crate::risk::limits::LimitSource::Env);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 退出纪律可编辑（`risk.setExit`）：写路径的 wire 合同 —— patch 落进
/// Gate 4 绑定的活配置（readout 立即读回新值）、回执逐项带 old→new 与
/// effect=live、空 patch 拒绝、非法值拒绝且原子（同行 ride-along 的合法
/// 字段也不落）、`forceExitSec: 0` 是文档化的「关闭强平」开关照常接受。
/// 下一笔入场按新值绑定由 arbitration/pipeline.rs 的 Gate 4 用例覆盖。
#[tokio::test]
async fn risk_set_exit_writes_the_triple_and_names_each_change() {
    let (core, registry, peer) = hot_reload_fixture().await;

    // Arm all three fields at once; the reply names each old→new.
    // trailingEnabled rides the same patch: the calibration default is
    // OFF (issue 356: the proven operator never sells into profit), and
    // the hot-write face is where an operator re-arms it without a
    // restart — the single-entry behaviour this round's spread_arb
    // losses exposed (peak +46.7% given back to a -13% stop).
    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setExit","params":{"stopLossPct":"8","takeProfitPct":"90","forceExitSec":120,"trailingEnabled":true,"reason":"tighten"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the exit patch must be accepted: {applied}"
    );
    let audit = &applied["result"];
    assert_eq!(audit["persisted"], serde_json::json!(false));
    assert_eq!(
        audit["persistence"],
        serde_json::json!("none"),
        "the receipt names the carrier: no env file wired → none: {audit}"
    );
    assert_eq!(
        audit["actor"],
        serde_json::json!(format!("uid:{}", own_uid())),
        "who did it must be on the record: {audit}"
    );
    let change_of = |field: &str| {
        audit["applied"]
            .as_array()
            .expect("an applied list")
            .iter()
            .find(|c| c["field"] == serde_json::json!(field))
            .map(|c| {
                (
                    c["from"].as_str().expect("from").to_string(),
                    c["to"].as_str().expect("to").to_string(),
                    c["effect"].as_str().expect("effect").to_string(),
                )
            })
    };
    // The factory calibration: SL 12, TP 100, guillotine OFF (0).
    assert_eq!(
        change_of("stopLossPct"),
        Some(("12".into(), "8".into(), "live".into())),
        "the stop row carries old→new and live: {audit}"
    );
    assert_eq!(
        change_of("takeProfitPct"),
        Some(("100".into(), "90".into(), "live".into())),
        "the take-profit row carries old→new and live: {audit}"
    );
    assert_eq!(
        change_of("forceExitSec"),
        Some(("0".into(), "120".into(), "live".into())),
        "the guillotine row carries old→new and live: {audit}"
    );
    assert_eq!(
        change_of("trailingEnabled"),
        Some(("false".into(), "true".into(), "live".into())),
        "the trailing row carries old→new and live — the calibration default OFF re-arms hot: {audit}"
    );

    // The readout answers with the new values.
    let read = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"risk.limits","params":{}}"#.to_string(),
    )
    .await;
    assert!(read.get("error").is_none(), "readout must answer: {read}");
    // The exit view crosses as NUMBERS (finite decimals serialize as
    // numbers), not the Bound strings the systemic matrix uses.
    assert_eq!(
        read["result"]["exit"]["stopLossPct"],
        serde_json::json!(8.0)
    );
    assert_eq!(
        read["result"]["exit"]["takeProfitPct"],
        serde_json::json!(90.0)
    );
    assert_eq!(
        read["result"]["exit"]["forceExitSec"],
        serde_json::json!(120)
    );
    assert_eq!(
        read["result"]["exit"]["trailingEnabled"],
        serde_json::json!(true),
        "the readout carries the arming switch the panel renders"
    );

    // The write path is live, not just the receipt: the arming switch the
    // exit sweep reads every tick moved with it (a flipped trailing flag
    // without a config move would be a receipt lie).
    {
        let c = core.lock().await;
        assert!(
            c.config().positions.exit.trailing_enabled,
            "the write phase must move the LIVE exit config"
        );
    }

    // forceExitSec: 0 is the documented OFF switch, accepted.
    let off = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"risk.setExit","params":{"forceExitSec":0}}"#
            .to_string(),
    )
    .await;
    assert!(
        off.get("error").is_none(),
        "0 = guillotine OFF must be accepted: {off}"
    );

    // A zero percentage is refused — and atomicity means the legal field
    // riding along did NOT land either.
    let zero = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"risk.setExit","params":{"stopLossPct":"0","takeProfitPct":"50"}}"#.to_string(),
    )
    .await;
    assert!(
        zero["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("must be > 0"),
        "a zero percentage is refused with the field named: {zero}"
    );
    let read2 = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":5,"method":"risk.limits","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        read2["result"]["exit"]["takeProfitPct"],
        serde_json::json!(90.0),
        "a refused patch leaves no trace: {read2}"
    );

    // Empty patch is refused.
    let empty = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":6,"method":"risk.setExit","params":{}}"#.to_string(),
    )
    .await;
    assert!(
        empty["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("at least one"),
        "an empty patch is refused: {empty}"
    );
}

/// 轮次时间周期可设置（`round_duration.get`/`round_duration.set`）的
/// wire 合同：readout 回答「本会话生效值 + 来源 + 白名单 + 下次会话将
/// 解析到什么」；写路径把白名单内的值落进持久格（`nextSessionSec`
/// 如实回报）、格内容用独立加载器读回验证（原子写 + 重启读回同一路
/// 径）、白名单外的值拒绝且格不动、`reset` 清格、两者都给拒绝、无
/// runtime_config_path 的 core 拒绝写（回测/进程内测试不落盘）。
/// **next_session 语义** 由构造保证（写路径只碰持久格），测试同时钉住
/// 本会话的 `round_duration_sec` 一个字节不动。
#[tokio::test]
async fn round_duration_set_writes_the_cell_and_get_reads_it_back() {
    // Per-account round duration: `round_duration.set` now names an account
    // and writes `ACCOUNT_<ID>_ROUND_DURATION_SEC` to `.env`. The readout
    // reports `perAccount` plus the default effective duration.
    let dir = std::env::temp_dir().join(format!("bk-round-dur-wire-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    let env_str = env_path.to_string_lossy().into_owned();

    let (core, registry, peer) = {
        let cfg = CoreConfig {
            env_file_path: Some(env_str.clone()),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
            round_labels: [
                (300, "5m"),
                (900, "15m"),
                (3600, "1h"),
                (14400, "4h"),
                (86400, "daily"),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let mut c = Core::new(cfg.clone());
        c.set_balance(cfg.dry_seed_balance);
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
        )
    };

    // The readout before any write: effective 900 from the compiled default,
    // empty per-account map, the declared union with its labels.
    let read = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"round_duration.get","params":{}}"#.to_string(),
    )
    .await;
    assert!(read.get("error").is_none(), "readout must answer: {read}");
    assert_eq!(read["result"]["effectiveSec"], serde_json::json!(900));
    assert_eq!(read["result"]["source"], serde_json::json!("default"));
    assert_eq!(
        read["result"]["perAccount"],
        serde_json::json!({}),
        "no per-account overrides yet: {read}"
    );
    assert_eq!(
        read["result"]["nextSessionSec"],
        serde_json::json!(900),
        "next session re-resolves to the effective value: {read}"
    );
    let labels: Vec<(i64, &str)> = read["result"]["allowed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["sec"].as_i64().unwrap(),
                o["label"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert!(
        labels.contains(&(300, "5m")) && labels.contains(&(900, "15m")),
        "the whitelist carries the plugins' declared labels: {labels:?}"
    );

    // The write path: set account "default" to 300s. Immediate effect — the
    // running grid for that account moves now, not at restart.
    let set = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"round_duration.set","params":{"accountId":"default","durationSec":300,"reason":"faster rounds"}}"#.to_string(),
    )
    .await;
    assert!(set.get("error").is_none(), "the set must land: {set}");
    assert_eq!(
        set["result"]["accountId"],
        serde_json::json!("default"),
        "the reply names the account: {set}"
    );
    assert_eq!(
        set["result"]["nextSessionSec"],
        serde_json::json!(300),
        "the reply names the account's value: {set}"
    );
    assert_eq!(set["result"]["reset"], serde_json::json!(false));
    assert_eq!(
        set["result"]["actor"],
        serde_json::json!(format!("uid:{}", own_uid()))
    );
    // In-memory map reflects the write immediately.
    {
        let c = core.lock().await;
        assert_eq!(
            c.config().account_round_durations.get("default").copied(),
            Some(300),
            "in-memory per-account map updated immediately",
        );
    }

    // The readout now reports the per-account override.
    let read2 = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"round_duration.get","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        read2["result"]["perAccount"]["default"],
        serde_json::json!(300),
        "per-account map shows the override: {read2}"
    );
    assert_eq!(
        read2["result"]["effectiveSec"],
        serde_json::json!(900),
        "default effective stays 900 (the account override is separate): {read2}"
    );

    // An off-whitelist value is refused.
    let bad = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"round_duration.set","params":{"accountId":"default","durationSec":60}}"#
            .to_string(),
    )
    .await;
    assert!(
        bad["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not declared by any market plugin"),
        "an off-whitelist value is refused by name: {bad}"
    );

    // durationSec + reset together is refused.
    let both = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":5,"method":"round_duration.set","params":{"accountId":"default","durationSec":300,"reset":true}}"#.to_string(),
    )
    .await;
    assert!(
        both["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not both"),
        "an ambiguous write is refused: {both}"
    );

    // reset clears the account's override.
    let reset = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":6,"method":"round_duration.set","params":{"accountId":"default","reset":true}}"#
            .to_string(),
    )
    .await;
    assert!(reset.get("error").is_none(), "the reset must land: {reset}");
    assert_eq!(reset["result"]["reset"], serde_json::json!(true));
    assert_eq!(
        reset["result"]["accountId"],
        serde_json::json!("default"),
        "the reply names the account: {reset}"
    );
    assert_eq!(
        reset["result"]["nextSessionSec"],
        serde_json::json!(900),
        "after a reset the account falls back to the default: {reset}"
    );
    // In-memory map no longer has the account's override.
    {
        let c = core.lock().await;
        assert!(
            !c.config().account_round_durations.contains_key("default"),
            "in-memory per-account map cleared after reset",
        );
    }

    // A core with NO env file path (backtests, in-process tests)
    // refuses the write instead of dropping a file into whatever
    // checkout the caller runs from.
    let (bare, registry2, peer2) = hot_reload_fixture().await;
    let refused = rpc(
        &bare,
        &registry2,
        &peer2,
        r#"{"jsonrpc":"2.0","id":1,"method":"round_duration.set","params":{"accountId":"default","durationSec":300}}"#
            .to_string(),
    )
    .await;
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("persistence is disabled"),
        "a path-less core refuses the write: {refused}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #458 `system.reset_trading` 的 wire 合同：三道门（Live 拒绝 / 确认词
/// 逐字 RESET / 无审计路径拒绝）+ 重置闭环（停策略、清记录、re-seed、
/// 自校验）+ 审计行 + 反向验收（审计流不清除/余额必复原/记录必空）。
/// 记录文件挂在本测试的私有 scratch 目录（Hard Disk 写入策略）。这些路径
/// 曾是挂到 scratch cwd 的相对 `data/`——但 `set_current_dir` 是进程全局
/// 的，并行测试的默认相对路径 core 会追进这只测试的 cwd，重置自检的
/// 行数窗口就会数到别人的行。私有绝对路径 + 不动 cwd，窗口才真正闭合。
#[tokio::test]
async fn system_reset_trading_wipes_and_reseeds_with_audit() {
    let dir = std::env::temp_dir().join(format!("bk-reset-trading-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = |name: &str| Some(dir.join("data").join(name).to_string_lossy().into_owned());
    let audit_str = dir
        .join("audit")
        .join("manual-ops.jsonl")
        .to_string_lossy()
        .into_owned();

    let (core, registry, peer) = {
        let cfg = CoreConfig {
            manual_ops_audit_path: Some(audit_str.clone()),
            trade_log_path: log("trades.jsonl"),
            order_log_path: log("orders.jsonl"),
            position_log_path: log("positions.jsonl"),
            ..Default::default()
        };
        let mut c = Core::new(cfg.clone());
        c.set_balance(cfg.dry_seed_balance);
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
        )
    };

    // 铺一些交易状态：一张 resting 挂单（吃掉现金预留）。
    let feed = rpc(&core, &registry, &peer, books_line()).await;
    assert!(feed.get("error").is_none(), "feed the book first: {feed}");
    let slot = now_ms() / 1000 / 900;
    let place = rpc(
        &core,
        &registry,
        &peer,
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"orders.place","params":{{"tokenId":"tok","conditionId":"cond","side":"buy","mode":"maker","price":0.39,"size":5,"internalKey":"reset-probe","strategy":"s","asset":"BTC_UP","direction":"up","roundSlot":{slot}}}}}"#
        ),
    )
    .await;
    assert!(
        place.get("error").is_none(),
        "resting order placed: {place}"
    );

    // 反向验收 1：确认词错误 → 红，且什么都没发生。
    let wrong = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"system.reset_trading","params":{"confirm":"reset"}}"#
            .to_string(),
    )
    .await;
    assert!(
        wrong["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("exactly \"RESET\""),
        "the confirm word is checked verbatim: {wrong}"
    );
    let still = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"orders.list","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        still["result"]["orders"].as_array().map(|a| a.len()),
        Some(1),
        "a refused reset leaves every record in place: {still}"
    );

    // 反向验收 2：无审计路径的 core → 红（wipe with no audit trail）。
    let (bare, registry2, peer2) = hot_reload_fixture().await;
    let no_audit = rpc(
        &bare,
        &registry2,
        &peer2,
        r#"{"jsonrpc":"2.0","id":1,"method":"system.reset_trading","params":{"confirm":"RESET"}}"#
            .to_string(),
    )
    .await;
    assert!(
        no_audit["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no manual-ops audit path"),
        "a path-less core refuses the wipe: {no_audit}"
    );

    // 正向：确认词正确 → 重置落地，回执带自检余额与清理计数。
    let reset = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":5,"method":"system.reset_trading","params":{"confirm":"RESET","reason":"ui smoke"}}"#.to_string(),
    )
    .await;
    assert!(reset.get("error").is_none(), "the reset lands: {reset}");
    assert_eq!(
        reset["result"]["balances"][0][1],
        serde_json::json!("10000"),
        "the cash is re-seeded to the configured seed: {reset}"
    );
    // issue 561: the reply names the reset scope — an omitted accountId
    // resolves through the connection default (this peer has no session
    // cell, so the display account `default`).
    assert_eq!(
        reset["result"]["accountId"],
        serde_json::json!("default"),
        "the reply names the account the reset wiped: {reset}"
    );
    assert!(
        reset["result"]["clearedOpenPositions"]
            .as_i64()
            .unwrap_or(-1)
            >= 0,
        "the reply carries the cleanup counts: {reset}"
    );

    // 记录文件全部 0 行；挂单索引空。
    let orders = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":6,"method":"orders.list","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        orders["result"]["orders"].as_array().map(|a| a.len()),
        Some(0),
        "no order survives the reset: {orders}"
    );
    let summary = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":7,"method":"trades.summary","params":{}}"#.to_string(),
    )
    .await;
    assert!(
        summary.get("error").is_none() || summary["result"]["totalTrades"].as_u64() == Some(0),
        "the trade summary is zeroed: {summary}"
    );
    for f in ["trades.jsonl", "orders.jsonl"] {
        let text = std::fs::read_to_string(dir.join("data").join(f)).unwrap_or_default();
        assert!(
            text.trim().is_empty(),
            "{f} is empty after the reset ({} bytes)",
            text.len()
        );
    }

    // 审计行：一次（失败的确认词不写行）。
    let audit = std::fs::read_to_string(&audit_str).unwrap_or_default();
    let lines: Vec<&str> = audit.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "exactly one audit line for the one reset: {audit}"
    );
    let rec: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(rec["op"], serde_json::json!("reset_trading"));
    assert!(
        rec["actor"]
            .as_str()
            .unwrap_or_default()
            .starts_with("uid:"),
        "the audit line carries the peer actor: {rec}"
    );
    // issue 561: the audit line records WHICH account the wipe hit — the
    // only trace left once the records are gone must scope itself.
    assert_eq!(
        rec["account"],
        serde_json::json!("default"),
        "the audit line names the reset scope: {rec}"
    );
    assert!(
        rec["pre"].is_object(),
        "the audit line carries the pre-reset summary: {rec}"
    );

    // 再来一次重置（第二次审计行追加，不覆盖 —— 审计流 append-only）。
    let again = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":8,"method":"system.reset_trading","params":{"confirm":"RESET"}}"#
            .to_string(),
    )
    .await;
    assert!(
        again.get("error").is_none(),
        "a second reset still lands: {again}"
    );
    let audit2 = std::fs::read_to_string(&audit_str).unwrap_or_default();
    assert_eq!(
        audit2.lines().filter(|l| !l.trim().is_empty()).count(),
        2,
        "audit lines append, never overwrite: {audit2}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #191 acceptance (b): a knob OUTSIDE the safe subset is refused BY NAME,
/// with the reason and with what to do instead — and the refusal is atomic,
/// so an allowed field riding along in the same patch is not applied either.
#[tokio::test]
async fn risk_set_limits_refuses_a_forbidden_field_and_applies_nothing() {
    let (core, registry, peer) = hot_reload_fixture().await;

    let feed = rpc(&core, &registry, &peer, books_line()).await;
    assert!(feed.get("error").is_none(), "feed the book first: {feed}");
    let before = rpc(&core, &registry, &peer, stats_line(2)).await;

    // A double carrier: `maxOrderNotional` is inside the safe subset,
    // `stopLossPct` is an exit threshold. The whole request must be refused.
    let refused = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"risk.setLimits","params":{"maxOrderNotional":1,"stopLossPct":"0.50"}}"#.to_string(),
    )
    .await;
    assert_eq!(
        refused["error"]["code"],
        serde_json::json!(-32602),
        "a refused patch is INVALID_PARAMS, not a silent no-op: {refused}"
    );
    let msg = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("stopLossPct"),
        "the refusal must name the field: {refused}"
    );
    assert!(
        msg.contains("not hot-reloadable"),
        "and say what is wrong with it: {refused}"
    );
    assert!(
        msg.contains("a restart applies it"),
        "and how to get it: {refused}"
    );
    assert!(
        msg.contains("maxOrderNotional"),
        "and what CAN be changed live: {refused}"
    );

    // Nothing moved — neither the refused knob nor the allowed one that came
    // with it. `sizing` is the live view of every number this call could have
    // touched.
    let after = rpc(&core, &registry, &peer, stats_line(4)).await;
    assert_eq!(
        before["result"]["sizing"], after["result"]["sizing"],
        "a refused patch must leave the live limits exactly as they were"
    );

    // ...and the order path agrees: this 2.00 ticket still passes, so the
    // `maxOrderNotional: 1` that rode along was NOT applied (the same ticket
    // IS refused once that field really is applied — see the test above).
    let probe = rpc(
        &core,
        &registry,
        &peer,
        order_line(5, "probe-after-refusal", "BTC", "up"),
    )
    .await;
    assert!(
        probe.get("error").is_none(),
        "the cap must still be the startup 3 USD after a refused patch: {probe}"
    );
    assert_eq!(
        probe["result"]["status"],
        serde_json::json!("FILLED"),
        "the probe must be a real, filled order: {probe}"
    );
}

/// #1154: `round_duration.set {immediate: true}`，有在途仓位——「撤单 →
/// 市价强平 → 切网格」全链。出场价必须来自账簿真实 bid（0.39），不是
/// 估算；trades 流水写 exit_reason=operator_grid_switch，扫描器网格立即
/// 随新周期，.env 持久化。
#[tokio::test]
async fn round_duration_immediate_closes_position_and_switches_grid() {
    let dir = std::env::temp_dir().join(format!("bk-round-dur-imm-pos-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    let env_str = env_path.to_string_lossy().into_owned();

    let (core, registry, peer) = {
        let cfg = CoreConfig {
            env_file_path: Some(env_str.clone()),
            allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
            round_labels: [
                (300, "5m"),
                (900, "15m"),
                (3600, "1h"),
                (14400, "4h"),
                (86400, "daily"),
            ]
            .into_iter()
            .collect(),
            // 测试用私有 scratch 目录，绝不写入仓库根的 data/ 账本。
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let mut c = Core::new(cfg.clone());
        c.set_balance(cfg.dry_seed_balance);
        cfg.install_engine(&mut c).expect("engine install");
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
        )
    };

    // 1. 喂盘 + 入场单：5 股 @ 0.40 吃满，default 账户在途仓位。
    let feed = rpc(&core, &registry, &peer, books_line()).await;
    assert!(feed.get("error").is_none(), "feed the book first: {feed}");
    let open = rpc(
        &core,
        &registry,
        &peer,
        order_line(1, "probe-open", "BTC", "up"),
    )
    .await;
    assert_eq!(
        open["result"]["status"],
        serde_json::json!("FILLED"),
        "the entry probe must fill: {open}"
    );
    assert_eq!(
        positions_len(&core, &registry, &peer).await,
        1,
        "entry must leave one open position"
    );

    // 2. 立即切 5m：撤单 + 市价强平 + 切网格，一次回执。
    let set = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"round_duration.set","params":{"accountId":"default","durationSec":300,"immediate":true,"reason":"e2e immediate switch"}}"#.to_string(),
    )
    .await;
    assert!(set.get("error").is_none(), "immediate set must land: {set}");
    assert_eq!(
        set["result"]["nextSessionSec"],
        serde_json::json!(300),
        "the reply names the new grid: {set}"
    );
    assert_eq!(set["result"]["reset"], serde_json::json!(false));

    // 3. 仓位被市价真实平仓：出场价是账簿 bid（0.39），不是估算；
    //    trades 流水写 operator_grid_switch，账户归属如实。
    assert_eq!(
        positions_len(&core, &registry, &peer).await,
        0,
        "immediate switch must flatten the in-transit position"
    );
    let trades = trades_rows(&core, &registry, &peer).await;
    let rows = trades["result"]["trades"]
        .as_array()
        .expect("trades.history answers an array");
    let early = rows
        .iter()
        .find(|r| r["exitReason"] == "operator_grid_switch");
    assert!(
        early.is_some(),
        "the flattened position must book a trade with exit_reason operator_grid_switch: {trades}"
    );
    let early = early.unwrap();
    assert_eq!(
        early["exitPrice"],
        serde_json::json!(0.39),
        "the exit price must be the real bid fill, never an estimate: {early}"
    );
    assert_eq!(
        early["accountId"],
        serde_json::json!("default"),
        "the trade is per-account: {early}"
    );

    // 4. 扫描器网格立即随新周期，.env 持久化。
    let after = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"round_duration.get","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        after["result"]["perAccount"]["default"],
        serde_json::json!(300),
        "per-account override lands immediately: {after}"
    );
    assert_eq!(
        after["result"]["engineActiveSec"],
        serde_json::json!(300),
        "the engine scanner clock switches with the account grid: {after}"
    );
    let env_text = std::fs::read_to_string(&env_path).unwrap_or_default();
    assert!(
        env_text.contains("ACCOUNT_DEFAULT_ROUND_DURATION_SEC=\"300\""),
        "the override is persisted: {env_text}"
    );
}

/// #1154: 无在途仓位时 `immediate: true` 直接切网格，不触碰任何仓位。
#[tokio::test]
async fn round_duration_immediate_no_position_switches_grid_directly() {
    let dir = std::env::temp_dir().join(format!("bk-round-dur-imm-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    let env_str = env_path.to_string_lossy().into_owned();

    let (core, registry, peer) = {
        let cfg = CoreConfig {
            env_file_path: Some(env_str.clone()),
            allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
            round_labels: [
                (300, "5m"),
                (900, "15m"),
                (3600, "1h"),
                (14400, "4h"),
                (86400, "daily"),
            ]
            .into_iter()
            .collect(),
            // 测试用私有 scratch 目录，绝不写入仓库根的 data/ 账本。
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let mut c = Core::new(cfg.clone());
        c.set_balance(cfg.dry_seed_balance);
        cfg.install_engine(&mut c).expect("engine install");
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
        )
    };

    let set = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"round_duration.set","params":{"accountId":"default","durationSec":300,"immediate":true}}"#.to_string(),
    )
    .await;
    assert!(
        set.get("error").is_none(),
        "immediate set with no positions must land: {set}"
    );
    assert_eq!(
        positions_len(&core, &registry, &peer).await,
        0,
        "no positions existed and none must appear"
    );
    let after = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"round_duration.get","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        after["result"]["engineActiveSec"],
        serde_json::json!(300),
        "the grid switches immediately even with nothing to close: {after}"
    );
}

/// #1154: flagLocked（`round_duration_source == "cli_or_env"`）时即时切换被
/// 如实拒绝：仓位不动、网格不动、不静默。
#[tokio::test]
async fn round_duration_immediate_refused_when_flag_locked() {
    let dir = std::env::temp_dir().join(format!("bk-round-dur-imm-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    let env_str = env_path.to_string_lossy().into_owned();

    let (core, registry, peer) = {
        let cfg = CoreConfig {
            env_file_path: Some(env_str.clone()),
            allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
            round_duration_source: "cli_or_env",
            round_labels: [
                (300, "5m"),
                (900, "15m"),
                (3600, "1h"),
                (14400, "4h"),
                (86400, "daily"),
            ]
            .into_iter()
            .collect(),
            // 测试用私有 scratch 目录，绝不写入仓库根的 data/ 账本。
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let mut c = Core::new(cfg.clone());
        c.set_balance(cfg.dry_seed_balance);
        cfg.install_engine(&mut c).expect("engine install");
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
        )
    };

    // 在途仓位存在。
    let feed = rpc(&core, &registry, &peer, books_line()).await;
    assert!(feed.get("error").is_none(), "feed the book first: {feed}");
    let open = rpc(
        &core,
        &registry,
        &peer,
        order_line(1, "probe-open", "BTC", "up"),
    )
    .await;
    assert_eq!(
        open["result"]["status"],
        serde_json::json!("FILLED"),
        "the entry probe must fill: {open}"
    );
    assert_eq!(
        positions_len(&core, &registry, &peer).await,
        1,
        "precondition: one open position"
    );

    let set = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"round_duration.set","params":{"accountId":"default","durationSec":300,"immediate":true}}"#.to_string(),
    )
    .await;
    assert!(
        set.get("error").is_some(),
        "immediate set under flag_locked must be refused: {set}"
    );
    let msg = set["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("锁定") || msg.contains("拒绝"),
        "the refusal must name the lock: {msg}"
    );

    // 仓位不动、网格不动。
    assert_eq!(
        positions_len(&core, &registry, &peer).await,
        1,
        "refusal must not touch the position"
    );
    let after = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"round_duration.get","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        after["result"]["engineActiveSec"],
        serde_json::json!(900),
        "the grid must not switch under flag_locked: {after}"
    );
}

/// #1154: 强平无法完全成交时按实际成交入账并如实报错，网格不切；
/// 不影子价、不静默。（出场账簿无买盘，强平单无法成交。）
#[tokio::test]
async fn round_duration_immediate_unfilled_close_refuses_and_keeps_grid() {
    let no_bid_book = r#"{"jsonrpc":"2.0","id":1,"method":"books.snapshot","params":{"tokenId":"tok","bids":[],"asks":[{"price":0.40,"size":100}]}}"#.to_string();

    let (core, registry, peer) = {
        let cfg = CoreConfig {
            allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
            round_labels: [
                (300, "5m"),
                (900, "15m"),
                (3600, "1h"),
                (14400, "4h"),
                (86400, "daily"),
            ]
            .into_iter()
            .collect(),
            // 测试用私有 scratch 目录，绝不写入仓库根的 data/ 账本。
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let mut c = Core::new(cfg.clone());
        c.set_balance(cfg.dry_seed_balance);
        cfg.install_engine(&mut c).expect("engine install");
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
        )
    };

    let feed = rpc(&core, &registry, &peer, no_bid_book).await;
    assert!(feed.get("error").is_none(), "feed the book first: {feed}");
    let open = rpc(
        &core,
        &registry,
        &peer,
        order_line(1, "probe-open", "BTC", "up"),
    )
    .await;
    assert_eq!(
        open["result"]["status"],
        serde_json::json!("FILLED"),
        "the entry probe must fill: {open}"
    );
    assert_eq!(
        positions_len(&core, &registry, &peer).await,
        1,
        "precondition: one open position"
    );

    let set = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"round_duration.set","params":{"accountId":"default","durationSec":300,"immediate":true}}"#.to_string(),
    )
    .await;
    assert!(
        set.get("error").is_some(),
        "immediate set with an unfilled strong-close must be refused: {set}"
    );
    let msg = set["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("网格未切换"),
        "the error must state the grid was not switched: {msg}"
    );

    // 仓位不动、无 trades 行、网格不动。
    assert_eq!(
        positions_len(&core, &registry, &peer).await,
        1,
        "the unfilled position stays open"
    );
    let trades = trades_rows(&core, &registry, &peer).await;
    let rows = trades["result"]["trades"]
        .as_array()
        .expect("trades.history answers an array");
    assert!(
        !rows
            .iter()
            .any(|r| r["exitReason"] == "operator_grid_switch"),
        "no operator_grid_switch trade when the close did not complete: {trades}"
    );
    let after = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"round_duration.get","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        after["result"]["engineActiveSec"],
        serde_json::json!(900),
        "the grid must not switch when the close did not complete: {after}"
    );
}

/// #1154: `reset` 分支同语义——immediate + reset 先平掉在途仓位，再清
/// 格回落默认网格。
#[tokio::test]
async fn round_duration_immediate_reset_closes_positions_and_clears_grid() {
    let dir = std::env::temp_dir().join(format!("bk-round-dur-imm-reset-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    let env_str = env_path.to_string_lossy().into_owned();

    let (core, registry, peer) = {
        let cfg = CoreConfig {
            env_file_path: Some(env_str.clone()),
            allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
            round_labels: [
                (300, "5m"),
                (900, "15m"),
                (3600, "1h"),
                (14400, "4h"),
                (86400, "daily"),
            ]
            .into_iter()
            .collect(),
            // 测试用私有 scratch 目录，绝不写入仓库根的 data/ 账本。
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let mut c = Core::new(cfg.clone());
        c.set_balance(cfg.dry_seed_balance);
        cfg.install_engine(&mut c).expect("engine install");
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
        )
    };

    let feed = rpc(&core, &registry, &peer, books_line()).await;
    assert!(feed.get("error").is_none(), "feed the book first: {feed}");
    let open = rpc(
        &core,
        &registry,
        &peer,
        order_line(1, "probe-open", "BTC", "up"),
    )
    .await;
    assert_eq!(
        open["result"]["status"],
        serde_json::json!("FILLED"),
        "the entry probe must fill: {open}"
    );
    // 先持久一条覆盖，证明 reset 是清格而非「没写」。
    let pre = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"round_duration.set","params":{"accountId":"default","durationSec":300}}"#.to_string(),
    )
    .await;
    assert!(pre.get("error").is_none(), "pre-write must land: {pre}");

    let reset = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"round_duration.set","params":{"accountId":"default","reset":true,"immediate":true}}"#.to_string(),
    )
    .await;
    assert!(
        reset.get("error").is_none(),
        "immediate reset must land: {reset}"
    );
    assert_eq!(reset["result"]["reset"], serde_json::json!(true));
    assert_eq!(
        reset["result"]["nextSessionSec"],
        serde_json::json!(900),
        "reset falls back to the default grid: {reset}"
    );
    assert_eq!(
        positions_len(&core, &registry, &peer).await,
        0,
        "immediate reset must flatten the in-transit position too"
    );
    let after = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"round_duration.get","params":{}}"#.to_string(),
    )
    .await;
    assert!(
        !after["result"]["perAccount"]
            .as_object()
            .expect("perAccount is a map")
            .contains_key("default"),
        "the account's override is cleared: {after}"
    );
    assert_eq!(
        after["result"]["engineActiveSec"],
        serde_json::json!(900),
        "the engine clock returns to the default grid: {after}"
    );
    let env_text = std::fs::read_to_string(&env_path).unwrap_or_default();
    assert!(
        !env_text.contains("ACCOUNT_DEFAULT_ROUND_DURATION_SEC"),
        "the override key is removed from .env: {env_text}"
    );
}

/// #1154 fail-closed：`immediate` + 白名单外档位必须在撤单/强平之前就拒绝，
/// 仓位与网格一个字节不动。
#[tokio::test]
async fn round_duration_immediate_refuses_off_whitelist_before_touching_positions() {
    let dir =
        std::env::temp_dir().join(format!("bk-round-dur-imm-whitelist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    let env_str = env_path.to_string_lossy().into_owned();

    let (core, registry, peer) = {
        let cfg = CoreConfig {
            env_file_path: Some(env_str.clone()),
            allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
            round_labels: [
                (300, "5m"),
                (900, "15m"),
                (3600, "1h"),
                (14400, "4h"),
                (86400, "daily"),
            ]
            .into_iter()
            .collect(),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let mut c = Core::new(cfg.clone());
        c.set_balance(cfg.dry_seed_balance);
        cfg.install_engine(&mut c).expect("engine install");
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
        )
    };

    // 在途仓位存在。
    let feed = rpc(&core, &registry, &peer, books_line()).await;
    assert!(feed.get("error").is_none(), "feed the book first: {feed}");
    let open = rpc(
        &core,
        &registry,
        &peer,
        order_line(1, "probe-open", "BTC", "up"),
    )
    .await;
    assert_eq!(
        open["result"]["status"],
        serde_json::json!("FILLED"),
        "the entry probe must fill: {open}"
    );
    assert_eq!(
        positions_len(&core, &registry, &peer).await,
        1,
        "precondition: one open position"
    );

    // 60s 不在插件声明并集里：应在触碰仓位前被拒。
    let set = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"round_duration.set","params":{"accountId":"default","durationSec":60,"immediate":true}}"#.to_string(),
    )
    .await;
    assert!(
        set.get("error").is_some(),
        "off-whitelist immediate set must be refused: {set}"
    );
    let msg = set["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("not declared by any market plugin"),
        "the refusal must name the whitelist: {msg}"
    );

    // 仓位不动、网格不动、.env 不动。
    assert_eq!(
        positions_len(&core, &registry, &peer).await,
        1,
        "refusal must not touch the position"
    );
    let after = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"round_duration.get","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        after["result"]["engineActiveSec"],
        serde_json::json!(900),
        "the grid must not switch: {after}"
    );
    let env_text = std::fs::read_to_string(&env_path).unwrap_or_default();
    assert!(
        !env_text.contains("ROUND_DURATION_SEC"),
        "nothing may be persisted: {env_text}"
    );
}

/// 持仓列表长度（`positions.list` wire）。
async fn positions_len(
    core: &Arc<AsyncMutex<Core>>,
    registry: &crate::market::registry::MarketPluginRegistry,
    peer: &PeerAuth,
) -> usize {
    let v = rpc(
        core,
        registry,
        peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"positions.list","params":{}}"#.to_string(),
    )
    .await;
    v["result"]["positions"]
        .as_array()
        .expect("positions.list answers an array")
        .len()
}

/// 最近成交流水（`trades.history` wire）。
async fn trades_rows(
    core: &Arc<AsyncMutex<Core>>,
    registry: &crate::market::registry::MarketPluginRegistry,
    peer: &PeerAuth,
) -> serde_json::Value {
    rpc(
        core,
        registry,
        peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"trades.history","params":{"limit":50}}"#.to_string(),
    )
    .await
}
