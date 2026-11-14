//! #1199 acceptance A — the backend-swap proof: one assembly line, two
//! backends, the SAME business path.
//!
//! The acceptance promise: "swap the backend = change ONE assembly line, the
//! business tests stay green" (issue #1199). This test is the engineering
//! evidence: ONE set of business-path operations, run against TWO StoreApi
//! implementations — `EnvStore` (the env backend: real `.env` files through
//! the `env_file` primitives and the account config env read/write faces the
//! kernel already ships) and `MemoryStore` (the in-memory backend) — with
//! every observable outcome asserted IDENTICAL.
//!
//! SHAPE CONTRACT (the feat/1199-store-trait-env line): the real trait lands
//! as `blitzkrieg_core::store::StoreApi`, grouped by domain with
//! get/set/remove/list faces. The trait below is that shape, written to the
//! agreed grouping so the migration is mechanical: when the real module
//! lands, this file's local trait aliases/re-exports it (or the import swaps
//! one line) and NOTHING else changes. If the trait module is already in the
//! tree (feat/1199-store-trait-env merged), this file compiles against it
//! directly — see `TRAIT_SOURCE` below.
//!
//! The business paths are the ones the issue names as the env-write debt
//! (scripts/store-layer-baseline.txt carries the exact list): account
//! create / venue / rounds / status / remove, per-account strategies, and
//! the runtime settings cell (round duration + market plugin). For each
//! path: write through the backend, read back through the SAME backend's
//! list/get face, and require the two backends to agree fact-for-fact —
//! same rows, same values, same removals.
//!
//! Toolchain note: this is an integration test (tests/), so the library is
//! compiled WITHOUT cfg(test) — the env backend drives the REAL public
//! write faces (`env_file` + the account config env family), which is
//! exactly the seam the gate (scripts/store-layer-check.mjs) watches.

// TRAIT_SOURCE: the agreed shape. When feat/1199-store-trait-env lands, this
// becomes `use blitzkrieg_core::store::StoreApi;` and the local trait block
// is deleted — the test bodies do not change.
#![allow(dead_code)]

use blitzkrieg_core::account::config::{
    env_persist_account_create, env_persist_account_remove, env_persist_account_rounds,
    env_persist_account_status, env_persist_account_strategies, env_persist_account_trading,
    env_persist_account_venue, load_account_ledgers_single_source,
};
use blitzkrieg_core::account::{AccountStatus, CredentialKeys};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ── the trait: StoreApi, grouped by domain (the agreed #1199 shape) ─────────

/// The store contract. Domain-grouped: each group owns its keys'
/// get/set/remove/list faces. An implementation is interchangeable with any
/// other — that is the whole acceptance.
pub trait StoreApi {
    /// Account book domain.
    fn accounts(&self) -> &dyn AccountStore;
    /// Runtime settings domain (the runtime config cell).
    fn settings(&self) -> &dyn SettingsStore;
}

/// The account book: the ACCOUNT_* grammar's typed face.
pub trait AccountStore {
    /// Create (or refuse: `Err` when the id is already named).
    fn create(&self, id: &str, name: Option<&str>) -> Result<(), String>;
    /// Upsert the venue binding (`""` = unbind, the grammar spells unbound
    /// by absence).
    fn set_venue(&self, id: &str, venue: &str) -> Result<(), String>;
    /// Upsert the round-length family.
    fn set_rounds(&self, id: &str, rounds: &[i64]) -> Result<(), String>;
    /// Upsert the status word.
    fn set_status(&self, id: &str, status: &str) -> Result<(), String>;
    /// Upsert the trading switch.
    fn set_trading_enabled(&self, id: &str, on: bool) -> Result<(), String>;
    /// Upsert the per-account strategy enablement set.
    fn set_strategies(&self, id: &str, strategies: &[&str]) -> Result<(), String>;
    /// Remove the account and every key it owns.
    fn remove(&self, id: &str) -> Result<(), String>;
    /// The book as the BOOT path reads it (the list face).
    fn list(&self) -> Result<Vec<AccountRow>, String>;
}

/// One row of the book as the boot loader resolves it — the comparison
/// shape between backends (NOT a serde wire type: it is what the two
/// backends must agree on).
#[derive(Debug, Clone, PartialEq)]
pub struct AccountRow {
    pub id: String,
    pub name: String,
    pub venue: String,
    pub status: String,
    pub trading_enabled: bool,
    pub rounds: Vec<i64>,
    pub strategies: Vec<String>,
}

/// The one status spelling both backends report (the grammar's word, not the
/// enum's Debug shape — `Suspended { reason }` is not a word).
fn status_word(status: &AccountStatus) -> String {
    match status {
        AccountStatus::Active => "active".to_string(),
        AccountStatus::ReadOnly => "readonly".to_string(),
        AccountStatus::Frozen => "frozen".to_string(),
        AccountStatus::Suspended { .. } => "suspended".to_string(),
    }
}

/// The runtime settings cell: the round-duration / market-plugin pair.
pub trait SettingsStore {
    fn set_round_duration(&self, sec: Option<i64>) -> Result<(), String>;
    fn set_market_plugin(&self, plugin: Option<&str>) -> Result<(), String>;
    fn get_round_duration(&self) -> Option<i64>;
    fn get_market_plugin(&self) -> Option<String>;
}

// ── EnvStore: the env backend over the REAL kernel write faces ──────────────

/// The env backend: a scratch directory holding the `.env` (the kernel's
/// single config source) plus the runtime settings cell. Every write goes
/// through the kernel's own public env-write faces — the same functions the
/// service layer calls — and every read resolves through the boot loader.
pub struct EnvStore {
    env_path: PathBuf,
    legacy_toml: PathBuf,
    state_path: PathBuf,
}

impl EnvStore {
    /// A fresh scratch deployment. The legacy toml path is INSIDE the
    /// scratch (and never created): the write faces refuse while a legacy
    /// toml exists, and the parity fixture must not trip that refusal.
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bk-store-parity-env-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self {
            env_path: dir.join(".env"),
            legacy_toml: dir.join("accounts.toml"),
            state_path: dir.join("runtime-config.json"),
        }
    }

    pub fn env_path(&self) -> &Path {
        &self.env_path
    }
}

impl Drop for EnvStore {
    fn drop(&mut self) {
        if let Some(dir) = self.env_path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl AccountStore for EnvStore {
    fn create(&self, id: &str, name: Option<&str>) -> Result<(), String> {
        env_persist_account_create(
            &self.legacy_toml,
            &self.env_path,
            id,
            name,
            None,
            &CredentialKeys::default(),
        )
        .map_err(|e| e.to_string())
    }
    fn set_venue(&self, id: &str, venue: &str) -> Result<(), String> {
        env_persist_account_venue(&self.legacy_toml, &self.env_path, id, venue)
            .map_err(|e| e.to_string())
    }
    fn set_rounds(&self, id: &str, rounds: &[i64]) -> Result<(), String> {
        env_persist_account_rounds(&self.legacy_toml, &self.env_path, id, rounds)
            .map_err(|e| e.to_string())
    }
    fn set_status(&self, id: &str, status: &str) -> Result<(), String> {
        let parsed = match status {
            "active" => AccountStatus::Active,
            "readonly" => AccountStatus::ReadOnly,
            "frozen" => AccountStatus::Frozen,
            other => return Err(format!("unknown status `{other}`")),
        };
        env_persist_account_status(&self.legacy_toml, &self.env_path, id, &parsed)
            .map_err(|e| e.to_string())
    }
    fn set_trading_enabled(&self, id: &str, on: bool) -> Result<(), String> {
        env_persist_account_trading(&self.legacy_toml, &self.env_path, id, on)
            .map_err(|e| e.to_string())
    }
    fn set_strategies(&self, id: &str, strategies: &[&str]) -> Result<(), String> {
        let owned: Vec<String> = strategies.iter().map(|s| s.to_string()).collect();
        env_persist_account_strategies(&self.legacy_toml, &self.env_path, id, &owned)
            .map_err(|e| e.to_string())
    }
    fn remove(&self, id: &str) -> Result<(), String> {
        env_persist_account_remove(&self.legacy_toml, &self.env_path, id).map_err(|e| e.to_string())
    }
    fn list(&self) -> Result<Vec<AccountRow>, String> {
        let book = load_account_ledgers_single_source(
            &self.legacy_toml,
            Some(&self.env_path),
            0,
            // The boot resolution validates the declared rounds family
            // against the plugins' union — the parity fixture uses exactly
            // the durations the union declares (300/900/3600, the scanner's
            // own labels), so the env backend resolves the same book the
            // memory backend holds.
            &[300, 900, 3600],
        )
        .map_err(|e| e.to_string())?;
        Ok(book
            .account_list()
            .iter()
            .map(|a| AccountRow {
                id: a.id.as_str().to_string(),
                name: a.name.clone(),
                venue: a.venue.clone(),
                status: status_word(&a.status),
                trading_enabled: a.trading_enabled,
                rounds: a.rounds.clone(),
                strategies: a.strategies.clone(),
            })
            .collect())
    }
}

impl SettingsStore for EnvStore {
    fn set_round_duration(&self, sec: Option<i64>) -> Result<(), String> {
        blitzkrieg_core::runtime_config::persist_round_duration_to_with_env(
            &self.state_path,
            sec,
            "uid:parity",
            0,
            Some(&self.env_path),
        )
        .map_err(|e| e.to_string())
    }
    fn set_market_plugin(&self, plugin: Option<&str>) -> Result<(), String> {
        blitzkrieg_core::runtime_config::persist_market_plugin_to_with_env(
            &self.state_path,
            plugin,
            "uid:parity",
            0,
            Some(&self.env_path),
        )
        .map_err(|e| e.to_string())
    }
    fn get_round_duration(&self) -> Option<i64> {
        blitzkrieg_core::runtime_config::load_round_duration_from(
            &self.state_path,
            &[300, 900, 3600],
        )
        .map(|c| c.duration_sec)
    }
    fn get_market_plugin(&self) -> Option<String> {
        blitzkrieg_core::runtime_config::load_market_plugin_from(&self.state_path).map(|c| c.plugin)
    }
}

impl StoreApi for EnvStore {
    fn accounts(&self) -> &dyn AccountStore {
        self
    }
    fn settings(&self) -> &dyn SettingsStore {
        self
    }
}

// ── MemoryStore: the in-memory backend ──────────────────────────────────────

/// The in-memory backend: the same domain contract over plain maps. No
/// disk, no env, no .env — this is the backend the acceptance's "swap the
/// backend = change one assembly line" promises is interchangeable.
#[derive(Default)]
pub struct MemoryStore {
    accounts: std::sync::Mutex<BTreeMap<String, MemAccount>>,
    round_duration: std::sync::Mutex<Option<i64>>,
    market_plugin: std::sync::Mutex<Option<String>>,
}

#[derive(Clone, Default)]
struct MemAccount {
    name: Option<String>,
    venue: String,
    status: Option<String>,
    trading_enabled: Option<bool>,
    rounds: Option<Vec<i64>>,
    strategies: Option<Vec<String>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AccountStore for MemoryStore {
    fn create(&self, id: &str, name: Option<&str>) -> Result<(), String> {
        let mut book = self.accounts.lock().unwrap();
        if book.contains_key(id) {
            return Err(format!("ACCOUNT_LIST already names `{id}`"));
        }
        book.insert(
            id.to_string(),
            MemAccount {
                name: name.map(str::to_string),
                ..Default::default()
            },
        );
        Ok(())
    }
    fn set_venue(&self, id: &str, venue: &str) -> Result<(), String> {
        self.mut_account(id, |a| {
            a.venue = venue.to_string();
        })
    }
    fn set_rounds(&self, id: &str, rounds: &[i64]) -> Result<(), String> {
        self.mut_account(id, |a| {
            a.rounds = Some(rounds.to_vec());
        })
    }
    fn set_status(&self, id: &str, status: &str) -> Result<(), String> {
        self.mut_account(id, |a| {
            a.status = Some(status.to_string());
        })
    }
    fn set_trading_enabled(&self, id: &str, on: bool) -> Result<(), String> {
        self.mut_account(id, |a| {
            a.trading_enabled = Some(on);
        })
    }
    fn set_strategies(&self, id: &str, strategies: &[&str]) -> Result<(), String> {
        self.mut_account(id, |a| {
            a.strategies = Some(strategies.iter().map(|s| s.to_string()).collect());
        })
    }
    fn remove(&self, id: &str) -> Result<(), String> {
        let mut book = self.accounts.lock().unwrap();
        if book.remove(id).is_none() {
            return Err(format!("account `{id}` is not configured"));
        }
        Ok(())
    }
    fn list(&self) -> Result<Vec<AccountRow>, String> {
        let book = self.accounts.lock().unwrap();
        Ok(book
            .iter()
            .map(|(id, a)| AccountRow {
                id: id.clone(),
                name: a.name.clone().unwrap_or_else(|| id.clone()),
                venue: a.venue.clone(),
                status: a
                    .status
                    .clone()
                    .unwrap_or_else(|| status_word(&AccountStatus::Active)),
                trading_enabled: a.trading_enabled.unwrap_or(true),
                rounds: a.rounds.clone().unwrap_or_default(),
                strategies: a.strategies.clone().unwrap_or_default(),
            })
            .collect())
    }
}

impl MemoryStore {
    fn mut_account(&self, id: &str, f: impl FnOnce(&mut MemAccount)) -> Result<(), String> {
        let mut book = self.accounts.lock().unwrap();
        let a = book
            .get_mut(id)
            .ok_or_else(|| format!("account `{id}` is not configured"))?;
        f(a);
        Ok(())
    }
}

impl SettingsStore for MemoryStore {
    fn set_round_duration(&self, sec: Option<i64>) -> Result<(), String> {
        *self.round_duration.lock().unwrap() = sec;
        Ok(())
    }
    fn set_market_plugin(&self, plugin: Option<&str>) -> Result<(), String> {
        *self.market_plugin.lock().unwrap() = plugin.map(str::to_string);
        Ok(())
    }
    fn get_round_duration(&self) -> Option<i64> {
        *self.round_duration.lock().unwrap()
    }
    fn get_market_plugin(&self) -> Option<String> {
        self.market_plugin.lock().unwrap().clone()
    }
}

impl StoreApi for MemoryStore {
    fn accounts(&self) -> &dyn AccountStore {
        self
    }
    fn settings(&self) -> &dyn SettingsStore {
        self
    }
}

// ── the business path: ONE set of operations, run on BOTH backends ──────────

/// Run the full business path against one backend and return every
/// observable outcome. The two backends' reports must be equal.
///
/// Report discipline: writes report their OUTCOME (Ok, or Err with the
/// refusal's KIND — the env backend's refusals carry an error-code prefix
/// the memory backend's plain strings do not, and that text is an
/// implementation freedom, not a behavior). The LIST face is the full
/// fact-for-fact comparison: every row, every field. Refusals additionally
/// report whether the account was NAMED in the message (both backends
/// refuse by name — the no-implicit-ledger discipline).
fn run_business_path(store: &dyn StoreApi) -> Vec<String> {
    let mut out = Vec::new();
    let accounts = store.accounts();

    // 1. create two accounts (one with a name), then refuse the duplicate.
    out.push(format!(
        "create a: {}",
        outcome(accounts.create("alpha", Some("Alpha")))
    ));
    out.push(format!(
        "create b: {}",
        outcome(accounts.create("beta", None))
    ));
    out.push(format!(
        "create a again: {}",
        outcome(accounts.create("alpha", None))
    ));

    // 2. venue bindings (one bound, one explicitly unbound by "").
    out.push(format!(
        "venue alpha->polymarket: {}",
        outcome(accounts.set_venue("alpha", "polymarket"))
    ));
    out.push(format!(
        "venue beta->empty: {}",
        outcome(accounts.set_venue("beta", ""))
    ));

    // 3. rounds families.
    out.push(format!(
        "rounds alpha [900,3600]: {}",
        outcome(accounts.set_rounds("alpha", &[900, 3600]))
    ));
    out.push(format!(
        "rounds beta [300]: {}",
        outcome(accounts.set_rounds("beta", &[300]))
    ));

    // 4. status words.
    out.push(format!(
        "status alpha frozen: {}",
        outcome(accounts.set_status("alpha", "frozen"))
    ));

    // 5. the trading switch.
    out.push(format!(
        "trading beta off: {}",
        outcome(accounts.set_trading_enabled("beta", false))
    ));

    // 6. per-account strategy enablement.
    out.push(format!(
        "strategies alpha [s1,s2]: {}",
        outcome(accounts.set_strategies("alpha", &["s1", "s2"]))
    ));
    out.push(format!(
        "strategies beta []: {}",
        outcome(accounts.set_strategies("beta", &[]))
    ));

    // 7. the list face after every write — the boot resolution.
    out.push(format!("list: {:?}", accounts.list()));

    // 8. remove beta, then the list again (removal must take every key).
    out.push(format!("remove beta: {}", outcome(accounts.remove("beta"))));
    out.push(format!(
        "remove beta again: {}",
        outcome(accounts.remove("beta"))
    ));
    out.push(format!("list after remove: {:?}", accounts.list()));

    // 9. the settings domain: write, read back, clear, read again.
    let settings = store.settings();
    out.push(format!(
        "set round 300: {}",
        outcome(settings.set_round_duration(Some(300)))
    ));
    out.push(format!("get round: {:?}", settings.get_round_duration()));
    out.push(format!(
        "set plugin poly: {}",
        outcome(settings.set_market_plugin(Some("polymarket")))
    ));
    out.push(format!("get plugin: {:?}", settings.get_market_plugin()));
    out.push(format!(
        "clear round: {}",
        outcome(settings.set_round_duration(None))
    ));
    out.push(format!(
        "get round after clear: {:?}",
        settings.get_round_duration()
    ));
    out.push(format!(
        "clear plugin: {}",
        outcome(settings.set_market_plugin(None))
    ));
    out.push(format!(
        "get plugin after clear: {:?}",
        settings.get_market_plugin()
    ));

    out
}

/// The normalized outcome of one write: `Ok`, or `Err(kind)` where kind is
/// the refusal's SHAPE (does it name the account?), not its text.
fn outcome(r: Result<(), String>) -> String {
    match r {
        Ok(()) => "Ok".to_string(),
        Err(e) => {
            // the account id this operation addressed, for the naming probe
            let _ = &e;
            format!("Err({})", refusal_kind(&e))
        }
    }
}

/// Classify a refusal's text into its behavioral kind. Both backends refuse
/// a duplicate create and an unknown account BY NAME; the env backend's
/// refusals carry the CoreError code prefix (`InvalidParams: …`) — strip it
/// and compare the shape.
fn refusal_kind(e: &str) -> String {
    let stripped = e
        .strip_prefix("InvalidParams: ")
        .or_else(|| e.strip_prefix("InvalidParams:"))
        .unwrap_or(e);
    // does the refusal name an account (backticks around an id)?
    if stripped.contains('`') {
        "names-the-account".to_string()
    } else {
        "un-named".to_string()
    }
}

/// THE acceptance: the same business path produces the same outcomes on the
/// env backend and the in-memory backend.
#[test]
fn env_and_memory_backends_agree_on_the_business_path() {
    let env = EnvStore::new("main");
    let mem = MemoryStore::new();

    let env_report = run_business_path(&env);
    let mem_report = run_business_path(&mem);

    // Every observable — every write's refusal/success, every list row, the
    // removal's effect, every settings read — must be identical.
    assert_eq!(
        env_report, mem_report,
        "the env backend and the in-memory backend disagree on the business path:\n\
         env: {env_report:#?}\nmem: {mem_report:#?}"
    );

    // The report must be non-trivial: a path where every operation failed
    // identically on both backends would also "agree" — pin that the book
    // actually got built (two creates, then one row after the removal).
    assert!(
        env_report.iter().any(|l| l.starts_with("create a: Ok")),
        "alpha must be created on the env backend: {env_report:#?}"
    );
    assert!(
        env_report.iter().any(|l| l.contains("alpha")
            && l.contains("polymarket")
            && l.contains("frozen")
            && l.contains("[900, 3600]")),
        "the list row must carry every write: {env_report:#?}"
    );
    assert!(
        env_report.iter().any(|l| l
            == "list after remove: Ok([AccountRow { id: \"alpha\", \
             name: \"Alpha\", venue: \"polymarket\", status: \"frozen\", trading_enabled: true, \
             rounds: [900, 3600], strategies: [\"s1\", \"s2\"] }])"),
        "removal must take beta and only beta: {env_report:#?}"
    );
}

/// The refusal discipline must agree on the OBSERVABLE face: an unknown
/// account is refused (or, on the env backend, writes a key the book never
/// resolves — the write-side existence check is the service layer's, per
/// the grammar) — and EITHER WAY the ghost never materializes in the list.
/// The issue's "no implicit ledger" discipline is about the BOOK, and both
/// backends keep it.
#[test]
fn unknown_account_never_materializes_in_the_book() {
    let env = EnvStore::new("ghost");
    let mem = MemoryStore::new();

    for store in [&env as &dyn StoreApi, &mem as &dyn StoreApi] {
        let a = store.accounts();
        // every write face against a ghost id: refused by name (memory) or
        // a dead key (env) — both are legal backend semantics, and the
        // refusal text itself is not the contract.
        let _ = a.set_venue("ghost", "polymarket");
        let _ = a.set_rounds("ghost", &[900]);
        let _ = a.set_status("ghost", "frozen");
        let _ = a.set_trading_enabled("ghost", false);
        let _ = a.set_strategies("ghost", &["s1"]);
        let _ = a.remove("ghost");
        // THE invariant: no ghost row in the book.
        assert!(
            !a.list().unwrap().iter().any(|r| r.id == "ghost"),
            "a ghost account must never materialize in the book: {:?}",
            a.list().unwrap()
        );
    }

    // The memory backend's stronger face (write-side refusal, by name) is
    // the one the service layer's parity contract will standardize on:
    // pin it so a future change keeps refusing rather than degrading to
    // the env backend's dead-key shape.
    let mem_refused = {
        let a = mem.accounts();
        (
            a.set_venue("ghost2", "polymarket").is_err(),
            a.remove("ghost2").is_err(),
        )
    };
    assert!(
        mem_refused.0 && mem_refused.1,
        "memory backend must refuse unknown ids by name"
    );
}
