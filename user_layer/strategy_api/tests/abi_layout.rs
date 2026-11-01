//! PR0 (#840) — ABI layout/symbol golden for BlitzkriegStrategy API 1.0.
//!
//! Freezes, as executable assertions:
//! - `BK_ABI_VERSION` / `BK_MIN_ABI_VERSION` (= 2, the 0.3 rename did NOT bump
//!   the wire — old cdylibs keep loading),
//! - the 13 exported symbol NAME constants (byte-exact, NUL-terminated),
//! - the 10 optional-symbol `Fn` alias signatures (compile-time: assigning a
//!   concretely-typed function to the alias fails the build on any drift),
//! - `LADDER_LEVELS`,
//! - the 9 `#[repr(C)]` structs' `size_of`/`align_of` and every field's
//!   `offset_of!`.
//!
//! `#[repr(C)]` guarantees field order == declaration order and forbids
//! reordering, so on any 64-bit target (pointers and `usize` = 8, `u32` = 4,
//! `u64`/`i64` = 8) these offsets are platform-stable. If this test goes red,
//! `BK_ABI_VERSION` must be bumped — an old library would read garbage.

use std::ffi::{c_char, c_void};
use std::mem::{align_of, offset_of, size_of};

use blitzkrieg_strategy_api::{
    BK_ABI_VERSION, BK_ATOMIC_BATCH_SYMBOL, BK_BIND_EVAL_CTX_SYMBOL, BK_CONFIG_VIEW_SYMBOL,
    BK_CREATE_SYMBOL, BK_DECLARE_MODES_SYMBOL, BK_ENTRY_PAIR_SYMBOL, BK_EVOLVABLE_KNOBS_SYMBOL,
    BK_FREE_STRING_SYMBOL, BK_GATE_EXEMPTIONS_SYMBOL, BK_MIN_ABI_VERSION, BK_ON_KLINE_SYMBOL,
    BK_SETTLEMENT_HOLDS_SYMBOL, BK_TRAILING_EXIT_SYMBOL, BK_VERSION_SYMBOL, BkAtomicBatchFn,
    BkBindEvalCtxFn, BkBookView, BkConfigViewFn, BkDeclareModesFn, BkEntryPairFn, BkEvalCtx,
    BkEvolvableKnobsFn, BkGateExemptionsFn, BkHandle, BkKlineView, BkLevel, BkMarket, BkOnKlineFn,
    BkRound, BkRoundView, BkSettlementHoldsFn, BkStrategyVtable, BkTokenBook, BkTrailingExitFn,
    LADDER_LEVELS,
};

/// The whole suite only means something on the 64-bit targets the kernel ships
/// for; a 32-bit build changes every pointer offset and MUST re-derive, not
/// silently pass.
const _: () = assert!(size_of::<usize>() == 8);
const _: () = assert!(size_of::<u64>() == 8);
const _: () = assert!(size_of::<u32>() == 4);

// ── version constants ────────────────────────────────────────────────────────

#[test]
fn abi_version_stays_at_2() {
    assert_eq!(
        BK_ABI_VERSION, 2,
        "the wire version is frozen by 0.3's rename"
    );
    assert_eq!(BK_MIN_ABI_VERSION, 2);
}

// ── symbol name constants (byte-exact) ───────────────────────────────────────

#[test]
fn all_thirteen_symbol_names_are_byte_frozen() {
    assert_eq!(BK_CREATE_SYMBOL, b"bk_strategy_create\0");
    assert_eq!(BK_VERSION_SYMBOL, b"bk_strategy_abi_version\0");
    assert_eq!(BK_FREE_STRING_SYMBOL, b"bk_strategy_free_string\0");
    assert_eq!(BK_GATE_EXEMPTIONS_SYMBOL, b"bk_strategy_gate_exemptions\0");
    assert_eq!(BK_EVOLVABLE_KNOBS_SYMBOL, b"bk_strategy_evolvable_knobs\0");
    assert_eq!(BK_BIND_EVAL_CTX_SYMBOL, b"bk_strategy_bind_eval_ctx\0");
    assert_eq!(BK_CONFIG_VIEW_SYMBOL, b"bk_strategy_config_view\0");
    assert_eq!(
        BK_SETTLEMENT_HOLDS_SYMBOL,
        b"bk_strategy_settlement_holds\0"
    );
    assert_eq!(BK_DECLARE_MODES_SYMBOL, b"bk_strategy_declare_modes\0");
    assert_eq!(BK_ON_KLINE_SYMBOL, b"bk_strategy_on_kline\0");
    assert_eq!(BK_ENTRY_PAIR_SYMBOL, b"bk_strategy_entry_pair\0");
    assert_eq!(BK_TRAILING_EXIT_SYMBOL, b"bk_strategy_trailing_exit\0");
    assert_eq!(BK_ATOMIC_BATCH_SYMBOL, b"bk_strategy_atomic_batch\0");
}

#[test]
fn every_symbol_name_is_nul_terminated_and_unique() {
    let all = [
        BK_CREATE_SYMBOL,
        BK_VERSION_SYMBOL,
        BK_FREE_STRING_SYMBOL,
        BK_GATE_EXEMPTIONS_SYMBOL,
        BK_EVOLVABLE_KNOBS_SYMBOL,
        BK_BIND_EVAL_CTX_SYMBOL,
        BK_CONFIG_VIEW_SYMBOL,
        BK_SETTLEMENT_HOLDS_SYMBOL,
        BK_DECLARE_MODES_SYMBOL,
        BK_ON_KLINE_SYMBOL,
        BK_ENTRY_PAIR_SYMBOL,
        BK_TRAILING_EXIT_SYMBOL,
        BK_ATOMIC_BATCH_SYMBOL,
    ];
    assert_eq!(
        all.len(),
        13,
        "13 symbols are the ABI's full symbol surface"
    );
    for s in all {
        assert!(
            !s.is_empty() && s[s.len() - 1] == 0,
            "{:?} must end in NUL",
            s
        );
    }
    let mut uniq: Vec<&[u8]> = all.to_vec();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), 13, "symbol names must not collide");
}

// ── the 10 optional Fn aliases (compile-time signature pins) ─────────────────

unsafe extern "C" fn probe_str(_: BkHandle) -> *mut c_char {
    std::ptr::null_mut()
}
unsafe extern "C" fn probe_int(_: BkHandle) -> i32 {
    0
}
unsafe extern "C" fn probe_ctx(_: BkHandle, _: *const BkEvalCtx) {}
unsafe extern "C" fn probe_kline(_: BkHandle, _: *const BkKlineView) {}

#[test]
fn fn_alias_signatures_are_pinned_by_assignment() {
    // 5 × handle → heap JSON string.
    let _f: BkGateExemptionsFn = probe_str;
    let _f: BkEvolvableKnobsFn = probe_str;
    let _f: BkConfigViewFn = probe_str;
    let _f: BkDeclareModesFn = probe_str;
    let _f: BkEntryPairFn = probe_str;
    // 3 × handle → tri-state int.
    let _f: BkSettlementHoldsFn = probe_int;
    let _f: BkTrailingExitFn = probe_int;
    let _f: BkAtomicBatchFn = probe_int;
    // 2 × void-returning borrowed hooks.
    let _f: BkBindEvalCtxFn = probe_ctx;
    let _f: BkOnKlineFn = probe_kline;
}

// ── ladder cap ───────────────────────────────────────────────────────────────

#[test]
fn ladder_cap_is_ten() {
    assert_eq!(LADDER_LEVELS, 10);
}

// ── repr(C) struct layouts: size / align / every offset ──────────────────────

#[test]
fn bk_level_layout() {
    assert_eq!(size_of::<BkLevel>(), 16);
    assert_eq!(align_of::<BkLevel>(), 8);
    assert_eq!(offset_of!(BkLevel, price), 0);
    assert_eq!(offset_of!(BkLevel, size), 8);
}

#[test]
fn bk_round_layout() {
    assert_eq!(size_of::<BkRound>(), 24);
    assert_eq!(align_of::<BkRound>(), 8);
    assert_eq!(offset_of!(BkRound, slot), 0);
    assert_eq!(offset_of!(BkRound, time_left_sec), 8);
    assert_eq!(offset_of!(BkRound, now_ms), 16);
}

#[test]
fn bk_market_layout() {
    assert_eq!(size_of::<BkMarket>(), 72);
    assert_eq!(align_of::<BkMarket>(), 8);
    assert_eq!(offset_of!(BkMarket, asset), 0);
    assert_eq!(offset_of!(BkMarket, condition_id), 8);
    assert_eq!(offset_of!(BkMarket, question_id), 16);
    assert_eq!(offset_of!(BkMarket, up_token), 24);
    assert_eq!(offset_of!(BkMarket, down_token), 32);
    assert_eq!(offset_of!(BkMarket, expires_at_ms), 40);
    assert_eq!(offset_of!(BkMarket, slot), 48);
    assert_eq!(offset_of!(BkMarket, neg_risk), 56);
    assert_eq!(offset_of!(BkMarket, venue), 64);
}

#[test]
fn bk_book_view_layout() {
    assert_eq!(size_of::<BkBookView>(), 120);
    assert_eq!(align_of::<BkBookView>(), 8);
    assert_eq!(offset_of!(BkBookView, symbol), 0);
    assert_eq!(offset_of!(BkBookView, asset), 8);
    assert_eq!(offset_of!(BkBookView, bids), 16);
    assert_eq!(offset_of!(BkBookView, bid_count), 24);
    assert_eq!(offset_of!(BkBookView, asks), 32);
    assert_eq!(offset_of!(BkBookView, ask_count), 40);
    assert_eq!(offset_of!(BkBookView, best_bid), 48);
    assert_eq!(offset_of!(BkBookView, best_ask), 56);
    assert_eq!(offset_of!(BkBookView, mid), 64);
    assert_eq!(offset_of!(BkBookView, bid_depth), 72);
    assert_eq!(offset_of!(BkBookView, ask_depth), 80);
    assert_eq!(offset_of!(BkBookView, obi), 88);
    assert_eq!(offset_of!(BkBookView, spread), 96);
    assert_eq!(offset_of!(BkBookView, spread_pct), 104);
    assert_eq!(offset_of!(BkBookView, timestamp_ms), 112);
}

#[test]
fn bk_kline_view_layout() {
    assert_eq!(size_of::<BkKlineView>(), 88);
    assert_eq!(align_of::<BkKlineView>(), 8);
    assert_eq!(offset_of!(BkKlineView, symbol), 0);
    assert_eq!(offset_of!(BkKlineView, interval), 8);
    assert_eq!(offset_of!(BkKlineView, open_time_ms), 16);
    assert_eq!(offset_of!(BkKlineView, close_time_ms), 24);
    assert_eq!(offset_of!(BkKlineView, open), 32);
    assert_eq!(offset_of!(BkKlineView, high), 40);
    assert_eq!(offset_of!(BkKlineView, low), 48);
    assert_eq!(offset_of!(BkKlineView, close), 56);
    assert_eq!(offset_of!(BkKlineView, volume), 64);
    assert_eq!(offset_of!(BkKlineView, trade_count), 72);
    assert_eq!(offset_of!(BkKlineView, is_closed), 80);
}

#[test]
fn bk_round_view_layout() {
    assert_eq!(size_of::<BkRoundView>(), 40);
    assert_eq!(align_of::<BkRoundView>(), 8);
    assert_eq!(offset_of!(BkRoundView, round), 0);
    assert_eq!(offset_of!(BkRoundView, markets), 24);
    assert_eq!(offset_of!(BkRoundView, market_count), 32);
}

#[test]
fn bk_token_book_layout() {
    assert_eq!(size_of::<BkTokenBook>(), 136);
    assert_eq!(align_of::<BkTokenBook>(), 8);
    assert_eq!(offset_of!(BkTokenBook, token), 0);
    assert_eq!(offset_of!(BkTokenBook, book), 8);
    assert_eq!(offset_of!(BkTokenBook, fresh), 128);
}

#[test]
fn bk_eval_ctx_layout() {
    assert_eq!(size_of::<BkEvalCtx>(), 56);
    assert_eq!(align_of::<BkEvalCtx>(), 8);
    assert_eq!(offset_of!(BkEvalCtx, view), 0);
    assert_eq!(offset_of!(BkEvalCtx, books), 40);
    assert_eq!(offset_of!(BkEvalCtx, book_count), 48);
}

#[test]
fn bk_handle_is_a_plain_pointer() {
    assert_eq!(size_of::<BkHandle>(), 8);
    assert_eq!(align_of::<BkHandle>(), 8);
    assert_eq!(
        size_of::<*const c_void>(),
        8,
        "BkHandle is *mut c_void; pin it so it never becomes a fat pointer"
    );
}

#[test]
fn bk_strategy_vtable_layout() {
    // 2 ptrs + 2 u32 + 11 optional fn slots.
    assert_eq!(size_of::<BkStrategyVtable>(), 112);
    assert_eq!(align_of::<BkStrategyVtable>(), 8);
    assert_eq!(offset_of!(BkStrategyVtable, name), 0);
    assert_eq!(offset_of!(BkStrategyVtable, version), 8);
    assert_eq!(offset_of!(BkStrategyVtable, abi_version), 16);
    assert_eq!(offset_of!(BkStrategyVtable, min_abi), 20);
    assert_eq!(offset_of!(BkStrategyVtable, create), 24);
    assert_eq!(offset_of!(BkStrategyVtable, destroy), 32);
    assert_eq!(offset_of!(BkStrategyVtable, on_book), 40);
    assert_eq!(offset_of!(BkStrategyVtable, on_round), 48);
    assert_eq!(offset_of!(BkStrategyVtable, evaluate), 56);
    assert_eq!(offset_of!(BkStrategyVtable, confirmed_tokens), 64);
    assert_eq!(offset_of!(BkStrategyVtable, take_breaks), 72);
    assert_eq!(offset_of!(BkStrategyVtable, diagnostics), 80);
    assert_eq!(offset_of!(BkStrategyVtable, on_config), 88);
    assert_eq!(offset_of!(BkStrategyVtable, on_hot_params), 96);
    assert_eq!(offset_of!(BkStrategyVtable, knobs), 104);
}
