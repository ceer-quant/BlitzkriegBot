//! SafeStrategy — the ergonomic half of `blitzkrieg-strategy-api` (E9-a / #60).
//!
//! Writing a v2 strategy against the raw C ABI means hand-writing ~250 lines of
//! `unsafe` plumbing (`user_layer/strategies/dog_strategy.rs`): the vtable,
//! `#[no_mangle]` exports, `create`/`destroy` boxing, `char*`↔String
//! conversions, and the JSON envelope around every hook. A business developer
//! should not touch any of that.
//!
//! [`SafeStrategy`] is the template contract: implement `name`, `on_book`,
//! `evaluate` (and optionally the config/knob hooks) on a plain `struct`, then
//! expose it with [`export_strategy!`]. That macro generates ALL of the ABI
//! surface — vtable, static name/version, the `#[no_mangle]` exports — with
//! zero `unsafe` in the caller's code. The generated dylib is indistinguishable
//! from a hand-rolled v2 library, so the kernel (`strategy.load`, negotiation,
//! receipts) needs no new support, and existing manual-FFI strategies
//! (dog_strategy) keep loading unchanged.
//!
//! Types are deliberately simple (`f64`/`String`): the shell parses the ABI's
//! decimal strings once per update, the same numbers dog_strategy computes.
//!
//! # The kernel-side contract is unchanged
//!
//! Intents are still just JSON returned from `evaluate`: entries carry no size
//! (the host sizes), exits carry no price (the host prices off the live book),
//! and everything still passes the kernel's risk gates. The strategy never
//! sees a signer, venue client, socket or credential.

use core::ffi::c_char;
use std::collections::HashMap;
use std::ffi::{CStr, CString};

pub use crate::{BK_ABI_VERSION, BK_MIN_ABI_VERSION, BkHandle};

/// One book/top-of-book update, numbers already parsed from the ABI decimal
/// strings. Side-level counts reflect how many ladder rows the host delivered.
#[derive(Debug, Clone, Default)]
pub struct BookUpdate {
    pub symbol: String,
    pub asset: String,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub mid: Option<f64>,
    pub bid_depth: Option<f64>,
    pub ask_depth: Option<f64>,
    pub obi: Option<f64>,
    pub spread: Option<f64>,
    pub spread_pct: Option<f64>,
    pub timestamp_ms: i64,
    pub bid_levels: usize,
    pub ask_levels: usize,
}

/// New round notification.
#[derive(Debug, Clone, Copy)]
pub struct RoundInfo {
    pub slot: i64,
    pub time_left_sec: i64,
    pub now_ms: i64,
}

/// A binary market in the current round, borrowed `char*` ids resolved to
/// owned strings.
#[derive(Debug, Clone)]
pub struct MarketInfo {
    pub asset: String,
    pub condition_id: String,
    pub up_token: String,
    pub down_token: String,
    pub expires_at_ms: i64,
    pub slot: i64,
    pub neg_risk: bool,
}

/// One evaluation cycle's context: round timing + the round's markets.
#[derive(Debug, Clone)]
pub struct RoundContext {
    pub round: RoundInfo,
    pub markets: Vec<MarketInfo>,
}

/// An entry intent. `price` is a LIMIT price; the kernel validates against the
/// live book, sizes the order and owns submission.
#[derive(Debug, Clone)]
pub struct Entry {
    pub token: String,
    pub price: f64,
    pub reason: String,
}

/// An exit intent for an open position; the kernel prices off the live book.
#[derive(Debug, Clone)]
pub struct Exit {
    pub token: String,
    pub reason: String,
}

/// A trend break (the kernel cancels the token's resting bids).
#[derive(Debug, Clone)]
pub struct Break {
    pub token: String,
    pub broken_price: f64,
}

/// The intents one `evaluate` call produces. Default = nothing.
#[derive(Debug, Default, Clone)]
pub struct Intents {
    pub entries: Vec<Entry>,
    pub exits: Vec<Exit>,
    pub breaks: Vec<Break>,
}

impl Intents {
    pub fn none() -> Self {
        Self::default()
    }
}

/// One evolvable knob declaration (E2-c): name, the value in force, and the
/// hard domain a shadow variant may never leave. Values are decimal strings to
/// match the ABI exactly.
#[derive(Debug, Clone)]
pub struct Knob {
    pub name: String,
    pub value: String,
    pub min: String,
    pub max: String,
}

/// Host config / shadow-evolution hot params, values kept as the ABI's decimal
/// strings. Helper accessors coerce to f64 on demand.
#[derive(Debug, Clone, Default)]
pub struct ParamBag(pub HashMap<String, String>);

impl ParamBag {
    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.0.get(key)?.parse().ok()
    }
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
}

/// The template contract a Blitzkrieg strategy author implements. Only `name`,
/// `on_book` and `evaluate` are required; every other hook has a neutral
/// default, exactly matching the kernel-side optional symbols.
pub trait SafeStrategy: Send + 'static {
    /// Unique strategy name the kernel registers (and the operator enables).
    fn name(&self) -> &str;
    /// Semantic version shown in load receipts and listings.
    fn version(&self) -> &str {
        "0.1.0"
    }

    /// Called on EVERY book/top-of-book update.
    fn on_book(&mut self, update: &BookUpdate);

    /// Called when a round starts. Default: no-op.
    fn on_round(&mut self, _round: RoundInfo) {}

    /// Produce this cycle's intents. REQUIRED.
    fn evaluate(&mut self, ctx: &RoundContext) -> Intents;

    /// Tokens currently considered entry-eligible. Default: none.
    fn confirmed_tokens(&self) -> Vec<String> {
        Vec::new()
    }

    /// Diagnostics payload entries (embedded as a JSON array). Default: none.
    fn diagnostics(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }

    /// Hot parameters the kernel pushes (host-config and shadow-evolution bags
    /// both arrive here). Return false to reject the whole bag (kernel keeps
    /// the previous). Default: accept.
    fn on_params(&mut self, _params: &ParamBag) -> bool {
        true
    }

    /// The knobs that are EVOLVABLE (E2-c). Empty = not evolvable, same as an
    /// ABI library without the optional symbol.
    fn evolvable_knobs(&self) -> Vec<Knob> {
        Vec::new()
    }

    /// Which shared entry gates this strategy wants exempted for its OWN
    /// candidates ("timing" and/or "momentum"). Empty = fully gated (E2-b).
    /// Not a safety bypass: the kernel honours only these two named gates and
    /// logs every honoured exemption.
    fn gate_exemptions(&self) -> &'static [&'static str] {
        &[]
    }
}

// ── shell plumbing (strategy authors never see any of this) ──────────────────

/// # Safety
/// `p` must point at a valid NUL-terminated string (or be null), borrowed for
/// the call — the ABI's read-only borrow rule.
pub unsafe fn cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .ok()
        .map(|s| s.to_string())
}
/// # Safety
/// Same contract as [`cstr`]: `p` is a borrowed NUL-terminated decimal string.
pub unsafe fn num(p: *const c_char) -> Option<f64> {
    unsafe { cstr(p) }.and_then(|s| s.parse().ok())
}

pub fn json_bytes_to_out_unused(v: &serde_json::Value) -> *mut c_char {
    match CString::new(v.to_string()) {
        Ok(cs) => cs.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// Hand an owned JSON string across the ABI (alias of [`crate::bk_string_out`]).
pub fn json_out(s: String) -> *mut c_char {
    crate::bk_string_out(s)
}

pub fn parse_params(json: &str) -> ParamBag {
    let mut bag = ParamBag::default();
    if let Some(obj) = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.as_object().cloned())
    {
        for (k, value) in obj {
            let s = match value {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            bag.0.insert(k, s);
        }
    }
    bag
}

pub fn parse_book(view: &crate::BkBookView) -> BookUpdate {
    unsafe {
        BookUpdate {
            symbol: cstr(view.symbol).unwrap_or_default(),
            asset: cstr(view.asset).unwrap_or_default(),
            best_bid: num(view.best_bid),
            best_ask: num(view.best_ask),
            mid: num(view.mid),
            bid_depth: num(view.bid_depth),
            ask_depth: num(view.ask_depth),
            obi: num(view.obi),
            spread: num(view.spread),
            spread_pct: num(view.spread_pct),
            timestamp_ms: view.timestamp_ms,
            bid_levels: if view.bids.is_null() {
                0
            } else {
                view.bid_count
            },
            ask_levels: if view.asks.is_null() {
                0
            } else {
                view.ask_count
            },
        }
    }
}

/// Expose a [`SafeStrategy`] as a full Blitzkrieg C ABI v2 dylib.
///
/// Usage — one line at crate root, after the `impl SafeStrategy` (the struct
/// must implement `Default`, which the factory uses to build instances):
///
/// ```ignore
/// #[derive(Default)]
/// struct MyStrategy { /* params */ }
/// impl SafeStrategy for MyStrategy { /* name / on_book / evaluate */ }
/// blitzkrieg_strategy_api::export_strategy!(MyStrategy);
/// ```
///
/// Emits an internal `__bk_export` module with:
/// - the vtable wired to every hook (hooks the strategy left at their neutral
///   default are simply never called with meaningful output = the same wire
///   behaviour as a hand-rolled library omitting the optional symbol),
/// - `#[no_mangle]` exports `bk_strategy_create`, `bk_strategy_abi_version`,
///   `bk_strategy_free_string`, `bk_strategy_gate_exemptions`,
///   `bk_strategy_evolvable_knobs` — the optional-symbol exports always
///   exist and degrade to "nothing declared" XML when the strategy returns
///   no exemptions/knobs, which the kernel reads exactly like a hand-rolled
///   library without the symbol.
#[macro_export]
macro_rules! export_strategy {
    ($type:ty) => {
        #[allow(non_snake_case)]
        mod __bk_export {
            type Strategy = $type;
            use $crate::shell::{cstr, json_out, parse_book, parse_params};
            use $crate::{
                BkBookView, BkHandle, BkRoundView, BkStrategyVtable, SafeStrategy, BK_ABI_VERSION,
                BK_MIN_ABI_VERSION,
            };
            use core::ffi::c_char;
            use std::collections::HashMap;

            struct Shell {
                inner: Strategy,
                books: HashMap<String, $crate::BookUpdate>,
            }
            impl Shell {
                fn new() -> Self {
                    Self {
                        inner: <Strategy as ::std::default::Default>::default(),
                        books: HashMap::new(),
                    }
                }
            }
            unsafe impl Send for Shell {}

            // ── required hooks ───────────────────────────────────────────────
            unsafe extern "C" fn create() -> BkHandle {
                Box::into_raw(Box::new(Shell::new())) as BkHandle
            }
            unsafe extern "C" fn destroy(handle: BkHandle) {
                if !handle.is_null() {
                    drop(unsafe { Box::from_raw(handle as *mut Shell) });
                }
            }
            unsafe extern "C" fn on_book(handle: BkHandle, view: *const BkBookView) {
                if handle.is_null() || view.is_null() {
                    return;
                }
                let s = unsafe { &mut *(handle as *mut Shell) };
                let upd = parse_book(unsafe { &*view });
                s.books.insert(upd.symbol.clone(), upd.clone());
                s.inner.on_book(&upd);
            }
            unsafe extern "C" fn on_round(handle: BkHandle, round: *const $crate::BkRound) {
                if handle.is_null() || round.is_null() {
                    return;
                }
                let s = unsafe { &mut *(handle as *mut Shell) };
                let r = unsafe { &*round };
                s.inner.on_round($crate::RoundInfo {
                    slot: r.slot,
                    time_left_sec: r.time_left_sec,
                    now_ms: r.now_ms,
                });
            }
            unsafe extern "C" fn evaluate(handle: BkHandle, view: *const BkRoundView) -> *mut c_char {
                if handle.is_null() || view.is_null() {
                    return core::ptr::null_mut();
                }
                let s = unsafe { &mut *(handle as *mut Shell) };
                let rv = unsafe { &*view };
                let round = $crate::RoundInfo {
                    slot: rv.round.slot,
                    time_left_sec: rv.round.time_left_sec,
                    now_ms: rv.round.now_ms,
                };
                let markets: Vec<$crate::MarketInfo> =
                    if rv.markets.is_null() || rv.market_count == 0 {
                        Vec::new()
                    } else {
                        unsafe { std::slice::from_raw_parts(rv.markets, rv.market_count) }
                            .iter()
                            .map(|m| $crate::MarketInfo {
                                asset: unsafe { cstr(m.asset) }.unwrap_or_default(),
                                condition_id: unsafe { cstr(m.condition_id) }.unwrap_or_default(),
                                up_token: unsafe { cstr(m.up_token) }.unwrap_or_default(),
                                down_token: unsafe { cstr(m.down_token) }.unwrap_or_default(),
                                expires_at_ms: m.expires_at_ms,
                                slot: m.slot,
                                neg_risk: m.neg_risk != 0,
                            })
                            .collect()
                    };
                let ctx = $crate::RoundContext { round, markets };
                let intents = s.inner.evaluate(&ctx);
                let entries: Vec<_> = intents
                    .entries
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "token": e.token,
                            "price": format!("{}", e.price),
                            "reason": e.reason,
                        })
                    })
                    .collect();
                let exits: Vec<_> = intents
                    .exits
                    .iter()
                    .map(|e| serde_json::json!({ "token": e.token, "reason": e.reason }))
                    .collect();
                let breaks: Vec<_> = intents
                    .breaks
                    .iter()
                    .map(|b| {
                        serde_json::json!({
                            "token": b.token,
                            "broken_price": format!("{}", b.broken_price),
                        })
                    })
                    .collect();
                json_out(
                    serde_json::json!({ "entries": entries, "exits": exits, "breaks": breaks })
                        .to_string(),
                )
            }

            // ── optional hooks (empty default = kernel "not declared") ──────
            unsafe extern "C" fn confirmed_tokens(handle: BkHandle) -> *mut c_char {
                if handle.is_null() {
                    return json_out("[]".into());
                }
                let s = unsafe { &*(handle as *const Shell) };
                json_out(
                    serde_json::to_string(&s.inner.confirmed_tokens())
                        .unwrap_or_else(|_| "[]".into()),
                )
            }
            unsafe extern "C" fn take_breaks(handle: BkHandle) -> *mut c_char {
                // Break intents flow through the evaluate envelope; this
                // optional kernel poll is not used by the shell.
                let _ = handle;
                json_out("[]".into())
            }
            unsafe extern "C" fn diagnostics(handle: BkHandle) -> *mut c_char {
                if handle.is_null() {
                    return json_out("[]".into());
                }
                let s = unsafe { &*(handle as *const Shell) };
                json_out(
                    serde_json::to_string(&s.inner.diagnostics())
                        .unwrap_or_else(|_| "[]".into()),
                )
            }
            unsafe extern "C" fn on_config(handle: BkHandle, json: *const c_char) -> i32 {
                let Some(j) = (unsafe { cstr(json) }) else { return 1 };
                let s = unsafe { &mut *(handle as *mut Shell) };
                if s.inner.on_params(&parse_params(&j)) { 0 } else { 1 }
            }
            unsafe extern "C" fn on_hot_params(handle: BkHandle, json: *const c_char) -> i32 {
                let Some(j) = (unsafe { cstr(json) }) else { return 1 };
                let s = unsafe { &mut *(handle as *mut Shell) };
                if s.inner.on_params(&parse_params(&j)) { 0 } else { 1 }
            }
            unsafe extern "C" fn knobs(handle: BkHandle) -> *mut c_char {
                if handle.is_null() {
                    return core::ptr::null_mut();
                }
                let s = unsafe { &*(handle as *const Shell) };
                let kn = s.inner.evolvable_knobs();
                if kn.is_empty() {
                    return core::ptr::null_mut();
                }
                let props: serde_json::Map<String, serde_json::Value> = kn
                    .iter()
                    .map(|k| {
                        (
                            k.name.clone(),
                            serde_json::json!({ "type": "string", "description": k.name }),
                        )
                    })
                    .collect();
                json_out(
                    serde_json::json!({ "type": "object", "properties": props }).to_string(),
                )
            }
            unsafe extern "C" fn evolvable_knobs(handle: BkHandle) -> *mut c_char {
                if handle.is_null() {
                    return json_out(r#"{"knobs":[]}"#.into());
                }
                let s = unsafe { &*(handle as *const Shell) };
                let kn: Vec<_> = s
                    .inner
                    .evolvable_knobs()
                    .into_iter()
                    .map(|k| {
                        serde_json::json!({
                            "name": k.name, "value": k.value, "min": k.min, "max": k.max,
                        })
                    })
                    .collect();
                json_out(serde_json::json!({ "knobs": kn }).to_string())
            }
            unsafe extern "C" fn gate_exemptions(handle: BkHandle) -> *mut c_char {
                let gates: Vec<&str> = if handle.is_null() {
                    Vec::new()
                } else {
                    let s = unsafe { &*(handle as *const Shell) };
                    s.inner.gate_exemptions().to_vec()
                };
                json_out(
                    serde_json::json!({
                        "timing": gates.contains(&"timing"),
                        "momentum": gates.contains(&"momentum"),
                    })
                    .to_string(),
                )
            }

            // The vtable's name must be what `SafeStrategy::name` declares:
            // the kernel dlopen resolves the factory, calls `create()` and
            // reads the name from this static. Cache the default instance's
            // name at first call — it is a fixed, static-lived string.
            static NAME_CACHED: std::sync::OnceLock<&'static [u8]> = std::sync::OnceLock::new();
            fn name_static() -> &'static [u8] {
                NAME_CACHED.get_or_init(|| {
                    let mut owned = <Strategy as ::std::default::Default>::default()
                        .name()
                        .as_bytes()
                        .to_vec();
                    owned.push(0); // NUL
                    owned.leak()
                })
            }
            static VERSION_UPDATED: std::sync::OnceLock<&'static [u8]> = std::sync::OnceLock::new();
            fn version_static() -> &'static [u8] {
                VERSION_UPDATED.get_or_init(|| {
                    let mut owned = <Strategy as ::std::default::Default>::default()
                        .version()
                        .as_bytes()
                        .to_vec();
                    owned.push(0); // NUL
                    owned.leak()
                })
            }
            // SAFETY: both point into OnceLock-stored Strings that are never
            // mutated after first init; the kernel reads them borrowed.
            static VTABLE_HOLDER: std::sync::Mutex<Option<BkStrategyVtable>> =
                std::sync::Mutex::new(None);
            fn vtable() -> &'static BkStrategyVtable {
                let mut held = VTABLE_HOLDER.lock().unwrap();
                if held.is_none() {
                    *held = Some(BkStrategyVtable {
                        name: name_static().as_ptr() as *const c_char,
                        version: version_static().as_ptr() as *const c_char,
                        abi_version: BK_ABI_VERSION,
                        min_abi: BK_MIN_ABI_VERSION,
                        create: Some(create),
                        destroy: Some(destroy),
                        on_book: Some(on_book),
                        on_round: Some(on_round),
                        evaluate: Some(evaluate),
                        confirmed_tokens: Some(confirmed_tokens),
                        take_breaks: Some(take_breaks),
                        diagnostics: Some(diagnostics),
                        on_config: Some(on_config),
                        on_hot_params: Some(on_hot_params),
                        knobs: Some(knobs),
                    });
                }
                let ptr = held.as_ref().unwrap() as *const BkStrategyVtable;
                // SAFETY: the Mutex<Option<T>> never moves after first write;
                // this is the lazy-static idiom.
                unsafe { &*ptr }
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn bk_strategy_create() -> *const BkStrategyVtable {
                vtable()
            }
            #[unsafe(no_mangle)]
            pub extern "C" fn bk_strategy_abi_version() -> u32 {
                BK_ABI_VERSION
            }
            #[unsafe(no_mangle)]
            pub extern "C" fn bk_strategy_gate_exemptions(handle: BkHandle) -> *mut c_char {
                unsafe { gate_exemptions(handle) }
            }
            #[unsafe(no_mangle)]
            pub extern "C" fn bk_strategy_evolvable_knobs(handle: BkHandle) -> *mut c_char {
                unsafe { evolvable_knobs(handle) }
            }
        }
    };
}

// ── E9-a self-test: the macro shell must produce a loadable vtable that
// round-trips intents exactly like a hand-rolled library (dog shape).
// Test-only: the example type is NOT part of the library, otherwise the
// `#[no_mangle]` exports it generates would collide with every strategy
// crate's own cdylib exports at link time.

#[cfg(test)]
#[derive(Default)]
struct Doubler {
    mids: Vec<(String, f64)>,
}
#[cfg(test)]
impl SafeStrategy for Doubler {
    fn name(&self) -> &str {
        "doubler"
    }
    fn version(&self) -> &str {
        "0.2.0"
    }
    fn on_book(&mut self, u: &BookUpdate) {
        if let Some(m) = u.mid {
            self.mids.push((u.symbol.clone(), m));
        }
    }
    fn evaluate(&mut self, ctx: &RoundContext) -> Intents {
        let mut out = Intents::none();
        for m in &ctx.markets {
            for token in [&m.up_token, &m.down_token] {
                if let Some((_, mid)) = self.mids.iter().find(|(s, _)| s == token) {
                    out.entries.push(Entry {
                        token: token.clone(),
                        price: *mid,
                        reason: "doubler_mid".into(),
                    });
                }
            }
        }
        out
    }
    fn confirmed_tokens(&self) -> Vec<String> {
        self.mids.iter().map(|(s, _)| s.clone()).collect()
    }
    fn diagnostics(&self) -> Vec<serde_json::Value> {
        vec![serde_json::json!({ "books_seen": self.mids.len() })]
    }
    fn evolvable_knobs(&self) -> Vec<Knob> {
        vec![Knob { name: "bias".into(), value: "1.0".into(), min: "0.1".into(), max: "2.0".into() }]
    }
    fn gate_exemptions(&self) -> &'static [&'static str] {
        &["timing"]
    }
}
#[cfg(test)]
export_strategy!(crate::safe::Doubler);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BkLevel, BkMarket, BkRoundView};
    use std::ffi::CString;

    #[test]
    fn macro_shell_round_trips_intents() {
        // 1. the exported vtable exists and is ABI v2.
        let vtp = super::__bk_export::bk_strategy_create();
        assert!(!vtp.is_null(), "create returned null");
        let vt = unsafe { &*vtp };
        assert_eq!(vt.abi_version, crate::BK_ABI_VERSION);
        assert_eq!(super::__bk_export::bk_strategy_abi_version(), crate::BK_ABI_VERSION);

        // 2. instance lifecycle.
        let handle = unsafe { (vt.create.expect("create"))() };
        assert!(!handle.is_null());

        // 3. on_book round-trips a parsed mid.
        let sym = CString::new("UP").unwrap();
        let mid = CString::new("0.5").unwrap();
        let bb = CString::new("0.49").unwrap();
        let ba = CString::new("0.51").unwrap();
        let depth = CString::new("100").unwrap();
        let zero = CString::new("0").unwrap();
        let lvl = BkLevel { price: bb.as_ptr(), size: depth.as_ptr() };
        let view = crate::BkBookView {
            symbol: sym.as_ptr(),
            asset: c"BTC".as_ptr(),
            bids: &lvl,
            bid_count: 1,
            asks: &lvl,
            ask_count: 1,
            best_bid: bb.as_ptr(),
            best_ask: ba.as_ptr(),
            mid: mid.as_ptr(),
            bid_depth: depth.as_ptr(),
            ask_depth: depth.as_ptr(),
            obi: zero.as_ptr(),
            spread: zero.as_ptr(),
            spread_pct: zero.as_ptr(),
            timestamp_ms: 7,
        };
        unsafe { (vt.on_book.expect("on_book"))(handle, &view) };

        // 4. evaluate returns the kernel envelope JSON with our entry.
        let market = BkMarket {
            asset: c"BTC".as_ptr(),
            condition_id: c"0xc".as_ptr(),
            question_id: c"0xq".as_ptr(),
            up_token: sym.as_ptr(),
            down_token: c"DOWN".as_ptr(),
            expires_at_ms: 0,
            slot: 0,
            neg_risk: 1,
        };
        let round = crate::BkRound { slot: 0, time_left_sec: 60, now_ms: 0 };
        let rv = BkRoundView {
            round,
            markets: &market,
            market_count: 1,
        };
        let out_raw = unsafe { (vt.evaluate.expect("evaluate hook"))(handle, &rv) };
        assert!(!out_raw.is_null(), "evaluate returned null");
        let out = unsafe { CStr::from_ptr(out_raw) }.to_string_lossy().into_owned();
        unsafe { crate::bk_strategy_free_string(out_raw) };
        let v: serde_json::Value = serde_json::from_str(&out).expect("envelope json");
        let entries = v["entries"].as_array().expect("entries array");
        assert_eq!(entries.len(), 1, "{v}");
        assert_eq!(entries[0]["token"], "UP");
        assert_eq!(entries[0]["price"], "0.5");
        assert_eq!(entries[0]["reason"], "doubler_mid");

        // 5. optional hooks emit the declared shapes.
        let conf = super::__bk_export::bk_strategy_gate_exemptions(handle);
        let conf_s = unsafe { CStr::from_ptr(conf) }.to_string_lossy().into_owned();
        unsafe { crate::bk_strategy_free_string(conf) };
        let conf_v: serde_json::Value = serde_json::from_str(&conf_s).expect("exemptions json");
        assert_eq!(conf_v["timing"], serde_json::json!(true), "{conf_s}");
        assert_eq!(conf_v["momentum"], serde_json::json!(false), "{conf_s}");

        // 6. diagnostics round-trip.
        let d_raw = unsafe { (vt.diagnostics.expect("diagnostics"))(handle) };
        let d = unsafe { CStr::from_ptr(d_raw) }.to_string_lossy().into_owned();
        unsafe { crate::bk_strategy_free_string(d_raw) };
        assert!(d.contains("books_seen"), "{d}");

        // 7. destroy must not leak/crash.
        unsafe { (vt.destroy.expect("destroy"))(handle) };
    }
}
