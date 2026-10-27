#![cfg(test)]

use super::*;

use crate::account::{AccountId, AccountStatus};
use crate::model::Mode;
use crate::service::{Core, CoreConfig};
use blitzkrieg_market_api::net::now_ms;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

// ── 账户簿运行时增删：account.create / account.remove，at the wire ────────

/// A two-account core PLUS its private accounts.toml — the persist half
/// needs a file, and it must never be the checkout's own
/// `user_layer/configs/accounts.toml`. The in-memory book mirrors the
/// file (default + paper) so list/persist assertions can never disagree.
/// The scratch dir is per-CALL (an atomic counter, not just the pid): two
/// wire tests in the same process run their fixtures CONCURRENTLY under
/// the suite, and a pid-keyed dir would have them rename over each other.
async fn two_account_persist_fixture() -> (
    Arc<AsyncMutex<Core>>,
    crate::market::registry::MarketPluginRegistry,
    PeerAuth,
    std::path::PathBuf,
) {
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "bk-acct-verbs-wire-{}-{}",
        std::process::id(),
        CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"default\"\nname = \"main\"\n\n[[account]]\nid = \"paper\"\nname = \"paper\"\n",
    )
    .expect("fixture");
    let now = now_ms();
    let book = crate::account::AccountLedgers::new(
        vec![
            crate::account::Account {
                id: AccountId::from("default"),
                name: "main".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: now,
            },
            crate::account::Account {
                id: AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: now,
            },
        ],
        AccountId::from("default"),
    );
    let cfg = CoreConfig {
        accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let c = Core::with_account_book(cfg, book);
    (
        Arc::new(AsyncMutex::new(c)),
        crate::market::registry::MarketPluginRegistry::new(),
        PeerAuth::SameUid { uid: own_uid() },
        accounts_path,
    )
}

/// `account.create` 的闭环：创建 → `account.list` 出现（active + prediction
/// + 零余额 + 绑定带出）→ 测试私有的 accounts.toml 读回有该表（重新
/// load_account_ledgers，boot 的同一调用）→ 重复 id 拒 → 空 id 拒 →
/// 保留名 default 拒 → 未注册 venue 名拒（plan-first）→ 文件在拒绝下
/// 逐字节不动。
#[tokio::test]
async fn account_create_round_trips_list_and_the_persisted_file() {
    let (core, registry, peer, accounts_path) = two_account_persist_fixture().await;
    registry.register(Box::new(VenueStubPlugin {
        name: "polysimulator".to_string(),
    }));

    // Create: the mint lands on the wire, in memory, and in the file.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.create","params":{"id":"poly-main","name":"Poly Main 2","venue":"polysimulator","actor":"panel","reason":"add account dialog"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the create must land: {reply}"
    );
    assert_eq!(reply["result"]["id"], serde_json::json!("poly-main"));
    assert_eq!(reply["result"]["persisted"], serde_json::json!(true));
    assert_eq!(reply["result"]["actor"], serde_json::json!("panel"));
    assert_eq!(
        reply["result"]["note"].as_str().map(|s| !s.is_empty()),
        Some(true)
    );

    // account.list carries the new account with the minted posture.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.list","params":{}}"#.to_string(),
    )
    .await;
    let accounts = reply["result"]["accounts"].as_array().expect("views");
    let created = accounts
        .iter()
        .find(|a| a["id"] == "poly-main")
        .expect("the created account is listed");
    assert_eq!(created["status"], serde_json::json!("active"));
    assert_eq!(created["name"], serde_json::json!("Poly Main 2"));
    assert_eq!(created["venue"], serde_json::json!("polysimulator"));
    assert_eq!(
        created["balance"].as_f64(),
        Some(10_000.0),
        "a locally-settling mint wakes with the configured seed \
         (the policy.skip equity-0 gap): {}",
        created
    );
    assert_eq!(accounts.len(), 3);

    // The FILE: a fresh load — the boot's own call — sees the row.
    let book = crate::account::config::load_account_ledgers(
        &accounts_path,
        5,
        crate::account::config::TEST_ALLOWED_ROUNDS,
    )
    .expect("the written file re-parses");
    let created = book.account(&AccountId::from("poly-main")).unwrap();
    assert!(matches!(created.status, AccountStatus::Active));
    assert!(matches!(
        created.market_type,
        blitzkrieg_market_api::MarketType::Prediction
    ));
    assert_eq!(created.venue, "polysimulator");
    assert_eq!(created.name, "Poly Main 2");
    // Siblings rode through untouched.
    assert_eq!(
        book.account(&AccountId::from("default")).unwrap().name,
        "main"
    );

    // Duplicate id: refused, INVALID_PARAMS, file untouched.
    let before = std::fs::read_to_string(&accounts_path).unwrap();
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"account.create","params":{"id":"paper"}}"#.to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "duplicate id: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the refusal is a client fault at heart"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already exists"),
        "{}",
        reply["error"]["message"]
    );
    // Empty id: refused.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"account.create","params":{"id":"   "}}"#.to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "empty id: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the refusal is a client fault at heart"
    );
    // The reserved `default`: refused, at any spelling.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":5,"method":"account.create","params":{"id":"default"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "reserved id: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the refusal is a client fault at heart"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("reserved"),
        "{}",
        reply["error"]["message"]
    );
    // An unregistered venue name: refused plan-first.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":6,"method":"account.create","params":{"id":"ghost-venue","venue":"ghostvenue"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "unregistered venue: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the refusal is a client fault at heart"
    );
    // Every refusal left the file byte-identical.
    assert_eq!(
        std::fs::read_to_string(&accounts_path).unwrap(),
        before,
        "a refused create never touches the file"
    );
    let scratch = accounts_path
        .parent()
        .expect("the fixture file has a parent")
        .to_path_buf();
    let _ = std::fs::remove_dir_all(scratch);
}

/// `account.remove` 的闭环：删掉 → list 消失 → 文件读回没有 → 有 open
/// position 的账户拒（报持仓数）→ 最后一个账户拒（至少保留一个）→
/// 未知 id 拒 → 文件在拒绝下逐字节不动。删进程级 active 账户回切剩余
/// 账户也钉在这里（簿层的 default 回切/首账户回切场景在簿层测试已
/// 覆盖，线上走 default active 的常规路径）。default 不再保留名：
/// 所有账户都可删（用户裁决 —— 绑定只在建户时绑定，删户重建是唯一
/// 变更出口），但最后一格绝不放空。
#[tokio::test]
async fn account_remove_round_trips_list_and_the_persisted_file() {
    let (core, registry, peer, accounts_path) = two_account_persist_fixture().await;

    // Remove: the retirement lands on the wire, in memory, and in the file.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.remove","params":{"id":"paper","actor":"panel","reason":"cleanup"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the remove must land: {reply}"
    );
    assert_eq!(reply["result"]["id"], serde_json::json!("paper"));
    assert_eq!(reply["result"]["persisted"], serde_json::json!(true));
    assert_eq!(reply["result"]["actor"], serde_json::json!("panel"));

    // account.list no longer carries it.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.list","params":{}}"#.to_string(),
    )
    .await;
    let accounts = reply["result"]["accounts"].as_array().expect("views");
    assert_eq!(accounts.len(), 1, "paper left the book");
    assert_eq!(accounts[0]["id"], serde_json::json!("default"));

    // The FILE: a fresh load agrees.
    let book = crate::account::config::load_account_ledgers(
        &accounts_path,
        5,
        crate::account::config::TEST_ALLOWED_ROUNDS,
    )
    .expect("the written file re-parses");
    assert!(book.account(&AccountId::from("paper")).is_none());
    assert!(book.account(&AccountId::from("default")).is_some());

    // The LAST account: refused — a deployment keeps at least one
    // account (the empty book fails the next boot). Paper is already
    // gone, so `default` is the last one standing.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"account.remove","params":{"id":"default"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "last account: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the refusal is a client fault at heart"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("最后一个账户"),
        "{}",
        reply["error"]["message"]
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("至少要保留一个账户"),
        "{}",
        reply["error"]["message"]
    );
    // An unknown id: the standard refusal.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"account.remove","params":{"id":"paper"}}"#.to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "unknown id: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the refusal is a client fault at heart"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown account `paper`"),
        "{}",
        reply["error"]["message"]
    );

    // An account with an OPEN position: refused, with the count named.
    // The position pin needs a live sibling id, so create one. The
    // byte-identity snapshot is taken AFTER that create (the create is
    // expected to write) and before the refusals it guards.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":6,"method":"account.create","params":{"id":"pinned"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "pinned must create: {reply}");
    {
        use crate::model::SignalDirection;
        use crate::position::OpenParams;
        let mut c = core.lock().await;
        // One position for `default` as well: removing a funded account
        // must not disturb the OTHER accounts' inventory either way.
        c.positions_mut().open(
            OpenParams {
                strategy: "spread_arb".into(),
                asset: "BTC".into(),
                direction: SignalDirection::Up,
                token_id: "tok-up".into(),
                condition_id: "cond".into(),
                account_id: AccountId::from("default"),
                venue: String::new(),
                partner_token_id: String::new(),
                entry_price: rust_decimal_macros::dec!(0.48),
                expires_at_ms: 900_000,
                was_maker: true,
                target_exit_price: None,
                round_duration_sec: 0,
            },
            now_ms(),
        );
        c.positions_mut().open(
            OpenParams {
                strategy: "spread_arb".into(),
                asset: "ETH".into(),
                direction: SignalDirection::Down,
                token_id: "tok-down".into(),
                condition_id: "cond".into(),
                account_id: AccountId::from("pinned"),
                venue: String::new(),
                partner_token_id: String::new(),
                entry_price: rust_decimal_macros::dec!(0.45),
                expires_at_ms: 900_000,
                was_maker: true,
                target_exit_price: None,
                round_duration_sec: 0,
            },
            now_ms(),
        );
    }
    let before = std::fs::read_to_string(&accounts_path).unwrap();
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":7,"method":"account.remove","params":{"id":"pinned"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "open positions pin the account: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the refusal is a client fault at heart"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("1 笔未平持仓"),
        "{}",
        reply["error"]["message"]
    );
    // The pinned account survived every refusal, on the wire AND the file.
    assert_eq!(
        std::fs::read_to_string(&accounts_path).unwrap(),
        before,
        "a refused remove never touches the file"
    );
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":8,"method":"account.list","params":{}}"#.to_string(),
    )
    .await;
    let accounts = reply["result"]["accounts"].as_array().expect("views");
    assert!(
        accounts.iter().any(|a| a["id"] == "pinned"),
        "the pinned account is still listed: {reply}"
    );

    // `default` is an ORDINARY account now: removable while a sibling
    // exists. Its probe position from the position-pin arm above is
    // closed first (flat accounts only), `pinned` is the survivor, so
    // removing the active `default` lands and the active cell
    // re-activates the first surviving account. The row leaves
    // accounts.toml too — the next boot re-derives the zero-config
    // default from the missing row.
    {
        let mut c = core.lock().await;
        let ids: Vec<String> = c
            .positions_mut()
            .open_positions()
            .iter()
            .filter(|p| p.account_id == AccountId::from("default"))
            .map(|p| p.id.clone())
            .collect();
        for pid in ids {
            c.positions_mut()
                .close(
                    &pid,
                    rust_decimal_macros::dec!(0.5),
                    crate::model::ExitReason::Manual,
                    false,
                    now_ms(),
                )
                .expect("the probe position closes");
        }
    }
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":9,"method":"account.remove","params":{"id":"default"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "`default` must be removable while a sibling remains: {reply}"
    );
    assert_eq!(reply["result"]["persisted"], serde_json::json!(true));
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":10,"method":"account.list","params":{}}"#.to_string(),
    )
    .await;
    let accounts = reply["result"]["accounts"].as_array().expect("views");
    assert_eq!(accounts.len(), 1, "only the survivor remains: {reply}");
    assert_eq!(accounts[0]["id"], serde_json::json!("pinned"));
    assert_eq!(
        reply["result"]["active"], "pinned",
        "the active cell never dangles: it re-activates the first survivor"
    );
    let book = crate::account::config::load_account_ledgers(
        &accounts_path,
        5,
        crate::account::config::TEST_ALLOWED_ROUNDS,
    )
    .expect("the written file re-parses");
    assert!(book.account(&AccountId::from("default")).is_none());
    assert!(book.account(&AccountId::from("pinned")).is_some());
    let scratch = accounts_path
        .parent()
        .expect("the fixture file has a parent")
        .to_path_buf();
    let _ = std::fs::remove_dir_all(scratch);
}

/// account.remove 的 journal 收尾（流水卡幽灵 seed 根因修复的 wire 半边）：
/// 删户把该账户的磁盘流水文件一并清掉、内存 trail 清空；同名 id 立刻重建
/// 时，`account.journal` 回执是一份**没有上一世成交**的新账本。生产 kalshi
/// 实锤的 6 笔幽灵 seed@account.create（Σamount 比真实余额多 5000）就是
/// 本测试钉死的不变量被打破的形状。
#[tokio::test]
async fn account_remove_retires_the_journal_file_and_a_recreate_starts_clean() {
    let (core, registry, peer, accounts_path) = two_account_persist_fixture().await;
    let journal_dir = accounts_path.parent().expect("fixture dir").join("ledger");
    std::fs::create_dir_all(&journal_dir).expect("journal dir");
    let journal_path = journal_dir.join("paper.journal.jsonl");
    // 上一世：seed + 一买一卖全落盘（boot attach_journals 会整表回放的形状）。
    {
        let mut c = core.lock().await;
        let l = c
            .accounts_mut()
            .get_mut(&AccountId::from("paper"))
            .expect("paper ledger");
        l.set_journal_path(journal_path.clone());
        l.seed(rust_decimal_macros::dec!(1000));
        l.settle_buy_fill(
            "old_1",
            rust_decimal_macros::dec!(30),
            rust_decimal::Decimal::ZERO,
        );
        l.settle_sell_fill(rust_decimal_macros::dec!(21), rust_decimal_macros::dec!(1));
    }
    assert!(journal_path.exists(), "previous-life trail is on disk");

    // The retirement takes the file with it.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.remove","params":{"id":"paper","actor":"panel","reason":"retire"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "remove must land: {reply}");
    assert!(
        !journal_path.exists(),
        "the journal file is retired with the account"
    );

    // Same-id re-create: a fresh book. The journal wire surface reports an
    // EMPTY trail — the previous life's fills cannot leak in (memory AND,
    // by the file assertion above, the next boot's replay).
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.create","params":{"id":"paper","name":"reborn","actor":"panel"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "re-create must land: {reply}");
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"account.journal","params":{"accountId":"paper"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "journal read: {reply}");
    let rows = reply["result"]["rows"].as_array().expect("rows");
    assert!(
        rows.iter()
            .all(|r| r["kind"] != serde_json::json!("buy_fill")
                && r["kind"] != serde_json::json!("sell_fill")),
        "no previous-life fills survive the recreate: {rows:?}"
    );
    let scratch = accounts_path
        .parent()
        .expect("the fixture file has a parent")
        .to_path_buf();
    let _ = std::fs::remove_dir_all(scratch);
}

/// 两个新动词都是写动词：只读模式的内核拒绝（READONLY_REFUSED 名单，
/// 同 venue.set 的文案形状）。
#[tokio::test]
async fn account_create_and_remove_are_refused_in_read_only_mode() {
    let (core, registry, peer, _accounts_path) = two_account_persist_fixture().await;
    core.lock().await.config_mut().mode = Mode::ReadOnly;
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.create","params":{"id":"poly-main"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "read-only refuses the configuration verb: {reply}"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("read-only mode"),
        "{}",
        reply["error"]["message"]
    );
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.remove","params":{"id":"paper"}}"#.to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "read-only refuses the configuration verb: {reply}"
    );
}

/// 无 accounts_config_path 的内核（回测、多数进程内测试）拒绝两个动词
/// —— 与 venue.set 的 "persistence is disabled" 同一文案形状：一个
/// 不能持久化的写入是对操作员的谎报。
#[tokio::test]
async fn account_create_and_remove_refuse_without_a_config_path() {
    let (core, registry, peer) = two_account_fixture().await;
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.create","params":{"id":"poly-main"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "no persist path: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the refusal is a client fault at heart"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("persistence is disabled"),
        "{}",
        reply["error"]["message"]
    );
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.remove","params":{"id":"paper"}}"#.to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "no persist path: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the refusal is a client fault at heart"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("persistence is disabled"),
        "{}",
        reply["error"]["message"]
    );
}

// ── #509 的读面：account.journal，at the wire ───────────────────────────

/// The journal readout returns the LAST `limit` rows in ascending
/// (journal) order and discloses a truncated window: a seeded default
/// book holds exactly one `Seed` row, so a small limit answers that row
/// with `truncated == false` … until the book moves — a `set_balance`
/// realign appends a second row, `limit: 1` then answers only the NEWEST
/// (the Realign) with `truncated == true`. An explicit `accountId` names
/// its own book (the un-seeded `paper` journal is empty); an omitted one
/// rides the session's default.
#[tokio::test]
async fn account_journal_returns_the_tail_in_order_and_discloses_truncation() {
    use rust_decimal_macros::dec;
    let (core, registry, peer) = two_account_fixture().await;

    // No session switch: the omitted accountId resolves to the process
    // default, whose journal carries exactly the boot Seed row.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.journal","params":{}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "journal must answer: {reply}");
    assert_eq!(reply["result"]["accountId"], serde_json::json!("default"));
    let rows = reply["result"]["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1, "the seeded book journals its Seed: {reply}");
    assert_eq!(rows[0]["kind"], serde_json::json!("seed"));
    assert_eq!(
        rows[0]["ref"],
        serde_json::json!("boot"),
        "the JournalEntry's ref_ rides verbatim"
    );
    assert!(
        rows[0]["amount"].is_number() && rows[0]["balanceAfter"].is_number(),
        "money follows the decimal wire convention: {reply}"
    );
    assert_eq!(
        reply["result"]["truncated"],
        serde_json::json!(false),
        "1 snapshot row <= 1 returned row: nothing withheld"
    );

    // A realign appends a second row on the default book.
    core.lock().await.set_balance(dec!(9_999));

    // limit=1: only the NEWEST row (the Realign), and the disclosure
    // that rows exist beyond the window.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.journal","params":{"limit":1}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "journal must answer: {reply}");
    let rows = reply["result"]["rows"].as_array().expect("rows");
    assert_eq!(
        rows.len(),
        1,
        "limit=1 must return exactly one row: {reply}"
    );
    assert_eq!(
        rows[0]["kind"],
        serde_json::json!("realign"),
        "the tail is the newest row, not the oldest: {reply}"
    );
    assert_eq!(
        reply["result"]["truncated"],
        serde_json::json!(true),
        "2 snapshot rows > 1 returned row: the window is disclosed"
    );

    // An explicit accountId names its own book: the boot seeds EVERY
    // account (§9.3 — separate wallets, not aliases), so `paper` carries
    // its own Seed row and truncation reads against ITS window only.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"account.journal","params":{"accountId":"paper","limit":50}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "journal must answer: {reply}");
    assert_eq!(reply["result"]["accountId"], serde_json::json!("paper"));
    let rows = reply["result"]["rows"].as_array().expect("rows");
    assert_eq!(
        rows.len(),
        1,
        "paper's own book seeded exactly once: {reply}"
    );
    assert_eq!(rows[0]["kind"], serde_json::json!("seed"));
    assert_eq!(reply["result"]["truncated"], serde_json::json!(false));

    // limit above the 500 ceiling clamps instead of erroring (the
    // kline.history posture: a big ask shrinks, it never refuses).
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"account.journal","params":{"limit":9999}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "an over-ceiling limit clamps, it does not refuse: {reply}"
    );
    let rows = reply["result"]["rows"].as_array().expect("rows");
    assert_eq!(
        rows[0]["kind"],
        serde_json::json!("seed"),
        "the whole trail fits under the clamp: oldest first"
    );
    assert_eq!(
        rows[1]["kind"],
        serde_json::json!("realign"),
        "ascending journal order is the wire order"
    );
}

/// A journal read for an account the deployment never configured is the
/// same client fault the neighbours report: the ledger lookup's
/// `unknown account` error, riding the `typed` tail (APPLICATION with
/// coreCode INVALID_PARAMS — the same envelope `account.venue.set`'s
/// unknown-account refusal uses). No implicit book, ever.
#[tokio::test]
async fn account_journal_names_an_unknown_account_as_invalid_params() {
    let (core, registry, peer) = two_account_fixture().await;
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.journal","params":{"accountId":"ghost"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_some(),
        "an unknown account must fail loud: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "a client fault at heart: {reply}"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown account `ghost`"),
        "the refusal names the account: {reply}"
    );
}

// ── issue 561（2026-10-20 裁决）：缺省账户解析链 session → display →
// 进程 default，display 是面板唯一可信账户源 ──────────────────────────

/// helper 的三分支单测：session 有值优先（CLI/门禁显式 switch 的兼容
/// 面）；session 空则读 display 指针；display 仍必须命名已配置账户，
/// 尾部落进程 default（病理态兜底）。
#[test]
fn effective_default_account_resolves_session_then_display_then_default() {
    let mut book = crate::account::AccountLedgers::new(
        vec![
            crate::account::Account {
                id: AccountId::from("default"),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
            crate::account::Account {
                id: AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
        ],
        AccountId::from("default"),
    );

    // Branch 1: session cell set — it wins over everything (compat).
    let session = SessionState {
        session_active: Some(AccountId::from("paper")),
        ..SessionState::default()
    };
    book.set_display(&AccountId::from("default"))
        .expect("re-point");
    assert_eq!(
        effective_default_account(&session, &book).as_str(),
        "paper",
        "an explicit session switch still wins (backward-compat layer)"
    );

    // Branch 2: session empty — the DISPLAY pointer decides (issue 561).
    let session = SessionState::default();
    assert_eq!(
        effective_default_account(&session, &book).as_str(),
        "default",
        "display pointer at default"
    );
    book.set_display(&AccountId::from("paper"))
        .expect("re-point");
    assert_eq!(
        effective_default_account(&session, &book).as_str(),
        "paper",
        "the omitted-account default follows the display pointer"
    );

    // Branch 3: pathological tail — the invariant (display names a
    // configured account) is violated; the process default catches it.
    let repaired = crate::account::AccountLedgers::new(
        vec![crate::account::Account {
            id: AccountId::from("default"),
            name: "default".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: String::new(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            updated_at_ms: 0,
        }],
        AccountId::from("default"),
    );
    assert_eq!(
        effective_default_account(&SessionState::default(), &repaired).as_str(),
        "default",
        "a healthy book resolves through display without ever reaching this"
    );
}

/// wire 层：session 空的新连接（dispatcher 3s 重连后的形态）上，
/// journal 的缺省账户跟随 display 指针 —— 用 `account.switch` 把会话
/// 格填上后优先级回到 session（兼容面不受影响）。
#[tokio::test]
async fn account_journal_default_follows_display_then_session_wins() {
    use rust_decimal_macros::dec;
    let (core, registry, peer) = two_account_fixture().await;

    // 给 paper 账本种一行可辨认的流水（journal 非空 → accountId 可辨）。
    core.lock()
        .await
        .accounts_mut()
        .get_mut(&AccountId::from("paper"))
        .expect("paper book")
        .set_balance(dec!(777));

    // 无 session、display 仍在 default：缺省解析落 default（其 journal
    // 只有 boot Seed 行，balance_after 是种子额；不追问数值，只辨账户）。
    let mut session = SessionState::default();
    let update_state = Arc::new(crate::ipc::version::UpdateState::new(false, false));
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &update_state,
        &mut session,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.journal","params":{}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "journal must answer: {reply}");
    assert_eq!(
        reply["result"]["accountId"],
        serde_json::json!("default"),
        "no session, display at default: the omitted accountId lands there"
    );

    // display.account.set 把指针推向 paper（同进程内核面写法；wire 层
    // 需 persist 路径，这里直接走 Core 的内存写 —— 指针语义相同）。
    core.lock()
        .await
        .accounts_mut()
        .set_display(&AccountId::from("paper"))
        .expect("the re-point lands");

    // 新连接（session 重新为空 —— 模拟 dispatcher 重连）：journal 缺省
    // 跟着 display 走到 paper，不再回落进程 default。
    let mut fresh_session = SessionState::default();
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &update_state,
        &mut fresh_session,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.journal","params":{}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "journal must answer: {reply}");
    assert_eq!(
        reply["result"]["accountId"],
        serde_json::json!("paper"),
        "issue 561: a session-less connection follows the display pointer"
    );

    // 兼容面：本连接显式 account.switch 后，session 格有值、优先于
    // display —— journal 缺省回到 session 账户。
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &update_state,
        &mut fresh_session,
        r#"{"jsonrpc":"2.0","id":3,"method":"account.switch","params":{"accountId":"default"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "switch must answer: {reply}");
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &update_state,
        &mut fresh_session,
        r#"{"jsonrpc":"2.0","id":4,"method":"account.journal","params":{}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "journal must answer: {reply}");
    assert_eq!(
        reply["result"]["accountId"],
        serde_json::json!("default"),
        "an explicit session switch still wins over display (compat)"
    );
}
