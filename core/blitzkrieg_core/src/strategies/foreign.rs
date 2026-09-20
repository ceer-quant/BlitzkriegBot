//! External strategy adapter for C ABI v2.
//!
//! A [`ForeignStrategy`] wraps a dlopen'ed v2 vtable and implements the SAME
//! full [`EngineStrategy`] contract as an in-tree strategy. "External" is only
//! a loading difference: it receives every `on_book` with the full depth ladder
//! and derived metrics, round/market context, config and hot parameters, and it
//! returns entries, close intents, trend breaks, confirmation state and
//! diagnostics. Entries are sized by the host; close intents are priced and
//! submitted by the host through the shared exit path. Nothing credential-,
//! network-, OME- or socket-related ever crosses this boundary.
//!
//! All structured outputs cross as heap JSON strings allocated and freed by the
//! SAME loaded library (its `bk_strategy_free_string`), so allocators never mix.
//!
//! E2-c (#28): the adapter is now per-strategy in the evolution chain too.
//!  - hot parameters are pushed from THIS strategy's own cell in the kernel's
//!    [`ParamRegistry`](crate::shadow_evolution::ParamRegistry) — a name→value bag
//!    of the knobs the library itself declared, not spread_arb's four fields;
//!  - `evolvable_knobs` reads the OPTIONAL `bk_strategy_evolvable_knobs` symbol,
//!    so a library self-certifies which knobs may evolve and over what domain;
//!  - `shadow_factory` builds a **twin** by calling the library's `create()`
//!    again (a second, independent instance from the SAME handle table) and
//!    pushing it the counterfactual parameters — so a shadow variant runs the
//!    library's own entry/exit logic, exactly as the in-tree path does.
//!
//! Library ownership: the `Library` is held behind an `Arc` (`LoadedLibrary`) and
//! every instance destroys its own handle in `Drop`, which runs BEFORE the `Arc`
//! releases the library — so no vtable function pointer can outlive its library.
//! A twin holds the same `Arc`, so the library stays mapped while any twin lives.

use super::shadow_twin::ShadowFactory;
use super::{EngineStrategy, GateExemptions, StrategyCtx, StrategyExitIntent};
use crate::model::OrderbookSnapshot;
use crate::shadow_evolution::{KnobDeclaration, KnobSpec, ParamRegistry, StrategyParams};
use crate::signal::{SpreadArbConfig, TradeSignal, TrendConfig};
use arc_swap::ArcSwap;
use blitzkrieg_strategy_api::{
    BkBookView, BkEvalCtx, BkEvolvableKnobsFn, BkGateExemptionsFn, BkHandle, BkLevel, BkMarket,
    BkRound, BkRoundView, BkStrategyVtable, BkTokenBook,
};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_char};
use std::str::FromStr;
use std::sync::Arc;

/// A dlopen'ed strategy library plus the symbols the kernel resolves once, at
/// load time. Shared (`Arc`) by every instance the library produces, including
/// shadow twins.
pub struct LoadedLibrary {
    lib: libloading::Library,
    /// `bk_strategy_create` — returns the library's STATIC vtable pointer. Called
    /// once per instance (the live strategy and each twin) so every instance gets
    /// its own opaque handle from the same table.
    create_sym: unsafe extern "C" fn() -> *const BkStrategyVtable,
    /// This library's own JSON deallocator (`bk_strategy_free_string`).
    free_string: Option<unsafe extern "C" fn(*mut c_char)>,
    // Optional symbols (absent = capability not declared).
    gate_exemptions_fn: Option<BkGateExemptionsFn>,
    evolvable_knobs_fn: Option<BkEvolvableKnobsFn>,
    /// OPTIONAL `bk_strategy_bind_eval_ctx` (E-parity): the fresh-book gate.
    /// None = the library does not use the context (an older library) and
    /// keeps the plain v2 contract unchanged.
    bind_eval_ctx_fn: Option<blitzkrieg_strategy_api::BkBindEvalCtxFn>,
    /// OPTIONAL `bk_strategy_config_view` (E-parity): the config currently in
    /// force, as the strategy reports it. None = "declares nothing".
    config_view_fn: Option<blitzkrieg_strategy_api::BkConfigViewFn>,
}

impl LoadedLibrary {
    /// # Safety
    /// `lib` must have been opened by the caller and must export the mandatory v2
    /// symbols; the resolved symbols are valid for as long as `lib` is loaded,
    /// which this struct guarantees by owning it.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn new(
        lib: libloading::Library,
        create_sym: unsafe extern "C" fn() -> *const BkStrategyVtable,
        free_string: Option<unsafe extern "C" fn(*mut c_char)>,
        gate_exemptions_fn: Option<BkGateExemptionsFn>,
        evolvable_knobs_fn: Option<BkEvolvableKnobsFn>,
        bind_eval_ctx_fn: Option<blitzkrieg_strategy_api::BkBindEvalCtxFn>,
        config_view_fn: Option<blitzkrieg_strategy_api::BkConfigViewFn>,
    ) -> Arc<Self> {
        Arc::new(Self {
            lib,
            create_sym,
            free_string,
            gate_exemptions_fn,
            evolvable_knobs_fn,
            bind_eval_ctx_fn,
            config_view_fn,
        })
    }

    /// The library's static vtable, copied by value.
    fn vtable(&self) -> *const BkStrategyVtable {
        // SAFETY: symbol resolved from this library; the library guarantees a
        // pointer to static data.
        unsafe { (self.create_sym)() }
    }

    /// Path the library was loaded from (diagnostics).
    pub fn path(&self) -> Option<std::path::PathBuf> {
        // libloading exposes the path only through its own handle; the load
        // report carries it, so this is a no-op for observability.
        None
    }

    /// Keeps the mapping alive; dropping this drops the `Library` itself.
    pub fn library(&self) -> &libloading::Library {
        &self.lib
    }
}

/// Keeps every CString backing one `BkBookView` alive for the duration of the
/// FFI call. The view's pointers borrow from this struct.
struct BookMarshal {
    symbol: CString,
    asset: CString,
    best_bid: CString,
    best_ask: CString,
    mid: CString,
    bid_depth: CString,
    ask_depth: CString,
    obi: CString,
    spread: CString,
    spread_pct: CString,
    /// Owning storage for every ladder level's price/size CStrings. The raw
    /// pointers handed across the ABI in [`Self::view`] borrow from these, so
    /// they must outlive the FFI call (they do — self lives across it).
    bid_levels: Vec<(CString, CString)>,
    ask_levels: Vec<(CString, CString)>,
}

impl BookMarshal {
    fn new(token_id: &str, asset: &str, s: &OrderbookSnapshot) -> Option<Self> {
        let cs = |d: Decimal| CString::new(d.to_string()).unwrap_or_default();
        let mk = |v: &[(Decimal, Decimal)]| {
            v.iter()
                .map(|(p, sz)| (cs(*p), cs(*sz)))
                .collect::<Vec<_>>()
        };
        let bid_levels = mk(&s.bids);
        let ask_levels = mk(&s.asks);
        Some(Self {
            symbol: CString::new(token_id).ok()?,
            asset: CString::new(asset).ok()?,
            best_bid: cs(s.best_bid),
            best_ask: cs(s.best_ask),
            mid: cs(s.mid_price),
            bid_depth: cs(s.bid_depth),
            ask_depth: cs(s.ask_depth),
            obi: cs(s.obi),
            spread: cs(s.spread),
            spread_pct: cs(s.spread_pct),
            bid_levels,
            ask_levels,
        })
    }

    /// Raw FFI level arrays borrowing from self's owned CStrings. The caller
    /// must bind the returned vecs and keep them alive across the FFI call.
    fn level_views(&self) -> (Vec<BkLevel>, Vec<BkLevel>) {
        let to = |v: &[(CString, CString)]| {
            v.iter()
                .map(|(p, sz)| BkLevel {
                    price: p.as_ptr(),
                    size: sz.as_ptr(),
                })
                .collect()
        };
        (to(&self.bid_levels), to(&self.ask_levels))
    }

    fn view(&self, bids: &[BkLevel], asks: &[BkLevel], timestamp_ms: i64) -> BkBookView {
        BkBookView {
            symbol: self.symbol.as_ptr(),
            asset: self.asset.as_ptr(),
            bids: if bids.is_empty() {
                std::ptr::null()
            } else {
                bids.as_ptr()
            },
            bid_count: bids.len(),
            asks: if asks.is_empty() {
                std::ptr::null()
            } else {
                asks.as_ptr()
            },
            ask_count: asks.len(),
            best_bid: self.best_bid.as_ptr(),
            best_ask: self.best_ask.as_ptr(),
            mid: self.mid.as_ptr(),
            bid_depth: self.bid_depth.as_ptr(),
            ask_depth: self.ask_depth.as_ptr(),
            obi: self.obi.as_ptr(),
            spread: self.spread.as_ptr(),
            spread_pct: self.spread_pct.as_ptr(),
            timestamp_ms,
        }
    }
}

/// Backing storage for one evaluation context: the round tokens' priceable
/// books marshalled to the ABI form, kept alive across the FFI call. The
/// `BkEvalCtx` handed to the library borrows from here.
struct EvalCtxMarshal {
    /// Token id per row (the BkTokenBook.token pointer borrows from these).
    tokens: Vec<CString>,
    books: Vec<BookMarshal>,
}

/// The (ctx, rows, views) triple `build` hands out: `ctx` borrows from `rows`
/// and `views`, and all three must outlive the FFI call they were built for.
type BoundCtx = (
    BkEvalCtx,
    Vec<BkTokenBook>,
    Vec<(Vec<BkLevel>, Vec<BkLevel>)>,
);

impl EvalCtxMarshal {
    /// Marshal the context exactly as an in-tree strategy would see it. The
    /// in-tree `StrategyCtx::fresh_book` returns Some(snapshot) for a priceable
    /// book and None for a stale/missing one — with no way to tell those two
    /// apart — so the bound context carries exactly the priceable rows
    /// (`fresh = 1`); a token with no row is "not priceable now", identical to
    /// what `fresh_book(..) == None` means on the trait side.
    fn new(
        markets: &[crate::model::CryptoMarket],
        fresh_book: &dyn Fn(&str) -> Option<OrderbookSnapshot>,
        now_ms: i64,
    ) -> Self {
        let mut tokens = Vec::new();
        let mut books = Vec::new();
        for m in markets {
            for token in [&m.up_token_id, &m.down_token_id] {
                let Some(snap) = fresh_book(token) else {
                    continue;
                };
                let Some(marshal) = BookMarshal::new(token, &m.asset, &snap) else {
                    continue;
                };
                tokens.push(CString::new(token.as_str()).unwrap_or_default());
                books.push(marshal);
            }
        }
        let _ = now_ms;
        Self { tokens, books }
    }

    /// Build the borrowed `BkEvalCtx` + the arrays it points into. The caller
    /// must keep the returned (ctx, rows, views) tuple alive across the FFI
    /// call — `ctx`'s pointers borrow from `rows` and `views`.
    fn build(&self, round: BkRound, markets: *const BkMarket, market_count: usize) -> BoundCtx {
        let views: Vec<(Vec<BkLevel>, Vec<BkLevel>)> = self
            .books
            .iter()
            .map(|b| {
                let (bids, asks) = b.level_views();
                (bids, asks)
            })
            .collect();
        let rows: Vec<BkTokenBook> = self
            .tokens
            .iter()
            .zip(self.books.iter())
            .zip(views.iter())
            .map(|((token, b), (bids, asks))| BkTokenBook {
                token: token.as_ptr(),
                book: b.view(bids, asks, round.now_ms),
                fresh: 1,
            })
            .collect();
        let ctx = BkEvalCtx {
            view: BkRoundView {
                round,
                markets,
                market_count,
            },
            books: if rows.is_empty() {
                std::ptr::null()
            } else {
                rows.as_ptr()
            },
            book_count: rows.len(),
        };
        (ctx, rows, views)
    }
}

/// A loaded external v2 strategy. Owns its opaque handle; shares the library.
pub struct ForeignStrategy {
    lib: Arc<LoadedLibrary>,
    vtable: BkStrategyVtable,
    handle: BkHandle,
    name: String,
    version: String,
    /// Deallocator resolved from THIS library for its JSON outputs.
    free_string: Option<unsafe extern "C" fn(*mut c_char)>,
    /// Optional v2 symbol `bk_strategy_gate_exemptions` (E2-b / #27). None when
    /// the library does not export it — then the strategy declares nothing and
    /// stays fully gated, exactly like the in-tree default.
    gate_exemptions_fn: Option<BkGateExemptionsFn>,
    /// Optional v2 symbol `bk_strategy_evolvable_knobs` (E2-c / #28). None when
    /// the library does not export it — then the strategy is **not evolvable**
    /// (no variants are built for it), which is the explicit declaration, not an
    /// error.
    evolvable_knobs_fn: Option<BkEvolvableKnobsFn>,
    /// This strategy's own evolvable knobs, resolved once at load (they are a
    /// property of the library build, not of the runtime state).
    knobs: Vec<KnobSpec>,
    /// OPTIONAL binder symbol (copied from the shared library record).
    bind_eval_ctx_fn: Option<blitzkrieg_strategy_api::BkBindEvalCtxFn>,
    /// OPTIONAL config-view symbol (copied from the shared library record).
    config_view_fn: Option<blitzkrieg_strategy_api::BkConfigViewFn>,

    // Outputs accumulated during evaluate(), drained by the host.
    exit_intents: Vec<StrategyExitIntent>,
    breaks: Vec<(String, Decimal)>,

    // Shadow Evolution: THIS strategy's cell plus the JSON we last pushed, so the
    // strategy only sees a callback on an actual change.
    params: Option<Arc<ArcSwap<StrategyParams>>>,
    last_hot_json: Option<String>,

    // token → asset, refreshed from each round's markets so on_book (which only
    // carries a token) can still label the view with the underlying asset.
    assets: HashMap<String, String>,
}

impl ForeignStrategy {
    /// # Safety
    /// Caller provides a successfully negotiated v2 library: vtable/handle come
    /// from `lib`, the library implements the documented contract (static
    /// vtable, valid create/destroy, JSON outputs freed by its own
    /// `bk_strategy_free_string`), and it is only ever driven single-threaded
    /// by the kernel's strategy loop.
    pub unsafe fn from_loaded(lib: Arc<LoadedLibrary>, name: String, version: String) -> Self {
        let vtable = unsafe { std::ptr::read(lib.vtable()) };
        // SAFETY: negotiation in the loader established a v2 library with a
        // static vtable and a non-null create().
        let handle: BkHandle = unsafe { vtable.create.unwrap_or_else(|| unreachable!())() };
        let free_string = lib.free_string;
        let gate_exemptions_fn = lib.gate_exemptions_fn;
        let evolvable_knobs_fn = lib.evolvable_knobs_fn;
        let bind_eval_ctx_fn = lib.bind_eval_ctx_fn;
        let config_view_fn = lib.config_view_fn;
        let mut s = Self {
            lib,
            vtable,
            handle,
            name,
            version,
            free_string,
            gate_exemptions_fn,
            evolvable_knobs_fn,
            knobs: Vec::new(),
            bind_eval_ctx_fn,
            config_view_fn,
            exit_intents: Vec::new(),
            breaks: Vec::new(),
            params: None,
            last_hot_json: None,
            assets: HashMap::new(),
        };
        // Read the declaration once: it is a load-time property, and the load
        // report must be able to state it (E2-c: "not evolvable" is explicit).
        s.knobs = s.read_knobs();
        s
    }

    /// A second, independent instance from the same library (shadow twin). Same
    /// code, its own state, its own handle — nothing is shared with the live
    /// instance except the read-only vtable and the library mapping.
    pub fn spawn_twin(&self) -> Option<Self> {
        let vtable = unsafe { std::ptr::read(self.lib.vtable()) };
        let create = vtable.create?;
        let handle = unsafe { create() };
        if handle.is_null() {
            return None;
        }
        Some(Self {
            lib: self.lib.clone(),
            vtable,
            handle,
            name: self.name.clone(),
            version: self.version.clone(),
            free_string: self.free_string,
            gate_exemptions_fn: self.gate_exemptions_fn,
            evolvable_knobs_fn: self.evolvable_knobs_fn,
            knobs: self.knobs.clone(),
            bind_eval_ctx_fn: self.bind_eval_ctx_fn,
            config_view_fn: self.config_view_fn,
            exit_intents: Vec::new(),
            breaks: Vec::new(),
            params: None,
            last_hot_json: None,
            assets: HashMap::new(),
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// The knobs this library declared evolvable at load time.
    pub fn declared_knobs(&self) -> &[KnobSpec] {
        &self.knobs
    }

    /// Take a JSON heap string returned by the library and copy it into Rust,
    /// freeing it through the library's own deallocator. None on null/UTF-8
    /// error (a malformed output is treated as "empty", never a crash).
    unsafe fn take_json(&self, p: *mut c_char) -> Option<serde_json::Value> {
        if p.is_null() {
            return None;
        }
        // SAFETY: pointer came from this library's CString::into_raw; copy then
        // free once via the matching allocator.
        let parsed = unsafe { CStr::from_ptr(p) }
            .to_str()
            .ok()
            .and_then(|s| serde_json::from_str(s).ok());
        if let Some(f) = self.free_string {
            // SAFETY: same provenance as above.
            unsafe { f(p) };
        }
        parsed
    }

    /// Take a heap string returned by the library and copy it into Rust as-is,
    /// freeing it through the library's own deallocator. None on null/UTF-8
    /// error (a malformed output is treated as "empty", never a crash).
    unsafe fn take_raw_string(&self, p: *mut c_char) -> Option<String> {
        if p.is_null() {
            return None;
        }
        // SAFETY: pointer came from this library's CString::into_raw; copy then
        // free once via the matching allocator.
        let s = unsafe { CStr::from_ptr(p) }
            .to_str()
            .ok()
            .map(str::to_string);
        if let Some(f) = self.free_string {
            // SAFETY: same provenance as above.
            unsafe { f(p) };
        }
        s
    }

    /// E-parity: the config currently in force, reported through the library's
    /// OPTIONAL `bk_strategy_config_view` symbol. Missing symbol, null return
    /// or malformed UTF-8 all mean "declares nothing" — the same default an
    /// in-tree strategy gets from the trait default.
    pub fn read_config_view(&self) -> Option<String> {
        let f = self.config_view_fn?;
        // SAFETY: valid handle; the returned string is owned by the library and
        // freed through its own deallocator by take_raw_string.
        unsafe { self.take_raw_string(f(self.handle)) }
    }

    /// Read (and validate) the optional knob declaration. Malformed JSON, a null
    /// return or a missing symbol all mean "declares nothing" — never a panic and
    /// never a partially trusted declaration (incoherent specs are dropped).
    fn read_knobs(&self) -> Vec<KnobSpec> {
        let Some(f) = self.evolvable_knobs_fn else {
            return Vec::new();
        };
        // SAFETY: valid handle; the returned JSON is owned by the library and
        // freed through the library's own deallocator by take_json.
        let text = unsafe { self.take_json(f(self.handle)) }.map(|v| v.to_string());
        match text {
            Some(t) => KnobDeclaration::parse(&t).knobs,
            None => Vec::new(),
        }
    }

    /// Push a parameter set to the library's `on_hot_params`, deduplicating
    /// against the last JSON sent. The payload is THIS strategy's own bag of
    /// knob values (decimal strings), not another strategy's field list.
    fn push_params_json(&mut self, params: &StrategyParams) {
        let Some(f) = self.vtable.on_hot_params else {
            return;
        };
        let Ok(json) = serde_json::to_string(params) else {
            return;
        };
        if self.last_hot_json.as_deref() == Some(json.as_str()) {
            return;
        }
        if let Ok(cs) = CString::new(json.clone()) {
            // SAFETY: valid handle + borrowed NUL string for the call.
            unsafe { f(self.handle, cs.as_ptr()) };
        }
        self.last_hot_json = Some(json);
    }

    fn push_hot_params_if_changed(&mut self) {
        let Some(cell) = &self.params else { return };
        let params = (**cell.load()).clone();
        self.push_params_json(&params);
    }

    /// Apply a parameter set immediately, bypassing the registry cell. Used to
    /// seed a shadow twin with its counterfactual values before it is driven.
    fn apply_params_direct(&mut self, params: &StrategyParams) {
        self.last_hot_json = None;
        self.push_params_json(params);
    }

    /// Bind the evaluation context (round view + priceable books) to the
    /// library for the duration of ONE hook call, if the library opted in via
    /// the OPTIONAL `bk_strategy_bind_eval_ctx` symbol. The returned rows/views
    /// own everything the context points at; the caller must keep them alive
    /// until after the FFI call and then invoke [`Self::unbind_ctx`].
    fn bind_ctx(
        &self,
        marshal: &EvalCtxMarshal,
        round: BkRound,
        markets: *const BkMarket,
        market_count: usize,
    ) -> Option<BoundCtx> {
        let f = self.bind_eval_ctx_fn?;
        let (ctx, rows, views) = marshal.build(round, markets, market_count);
        // SAFETY: valid handle; `ctx` borrows `rows`/`views`, which the caller
        // keeps alive until the matching unbind.
        unsafe { f(self.handle, &ctx) };
        Some((ctx, rows, views))
    }

    /// Unbind whatever context is bound (harmless when the library has no
    /// binder or nothing is bound).
    fn unbind_ctx(&self) {
        if let Some(f) = self.bind_eval_ctx_fn {
            // SAFETY: valid handle; null only ends the borrow window.
            unsafe { f(self.handle, std::ptr::null()) };
        }
    }

    /// Bind the context, run `call` (the FFI evaluate/diagnostics hook), and
    /// guarantee the unbind (bind null) happens on EVERY path — including a
    /// panic. The `BkEvalCtx` the library sees borrows from host-owned storage
    /// that is freed when the guard drops; leaving it bound across that free
    /// would be a use-after-free waiting for the next hook call. A panicking
    /// hook is caught, the guard dropped (unbind first, rows/views after), and
    /// the panic RESUMED, so kernel-level panic semantics are exactly what they
    /// were before the binder existed. (A library built with the plain "C"
    /// unwind convention aborts inside its own frame before this can catch —
    /// this guard is the belt to that suspenders, covering host-side marshalling
    /// panics and C-unwind libraries.)
    fn call_with_bound_ctx<T>(
        &self,
        marshal: &EvalCtxMarshal,
        round: BkRound,
        markets: *const BkMarket,
        market_count: usize,
        call: impl FnOnce() -> T,
    ) -> T {
        let bound = self.bind_ctx(marshal, round, markets, market_count);
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(call));
        self.unbind_ctx();
        drop(bound);
        match out {
            Ok(v) => v,
            Err(e) => std::panic::resume_unwind(e),
        }
    }
}

// SAFETY: the loaded instance is driven only by the kernel's single strategy
// loop. The vtable is static; the opaque handle is owned exclusively here.
unsafe impl Send for ForeignStrategy {}
unsafe impl Sync for ForeignStrategy {}

impl EngineStrategy for ForeignStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    /// E2-b / #27: read the library's OPTIONAL `bk_strategy_gate_exemptions`
    /// symbol. A library that does not export it, returns null, or returns
    /// malformed JSON declares nothing and stays fully gated — the same default
    /// an in-tree strategy gets from the trait.
    fn gate_exemptions(&self) -> GateExemptions {
        let Some(f) = self.gate_exemptions_fn else {
            return GateExemptions::none();
        };
        // SAFETY: valid handle; the returned JSON string is owned by the library
        // and freed through the library's own deallocator by take_json.
        let v = unsafe { self.take_json(f(self.handle)) };
        v.as_ref()
            .map(GateExemptions::from_json)
            .unwrap_or_default()
    }

    /// E2-c / #28: the knobs this library declared evolvable (empty ⇒ not
    /// evolvable). Resolved once at load; a v2 library without the optional
    /// symbol is simply not evolvable, with no ABI bump required.
    fn evolvable_knobs(&self) -> Vec<KnobSpec> {
        self.knobs.clone()
    }

    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
        if self.knobs.is_empty() {
            return None; // explicit "not evolvable"
        }
        Some(Box::new(ForeignShadowFactory {
            lib: self.lib.clone(),
            name: self.name.clone(),
            knobs: self.knobs.clone(),
        }))
    }

    /// E-parity: the library's own config-in-force view, via the OPTIONAL
    /// `bk_strategy_config_view` symbol ("nothing declared" when absent).
    fn config_view_json(&self) -> Option<String> {
        self.read_config_view()
    }

    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, now_ms: i64) {
        let Some(f) = self.vtable.on_book else { return };
        // on_book carries only a token; label the view with the asset resolved
        // from the last round's markets (falls back to the token itself).
        let asset = self
            .assets
            .get(token_id)
            .map(|s| s.as_str())
            .unwrap_or(token_id);
        let Some(m) = BookMarshal::new(token_id, asset, snap) else {
            return;
        };
        // These raw-pointer arrays borrow from `m`; bind and hold them (along
        // with `m`) across the FFI call so nothing is freed underneath it.
        let (bid_levels, ask_levels) = m.level_views();
        let view = m.view(&bid_levels, &ask_levels, now_ms);
        // SAFETY: valid handle and a live view whose backing outlives the call.
        unsafe { f(self.handle, &view) };
    }

    fn on_round(&mut self, slot: i64, time_left_sec: i64, now_ms: i64) {
        if let Some(f) = self.vtable.on_round {
            // E-parity: the round's real timing, not zeros. The trait contract
            // promises the same context an in-tree strategy sees; the seconds
            // remaining and the host clock are part of that.
            let round = BkRound {
                slot,
                time_left_sec,
                now_ms,
            };
            // SAFETY: valid handle + borrowed round for the call.
            unsafe { f(self.handle, &round) };
        }
    }

    fn take_breaks(&mut self) -> Vec<(String, Decimal)> {
        // Merge any breaks the evaluate() JSON reported with the library's own
        // drained take_breaks hook output.
        if let Some(f) = self.vtable.take_breaks {
            // SAFETY: hook returns a heap JSON array owned by the library.
            if let Some(v) = unsafe { self.take_json(f(self.handle)) } {
                parse_breaks(&v)
                    .into_iter()
                    .for_each(|b| self.breaks.push(b));
            }
        }
        std::mem::take(&mut self.breaks)
    }

    fn confirmed_tokens(&self) -> HashSet<String> {
        let Some(f) = self.vtable.confirmed_tokens else {
            return HashSet::new();
        };
        // SAFETY: hook returns a heap JSON array owned by the library.
        match unsafe { self.take_json(f(self.handle)) } {
            Some(serde_json::Value::Array(a)) => a
                .into_iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
            _ => HashSet::new(),
        }
    }

    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        let Some(evaluate) = self.vtable.evaluate else {
            return Vec::new();
        };
        self.push_hot_params_if_changed();

        // Refresh token→asset for on_book labelling between evaluations.
        for m in ctx.markets() {
            self.assets.insert(m.up_token_id.clone(), m.asset.clone());
            self.assets.insert(m.down_token_id.clone(), m.asset.clone());
        }

        // Backing storage for the market rows must live across the FFI call.
        struct Rows {
            strs: Vec<(CString, CString, CString, CString, CString)>,
            view: Vec<BkMarket>,
        }
        let mut rows = Rows {
            strs: Vec::new(),
            view: Vec::new(),
        };
        for m in ctx.markets() {
            let cs = |s: &str| CString::new(s).unwrap_or_default();
            rows.strs.push((
                cs(&m.asset),
                cs(&m.condition_id),
                cs(&m.question_id),
                cs(&m.up_token_id),
                cs(&m.down_token_id),
            ));
        }
        rows.view = rows
            .strs
            .iter()
            .zip(ctx.markets().iter())
            .map(|(s, m)| BkMarket {
                asset: s.0.as_ptr(),
                condition_id: s.1.as_ptr(),
                question_id: s.2.as_ptr(),
                up_token: s.3.as_ptr(),
                down_token: s.4.as_ptr(),
                expires_at_ms: m.expires_at_ms,
                slot: m.round_slot,
                neg_risk: if m.neg_risk { 1 } else { 0 },
            })
            .collect();

        let round = BkRound {
            slot: ctx.round_slot(),
            time_left_sec: ctx.time_left_sec(),
            now_ms: ctx.now_ms(),
        };
        let rv = BkRoundView {
            round,
            markets: if rows.view.is_empty() {
                std::ptr::null()
            } else {
                rows.view.as_ptr()
            },
            market_count: rows.view.len(),
        };

        // E-parity: bind the evaluation context (round view + the PRICEABLE
        // books, i.e. exactly what `ctx.fresh_book` would answer) for the
        // duration of this evaluate call. The marshal owns every CString the
        // context points at and outlives the FFI call below; the unbind runs on
        // every path, panic included (see call_with_bound_ctx).
        let eval_marshal = EvalCtxMarshal::new(ctx.markets(), &ctx.fresh_book, ctx.now_ms());

        // SAFETY: valid handle and a round view whose backing lives to end of
        // scope; the returned JSON is copied and freed via the library.
        let out = self.call_with_bound_ctx(
            &eval_marshal,
            round,
            rv.markets,
            rv.market_count,
            || unsafe { evaluate(self.handle, &rv) },
        );
        let Some(v) = (unsafe { self.take_json(out) }) else {
            return Vec::new();
        };

        if let Some(serde_json::Value::Array(exits)) = v.get("exits") {
            for e in exits {
                if let Some(token) = e.get("token").and_then(|t| t.as_str()) {
                    let reason = e
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("strategy")
                        .to_string();
                    self.exit_intents.push(StrategyExitIntent {
                        token_id: token.to_string(),
                        reason,
                    });
                }
            }
        }
        if let Some(serde_json::Value::Array(br)) = v.get("breaks") {
            parse_breaks(&serde_json::Value::Array(br.clone()))
                .into_iter()
                .for_each(|b| self.breaks.push(b));
        }

        let mut candidates = Vec::new();
        let Some(entries) = v.get("entries").and_then(|e| e.as_array()) else {
            return candidates;
        };
        for e in entries {
            let (Some(token), Some(price_s)) = (
                e.get("token").and_then(|t| t.as_str()),
                e.get("price").and_then(|p| p.as_str()),
            ) else {
                continue;
            };
            let Ok(price) = Decimal::from_str(price_s) else {
                continue;
            };
            let reason = e
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("strategy entry")
                .to_string();
            // Resolve asset/condition/direction from the round's markets. A
            // token outside this round cannot be traded and is dropped.
            for m in ctx.markets() {
                if m.up_token_id == token {
                    candidates.push(TradeSignal {
                        strategy: self.name.clone(),
                        asset: m.asset.clone(),
                        direction: crate::model::SignalDirection::Up,
                        token_id: m.up_token_id.clone(),
                        condition_id: m.condition_id.clone(),
                        price,
                        reason: reason.clone(),
                    });
                    break;
                }
                if m.down_token_id == token {
                    candidates.push(TradeSignal {
                        strategy: self.name.clone(),
                        asset: m.asset.clone(),
                        direction: crate::model::SignalDirection::Down,
                        token_id: m.down_token_id.clone(),
                        condition_id: m.condition_id.clone(),
                        price,
                        reason: reason.clone(),
                    });
                    break;
                }
            }
        }
        candidates
    }

    fn take_exit_intents(&mut self) -> Vec<StrategyExitIntent> {
        std::mem::take(&mut self.exit_intents)
    }

    fn diagnostics(&self, ctx: &StrategyCtx<'_>) -> Vec<serde_json::Value> {
        let Some(f) = self.vtable.diagnostics else {
            return Vec::new();
        };
        // E-parity: the diagnostics hook sees the same bound context its
        // evaluate sees, so a diagnostic can report freshness-gated values
        // exactly like an in-tree strategy's `diagnostics(&ctx)` does.
        let round = BkRound {
            slot: ctx.round_slot(),
            time_left_sec: ctx.time_left_sec(),
            now_ms: ctx.now_ms(),
        };
        // Backing for the market rows, mirroring find_candidates.
        let mut strs: Vec<(CString, CString, CString, CString, CString)> = Vec::new();
        for m in ctx.markets() {
            let cs = |s: &str| CString::new(s).unwrap_or_default();
            strs.push((
                cs(&m.asset),
                cs(&m.condition_id),
                cs(&m.question_id),
                cs(&m.up_token_id),
                cs(&m.down_token_id),
            ));
        }
        let view: Vec<BkMarket> = strs
            .iter()
            .zip(ctx.markets().iter())
            .map(|(s, m)| BkMarket {
                asset: s.0.as_ptr(),
                condition_id: s.1.as_ptr(),
                question_id: s.2.as_ptr(),
                up_token: s.3.as_ptr(),
                down_token: s.4.as_ptr(),
                expires_at_ms: m.expires_at_ms,
                slot: m.round_slot,
                neg_risk: if m.neg_risk { 1 } else { 0 },
            })
            .collect();
        let markets_ptr = if view.is_empty() {
            std::ptr::null()
        } else {
            view.as_ptr()
        };
        let eval_marshal = EvalCtxMarshal::new(ctx.markets(), &ctx.fresh_book, ctx.now_ms());
        // SAFETY: hook returns a heap JSON array owned by the library; the
        // bound context (if any) borrows from `eval_marshal`/`bound`, which
        // outlive the call (unbind runs on every path, panic included).
        let raw =
            self.call_with_bound_ctx(&eval_marshal, round, markets_ptr, view.len(), || unsafe {
                f(self.handle)
            });
        let parsed = unsafe { self.take_json(raw) };
        match parsed {
            Some(serde_json::Value::Array(a)) => a,
            _ => Vec::new(),
        }
    }

    fn set_hot_params(&mut self, registry: Option<Arc<ParamRegistry>>) {
        // Resolve only OUR cell: a library that declared no knobs has no cell and
        // therefore receives no hot parameters at all (explicit "not evolvable").
        // `None` detaches: the library then runs on the config the kernel pushed
        // through `on_config`, which is the pre-evolution behaviour exactly.
        let cell = registry.as_ref().and_then(|r| r.handle_for(&self.name));
        // Reset the dedup marker so a fresh cell always pushes once.
        self.last_hot_json = None;
        self.params = cell;
        // An already-published value is delivered on the next evaluation, not
        // here: the push is issued from the hot path so it happens at most once
        // per actual change.
    }

    fn on_config(&mut self, trend: &TrendConfig, spread_arb: &SpreadArbConfig) {
        let Some(f) = self.vtable.on_config else {
            return;
        };
        let json = serde_json::json!({
            "trend": {
                "confirmSec": trend.confirm_sec,
                "minPrice": trend.min_price.to_string(),
                "brokenPrice": trend.broken_price.to_string(),
                "ratio": trend.ratio.to_string(),
                "windowFloorMs": trend.window_floor_ms,
            },
            "spreadArb": {
                "trendMinPrice": spread_arb.trend_min_price.to_string(),
                "trendConfirmSec": spread_arb.trend_confirm_sec,
                "trendBrokenPrice": spread_arb.trend_broken_price.to_string(),
                "trendEntryPrice": spread_arb.trend_entry_price.to_string(),
                "trendEntryFactor": spread_arb.trend_entry_factor.to_string(),
                "trendMaxEntryPrice": spread_arb.trend_max_entry_price.to_string(),
                "entryMinObi": spread_arb.entry_min_obi.to_string(),
                "entryMaxSpreadPct": spread_arb.entry_max_spread_pct.to_string(),
                "entryDipMaxPct": spread_arb.entry_dip_max_pct.to_string(),
                "entryBounceMinPct": spread_arb.entry_bounce_min_pct.to_string(),
                "entryBounceWindowSec": spread_arb.entry_bounce_window_sec,
            }
        });
        if let Ok(cs) = CString::new(json.to_string()) {
            // SAFETY: valid handle + borrowed JSON for the call; the library's
            // accept/reject return is advisory (kernel keeps driving either way).
            let _ = unsafe { f(self.handle, cs.as_ptr()) };
        }
    }
}

impl Drop for ForeignStrategy {
    fn drop(&mut self) {
        if let Some(destroy) = self.vtable.destroy {
            // SAFETY: handle came from this library's create(). Destroying it
            // here, before the `Arc<LoadedLibrary>` field is released, is what
            // guarantees the library is still mapped when its code runs.
            unsafe { destroy(self.handle) };
        }
    }
}

/// Builds independent twins of a loaded external strategy (E2-c / #28).
///
/// A twin is a SECOND instance from the same library (`create()` again), so the
/// counterfactual runs the library's own logic — the same contract the in-tree
/// twin path uses. The library stays mapped while any twin lives.
pub struct ForeignShadowFactory {
    lib: Arc<LoadedLibrary>,
    name: String,
    knobs: Vec<KnobSpec>,
}

impl ShadowFactory for ForeignShadowFactory {
    fn strategy(&self) -> String {
        self.name.clone()
    }

    fn knobs(&self) -> Vec<KnobSpec> {
        self.knobs.clone()
    }

    fn make(&self, params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
        let vtable = unsafe { std::ptr::read(self.lib.vtable()) };
        let create = vtable.create?;
        let handle = unsafe { create() };
        if handle.is_null() {
            return None;
        }
        let mut twin = ForeignStrategy {
            lib: self.lib.clone(),
            vtable,
            handle,
            name: self.name.clone(),
            version: String::new(),
            free_string: self.lib.free_string,
            gate_exemptions_fn: self.lib.gate_exemptions_fn,
            evolvable_knobs_fn: self.lib.evolvable_knobs_fn,
            knobs: self.knobs.clone(),
            bind_eval_ctx_fn: self.lib.bind_eval_ctx_fn,
            config_view_fn: self.lib.config_view_fn,
            exit_intents: Vec::new(),
            breaks: Vec::new(),
            params: None,
            last_hot_json: None,
            assets: HashMap::new(),
        };
        // Seed the twin with its counterfactual values BEFORE it is driven, so its
        // very first evaluation already uses them (next-tick semantics would give
        // the twin one tick at the live parameters).
        twin.apply_params_direct(params);
        Some(Box::new(twin))
    }
}

fn parse_breaks(v: &serde_json::Value) -> Vec<(String, Decimal)> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|b| {
                    let token = b.get("token").and_then(|t| t.as_str())?;
                    let price = b
                        .get("broken_price")
                        .and_then(|p| p.as_str())
                        .and_then(|s| Decimal::from_str(s).ok())
                        .unwrap_or(Decimal::ZERO);
                    Some((token.to_string(), price))
                })
                .collect()
        })
        .unwrap_or_default()
}
