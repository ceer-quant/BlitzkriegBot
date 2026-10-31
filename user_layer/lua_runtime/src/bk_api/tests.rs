use blitzkrieg_strategy_api::{BookUpdate, Kline, KlineInterval, MarketInfo};
use mlua::Value;
use rust_decimal::Decimal;
use std::sync::{Arc, Mutex};

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
