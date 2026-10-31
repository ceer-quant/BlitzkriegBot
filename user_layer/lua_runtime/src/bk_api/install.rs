//! Registration plumbing for the read-only `bk.*` globals.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use blitzkrieg_strategy_api::RoundInfo;
use mlua::{Function, Lua, Table, Value};
use rust_decimal::Decimal;

use super::state::*;

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
