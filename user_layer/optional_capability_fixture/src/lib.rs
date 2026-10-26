//! optional_capability_fixture — a TEST fixture, not a reference example.
//!
//! Hand-written C-ABI surface (the `parity_strategy.rs` shape) exporting ALL
//! FOUR new optional symbols of issue 642, so
//! `core/blitzkrieg_core/tests/foreign_optional_symbols_check.rs` can drive
//! them through the REAL loader (`load_foreign`) and assert behaviour
//! bit-aligned with the Lua side:
//!
//!  - `bk_strategy_on_kline` — records every bar the host pushes (CLOSED
//!    bars only reach it); the vtable's EXISTING `diagnostics` slot reports
//!    the records back as a JSON array, which is the only channel a dlopen'd
//!    fixture can speak to its test through (an rlib link would give the test
//!    binary its OWN copy of every static — two KLINES, one always empty).
//!  - `bk_strategy_entry_pair` — declares `{"key":"fixture-pair","leg":"up"}`
//!    as the pair DEFAULT; the evaluate JSON's own per-entry `pair` field
//!    (down leg) wins where it is set — both channels of the Lua mirror.
//!  - `bk_strategy_trailing_exit` — returns 1 (`Some(true)`: bank profit
//!    even when the kernel value is off), the `spread_arb` manifest shape.
//!  - `bk_strategy_atomic_batch` — returns 1 (the E-VA batch declaration).
//!
//! The evaluate output is deliberately minimal and deterministic: one entry
//! per round token priced off the last book (parity-fixture style), with a
//! per-entry `pair` on the DOWN token only. Reserved keys are absent (the
//! §2.4 seal is not this fixture's subject — the reserved_key_tests own it).
//!
//! Record shapes: a bar record is
//! `{"kind":"kline","symbol":..,"interval":..,"openTimeMs":..,"closeTimeMs":..,
//!   "open":..,"high":..,"low":..,"close":..,"volume":..,"tradeCount":..,
//!   "isClosed":true}` — decimals verbatim STRINGS, the wire's numeric rule.

use blitzkrieg_strategy_api::{
    bk_string_out, BK_ABI_VERSION, BK_MIN_ABI_VERSION, BkBookView, BkHandle, BkKlineView,
    BkRound, BkRoundView, BkStrategyVtable,
};
use core::ffi::c_char;
use std::sync::Mutex;

static NAME: &[u8] = b"optional_capability_fixture\0";
static VERSION: &[u8] = b"0.1.0\0";

/// The bars the host pushed, as readable field snapshots the test asserts on.
/// A `Mutex` (not the vtable's statics): the handle is driven single-threaded,
/// but the fixture must stay Send/Sync-clean without pretending the raw
/// pointer world is.
static KLINES: Mutex<Vec<KlineRecord>> = Mutex::new(Vec::new());

#[derive(Debug, Clone, PartialEq)]
struct KlineRecord {
    symbol: String,
    interval: String,
    open_time_ms: i64,
    close_time_ms: i64,
    open: String,
    high: String,
    low: String,
    close: String,
    volume: String,
    trade_count: u64,
    is_closed: bool,
}

unsafe extern "C" fn create() -> BkHandle {
    Box::into_raw(Box::new(0u8)) as BkHandle
}

unsafe extern "C" fn destroy(handle: BkHandle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle as *mut u8) });
    }
}

unsafe extern "C" fn on_book(handle: BkHandle, _view: *const BkBookView) {
    let _ = handle;
}

unsafe extern "C" fn on_round(_handle: BkHandle, _round: *const BkRound) {}

/// One entry per round market, priced off the token's role (the fixture has
/// no book-tracking state to price off; the kernel validates against the live
/// book anyway). The DOWN token carries its own per-entry `pair` — the
/// channel that must WIN over the load-time default.
unsafe extern "C" fn evaluate(_handle: BkHandle, view: *const BkRoundView) -> *mut c_char {
    let Some(view) = (unsafe { view.as_ref() }) else {
        return core::ptr::null_mut();
    };
    let markets = if view.markets.is_null() {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(view.markets, view.market_count) }
    };
    let cstr = |p: *const c_char| -> String {
        if p.is_null() {
            String::new()
        } else {
            unsafe { core::ffi::CStr::from_ptr(p) }
                .to_string_lossy()
                .into_owned()
        }
    };
    let mut entries = Vec::new();
    for m in markets {
        // The UP token carries NO per-entry pair: the load-time DEFAULT
        // (`bk_strategy_entry_pair`) must fill it.
        entries.push(serde_json::json!({
            "token": cstr(m.up_token),
            "price": "0.10",
            "reason": "fixture_entry",
        }));
        // The DOWN token carries its own per-entry `pair` — the channel that
        // must WIN over the load-time default.
        entries.push(serde_json::json!({
            "token": cstr(m.down_token),
            "price": "0.10",
            "reason": "fixture_entry_paired",
            "pair": { "key": "fixture-pair-per-entry", "leg": "down" },
        }));
    }
    bk_string_out(serde_json::json!({ "entries": entries }).to_string())
}

/// OPTIONAL symbol `bk_strategy_on_kline` (#642): record the borrowed view
/// into owned strings (the view dies with the call).
unsafe extern "C" fn on_kline(_handle: BkHandle, kline: *const BkKlineView) {
    let Some(k) = (unsafe { kline.as_ref() }) else {
        return;
    };
    let s = |p: *const c_char| -> String {
        if p.is_null() {
            String::new()
        } else {
            unsafe { core::ffi::CStr::from_ptr(p) }
                .to_string_lossy()
                .into_owned()
        }
    };
    let record = KlineRecord {
        symbol: s(k.symbol),
        interval: s(k.interval),
        open_time_ms: k.open_time_ms,
        close_time_ms: k.close_time_ms,
        open: s(k.open),
        high: s(k.high),
        low: s(k.low),
        close: s(k.close),
        volume: s(k.volume),
        trade_count: k.trade_count,
        is_closed: k.is_closed != 0,
    };
    if let Ok(mut v) = KLINES.lock() {
        v.push(record);
    }
}

/// OPTIONAL symbol `bk_strategy_entry_pair` (#642): the pair DEFAULT.
extern "C" fn entry_pair(_handle: BkHandle) -> *mut c_char {
    bk_string_out(r#"{"key":"fixture-pair-default","leg":"up"}"#.to_string())
}

/// OPTIONAL symbol `bk_strategy_trailing_exit` (#642): 1 = trail even when
/// the kernel value is off (the three-state `Some(true)`).
extern "C" fn trailing_exit(_handle: BkHandle) -> i32 {
    1
}

/// OPTIONAL symbol `bk_strategy_atomic_batch` (#642): 1 = the entries of one
/// evaluation cycle are ONE economic ticket.
extern "C" fn atomic_batch(_handle: BkHandle) -> i32 {
    1
}

static VTABLE: BkStrategyVtable = BkStrategyVtable {
    name: NAME.as_ptr() as *const c_char,
    version: VERSION.as_ptr() as *const c_char,
    abi_version: BK_ABI_VERSION,
    min_abi: BK_MIN_ABI_VERSION,
    create: Some(create),
    destroy: Some(destroy),
    on_book: Some(on_book),
    on_round: Some(on_round),
    evaluate: Some(evaluate),
    confirmed_tokens: None,
    take_breaks: None,
    diagnostics: Some(diagnostics),
    on_config: None,
    on_hot_params: None,
    knobs: None,
};

/// The vtable's EXISTING `diagnostics` slot, doing double duty as the
/// test's only observation channel across the dlopen boundary: one JSON
/// object per recorded bar (`kind:"kline"`), decimals verbatim STRINGS.
/// Drain is read-only (the records survive), so repeated diagnostics calls
/// answer identically — a diagnostics hook reports, it does not consume.
unsafe extern "C" fn diagnostics(_handle: BkHandle) -> *mut c_char {
    let arr = KLINES
        .lock()
        .map(|v| {
            v.iter()
                .map(|k| {
                    serde_json::json!({
                        "kind": "kline",
                        "symbol": k.symbol,
                        "interval": k.interval,
                        "openTimeMs": k.open_time_ms,
                        "closeTimeMs": k.close_time_ms,
                        "open": k.open,
                        "high": k.high,
                        "low": k.low,
                        "close": k.close,
                        "volume": k.volume,
                        "tradeCount": k.trade_count,
                        "isClosed": k.is_closed,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    bk_string_out(serde_json::Value::Array(arr).to_string())
}

#[unsafe(no_mangle)]
pub extern "C" fn bk_strategy_create() -> *const BkStrategyVtable {
    &VTABLE
}

#[unsafe(no_mangle)]
pub extern "C" fn bk_strategy_abi_version() -> u32 {
    BK_ABI_VERSION
}

#[unsafe(no_mangle)]
/// # Safety
/// `kline` must be a valid borrowed `BkKlineView` (or null), live for the
/// call — the ABI's read-only borrow rule.
pub unsafe extern "C" fn bk_strategy_on_kline(handle: BkHandle, kline: *const BkKlineView) {
    // SAFETY: the exported body forwards the borrowed view to the unsafe hook.
    unsafe { on_kline(handle, kline) }
}

#[unsafe(no_mangle)]
pub extern "C" fn bk_strategy_entry_pair(handle: BkHandle) -> *mut c_char {
    entry_pair(handle)
}

#[unsafe(no_mangle)]
pub extern "C" fn bk_strategy_trailing_exit(handle: BkHandle) -> i32 {
    trailing_exit(handle)
}

#[unsafe(no_mangle)]
pub extern "C" fn bk_strategy_atomic_batch(handle: BkHandle) -> i32 {
    atomic_batch(handle)
}
