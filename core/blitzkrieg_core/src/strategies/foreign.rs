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

use super::{EngineStrategy, GateExemptions, StrategyCtx, StrategyExitIntent};
use crate::model::OrderbookSnapshot;
use crate::shadow_evolution::MutableParams;
use crate::signal::{SpreadArbConfig, TradeSignal, TrendConfig};
use arc_swap::ArcSwap;
use blitzkrieg_strategy_api::{
    BkBookView, BkGateExemptionsFn, BkHandle, BkLevel, BkMarket, BkRound, BkRoundView,
    BkStrategyVtable,
};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_char};
use std::str::FromStr;
use std::sync::Arc;

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

/// A loaded external v2 strategy. Owns the dlopen handle so the vtable's
/// function pointers stay valid for its whole lifetime.
pub struct ForeignStrategy {
    // The library must drop AFTER the vtable/handle: field order drops it last
    // (fields drop top-to-bottom), so declare it first.
    _lib: libloading::Library,
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

    // Outputs accumulated during evaluate(), drained by the host.
    exit_intents: Vec<StrategyExitIntent>,
    breaks: Vec<(String, Decimal)>,

    // Shadow Evolution: latest handle plus the JSON we last pushed, so the
    // strategy only sees a callback on an actual change (next-tick semantics).
    hot_params: Option<Arc<ArcSwap<MutableParams>>>,
    last_hot_json: Option<String>,

    // token → asset, refreshed from each round's markets so on_book (which only
    // carries a token) can still label the view with the underlying asset.
    assets: HashMap<String, String>,
}

impl ForeignStrategy {
    /// # Safety
    /// Caller provides a successfully negotiated v2 library: vtable/handle come
    /// from this `lib`, the library implements the documented contract (static
    /// vtable, valid create/destroy, JSON outputs freed by its own
    /// `bk_strategy_free_string`), and it is only ever driven single-threaded
    /// by the kernel's strategy loop.
    pub unsafe fn from_loaded(
        lib: libloading::Library,
        vtable: BkStrategyVtable,
        handle: BkHandle,
        name: String,
        version: String,
        free_string: Option<unsafe extern "C" fn(*mut c_char)>,
        gate_exemptions_fn: Option<BkGateExemptionsFn>,
    ) -> Self {
        Self {
            _lib: lib,
            vtable,
            handle,
            name,
            version,
            free_string,
            gate_exemptions_fn,
            exit_intents: Vec::new(),
            breaks: Vec::new(),
            hot_params: None,
            last_hot_json: None,
            assets: HashMap::new(),
        }
    }

    pub fn version(&self) -> &str {
        &self.version
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

    fn push_hot_params_if_changed(&mut self) {
        let Some(h) = &self.hot_params else { return };
        let Some(f) = self.vtable.on_hot_params else {
            return;
        };
        let p = h.load();
        let json = hot_params_json(&p);
        if self.last_hot_json.as_deref() == Some(json.as_str()) {
            return;
        }
        if let Ok(cs) = CString::new(json.clone()) {
            // SAFETY: valid handle + borrowed NUL string for the call.
            unsafe { f(self.handle, cs.as_ptr()) };
        }
        self.last_hot_json = Some(json);
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
    /// an in-tree strategy gets from the trait. Cached: the declaration is a
    /// property of the loaded instance, so it is resolved once (the symbol is
    /// called on demand but its result never widens between ticks).
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

    fn on_round(&mut self, slot: i64) {
        if let Some(f) = self.vtable.on_round {
            let round = BkRound {
                slot,
                time_left_sec: 0,
                now_ms: 0,
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

        // SAFETY: valid handle and a round view whose backing lives to end of
        // scope; the returned JSON is copied and freed via the library.
        let out = unsafe { evaluate(self.handle, &rv) };
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

    fn diagnostics(&self, _ctx: &StrategyCtx<'_>) -> Vec<serde_json::Value> {
        let Some(f) = self.vtable.diagnostics else {
            return Vec::new();
        };
        // SAFETY: hook returns a heap JSON array owned by the library.
        match unsafe { self.take_json(f(self.handle)) } {
            Some(serde_json::Value::Array(a)) => a,
            _ => Vec::new(),
        }
    }

    fn set_hot_params(&mut self, handle: Arc<ArcSwap<MutableParams>>) {
        // Reset the dedup marker so a fresh handle always pushes once.
        self.last_hot_json = None;
        self.hot_params = Some(handle);
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
            // SAFETY: handle came from this library's create().
            unsafe { destroy(self.handle) };
        }
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

/// Serialize the current MutableParams as camelCase JSON with decimal values as
/// strings (consistent with the no-float wire rule).
fn hot_params_json(p: &MutableParams) -> String {
    serde_json::json!({
        "trendMinPrice": p.trend_min_price.to_string(),
        "trendEntryFactor": p.trend_entry_factor.to_string(),
        "trendMaxEntryPrice": p.trend_max_entry_price.to_string(),
        "trendBrokenPrice": p.trend_broken_price.to_string(),
    })
    .to_string()
}
