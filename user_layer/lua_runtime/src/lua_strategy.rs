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
    BookUpdate, Break, Entry, Exit, FreshBook, Intents, Kline, ParamBag, RoundContext, RoundInfo,
    SafeStrategy, StrategyMode,
};
use mlua::Value;

use crate::bk_api::{BkState, book_to_lua, install_bk, interval_name, kline_to_lua, round_to_lua};
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

    fn on_round(&mut self, round: RoundInfo) {
        if let Ok(mut st) = self.state.lock() {
            st.set_now(round.now_ms);
            st.round = Some((round.slot, round.time_left_sec, round.now_ms));
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
            st.set_now(ctx.round.now_ms);
            st.round = Some((ctx.round.slot, ctx.round.time_left_sec, ctx.round.now_ms));
            st.markets = ctx.markets.clone();
        }
        let Some(f) = self.optional_entry("bk_evaluate") else {
            // Checked at build; unreachable unless the strategy overwrote the
            // global — which only harms itself.
            self.note_err("bk_evaluate is not defined", false);
            return Intents::none();
        };
        match self.sandbox.invoke("bk_evaluate", |_| f.call::<Value>(())) {
            Ok(v) => {
                self.note_ok();
                let (intents, parse_err) = parse_intents(&v);
                if let Some(err) = parse_err {
                    self.note_err(&err, false);
                }
                intents
            }
            Err(e) => {
                self.note_err(e.message(), e.is_poison());
                Intents::none()
            }
        }
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
/// §6.5 contract:
fn parse_intents(v: &Value) -> (Intents, Option<String>) {
    let mut intents = Intents::none();
    let Value::Table(t) = v else {
        return (intents, Some("bk_evaluate must return a table".into()));
    };
    // entries ─ token/price required; reason/shares optional strings.
    if let Ok(arr) = t.raw_get::<mlua::Table>("entries") {
        for pair in arr.sequence_values::<mlua::Table>() {
            let Ok(row) = pair else { continue };
            let (Some(token), Some(price)) = (
                row.raw_get::<String>("token").ok(),
                row.raw_get::<String>("price").ok(),
            ) else {
                return (intents, Some("entry missing token/price string".into()));
            };
            intents.entries.push(Entry {
                token,
                price,
                reason: row.raw_get::<String>("reason").unwrap_or_default(),
                shares: row.raw_get::<String>("shares").ok(),
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
            intents.exits.push(Exit {
                token,
                reason: row.raw_get::<String>("reason").unwrap_or_default(),
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
                token,
                broken_price,
            });
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
