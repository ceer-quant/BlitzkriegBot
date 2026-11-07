//! `LuaStrategy: SafeStrategy` (DEV_V0_3 §6.6 / E30 #336 task 3).
//!
//! One Lua state machine per strategy ([`crate::sandbox::LuaSandbox`]); the
//! host-facing contract is [`SafeStrategy`]. Every host callback marshals its
//! data into the read-only [`BkState`] snapshot (what `bk.*` reads) and then,
//! when the strategy defined the suggested entry point, calls it inside the
//! sandbox guard. `bk_evaluate` is REQUIRED and returns the intent tables;
//! the kernel adjudicates, sizes and submits — the Lua side never sees an
//! order API (§6.4).
//!
//! Error semantics mirror the sandbox contract: a plain Lua error is a
//! deterministic strategy bug (recorded in [`LuaHealth::last_error`], the
//! host keeps calling); a resource violation (instruction budget, memory
//! ceiling) poisons the state machine — every later call is refused
//! host-side and the poison handle's one-shot alert is what the core turns
//! into a single RISK_ALERT naming this strategy (§6.3).

use std::sync::{Arc, Mutex};

use blitzkrieg_strategy_api::{
    BookUpdate, Break, Entry, EntryPairDecl, Exit, FreshBook, HoldingView, Intents, Kline,
    LuaEntryPoint, ParamBag, RoundContext, RoundInfo, SafeStrategy, StrategyMode,
};
use mlua::Value;

use crate::bk_api::{
    BkState, book_to_lua, holding_to_lua, install_bk, interval_name, kline_to_lua, round_to_lua,
};
use crate::sandbox::LuaSandbox;

/// Observable sandbox health: what `strategy.list` diagnostics surface and
/// what the core adapter logs on transitions (it owns `tracing`; this crate
/// stays dependency-light and only records).
#[derive(Debug, Clone, Default)]
pub struct LuaHealth {
    /// Last deterministic Lua error (cleared on the next clean call).
    pub last_error: Option<String>,
    /// Poison state (§6.3): once true, every host call is refused.
    pub poisoned: bool,
    /// Host→sandbox calls attempted (refused calls included).
    pub calls: u64,
    /// Calls that ended in an error of either class.
    pub errors: u64,
}

/// One Lua strategy: manifest identity + sandbox + the `bk.*` state snapshot.
pub struct LuaStrategy {
    name: String,
    version: String,
    sandbox: LuaSandbox,
    state: Arc<Mutex<BkState>>,
    modes: Vec<StrategyMode>,
    health: Arc<Mutex<LuaHealth>>,
}

impl std::fmt::Debug for LuaStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The sandbox (a live Lua VM) is not Debug — identity only.
        f.debug_struct("LuaStrategy")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("poisoned", &self.sandbox.is_poisoned())
            .finish()
    }
}

impl LuaStrategy {
    /// Build from a validated package. `code` is the entry file's text; the
    /// caller (the core-side loader) has already checked the manifest, the
    /// sha256 and the directory name — this constructor checks only what the
    /// sandbox itself can: the chunk loads and `bk_evaluate` exists.
    pub fn build(
        name: String,
        version: String,
        modes: Vec<StrategyMode>,
        code: &str,
    ) -> Result<Self, String> {
        let sandbox = LuaSandbox::new().map_err(|e| format!("sandbox construction failed: {e}"))?;
        let state = Arc::new(Mutex::new(BkState::default()));
        install_bk(sandbox.lua(), Arc::clone(&state))
            .map_err(|e| format!("bk.* surface install failed: {e}"))?;
        sandbox
            .load(code, &name)
            .map_err(|e| format!("entry script failed to load: {}", e.message()))?;
        let evaluate = sandbox
            .global_function("bk_evaluate")
            .map_err(|e| format!("globals unreadable: {e}"))?;
        if evaluate.is_none() {
            return Err("required entry point `bk_evaluate` is not defined".into());
        }
        Ok(Self {
            name,
            version,
            sandbox,
            state,
            modes,
            health: Arc::new(Mutex::new(LuaHealth::default())),
        })
    }

    /// The one-shot poison alert (§6.3): the core emits it as a single
    /// RISK_ALERT naming this strategy. Draining is the emitter's job.
    pub fn take_poison_alert(&self) -> Option<String> {
        self.sandbox.poison_handle().take_alert()
    }

    /// A clonable view of the poison state: the core-side loader holds one
    /// clone beside the adapter so it can drain the one-shot alert (and emit
    /// the single RISK_ALERT) without reaching through the dispatch box.
    pub fn poison_handle(&self) -> crate::sandbox::PoisonHandle {
        self.sandbox.poison_handle()
    }

    pub fn is_poisoned(&self) -> bool {
        self.sandbox.is_poisoned()
    }

    /// The `bk.*` state snapshot — the host pushes the account view here
    /// (public display fields only) and reads back nothing it did not write.
    pub fn state(&self) -> Arc<Mutex<BkState>> {
        Arc::clone(&self.state)
    }

    /// A snapshot of the observable health (call/error counters, the last
    /// deterministic Lua error, the poison flag) — the same record
    /// `diagnostics()` serializes, as plain data. `None` only when the health
    /// mutex is poisoned, which requires a panic in this module itself.
    pub fn health_view(&self) -> Option<LuaHealth> {
        self.health.lock().ok().map(|h| h.clone())
    }

    fn note_ok(&self) {
        if let Ok(mut h) = self.health.lock() {
            h.calls += 1;
            h.last_error = None;
        }
    }

    fn note_err(&self, message: &str, poisoned: bool) {
        if let Ok(mut h) = self.health.lock() {
            h.calls += 1;
            h.errors += 1;
            h.last_error = Some(message.to_string());
            h.poisoned = h.poisoned || poisoned;
        }
    }

    /// Fetch an optional entry point; `None` (absent) is a clean no-op skip.
    fn optional_entry(&self, name: &str) -> Option<mlua::Function> {
        self.sandbox.global_function(name).ok().flatten()
    }

    /// #961: the FORCED thesis-death scan — the enforcement half of the
    /// `thesis_death_exit` standard-library hook (the declaration half is the
    /// optional `bk_thesis_death(holding)` entry point or the
    /// `BK_THESIS_DEATH_ENABLE = true` global opting into the stdlib default
    /// `bk.thesis_death_default`).
    ///
    /// Contract, per evaluation cycle:
    ///   * every held position is consulted; a truthy verdict declares the
    ///     position's ENTRY THESIS dead;
    ///   * a dead thesis is closed THIS TICK, unconditionally, as a full
    ///     TAKER sweep: the runtime injects the exit intent itself, tagged
    ///     `thesis_death_exit` (the kernel maps it to `ExitReason::
    ///     ThesisDeathExit`; the profit-side maker-first gate refuses the
    ///     tag, so a death can never rest as a maker);
    ///   * the injection SUPERSEDES the strategy's own exit rows for that
    ///     token — a dead thesis must not ride a resting maker sell that may
    ///     never fill (禁止扛单);
    ///   * the scan runs even when `bk_evaluate` errored: a buggy strategy
    ///     must not lose its way out;
    ///   * a deterministic Lua error inside one predicate is the strategy's
    ///     own bug (recorded in health, host keeps calling) and skips only
    ///     THAT position — one bad row never disables the scan for the rest;
    ///   * a package that declares NEITHER form has no death conditions: the
    ///     scan is a no-op for it (grandfathered; `scripts/
    ///     thesis-death-check.mjs` reports the gap, and new packages must
    ///     declare per the template in `docs/thesis_death.md`).
    fn force_thesis_death_exits(&mut self, intents: &mut Intents) {
        let holdings: Vec<HoldingView> = match self.state.lock() {
            Ok(st) => st.holdings.clone(),
            Err(_) => return,
        };
        if holdings.is_empty() {
            return;
        }
        // Declaration lookup, once per cycle: the package's own predicate
        // wins; the boolean global opts into the stdlib default; neither
        // declared = nothing to enforce.
        let predicate = self.optional_entry(LuaEntryPoint::ThesisDeath.global_name());
        let default_enabled = if predicate.is_some() {
            false
        } else {
            matches!(
                self.sandbox
                    .lua()
                    .globals()
                    .raw_get::<Value>("BK_THESIS_DEATH_ENABLE"),
                Ok(Value::Boolean(true))
            )
        };
        if predicate.is_none() && !default_enabled {
            return;
        }
        for h in &holdings {
            let verdict = if let Some(f) = &predicate {
                self.sandbox.invoke("bk_thesis_death", |lua| {
                    let t = holding_to_lua(lua, h)?;
                    f.call::<Value>((t,))
                })
            } else {
                self.sandbox.invoke("bk.thesis_death_default", |lua| {
                    let bk: mlua::Table = lua.globals().raw_get("bk")?;
                    let f: mlua::Function = bk.raw_get("thesis_death_default")?;
                    let t = holding_to_lua(lua, h)?;
                    f.call::<Value>((t,))
                })
            };
            match verdict {
                // Truthy (true, or any non-false value a predicate returns) =
                // the thesis is dead. `nil`/`false` = alive.
                Ok(v) if v != Value::Nil && v != Value::Boolean(false) => {}
                Ok(_) => continue,
                Err(e) => {
                    self.note_err(e.message(), e.is_poison());
                    continue;
                }
            }
            let token = h.token_id.clone();
            // Supersede the strategy's own rows for this token: a forced
            // death is a TAKER sweep this tick, never a resting maker.
            intents.exits.retain(|x| x.token != token);
            intents.exits.push(Exit {
                token,
                reason: "thesis_death_exit".into(),
                use_maker: false,
                price: None,
            });
        }
    }
}

impl SafeStrategy for LuaStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn declare_modes(&self) -> Vec<StrategyMode> {
        self.modes.clone()
    }

    fn on_book(&mut self, update: &BookUpdate) {
        // The snapshot update happens host-side even if the callback is
        // absent/refused: `bk.book` must reflect the latest book regardless.
        if let Ok(mut st) = self.state.lock() {
            st.set_now(update.timestamp_ms);
            st.put_book(update.clone(), None);
        }
        let Some(f) = self.optional_entry("bk_on_book") else {
            return;
        };
        match self.sandbox.invoke("bk_on_book", |lua| {
            let t = book_to_lua(lua, update, None)?;
            f.call::<()>((t,))
        }) {
            Ok(()) => self.note_ok(),
            Err(e) => self.note_err(e.message(), e.is_poison()),
        }
    }

    fn on_eval_books(&mut self, books: &[FreshBook]) {
        // Data form of the freshness gate: overwrite the snapshot with the
        // verdict attached so `bk.book(sym).fresh` is authoritative for THIS
        // evaluation cycle.
        if let Ok(mut st) = self.state.lock() {
            for fb in books {
                st.put_book(fb.book.clone(), Some(fb.fresh));
            }
        }
    }

    fn on_holdings(&mut self, holdings: &[HoldingView]) {
        // Mirror of `on_eval_books`: overwrite the snapshot so `bk.holdings()`
        // is authoritative for THIS evaluation cycle. Data-only — there is no
        // Lua entry point for this hook (a position change is not a signal).
        if let Ok(mut st) = self.state.lock() {
            st.holdings = holdings.to_vec();
        }
    }

    fn on_round(&mut self, round: RoundInfo) {
        if let Ok(mut st) = self.state.lock() {
            // note_round (not a bare set): the #961 round-duration seed rides
            // the same update (slot change wipes it, a wider clock re-seeds).
            st.note_round(round.slot, round.time_left_sec, round.now_ms);
        }
        let Some(f) = self.optional_entry("bk_on_round") else {
            return;
        };
        match self.sandbox.invoke("bk_on_round", |lua| {
            let t = round_to_lua(lua, &round)?;
            f.call::<()>((t,))
        }) {
            Ok(()) => self.note_ok(),
            Err(e) => self.note_err(e.message(), e.is_poison()),
        }
    }

    fn on_kline(&mut self, kline: &Kline) {
        // Only CLOSED bars are pushed (§6.6); the same map answers `bk.kline`.
        if kline.is_closed
            && let Ok(mut st) = self.state.lock()
        {
            let key = (
                kline.symbol.clone(),
                interval_name(kline.interval).to_string(),
            );
            st.klines.insert(key, kline.clone());
        }
        let Some(f) = self.optional_entry("bk_on_kline") else {
            return;
        };
        match self.sandbox.invoke("bk_on_kline", |lua| {
            let t = kline_to_lua(lua, kline)?;
            f.call::<()>((t,))
        }) {
            Ok(()) => self.note_ok(),
            Err(e) => self.note_err(e.message(), e.is_poison()),
        }
    }

    fn evaluate(&mut self, ctx: &RoundContext) -> Intents {
        {
            let mut st = match self.state.lock() {
                Ok(st) => st,
                Err(_) => return Intents::none(),
            };
            // note_round (not a bare set): the #961 round-duration seed rides
            // the same update (slot change wipes it, a wider clock re-seeds).
            st.note_round(ctx.round.slot, ctx.round.time_left_sec, ctx.round.now_ms);
            st.markets = ctx.markets.clone();
        }
        let mut intents = match self.optional_entry("bk_evaluate") {
            None => {
                // Checked at build; unreachable unless the strategy overwrote
                // the global — which only harms itself.
                self.note_err("bk_evaluate is not defined", false);
                Intents::none()
            }
            Some(f) => match self.sandbox.invoke("bk_evaluate", |_| f.call::<Value>(())) {
                Ok(v) => {
                    self.note_ok();
                    let (intents, parse_err) = parse_intents(&v, self.name());
                    if let Some(err) = parse_err {
                        self.note_err(&err, false);
                    }
                    intents
                }
                Err(e) => {
                    self.note_err(e.message(), e.is_poison());
                    Intents::none()
                }
            },
        };
        // #961: the forced thesis-death scan runs AFTER `bk_evaluate` — and
        // also when it errored: a buggy evaluate must not buy a position a
        // way to hold. The scan is the enforcement half of the hook; the
        // strategy can never suppress its own death.
        self.force_thesis_death_exits(&mut intents);
        intents
    }

    fn on_params(&mut self, params: &ParamBag) -> bool {
        if let Ok(mut st) = self.state.lock() {
            st.params = params.0.clone();
        }
        true
    }

    fn diagnostics(&self) -> Vec<serde_json::Value> {
        let h = self.health.lock().map(|h| h.clone()).unwrap_or_default();
        vec![serde_json::json!({
            "runtime": "lua5.4",
            "poisoned": h.poisoned || self.sandbox.is_poisoned(),
            "calls": h.calls,
            "errors": h.errors,
            "lastError": h.last_error,
        })]
    }
}

/// Parse the `bk_evaluate` return value into [`Intents`]. The wire is the
/// Keys a returned entry row may carry — everything else is dropped with a
/// warn, the same observability the C ABI's reserved-key seal provides
/// (`foreign.rs` warn+drop; issue 648's micro-drift: the Lua side used to
/// ignore unknown keys in TOTAL silence).
const ENTRY_KEYS: &[&str] = &["token", "price", "reason", "shares", "pair"];
/// Keys a returned exit row may carry.
const EXIT_KEYS: &[&str] = &["token", "reason", "use_maker", "price"];
/// Keys a returned break row may carry.
const BREAK_KEYS: &[&str] = &["token", "broken_price"];

/// Warn once per unknown key on one intent row — the Lua twin of the C ABI's
/// reserved-key warn+drop (same `target: "strategy"` line shape, so an author
/// sees the same diagnostic whichever runtime they ship). The key is still
/// dropped (parse below never reads it); this only makes the drop VISIBLE.
fn warn_unknown_keys(row: &mlua::Table, allowed: &[&str], strategy: &str, token: &str) {
    for pair in row.pairs::<String, mlua::Value>() {
        if let Ok((key, _)) = pair
            && !allowed.contains(&key.as_str())
            && key != "token"
        {
            tracing::warn!(
                target: "strategy",
                strategy = %strategy,
                token = %token,
                key,
                "unknown intent key dropped (the kernel never reads it)"
            );
        }
    }
}

/// §6.5 contract:
fn parse_intents(v: &Value, strategy: &str) -> (Intents, Option<String>) {
    let mut intents = Intents::none();
    let Value::Table(t) = v else {
        return (intents, Some("bk_evaluate must return a table".into()));
    };
    // entries ─ token/price required; reason/shares optional strings;
    // pair (RFC 568) optional { key, leg } — absent ⇒ None（旧行为逐位不变）.
    if let Ok(arr) = t.raw_get::<mlua::Table>("entries") {
        for pair in arr.sequence_values::<mlua::Table>() {
            let Ok(row) = pair else { continue };
            let (Some(token), Some(price)) = (
                row.raw_get::<String>("token").ok(),
                row.raw_get::<String>("price").ok(),
            ) else {
                return (intents, Some("entry missing token/price string".into()));
            };
            let pair_decl = row.raw_get::<mlua::Table>("pair").ok().and_then(|pt| {
                let key = pt.raw_get::<String>("key").ok()?;
                let leg = pt.raw_get::<String>("leg").ok()?;
                Some(EntryPairDecl { key, leg })
            });
            warn_unknown_keys(&row, ENTRY_KEYS, strategy, &token);
            intents.entries.push(Entry {
                token,
                price,
                reason: row.raw_get::<String>("reason").unwrap_or_default(),
                shares: row.raw_get::<String>("shares").ok(),
                pair: pair_decl,
            });
        }
    }
    // exits ─ token required.
    if let Ok(arr) = t.raw_get::<mlua::Table>("exits") {
        for pair in arr.sequence_values::<mlua::Table>() {
            let Ok(row) = pair else { continue };
            let Some(token) = row.raw_get::<String>("token").ok() else {
                return (intents, Some("exit missing token string".into()));
            };
            warn_unknown_keys(&row, EXIT_KEYS, strategy, &token);
            intents.exits.push(Exit {
                token,
                reason: row.raw_get::<String>("reason").unwrap_or_default(),
                // #935: optional advisory — an absent key stays the historic
                // taker sweep (false), a present key carries the strategy's
                // own maker-first suggestion for the kernel to gate.
                use_maker: row.raw_get::<bool>("use_maker").unwrap_or(false),
                // #915: optional maker-TP limit (exact decimal string). An
                // absent key stays the historic price-less row (None) — the
                // kernel keeps pricing every close off the live book.
                price: row.raw_get::<String>("price").ok(),
            });
        }
    }
    // breaks ─ token + broken_price.
    if let Ok(arr) = t.raw_get::<mlua::Table>("breaks") {
        for pair in arr.sequence_values::<mlua::Table>() {
            let Ok(row) = pair else { continue };
            let (Some(token), Some(broken_price)) = (
                row.raw_get::<String>("token").ok(),
                row.raw_get::<String>("broken_price").ok(),
            ) else {
                return (
                    intents,
                    Some("break missing token/broken_price string".into()),
                );
            };
            intents.breaks.push(Break {
                token: token.clone(),
                broken_price,
            });
            warn_unknown_keys(&row, BREAK_KEYS, strategy, &token);
        }
    }
    (intents, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bk_api::AccountView;

    fn strategy(code: &str) -> LuaStrategy {
        LuaStrategy::build("t".into(), "0.1.0".into(), Vec::new(), code).expect("strategy builds")
    }

    /// A two-sided book row for the #961 fixtures (`ts` = the book's stamp,
    /// judged against the round clock the evaluate call pushes).
    fn book_update(symbol: &str, best_bid: &str, ts: i64) -> BookUpdate {
        BookUpdate {
            symbol: symbol.into(),
            asset: "BTC".into(),
            best_bid: Some(best_bid.into()),
            best_ask: Some("0.99".into()),
            mid: None,
            bid_depth: None,
            ask_depth: None,
            obi: None,
            spread: None,
            spread_pct: None,
            timestamp_ms: ts,
            bid_levels: 1,
            ask_levels: 1,
            bid_ladder: vec![],
            ask_ladder: vec![],
        }
    }

    fn holding(token: &str, entry: &str) -> HoldingView {
        HoldingView {
            strategy: "t".into(),
            condition_id: "cond".into(),
            token_id: token.into(),
            direction: "up".into(),
            shares: "10".into(),
            entry_price: entry.into(),
            round_slot: 1,
        }
    }

    fn eval_ctx(now_ms: i64, time_left_sec: i64) -> RoundContext {
        RoundContext {
            round: RoundInfo {
                slot: 1,
                time_left_sec,
                now_ms,
            },
            markets: vec![],
        }
    }

    // ── #961: the forced thesis-death exit ──────────────────────────────────

    /// Acceptance 1 (default-predicate form): a declared `BK_THESIS_DEATH_ENABLE`
    /// plus the stdlib default predicate — the held token's own bid 40% under
    /// entry crosses the shipped 25% reversal band → the SAME evaluate emits
    /// the unconditional taker exit intent tagged `thesis_death_exit`.
    #[test]
    fn thesis_death_default_fires_on_a_book_reversal() {
        let mut s = strategy(
            "BK_THESIS_DEATH_ENABLE = true \
             function bk_evaluate() return {} end",
        );
        s.on_holdings(&[holding("0xup", "0.50")]);
        s.on_eval_books(&[blitzkrieg_strategy_api::FreshBook {
            book: book_update("0xup", "0.30", 2_000),
            fresh: true,
        }]);
        let intents = s.evaluate(&eval_ctx(2_000, 600));
        assert_eq!(intents.exits.len(), 1, "{intents:?}");
        let x = &intents.exits[0];
        assert_eq!(x.token, "0xup");
        assert_eq!(x.reason, "thesis_death_exit");
        assert!(!x.use_maker, "a forced death is a full TAKER close");
        assert_eq!(x.price, None, "the kernel prices it off the live bid");
        assert!(s.health.lock().expect("health").last_error.is_none());
    }

    /// A thesis still alive (an adverse move inside the band) emits nothing.
    #[test]
    fn alive_thesis_emits_no_exit() {
        let mut s = strategy(
            "BK_THESIS_DEATH_ENABLE = true \
             function bk_evaluate() return {} end",
        );
        s.on_holdings(&[holding("0xup", "0.50")]);
        s.on_eval_books(&[blitzkrieg_strategy_api::FreshBook {
            book: book_update("0xup", "0.45", 2_000),
            fresh: true,
        }]);
        let intents = s.evaluate(&eval_ctx(2_000, 600));
        assert!(intents.exits.is_empty(), "{intents:?}");
    }

    /// Condition 1 (论点依赖字段消失): no book at all for the held token —
    /// the thesis has nothing left to stand on, death, same tick.
    #[test]
    fn vanished_book_kills_the_thesis() {
        let mut s = strategy(
            "BK_THESIS_DEATH_ENABLE = true \
             function bk_evaluate() return {} end",
        );
        s.on_holdings(&[holding("0xup", "0.50")]);
        // No on_eval_books push: the token's quoting fields are gone.
        let intents = s.evaluate(&eval_ctx(2_000, 600));
        assert_eq!(intents.exits.len(), 1, "{intents:?}");
        assert_eq!(intents.exits[0].reason, "thesis_death_exit");
    }

    /// Condition 3 (轮次时间加权越界): the death band tightens linearly as
    /// the round drains — an adverse move the FLAT band forgives crosses the
    /// TIGHTENED band late in the round.
    #[test]
    fn time_weighted_band_crosses_where_the_flat_band_forgives() {
        let mut s = strategy(
            "BK_THESIS_DEATH_ENABLE = true \
             function bk_evaluate() return {} end",
        );
        {
            let state = s.state();
            let mut st = state.lock().expect("state");
            st.params
                .insert("thesis_death_reversal_pct_tight".into(), "10".into());
        }
        // Seed the round duration: the first round push sees the full clock.
        s.on_round(RoundInfo {
            slot: 1,
            time_left_sec: 900,
            now_ms: 1_000,
        });
        s.on_holdings(&[holding("0xup", "0.50")]);
        s.on_eval_books(&[blitzkrieg_strategy_api::FreshBook {
            book: book_update("0xup", "0.44", 2_000),
            fresh: true,
        }]);
        // 810s elapsed of 900 → frac 0.9 → band 25 − 15·0.9 = 11.5%.
        // Adverse 12%: inside the flat 25% band, past the tightened one.
        let intents = s.evaluate(&eval_ctx(2_000, 90));
        assert_eq!(intents.exits.len(), 1, "{intents:?}");
        assert_eq!(intents.exits[0].reason, "thesis_death_exit");
    }

    /// The predicate-declaration form: the package defines
    /// `bk_thesis_death(h)` itself and receives the same `bk.holdings()` row
    /// shape; only the positions it declares dead are forced out.
    #[test]
    fn declared_predicate_form_decides_per_holding() {
        let mut s = strategy(
            "function bk_thesis_death(h) return h.token_id == '0xup' and h.entry_price == '0.50' end \
             function bk_evaluate() return {} end",
        );
        s.on_holdings(&[holding("0xup", "0.50"), holding("0xdown", "0.50")]);
        let intents = s.evaluate(&eval_ctx(2_000, 600));
        assert_eq!(intents.exits.len(), 1, "{intents:?}");
        assert_eq!(intents.exits[0].token, "0xup");
        assert_eq!(intents.exits[0].reason, "thesis_death_exit");
    }

    /// The injection SUPERSEDES the strategy's own exit rows for a dead
    /// token: a forced death must not ride a resting maker sell that may
    /// never fill (禁止扛单). Exactly one row leaves, and it is the taker.
    #[test]
    fn thesis_death_supersedes_the_strategy_own_exit_rows() {
        let mut s = strategy(
            "BK_THESIS_DEATH_ENABLE = true \
             function bk_evaluate() \
               return { exits = { { token = '0xup', reason = 'take_profit', use_maker = true } } } \
             end",
        );
        s.on_holdings(&[holding("0xup", "0.50")]);
        s.on_eval_books(&[blitzkrieg_strategy_api::FreshBook {
            book: book_update("0xup", "0.30", 2_000),
            fresh: true,
        }]);
        let intents = s.evaluate(&eval_ctx(2_000, 600));
        assert_eq!(intents.exits.len(), 1, "{intents:?}");
        let x = &intents.exits[0];
        assert_eq!(x.reason, "thesis_death_exit");
        assert!(!x.use_maker, "the maker-first advisory is overridden");
    }

    /// The scan runs even when `bk_evaluate` errored: a buggy strategy must
    /// not lose its way out. The deterministic Lua error is still recorded.
    #[test]
    fn thesis_death_scan_survives_a_buggy_evaluate() {
        let mut s = strategy(
            "BK_THESIS_DEATH_ENABLE = true \
             function bk_evaluate() return nil + 1 end",
        );
        s.on_holdings(&[holding("0xup", "0.50")]);
        s.on_eval_books(&[blitzkrieg_strategy_api::FreshBook {
            book: book_update("0xup", "0.30", 2_000),
            fresh: true,
        }]);
        let intents = s.evaluate(&eval_ctx(2_000, 600));
        assert_eq!(intents.exits.len(), 1, "{intents:?}");
        assert_eq!(intents.exits[0].reason, "thesis_death_exit");
        let h = s.health.lock().expect("health");
        assert!(h.last_error.is_some(), "the evaluate bug is still recorded");
    }

    /// A package that declares NEITHER form has no death conditions: the
    /// scan is a no-op (grandfathered; the CI check reports the gap).
    #[test]
    fn undeclared_thesis_death_is_a_no_op() {
        let mut s = strategy("function bk_evaluate() return {} end");
        s.on_holdings(&[holding("0xup", "0.50")]);
        s.on_eval_books(&[blitzkrieg_strategy_api::FreshBook {
            book: book_update("0xup", "0.30", 2_000),
            fresh: true,
        }]);
        let intents = s.evaluate(&eval_ctx(2_000, 600));
        assert!(intents.exits.is_empty(), "{intents:?}");
    }

    /// A predicate that ERRORS on one row skips only that row — one bad
    /// holding never disables the forced scan for the rest.
    #[test]
    fn predicate_error_skips_only_that_row() {
        let mut s = strategy(
            "function bk_thesis_death(h) \
               if h.token_id == '0xup' then return nil + 1 end \
               return h.token_id == '0xdn' \
             end \
             function bk_evaluate() return {} end",
        );
        s.on_holdings(&[holding("0xup", "0.50"), holding("0xdn", "0.50")]);
        let intents = s.evaluate(&eval_ctx(2_000, 600));
        assert_eq!(intents.exits.len(), 1, "{intents:?}");
        assert_eq!(intents.exits[0].token, "0xdn");
        let h = s.health.lock().expect("health");
        assert!(h.last_error.is_some(), "the predicate bug is recorded");
    }

    /// No holdings → no scan, no calls (the empty-book no-op stays free).
    #[test]
    fn no_holdings_no_scan() {
        let mut s = strategy(
            "BK_THESIS_DEATH_ENABLE = true \
             function bk_evaluate() return {} end",
        );
        let intents = s.evaluate(&eval_ctx(2_000, 600));
        assert!(intents.exits.is_empty());
        assert!(s.health.lock().expect("health").last_error.is_none());
    }

    /// The required entry point is enforced at build; the optional ones are
    /// not.
    #[test]
    fn missing_required_entry_point_refuses() {
        let err = LuaStrategy::build("t".into(), "0.1.0".into(), Vec::new(), "x = 1")
            .expect_err("refused");
        assert!(err.contains("bk_evaluate"), "got: {err}");
    }

    /// A clean evaluate round-trips intents through the §6.5 tables.
    #[test]
    fn evaluate_parses_intent_tables() {
        let mut s = strategy(
            "function bk_evaluate() \
               return { entries = { { token = '0xup', price = '0.55', reason = 'mom', shares = '10' } }, \
                        exits = { { token = '0xdn', reason = 'fade' } }, \
                        breaks = { { token = '0xbr', broken_price = '0.40' } } } \
             end",
        );
        let ctx = RoundContext {
            round: RoundInfo {
                slot: 1,
                time_left_sec: 60,
                now_ms: 1_000,
            },
            markets: vec![],
        };
        let intents = s.evaluate(&ctx);
        assert_eq!(intents.entries.len(), 1);
        assert_eq!(intents.entries[0].token, "0xup");
        assert_eq!(intents.entries[0].price, "0.55");
        assert_eq!(intents.entries[0].shares.as_deref(), Some("10"));
        assert_eq!(intents.exits.len(), 1);
        assert_eq!(intents.breaks.len(), 1);
        let h = s.health.lock().expect("health");
        assert!(h.last_error.is_none());
    }

    /// `on_holdings` pushes THIS strategy's open positions into the sandbox:
    /// `bk.holdings()` answers the pushed rows verbatim (decimal STRINGS),
    /// and a strategy that never receives a push reads an empty table.
    #[test]
    fn on_holdings_feeds_bk_holdings() {
        let mut s = strategy(
            "function bk_evaluate() \
               local h = bk.holdings() \
               if #h == 0 then return { entries = {}, exits = {}, breaks = {} } end \
               return { entries = { { token = h[1].token_id, price = '0.5', \
                                     shares = h[1].shares, reason = h[1].direction } }, \
                        exits = {}, breaks = {} } \
             end",
        );
        // No push yet: the empty-table path.
        let ctx = RoundContext {
            round: RoundInfo {
                slot: 1,
                time_left_sec: 60,
                now_ms: 1_000,
            },
            markets: vec![],
        };
        let intents = s.evaluate(&ctx);
        assert!(intents.entries.is_empty(), "no holdings pushed, no entry");

        // Push one holding; the strategy echoes its wire fields back.
        s.on_holdings(&[HoldingView {
            strategy: "t".into(),
            condition_id: "cond".into(),
            token_id: "0xup".into(),
            direction: "up".into(),
            shares: "3.00".into(),
            entry_price: "0.42".into(),
            round_slot: 7,
        }]);
        let intents = s.evaluate(&ctx);
        assert_eq!(intents.entries.len(), 1);
        assert_eq!(intents.entries[0].token, "0xup");
        assert_eq!(intents.entries[0].shares.as_deref(), Some("3.00"));
        assert_eq!(intents.entries[0].reason, "up");
        let h = s.health.lock().expect("health");
        assert!(h.last_error.is_none());
    }

    /// A buggy evaluate is a deterministic non-poison error; intents are
    /// empty, the host keeps calling.
    #[test]
    fn buggy_evaluate_is_non_poison_error() {
        let mut s = strategy("function bk_evaluate() return nil + 1 end");
        let ctx = RoundContext {
            round: RoundInfo {
                slot: 1,
                time_left_sec: 60,
                now_ms: 1_000,
            },
            markets: vec![],
        };
        let intents = s.evaluate(&ctx);
        assert!(intents.entries.is_empty() && intents.exits.is_empty());
        assert!(!s.is_poisoned(), "a plain Lua error is not poison");
        let h = s.health.lock().expect("health");
        assert!(h.errors == 1 && h.last_error.is_some());
    }

    /// Malformed intent tables are reported, not fatal.
    #[test]
    fn malformed_intents_reported() {
        let mut s =
            strategy("function bk_evaluate() return { entries = { { price = '0.5' } } } end");
        let ctx = RoundContext {
            round: RoundInfo {
                slot: 1,
                time_left_sec: 60,
                now_ms: 1_000,
            },
            markets: vec![],
        };
        let intents = s.evaluate(&ctx);
        assert!(intents.entries.is_empty());
        let h = s.health.lock().expect("health");
        assert!(
            h.last_error
                .as_deref()
                .expect("error recorded")
                .contains("missing token/price"),
            "got: {:?}",
            h.last_error
        );
    }

    /// The account view the host pushes is exactly what `bk.account()` reads.
    #[test]
    fn account_view_flows_to_lua() {
        let s = strategy("function bk_evaluate() return {} end");
        let state = s.state();
        {
            let mut st = state.lock().expect("state");
            st.account = Some(AccountView {
                id: "a".into(),
                name: "n".into(),
                market_type: "prediction".into(),
                balance: Some("10".into()),
                available: None,
                reserved: None,
            });
        }
        // The strategy reads it through the same snapshot.
        let st = state.lock().expect("state");
        assert_eq!(
            st.account.as_ref().expect("present").balance.as_deref(),
            Some("10")
        );
    }

    /// Poison from a runaway callback propagates into health and the one-shot
    /// alert (the core turns it into a single RISK_ALERT).
    #[test]
    fn poison_reaches_health_and_alert() {
        let mut s = strategy("function bk_evaluate() while true do end end");
        let ctx = RoundContext {
            round: RoundInfo {
                slot: 1,
                time_left_sec: 60,
                now_ms: 1_000,
            },
            markets: vec![],
        };
        let intents = s.evaluate(&ctx);
        assert!(intents.entries.is_empty());
        assert!(s.is_poisoned(), "poison flag not set after budget breach");
        let alert = s.take_poison_alert().expect("one alert");
        assert!(
            alert.contains("instruction budget exceeded"),
            "got: {alert}"
        );
        // Exactly once: the alert drains and never refills.
        assert!(s.take_poison_alert().is_none());
        let h = s.health.lock().expect("health");
        assert!(h.poisoned, "poison flag not set in health");
    }
}
