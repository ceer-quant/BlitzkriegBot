//! Lua strategy package discovery, validation and the engine adapter (E30
//! #336 task 4; DEV_V0_3 §6.4/§6.6).
//!
//! A Lua package is a DIRECTORY (§6.4): `manifest.json` + the entry script
//! (usually `strategy.lua`) + a required README. The loader is the Lua mirror
//! of [`super::loader`]'s dylib path, with the same refusal discipline:
//!
//! * `manifest.json` must parse and carry `api == "1.0"`;
//! * directory name ≠ `manifest.name` → REFUSE (two identities, §6.4);
//! * `manifest.sha256` MUST match the entry file's actual SHA-256 — the
//!   refusal names BOTH values (§6.4: 不能追溯到源码的策略不予加载; the
//!   mismatch error carries expected and actual for the operator);
//! * `entry` must be a relative path inside the package (no `..`, no
//!   absolute) — a manifest cannot point outside its own directory;
//! * `README.md` missing → WARN, not refuse (docs must not block trading,
//!   §6.4);
//! * `bk_evaluate` missing → REFUSE (the loader-side check; the sandbox
//!   construction re-checks).
//!
//! [`LuaEngineAdapter`] then wraps the [`blitzkrieg_lua_runtime::LuaStrategy`]
//! into the engine's full [`crate::strategies::EngineStrategy`] contract — the
//! same shape a dylib reaches through `ForeignStrategy`. Marshalling is the
//! mirror image of the foreign path: `OrderbookSnapshot` → decimal-STRING
//! `BookUpdate` rows, market rows into the eval context, and the strategy's
//! intent tables adopted back into `TradeSignal`s / exit intents / breaks.
//! A token outside the live round is dropped exactly like the foreign
//! adoption does — the kernel never trades an unknown token.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use rust_decimal::Decimal;
use serde::Deserialize;

use blitzkrieg_lua_runtime::LuaStrategy;
use blitzkrieg_strategy_api::{
    BookUpdate, FreshBook, MarketInfo, RoundContext, RoundInfo, SafeStrategy, StrategyMode,
};

use crate::model::{OrderbookSnapshot, SignalDirection};
use crate::shadow_evolution::knobs::StrategyParams;
use crate::signal::TradeSignal;
use crate::strategies::{EngineStrategy, GateExemptions, StrategyCtx, StrategyExitIntent};

/// The one supported manifest API version (§6.4).
const MANIFEST_API: &str = "1.0";

/// One `manifest.json`. `modes` is OPTIONAL (§7): absent = the strategy does
/// not participate in the load-time compatibility handshake, exactly like a
/// 0.2 dylib without the declaration symbol.
#[derive(Debug, Clone, Deserialize)]
pub struct LuaManifest {
    pub name: String,
    pub version: String,
    pub api: String,
    pub entry: String,
    pub sha256: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
    /// Tunable defaults (`name → {type, default}`): the initial `bk.params()`
    /// bag. Hot-param pushes (the ParamRegistry cell) replace it when a cell
    /// is attached.
    #[serde(default)]
    pub tunables: BTreeMapDefs,
    #[serde(default)]
    pub modes: Option<serde_json::Value>,
}

/// `{"threshold": {"type": "decimal", "default": "0.04"}}` — the §6.4
/// tunables form. Values cross as strings (the decimal-STRING wire rule).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BTreeMapDefs(pub std::collections::BTreeMap<String, TunableDef>);

#[derive(Debug, Clone, Deserialize)]
pub struct TunableDef {
    #[serde(default)]
    pub r#type: String,
    pub default: String,
}

impl BTreeMapDefs {
    /// The flat `name → default-string` bag `bk.params()` starts from.
    pub fn defaults(&self) -> HashMap<String, String> {
        self.0
            .iter()
            .map(|(k, v)| (k.clone(), v.default.clone()))
            .collect()
    }
}

/// Everything the service needs from one validated package.
#[derive(Debug)]
pub struct LoadedLua {
    pub strategy: LuaStrategy,
    pub name: String,
    pub version: String,
    pub declared_modes: Vec<StrategyMode>,
    /// The manifest tunables' defaults — the initial `bk.params()` bag
    /// (§6.4); a ParamRegistry cell replaces it if one is ever attached.
    pub tunables: HashMap<String, String>,
}

/// Every package directory directly under `dir` that carries a
/// `manifest.json`, sorted by path. Depth-0 on purpose: §6.4's layout is
/// flat (one package = one directory), and a recursive walk would let a
/// nested crate checkout smuggle packages into the scan.
pub fn discover_lua_packages(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("manifest.json").is_file())
        .collect();
    out.sort();
    out
}

/// Load and validate one Lua package directory.
pub fn load_lua_package(dir: &Path) -> Result<LoadedLua, String> {
    let manifest_path = dir.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("manifest unreadable: {}: {e}", manifest_path.display()))?;
    let manifest: LuaManifest = serde_json::from_str(&raw)
        .map_err(|e| format!("manifest malformed: {}: {e}", manifest_path.display()))?;

    if manifest.api != MANIFEST_API {
        return Err(format!(
            "manifest api {} unsupported (this kernel speaks {MANIFEST_API})",
            manifest.api
        ));
    }
    // §6.4: two identities are one too many — the directory name IS the
    // registration name.
    let dir_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if manifest.name.is_empty() || manifest.name != dir_name {
        return Err(format!(
            "directory name `{dir_name}` does not match manifest name `{}` — refusing a package with two identities",
            manifest.name
        ));
    }
    // The entry points INSIDE the package, or it points nowhere.
    let entry_rel = Path::new(&manifest.entry);
    if entry_rel.is_absolute() || manifest.entry.split(['/', '\\']).any(|c| c == "..") {
        return Err(format!(
            "manifest entry `{}` escapes the package directory",
            manifest.entry
        ));
    }
    let entry_path = dir.join(&manifest.entry);
    let code = std::fs::read_to_string(&entry_path)
        .map_err(|e| format!("entry script unreadable: {}: {e}", entry_path.display()))?;

    // §6.4: the fingerprint is MANDATORY. The refusal names both values —
    // expected (manifest) and actual (computed) — so an operator can see
    // WHICH side moved without recomputing anything (reverse acceptance F).
    let expected = manifest.sha256.trim().to_lowercase();
    let actual = super::loader::sha256_file(&entry_path)
        .map_err(|e| format!("entry script unreadable: {}: {e}", entry_path.display()))?;
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "manifest sha256 `{expected}` is not a sha256 hex digest (expected a 64-char digest, actual entry digest is `{actual}`)"
        ));
    }
    if expected != actual {
        return Err(format!(
            "sha256 mismatch for {}: manifest expects `{expected}`, actual is `{actual}` — the script on disk is not the script the manifest vouches for",
            manifest.entry
        ));
    }

    // §6.4: README is required — its ABSENCE is a warning, not a refusal.
    if !dir.join("README.md").is_file() {
        tracing::warn!(
            package = %manifest.name,
            "lua package has no README.md (documentation gap, load continues)"
        );
    }

    // §7: the OPTIONAL modes declaration rides the SAME §7.4 validator as a
    // dylib's — an invalid declaration refuses the load, an absent one means
    // "undeclared" (no handshake participation).
    let declared_modes = match &manifest.modes {
        None => Vec::new(),
        Some(value) => {
            // The validator speaks the §2.3 wire (`{"modes":[...]}`); the
            // manifest carries the bare array — wrap for one call.
            let payload = serde_json::json!({ "modes": value }).to_string();
            blitzkrieg_strategy_api::modes::parse_strategy_modes(&payload)
                .map_err(|e| format!("manifest modes invalid: {e}"))?
        }
    };

    let strategy = LuaStrategy::build(
        manifest.name.clone(),
        manifest.version.clone(),
        declared_modes.clone(),
        &code,
    )
    .map_err(|e| format!("lua strategy refused: {e}"))?;

    Ok(LoadedLua {
        strategy,
        name: manifest.name,
        version: manifest.version,
        declared_modes,
        tunables: manifest.tunables.defaults(),
    })
}

/// The engine-side adapter: [`EngineStrategy`] on the outside, one sandboxed
/// Lua state machine on the inside. Same role as `ForeignStrategy` for dylibs.
pub struct LuaEngineAdapter {
    inner: LuaStrategy,
    /// token → asset, refreshed from the live round at every evaluate (labels
    /// `on_book` rows between evaluations).
    assets: HashMap<String, String>,
    /// Last book seen per token — the `fresh = false` rows of the eval
    /// context need SOMETHING to show (`StrategyCtx::fresh_book` answers
    /// `None` for stale books and the stale snapshot is not reachable).
    last_books: HashMap<String, BookUpdate>,
    exit_intents: Vec<StrategyExitIntent>,
    breaks: Vec<(String, Decimal)>,
    /// The strategy's cell of the shared ParamRegistry, when one is attached
    /// (E2-c plumbing; a cell exists only for strategies that declared
    /// evolvable knobs).
    params_cell: Option<Arc<ArcSwap<StrategyParams>>>,
}

impl LuaEngineAdapter {
    pub fn new(inner: LuaStrategy, tunables: HashMap<String, String>) -> Self {
        // Seed `bk.params()` with the manifest defaults; a registry cell (if
        // ever attached) replaces the bag per evaluate.
        let state = inner.state();
        if let Ok(mut st) = state.lock() {
            st.params = tunables;
        }
        Self {
            inner,
            assets: HashMap::new(),
            last_books: HashMap::new(),
            exit_intents: Vec::new(),
            breaks: Vec::new(),
            params_cell: None,
        }
    }

    /// `OrderbookSnapshot` → the §6.5 decimal-STRING book row. Empty sides →
    /// `None` fields, never "0".
    fn book_update(&self, token_id: &str, snap: &OrderbookSnapshot) -> BookUpdate {
        let side = |levels: &[(Decimal, Decimal)]| {
            if levels.is_empty() {
                None
            } else {
                Some(levels[0].0.to_string())
            }
        };
        BookUpdate {
            symbol: token_id.to_string(),
            asset: self.assets.get(token_id).cloned().unwrap_or_default(),
            best_bid: side(&snap.bids),
            best_ask: side(&snap.asks),
            mid: Some(snap.mid_price.to_string()),
            bid_depth: Some(snap.bid_depth.to_string()),
            ask_depth: Some(snap.ask_depth.to_string()),
            obi: Some(snap.obi.to_string()),
            spread: Some(snap.spread.to_string()),
            spread_pct: Some(snap.spread_pct.to_string()),
            timestamp_ms: snap.timestamp,
            bid_levels: snap.bids.len(),
            ask_levels: snap.asks.len(),
        }
    }
}

impl EngineStrategy for LuaEngineAdapter {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, _now_ms: i64) {
        let update = self.book_update(token_id, snap);
        self.last_books.insert(token_id.to_string(), update.clone());
        self.inner.on_book(&update);
    }

    fn on_round(&mut self, slot: i64, time_left_sec: i64, now_ms: i64) {
        self.inner.on_round(RoundInfo {
            slot,
            time_left_sec,
            now_ms,
        });
    }

    fn take_breaks(&mut self) -> Vec<(String, Decimal)> {
        std::mem::take(&mut self.breaks)
    }

    // E29 (§10.4): forward CLOSED bars into the Lua runtime — LuaStrategy's
    // `on_kline` records the bar for `bk.kline` reads and calls the script's
    // `bk_on_kline`. Same `Kline` type on both sides (one re-export, no
    // mirror), so this is a pass-through, not a translation.
    fn on_kline(&mut self, kline: &crate::kline::Kline) {
        self.inner.on_kline(kline);
    }

    fn confirmed_tokens(&self) -> HashSet<String> {
        self.inner.confirmed_tokens().into_iter().collect()
    }

    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        // Refresh the token→asset labels and stage the eval context: the
        // freshness verdict per token is the data form of the host's
        // `fresh_book` gate (§6.6) — fresh rows carry the live snapshot,
        // stale rows carry the last known book stamped `fresh = false`.
        let mut books: Vec<FreshBook> = Vec::new();
        for m in ctx.markets() {
            self.assets.insert(m.up_token_id.clone(), m.asset.clone());
            self.assets.insert(m.down_token_id.clone(), m.asset.clone());
            for token in [&m.up_token_id, &m.down_token_id] {
                match ctx.fresh_book(token) {
                    Some(snap) => {
                        let update = self.book_update(token, &snap);
                        self.last_books.insert(token.to_string(), update.clone());
                        books.push(FreshBook {
                            book: update,
                            fresh: true,
                        });
                    }
                    None => {
                        let update = self.last_books.get(token).cloned().unwrap_or(BookUpdate {
                            symbol: token.to_string(),
                            asset: self.assets.get(token).cloned().unwrap_or_default(),
                            ..Default::default()
                        });
                        books.push(FreshBook {
                            book: update,
                            fresh: false,
                        });
                    }
                }
            }
        }
        // Hot parameters: the registry cell (when attached) replaces the
        // manifest-default bag for this cycle (§6.6: same ParamRegistry).
        if let Some(cell) = &self.params_cell {
            let params = (**cell.load()).clone();
            if let Ok(mut st) = self.inner.state().lock() {
                st.params = params
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
            }
        }
        self.inner.on_eval_books(&books);

        let round_ctx = RoundContext {
            round: RoundInfo {
                slot: ctx.round_slot(),
                time_left_sec: ctx.time_left_sec(),
                now_ms: ctx.now_ms(),
            },
            markets: ctx
                .markets()
                .iter()
                .map(|m| MarketInfo {
                    asset: m.asset.clone(),
                    condition_id: m.condition_id.clone(),
                    up_token: m.up_token_id.clone(),
                    down_token: m.down_token_id.clone(),
                    expires_at_ms: m.expires_at_ms,
                    slot: m.round_slot,
                    neg_risk: m.neg_risk,
                })
                .collect(),
        };
        let intents = self.inner.evaluate(&round_ctx);

        // Adopt the intent tables — the mirror of `foreign.rs`'s
        // `adopt_eval_json`: exits/breaks buffer here, entries resolve to
        // round tokens and become `TradeSignal`s. A token outside this round
        // is dropped (the kernel never trades an unknown token).
        for e in intents.exits {
            self.exit_intents.push(StrategyExitIntent {
                token_id: e.token,
                reason: if e.reason.is_empty() {
                    "strategy".to_string()
                } else {
                    e.reason
                },
            });
        }
        for b in intents.breaks {
            let price = Decimal::from_str_exact(&b.broken_price).unwrap_or(Decimal::ZERO);
            self.breaks.push((b.token, price));
        }

        let mut candidates = Vec::new();
        for entry in intents.entries {
            let Ok(price) = Decimal::from_str_exact(&entry.price) else {
                continue;
            };
            let shares = entry
                .shares
                .as_deref()
                .and_then(|s| Decimal::from_str_exact(s).ok());
            let reason = if entry.reason.is_empty() {
                "strategy entry".to_string()
            } else {
                entry.reason
            };
            for m in ctx.markets() {
                if m.up_token_id == entry.token {
                    candidates.push(TradeSignal {
                        strategy: self.name().to_string(),
                        asset: m.asset.clone(),
                        direction: SignalDirection::Up,
                        token_id: m.up_token_id.clone(),
                        condition_id: m.condition_id.clone(),
                        price,
                        reason: reason.clone(),
                        shares,
                    });
                    break;
                }
                if m.down_token_id == entry.token {
                    candidates.push(TradeSignal {
                        strategy: self.name().to_string(),
                        asset: m.asset.clone(),
                        direction: SignalDirection::Down,
                        token_id: m.down_token_id.clone(),
                        condition_id: m.condition_id.clone(),
                        price,
                        reason: reason.clone(),
                        shares,
                    });
                    break;
                }
            }
        }
        candidates
    }

    fn gate_exemptions(&self) -> GateExemptions {
        GateExemptions::none()
    }

    fn take_exit_intents(&mut self) -> Vec<StrategyExitIntent> {
        std::mem::take(&mut self.exit_intents)
    }

    fn diagnostics(&self, _ctx: &StrategyCtx<'_>) -> Vec<serde_json::Value> {
        self.inner.diagnostics()
    }

    fn set_hot_params(&mut self, registry: Option<Arc<crate::shadow_evolution::ParamRegistry>>) {
        // Resolve only OUR cell (the foreign.rs pattern): no declared knobs →
        // no cell → the manifest defaults keep answering `bk.params()`.
        self.params_cell = registry.as_ref().and_then(|r| r.handle_for(self.name()));
    }

    fn config_view_json(&self) -> Option<String> {
        // The Lua strategy declares no config of its own yet — the view names
        // the runtime and version so an operator sees what every strategy is
        // actually running (the ONLY config-observability surface).
        match self.inner.config_view() {
            Some(v) => Some(v.to_string()),
            None => Some(
                serde_json::json!({
                    "runtime": "lua5.4",
                    "version": self.inner.version(),
                })
                .to_string(),
            ),
        }
    }

    /// §6.3: the one-shot poison alert. Drained by the host exactly once and
    /// raised as a single RISK_ALERT naming this strategy.
    fn poison_alert(&mut self) -> Option<String> {
        self.inner.take_poison_alert()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRATEGY_LUA: &str =
        "function bk_evaluate() return { entries = {}, exits = {}, breaks = {} } end\n";

    fn write_package(dir: &Path, name: &str, manifest_overrides: &[(&str, String)]) -> PathBuf {
        std::fs::create_dir_all(dir).expect("mkdir");
        std::fs::write(dir.join("strategy.lua"), STRATEGY_LUA).expect("write entry");
        std::fs::write(dir.join("README.md"), "# test package\n").expect("write readme");
        let digest = super::super::loader::sha256_file(&dir.join("strategy.lua")).expect("sha");
        let mut manifest = serde_json::json!({
            "name": name,
            "version": "0.1.0",
            "api": "1.0",
            "entry": "strategy.lua",
            "sha256": digest,
            "author": "test",
            "description": "test",
        });
        if let serde_json::Value::Object(map) = &mut manifest {
            for (k, v) in manifest_overrides {
                map.insert((*k).to_string(), serde_json::Value::String(v.clone()));
            }
        }
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).expect("write manifest");
        dir.to_path_buf()
    }

    /// The happy path: a well-formed package loads and reports its identity.
    #[test]
    fn well_formed_package_loads() {
        let tmp = std::env::temp_dir().join(format!("bk-lua-ok-{}", std::process::id()));
        let pkg = write_package(&tmp.join("lua_ok"), "lua_ok", &[]);
        let loaded = load_lua_package(&pkg).expect("loads");
        assert_eq!(loaded.name, "lua_ok");
        assert_eq!(loaded.version, "0.1.0");
        assert!(loaded.declared_modes.is_empty());
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// §6.4: directory name ≠ manifest name → refuse (two identities).
    #[test]
    fn directory_name_mismatch_refuses() {
        let tmp = std::env::temp_dir().join(format!("bk-lua-name-{}", std::process::id()));
        let pkg = write_package(&tmp.join("dir_a"), "manifest_b", &[]);
        let err = load_lua_package(&pkg).expect_err("refused");
        assert!(err.contains("two identities"), "got: {err}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Reverse acceptance F: a tampered sha256 refuses with BOTH values in
    /// the message.
    #[test]
    fn sha256_mismatch_refuses_with_expected_and_actual() {
        let tmp = std::env::temp_dir().join(format!("bk-lua-sha-{}", std::process::id()));
        let fake = "0".repeat(64);
        let pkg = write_package(&tmp.join("lua_sha"), "lua_sha", &[("sha256", fake.clone())]);
        let err = load_lua_package(&pkg).expect_err("refused");
        assert!(err.contains("sha256 mismatch"), "got: {err}");
        assert!(err.contains(&fake), "expected value present: {err}");
        // The ACTUAL digest is the real one of the untouched script.
        let actual = super::super::loader::sha256_file(&pkg.join("strategy.lua")).expect("sha");
        assert!(err.contains(&actual), "actual value present: {err}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A non-hex "sha256" is refused as malformed (not as a mismatch).
    #[test]
    fn malformed_sha256_refuses() {
        let tmp = std::env::temp_dir().join(format!("bk-lua-badsha-{}", std::process::id()));
        let pkg = write_package(
            &tmp.join("lua_bad"),
            "lua_bad",
            &[("sha256", "nope".into())],
        );
        let err = load_lua_package(&pkg).expect_err("refused");
        assert!(err.contains("not a sha256 hex digest"), "got: {err}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// An entry path escaping the package is refused.
    #[test]
    fn entry_path_escape_refuses() {
        let tmp = std::env::temp_dir().join(format!("bk-lua-esc-{}", std::process::id()));
        let pkg = write_package(
            &tmp.join("lua_esc"),
            "lua_esc",
            &[("entry", "../other.lua".into())],
        );
        let err = load_lua_package(&pkg).expect_err("refused");
        assert!(err.contains("escapes the package"), "got: {err}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A wrong api version is refused.
    #[test]
    fn unsupported_api_refuses() {
        let tmp = std::env::temp_dir().join(format!("bk-lua-api-{}", std::process::id()));
        let pkg = write_package(&tmp.join("lua_api"), "lua_api", &[("api", "2.0".into())]);
        let err = load_lua_package(&pkg).expect_err("refused");
        assert!(err.contains("unsupported"), "got: {err}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A script without `bk_evaluate` is refused (the required entry point).
    #[test]
    fn missing_evaluate_refuses() {
        let tmp = std::env::temp_dir().join(format!("bk-lua-noeval-{}", std::process::id()));
        let pkg = write_package(&tmp.join("lua_no"), "lua_no", &[]);
        std::fs::write(pkg.join("strategy.lua"), "x = 1\n").expect("rewrite");
        // The manifest sha256 now matches the REWRITTEN file only if
        // recomputed — rewrite it so the refusal is about the entry point.
        let digest = super::super::loader::sha256_file(&pkg.join("strategy.lua")).expect("sha");
        let manifest_path = pkg.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("read"))
                .expect("json");
        manifest["sha256"] = serde_json::Value::String(digest);
        std::fs::write(&manifest_path, manifest.to_string()).expect("write");
        let err = load_lua_package(&pkg).expect_err("refused");
        assert!(err.contains("bk_evaluate"), "got: {err}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Discovery lists only manifest-carrying directories, sorted.
    #[test]
    fn discovery_lists_packages_only() {
        let tmp = std::env::temp_dir().join(format!("bk-lua-disc-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("plain_dir")).expect("mkdir");
        write_package(&tmp.join("zeta"), "zeta", &[]);
        write_package(&tmp.join("alpha"), "alpha", &[]);
        std::fs::write(tmp.join("loose.txt"), "not a package").expect("write");
        let found = discover_lua_packages(&tmp);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["alpha", "zeta"], "sorted, packages only");
        std::fs::remove_dir_all(&tmp).ok();
    }
}
