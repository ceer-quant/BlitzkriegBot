//! External parity-harness strategy — C ABI v2 cdylib.
//!
//! This is a REAL external strategy, built in its own nested workspace exactly
//! as a third-party author would (separate Cargo.lock). It maps the v2 C structs
//! onto the neutral `parity_logic` types and calls the SAME deterministic
//! `ParityStrategy` the in-tree parity wrapper calls. The `foreign_parity`
//! integration test replays identical events through this dylib and an in-tree
//! strategy and requires signal-for-signal equality (E7 / issue #38).
//!
//! Build: `(cd user_layer/parity_strategy && cargo build --release)`.

use blitzkrieg_strategy_api::{
    bk_string_out, BkBookView, BkHandle, BkLevel, BkMarket, BkRound, BkRoundView,
    BkStrategyVtable, BK_ABI_VERSION,
};
use core::ffi::c_char;
use parity_logic::{PBook, PLevel, PMarket, ParityStrategy};
use std::ffi::CStr;

unsafe fn cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }.to_str().ok().map(|s| s.to_string())
}

unsafe fn levels(base: *const BkLevel, count: usize) -> Vec<PLevel> {
    if base.is_null() || count == 0 {
        return Vec::new();
    }
    // SAFETY: kernel guarantees `count` BkLevel entries live for the call.
    let slice = unsafe { std::slice::from_raw_parts(base, count) };
    slice
        .iter()
        .filter_map(|l| {
            Some(PLevel { price: unsafe { cstr(l.price) }?, size: unsafe { cstr(l.size) }? })
        })
        .collect()
}

unsafe fn book_from(view: &BkBookView) -> PBook {
    PBook {
        symbol: unsafe { cstr(view.symbol) }.unwrap_or_default(),
        bids: unsafe { levels(view.bids, view.bid_count) },
        asks: unsafe { levels(view.asks, view.ask_count) },
        best_bid: unsafe { cstr(view.best_bid) }.unwrap_or_default(),
        best_ask: unsafe { cstr(view.best_ask) }.unwrap_or_default(),
        mid: unsafe { cstr(view.mid) }.unwrap_or_default(),
        bid_depth: unsafe { cstr(view.bid_depth) }.unwrap_or_default(),
        ask_depth: unsafe { cstr(view.ask_depth) }.unwrap_or_default(),
        obi: unsafe { cstr(view.obi) }.unwrap_or_default(),
        spread: unsafe { cstr(view.spread) }.unwrap_or_default(),
        spread_pct: unsafe { cstr(view.spread_pct) }.unwrap_or_default(),
    }
}

unsafe fn markets_from(view: &BkRoundView) -> Vec<PMarket> {
    if view.markets.is_null() || view.market_count == 0 {
        return Vec::new();
    }
    // SAFETY: kernel guarantees `market_count` BkMarket entries for the call.
    let slice = unsafe { std::slice::from_raw_parts(view.markets, view.market_count) };
    slice
        .iter()
        .map(|m: &BkMarket| PMarket {
            up_token: unsafe { cstr(m.up_token) }.unwrap_or_default(),
            down_token: unsafe { cstr(m.down_token) }.unwrap_or_default(),
        })
        .collect()
}

// ── vtable functions ────────────────────────────────────────────────────────

unsafe extern "C" fn create() -> BkHandle {
    Box::into_raw(Box::new(ParityStrategy::new("parity"))) as BkHandle
}

unsafe extern "C" fn destroy(handle: BkHandle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle as *mut ParityStrategy) });
    }
}

unsafe extern "C" fn on_book(handle: BkHandle, view: *const BkBookView) {
    if handle.is_null() || view.is_null() {
        return;
    }
    let s = unsafe { &mut *(handle as *mut ParityStrategy) };
    let book = unsafe { book_from(&*view) };
    let token = book.symbol.clone();
    s.observe(&token, book);
}

unsafe extern "C" fn on_round(handle: BkHandle, round: *const BkRound) {
    if handle.is_null() || round.is_null() {
        return;
    }
    let s = unsafe { &mut *(handle as *mut ParityStrategy) };
    s.reset_round(unsafe { (*round).slot });
}

unsafe extern "C" fn evaluate(handle: BkHandle, view: *const BkRoundView) -> *mut c_char {
    if handle.is_null() || view.is_null() {
        return core::ptr::null_mut();
    }
    let s = unsafe { &mut *(handle as *mut ParityStrategy) };
    let markets = unsafe { markets_from(&*view) };
    let d = s.evaluate(&markets);
    let json = serde_json::json!({
        "entries": d.entries.iter().map(|e| serde_json::json!({
            "token": e.token, "price": e.price, "reason": e.reason
        })).collect::<Vec<_>>(),
        "exits": d.exits.iter().map(|e| serde_json::json!({
            "token": e.token, "reason": e.reason
        })).collect::<Vec<_>>(),
        "breaks": d.breaks.iter().map(|b| serde_json::json!({
            "token": b.token, "broken_price": b.broken_price
        })).collect::<Vec<_>>(),
    });
    bk_string_out(json.to_string())
}

unsafe extern "C" fn confirmed_tokens(handle: BkHandle) -> *mut c_char {
    let s = unsafe { &*(handle as *const ParityStrategy) };
    bk_string_out(serde_json::to_string(&s.confirmed_tokens()).unwrap_or_else(|_| "[]".into()))
}

unsafe extern "C" fn diagnostics(handle: BkHandle) -> *mut c_char {
    let s = unsafe { &*(handle as *const ParityStrategy) };
    bk_string_out(serde_json::to_string(&s.diagnostics()).unwrap_or_else(|_| "[]".into()))
}

unsafe extern "C" fn on_config(_handle: BkHandle, _json: *const c_char) -> i32 {
    0
}

unsafe extern "C" fn on_hot_params(handle: BkHandle, json: *const c_char) -> i32 {
    let Some(s) = (if handle.is_null() { None } else { Some(unsafe { &mut *(handle as *mut ParityStrategy) }) }) else {
        return 1;
    };
    match unsafe { cstr(json) } {
        Some(j) if s.apply_hot_json(&j) => 0,
        _ => 1,
    }
}

unsafe extern "C" fn knobs(handle: BkHandle) -> *mut c_char {
    let s = unsafe { &*(handle as *const ParityStrategy) };
    bk_string_out(s.knobs().to_string())
}

static NAME: &[u8] = b"parity\0";
static VERSION: &[u8] = b"0.1.0\0";

static VTABLE: BkStrategyVtable = BkStrategyVtable {
    name: NAME.as_ptr() as *const c_char,
    version: VERSION.as_ptr() as *const c_char,
    abi_version: BK_ABI_VERSION,
    min_abi: blitzkrieg_strategy_api::BK_MIN_ABI_VERSION,
    create: Some(create),
    destroy: Some(destroy),
    on_book: Some(on_book),
    on_round: Some(on_round),
    evaluate: Some(evaluate),
    confirmed_tokens: Some(confirmed_tokens),
    take_breaks: None,
    diagnostics: Some(diagnostics),
    on_config: Some(on_config),
    on_hot_params: Some(on_hot_params),
    knobs: Some(knobs),
};

#[unsafe(no_mangle)]
pub extern "C" fn bk_strategy_create() -> *const BkStrategyVtable {
    &VTABLE
}

#[unsafe(no_mangle)]
pub extern "C" fn bk_strategy_abi_version() -> u32 {
    BK_ABI_VERSION
}
