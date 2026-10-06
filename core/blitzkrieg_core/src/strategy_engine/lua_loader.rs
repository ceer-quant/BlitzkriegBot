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
use std::sync::{Arc, Mutex, OnceLock};

use arc_swap::ArcSwap;
use rust_decimal::Decimal;
use serde::Deserialize;

use blitzkrieg_lua_runtime::{FeeScheduleView, LuaStrategy};
use blitzkrieg_strategy_api::{
    BookUpdate, FreshBook, MarketInfo, RoundContext, RoundInfo, SafeStrategy, StrategyMode,
};

use crate::model::{OrderbookSnapshot, SignalDirection};
use crate::shadow_evolution::knobs::{KnobSpec, StrategyParams};
use crate::signal::TradeSignal;
use crate::strategies::shadow_twin::ShadowFactory;
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
    /// Hold-to-settlement declaration: the strategy's positions are meant to
    /// be COLLECTED at expiry — redeemed on settlement or MERGED as complete
    /// UP+DOWN pairs — never sold on the exit ladder. Gates two host
    /// behaviours: the pair-completion entry exemption ("Already in {asset}"
    /// waived for the opposite leg of the same condition) and the exit-ladder
    /// skip (dry/read-only). The merge channel itself needs no declaration —
    /// it is intent-driven, and merging requires the account to actually hold
    /// both legs (the same authority an on-chain wallet needs to burn its own
    /// pair). Default false — existing packages keep today's behaviour.
    #[serde(default)]
    pub holds_to_settlement: bool,
    /// Entry-gate exemptions (E2-b, the dylib declaration surface's Lua
    /// mirror). Default: nothing declared = fully gated.
    #[serde(default)]
    pub gate_exemptions: ManifestGateExemptions,
}

/// The manifest's spelling of [`crate::strategies::GateExemptions`] — that
/// type derives no serde, so the wire gets its own struct and the loader
/// converts once.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ManifestGateExemptions {
    #[serde(default)]
    pub timing: bool,
    #[serde(default)]
    pub momentum: bool,
    #[serde(default)]
    pub timing_min_time_left_sec: Option<i64>,
}

/// `{"threshold": {"type": "decimal", "default": "0.04"}}` — the §6.4
/// tunables form. Values cross as strings (the decimal-STRING wire rule).
/// #393: `min`/`max` are OPTIONAL — a tunable that declares a domain opts its
/// name into shadow evolution; one that does not stays a plain `bk.params()`
/// entry. The domain is a hard outer bound the evolution guard enforces, so
/// it may never be invented kernel-side: absent bounds = not evolvable.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BTreeMapDefs(pub std::collections::BTreeMap<String, TunableDef>);

#[derive(Debug, Clone, Deserialize)]
pub struct TunableDef {
    #[serde(default)]
    pub r#type: String,
    pub default: String,
    /// Optional domain (issue 393): decimal strings. Absent = not evolvable.
    #[serde(default)]
    pub min: String,
    #[serde(default)]
    pub max: String,
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

/// #393: the evolvable-knob declaration a Lua manifest carries — the Lua
/// mirror of the dylib surface's OPTIONAL `bk_strategy_evolvable_knobs`
/// symbol (`ForeignStrategy::read_knobs`). A tunable opts in by declaring a
/// DOMAIN: `type` `decimal`/`int` plus `min`/`max` bounds that are coherent
/// (`min <= max`) and contain the declared default. Everything else — a
/// missing bound, an unparseable number, an incoherent domain — is skipped,
/// and a package with no qualifying tunable stays **not evolvable**:
/// fail-closed, and the explicit declaration (D6) the evolution machinery
/// demands. The kernel never invents a domain.
fn specs_from_tunables(tunables: &BTreeMapDefs) -> Vec<KnobSpec> {
    let mut out = Vec::new();
    for (name, def) in &tunables.0 {
        if !matches!(def.r#type.as_str(), "decimal" | "int") {
            continue;
        }
        if def.min.is_empty() || def.max.is_empty() {
            continue;
        }
        let (Ok(value), Ok(min), Ok(max)) = (
            Decimal::from_str_exact(&def.default),
            Decimal::from_str_exact(&def.min),
            Decimal::from_str_exact(&def.max),
        ) else {
            continue;
        };
        let spec = KnobSpec::new(name.clone(), value, min, max);
        if !spec.is_coherent() {
            continue;
        }
        out.push(spec);
    }
    out
}

/// #393: what an adapter needs to rebuild INDEPENDENT twins of one loaded
/// package — the entry text, the declared modes, the knob specs and the
/// manifest defaults. `load_lua_package` records it here keyed by package
/// name (the manifest name IS the identity, §6.4) because the construction
/// site hands the adapter only the built [`LuaStrategy`] and the tunables
/// bag — there is no other channel for the source text a twin's
/// `LuaStrategy::build` needs. Reload overwrites; a package never loaded
/// through this loader has no entry, and its adapter stays not-evolvable
/// (fail-closed).
#[derive(Debug, Clone)]
struct PackageSource {
    code: Arc<str>,
    modes: Vec<StrategyMode>,
    specs: Vec<KnobSpec>,
    defaults: HashMap<String, String>,
}

static PACKAGE_SOURCES: OnceLock<Mutex<HashMap<String, PackageSource>>> = OnceLock::new();

fn source_vault_remember(name: &str, source: PackageSource) {
    let map = PACKAGE_SOURCES.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut m) = map.lock() {
        m.insert(name.to_string(), source);
    }
}

fn source_vault_for(name: &str) -> Option<PackageSource> {
    let map = PACKAGE_SOURCES.get_or_init(|| Mutex::new(HashMap::new()));
    map.lock().ok()?.get(name).cloned()
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
    /// The manifest's hold-to-settlement declaration (collection semantics:
    /// redeem or MERGE, never ladder-sell). `false` when absent.
    pub holds_to_settlement: bool,
    /// The manifest's entry-gate exemptions. `none()` when absent — every
    /// existing package keeps today's fully-gated behaviour.
    pub gate_exemptions: GateExemptions,
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

    // #393: the package identity twins are rebuilt from — recorded only after
    // every validation above passed, so the vault never holds a refused
    // package's text.
    source_vault_remember(
        &manifest.name,
        PackageSource {
            code: Arc::from(code.as_str()),
            modes: declared_modes.clone(),
            specs: specs_from_tunables(&manifest.tunables),
            defaults: manifest.tunables.defaults(),
        },
    );

    Ok(LoadedLua {
        strategy,
        name: manifest.name,
        version: manifest.version,
        declared_modes,
        tunables: manifest.tunables.defaults(),
        holds_to_settlement: manifest.holds_to_settlement,
        gate_exemptions: GateExemptions {
            timing: manifest.gate_exemptions.timing,
            momentum: manifest.gate_exemptions.momentum,
            timing_min_time_left_sec: manifest.gate_exemptions.timing_min_time_left_sec,
        },
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
    /// Manifest declarations (§6.4): collection semantics + entry-gate
    /// exemptions. Defaults keep today's behaviour for packages that declare
    /// neither.
    holds_to_settlement: bool,
    gate_exemptions: GateExemptions,
    /// The strategy's cell of the shared ParamRegistry, when one is attached
    /// (E2-c plumbing; a cell exists only for strategies that declared
    /// evolvable knobs).
    params_cell: Option<Arc<ArcSwap<StrategyParams>>>,
    /// The manifest tunables' defaults — the base bag the hot-param cell
    /// merges over (#393): the cell carries the EVOLVED knobs, tunables that
    /// declared no domain keep their manifest default.
    defaults: HashMap<String, String>,
    /// #393: the evolvable knobs this package declared (manifest tunables
    /// with a coherent domain). Empty = not evolvable — the explicit
    /// declaration, never a kernel-side guess.
    specs: Vec<KnobSpec>,
    /// The fee schedule cell shared with any twins this adapter's factory
    /// builds: `set_fee_schedule` writes it, a twin's `make` reads it at
    /// birth — a twin prices `bk.fees()` from the same curve the live
    /// strategy sees.
    fee_cell: Arc<Mutex<Option<FeeScheduleView>>>,
}

impl LuaEngineAdapter {
    pub fn new(inner: LuaStrategy, tunables: HashMap<String, String>) -> Self {
        // Seed `bk.params()` with the manifest defaults; a registry cell (if
        // ever attached) merges over the bag per evaluate.
        let state = inner.state();
        if let Ok(mut st) = state.lock() {
            st.params = tunables.clone();
        }
        let specs = source_vault_for(inner.name())
            .map(|s| s.specs)
            .unwrap_or_default();
        Self {
            inner,
            assets: HashMap::new(),
            last_books: HashMap::new(),
            exit_intents: Vec::new(),
            breaks: Vec::new(),
            holds_to_settlement: false,
            gate_exemptions: GateExemptions::none(),
            params_cell: None,
            defaults: tunables,
            specs,
            fee_cell: Arc::new(Mutex::new(None)),
        }
    }

    /// #393: the twin constructor — the same shape as [`Self::new`], but the
    /// fee cell is SHARED with the live adapter (a twin prices `bk.fees()`
    /// from the same schedule the live strategy sees) and the vault is not
    /// consulted (twins are never registered for evolution themselves).
    fn new_with_fee(
        inner: LuaStrategy,
        tunables: HashMap<String, String>,
        fee_cell: Arc<Mutex<Option<FeeScheduleView>>>,
    ) -> Self {
        let state = inner.state();
        if let Ok(mut st) = state.lock() {
            st.params = tunables.clone();
        }
        Self {
            inner,
            assets: HashMap::new(),
            last_books: HashMap::new(),
            exit_intents: Vec::new(),
            breaks: Vec::new(),
            holds_to_settlement: false,
            gate_exemptions: GateExemptions::none(),
            params_cell: None,
            defaults: tunables,
            specs: Vec::new(),
            fee_cell,
        }
    }

    /// Stamp the manifest's declarations (the loaded-package path). A strategy
    /// that declared hold-to-settlement holds its legs for COLLECTION —
    /// redeemed at settlement or MERGED as complete pairs — so the exit ladder
    /// skips it (dry/read-only) and the pair-completion entry exemption may
    /// assemble both legs of one condition.
    pub fn declare(mut self, holds_to_settlement: bool, gate_exemptions: GateExemptions) -> Self {
        self.holds_to_settlement = holds_to_settlement;
        self.gate_exemptions = gate_exemptions;
        self
    }

    /// Inject the kernel's ONE fee schedule (the adapter cannot read the core;
    /// the service can). Powers `bk.fees()` — a strategy prices the fee from
    /// the same curve the charge path settles in, never from a copied
    /// constant. A package that never receives this sees `bk.fees() == nil`
    /// and must fail closed (no entries, never "assume the fee is zero").
    /// #393: the view also lands in the shared cell, so a shadow twin built
    /// later prices from the SAME schedule, not a guessed one.
    pub fn set_fee_schedule(&self, view: FeeScheduleView) {
        if let Ok(mut st) = self.inner.state().lock() {
            st.fee_schedule = Some(view.clone());
        }
        if let Ok(mut cell) = self.fee_cell.lock() {
            *cell = Some(view);
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
        // Hot parameters: the registry cell (when attached) merges the EVOLVED
        // knobs over the manifest-default bag for this cycle (§6.6: same
        // ParamRegistry). #393: MERGE, not replace — the cell only carries the
        // knobs that declared domains, and a tunable without a domain must
        // keep its manifest default, never vanish from `bk.params()`. A
        // strategy with no cell (not evolvable) never runs this branch.
        if let Some(cell) = &self.params_cell {
            let params = (**cell.load()).clone();
            let mut bag = self.defaults.clone();
            for (k, v) in params.iter() {
                bag.insert(k.to_string(), v.to_string());
            }
            if let Ok(mut st) = self.inner.state().lock() {
                st.params = bag;
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
        self.gate_exemptions
    }

    fn holds_to_settlement(&self) -> bool {
        self.holds_to_settlement
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

    /// #393: the knobs this package declared evolvable (manifest tunables
    /// with a coherent `min`/`max` domain). Empty = not evolvable — the same
    /// explicit declaration a dylib without `bk_strategy_evolvable_knobs`
    /// makes; every shipped package keeps today's behaviour.
    fn evolvable_knobs(&self) -> Vec<KnobSpec> {
        self.specs.clone()
    }

    /// #393: build twins of this strategy — the mirror of
    /// `ForeignStrategy::shadow_factory`. The twin is an INDEPENDENT sandbox
    /// built from the SAME entry text (the package source recorded at load),
    /// seeded with the counterfactual parameters before it is driven. No new
    /// `bk.*` API: the twin's `bk.params()` is the ordinary host-pushed bag,
    /// and its fee schedule arrives through the shared cell.
    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
        if self.specs.is_empty() {
            return None; // explicit "not evolvable"
        }
        let source = source_vault_for(self.inner.name())?;
        Some(Box::new(LuaShadowFactory {
            name: self.inner.name().to_string(),
            version: self.inner.version().to_string(),
            specs: self.specs.clone(),
            source,
            holds_to_settlement: self.holds_to_settlement,
            gate_exemptions: self.gate_exemptions,
            fee_cell: Arc::clone(&self.fee_cell),
        }))
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

/// Builds independent twins of a loaded Lua strategy (#393) — the mirror of
/// `ForeignShadowFactory`. A twin is a SECOND sandbox from the SAME entry
/// text (the package source the loader vaulted), seeded with the
/// counterfactual parameters BEFORE it is driven, stamped with the live
/// adapter's declarations, and priced by the SAME fee schedule (the shared
/// cell the live adapter's `set_fee_schedule` writes). A twin whose sandbox
/// cannot be built is `None` — the variant is absent, never a half-built one.
struct LuaShadowFactory {
    name: String,
    version: String,
    specs: Vec<KnobSpec>,
    source: PackageSource,
    holds_to_settlement: bool,
    gate_exemptions: GateExemptions,
    fee_cell: Arc<Mutex<Option<FeeScheduleView>>>,
}

impl ShadowFactory for LuaShadowFactory {
    fn strategy(&self) -> String {
        self.name.clone()
    }

    fn knobs(&self) -> Vec<KnobSpec> {
        self.specs.clone()
    }

    fn make(&self, params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
        let twin = LuaStrategy::build(
            self.name.clone(),
            self.version.clone(),
            self.source.modes.clone(),
            &self.source.code,
        )
        .ok()?;
        // The counterfactual bag: the manifest defaults for tunables that
        // declared no domain, the variant's values for the knobs that did —
        // seeded BEFORE the twin is driven, so its very first evaluation
        // already runs the mutated parameters.
        let mut bag = self.source.defaults.clone();
        for (k, v) in params.iter() {
            bag.insert(k.to_string(), v.to_string());
        }
        let twin = LuaEngineAdapter::new_with_fee(twin, bag, Arc::clone(&self.fee_cell))
            .declare(self.holds_to_settlement, self.gate_exemptions);
        Some(Box::new(twin))
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

    /// #393: rewrite one test package's manifest through `f` after the base
    /// write (the entry file is untouched, so the sha256 stays valid).
    fn with_manifest(dir: &Path, f: impl FnOnce(&mut serde_json::Value)) {
        let manifest_path = dir.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("read"))
                .expect("json");
        f(&mut manifest);
        std::fs::write(&manifest_path, manifest.to_string()).expect("write");
    }

    /// #393: a tunable that declares a coherent domain opts the strategy into
    /// evolution — it becomes a knob with that domain, the factory is present,
    /// and the factory builds an independent twin of the SAME strategy seeded
    /// with the counterfactual values. A tunable without a domain (here the
    /// string one) must NOT become a knob.
    #[test]
    fn tunables_with_domains_become_evolvable() {
        let tmp = std::env::temp_dir().join(format!("bk-lua-evo-{}", std::process::id()));
        let pkg = write_package(&tmp.join("lua_evo"), "lua_evo", &[]);
        with_manifest(&pkg, |m| {
            m["tunables"] = serde_json::json!({
                "entry_max_price": { "type": "decimal", "default": "0.40", "min": "0.05", "max": "0.95" },
                "label": { "type": "string", "default": "x" }
            });
        });
        let loaded = load_lua_package(&pkg).expect("loads");
        let adapter = LuaEngineAdapter::new(loaded.strategy, loaded.tunables);
        let knobs = adapter.evolvable_knobs();
        assert_eq!(
            knobs.len(),
            1,
            "only the domain-declaring tunable is a knob"
        );
        assert_eq!(knobs[0].name, "entry_max_price");
        assert_eq!(knobs[0].min, rust_decimal_macros::dec!(0.05));
        assert_eq!(knobs[0].max, rust_decimal_macros::dec!(0.95));

        let factory = adapter.shadow_factory().expect("factory present");
        let mut params = StrategyParams::new();
        params.set("entry_max_price", rust_decimal_macros::dec!(0.55));
        let twin = factory.make(&params).expect("twin builds");
        assert_eq!(twin.name(), "lua_evo", "the twin is the same strategy");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// #393: a tunable with NO domain stays a plain `bk.params()` entry — the
    /// package remains not-evolvable, exactly as every shipped package is
    /// today (fail-closed; the kernel never invents a domain).
    #[test]
    fn tunables_without_domains_stay_not_evolvable() {
        let tmp = std::env::temp_dir().join(format!("bk-lua-noevo-{}", std::process::id()));
        let pkg = write_package(&tmp.join("lua_noevo"), "lua_noevo", &[]);
        with_manifest(&pkg, |m| {
            m["tunables"] = serde_json::json!({
                "entry_max_price": { "type": "decimal", "default": "0.40" }
            });
        });
        let loaded = load_lua_package(&pkg).expect("loads");
        let adapter = LuaEngineAdapter::new(loaded.strategy, loaded.tunables);
        assert!(adapter.evolvable_knobs().is_empty(), "no domain, no knob");
        assert!(adapter.shadow_factory().is_none(), "no knobs, no factory");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// #393: an incoherent domain (`min > max`) is skipped, not patched — the
    /// declaration is the author's, and a bounds the guard would refuse is
    /// not silently turned into one it would accept.
    #[test]
    fn incoherent_domain_is_skipped() {
        let tmp = std::env::temp_dir().join(format!("bk-lua-baddom-{}", std::process::id()));
        let pkg = write_package(&tmp.join("lua_baddom"), "lua_baddom", &[]);
        with_manifest(&pkg, |m| {
            m["tunables"] = serde_json::json!({
                "entry_max_price": { "type": "decimal", "default": "0.40", "min": "0.95", "max": "0.05" }
            });
        });
        let loaded = load_lua_package(&pkg).expect("loads");
        let adapter = LuaEngineAdapter::new(loaded.strategy, loaded.tunables);
        assert!(
            adapter.evolvable_knobs().is_empty(),
            "incoherent domain is not a knob"
        );
        assert!(adapter.shadow_factory().is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
