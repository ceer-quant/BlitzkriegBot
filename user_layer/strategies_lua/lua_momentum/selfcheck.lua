-- lua_momentum self-check — pure Lua (5.4+), zero dependencies.
--
-- Drives the REAL strategy.lua through a stub `bk` surface (the §6.5 wire
-- shape: decimal strings, fresh flags) and asserts the direction surgery the
-- README promises:
--   direction gate — a crashing DOWN leg arms ONE entry at its own best_bid
--               (post-only shape: strictly under the ask, shares omitted);
--               the UP leg NEVER bids — not on its spike (the deleted
--               WR-7.5% leg), not on its crash (only the DOWN token is ever
--               buyable) — and a rebounding DOWN leg bids nothing;
--   premise    — a crashing DOWN leg arms ONE entry at its own best_bid
--               (post-only shape: strictly under the ask, shares omitted);
--               the UP leg NEVER bids — not on its spike (the deleted
--               WR-7.5% leg), not on its crash (only the DOWN token is ever
--               buyable) — and a rebounding DOWN leg bids nothing;
--   gates      — a stale book refuses to price, the force-exit window and
--               the between-rounds gap emit no entries;
--   seal       — flat (nothing held) the exit list stays empty;
--   exits      — a held winner is sold off the fresh BEST BID after the
--               taker fee, never the mid: take-profit, a price target, a
--               trailing giveback, an optional time exit; zeroing the knobs
--               restores hold-to-settlement; the opposite leg is refused
--               unless allow_both_sides is 1.
--
-- Run: lua selfcheck.lua   (exit 0 = all rows hold)

local here = (arg and arg[0]):match("^(.*)/[^/]+$") or "."
local real_loadfile, real_io, real_os, real_print =
  loadfile, io, os, print

-- the globals the sandbox pins to nil (self-check runs on plain Lua; emulate
-- the strategy's actual environment) — captured above first
_ENV.require = nil
_ENV.os = nil
_ENV.io = nil
_ENV.print = nil
_ENV._G = nil
_ENV.load = nil

local checks = 0
local failures = {}

local function ok(cond, name)
  checks = checks + 1
  if not cond then failures[#failures + 1] = name end
end

-- ── the stub bk surface (§6.5 shapes) ───────────────────────────────────────
-- state = { now, slot, time_left, params, round_open, books = { [tok] = row } }

local T0 = 1700000000000

local state = {
  now = T0, slot = 1, time_left = 900,
  params = {}, round_open = true,
  books = {},
  holdings = {},
  fees = nil,
}

local bk = {}
bk.now_ms = function() return state.now end
bk.round = function()
  if not state.round_open then return nil end
  return { slot = state.slot, time_left_sec = state.time_left, now_ms = state.now }
end
bk.markets = function()
  return {
    {
      asset = "BTC", condition_id = "c1",
      up_token = "up-1", down_token = "down-1",
      expires_at_ms = state.now + state.time_left * 1000,
      slot = state.slot, neg_risk = false,
    },
  }
end
bk.book = function(sym) return state.books[sym] end
bk.params = function() return state.params end
bk.holdings = function() return state.holdings end
bk.fees = function() return state.fees end

-- the strategy is PERSISTENT across evaluations within a row (last_mid and
-- confirmed must survive); a row starts from reset(). strategy.lua defines
-- GLOBALS the way the kernel loads it — grab them off _ENV and strip again.
-- ── helper functions (used by self-check tests) ───────────────────────────

local function hold(token, entry)
  state.holdings = {
    {
      token_id = token, entry_price = entry, shares = "10",
      direction = "down", condition_id = "c1", round_slot = 1,
    },
  }
end

local function quote(token, bid, ask, fresh)
  state.books[token] = {
    symbol = token,
    best_bid = bid, best_ask = ask,
    mid = nil,
    obi = "0", spread = "0.01",
    bid_depth = "900", ask_depth = "900",
    ts_ms = state.now, fresh = fresh ~= false,
  }
end

local function feed(tok, mid_num, t_ms, fresh)
  if t_ms then state.now = T0 + t_ms end
  local b = book_row(tok, mid_num, fresh)
  state.books[tok] = b
  S.bk_on_book(b)
end

local S
local function reset()
  S = real_loadfile(here .. "/strategy.lua")()
  if type(S) ~= "table" then
    S = {
      bk_evaluate = _ENV.bk_evaluate,
      bk_on_book = _ENV.bk_on_book,
      bk_on_round = _ENV.bk_on_round,
    }
  end
  _ENV.bk_evaluate = nil
  _ENV.bk_on_book = nil
  _ENV.bk_on_round = nil
  S.bk = bk
  _ENV.bk = bk -- the strategy reads the GLOBAL bk inside its closures
  state.now = T0
  state.slot = 1
  state.time_left = 900
  state.params = {}
  state.round_open = true
  state.books = {}
  state.holdings = {}
  state.fees = nil
end

local function book_row(sym, mid_num, fresh)
  return {
    symbol = sym,
    best_bid = string.format("%.2f", mid_num - 0.01),
    best_ask = string.format("%.2f", mid_num + 0.01),
    mid = string.format("%.2f", mid_num),
    obi = "0", spread = "0.02",
    bid_depth = "900", ask_depth = "900",
    ts_ms = state.now, fresh = fresh ~= false,
  }
end

-- ── Advance the clock (t_ms = ms since T0) and feed one leg's book row.
local function feed(tok, mid_num, t_ms, fresh)
  if t_ms then state.now = T0 + t_ms end
  local b = book_row(tok, mid_num, fresh)
  state.books[tok] = b
  S.bk_on_book(b)
end

local function evaluate() return S.bk_evaluate() end

-- ── local helpers for exit reasons -----------------------------------------

local function exit_reasons(r)
  local s = {}
  for i = 1, #r.exits do s[i] = r.exits[i].reason end
  return table.concat(s, ",")
end

-- ── #1123 入场门：min_time_left_for_entry ──────────────────────────────────

-- time_left_sec 低于安全余量时不开新仓（fail-closed：读不到时间也不开仓）。
do
  reset()
  state.params = { min_time_left_for_entry = "120" }
  state.time_left = 100 -- 不足安全余量
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000) -- −10% ≥ 4% threshold, premise confirmed
  local r = evaluate()
  ok(#r.entries == 0, "time_left 100 < min_time_left_for_entry 120: no new entry, got " .. #r.entries)
end

do
  reset()
  state.params = { min_time_left_for_entry = "120" }
  state.time_left = 200 -- 余量充足
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  local r = evaluate()
  ok(#r.entries == 1, "time_left 200 ≥ min_time_left_for_entry 120: entry offered, got " .. #r.entries)
end

-- 0 禁用入场门：余量充足时也仍出价（60s force-exit 窗口另行收口，故此处用 100s）。
do
  reset()
  state.params = { min_time_left_for_entry = "0" }
  state.time_left = 100
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  local r = evaluate()
  ok(#r.entries == 1, "min_time_left_for_entry 0: entry offered at time_left 100, got " .. #r.entries)
end

-- ── #1130 轮首禁入窗：entry_open_after_pct ──────────────────────────────────

-- 入场后前 entry_open_after_pct% 时间不开新仓（fail-closed，duration 用
-- round_duration_cache 动态种子，读不到时拒新入场）。只挡新开仓，不动
-- 已持仓退出。
do
  reset()
  state.params = { ["stop_loss_pct"] = "0", take_profit_pct = "0",
                   trail_arm_pct = "0", trail_giveback_pct = "0",
                   max_entry_price = "0", min_time_left_for_entry = "0" }
  state.time_left = 900
  hold("down-1", "0.28")
  quote("down-1", "0.42", "0.43")
  evaluate() -- 种 round_duration_cache = 900
  state.holdings = {} -- 清持仓，只测 entry 门

  -- 轮头 elapsed_frac = 0 < 0.20：拒新入场
  state.time_left = 900
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  local r = evaluate()
  ok(#r.entries == 0, "轮头 elapsed_frac=0 < 20%：拒新入场，got " .. #r.entries)

  -- elapsed_frac = 0.20 恰好边界：不拒（< 不包含等号）
  state.time_left = 720
  feed("down-1", 0.50, 0) -- 重置基准
  feed("down-1", 0.45, 2000) -- −10% 确认入场门
  local r = evaluate()
  ok(#r.entries == 1, "elapsed_frac=0.20 边界不拒：entry offered, got " .. #r.entries)

  -- 深轮尾 time_left=100（>60s force-exit 窗口，<720s 已过轮首）：允许入场
  state.time_left = 100
  feed("down-1", 0.50, 0) -- 重置基准
  feed("down-1", 0.45, 4000)
  local r = evaluate()
  ok(#r.entries == 1, "time_left=100 在轮首窗外：entry offered, got " .. #r.entries)
end

-- ── helper functions (used by self-check tests) ───────────────────────────

local function hold(token, entry)
  state.holdings = {
    {
      token_id = token, entry_price = entry, shares = "10",
      direction = "down", condition_id = "c1", round_slot = 1,
    },
  }
end

local function quote(token, bid, ask, fresh)
  state.books[token] = {
    symbol = token,
    best_bid = bid, best_ask = ask,
    mid = nil,
    obi = "0", spread = "0.01",
    bid_depth = "900", ask_depth = "900",
    ts_ms = state.now, fresh = fresh ~= false,
  }
end

local function feed(tok, mid_num, t_ms, fresh)
  if t_ms then state.now = T0 + t_ms end
  local b = book_row(tok, mid_num, fresh)
  state.books[tok] = b
  S.bk_on_book(b)
end

-- ── #1123 持仓门：unverifiable_window_sec ───────────────────────────────────