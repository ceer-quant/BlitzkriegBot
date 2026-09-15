//! Example external strategy — "疯狗策略" (dog strategy), C ABI **v2**.
//!
//! A real shared-library strategy implementing the full-featured
//! `blitzkrieg_strategy_api` ABI: it receives every book with the full depth
//! ladder and derived metrics, round/market context, config and hot parameters;
//! it returns entries, exit intents, confirmation and diagnostics. It cannot
//! touch credentials, the venue, the order manager or the network — it links
//! none of them, and every intent still passes the kernel's gates.
//!
//! Rule (toy, deterministic):
//!  - on a FRESH deep book (bid depth >= `min_depth`) with mid <= `buy_below`,
//!    buy at the best ask and mark the token in-position;
//!  - while in position, request an EXIT once the best bid recovers to
//!    `take_profit` (a strategy close intent — the kernel still prices/submits);
//!  - `buy_below` is hot-swappable via the kernel MutableParams field
//!    `trendMaxEntryPrice` and the initial config.
//!
//! Build: `(cd user_layer/strategies && cargo build --release)` →
//! `target/release/libdog_strategy.{dylib,so,dll}`.

use blitzkrieg_strategy_api::{
    bk_string_out, BkBookView, BkHandle, BkLevel, BkRound, BkRoundView, BkStrategyVtable,
    BK_ABI_VERSION, BK_MIN_ABI_VERSION,
};
use core::ffi::c_char;
use std::ffi::CStr;
use std::collections::{HashMap, HashSet};

#[derive(Clone)]
struct BookState {
    mid: f64,
    best_bid: f64,
    best_ask: f64,
    bid_depth: f64,
    bid_levels: usize,
    ask_levels: usize,
}

struct DogStrategy {
    /// Dip entry ceiling (mid <= this to buy).
    buy_below: f64,
    /// Exit when best bid recovers to this.
    take_profit: f64,
    /// Minimum total bid depth required to trust the dip.
    min_depth: f64,
    books: HashMap<String, BookState>,
    /// Tokens currently in-position (entry fired, exit pending).
    holding: HashSet<String>,
    /// Pending exit intents drained by the kernel each evaluate.
    exits: Vec<(String, String)>,
}

impl DogStrategy {
    fn new() -> Self {
        Self {
            buy_below: 0.43,
            take_profit: 0.60,
            min_depth: 50.0,
            books: HashMap::new(),
            holding: HashSet::new(),
            exits: Vec::new(),
        }
    }
}

unsafe fn cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }.to_str().ok().map(|s| s.to_string())
}
unsafe fn num(p: *const c_char) -> Option<f64> {
    unsafe { cstr(p) }.and_then(|s| s.parse().ok())
}
unsafe fn count_levels(base: *const BkLevel, n: usize) -> usize {
    if base.is_null() || n == 0 {
        return 0;
    }
    unsafe { std::slice::from_raw_parts(base, n) }.len()
}

// ── vtable functions ────────────────────────────────────────────────────────

unsafe extern "C" fn create() -> BkHandle {
    Box::into_raw(Box::new(DogStrategy::new())) as BkHandle
}

unsafe extern "C" fn destroy(handle: BkHandle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle as *mut DogStrategy) });
    }
}

unsafe extern "C" fn on_book(handle: BkHandle, view: *const BkBookView) {
    if handle.is_null() || view.is_null() {
        return;
    }
    let s = unsafe { &mut *(handle as *mut DogStrategy) };
    let v = unsafe { &*view };
    let (Some(symbol), Some(mid), Some(best_bid), Some(best_ask), Some(bid_depth)) = (
        unsafe { cstr(v.symbol) },
        unsafe { num(v.mid) },
        unsafe { num(v.best_bid) },
        unsafe { num(v.best_ask) },
        unsafe { num(v.bid_depth) },
    ) else {
        return;
    };
    s.books.insert(
        symbol,
        BookState {
            mid,
            best_bid,
            best_ask,
            bid_depth,
            bid_levels: unsafe { count_levels(v.bids, v.bid_count) },
            ask_levels: unsafe { count_levels(v.asks, v.ask_count) },
        },
    );
}

unsafe extern "C" fn on_round(_handle: BkHandle, _round: *const BkRound) {
    let s = unsafe { &mut *(_handle as *mut DogStrategy) };
    // A new round clears transient in-position state (positions are still owned
    // by the kernel; this is just the strategy's local bookkeeping).
    s.holding.clear();
    s.exits.clear();
}

unsafe extern "C" fn evaluate(handle: BkHandle, view: *const BkRoundView) -> *mut c_char {
    if handle.is_null() || view.is_null() {
        return core::ptr::null_mut();
    }
    let s = unsafe { &mut *(handle as *mut DogStrategy) };
    let rv = unsafe { &*view };

    let mut entries = Vec::new();
    if !rv.markets.is_null() && rv.market_count > 0 {
        let markets = unsafe { std::slice::from_raw_parts(rv.markets, rv.market_count) };
        for m in markets {
            let tokens = [unsafe { cstr(m.up_token) }, unsafe { cstr(m.down_token) }];
            for token in tokens.into_iter().flatten() {
                let Some(b) = s.books.get(&token) else { continue };
                if s.holding.contains(&token) {
                    // Manage the open: exit on recovery.
                    if b.best_bid >= s.take_profit {
                        s.exits.push((token.clone(), "dog_tp".into()));
                        s.holding.remove(&token);
                    }
                } else if b.mid <= s.buy_below
                    && b.bid_depth >= s.min_depth
                    && b.bid_levels >= 1
                    && b.ask_levels >= 1
                {
                    entries.push(serde_json::json!({
                        "token": token,
                        "price": format!("{}", b.best_ask),
                        "reason": "dog_dip",
                    }));
                    s.holding.insert(token);
                }
            }
        }
    }

    let exits: Vec<_> = s
        .exits
        .drain(..)
        .map(|(token, reason)| serde_json::json!({ "token": token, "reason": reason }))
        .collect();

    bk_string_out(
        serde_json::json!({ "entries": entries, "exits": exits, "breaks": [] }).to_string(),
    )
}

unsafe extern "C" fn confirmed_tokens(handle: BkHandle) -> *mut c_char {
    if handle.is_null() {
        return bk_string_out("[]".into());
    }
    let s = unsafe { &*(handle as *const DogStrategy) };
    let mut v: Vec<String> = s
        .books
        .iter()
        .filter(|(_, b)| b.bid_levels >= 2 && b.ask_levels >= 2)
        .map(|(t, _)| t.clone())
        .collect();
    v.sort();
    bk_string_out(serde_json::to_string(&v).unwrap_or_else(|_| "[]".into()))
}

unsafe extern "C" fn diagnostics(handle: BkHandle) -> *mut c_char {
    if handle.is_null() {
        return bk_string_out("[]".into());
    }
    let s = unsafe { &*(handle as *const DogStrategy) };
    let mut tokens: Vec<&String> = s.books.keys().collect();
    tokens.sort();
    let out: Vec<_> = tokens
        .iter()
        .map(|t| {
            let b = &s.books[*t];
            serde_json::json!({
                "symbol": t,
                "mid": b.mid,
                "bestBid": b.best_bid,
                "bestAsk": b.best_ask,
                "bidDepth": b.bid_depth,
                "holding": s.holding.contains(*t),
            })
        })
        .collect();
    bk_string_out(serde_json::to_string(&out).unwrap_or_else(|_| "[]".into()))
}

unsafe extern "C" fn on_config(handle: BkHandle, json: *const c_char) -> i32 {
    let Some(j) = (unsafe { cstr(json) }) else { return 1 };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&j) else { return 1 };
    let s = unsafe { &mut *(handle as *mut DogStrategy) };
    if let Some(x) = v.pointer("/spreadArb/trendMaxEntryPrice").and_then(|x| x.as_str()) {
        if let Ok(f) = x.parse::<f64>() {
            s.buy_below = f;
        }
    }
    0
}

unsafe extern "C" fn on_hot_params(handle: BkHandle, json: *const c_char) -> i32 {
    let Some(j) = (unsafe { cstr(json) }) else { return 1 };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&j) else { return 1 };
    let s = unsafe { &mut *(handle as *mut DogStrategy) };
    if let Some(x) = v.get("trendMaxEntryPrice").and_then(|x| x.as_str()) {
        if let Ok(f) = x.parse::<f64>() {
            s.buy_below = f;
            return 0;
        }
    }
    1
}

unsafe extern "C" fn knobs(_handle: BkHandle) -> *mut c_char {
    bk_string_out(
        serde_json::json!({
            "type": "object",
            "properties": {
                "trendMaxEntryPrice": { "type": "string", "description": "dip entry ceiling" }
            }
        })
        .to_string(),
    )
}

/// E2-b (#27): the dog strategy hunts dips, which is mean-reversion — it wants
/// the whole round, so it declares the round-timing WINDOW gate unnecessary for
/// its entries while still asking for the spot momentum alignment filter (it
/// only fades a dip it believes is noise, not a real move against it).
///
/// This is an OPTIONAL symbol: a v2 library without it stays fully gated, which
/// is why the declaration needs no ABI bump. The kernel logs every honoured
/// exemption («本单因策略 dog_strategy 豁免门禁 timing») — this is not a bypass of
/// any safety boundary.
unsafe extern "C" fn gate_exemptions(_handle: BkHandle) -> *mut c_char {
    bk_string_out(serde_json::json!({ "timing": true, "momentum": false }).to_string())
}

static NAME: &[u8] = b"dog_strategy\0";
static VERSION: &[u8] = b"0.2.0\0";
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

#[unsafe(no_mangle)]
pub extern "C" fn bk_strategy_gate_exemptions(handle: BkHandle) -> *mut c_char {
    unsafe { gate_exemptions(handle) }
}
