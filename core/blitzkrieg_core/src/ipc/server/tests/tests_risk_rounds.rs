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
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"maxTotalPosition":1,"maxDailyDrawdownUsd":50,"reason":"live arm"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the systemic patch must be accepted: {applied}"
    );
    let audit = &applied["result"];
    assert_eq!(audit["persisted"], serde_json::json!(false));
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
        effect_of("maxTotalPosition").as_deref(),
        Some("live"),
        "the position-count cap is a live bound: {audit}"
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
        from_of("maxTotalPosition").as_deref(),
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
        read["result"]["global"]["limits"]["maxTotalPosition"]["value"],
        serde_json::json!("1")
    );
    assert_eq!(
        read["result"]["global"]["limits"]["maxTotalPosition"]["source"],
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
        r#"{"jsonrpc":"2.0","id":4,"method":"risk.setSystemic","params":{"maxTotalExposureUsd":"-5","maxCorrelationUsd":10}}"#.to_string(),
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
        read2["result"]["global"]["limits"]["maxCorrelationUsd"]["value"],
        serde_json::json!("0"),
        "a refused patch leaves no trace: {read2}"
    );
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
    // The cell's test path lives in a scratch dir the test owns (Hard
    // Disk per the repo's write policy); the fixture core points at it.
    let dir = std::env::temp_dir().join(format!("bk-round-dur-wire-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cell_path = dir.join("config.json");
    let cell_str = cell_path.to_string_lossy().into_owned();

    let (core, registry, peer) = {
        let cfg = CoreConfig {
            runtime_config_path: Some(cell_str.clone()),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            // The plugins' declared union as a test fixture (the values
            // the shipped plugins declare): the readout's label check
            // below judges against it. The labels travel the same road
            // (issue 648: the fixture states the plugins' declared
            // vocabulary itself — the shape boot aggregates).
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

    // The readout before any write: effective 900 from the compiled
    // default, nothing persisted, the declared union with its labels.
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
    assert_eq!(read["result"]["persisted"], serde_json::json!(false));
    assert_eq!(
        read["result"]["nextSessionSec"],
        serde_json::json!(900),
        "no cell means the next session re-resolves to the effective value: {read}"
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

    // The write path: 5m lands in the cell, the reply names the NEXT
    // session's value, and the RUNNING session is untouched.
    let set = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"round_duration.set","params":{"durationSec":300,"reason":"faster rounds"}}"#.to_string(),
    )
    .await;
    assert!(set.get("error").is_none(), "the set must land: {set}");
    assert_eq!(
        set["result"]["nextSessionSec"],
        serde_json::json!(300),
        "the reply names what the restart will run: {set}"
    );
    assert_eq!(set["result"]["reset"], serde_json::json!(false));
    assert_eq!(
        set["result"]["actor"],
        serde_json::json!(format!("uid:{}", own_uid()))
    );
    {
        let c = core.lock().await;
        assert_eq!(
            c.config().round_duration_sec,
            900,
            "next-session semantics: the running grid does not move"
        );
    }
    // The cell content, read back through the SAME loader a restart
    // would use — the write and the boot read agree by construction.
    let cell = crate::runtime_config::load_round_duration_from(
        std::path::Path::new(&cell_str),
        crate::account::config::TEST_ALLOWED_ROUNDS,
    )
    .expect("the cell must load");
    assert_eq!(cell.duration_sec, 300);

    // The readout now reports the cell as the next session's value while
    // the effective value stays this session's.
    let read2 = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"round_duration.get","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(read2["result"]["effectiveSec"], serde_json::json!(900));
    assert_eq!(read2["result"]["nextSessionSec"], serde_json::json!(300));
    assert_eq!(read2["result"]["persisted"], serde_json::json!(true));

    // An off-whitelist value is refused and the cell is untouched.
    let bad = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"round_duration.set","params":{"durationSec":60}}"#
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
    assert_eq!(
        crate::runtime_config::load_round_duration_from(
            std::path::Path::new(&cell_str),
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap()
        .duration_sec,
        300,
        "a refused write leaves the cell exactly as it was"
    );

    // durationSec + reset together is refused.
    let both = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":5,"method":"round_duration.set","params":{"durationSec":300,"reset":true}}"#.to_string(),
    )
    .await;
    assert!(
        both["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not both"),
        "an ambiguous write is refused: {both}"
    );

    // reset clears the cell: the next session falls back to the boot
    // resolution, and the readout reports nothing persisted.
    let reset = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":6,"method":"round_duration.set","params":{"reset":true}}"#
            .to_string(),
    )
    .await;
    assert!(reset.get("error").is_none(), "the reset must land: {reset}");
    assert_eq!(reset["result"]["reset"], serde_json::json!(true));
    assert_eq!(
        reset["result"]["nextSessionSec"],
        serde_json::json!(900),
        "after a reset the next session re-resolves to the effective value: {reset}"
    );
    assert_eq!(
        crate::runtime_config::load_round_duration_from(
            std::path::Path::new(&cell_str),
            crate::account::config::TEST_ALLOWED_ROUNDS,
        ),
        None,
        "the cell holds nothing after a reset"
    );
    let read3 = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":7,"method":"round_duration.get","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(read3["result"]["persisted"], serde_json::json!(false));
    assert_eq!(read3["result"]["nextSessionSec"], serde_json::json!(900));

    // A core with NO runtime config path (backtests, in-process tests)
    // refuses the write instead of dropping a file into whatever
    // checkout the caller runs from.
    let (bare, registry2, peer2) = hot_reload_fixture().await;
    let refused = rpc(
        &bare,
        &registry2,
        &peer2,
        r#"{"jsonrpc":"2.0","id":1,"method":"round_duration.set","params":{"durationSec":300}}"#
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
