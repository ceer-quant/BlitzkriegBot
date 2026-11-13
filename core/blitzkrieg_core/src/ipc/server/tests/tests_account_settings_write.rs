#![cfg(test)]

//! #1189 PR-C: the account-level settings write path at the wire — every
//! write face lands its `ACCOUNT_<SFX>_*` / `POLICY_*_<SFX>` keys in the
//! `.env` first and then re-derives the in-memory book through the same
//! parse the boot runs (`Core::reload_accounts`), so `account.list` and
//! `execution_policy.get` answer from the book the process will trade
//! under, not from a second in-memory spelling.
//!
//! The four contract teeth:
//!   1. `account.create` with a duplicate id is refused (and the file
//!      stays byte-identical);
//!   2. `account.venue.set` is visible to `account.list` IN THE SAME
//!      PROCESS (the reload half), and the `.env` agrees with a fresh
//!      boot-shaped load;
//!   3. `strategy.enable` is per-account isolated — enabling on A never
//!      touches B's set (env keys included);
//!   4. `execution_policy.set` with an `accountId` writes the
//!      `POLICY_*_<SFX>` keys (never the global names) and the reloaded
//!      section view reflects them.

use super::*;

use crate::account::{AccountId, AccountStatus};
use crate::engine::EngineConfig;
use crate::service::{Core, CoreConfig};
use crate::strategies::test_support;
use blitzkrieg_market_api::net::now_ms;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

/// A private `.env` + legacy-toml path (never written) + a two-account book
/// that mirrors the file — the production shape the write faces require
/// (the persist half refuses without the accounts config path, and the
/// same-process reload re-derives the book from exactly this file).
fn scratch_env(tag: &str, env_body: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "bk-1189-write-{tag}-{}-{}",
        std::process::id(),
        CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(&env_path, env_body).expect("fixture env");
    (accounts_path, env_path)
}

/// The book the fixtures above spell: `default` + `paper`, both active,
/// unbound, no sets. Kept byte-for-byte in sync with the env body each
/// caller passes (the reload re-derives from the file).
fn two_account_book(now: i64) -> crate::account::AccountLedgers {
    crate::account::AccountLedgers::new(
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
                opening_equity: None,
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
                opening_equity: None,
                updated_at_ms: now,
            },
        ],
        AccountId::from("default"),
    )
}

fn env_map(path: &std::path::Path) -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(crate::env_file::parse_line)
        .collect()
}

// ── 1. account.create 带重复 id 拒绝 ────────────────────────────────────

/// `account.create` is the mint face of the account-level write path: a
/// duplicate id (the one refusal that needs no runtime facts beyond the
/// book) must fail closed — no list growth, no key written, the file
/// byte-identical — and the second attempt must name the collision.
#[tokio::test]
async fn account_create_refuses_a_duplicate_id_and_leaves_the_file_untouched() {
    let (accounts_path, env_path) = scratch_env(
        "dup-create",
        "ACCOUNT_LIST=default,paper\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_PAPER_NAME=\"paper\"\n",
    );
    let core = Arc::new(AsyncMutex::new(Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        },
        two_account_book(now_ms()),
    )));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    registry.register(Box::new(VenueStubPlugin {
        name: "polysimulator".to_string(),
    }));
    let peer = PeerAuth::SameUid { uid: own_uid() };

    let first = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.create","params":{"id":"poly-main","venue":"polysimulator"}}"#.to_string(),
    )
    .await;
    assert!(first.get("error").is_none(), "the mint must land: {first}");

    let before = std::fs::read_to_string(&env_path).expect("env after the mint");
    let duplicate = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.create","params":{"id":"poly-main"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        duplicate["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the duplicate must be refused as a client fault: {duplicate}"
    );
    assert!(
        duplicate["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already exists"),
        "the refusal names the collision: {duplicate}"
    );
    assert_eq!(
        std::fs::read_to_string(&env_path).expect("env after the refusal"),
        before,
        "a refused create never touches the file"
    );
    // The book did not grow either (memory half untouched).
    let c = core.lock().await;
    assert!(
        c.accounts()
            .account(&AccountId::from("poly-main"))
            .is_some(),
        "the first mint is still the only poly-main"
    );
    assert_eq!(c.accounts().account_list().len(), 3, "no phantom row");
    let _ = std::fs::remove_dir_all(env_path.parent().unwrap());
}

// ── 2. account.venue.set 后 account.list 反映新 venue（同进程 reload） ──

/// The write-shape tooth: `account.venue.set` lands `ACCOUNT_PAPER_VENUE`
/// in the `.env`, re-derives the whole book through the boot's own parse
/// (`Core::reload_accounts`), and `account.list` answers the RELOADED
/// binding — same process, no restart. The file also agrees with a fresh
/// boot-shaped load, so what the write applied IS what the next boot
/// loads.
#[tokio::test]
async fn venue_set_is_visible_to_account_list_through_the_same_process_reload() {
    let (accounts_path, env_path) = scratch_env(
        "venue-reload",
        "ACCOUNT_LIST=default,paper\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_PAPER_NAME=\"paper\"\n",
    );
    let core = Arc::new(AsyncMutex::new(Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        },
        two_account_book(now_ms()),
    )));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    registry.register(Box::new(VenueStubPlugin {
        name: "polysimulator".to_string(),
    }));
    let peer = PeerAuth::SameUid { uid: own_uid() };

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.venue.set","params":{"accountId":"paper","venue":"polysimulator","reason":"venue panel"}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "the bind must land: {reply}");
    assert_eq!(reply["result"]["venue"], serde_json::json!("polysimulator"));

    // The same-process reload half: account.list reads the re-derived book.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.list","params":{}}"#.to_string(),
    )
    .await;
    let accounts = reply["result"]["accounts"].as_array().expect("views");
    let paper = accounts
        .iter()
        .find(|a| a["id"] == "paper")
        .expect("paper listed");
    assert_eq!(
        paper["venue"],
        serde_json::json!("polysimulator"),
        "the reload put the new binding in force: {paper}"
    );
    let default = accounts
        .iter()
        .find(|a| a["id"] == "default")
        .expect("default listed");
    assert_eq!(
        default["venue"],
        serde_json::json!(""),
        "the sibling is untouched: {default}"
    );

    // The durable half: the .env key, and a fresh boot-shaped load agrees.
    let on_file = env_map(&env_path);
    assert_eq!(
        on_file.get("ACCOUNT_PAPER_VENUE").map(String::as_str),
        Some("polysimulator"),
        "the binding is on disk: {on_file:?}"
    );
    let reread = crate::account::config::load_account_ledgers_single_source(
        &accounts_path,
        Some(&env_path),
        now_ms(),
        crate::account::config::TEST_ALLOWED_ROUNDS,
    )
    .expect("the written file re-parses");
    assert_eq!(
        reread
            .account(&AccountId::from("paper"))
            .expect("paper")
            .venue,
        "polysimulator",
        "what the write applied IS what the next boot loads"
    );
    let _ = std::fs::remove_dir_all(env_path.parent().unwrap());
}

// ── 3. strategy.enable 按账户隔离（A 启用不影响 B） ─────────────────────

/// A core whose engine registers `spread_arb` (the test adapter) and whose
/// book carries `default` + `paper` over a private `.env` with
/// `ACCOUNT_LIST` spelled (the production shape: the write faces' persist
/// half and their same-process reload both read this file).
async fn strategy_isolation_fixture() -> (
    Arc<AsyncMutex<Core>>,
    crate::market::registry::MarketPluginRegistry,
    PeerAuth,
    std::path::PathBuf,
) {
    let (accounts_path, env_path) = scratch_env(
        "strategy-iso",
        "ACCOUNT_LIST=default,paper\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_PAPER_NAME=\"paper\"\n",
    );
    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        },
        two_account_book(now_ms()),
    );
    let mut engine = crate::engine::Engine::new(EngineConfig::default());
    test_support::host_disabled(
        &mut engine,
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    c.enable_engine(engine);
    (
        Arc::new(AsyncMutex::new(c)),
        crate::market::registry::MarketPluginRegistry::new(),
        PeerAuth::SameUid { uid: own_uid() },
        env_path,
    )
}

/// `strategy.enable {name, accountId}` is per-account state: enabling
/// `spread_arb` on `paper` must move ONLY paper's enablement set — the
/// `default` row stays silent, and the `.env` gains exactly one
/// `ACCOUNT_PAPER_STRATEGIES` key. Then enabling it on `default` moves
/// only that key. Neither write leaks onto the other account (the
/// pre-#1189 file-first read could resurrect cross-account state).
#[tokio::test]
async fn strategy_enable_is_per_account_isolated_between_default_and_paper() {
    let (core, registry, peer, env_path) = strategy_isolation_fixture().await;

    // Enable on paper.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"strategy.enable","params":{"name":"spread_arb","enabled":true,"accountId":"paper"}}"#.to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the enable must land: {reply}"
    );
    assert_eq!(reply["result"]["accountId"], serde_json::json!("paper"));

    let on_file = env_map(&env_path);
    assert_eq!(
        on_file.get("ACCOUNT_PAPER_STRATEGIES").map(String::as_str),
        Some("spread_arb"),
        "paper's set is on disk: {on_file:?}"
    );
    assert!(
        !on_file.keys().any(|k| k == "ACCOUNT_DEFAULT_STRATEGIES"),
        "the default account's set is untouched by paper's enable: {on_file:?}"
    );

    // The book answers per account (the same-process reload re-derived it).
    let c = core.lock().await;
    let paper_set = c
        .accounts()
        .account(&AccountId::from("paper"))
        .expect("paper")
        .strategies
        .clone();
    let default_set = c
        .accounts()
        .account(&AccountId::from("default"))
        .expect("default")
        .strategies
        .clone();
    drop(c);
    assert_eq!(paper_set, vec!["spread_arb".to_string()], "paper enabled");
    assert!(
        default_set.is_empty(),
        "default unaffected by paper's enable"
    );

    // Enable on default — only default's key appears.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"strategy.enable","params":{"name":"spread_arb","enabled":true,"accountId":"default"}}"#.to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the enable must land: {reply}"
    );
    let on_file = env_map(&env_path);
    assert_eq!(
        on_file
            .get("ACCOUNT_DEFAULT_STRATEGIES")
            .map(String::as_str),
        Some("spread_arb"),
        "the default account's set is on disk: {on_file:?}"
    );
    assert_eq!(
        on_file.get("ACCOUNT_PAPER_STRATEGIES").map(String::as_str),
        Some("spread_arb"),
        "paper's set survives the default's enable: {on_file:?}"
    );

    // And disabling on paper leaves default's set alone (the isolation
    // holds in both directions).
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"strategy.enable","params":{"name":"spread_arb","enabled":false,"accountId":"paper"}}"#.to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the disable must land: {reply}"
    );
    let on_file = env_map(&env_path);
    assert!(
        !on_file.keys().any(|k| k == "ACCOUNT_PAPER_STRATEGIES"),
        "paper's key left the file: {on_file:?}"
    );
    assert_eq!(
        on_file
            .get("ACCOUNT_DEFAULT_STRATEGIES")
            .map(String::as_str),
        Some("spread_arb"),
        "the default account's set survives paper's disable: {on_file:?}"
    );
    let _ = std::fs::remove_dir_all(env_path.parent().unwrap());
}

// ── 4. execution_policy.set 带 accountId 写 POLICY_*_<SFX> 键 ───────────

/// `execution_policy.set` with an `accountId` is the per-account section
/// write: the six `POLICY_*_<SFX>` keys land in the `.env` (never the
/// global names), a sibling account's section is untouched, and the
/// reloaded section view (`execution_policy.get`, the state the kernel
/// will judge under) reflects the write in the same process.
#[tokio::test]
async fn execution_policy_set_writes_the_per_account_suffix_keys_and_the_reload_reflects_them() {
    let (core, registry, peer, env_path) =
        policy_fixture_with_accounts(&["default", "paper"]).await;

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"paper","budgetRatio":"0.25","minBudgetUsd":"5","maxBudgetUsd":"50"}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "the set must land: {reply}");

    let on_file = env_map(&env_path);
    assert_eq!(
        on_file.get("POLICY_BUDGET_RATIO_PAPER").map(String::as_str),
        Some("0.25"),
        "the per-account key carries the ratio: {on_file:?}"
    );
    assert_eq!(
        on_file
            .get("POLICY_MIN_BUDGET_USD_PAPER")
            .map(String::as_str),
        Some("5"),
        "{on_file:?}"
    );
    assert_eq!(
        on_file
            .get("POLICY_MAX_BUDGET_USD_PAPER")
            .map(String::as_str),
        Some("50"),
        "{on_file:?}"
    );
    // The GLOBAL names are not this write's face (the fixture seeds the
    // global ratio 0.10; the account section rides its own keys).
    assert_eq!(
        on_file.get("POLICY_BUDGET_RATIO").map(String::as_str),
        Some("0.10"),
        "the global default is untouched: {on_file:?}"
    );
    assert!(
        !on_file.keys().any(|k| k == "POLICY_BUDGET_RATIO_DEFAULT"),
        "the default account's section is untouched: {on_file:?}"
    );

    // The same-process reload half: execution_policy.get answers the
    // reloaded section — the state the kernel will judge under.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"execution_policy.get","params":{"accountId":"paper"}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "get must answer: {reply}");
    assert_eq!(
        reply["result"]["budgetRatio"],
        serde_json::json!("0.25"),
        "the reloaded section reflects the write: {reply}"
    );

    // Reset sweeps the account's keys (the section falls back to the
    // globals) — and only that account's.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"execution_policy.reset","params":{"accountId":"paper"}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "the reset must land: {reply}");
    let on_file = env_map(&env_path);
    assert!(
        !on_file.keys().any(|k| k.ends_with("_PAPER")),
        "the reset swept paper's section: {on_file:?}"
    );
    assert_eq!(
        on_file.get("POLICY_BUDGET_RATIO").map(String::as_str),
        Some("0.10"),
        "the global default survives the account reset: {on_file:?}"
    );
    let _ = std::fs::remove_dir_all(env_path.parent().unwrap());
}
