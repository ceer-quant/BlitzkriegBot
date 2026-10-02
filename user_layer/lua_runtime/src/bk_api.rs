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
//! | `bk.markets()`    | array of round markets (`asset/condition_id/up_token/…`)   |
//! | `bk.book(sym)`    | one round token's book, decimal STRINGS, or `nil`          |
//! | `bk.params()`     | this strategy's hot parameters (`name → string value`)     |
//! | `bk.kline(sym,iv)`| the last CLOSED bar, or `nil` (E29's aggregator owns bars) |
//! | `bk.account()`    | public display fields only (§11.1: no credentials)         |
//! | `bk.fees()`       | the active taker-fee schedule (`name/rate/exponent`) or nil |
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
use std::sync::{Arc, Mutex};

use blitzkrieg_strategy_api::{BookUpdate, Kline, KlineInterval, MarketInfo, RoundInfo};
use mlua::{Lua, Table, Value};

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
/// spelling, §6.4 table). Lives here because both the `bk.kline` reader and
/// the `on_kline` pusher must agree on ONE spelling of "min1".
pub fn interval_name(interval: KlineInterval) -> &'static str {
    match interval {
        KlineInterval::Sec1 => "sec1",
        KlineInterval::Sec5 => "sec5",
        KlineInterval::Sec15 => "sec15",
        KlineInterval::Min1 => "min1",
        KlineInterval::Min5 => "min5",
        KlineInterval::Min15 => "min15",
        KlineInterval::Hour1 => "hour1",
        KlineInterval::Hour4 => "hour4",
        KlineInterval::Day1 => "day1",
    }
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
/// ask_depth/ts_ms/fresh`. `fresh` stamps the host's freshness verdict when
/// the caller has one (the eval gate); absent otherwise.
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
/// machine (each strategy owns its own `Lua`).
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
        bk.raw_set(
            "round",
            lua.create_function(move |lua, ()| {
                let st = st.lock().expect("bk state mutex");
                match st.round {
                    None => Ok(Value::Nil),
                    Some((slot, time_left_sec, now)) => Ok(Value::Table(round_to_lua(
                        lua,
                        &RoundInfo {
                            slot,
                            time_left_sec,
                            now_ms: now,
                        },
                    )?)),
                }
            })?,
        )?;
    }

    // -- bk.markets() -------------------------------------------------------
    {
        let st = Arc::clone(&state);
        bk.raw_set(
            "markets",
            lua.create_function(move |lua, ()| {
                let st = st.lock().expect("bk state mutex");
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
                    arr.raw_set(i as i64 + 1, t)?;
                }
                Ok(Value::Table(arr))
            })?,
        )?;
    }

    // -- bk.book(symbol) ----------------------------------------------------
    {
        let st = Arc::clone(&state);
        bk.raw_set(
            "book",
            lua.create_function(move |lua, symbol: String| {
                let st = st.lock().expect("bk state mutex");
                let Some(u) = st.books.get(&symbol) else {
                    return Ok(Value::Nil);
                };
                let fresh = st.fresh.get(&symbol).copied();
                Ok(Value::Table(book_to_lua(lua, u, fresh)?))
            })?,
        )?;
    }

    // -- bk.params() --------------------------------------------------------
    {
        let st = Arc::clone(&state);
        bk.raw_set(
            "params",
            lua.create_function(move |lua, ()| {
                let st = st.lock().expect("bk state mutex");
                let t = lua.create_table()?;
                for (k, v) in &st.params {
                    t.raw_set(k.as_str(), v.as_str())?;
                }
                Ok(Value::Table(t))
            })?,
        )?;
    }

    // -- bk.kline(symbol, interval) -----------------------------------------
    {
        let st = Arc::clone(&state);
        bk.raw_set(
            "kline",
            lua.create_function(move |lua, (symbol, interval): (String, String)| {
                let st = st.lock().expect("bk state mutex");
                let Some(k) = st.klines.get(&(symbol, interval)) else {
                    // E29: no aggregator behind this yet — "no data", not an error.
                    return Ok(Value::Nil);
                };
                Ok(Value::Table(kline_to_lua(lua, k)?))
            })?,
        )?;
    }

    // -- bk.account() -------------------------------------------------------
    {
        let st = Arc::clone(&state);
        bk.raw_set(
            "account",
            lua.create_function(move |lua, ()| {
                let st = st.lock().expect("bk state mutex");
                let Some(a) = &st.account else {
                    return Ok(Value::Nil);
                };
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
                Ok(Value::Table(t))
            })?,
        )?;
    }

    // -- bk.fees() ----------------------------------------------------------
    {
        let st = Arc::clone(&state);
        bk.raw_set(
            "fees",
            lua.create_function(move |lua, ()| {
                let st = st.lock().expect("bk state mutex");
                let Some(f) = &st.fee_schedule else {
                    // No schedule in force: "unknown", never "free". A strategy
                    // that prices fees must fail closed on nil.
                    return Ok(Value::Nil);
                };
                let t = lua.create_table()?;
                t.raw_set("name", f.name.as_str())?;
                t.raw_set("rate", f.rate.as_str())?;
                t.raw_set("exponent", f.exponent)?;
                Ok(Value::Table(t))
            })?,
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
        }
        sb.load(
            "function fail(msg) error(msg, 0) end \
             function bk_evaluate() \
                local r = bk.round() \
                if r == nil or r.slot ~= 42 then fail('round') end \
                if bk.now_ms() ~= 1234567 then fail('now') end \
                local ms = bk.markets() \
                if #ms ~= 1 or ms[1].up_token ~= '0xup' or ms[1].neg_risk ~= false then fail('markets') end \
                local b = bk.book('0xup') \
                if b == nil or b.best_bid ~= '0.55' or b.fresh ~= true or b.bid_levels ~= nil then fail('book') end \
                if bk.book('nope') ~= nil then fail('missing book') end \
                if bk.params().threshold ~= '0.03' then fail('params') end \
                local a = bk.account() \
                if a == nil or a.balance ~= '10000.00' or a.id ~= 'acct-1' then fail('account') end \
                local k = bk.kline('0xup', 'min1') \
                if k == nil or k.close ~= '0.55' or k.is_closed ~= true then fail('kline') end \
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
}
