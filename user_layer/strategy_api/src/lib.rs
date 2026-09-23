//! Blitzkrieg Strategy API — C ABI **v2** (full-featured external strategies).
//!
//! This crate is the ONLY binary contract between the kernel and a strategy
//! shipped as a shared library (`.so` / `.dylib` / `.dll`). v2 closes the
//! capability gap that v1 had between in-tree strategies (`EngineStrategy`) and
//! external ones: a v2 dylib sees exactly what an in-tree strategy sees —
//! every book callback, the full price-depth ladder, derived depth/OBI/spread
//! metrics, the round/market context — and can express entries, exits, trend
//! breaks, confirmation state, diagnostics, config and hot-parameter updates.
//!
//! The security model is unchanged and non-negotiable: the strategy only ever
//! receives borrowed, read-only market/context data and returns *intents* as
//! data. It never receives a signer, venue client, order manager, UDS socket or
//! any credential, and it cannot bypass the kernel's risk gates — the kernel
//! validates, prices, sizes, reserves, signs and submits everything.
//!
//! # Wire rules
//!
//! - Everything is `#[repr(C)]`; no `String`/`Vec`/trait object crosses the
//!   boundary, so the ABI is stable across compiler versions and callable from
//!   C / C++ / any FFI language.
//! - Prices/sizes/depth are NUL-terminated decimal UTF-8 strings (no float
//!   rounding drift).
//! - All input pointers are BORROWED and valid only for the duration of the
//!   call; the kernel copies anything it keeps.
//! - Structured/variable-length OUTPUTS are heap UTF-8 JSON strings allocated by
//!   the strategy library and freed through THAT SAME library's
//!   `bk_strategy_free_string` (resolved from the loaded lib), so allocators
//!   never cross. serde stays inside each side; the ABI only carries `char*`.
//!
//! # Required exports
//!
//! ```c
//! uint32_t bk_strategy_abi_version(void);             // must return 2
//! const bk_strategy_vtable* bk_strategy_create(void);
//! void bk_strategy_free_string(char*);                // frees JSON outputs
//! ```
//!
//! Optional (resolved by name at load; absent = "not declared"):
//!
//! ```c
//! char* bk_strategy_gate_exemptions(void* handle);    // {"timing":b,"momentum":b[, "timing_min_time_left_sec":n]}
//! char* bk_strategy_evolvable_knobs(void* handle);    // {"knobs":[{name,value,min,max}]}
//! void  bk_strategy_bind_eval_ctx(void* h, const BkEvalCtx*);  // fresh-book gate
//! char* bk_strategy_config_view(void* handle);        // config in force, JSON
//! ```

use core::ffi::{c_char, c_void};
use std::ffi::CString;

/// ABI version. Bump ONLY on a breaking change to the structs/vtable below.
pub const BK_ABI_VERSION: u32 = 2;

/// Minimum ABI a v2 kernel can drive (v2 is a clean break; no v1 shim — see
/// dev-docs/DECISIONS_PENDING.md D-15).
pub const BK_MIN_ABI_VERSION: u32 = 2;

/// Symbol name for the factory function.
pub const BK_CREATE_SYMBOL: &[u8] = b"bk_strategy_create\0";
/// Symbol name for the mandatory ABI-version function.
pub const BK_VERSION_SYMBOL: &[u8] = b"bk_strategy_abi_version\0";
/// Symbol name for the JSON-string deallocator.
pub const BK_FREE_STRING_SYMBOL: &[u8] = b"bk_strategy_free_string\0";
/// Symbol name for the OPTIONAL per-strategy gate-exemption declaration
/// (E2-b / #27): `char* bk_strategy_gate_exemptions(void* handle)` returning
/// `{"timing":bool,"momentum":bool}` — plus, since D-31, an optional
/// `"timing_min_time_left_sec":int` lower bound below which the `timing`
/// exemption is NOT honoured.
///
/// Deliberately a separate optional symbol rather than a new vtable field: the
/// kernel copies `BkStrategyVtable` BY VALUE, so appending a field would change
/// `sizeof` and make an older library an out-of-bounds read — a breaking change
/// that would force `BK_ABI_VERSION` to 3. A missing symbol / NULL / malformed
/// JSON degrades to "nothing declared", exactly like an in-tree strategy relying
/// on the trait's default, so v2 stays frozen and old libraries keep loading.
/// The already-optional JSON payload is what lets D-31 add a numeric field with
/// no ABI bump: a library that predates it simply omits the key, and the kernel
/// uses its own (stricter) default floor.
pub const BK_GATE_EXEMPTIONS_SYMBOL: &[u8] = b"bk_strategy_gate_exemptions\0";

/// Signature of the optional [`BK_GATE_EXEMPTIONS_SYMBOL`] entry point.
pub type BkGateExemptionsFn = unsafe extern "C" fn(handle: BkHandle) -> *mut c_char;

/// Symbol name for the OPTIONAL per-strategy evolvable-knob declaration
/// (E2-c / #28): `char* bk_strategy_evolvable_knobs(void* handle)` returning
/// `{"knobs":[{"name":..,"value":..,"min":..,"max":..}, ...]}` with every value a
/// decimal STRING.
///
/// Same ABI-evolution rule as [`BK_GATE_EXEMPTIONS_SYMBOL`] (ABI v2 design §3.5):
/// the kernel copies `BkStrategyVtable` BY VALUE, so appending a field would
/// change `sizeof` and break older libraries. An optional symbol costs an old
/// library nothing and degrades to "declares nothing" = **not evolvable**, which
/// is also the in-tree trait default. A strategy that declares knobs must also be
/// able to build a twin of itself (the same parameters applied to its own logic);
/// otherwise the declaration is inert and no variant is run for it.
///
/// Distinct from the vtable's `knobs` slot, which carries a human-facing JSON
/// Schema fragment and keeps its original meaning.
pub const BK_EVOLVABLE_KNOBS_SYMBOL: &[u8] = b"bk_strategy_evolvable_knobs\0";

/// Signature of the optional [`BK_EVOLVABLE_KNOBS_SYMBOL`] entry point.
pub type BkEvolvableKnobsFn = unsafe extern "C" fn(handle: BkHandle) -> *mut c_char;

/// Symbol name for the OPTIONAL evaluation-context binder:
/// `void bk_strategy_bind_eval_ctx(void* handle, const BkEvalCtx* ctx)`.
///
/// E-parity: the ONE capability an in-tree strategy has that raw ABI v2 lacked —
/// the host's fresh-book gate. An in-tree strategy prices only off
/// `StrategyCtx::fresh_book(token)`, which yields nothing for a book too stale
/// to price off; an external strategy previously received every book with no way
/// to apply the same rule. The binder closes that gap the same way the optional
/// exemption/knob symbols do — a separate symbol, the vtable untouched, an old
/// library simply never bound to a context.
///
/// The kernel calls `bind(ctx)` immediately before `evaluate` (and before
/// `diagnostics`), then `bind(null)` after the call returns. The context (and
/// every pointer inside it) is BORROWED and valid ONLY inside that window — a
/// strategy that wants the books later must copy what it needs.
pub const BK_BIND_EVAL_CTX_SYMBOL: &[u8] = b"bk_strategy_bind_eval_ctx\0";

/// Signature of the optional [`BK_BIND_EVAL_CTX_SYMBOL`] entry point. `ctx` is
/// null to end the borrow window (unbind).
pub type BkBindEvalCtxFn = unsafe extern "C" fn(handle: BkHandle, ctx: *const BkEvalCtx);

/// Symbol name for the OPTIONAL effective-config reporter:
/// `char* bk_strategy_config_view(void* handle)` returning arbitrary JSON
/// describing the config currently in force (defaults + accepted hot
/// parameters), for observability surfaces such as `engine.stats`.
///
/// This is the general form of what the in-tree `spread_arb` exposed through its
/// `spread_arb_view()` hook: what a strategy is ACTUALLY running with, reported
/// by the strategy itself. Absent symbol / null / malformed JSON = "declares
/// nothing" and the field is omitted.
pub const BK_CONFIG_VIEW_SYMBOL: &[u8] = b"bk_strategy_config_view\0";

/// Signature of the optional [`BK_CONFIG_VIEW_SYMBOL`] entry point.
pub type BkConfigViewFn = unsafe extern "C" fn(handle: BkHandle) -> *mut c_char;

/// Symbol name for the OPTIONAL hold-to-settlement declaration:
/// `int bk_strategy_settlement_holds(void* handle)` returning 1 when the
/// strategy's positions are meant to be HELD TO SETTLEMENT (expiry redemption)
/// rather than sold on the exit ladder, 0 otherwise.
///
/// Why it exists: a complete-set pair (UP + DOWN bought below $1) pays exactly
/// $1 per share-pair at settlement regardless of which side wins. Selling a leg
/// before expiry turns a riskless payoff into a directional trade, so the host
/// suppresses the policy exit ladder for such strategies and closes expired
/// positions at their redemption value instead.
///
/// Same ABI-evolution rule as [`BK_GATE_EXEMPTIONS_SYMBOL`]: a separate symbol,
/// the vtable untouched, an absent symbol reads as 0 ("no declaration") so
/// older libraries keep loading unchanged and every existing strategy keeps
/// its exits.
pub const BK_SETTLEMENT_HOLDS_SYMBOL: &[u8] = b"bk_strategy_settlement_holds\0";

/// Signature of the optional [`BK_SETTLEMENT_HOLDS_SYMBOL`] entry point.
pub type BkSettlementHoldsFn = unsafe extern "C" fn(handle: BkHandle) -> i32;

/// One price/size level of the order book. Both are decimal strings.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BkLevel {
    pub price: *const c_char,
    pub size: *const c_char,
}

/// Full order-book view for one token. The bid/ask ladders are length-prefixed
/// arrays (`bids` is sorted best→worst descending by price; `asks` ascending).
/// All derived metrics are precomputed by the HOST using the same
/// `OrderbookSnapshot::from_levels` path the kernel and in-tree strategies use,
/// so an external strategy observes identical numbers.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BkBookView {
    /// Token/contract identifier (borrowed).
    pub symbol: *const c_char,
    /// Underlying asset, e.g. "BTC" (borrowed).
    pub asset: *const c_char,
    pub bids: *const BkLevel,
    pub bid_count: usize,
    pub asks: *const BkLevel,
    pub ask_count: usize,
    pub best_bid: *const c_char,
    pub best_ask: *const c_char,
    pub mid: *const c_char,
    /// Sum of sizes across the ladder (decimal string).
    pub bid_depth: *const c_char,
    pub ask_depth: *const c_char,
    /// Order-book imbalance (bid_depth-ask_depth)/(bid_depth+ask_depth).
    pub obi: *const c_char,
    pub spread: *const c_char,
    pub spread_pct: *const c_char,
    pub timestamp_ms: i64,
}

/// Round timing for the current evaluation.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BkRound {
    pub slot: i64,
    pub time_left_sec: i64,
    pub now_ms: i64,
}

/// One binary market in the current round (its two outcome tokens).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BkMarket {
    pub asset: *const c_char,
    pub condition_id: *const c_char,
    pub question_id: *const c_char,
    pub up_token: *const c_char,
    pub down_token: *const c_char,
    pub expires_at_ms: i64,
    pub slot: i64,
    pub neg_risk: u8,
}

/// Everything a strategy gets for one evaluation: round timing plus the round's
/// markets. Token→asset/condition/direction is resolved by the host against
/// these; an intent on a token not present here is discarded.
#[repr(C)]
pub struct BkRoundView {
    pub round: BkRound,
    pub markets: *const BkMarket,
    pub market_count: usize,
}

/// One round token's book as bound through [`BkEvalCtx`], with the host's
/// freshness verdict. `fresh != 0` means the snapshot passed the SAME
/// staleness rule the kernel applies to in-tree strategies
/// (non-empty book, younger than the configured max staleness): pricing off a
/// non-fresh book is a stale-price bug the host gate exists to prevent.
#[repr(C)]
pub struct BkTokenBook {
    /// Token the view belongs to (borrowed, NUL-terminated).
    pub token: *const c_char,
    pub book: BkBookView,
    /// 1 = fresh enough to price off. Never anything else in this version.
    pub fresh: u8,
}

/// The OPTIONAL evaluation context an in-tree strategy receives as
/// `StrategyCtx`, bound through [`BK_BIND_EVAL_CTX_SYMBOL`]. Borrowed: valid
/// only between `bind(ctx)` and the host's `bind(null)`.
///
/// `view` is the same round view `evaluate` already receives; `books` carries
/// exactly the PRICEABLE books of the round (`fresh = 1`), the data form of
/// `StrategyCtx::fresh_book` returning `Some`. A token with NO row here is
/// "not priceable right now" — a stale or missing book — and pricing an entry
/// off it is a bug the in-tree gate exists to prevent; a strategy must not.
#[repr(C)]
pub struct BkEvalCtx {
    pub view: BkRoundView,
    pub books: *const BkTokenBook,
    pub book_count: usize,
}

/// Opaque per-instance state owned by the strategy.
pub type BkHandle = *mut c_void;

/// The v2 vtable. A strategy returns a pointer to a `'static` instance from
/// `bk_strategy_create`. Field order/types MUST NOT change without bumping
/// `BK_ABI_VERSION`.
///
/// Required: `create`, `destroy`, `on_book`, `on_round`, `evaluate`. Optional
/// hooks may be NULL — NULL means "this strategy does not use this hook", the
/// same as an in-tree strategy relying on the trait's default impl. New
/// capabilities are added as separate optional SYMBOLS (e.g.
/// [`BK_GATE_EXEMPTIONS_SYMBOL`]) precisely so this struct never grows.
///
/// Functions returning JSON (`evaluate`, `confirmed_tokens`, `take_breaks`,
/// `diagnostics`, `knobs`) return a heap string allocated by THIS library (use
/// [`bk_string_out`]); the kernel frees it via `bk_strategy_free_string`. A NULL
/// return means "empty" for that hook.
#[repr(C)]
pub struct BkStrategyVtable {
    /// Human-readable name (NUL-terminated, static lifetime).
    pub name: *const c_char,
    /// Semantic version string (NUL-terminated, static lifetime).
    pub version: *const c_char,
    /// ABI version this vtable was built against (== `BK_ABI_VERSION`).
    pub abi_version: u32,
    /// Oldest ABI this build can speak (== `BK_MIN_ABI_VERSION`).
    pub min_abi: u32,

    /// Allocate/initialise an instance. Returns null on failure. REQUIRED.
    pub create: Option<unsafe extern "C" fn() -> BkHandle>,
    /// Free an instance created by `create`. REQUIRED.
    pub destroy: Option<unsafe extern "C" fn(handle: BkHandle)>,

    /// Called on EVERY book/top-of-book update, with the full depth view.
    pub on_book: Option<unsafe extern "C" fn(handle: BkHandle, view: *const BkBookView)>,
    /// Called when a new round starts.
    pub on_round: Option<unsafe extern "C" fn(handle: BkHandle, round: *const BkRound)>,

    /// Produce this cycle's intents as JSON:
    /// `{"entries":[{"token","price","reason"}],
    ///   "exits":[{"token","reason"}],
    ///   "breaks":[{"token","broken_price"}]}`.
    /// Entries carry NO size (the host sizes); exits carry NO price (the host
    /// prices off the live book). REQUIRED.
    pub evaluate:
        Option<unsafe extern "C" fn(handle: BkHandle, view: *const BkRoundView) -> *mut c_char>,

    /// Tokens the strategy currently considers entry-eligible, as a JSON array
    /// of strings. Optional.
    pub confirmed_tokens: Option<unsafe extern "C" fn(handle: BkHandle) -> *mut c_char>,
    /// Trend breaks since the last call (each drained once), JSON array of
    /// `{"token","broken_price"}`. The host cancels their resting bids.
    /// Optional.
    pub take_breaks: Option<unsafe extern "C" fn(handle: BkHandle) -> *mut c_char>,
    /// Observability payload, a JSON array of arbitrary JSON values. Optional.
    pub diagnostics: Option<unsafe extern "C" fn(handle: BkHandle) -> *mut c_char>,

    /// Host config changed. `json` is borrowed for the call. Return 0 to accept,
    /// non-zero to reject (kernel keeps previous config). Optional.
    pub on_config: Option<unsafe extern "C" fn(handle: BkHandle, json: *const c_char) -> i32>,
    /// Shadow-Evolution hot parameters changed (camelCase JSON, next-tick
    /// semantics). `json` borrowed for the call. Return 0/non-zero. Optional.
    pub on_hot_params: Option<unsafe extern "C" fn(handle: BkHandle, json: *const c_char) -> i32>,
    /// The strategy's self-declared evolvable knobs as a JSON Schema fragment
    /// (drives per-strategy shadow evolution, E2-c). Optional.
    pub knobs: Option<unsafe extern "C" fn(handle: BkHandle) -> *mut c_char>,
}

// SAFETY: the vtable is required to be static data shared by the library; the
// handle it describes is driven only by the kernel's single strategy loop.
unsafe impl Send for BkStrategyVtable {}
unsafe impl Sync for BkStrategyVtable {}

/// Hand an owned Rust `String` across the ABI as a NUL-terminated heap C string.
/// Pair with the kernel calling THIS LIBRARY's `bk_strategy_free_string`.
///
/// Returns null for an input containing an interior NUL (treated as no output).
pub fn bk_string_out(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(cs) => cs.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// Free a string previously produced by [`bk_string_out`] in THIS library.
///
/// # Safety
/// `p` must be a pointer returned by `CString::into_raw` inside this same
/// shared object (or null). The kernel resolves and invokes the deallocator
/// from the loaded library, so the matching allocator always frees it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bk_strategy_free_string(p: *mut c_char) {
    if p.is_null() {
        return;
    }
    // SAFETY: caller guarantees this pointer came from `CString::into_raw` in
    // this library and is freed exactly once.
    drop(unsafe { CString::from_raw(p) });
}

// ── SafeStrategy ergonomic layer (E9-a / #60) ───────────────────────────────
// `use blitzkrieg_strategy_api::{SafeStrategy, export_strategy!}` is now the
// whole template contract for a third-party strategy crate; the raw ABI below
// stays available for full-control authors (user_layer/parity_strategy) and for
// the kernel.
pub mod safe;
pub use safe::{
    BookUpdate, Break, Entry, Exit, FreshBook, Intents, Knob, MarketInfo, ParamBag, RoundContext,
    RoundInfo, SafeStrategy, dec,
};

/// Shell helpers the generated `__bk_export` module imports via `$crate::shell`.
#[doc(hidden)]
pub mod shell {
    pub use crate::safe::{cstr, field, json_out, parse_book, parse_eval_ctx, parse_params};
}
