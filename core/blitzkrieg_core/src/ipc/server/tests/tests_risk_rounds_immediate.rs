#![cfg(test)]

use super::*;

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
