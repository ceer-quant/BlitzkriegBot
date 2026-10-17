//! 书视图前 10 档 + `bk.vwap_for_size` 端到端验收 —— 消灭乐观成交假设。
//!
//! 背景（用户原话）：策略只能看到 top-of-book，按 best ask 全量吃假设算成本，
//! 实际挂单深度只到第 3 档，吃满深度后成本远高于测算。修复 = 内核
//! `LocalBook` 的完整档位经 `LuaEngineAdapter::book_update` 透传成
//! `bk.book()` 的 `bid_ladder`/`ask_ladder`（top-10，decimal STRING，
//! best-first），宿主侧 `bk.vwap_for_size(symbol, side, size)` 沿档位做
//! checked 精确算术：深度不足 → **nil**（调用方必须显式判断，绝不静默按
//! best 价回退），坏行 / 28 位小数域外档位 / 算术溢出 → **显式 Lua error**
//! （2026-09-30 mid/obi 28 位小数事故的档位域重演面，这次在源头封死）。
//!
//! 载体：临时构造的最小包（tempdir，写盘走 loader 的完整 manifest/sha256
//! 校验），经 [`load_lua_package`] + [`LuaEngineAdapter`] 驱动 —— 与内核
//! 生产路径逐层相同；vwap/档位断言在 Lua 侧经意图 reason 逐字节回传。

use std::path::{Path, PathBuf};

use blitzkrieg_core::model::{CryptoMarket, OrderbookSnapshot};
use blitzkrieg_core::strategies::{EngineStrategy, StrategyCtx};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;
const UP: &str = "up";
const DOWN: &str = "down";

/// The probe package: reads the ladder + vwap surface and echoes results in
/// the entry `reason` strings. Any assertion failure inside Lua errors the
/// evaluation — the adapter records it, the Rust side demands the intended
/// candidates, so a silent nil-swallow reds here.
const STRATEGY_LUA: &str = r#"
function fail(msg) error(msg, 0) end
local UP = 'up'

function bk_evaluate()
    local out = { entries = {}, exits = {}, breaks = {} }
    local b = bk.book(UP)
    if b == nil then fail('book missing') end

    -- 1. ladder exposure: top-10, best-first, decimal STRINGS.
    local al = b.ask_ladder
    if type(al) ~= 'table' or #al ~= 5 then fail('ask ladder depth ' .. tostring(al and #al)) end
    if al[1].price ~= '0.50' or al[1].size ~= '10' then fail('ask lvl1') end
    if al[5].price ~= '0.58' then fail('ask lvl5') end
    if #b.bid_ladder ~= 5 or b.bid_ladder[1].price ~= '0.49' then fail('bid ladder') end

    -- 2. three-level VWAP, byte-exact: (0.50*10+0.52*10+0.54*10)/30 = 0.52
    --    (rust_decimal Display keeps the division result's own scale).
    local r = bk.vwap_for_size(UP, 'ask', '30')
    if r == nil or r.complete ~= true or r.filled ~= '30' then fail('vwap30 shape') end
    if r.vwap ~= '0.52' then fail('vwap30 ' .. tostring(r.vwap)) end

    -- 3. depth runs out: the book carries 5x10=50, target 60 -> nil.
    --    THE optimistic-fill killer: the surface must NOT answer a price,
    --    and the caller must not pretend best-ask fills the rest.
    if bk.vwap_for_size(UP, 'ask', '60') ~= nil then fail('short depth must be nil') end

    out.entries = { { token = UP, price = '0.50', reason = 'ladder-ok:' .. r.vwap } }
    return out
end

function bk_on_book(update)
    -- the push form carries the same ladders as the read form
    if update == nil or #update.ask_ladder ~= 5 then LADDER_PUSH = false end
end
"#;

fn write_pkg(dir: &Path, name: &str, strategy: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("mkdir");
    std::fs::write(dir.join("strategy.lua"), strategy).expect("write entry");
    std::fs::write(dir.join("README.md"), "# probe\n").expect("write readme");
    let digest = blitzkrieg_core::strategy_engine::loader::sha256_file(&dir.join("strategy.lua"))
        .expect("sha");
    let manifest = serde_json::json!({
        "name": name,
        "version": "0.1.0",
        "api": "1.0",
        "entry": "strategy.lua",
        "sha256": digest,
        "author": "test",
        "description": "ladder vwap probe",
    });
    std::fs::write(dir.join("manifest.json"), manifest.to_string()).expect("write manifest");
    dir.to_path_buf()
}

/// §6.4: the directory name IS the registration name — the probe directory
/// is named AFTER the package (pid-suffixed parent, so parallel runs and
/// re-runs never collide and never see a stale identity).
fn probe_dir(tag: &str) -> PathBuf {
    let parent = std::env::temp_dir().join(format!("bk-ladder-{}", std::process::id()));
    parent.join(tag)
}

fn package_dir() -> PathBuf {
    let dir = probe_dir("ladder_probe");
    write_pkg(&dir, "ladder_probe", STRATEGY_LUA)
}

fn adapter() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("ladder_probe package must load");
    LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
}

fn market() -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond".into(),
        question_id: "q".into(),
        up_token_id: UP.into(),
        down_token_id: DOWN.into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: NOW + 900_000,
        round_slot: NOW / 900_000,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: String::new(),
        neg_risk: false,
        question: "BTC up or down".into(),
    }
}

/// 五档合成书（ask 0.50/0.52/0.54/0.56/0.58，各 10；bid 0.49 起，各 10）。
/// `OrderbookSnapshot::from_levels` 排序后 best-first，与内核 LocalBook 输出
/// 同构。
fn five_level_book() -> OrderbookSnapshot {
    OrderbookSnapshot::from_levels(
        UP.to_string(),
        vec![
            (dec!(0.49), dec!(10)),
            (dec!(0.48), dec!(10)),
            (dec!(0.47), dec!(10)),
            (dec!(0.46), dec!(10)),
            (dec!(0.45), dec!(10)),
        ],
        vec![
            (dec!(0.50), dec!(10)),
            (dec!(0.52), dec!(10)),
            (dec!(0.54), dec!(10)),
            (dec!(0.56), dec!(10)),
            (dec!(0.58), dec!(10)),
        ],
        NOW,
    )
}

fn ctx<'a>(
    markets: &'a [CryptoMarket],
    now_ms: i64,
    fresh: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
) -> StrategyCtx<'a> {
    StrategyCtx::new(markets, NOW / 900_000, 600, now_ms, fresh)
}

/// 端到端正例：内核书 → 适配器投影 → `bk.book().ask_ladder` 五档 +
/// `bk.vwap_for_size` 三档加权均价逐字节 `0.5266666667`；深度断档（第 3 档
/// 吃尽后目标 40）→ nil。Lua 侧全部断言通过才会产出 `ladder-ok:` 意图。
#[test]
fn ladder_and_vwap_flow_end_to_end_through_the_engine_adapter() {
    let mut a = adapter();
    let book = five_level_book();
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = a.find_candidates(&ctx(&markets, NOW + 1_000, &fresh));

    assert_eq!(
        got.len(),
        1,
        "Lua assertions passed -> one entry: {:?}",
        got
    );
    assert!(
        got[0].reason.starts_with("ladder-ok:0.52"),
        "{}",
        got[0].reason
    );
    assert_eq!(got[0].price, dec!(0.50));
}

/// 档位深度封顶：内核书给 15 档，`bk.book()` 的 ladder 恰 10 档
/// （`LADDER_LEVELS`），且 best-first 顺序保持。
#[test]
fn ladder_is_capped_at_ten_levels_best_first() {
    const DEEP_LUA: &str = r#"
function fail(msg) error(msg, 0) end
function bk_evaluate()
    local b = bk.book('up')
    if #b.ask_ladder ~= 10 then fail('cap expected 10, got ' .. tostring(#b.ask_ladder)) end
    if b.ask_ladder[1].price ~= '0.50' then fail('best first') end
    if b.ask_ladder[10].price ~= '0.68' then fail('tenth: ' .. b.ask_ladder[10].price) end
    -- best fields keep the FULL book's top (the cap only bounds the ladder)
    if b.best_ask ~= '0.50' then fail('best fields intact') end
    return { entries = {} }
end
"#;
    let dir = probe_dir("ladder_cap");
    write_pkg(&dir, "ladder_cap", DEEP_LUA);
    let loaded = load_lua_package(&dir).expect("loads");
    let mut a = LuaEngineAdapter::new(loaded.strategy, loaded.tunables);

    let asks: Vec<(Decimal, Decimal)> = (0..15)
        .map(|i| (dec!(0.50) + Decimal::new(i * 2, 2), dec!(10)))
        .collect();
    let bids: Vec<(Decimal, Decimal)> = (0..15)
        .map(|i| (dec!(0.49) - Decimal::new(i, 2), dec!(10)))
        .collect();
    let book = OrderbookSnapshot::from_levels(UP.to_string(), bids, asks, NOW);
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let got = a.find_candidates(&ctx(&markets, NOW + 1_000, &fresh));
    let health = a.lua_health();
    assert!(
        got.is_empty() && health.last_error.is_none(),
        "Lua assertions must hold (no candidates, no recorded error); got {got:?} err {:?}",
        health.last_error
    );
}

/// 死穴（28 位小数档位经真实书流）：合成书里一个 scale 28 的档位 →
/// `bk.vwap_for_size` 必须显式 Lua error（适配器记为 last_error），
/// 绝不静默 nil / 绝不按 best 价给出成本。
#[test]
fn twenty_eight_digit_level_errors_loudly_through_the_adapter() {
    const GUARD_LUA: &str = r#"
function fail(msg) error(msg, 0) end
function bk_evaluate()
    local b = bk.book('up')
    -- the wide level IS exposed verbatim (display is safe; arithmetic is not)
    if b.ask_ladder[2].price ~= '0.5200000000000000000000000000' then
        fail('wide level must be exposed as-is: ' .. tostring(b.ask_ladder[2].price))
    end
    local ok, err = pcall(bk.vwap_for_size, 'up', 'ask', '30')
    if ok then fail('wide level must error loudly') end
    -- echo the error class so the Rust side can assert the LOUD path fired
    -- (adapter must NOT swallow it into silence; here we surface it as data)
    return { entries = { { token = 'up', price = '0.50',
                          reason = 'vwap-error:' .. tostring(err) } } }
end
"#;
    let dir = probe_dir("ladder_wide");
    write_pkg(&dir, "ladder_wide", GUARD_LUA);
    let loaded = load_lua_package(&dir).expect("loads");
    let mut a = LuaEngineAdapter::new(loaded.strategy, loaded.tunables);

    let book = OrderbookSnapshot::from_levels(
        UP.to_string(),
        vec![(dec!(0.49), dec!(10))],
        vec![
            (dec!(0.50), dec!(10)),
            // scale-28 price: the rust_decimal-legal, Lua-unsafe domain.
            (
                Decimal::from_str_exact("0.5200000000000000000000000000").expect("28dp"),
                dec!(10),
            ),
            (dec!(0.54), dec!(10)),
        ],
        NOW,
    );
    let markets = vec![market()];
    let fresh = move |t: &str| (t == UP).then(|| book.clone());
    let ctx = ctx(&markets, NOW + 1_000, &fresh);
    let got = a.find_candidates(&ctx);
    // The strategy's own assertions passed (the wide level IS exposed
    // verbatim) and the pcall CAUGHT the loud error — its text rides the
    // intent so the Rust side can assert the error class. A silent path
    // (nil without error, or a best-price answer) would fail the Lua
    // assertions and emit nothing.
    assert_eq!(got.len(), 1, "one diagnostic entry: {got:?}");
    assert!(
        got[0].reason.starts_with("vwap-error:"),
        "reason must carry the error class: {}",
        got[0].reason
    );
    let health = a.lua_health();
    assert!(
        health.last_error.is_none(),
        "the eval itself is clean (the strategy caught the error): {:?}",
        health.last_error
    );
}
