//! The read-only `bk.*` surface (DEV_V0_3 §6.4 / E30 #336 task 2) plus the
//! suggested entry-point contract.
//!
//! Everything under `bk` is a PROJECTION of host state, never a capability:
//! no credentials, no order submission, no venue client. A strategy reads the
//! world and returns intents — the kernel adjudicates, sizes, gates, signs and
//! submits (§6.4: "建议出口，返回值交内核裁决 —— 不是下单 API").
//!
//! Surfaces (all read-only):
//!
//! | function          | returns                                                   |
//! |-------------------|-----------------------------------------------------------|
//! | `bk.now_ms()`     | host clock, integer ms                                     |
//! | `bk.round()`      | `{slot=, time_left_sec=, now_ms=}` or `nil` between rounds |
//! | `bk.markets()`    | array of round markets (`asset/condition_id/up_token/venue/…`) |
//! | `bk.book(sym)`    | one round token's book, decimal STRINGS, or `nil`          |
//! | `bk.params()`     | this strategy's hot parameters (`name → string value`)     |
//! | `bk.kline(sym,iv)`| the last CLOSED bar, or `nil` (E29's aggregator owns bars) |
//! | `bk.account()`    | public display fields only (§11.1: no credentials)         |
//! | `bk.holdings()`   | THIS strategy's open positions (array, decimal STRINGS)    |
//! | `bk.fees()`       | the active taker-fee schedule (`name/rate/exponent`) or nil |
//! | `bk.unified_events()` | confirmed cross-venue mappings (#427) or empty array   |
//! | `bk.arb.pairs()`  | RFC 568: THIS strategy's pair-book views (owner-filtered, eventually consistent) or empty array |
//!
//! Prices and sizes cross as exact decimal STRINGS (the `safe.rs` boundary
//! rule): a float would let a resting price drift by a tick. `bk.kline`
//! returns `nil` until the E29 aggregator lands behind it — disclosed in the
//! PR; the strategy treats it as "no data yet", not as an error.
//!
//! The `*_to_lua` helpers build the SAME table shapes the closures read, so
//! the adapter can push callback arguments through one spelling and the
//! read-only surface answers through another without the two drifting.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use blitzkrieg_strategy_api::{
    BookUpdate, HoldingView, Kline, KlineInterval, MarketInfo, RoundInfo,
};
use mlua::{Function, Lua, Table, Value};
use rust_decimal::Decimal;

/// §11.1: the public display fields of the account a strategy is authorized
/// to act for. `balance/available/reserved` are exact decimal STRINGS; there
/// is deliberately NO key/secret/address here (§11.1: 无凭证).
#[derive(Debug, Clone, Default)]
pub struct AccountView {
    pub id: String,
    pub name: String,
    pub market_type: String,
    pub balance: Option<String>,
    pub available: Option<String>,
    pub reserved: Option<String>,
}

/// The active taker-fee schedule as plain data (`bk.fees()`), the same
/// spelling the kernel charges by: `fee_usd = rate * (p*(1-p))^exponent` per
/// share. Strings on the decimal-STRING wire rule; `exponent` is an integer
/// count. Injected by the adapter from the core's ONE fee schedule — a
/// strategy never carries its own fee copy to guess with.
#[derive(Debug, Clone, Default)]
pub struct FeeScheduleView {
    pub name: String,
    pub rate: String,
    pub exponent: u32,
}

/// What walking the ladder for a target size yields.
///
/// `vwap` is the size-weighted average price across the levels actually
/// consumed, `filled` the size actually available (== target when `complete`,
/// less when the book ran out). Exact `rust_decimal` arithmetic host-side:
/// checked mul/add (a malicious or corrupt ladder can name huge mantissas —
/// the 2026-09-30 28-digit incident's domain), one final division, and the
/// result rounded to 10 decimal places so the STRING that crosses into Lua
/// parses back inside the safe 18-significant-digit domain of
/// `common/decimal.lua` (never the 28-digit float-mantissa trap).
#[derive(Debug, Clone)]
pub struct VwapWalk {
    pub vwap: Decimal,
    pub filled: Decimal,
    pub complete: bool,
}

/// Walk `ladder` (best-first `(price, size)` pairs) consuming `target` size.
/// `None` = a level failed to parse, sits outside the Lua-safe decimal domain,
/// or the arithmetic overflowed — an ERROR class the caller must surface
/// loudly, never a "treat as empty" (a corrupt ladder silently priced at best
/// is exactly the optimistic-fill bug).
///
/// The SAFE-DOMAIN gate (the 2026-09-30 28-digit incident's structural fix):
/// a row whose price/size carries more than 18 significant digits or a scale
/// above 18 is refused (`SAFE_MANTISSA_DIGITS`/`SAFE_SCALE`). Real books quote
/// ≤ 4 dp prices and ≤ 2 dp sizes, so the gate never fires on live data; it
/// exists so a host-derived or corrupt 28-digit mantissa can NEVER cross into
/// the arithmetic — the same domain `common/decimal.lua`'s `dec_parse` caps
/// at, enforced one layer earlier, in exact arithmetic instead of Lua floats.
pub fn vwap_for_size(ladder: &[(String, String)], target: Decimal) -> Option<VwapWalk> {
    const SAFE_MANTISSA_LIMIT: i128 = 10_000_000_000_000_000_000; // 10^19 exclusive
    const SAFE_SCALE: u32 = 18;
    let in_safe_domain =
        |d: Decimal| d.scale() <= SAFE_SCALE && d.mantissa().abs() < SAFE_MANTISSA_LIMIT;
    let mut filled = Decimal::ZERO;
    let mut notional = Decimal::ZERO;
    for (price_s, size_s) in ladder {
        let price = Decimal::from_str_exact(price_s.trim()).ok()?;
        let size = Decimal::from_str_exact(size_s.trim()).ok()?;
        if !in_safe_domain(price) || !in_safe_domain(size) {
            return None;
        }
        if price <= Decimal::ZERO || size <= Decimal::ZERO {
            // A book never quotes a non-positive level; treat a corrupt row as
            // a parse failure (loud), not as skippable (silent).
            return None;
        }
        let take = if filled + size >= target {
            target - filled
        } else {
            size
        };
        // checked: price*take overflows the 96-bit mantissa on absurd input
        // (second net under the domain gate above).
        let row = price.checked_mul(take)?;
        notional = notional.checked_add(row)?;
        filled = filled.checked_add(take)?;
        if filled >= target {
            break;
        }
    }
    if filled <= Decimal::ZERO {
        return None;
    }
    // Truncating division (checked_div): an under-deep ladder stops early with
    // filled < target — the VWAP is over what IS there, then `complete=false`
    // tells the caller the walk came up short. Scale capped at 10 dp so the
    // emitted string re-parses inside the Lua-side 18-digit safe domain.
    let vwap = notional.checked_div(filled)?;
    Some(VwapWalk {
        vwap: vwap.round_dp(10),
        filled,
        complete: filled >= target,
    })
}

/// Host-side snapshot the adapter refreshes before every sandbox call. One
/// `Mutex` around plain data: the adapter writes on its host thread, the `bk`
/// closures read on the same thread inside the VM call — no contention in
/// practice, and a poisoned mutex is impossible without a panic in the
/// adapter itself.
#[derive(Debug, Default)]
pub struct BkState {
    pub now_ms: i64,
    /// `(slot, time_left_sec, now_ms)` of the live round, if any.
    pub round: Option<(i64, i64, i64)>,
    pub markets: Vec<MarketInfo>,
    /// Last book per symbol (`on_book` pushes; `on_eval_books` overwrites with
    /// the freshness verdict attached).
    pub books: HashMap<String, BookUpdate>,
    /// Freshness verdict per symbol, from the eval-books gate (`None` = not
    /// part of this evaluation).
    pub fresh: HashMap<String, bool>,
    /// This strategy's hot parameters (host config + shadow-evolution bag).
    pub params: HashMap<String, String>,
    /// THIS strategy's open positions as of the start of this evaluation
    /// cycle (`bk.holdings()`), pushed by the adapter from the ctx's view.
    /// Empty until a host pushes one — `bk.holdings()` honestly returns an
    /// empty table, never another package's book.
    pub holdings: Vec<HoldingView>,
    /// The active taker-fee schedule, as plain data (lua_runtime must not
    /// depend on the core): `name/rate/exponent` strings. `None` = no schedule
    /// in force — `bk.fees()` returns nil and a strategy that prices fees
    /// must fail closed (skip the trade, never assume zero).
    pub fee_schedule: Option<FeeScheduleView>,
    pub account: Option<AccountView>,
    /// Last CLOSED bar per `(symbol, interval)` the host has seen. Empty until
    /// a host actually feeds klines — with no E29 aggregator there is no
    /// source, so `bk.kline` honestly returns `nil`.
    pub klines: HashMap<(String, String), Kline>,
    /// #427: the confirmed cross-venue mappings (`bk.unified_events()`),
    /// pushed by the adapter from the ctx's view. Empty until a host pushes
    /// one — single-venue deployments answer an empty table, never an error.
    pub unified_events: Vec<UnifiedEventView>,
    /// RFC 568 阶段一：THIS strategy's pair-book views (`bk.arb.pairs()`),
    /// pushed by the adapter from the ctx's view（owner 过滤已在内核侧做完
    /// —— 跨策略/跨账户不可见）。Empty until a host pushes one —— 策略读
    /// 不到配对簿时按「无登记」处理，绝不视为错误。最终一致读数（允许滞
    /// 后一拍，RFC 568 附注 2）。
    pub pair_views: Vec<PairViewRow>,
}

/// RFC 568 阶段一：one pair-book row as the `bk.arb.pairs()` wire row —
/// plain data（lua_runtime 不依赖 core 的 PairBook 类型）：id 是可读标签
/// （`strategy\u{1}account\u{1}pair_key` 拼形），status 是五态字面量。
#[derive(Debug, Clone, Default)]
pub struct PairViewRow {
    pub id: String,
    pub slot: i64,
    /// "pending" | "partial" | "complete" | "orphan" | "closed"。
    pub status: String,
    pub legs: Vec<PairLegRow>,
    /// 裸腿 token（partial/orphan 记名；其余空串）。
    pub naked_token: String,
}

/// RFC 568 阶段一：one leg row inside [`PairViewRow`]. `account_id` 是腿
/// 账户（策略本来就知道的盖章结果，非凭证字段）。
#[derive(Debug, Clone, Default)]
pub struct PairLegRow {
    pub token: String,
    pub account_id: String,
    pub shares: String,
    pub filled: String,
}

/// #427: one confirmed cross-venue mapping as the `bk.unified_events()` wire
/// row — plain data (lua_runtime must not depend on market_api): each venue
/// listing carries its own token ids. Decimal-free; ids and titles are the
/// only payload a cross-venue strategy needs.
#[derive(Debug, Clone, Default)]
pub struct UnifiedEventView {
    pub id: String,
    pub title: String,
    /// Venue-ordered rows: `venue/up_token/down_token/status` per listing.
    pub listings: Vec<UnifiedListingView>,
    /// Machine-readable discrepancy marks ("settlement_source" /
    /// "settlement_rules" / "expiry_gap") — a package that hedges must
    /// fail closed on any mark.
    pub discrepancies: Vec<String>,
    /// "paired" | "singleLegAvailable" (camelCase = the kernel wire).
    pub status: String,
}

/// One venue listing inside [`UnifiedEventView`].
#[derive(Debug, Clone, Default)]
pub struct UnifiedListingView {
    pub venue: String,
    pub up_token: String,
    pub down_token: String,
    pub expires_at_ms: i64,
}

/// Content-keyed cache slot for one `bk.*` projection (#448/#440).
///
/// The bk closures rebuild their Lua tables from scratch on every call, but
/// the host state behind them changes far less often than strategies read it
/// (the sampling that opened #448: table rebuilds + string interning + the
/// GC pressure they cause are ~50% of a replay's main thread). Each closure
/// keeps the LAST fingerprint it rendered plus the pinned table; a call whose
/// fingerprint is unchanged returns the cached table instead of rebuilding.
///
/// The fingerprint covers exactly the RENDERED fields (nothing more), so two
/// host snapshots that spell the same projection share one table. A single
/// slot per surface: the cache never grows — a content change re-renders over
/// it, and replacing the [`mlua::RegistryKey`] un-pins the old table for the
/// collector. The cached table is also the one a strategy may still hold from
/// an earlier call — same numbers it saw then, exactly as without the cache.
type CacheSlot = Option<(String, mlua::RegistryKey)>;

/// Fingerprint builder: every rendered field lands in one `\u{1}`-separated
/// string. Rendered-field-exact by construction — decimal-bearing fields
/// fingerprint through their `String` spellings, so two renderings differ iff
/// their fingerprints differ.
struct Fp(String);

impl Fp {
    fn new() -> Self {
        Fp(String::new())
    }
    fn field(&mut self, s: &str) -> &mut Self {
        self.0.push_str(s);
        self.0.push('\u{1}');
        self
    }
    fn opt(&mut self, v: &Option<String>) -> &mut Self {
        match v {
            Some(s) => self.field(s),
            None => {
                let _ = self.field("\u{0}");
                self
            }
        }
    }
    fn num<T: std::fmt::Display>(&mut self, v: T) -> &mut Self {
        let _ = write!(self.0, "{v}\u{1}");
        self
    }
    fn done(self) -> String {
        self.0
    }
}

/// Guard a rendered projection table against writes. §6.4 declares `bk.*` a
/// read-only projection; with the cache a mutation would now leak across
/// calls, so a hostile write is refused LOUDLY at the write site instead of
/// silently corrupting a shared table. `__index` stays unset (plain field
/// reads), `__metatable` hides the guard from `getmetatable`.
/// Freeze a rendered projection behind a read-only proxy (#448). Returns the
/// PROXY to hand out — the caller must never return the data table itself.
///
/// A plain metatable with `__newindex` does NOT suffice: Lua fires it only
/// for keys ABSENT from the table, so `t.best_bid = '0.99'` on an existing
/// field would land as a raw assignment. The proxy shape closes that hole by
/// construction — the proxy table itself is always empty, so EVERY write
/// goes through `__newindex` and errors, while reads forward through
/// `__index` to the real data table (kept alive by the metamethod capture).
/// `__len`/`__pairs` keep `#t`, `ipairs` and `pairs` working over the hidden
/// data (they ignore `__index`, verified against Lua semantics); the empty
/// table needs none of them — `#` is 0 and iteration yields nothing, which
/// is exactly the empty answer. Nested tables inside array projections are
/// frozen recursively, so `ms[1].asset = 'x'` is refused the same way.
fn guard_readonly(lua: &Lua, t: &Table) -> mlua::Result<Table> {
    // Freeze children first: array rows become proxies before anyone can
    // reach them through __index. Plain-data trees, no cycles by construction.
    for i in 1..=t.raw_len() {
        let v: Value = t.raw_get(i as i64)?;
        if let Value::Table(inner) = v {
            let frozen = guard_readonly(lua, &inner)?;
            t.raw_set(i as i64, frozen)?;
        }
    }
    let deny = lua.create_function(|_, (key, _): (Value, Value)| {
        let name = match &key {
            Value::String(s) => s.to_string_lossy(),
            other => format!("{other:?}"),
        };
        Err::<(), _>(mlua::Error::runtime(format!(
            "bk.* projections are read-only (attempt to set '{name}')"
        )))
    })?;
    let proxy = lua.create_table()?;
    let mt = lua.create_table()?;
    mt.raw_set("__index", t.clone())?;
    mt.raw_set("__newindex", deny)?;
    if t.raw_len() > 0 {
        // `__len` must answer the DATA table's length — the proxy's own is 0.
        let data = t.clone();
        mt.raw_set(
            "__len",
            lua.create_function(move |_, ()| Ok(data.raw_len()))?,
        )?;
        // pairs() ignores __index; hand it the real table with the global next.
        let data = t.clone();
        mt.raw_set(
            "__pairs",
            lua.create_function(move |lua, ()| {
                let next: Function = lua.globals().raw_get("next")?;
                Ok((next, data.clone(), Value::Nil))
            })?,
        )?;
    }
    mt.raw_set("__metatable", "bk projection")?;
    let _ = proxy.set_metatable(Some(mt));
    Ok(proxy)
}

impl BkState {
    /// Refresh the clock (the adapter calls this before every callback).
    pub fn set_now(&mut self, now_ms: i64) {
        self.now_ms = now_ms;
    }

    /// Overwrite the book of one symbol and stamp its freshness.
    pub fn put_book(&mut self, update: BookUpdate, fresh: Option<bool>) {
        let symbol = update.symbol.clone();
        if let Some(f) = fresh {
            self.fresh.insert(symbol.clone(), f);
        }
        self.books.insert(symbol, update);
    }
}

/// The suggested entry points a Lua strategy MAY define (§6.5). `bk_evaluate`
/// is the only REQUIRED one. The loader checks it (missing required →
/// refuse); `LuaStrategy` calls the ones that exist and skips the rest.
pub const ENTRY_POINTS: [&str; 4] = ["bk_evaluate", "bk_on_book", "bk_on_round", "bk_on_kline"];

/// snake_case wire name of an interval (the `bk.kline(sym, interval)` key
/// spelling, §6.4 table). Delegates to the shared type's ONE mapping
/// (`KlineInterval::wire_name`) so the Lua table and the C-ABI
/// `bk_strategy_on_kline` view can never disagree on "min1".
pub fn interval_name(interval: KlineInterval) -> &'static str {
    interval.wire_name()
}

/// `{slot=, time_left_sec=, now_ms=}` — the `RoundInfo` table.
pub fn round_to_lua(lua: &Lua, round: &RoundInfo) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("slot", round.slot)?;
    t.raw_set("time_left_sec", round.time_left_sec)?;
    t.raw_set("now_ms", round.now_ms)?;
    Ok(t)
}

/// One book as decimal STRINGS (`None` fields → nil). Field set is the §6.5
/// wire exactly: `symbol/best_bid/best_ask/mid/obi/spread/bid_depth/
/// ask_depth/ts_ms/fresh` plus the top-N ladders `bid_ladder/ask_ladder`
/// (arrays of `{price=, size=}` decimal STRINGS, best-first; empty when the
/// side is empty or the host predates the ladders). `fresh` stamps the host's
/// freshness verdict when the caller has one (the eval gate); absent otherwise.
pub fn book_to_lua(lua: &Lua, u: &BookUpdate, fresh: Option<bool>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("symbol", u.symbol.as_str())?;
    for (key, value) in [
        ("best_bid", &u.best_bid),
        ("best_ask", &u.best_ask),
        ("mid", &u.mid),
        ("obi", &u.obi),
        ("spread", &u.spread),
        ("bid_depth", &u.bid_depth),
        ("ask_depth", &u.ask_depth),
    ] {
        match value {
            Some(v) => t.raw_set(key, v.as_str())?,
            None => t.raw_set(key, Value::Nil)?,
        }
    }
    t.raw_set("ts_ms", u.timestamp_ms)?;
    match fresh {
        Some(f) => t.raw_set("fresh", f)?,
        None => t.raw_set("fresh", Value::Nil)?,
    }
    for (key, ladder) in [("bid_ladder", &u.bid_ladder), ("ask_ladder", &u.ask_ladder)] {
        let arr = lua.create_table()?;
        for (i, (price, size)) in ladder.iter().enumerate() {
            let row = lua.create_table()?;
            row.raw_set("price", price.as_str())?;
            row.raw_set("size", size.as_str())?;
            arr.raw_set(i as i64 + 1, row)?;
        }
        t.raw_set(key, arr)?;
    }
    Ok(t)
}

/// One CLOSED bar as decimal STRINGS (`is_closed` is the only boolean — a
/// strategy that forgot to check it would fire repeated signals, §6.6).
pub fn kline_to_lua(lua: &Lua, k: &Kline) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("symbol", k.symbol.as_str())?;
    t.raw_set("interval", interval_name(k.interval))?;
    t.raw_set("open_time_ms", k.open_time_ms)?;
    t.raw_set("close_time_ms", k.close_time_ms)?;
    t.raw_set("open", k.open.to_string())?;
    t.raw_set("high", k.high.to_string())?;
    t.raw_set("low", k.low.to_string())?;
    t.raw_set("close", k.close.to_string())?;
    t.raw_set("volume", k.volume.to_string())?;
    t.raw_set("trade_count", k.trade_count)?;
    t.raw_set("is_closed", k.is_closed)?;
    Ok(t)
}

/// Install the `bk` table into the sandbox globals. Idempotent per state
/// machine (each strategy owns its own `Lua`). Every table-returning closure
/// renders through a content-keyed cache ([`ProjectionCache`]) — see #448.
pub fn install_bk(lua: &Lua, state: Arc<Mutex<BkState>>) -> mlua::Result<()> {
    let bk = lua.create_table()?;

    // -- bk.now_ms() --------------------------------------------------------
    {
        let st = Arc::clone(&state);
        bk.raw_set(
            "now_ms",
            lua.create_function(move |_, ()| {
                let st = st.lock().expect("bk state mutex");
                Ok(st.now_ms)
            })?,
        )?;
    }

    // -- bk.round() ---------------------------------------------------------
    {
        let st = Arc::clone(&state);
        let slot: std::cell::RefCell<CacheSlot> = std::cell::RefCell::new(None);
        bk.raw_set(
            "round",
            lua.create_function(move |lua, ()| {
                let st = st.lock().expect("bk state mutex");
                match st.round {
                    None => Ok(Value::Nil),
                    Some((slot_, time_left_sec, now)) => {
                        let mut fp = Fp::new();
                        let _ = fp.num(slot_).num(time_left_sec).num(now);
                        let key = fp.done();
                        if let Some((k, reg)) = slot.borrow().as_ref()
                            && *k == key
                        {
                            let t: Table = lua.registry_value(reg)?;
                            return Ok(Value::Table(t));
                        }
                        let t = round_to_lua(
                            lua,
                            &RoundInfo {
                                slot: slot_,
                                time_left_sec,
                                now_ms: now,
                            },
                        )?;
                        let t = guard_readonly(lua, &t)?;
                        let reg = lua.create_registry_value(t)?;
                        *slot.borrow_mut() = Some((key, reg));
                        match slot.borrow().as_ref() {
                            Some((_, reg)) => {
                                let t: Table = lua.registry_value(reg)?;
                                Ok(Value::Table(t))
                            }
                            None => unreachable!("slot just stored"),
                        }
                    }
                }
            })?,
        )?;
    }

    // -- bk.markets() -------------------------------------------------------
    {
        let st = Arc::clone(&state);
        let slot: std::cell::RefCell<CacheSlot> = std::cell::RefCell::new(None);
        bk.raw_set(
            "markets",
            lua.create_function(move |lua, ()| {
                let st = st.lock().expect("bk state mutex");
                let mut fp = Fp::new();
                for m in &st.markets {
                    fp.field(&m.asset)
                        .field(&m.condition_id)
                        .field(&m.up_token)
                        .field(&m.down_token)
                        .num(m.expires_at_ms)
                        .num(m.slot)
                        .field(if m.neg_risk { "1" } else { "0" })
                        .field(&m.venue);
                }
                let key = fp.done();
                if let Some((k, reg)) = slot.borrow().as_ref()
                    && *k == key
                {
                    let t: Table = lua.registry_value(reg)?;
                    return Ok(Value::Table(t));
                }
                let arr = lua.create_table()?;
                for (i, m) in st.markets.iter().enumerate() {
                    let t = lua.create_table()?;
                    t.raw_set("asset", m.asset.as_str())?;
                    t.raw_set("condition_id", m.condition_id.as_str())?;
                    t.raw_set("up_token", m.up_token.as_str())?;
                    t.raw_set("down_token", m.down_token.as_str())?;
                    t.raw_set("expires_at_ms", m.expires_at_ms)?;
                    t.raw_set("slot", m.slot)?;
                    t.raw_set("neg_risk", m.neg_risk)?;
                    // #427: the listing venue, pure data (nil = undeclared).
                    if m.venue.is_empty() {
                        t.raw_set("venue", Value::Nil)?;
                    } else {
                        t.raw_set("venue", m.venue.as_str())?;
                    }
                    arr.raw_set(i as i64 + 1, t)?;
                }
                let arr = guard_readonly(lua, &arr)?;
                let reg = lua.create_registry_value(arr)?;
                *slot.borrow_mut() = Some((key, reg));
                match slot.borrow().as_ref() {
                    Some((_, reg)) => {
                        let t: Table = lua.registry_value(reg)?;
                        Ok(Value::Table(t))
                    }
                    None => unreachable!("slot just stored"),
                }
            })?,
        )?;
    }

    // -- bk.book(symbol) ----------------------------------------------------
    {
        let st = Arc::clone(&state);
        let slot: std::cell::RefCell<CacheSlot> = std::cell::RefCell::new(None);
        bk.raw_set(
            "book",
            lua.create_function(move |lua, symbol: String| {
                let st = st.lock().expect("bk state mutex");
                let Some(u) = st.books.get(&symbol) else {
                    return Ok(Value::Nil);
                };
                let fresh = st.fresh.get(&symbol).copied();
                let mut fp = Fp::new();
                fp.field(&u.symbol).field(&u.asset);
                for v in [
                    &u.best_bid,
                    &u.best_ask,
                    &u.mid,
                    &u.obi,
                    &u.spread,
                    &u.bid_depth,
                    &u.ask_depth,
                    &u.spread_pct,
                ] {
                    let _ = fp.opt(v);
                }
                let _ = fp.num(u.timestamp_ms).num(u.bid_levels).num(u.ask_levels);
                match fresh {
                    Some(f) => {
                        let _ = fp.field(if f { "1" } else { "0" });
                    }
                    None => {
                        let _ = fp.field("\u{0}");
                    }
                }
                let key = fp.done();
                if let Some((k, reg)) = slot.borrow().as_ref()
                    && *k == key
                {
                    let t: Table = lua.registry_value(reg)?;
                    return Ok(Value::Table(t));
                }
                let t = book_to_lua(lua, u, fresh)?;
                let t = guard_readonly(lua, &t)?;
                let reg = lua.create_registry_value(t)?;
                *slot.borrow_mut() = Some((key, reg));
                match slot.borrow().as_ref() {
                    Some((_, reg)) => {
                        let t: Table = lua.registry_value(reg)?;
                        Ok(Value::Table(t))
                    }
                    None => unreachable!("slot just stored"),
                }
            })?,
        )?;
    }

    // -- bk.params() --------------------------------------------------------
    {
        let st = Arc::clone(&state);
        let slot: std::cell::RefCell<CacheSlot> = std::cell::RefCell::new(None);
        bk.raw_set(
            "params",
            lua.create_function(move |lua, ()| {
                let st = st.lock().expect("bk state mutex");
                let mut fp = Fp::new();
                // HashMap order is unstable — sort so equal content equals key.
                let mut keys: Vec<&String> = st.params.keys().collect();
                keys.sort_unstable();
                for k in keys {
                    let _ = fp.field(k).field(&st.params[k]);
                }
                let key = fp.done();
                if let Some((k, reg)) = slot.borrow().as_ref()
                    && *k == key
                {
                    let t: Table = lua.registry_value(reg)?;
                    return Ok(Value::Table(t));
                }
                let t = lua.create_table()?;
                for (k, v) in &st.params {
                    t.raw_set(k.as_str(), v.as_str())?;
                }
                let t = guard_readonly(lua, &t)?;
                let reg = lua.create_registry_value(t)?;
                *slot.borrow_mut() = Some((key, reg));
                match slot.borrow().as_ref() {
                    Some((_, reg)) => {
                        let t: Table = lua.registry_value(reg)?;
                        Ok(Value::Table(t))
                    }
                    None => unreachable!("slot just stored"),
                }
            })?,
        )?;
    }

    // -- bk.kline(symbol, interval) -----------------------------------------
    {
        let st = Arc::clone(&state);
        let slot: std::cell::RefCell<CacheSlot> = std::cell::RefCell::new(None);
        bk.raw_set(
            "kline",
            lua.create_function(move |lua, (symbol, interval): (String, String)| {
                let st = st.lock().expect("bk state mutex");
                let Some(k) = st.klines.get(&(symbol, interval)) else {
                    // E29: no aggregator behind this yet — "no data", not an error.
                    return Ok(Value::Nil);
                };
                let mut fp = Fp::new();
                let _ = fp
                    .field(&k.symbol)
                    .field(interval_name(k.interval))
                    .num(k.open_time_ms)
                    .num(k.close_time_ms)
                    .field(&k.open.to_string())
                    .field(&k.high.to_string())
                    .field(&k.low.to_string())
                    .field(&k.close.to_string())
                    .field(&k.volume.to_string())
                    .num(k.trade_count)
                    .field(if k.is_closed { "1" } else { "0" });
                let key = fp.done();
                if let Some((k_, reg)) = slot.borrow().as_ref()
                    && *k_ == key
                {
                    let t: Table = lua.registry_value(reg)?;
                    return Ok(Value::Table(t));
                }
                let t = kline_to_lua(lua, k)?;
                let t = guard_readonly(lua, &t)?;
                let reg = lua.create_registry_value(t)?;
                *slot.borrow_mut() = Some((key, reg));
                match slot.borrow().as_ref() {
                    Some((_, reg)) => {
                        let t: Table = lua.registry_value(reg)?;
                        Ok(Value::Table(t))
                    }
                    None => unreachable!("slot just stored"),
                }
            })?,
        )?;
    }

    // -- bk.account() -------------------------------------------------------
    {
        let st = Arc::clone(&state);
        let slot: std::cell::RefCell<CacheSlot> = std::cell::RefCell::new(None);
        bk.raw_set(
            "account",
            lua.create_function(move |lua, ()| {
                let st = st.lock().expect("bk state mutex");
                let Some(a) = &st.account else {
                    return Ok(Value::Nil);
                };
                let mut fp = Fp::new();
                let _ = fp
                    .field(&a.id)
                    .field(&a.name)
                    .field(&a.market_type)
                    .opt(&a.balance)
                    .opt(&a.available)
                    .opt(&a.reserved);
                let key = fp.done();
                if let Some((k, reg)) = slot.borrow().as_ref()
                    && *k == key
                {
                    let t: Table = lua.registry_value(reg)?;
                    return Ok(Value::Table(t));
                }
                let t = lua.create_table()?;
                t.raw_set("id", a.id.as_str())?;
                t.raw_set("name", a.name.as_str())?;
                t.raw_set("market_type", a.market_type.as_str())?;
                for (key, value) in [
                    ("balance", &a.balance),
                    ("available", &a.available),
                    ("reserved", &a.reserved),
                ] {
                    match value {
                        Some(v) => t.raw_set(key, v.as_str())?,
                        None => t.raw_set(key, Value::Nil)?,
                    }
                }
                let t = guard_readonly(lua, &t)?;
                let reg = lua.create_registry_value(t)?;
                *slot.borrow_mut() = Some((key, reg));
                match slot.borrow().as_ref() {
                    Some((_, reg)) => {
                        let t: Table = lua.registry_value(reg)?;
                        Ok(Value::Table(t))
                    }
                    None => unreachable!("slot just stored"),
                }
            })?,
        )?;
    }

    // -- bk.holdings() ------------------------------------------------------
    {
        let st = Arc::clone(&state);
        let slot: std::cell::RefCell<CacheSlot> = std::cell::RefCell::new(None);
        bk.raw_set(
            "holdings",
            lua.create_function(move |lua, ()| {
                let st = st.lock().expect("bk state mutex");
                let mut fp = Fp::new();
                for h in &st.holdings {
                    let _ = fp
                        .field(&h.condition_id)
                        .field(&h.token_id)
                        .field(&h.direction)
                        .field(&h.shares)
                        .field(&h.entry_price)
                        .num(h.round_slot);
                }
                let key = fp.done();
                if let Some((k, reg)) = slot.borrow().as_ref()
                    && *k == key
                {
                    let t: Table = lua.registry_value(reg)?;
                    return Ok(Value::Table(t));
                }
                let arr = lua.create_table()?;
                for (i, h) in st.holdings.iter().enumerate() {
                    let t = lua.create_table()?;
                    t.raw_set("condition_id", h.condition_id.as_str())?;
                    t.raw_set("token_id", h.token_id.as_str())?;
                    t.raw_set("direction", h.direction.as_str())?;
                    t.raw_set("shares", h.shares.as_str())?;
                    t.raw_set("entry_price", h.entry_price.as_str())?;
                    t.raw_set("round_slot", h.round_slot)?;
                    arr.raw_set(i as i64 + 1, t)?;
                }
                let arr = guard_readonly(lua, &arr)?;
                let reg = lua.create_registry_value(arr)?;
                *slot.borrow_mut() = Some((key, reg));
                match slot.borrow().as_ref() {
                    Some((_, reg)) => {
                        let t: Table = lua.registry_value(reg)?;
                        Ok(Value::Table(t))
                    }
                    None => unreachable!("slot just stored"),
                }
            })?,
        )?;
    }

    // -- bk.fees() ----------------------------------------------------------
    {
        let st = Arc::clone(&state);
        let slot: std::cell::RefCell<CacheSlot> = std::cell::RefCell::new(None);
        bk.raw_set(
            "fees",
            lua.create_function(move |lua, ()| {
                let st = st.lock().expect("bk state mutex");
                let Some(f) = &st.fee_schedule else {
                    // No schedule in force: "unknown", never "free". A strategy
                    // that prices fees must fail closed on nil.
                    return Ok(Value::Nil);
                };
                let mut fp = Fp::new();
                let _ = fp.field(&f.name).field(&f.rate).num(f.exponent);
                let key = fp.done();
                if let Some((k, reg)) = slot.borrow().as_ref()
                    && *k == key
                {
                    let t: Table = lua.registry_value(reg)?;
                    return Ok(Value::Table(t));
                }
                let t = lua.create_table()?;
                t.raw_set("name", f.name.as_str())?;
                t.raw_set("rate", f.rate.as_str())?;
                t.raw_set("exponent", f.exponent)?;
                let t = guard_readonly(lua, &t)?;
                let reg = lua.create_registry_value(t)?;
                *slot.borrow_mut() = Some((key, reg));
                match slot.borrow().as_ref() {
                    Some((_, reg)) => {
                        let t: Table = lua.registry_value(reg)?;
                        Ok(Value::Table(t))
                    }
                    None => unreachable!("slot just stored"),
                }
            })?,
        )?;
    }

    // -- bk.unified_events() (#427) ------------------------------------------
    {
        let st = Arc::clone(&state);
        let slot: std::cell::RefCell<CacheSlot> = std::cell::RefCell::new(None);
        bk.raw_set(
            "unified_events",
            lua.create_function(move |lua, ()| {
                let st = st.lock().expect("bk state mutex");
                let mut fp = Fp::new();
                for e in &st.unified_events {
                    let _ = fp.field(&e.id).field(&e.title).field(&e.status);
                    for d in &e.discrepancies {
                        let _ = fp.field(d);
                    }
                    let _ = fp.field("\u{2}");
                    for l in &e.listings {
                        let _ = fp
                            .field(&l.venue)
                            .field(&l.up_token)
                            .field(&l.down_token)
                            .num(l.expires_at_ms);
                    }
                    let _ = fp.field("\u{2}");
                }
                let key = fp.done();
                if let Some((k, reg)) = slot.borrow().as_ref()
                    && *k == key
                {
                    let t: Table = lua.registry_value(reg)?;
                    return Ok(Value::Table(t));
                }
                let arr = lua.create_table()?;
                for (i, e) in st.unified_events.iter().enumerate() {
                    let t = lua.create_table()?;
                    t.raw_set("id", e.id.as_str())?;
                    t.raw_set("title", e.title.as_str())?;
                    t.raw_set("status", e.status.as_str())?;
                    let disc = lua.create_table()?;
                    for (j, d) in e.discrepancies.iter().enumerate() {
                        disc.raw_set(j as i64 + 1, d.as_str())?;
                    }
                    t.raw_set("discrepancies", disc)?;
                    let ls = lua.create_table()?;
                    for (j, l) in e.listings.iter().enumerate() {
                        let lt = lua.create_table()?;
                        lt.raw_set("venue", l.venue.as_str())?;
                        lt.raw_set("up_token", l.up_token.as_str())?;
                        lt.raw_set("down_token", l.down_token.as_str())?;
                        lt.raw_set("expires_at_ms", l.expires_at_ms)?;
                        ls.raw_set(j as i64 + 1, lt)?;
                    }
                    t.raw_set("listings", ls)?;
                    arr.raw_set(i as i64 + 1, t)?;
                }
                let arr = guard_readonly(lua, &arr)?;
                let reg = lua.create_registry_value(arr)?;
                *slot.borrow_mut() = Some((key, reg));
                match slot.borrow().as_ref() {
                    Some((_, reg)) => {
                        let t: Table = lua.registry_value(reg)?;
                        Ok(Value::Table(t))
                    }
                    None => unreachable!("slot just stored"),
                }
            })?,
        )?;
    }

    // -- bk.arb.pairs() (RFC 568 阶段一) --------------------------------------
    // The pair-book's owner-filtered read projection: one row per pair THIS
    // strategy instance may see (owner 过滤在内核侧做完 —— 跨策略/跨账户不
    // 可见)。最终一致读数（RFC 568 附注 2）：读数是调用瞬间的快照，允许滞
    // 后一拍 —— 事实事件（成交/死亡）落账与本次读数之间没有 happens-before。
    // Empty until a host pushes one：读不到配对簿按「无登记」处理（幂等去
    // 重内核兜底），绝不视为错误。
    {
        let st = Arc::clone(&state);
        let slot: std::cell::RefCell<CacheSlot> = std::cell::RefCell::new(None);
        let arb = lua.create_table()?;
        arb.raw_set(
            "pairs",
            lua.create_function(move |lua, ()| {
                let st = st.lock().expect("bk state mutex");
                let mut fp = Fp::new();
                for p in &st.pair_views {
                    let _ = fp.field(&p.id).num(p.slot).field(&p.status);
                    for l in &p.legs {
                        let _ = fp
                            .field(&l.token)
                            .field(&l.account_id)
                            .field(&l.shares)
                            .field(&l.filled);
                    }
                    let _ = fp.field(&p.naked_token).field("\u{2}");
                }
                let key = fp.done();
                if let Some((k, reg)) = slot.borrow().as_ref()
                    && *k == key
                {
                    let t: Table = lua.registry_value(reg)?;
                    return Ok(Value::Table(t));
                }
                let arr = lua.create_table()?;
                for (i, p) in st.pair_views.iter().enumerate() {
                    let t = lua.create_table()?;
                    t.raw_set("id", p.id.as_str())?;
                    t.raw_set("slot", p.slot)?;
                    t.raw_set("status", p.status.as_str())?;
                    t.raw_set("naked_token", p.naked_token.as_str())?;
                    let legs = lua.create_table()?;
                    for (j, l) in p.legs.iter().enumerate() {
                        let lt = lua.create_table()?;
                        lt.raw_set("token", l.token.as_str())?;
                        lt.raw_set("account_id", l.account_id.as_str())?;
                        lt.raw_set("shares", l.shares.as_str())?;
                        lt.raw_set("filled", l.filled.as_str())?;
                        legs.raw_set(j as i64 + 1, lt)?;
                    }
                    t.raw_set("legs", legs)?;
                    arr.raw_set(i as i64 + 1, t)?;
                }
                let arr = guard_readonly(lua, &arr)?;
                let reg = lua.create_registry_value(arr)?;
                *slot.borrow_mut() = Some((key, reg));
                match slot.borrow().as_ref() {
                    Some((_, reg)) => {
                        let t: Table = lua.registry_value(reg)?;
                        Ok(Value::Table(t))
                    }
                    None => unreachable!("slot just stored"),
                }
            })?,
        )?;
        bk.raw_set("arb", arb)?;
    }

    // -- bk.vwap_for_size(symbol, side, size) ---------------------------------
    // The depth-aware cost model: walk the top-N ladder of ONE side consuming
    // `size`, answer `{ vwap=, filled=, complete= }` (decimal STRINGS +
    // boolean) — or `nil` when the ladder cannot fill the target. NIL IS THE
    // CONTRACT for "not fillable here": a caller that ignores it must not
    // fall back to the best price (that IS the optimistic-fill bug this
    // surface exists to kill). Fail-loud errors:
    //   * unknown symbol  → nil (same answer `bk.book` gives);
    //   * non-numeric size / side not "bid"/"ask" → LOUD error (a strategy
    //     bug, deterministic);
    //   * a corrupt ladder row (unparseable / non-positive) or a checked
    //     arithmetic overflow → LOUD error naming the symbol — never a silent
    //     nil, never a best-price fallback (the 2026-09-30 28-digit incident's
    //     failure mode, killed at the surface this time).
    {
        let st = Arc::clone(&state);
        bk.raw_set(
            "vwap_for_size",
            lua.create_function(
                move |lua, (symbol, side, size_s): (String, String, String)| {
                    let side_l = side.trim().to_ascii_lowercase();
                    if side_l != "bid" && side_l != "ask" {
                        return Err(mlua::Error::runtime(format!(
                            "bk.vwap_for_size: side must be 'bid' or 'ask', got '{side}'"
                        )));
                    }
                    let Ok(target) = Decimal::from_str_exact(size_s.trim()) else {
                        return Err(mlua::Error::runtime(format!(
                            "bk.vwap_for_size: size must be a decimal string, got '{size_s}'"
                        )));
                    };
                    if target <= Decimal::ZERO {
                        return Err(mlua::Error::runtime(format!(
                            "bk.vwap_for_size: size must be positive, got '{size_s}'"
                        )));
                    }
                    let st = st.lock().expect("bk state mutex");
                    let Some(u) = st.books.get(&symbol) else {
                        return Ok(Value::Nil);
                    };
                    let ladder = if side_l == "bid" {
                        &u.bid_ladder
                    } else {
                        &u.ask_ladder
                    };
                    // Empty ladder / zero book: nothing to fill from — nil.
                    if ladder.is_empty() {
                        return Ok(Value::Nil);
                    }
                    let Some(walk) = vwap_for_size(ladder, target) else {
                        return Err(mlua::Error::runtime(format!(
                            "bk.vwap_for_size: corrupt ladder row or arithmetic overflow on {symbol} ({side_l}) — refusing to price silently"
                        )));
                    };
                    if !walk.complete {
                        // Deep-shortage: nil, per the contract — the caller
                        // decides to shrink the target or skip, never to
                        // assume the best price fills the difference.
                        return Ok(Value::Nil);
                    }
                    let t = lua.create_table()?;
                    t.raw_set("vwap", walk.vwap.to_string())?;
                    t.raw_set("filled", walk.filled.to_string())?;
                    t.raw_set("complete", walk.complete)?;
                    Ok(Value::Table(t))
                },
            )?,
        )?;
    }

    let globals = lua.globals();
    globals.raw_set("bk", bk)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::LuaSandbox;

    fn market() -> MarketInfo {
        MarketInfo {
            asset: "BTC".into(),
            condition_id: "0xcond".into(),
            up_token: "0xup".into(),
            down_token: "0xdown".into(),
            expires_at_ms: 1_700_000_000_000,
            slot: 42,
            neg_risk: false,
            venue: "polymarket".into(),
        }
    }

    fn book(symbol: &str) -> BookUpdate {
        BookUpdate {
            symbol: symbol.into(),
            asset: "BTC".into(),
            best_bid: Some("0.55".into()),
            best_ask: Some("0.57".into()),
            mid: Some("0.56".into()),
            bid_depth: Some("120".into()),
            ask_depth: Some("80".into()),
            obi: Some("0.2".into()),
            spread: Some("0.02".into()),
            spread_pct: Some("3.57".into()),
            timestamp_ms: 1_234,
            bid_levels: 5,
            ask_levels: 4,
            bid_ladder: vec![
                ("0.55".into(), "20".into()),
                ("0.54".into(), "30".into()),
                ("0.53".into(), "50".into()),
                ("0.52".into(), "10".into()),
                ("0.51".into(), "10".into()),
            ],
            ask_ladder: vec![
                ("0.57".into(), "10".into()),
                ("0.58".into(), "20".into()),
                ("0.59".into(), "30".into()),
                ("0.60".into(), "20".into()),
            ],
        }
    }

    /// A 3-level synthetic ask ladder for the VWAP byte-exact assertions.
    fn three_level_book(symbol: &str) -> BookUpdate {
        BookUpdate {
            ask_ladder: vec![
                ("0.50".into(), "10".into()),
                ("0.52".into(), "10".into()),
                ("0.56".into(), "10".into()),
            ],
            ..book(symbol)
        }
    }

    fn kline() -> Kline {
        Kline {
            symbol: "0xup".into(),
            interval: KlineInterval::Min1,
            open_time_ms: 1_000,
            close_time_ms: 1_059_999,
            open: "0.50".parse().expect("dec"),
            high: "0.60".parse().expect("dec"),
            low: "0.49".parse().expect("dec"),
            close: "0.55".parse().expect("dec"),
            volume: "1234.5".parse().expect("dec"),
            trade_count: 42,
            is_closed: true,
        }
    }

    fn with_state() -> (LuaSandbox, Arc<Mutex<BkState>>) {
        let sb = LuaSandbox::new().expect("sandbox");
        let st = Arc::new(Mutex::new(BkState::default()));
        install_bk(sb.lua(), Arc::clone(&st)).expect("install bk");
        (sb, st)
    }

    /// §6.4: the whole surface is readable and shaped as documented.
    #[test]
    fn bk_surface_reads_host_state() {
        let (sb, st) = with_state();
        {
            let mut s = st.lock().expect("lock");
            s.set_now(1_234_567);
            s.round = Some((42, 300, 1_234_567));
            s.markets = vec![market()];
            s.put_book(book("0xup"), Some(true));
            s.params.insert("threshold".into(), "0.03".into());
            s.account = Some(AccountView {
                id: "acct-1".into(),
                name: "dry-run".into(),
                market_type: "prediction".into(),
                balance: Some("10000.00".into()),
                available: Some("9500.00".into()),
                reserved: Some("500.00".into()),
            });
            s.klines.insert(("0xup".into(), "min1".into()), kline());
            s.unified_events = vec![UnifiedEventView {
                id: "evt-1".into(),
                title: "Bitcoin Up or Down".into(),
                status: "paired".into(),
                discrepancies: vec![],
                listings: vec![
                    UnifiedListingView {
                        venue: "polymarket".into(),
                        up_token: "pm-up".into(),
                        down_token: "pm-down".into(),
                        expires_at_ms: 1_700_000_000_000,
                    },
                    UnifiedListingView {
                        venue: "kalshi".into(),
                        up_token: "kx-up".into(),
                        down_token: "kx-down".into(),
                        expires_at_ms: 1_700_000_000_000,
                    },
                ],
            }];
        }
        sb.load(
            "function fail(msg) error(msg, 0) end \
             function bk_evaluate() \
                local r = bk.round() \
                if r == nil or r.slot ~= 42 then fail('round') end \
                if bk.now_ms() ~= 1234567 then fail('now') end \
                local ms = bk.markets() \
                if #ms ~= 1 or ms[1].up_token ~= '0xup' or ms[1].neg_risk ~= false then fail('markets') end \
                if ms[1].venue ~= 'polymarket' then fail('market venue') end \
                local b = bk.book('0xup') \
                if b == nil or b.best_bid ~= '0.55' or b.fresh ~= true or b.bid_levels ~= nil then fail('book') end \
                if bk.book('nope') ~= nil then fail('missing book') end \
                if bk.params().threshold ~= '0.03' then fail('params') end \
                local a = bk.account() \
                if a == nil or a.balance ~= '10000.00' or a.id ~= 'acct-1' then fail('account') end \
                local k = bk.kline('0xup', 'min1') \
                if k == nil or k.close ~= '0.55' or k.is_closed ~= true then fail('kline') end \
                local ue = bk.unified_events() \
                if #ue ~= 1 or ue[1].id ~= 'evt-1' then fail('unified') end \
                if ue[1].status ~= 'paired' then fail('unified status') end \
                if #ue[1].listings ~= 2 or ue[1].listings[1].venue ~= 'polymarket' or ue[1].listings[2].up_token ~= 'kx-up' then fail('unified listings') end \
                return {} \
             end",
            "surface",
        )
        .expect("loads");
        let f: mlua::Function = sb
            .lua()
            .globals()
            .get("bk_evaluate")
            .expect("bk_evaluate present");
        sb.invoke("evaluate", |_| f.call::<Value>(()))
            .expect("all surfaces read cleanly");
    }

    /// Empty state: every function answers nil/empty, never errors — a strategy
    /// must tolerate a round gap without a host crash.
    #[test]
    fn bk_surface_tolerates_empty_state() {
        let (sb, _st) = with_state();
        sb.load(
            "function fail(msg) error(msg, 0) end \
             function bk_evaluate() \
                if bk.round() ~= nil then fail('round') end \
                if #bk.markets() ~= 0 then fail('markets') end \
                if bk.book('x') ~= nil then fail('book') end \
                if bk.account() ~= nil then fail('account') end \
                if bk.kline('x', 'min1') ~= nil then fail('kline') end \
                if #bk.unified_events() ~= 0 then fail('unified') end \
                return {} \
             end",
            "empty",
        )
        .expect("loads");
        let f: mlua::Function = sb
            .lua()
            .globals()
            .get("bk_evaluate")
            .expect("bk_evaluate present");
        sb.invoke("evaluate", |_| f.call::<Value>(()))
            .expect("empty state is benign");
    }

    /// The entry-point contract is the loader's check list; the interval
    /// spelling is the single wire form.
    #[test]
    fn entry_points_and_interval_names_are_stable() {
        assert_eq!(ENTRY_POINTS[0], "bk_evaluate");
        assert_eq!(ENTRY_POINTS.len(), 4);
        assert_eq!(interval_name(KlineInterval::Min1), "min1");
        assert_eq!(interval_name(KlineInterval::Hour4), "hour4");
    }

    // ── #448: content-keyed projection cache + the read-only guard ──────────

    /// A cached table must read IDENTICALLY to a fresh render — same call,
    /// same content, second call hits the cache and still answers every field.
    #[test]
    fn cached_book_table_reads_identically_across_calls() {
        let (sb, st) = with_state();
        {
            let mut s = st.lock().expect("lock");
            s.put_book(book("0xup"), Some(true));
        }
        sb.load(
            "function fail(msg) error(msg, 0) end \
             local first, second \
             function bk_evaluate() \
                first = bk.book('0xup') \
                second = bk.book('0xup') \
                if second.best_bid ~= '0.55' or second.fresh ~= true or second.ts_ms ~= 1234 then \
                    fail('cached book lost a field') \
                end \
                return {} \
             end",
            "cache-hit",
        )
        .expect("loads");
        let f: mlua::Function = sb.lua().globals().get("bk_evaluate").expect("entry");
        sb.invoke("evaluate", |_| f.call::<Value>(()))
            .expect("cache hit reads cleanly");
    }

    /// Content change must re-render: put a different book, the next read
    /// answers the NEW numbers (a stale cache is a correctness bug, not a
    /// perf trade).
    #[test]
    fn book_content_change_rerenders_the_table() {
        let (sb, st) = with_state();
        {
            let mut s = st.lock().expect("lock");
            s.put_book(book("0xup"), Some(true));
        }
        sb.load(
            "function fail(msg) error(msg, 0) end \
             function bk_evaluate() \
                local b = bk.book('0xup') \
                if b.best_ask ~= '0.57' then fail('first render') end \
                return {} \
             end \
             function bk_evaluate_again() \
                local b = bk.book('0xup') \
                if b.best_ask ~= '0.61' or b.ts_ms ~= 9999 then fail('re-render') end \
                return {} \
             end",
            "rerender",
        )
        .expect("loads");
        let call = |name: &str| {
            let f: mlua::Function = sb.lua().globals().get(name).expect(name);
            sb.invoke(name, |_| f.call::<Value>(()))
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        };
        call("bk_evaluate");
        {
            let mut s = st.lock().expect("lock");
            let mut b2 = book("0xup");
            b2.best_ask = Some("0.61".into());
            b2.timestamp_ms = 9_999;
            s.put_book(b2, Some(true));
        }
        call("bk_evaluate_again");
    }

    /// `fresh` is part of the rendered content: the same book stamped
    /// fresh=false must re-render (the eval gate verdict is authoritative).
    #[test]
    fn fresh_verdict_change_rerenders_the_book() {
        let (sb, st) = with_state();
        {
            let mut s = st.lock().expect("lock");
            s.put_book(book("0xup"), Some(true));
        }
        sb.load(
            "function fail(msg) error(msg, 0) end \
             function bk_evaluate() \
                if bk.book('0xup').fresh ~= true then fail('first') end \
                return {} \
             end \
             function bk_evaluate_again() \
                if bk.book('0xup').fresh ~= false then fail('fresh flip') end \
                return {} \
             end",
            "fresh",
        )
        .expect("loads");
        let call = |name: &str| {
            let f: mlua::Function = sb.lua().globals().get(name).expect(name);
            sb.invoke(name, |_| f.call::<Value>(()))
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        };
        call("bk_evaluate");
        {
            let mut s = st.lock().expect("lock");
            s.put_book(book("0xup"), Some(false));
        }
        call("bk_evaluate_again");
    }

    /// The read-only guard: a hostile write to a projection table must error
    /// LOUDLY (with the cache it would otherwise leak across calls), and the
    /// metatable must be hidden from `getmetatable` probes.
    #[test]
    fn projection_tables_refuse_writes_loudly() {
        let (sb, st) = with_state();
        {
            let mut s = st.lock().expect("lock");
            s.put_book(book("0xup"), Some(true));
        }
        sb.load(
            "function fail(msg) error(msg, 0) end \
             function bk_evaluate() \
                local b = bk.book('0xup') \
                local ok, err = pcall(function() b.best_bid = '0.99' end) \
                if ok then fail('write to bk.book succeeded') end \
                if not tostring(err):find('read%-only') then fail('wrong error: ' .. tostring(err)) end \
                if getmetatable(b) ~= 'bk projection' then fail('metatable leaked') end \
                local ok2 = pcall(function() bk.params().x = 'y' end) \
                if ok2 then fail('write to bk.params succeeded') end \
                local ok3 = pcall(function() bk.markets()[1] = nil end) \
                if ok3 then fail('write to bk.markets succeeded') end \
                return {} \
             end",
            "guard",
        )
        .expect("loads");
        let f: mlua::Function = sb.lua().globals().get("bk_evaluate").expect("entry");
        sb.invoke("evaluate", |_| f.call::<Value>(()))
            .expect("guard contract holds");
    }

    // ── ladder exposure + vwap_for_size (书视图前 10 档 / 深度感知成本面) ────────

    /// `bk.book` 暴露 `bid_ladder`/`ask_ladder`：top-N、best-first、decimal
    /// STRING。空侧 → 空表，从不造数据。
    #[test]
    fn book_exposes_top_ladders_as_decimal_strings() {
        let (sb, st) = with_state();
        {
            let mut s = st.lock().expect("lock");
            s.put_book(three_level_book("0xup"), Some(true));
        }
        sb.load(
        "function fail(msg) error(msg, 0) end \
         function bk_evaluate() \
            local b = bk.book('0xup') \
            if b == nil then fail('book') end \
            local bl, al = b.bid_ladder, b.ask_ladder \
            if type(bl) ~= 'table' or type(al) ~= 'table' then fail('ladders missing') end \
            if #al ~= 3 then fail('ask ladder depth: ' .. tostring(#al)) end \
            if al[1].price ~= '0.50' or al[1].size ~= '10' then fail('ask lvl1') end \
            if al[2].price ~= '0.52' or al[3].price ~= '0.56' then fail('ask order') end \
            if type(al[1].price) ~= 'string' or type(al[1].size) ~= 'string' then fail('strings only') end \
            return {} \
         end",
        "ladder",
    )
    .expect("loads");
        let f: mlua::Function = sb.lua().globals().get("bk_evaluate").expect("entry");
        sb.invoke("evaluate", |_| f.call::<Value>(()))
            .expect("ladder reads cleanly");
    }

    /// 空 book：ladder 是空表（不是 nil，不是造出来的档位）。
    #[test]
    fn empty_book_ladders_are_empty_tables() {
        let (sb, st) = with_state();
        {
            let mut s = st.lock().expect("lock");
            s.put_book(
                blitzkrieg_strategy_api::BookUpdate {
                    symbol: "x".into(),
                    ..Default::default()
                },
                None,
            );
        }
        sb.load(
            "function fail(msg) error(msg, 0) end \
         function bk_evaluate() \
            local b = bk.book('x') \
            if b == nil then fail('book missing') end \
            if type(b.bid_ladder) ~= 'table' or #b.bid_ladder ~= 0 then fail('bid ladder') end \
            if type(b.ask_ladder) ~= 'table' or #b.ask_ladder ~= 0 then fail('ask ladder') end \
            return {} \
         end",
            "empty-ladder",
        )
        .expect("loads");
        let f: mlua::Function = sb.lua().globals().get("bk_evaluate").expect("entry");
        sb.invoke("evaluate", |_| f.call::<Value>(()))
            .expect("empty ladders benign");
    }

    /// 三档合成书的加权均价：0.50×10 + 0.52×10 + 0.56×10 / 30 = 0.52666…，
    /// round_dp(10) → "0.5266666667"。逐字节断言。
    #[test]
    fn vwap_walks_three_levels_byte_exact() {
        let (sb, st) = with_state();
        {
            let mut s = st.lock().expect("lock");
            s.put_book(three_level_book("0xup"), Some(true));
        }
        sb.load(
            "function fail(msg) error(msg, 0) end \
         function bk_evaluate() \
            local r = bk.vwap_for_size('0xup', 'ask', '30') \
            if r == nil then fail('must fill 30') end \
            if r.complete ~= true then fail('complete') end \
            if r.filled ~= '30' then fail('filled: ' .. tostring(r.filled)) end \
            if r.vwap ~= '0.5266666667' then fail('vwap: ' .. tostring(r.vwap)) end \
            return {} \
         end",
            "vwap3",
        )
        .expect("loads");
        let f: mlua::Function = sb.lua().globals().get("bk_evaluate").expect("entry");
        sb.invoke("evaluate", |_| f.call::<Value>(()))
            .expect("vwap walks cleanly");
    }

    /// 部分跨档消耗：目标 15 → 全吃第一档 + 半吃第二档。
    /// (0.50×10 + 0.52×5)/15 = 7.6/15 = 0.50666…  → "0.5066666667"。
    #[test]
    fn vwap_partial_level_consumption_is_exact() {
        let (sb, st) = with_state();
        {
            let mut s = st.lock().expect("lock");
            s.put_book(three_level_book("0xup"), Some(true));
        }
        sb.load(
            "function fail(msg) error(msg, 0) end \
         function bk_evaluate() \
            local r = bk.vwap_for_size('0xup', 'ask', '15') \
            if r == nil or r.complete ~= true then fail('must fill 15') end \
            if r.vwap ~= '0.5066666667' then fail('vwap: ' .. tostring(r.vwap)) end \
            return {} \
         end",
            "vwap-partial",
        )
        .expect("loads");
        let f: mlua::Function = sb.lua().globals().get("bk_evaluate").expect("entry");
        sb.invoke("evaluate", |_| f.call::<Value>(()))
            .expect("partial consumption clean");
    }

    /// 红（核心契约）：第 3 档就断深度 —— 目标 40 > 全侧深度 30，必须返回 nil，
    /// 绝不许按 best ask 全量价假装可成交。
    #[test]
    fn vwap_returns_nil_when_depth_runs_out_at_third_level() {
        let (sb, st) = with_state();
        {
            let mut s = st.lock().expect("lock");
            s.put_book(three_level_book("0xup"), Some(true));
        }
        sb.load(
            "function fail(msg) error(msg, 0) end \
         function bk_evaluate() \
            local r = bk.vwap_for_size('0xup', 'ask', '40') \
            if r ~= nil then fail('must be nil when depth runs out, got ' .. tostring(r)) end \
            return {} \
         end",
            "vwap-short",
        )
        .expect("loads");
        let f: mlua::Function = sb.lua().globals().get("bk_evaluate").expect("entry");
        sb.invoke("evaluate", |_| f.call::<Value>(()))
            .expect("depth shortage answers nil");
    }

    /// 空 ladder（空侧/未知 symbol）→ nil，不是错误、不是 0。
    #[test]
    fn vwap_returns_nil_for_empty_ladder_and_unknown_symbol() {
        let (sb, st) = with_state();
        {
            let mut s = st.lock().expect("lock");
            s.put_book(BookUpdate::default(), None);
        }
        sb.load(
        "function fail(msg) error(msg, 0) end \
         function bk_evaluate() \
            if bk.vwap_for_size('x', 'ask', '10') ~= nil then fail('empty ladder must be nil') end \
            if bk.vwap_for_size('nope', 'ask', '10') ~= nil then fail('unknown symbol must be nil') end \
            return {} \
         end",
        "vwap-empty",
    )
    .expect("loads");
        let f: mlua::Function = sb.lua().globals().get("bk_evaluate").expect("entry");
        sb.invoke("evaluate", |_| f.call::<Value>(()))
            .expect("empty/unknown answers nil");
    }

    /// 策略 bug（side 拼错 / size 非数 / size 非正）→ LOUD Lua error，
    /// 绝不静默 nil。
    #[test]
    fn vwap_caller_bugs_error_loudly() {
        let (sb, st) = with_state();
        {
            let mut s = st.lock().expect("lock");
            s.put_book(three_level_book("0xup"), Some(true));
        }
        sb.load(
            "function fail(msg) error(msg, 0) end \
         function try(side, size) \
            local ok, err = pcall(bk.vwap_for_size, '0xup', side, size) \
            if ok then fail('must error for ' .. tostring(side) .. '/' .. tostring(size)) end \
            return err \
         end \
         function bk_evaluate() \
            local e1 = try('middle', '10') \
            if not tostring(e1):find('bid') then fail('side error: ' .. tostring(e1)) end \
            local e2 = try('ask', 'abc') \
            if not tostring(e2):find('decimal') then fail('size error: ' .. tostring(e2)) end \
            local e3 = try('ask', '0') \
            if not tostring(e3):find('positive') then fail('zero error: ' .. tostring(e3)) end \
            return {} \
         end",
            "vwap-bugs",
        )
        .expect("loads");
        let f: mlua::Function = sb.lua().globals().get("bk_evaluate").expect("entry");
        sb.invoke("evaluate", |_| f.call::<Value>(()))
            .expect("caller bugs fail loudly");
    }

    /// 死穴守卫（2026-09-30 28 位小数事故的重演面）：档位里塞一个 mantissa
    /// 会溢出 rust_decimal 的坏行 → 必须显式 Lua error 指名 symbol，绝不静默
    /// nil 崩溃、绝不按 best 价回退。
    #[test]
    fn vwap_corrupt_ladder_row_errors_loudly_not_silently() {
        let (sb, st) = with_state();
        {
            let mut s = st.lock().expect("lock");
            let mut b = three_level_book("0xup");
            // 第二档是坏的：28 位小数（rust_decimal 解析直接拒绝的域外输入）。
            b.ask_ladder[1] = ("0.5200000000000000000000000000".into(), "10".into());
            s.put_book(b, Some(true));
        }
        sb.load(
        "function fail(msg) error(msg, 0) end \
         function bk_evaluate() \
            local ok, err = pcall(bk.vwap_for_size, '0xup', 'ask', '30') \
            if ok then fail('corrupt row must error, got a price') end \
            if not tostring(err):find('0xup') then fail('error must name the symbol: ' .. tostring(err)) end \
            return {} \
         end",
        "vwap-corrupt",
    )
    .expect("loads");
        let f: mlua::Function = sb.lua().globals().get("bk_evaluate").expect("entry");
        sb.invoke("evaluate", |_| f.call::<Value>(()))
            .expect("corrupt ladder errors loudly");
    }

    /// Rust 侧直接验证 vwap_for_size 的算术域：checked 乘加 + round_dp(10)，
    /// 并确认坏行（无法解析）返回 None（供宿主转 LOUD error）。
    #[test]
    fn vwap_for_size_rust_unit_contract() {
        let d = |s: &str| Decimal::from_str_exact(s).expect("decimal literal");
        let ladder = vec![
            ("0.50".into(), "10".into()),
            ("0.52".into(), "10".into()),
            ("0.56".into(), "10".into()),
        ];
        let walk = vwap_for_size(&ladder, d("30")).expect("fills");
        assert_eq!(walk.vwap.to_string(), "0.5266666667");
        assert_eq!(walk.filled.to_string(), "30");
        assert!(walk.complete);

        let short = vwap_for_size(&ladder, d("40")).expect("parses");
        assert!(!short.complete, "depth shortage is reported, not faked");
        assert_eq!(short.filled.to_string(), "30");

        let bad = vec![("0.5".into(), "1e5".into())];
        assert!(
            vwap_for_size(&bad, d("1")).is_none(),
            "bad row = None (loud upstream)"
        );

        // 28 位小数档位（2026-09-30 事故的域）：合法 rust_decimal，但超出
        // Lua 侧 18 位有效数字安全域 —— 域闸拒绝，绝不进入算术。
        let wide = vec![("0.5200000000000000000000000000".into(), "10".into())];
        assert!(
            vwap_for_size(&wide, d("5")).is_none(),
            "28-dp level = outside the safe domain = None (loud upstream)"
        );

        // checked 算术的第二道网：域内的两个档位，名义额 5e18 × 2e10 = 1e29
        // 越过 rust_decimal 96 位 mantissa 上限（~7.9e28）→ checked_mul 拒绝。
        let huge = vec![("5000000000000000000".into(), "20000000000".into())];
        assert!(
            vwap_for_size(&huge, d("10000000000000")).is_none(),
            "overflow = None (loud upstream), never a wrapped price"
        );
    }
}
