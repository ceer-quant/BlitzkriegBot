//! Example user-layer strategy: "疯狗策略" (dog strategy) — FROZEN C ABI v1.
//!
//! This is a REAL shared-library strategy implementing the frozen
//! `blitzkrieg_strategy_api` ABI. It demonstrates the user-layer contract:
//! it receives a tick and returns a signal; it CANNOT touch credentials, the
//! venue, the order manager or the network (it does not even link them).
//!
//! Build (produces `target/release/libdog_strategy.dylib` on macOS or `.so` on
//! Linux):
//! ```bash
//! cd user_layer/strategies && cargo build --release
//! ```
//! Load it into a kernel built with `--features strategy-loading` via IPC:
//! ```text
//! { "method": "strategy.load",
//!   "params": { "path": "user_layer/strategies/target/release/libdog_strategy.dylib" } }
//! ```

use blitzkrieg_strategy_api::{BkHandle, BkSide, BkSignal, BkStrategyVtable, BkTick, BK_ABI_VERSION};
use core::ffi::c_char;
use std::ffi::{CStr, CString};
use std::str::FromStr;

/// Per-instance state. `buy_price` is the dip level; the name is kept alive for
/// the handle's lifetime.
struct DogStrategy {
    buy_price: f64,
    _name: CString,
}

static NAME: &[u8] = b"dog_strategy\0";
static VERSION: &[u8] = b"0.1.0\0";
/// Size returned with a buy signal (decimal string, NUL-terminated).
static SIZE: &[u8] = b"10\0";

unsafe extern "C" fn create() -> BkHandle {
    let s = Box::new(DogStrategy { buy_price: 0.43, _name: CString::new("dog_strategy").unwrap() });
    Box::into_raw(s) as BkHandle
}

unsafe extern "C" fn destroy(handle: BkHandle) {
    if handle.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(handle as *mut DogStrategy) });
}

/// Parse a decimal string to f64 (borrowed pointer valid for the call).
unsafe fn parse_f64(p: *const c_char) -> Option<f64> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(p) }.to_str().ok()?;
    f64::from_str(s).ok()
}

/// Strategy logic: buy when the mid falls to/below the configured price.
/// Exits are handled by the kernel's position/exit policy (not here).
unsafe extern "C" fn on_tick(handle: BkHandle, tick: *const BkTick) -> BkSignal {
    if handle.is_null() || tick.is_null() {
        return BkSignal::hold();
    }
    let strat = unsafe { &*(handle as *const DogStrategy) };
    let t = unsafe { &*tick };
    let Some(mid) = (unsafe { parse_f64(t.mid) }) else {
        return BkSignal::hold();
    };
    if mid <= strat.buy_price {
        // Return a Buy using the tick's own symbol (borrowed; kernel copies it).
        BkSignal {
            side: BkSide::Buy,
            symbol: t.symbol,
            price: t.mid,
            size: SIZE.as_ptr() as *const c_char,
        }
    } else {
        BkSignal::hold()
    }
}

unsafe extern "C" fn on_round(_handle: BkHandle, _slot: i64) {
    // Stateless strategy: nothing to reset. Exercises the optional hook.
}

static VTABLE: BkStrategyVtable = BkStrategyVtable {
    name: NAME.as_ptr() as *const c_char,
    version: VERSION.as_ptr() as *const c_char,
    abi_version: BK_ABI_VERSION,
    create: Some(create),
    destroy: Some(destroy),
    on_tick: Some(on_tick),
    on_round: Some(on_round),
};

/// FROZEN ABI entry point #1: the factory.
#[unsafe(no_mangle)]
pub extern "C" fn bk_strategy_create() -> *const BkStrategyVtable {
    &VTABLE
}

/// FROZEN ABI entry point #2: ABI-version negotiation.
#[unsafe(no_mangle)]
pub extern "C" fn bk_strategy_abi_version() -> u32 {
    BK_ABI_VERSION
}
