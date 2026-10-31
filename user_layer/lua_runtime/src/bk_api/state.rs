//! Host state snapshot and the Lua projection helpers behind `bk.*`.
//!
//! These are the table shapes both the adapter and the `bk` closures render,
//! so callback arguments and the read-only surface cannot drift.

use std::collections::HashMap;

use blitzkrieg_strategy_api::{
    BookUpdate, HoldingView, Kline, KlineInterval, MarketInfo, RoundInfo,
};
use mlua::{Lua, Table, Value};
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
