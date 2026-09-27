//! e24_modes_fixture — a TEST fixture, not a reference example.
//!
//! Hand-written C-ABI surface (the `parity_strategy.rs` shape) whose OPTIONAL
//! `bk_strategy_declare_modes` symbol returns whatever payload the
//! `BK_E24_DECLARATION_FILE` env var points at, read fresh at every call. That
//! lets `scripts/strategy-declaration-check.mjs` rewrite the file between
//! loads and drive ALL of DEV_V0_3 §16.4's declaration cases (3 legal / 5
//! illegal) through the REAL loader with one dylib. The `export_strategy!`
//! macro cannot serve here: it can only ever emit a VALID payload, and the
//! whole point of this fixture is to put invalid ones in front of the loader.
//!
//! Missing env or unreadable file => NULL return = "undeclared" (§7.4).

use blitzkrieg_strategy_api::{
    bk_string_out, BK_ABI_VERSION, BK_MIN_ABI_VERSION, BkBookView, BkHandle, BkRound,
    BkRoundView, BkStrategyVtable,
};
use core::ffi::c_char;

static NAME: &[u8] = b"e24_modes_fixture\0";
static VERSION: &[u8] = b"0.1.0\0";

unsafe extern "C" fn create() -> BkHandle {
    Box::into_raw(Box::new(0u8)) as BkHandle
}

unsafe extern "C" fn destroy(handle: BkHandle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle as *mut u8) });
    }
}

unsafe extern "C" fn on_book(_handle: BkHandle, _view: *const BkBookView) {}

unsafe extern "C" fn on_round(_handle: BkHandle, _round: *const BkRound) {}

unsafe extern "C" fn evaluate(_handle: BkHandle, _view: *const BkRoundView) -> *mut c_char {
    core::ptr::null_mut()
}

/// The payload is read from `BK_E24_DECLARATION_FILE` at CALL time (the file
/// content IS the wire payload, byte for byte). Unset env / unreadable file /
/// interior NUL all degrade to NULL = "undeclared", exactly like a library
/// that exports no symbol at all.
unsafe extern "C" fn declare_modes(_handle: BkHandle) -> *mut c_char {
    let Ok(path) = std::env::var("BK_E24_DECLARATION_FILE") else {
        return core::ptr::null_mut();
    };
    match std::fs::read_to_string(path) {
        Ok(text) => bk_string_out(text),
        Err(_) => core::ptr::null_mut(),
    }
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
    diagnostics: None,
    on_config: None,
    on_hot_params: None,
    knobs: None,
};

#[unsafe(no_mangle)]
pub extern "C" fn bk_strategy_create() -> *const BkStrategyVtable {
    &VTABLE
}

#[unsafe(no_mangle)]
pub extern "C" fn bk_strategy_abi_version() -> u32 {
    BK_ABI_VERSION
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn bk_strategy_declare_modes(handle: BkHandle) -> *mut c_char {
    unsafe { declare_modes(handle) }
}
