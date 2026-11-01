-- smart_money_reverse self-check — pure Lua (5.4+), zero dependencies.
--
-- Drives the REAL strategy.lua through a stub `bk` surface (the §6.5 wire
-- shape: decimal strings, fresh flags, host-pushed holdings) and asserts the
-- rows the README promises:
--   scenario A positive — crash arms, quiet + thickened depth stabilize, ONE
--               entry on the CRASHED leg at its own best_bid (post-only
--               shape: strictly under the ask, shares omitted); holding +
--               mid under the rebound anchor → ONE kernel-priced
--               thesis_reversed exit, latched for the round;
--   scenario A negative — thin depth (knife still falling) refuses, a fresh
--               low restarts the quiet clock, a fresh low after
--               stabilization breaks the resting bid then re-arms, the spot
--               waterfall veto and the spot_missing=block default refuse,
--               below min_time_left_sec refuses (breaks still drain), a new
--               round retires every latch;
--   scenario B — fade FOMO fires only when fomo_enabled="on", bids the
--               OPPOSITE leg at fomo_fade_price, and a dead ramp breaks the
--               fade bid;
--   seal      — every other phase of the round emits no exits, and no row
--               ever carries shares or an exit price.
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

local checks, failures = 0, {}

local function ok(cond, name)
  checks = checks + 1
  if not cond then failures[#failures + 1] = name end
end

-- ── the stub bk surface (§6.5 shapes) ───────────────────────────────────────
-- state = { now, slot, time_left, params, books = { [tok] = book },
--           holdings = { { token_id=, ... }, ... } }

local T0 = 1700000000000

local state = {
  now = T0, slot = 1, time_left = 900,
  params = {},
  books = {}, holdings = {},
}

local bk = {}
bk.now_ms = function() return state.now end
bk.round = function()
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
bk.holdings = function()
  local out = {}
  for i, h in ipairs(state.holdings) do
    out[i] = {
      strategy = "smart_money_reverse", condition_id = "c1",
      token_id = h.token_id, direction = h.direction or "up",
      shares = h.shares or "1.00", entry_price = h.entry_price or "0.35",
      round_slot = state.slot,
    }
  end
  return out
end

-- the strategy is PERSISTENT across evaluations within a row (latches must
-- survive); a row starts from reset(). strategy.lua defines GLOBALS the way
-- the kernel loads it — grab them off _ENV and strip them again.
local S
local function reset()
  S = real_loadfile(here .. "/strategy.lua")()
  if type(S) ~= "table" then
    S = {
      bk_evaluate = _ENV.bk_evaluate,
      bk_on_book = _ENV.bk_on_book,
      bk_on_round = _ENV.bk_on_round,
      bk_on_kline = _ENV.bk_on_kline,
    }
  end
  _ENV.bk_evaluate = nil
  _ENV.bk_on_book = nil
  _ENV.bk_on_round = nil
  _ENV.bk_on_kline = nil
  S.bk = bk
  _ENV.bk = bk -- the strategy reads the GLOBAL bk inside its closures
  state.now = T0
  state.slot = 1
  state.time_left = 900
  state.params = { spot_missing = "pass" } -- book-only rows; the spot block
  -- default has its own dedicated rows below
  state.books = {}
  state.holdings = {}
end

local function book_row(sym, mid_num, depth, fresh)
  return {
    symbol = sym,
    best_bid = string.format("%.2f", mid_num - 0.01),
    best_ask = string.format("%.2f", mid_num + 0.01),
    mid = string.format("%.2f", mid_num),
    obi = "0", spread = "0.02",
    bid_depth = tostring(depth), ask_depth = tostring(depth),
    ts_ms = state.now, fresh = fresh ~= false,
  }
end

--- Advance the clock (t_ms = ms since T0 unless absolute) and feed a book.
local function feed(tok, mid_num, depth, t_ms)
  if t_ms then state.now = T0 + t_ms end
  local b = book_row(tok, mid_num, depth or 900)
  state.books[tok] = b
  S.bk_on_book(b)
end

local function feed_spot(closes, start_ms, sym)
  for i = 1, #closes do
    S.bk_on_kline({
      interval = "sec1", is_closed = true, close = closes[i],
      close_time_ms = T0 + start_ms + (i - 1) * 1000,
      symbol = sym or "BTC",
    })
  end
end

local function evaluate() return S.bk_evaluate() end

--- The canonical scenario-A arm: 0.50 → 0.36 (28% inside the window), thin
--- crash depth 400, quiet until t=14s, thickened depth 700 (≥ 1.5×400 and
--- ≥ 500). Leaves the state STABILIZED at mid 0.36 with t=14s on the clock.
local function arm_crash()
  feed("up-1", 0.50, 900, 0)
  feed("up-1", 0.36, 400, 5000)
  feed("up-1", 0.36, 400, 8000)
  feed("up-1", 0.36, 700, 14000)
end

-- ═══ SCENARIO A — POSITIVE ══════════════════════════════════════════════════

-- the happy path: arm → stabilize → ONE post-only maker bid on the crashed
-- leg at its own best_bid.
do
  reset()
  arm_crash()
  local r = evaluate()
  ok(#r.entries == 1, "stabilized crash fires exactly one entry, got " .. #r.entries)
  local e = r.entries[1]
  ok(e ~= nil and e.token == "up-1", "entry buys the CRASHED leg (up-1)")
  ok(e ~= nil and e.price == "0.35", "entry rests at best_bid 0.35 (offset 0), got "
    .. tostring(e and e.price))
  ok(e ~= nil and tonumber(e.price) < 0.37,
    "entry is strictly under the ask (post-only shape)")
  ok(e ~= nil and e.shares == nil, "shares omitted — the kernel sizes")
  ok(e ~= nil and e.reason:find("crashed", 1, true) ~= nil,
    "reason names the trigger shape")
end

-- thesis verdict: holding + mid > 3% under the anchor → ONE exits row, no
-- price field, latched for the round (and the dead bid is broken).
do
  reset()
  arm_crash()
  ok(#evaluate().entries == 1, "enter at T-886")
  state.holdings = { { token_id = "up-1" } }
  feed("up-1", 0.36, 700, 15000)
  local r = evaluate()
  ok(#r.exits == 0 and #r.entries == 0,
    "holding at the anchor: no verdict yet, no pyramid either")
  feed("up-1", 0.34, 700, 16000) -- fresh low: bid broken AND thesis reversed
  r = evaluate()
  ok(#r.exits == 1, "mid > 3% under the anchor emits ONE thesis_reversed row")
  local x = r.exits[1]
  ok(x ~= nil and x.token == "up-1", "the verdict names the held token")
  ok(x ~= nil and x.price == nil, "the verdict carries NO price (kernel prices it)")
  ok(x ~= nil and x.reason:find("thesis_reversed", 1, true) ~= nil,
    "reason names the thesis shape")
  ok(#r.breaks == 1 and r.breaks[1].token == "up-1"
    and r.breaks[1].broken_price == "0.35",
    "the fresh low also breaks the resting bid at its own price")
  feed("up-1", 0.30, 700, 17000)
  r = evaluate()
  ok(#r.exits == 0, "the verdict is latched: no second exit this round")
  ok(#r.entries == 0, "a thesis-reversed round never re-enters")
end

-- the anchor survives an unconfirm: a fill plus a fresh low still produces
-- the verdict (the exit watch reads the anchor, not the confirm latch).
do
  reset()
  arm_crash()
  evaluate()
  state.holdings = { { token_id = "up-1" } }
  feed("up-1", 0.34, 700, 16000)
  local r = evaluate()
  ok(#r.exits == 1, "fill + fresh low in one step: the verdict still fires")
end

-- ═══ SCENARIO A — NEGATIVE ══════════════════════════════════════════════════

-- the knife is still falling: depth never thickens → no bid, ever.
do
  reset()
  feed("up-1", 0.50, 900, 0)
  feed("up-1", 0.36, 400, 5000)
  feed("up-1", 0.36, 400, 14000)
  ok(#evaluate().entries == 0,
    "thin depth (400 < 1.5x400 and < 500) refuses — no knife catching")
end

-- a fresh low restarts the quiet clock.
do
  reset()
  feed("up-1", 0.50, 900, 0)
  feed("up-1", 0.36, 400, 5000)
  feed("up-1", 0.33, 700, 10000)
  local r = evaluate() -- now = T+10s, quiet only 0s since the 0.33 low
  ok(#r.entries == 0, "a fresh low restarts the settle clock")
  feed("up-1", 0.33, 700, 19000) -- 9s quiet, depth 700 ≥ 600
  r = evaluate()
  ok(#r.entries == 1, "quiet again: the crash re-stabilizes and fires")
  ok(r.entries[1].token == "up-1" and r.entries[1].price == "0.32",
    "re-stabilized entry tracks the new best_bid")
end

-- a fresh low AFTER stabilization breaks the resting bid, then re-arms.
do
  reset()
  arm_crash()
  ok(#evaluate().entries == 1, "stabilize and bid at 0.35")
  feed("up-1", 0.30, 700, 16000)
  local r = evaluate()
  ok(#r.breaks == 1 and r.breaks[1].token == "up-1"
    and r.breaks[1].broken_price == "0.35",
    "fresh low after stabilization breaks the stale bid")
  ok(#r.entries == 0, "the broken thesis does not bid in the same cycle")
  feed("up-1", 0.30, 700, 25000) -- quiet 9s from the 0.30 low, depth ok
  r = evaluate()
  ok(#r.entries == 1 and r.entries[1].price == "0.29",
    "the crash re-arms from the new low and re-stabilizes")
end

-- the spot waterfall veto (default spot_missing="block", data present):
-- BTC down 4% over the window vetoes buying the UP leg.
do
  reset()
  state.params = {}
  local closes = {}
  for i = 0, 10 do closes[#closes + 1] = string.format("%.3f", 0.500 - i * 0.002) end
  feed("up-1", 0.50, 900, 0)
  feed("up-1", 0.36, 400, 5000)
  feed("up-1", 0.36, 700, 14000)
  -- bars covering the 10s window AT evaluate time (T+4s .. T+14s)
  feed_spot(closes, 4000)
  ok(#evaluate().entries == 0,
    "a live spot waterfall vetoes the UP buy even when stabilized")
end

-- calm spot bars let the same shape through.
do
  reset()
  state.params = {}
  local closes = {}
  for i = 0, 10 do closes[#closes + 1] = string.format("%.3f", 0.500 + i * 0.0001) end
  feed("up-1", 0.50, 900, 0)
  feed("up-1", 0.36, 400, 5000)
  feed("up-1", 0.36, 700, 14000)
  feed_spot(closes, 4000) -- +0.22%: calm
  ok(#evaluate().entries == 1, "calm spot: the stabilized crash fires")
end

-- spot_missing default "block" with NO data: refuse (fail closed).
do
  reset()
  state.params = {}
  arm_crash()
  ok(#evaluate().entries == 0,
    "no spot data and spot_missing=block: no bid (cannot confirm calm)")
end

-- below min_time_left_sec: no NEW entries, but breaks still drain.
do
  reset()
  arm_crash()
  ok(#evaluate().entries == 1, "bid armed inside the window")
  feed("up-1", 0.30, 700, 16000)
  state.time_left = 100
  local r = evaluate()
  ok(#r.entries == 0, "inside the force-exit window: no new bids")
  ok(#r.breaks == 1, "breaks still drain inside the window")
  state.holdings = { { token_id = "up-1" } }
  feed("up-1", 0.20, 700, 17000)
  r = evaluate()
  ok(#r.exits == 1, "the verdict is NOT time-gated: it fires at T-99 too")
end

-- a new round slot retires every latch.
do
  reset()
  arm_crash()
  evaluate()
  S.bk_on_round({ slot = 2 })
  state.slot = 2
  feed("up-1", 0.36, 700, 15000)
  local r = evaluate()
  ok(#r.entries == 0 and #r.exits == 0 and #r.breaks == 0,
    "a new round retires the crash, the anchor and every latch")
end

-- stale books are not prices.
do
  reset()
  arm_crash()
  state.books["up-1"].fresh = false
  ok(#evaluate().entries == 0, "a stale book refuses to price an entry")
end

-- ═══ SCENARIO B — FADE FOMO (default OFF) ═══════════════════════════════════

-- default off: a violent ramp bids nothing.
do
  reset()
  feed("up-1", 0.60, 900, 0)
  feed("up-1", 0.90, 900, 25000)
  feed("down-1", 0.10, 900, 26000)
  local r = evaluate()
  ok(#r.entries == 0, "fomo_enabled defaults to off: no fade bid")
end

-- on: the ramp fades the OPPOSITE leg at the fixed 0.10 price.
do
  reset()
  state.params = { spot_missing = "pass", fomo_enabled = "on" }
  feed("up-1", 0.60, 900, 0)
  feed("up-1", 0.90, 900, 25000) -- +50% inside 30s, mid ≥ 0.85
  feed("down-1", 0.10, 900, 26000)
  local r = evaluate()
  ok(#r.entries == 1, "fomo fade fires exactly one entry")
  local e = r.entries[1]
  ok(e ~= nil and e.token == "down-1", "the fade bids the OPPOSITE leg")
  ok(e ~= nil and e.price == "0.10", "the fade rests at fomo_fade_price")
  ok(e ~= nil and tonumber(e.price) < 0.11, "the fade never crosses the ask")
  -- the ramp dies: the stale fade bid is cancelled on the opposite leg.
  feed("up-1", 0.80, 900, 27000)
  r = evaluate()
  ok(#r.breaks == 1 and r.breaks[1].token == "down-1"
    and r.breaks[1].broken_price == "0.10",
    "a dead ramp breaks the fade bid on the opposite leg")
  ok(#r.entries == 0, "no fade re-bid after the ramp died")
end

-- a crashed leg wins over a fade bid on the same token: scenario A owns it.
do
  reset()
  state.params = { spot_missing = "pass", fomo_enabled = "on" }
  feed("up-1", 0.60, 900, 0)
  feed("up-1", 0.90, 900, 25000)
  feed("down-1", 0.40, 900, 26000) -- baseline for the down book
  feed("down-1", 0.30, 400, 27000) -- down crashes 25% → scenario A arms
  feed("down-1", 0.30, 400, 30000)
  feed("down-1", 0.30, 700, 36000)
  local r = evaluate()
  ok(#r.entries == 1 and r.entries[1].token == "down-1"
    and r.entries[1].price == "0.29",
    "a crash-armed leg takes the scenario-A bid, not the fade price")
end

-- ═══ SEAL ═══════════════════════════════════════════════════════════════════

-- at every phase with no holding, no thesis verdict: exits stay empty; and
-- no entry row anywhere carries shares.
do
  local shapes = {
    { 0.50, 900, 0 }, { 0.36, 400, 5000 }, { 0.36, 700, 14000 },
  }
  for i, s in ipairs(shapes) do
    reset()
    feed("up-1", s[1], s[2], s[3])
    local r = evaluate()
    ok(#r.exits == 0, "shape " .. i .. ": no exits without a holding")
    for j = 1, #r.entries do
      ok(r.entries[j].shares == nil, "shape " .. i .. ": entries carry no shares")
      ok(r.entries[j].price ~= nil, "shape " .. i .. ": entries carry a price")
    end
  end
end

-- corrupt params fall back to the manifest defaults (nil-guard posture).
do
  reset()
  state.params = {
    crash_window_sec = "abc", crash_drop_pct = "xyz", settle_quiet_sec = "",
    spot_missing = "pass",
  }
  feed("up-1", 0.50, 900, 0)
  feed("up-1", 0.36, 400, 5000)
  feed("up-1", 0.36, 700, 14000)
  ok(#evaluate().entries == 1,
    "corrupt params fall back to defaults: the same shape still fires")
end

-- ── verdict ─────────────────────────────────────────────────────────────────
if #failures > 0 then
  real_io.stderr:write(string.format("selfcheck FAILED: %d/%d rows\n", #failures, checks))
  for _, f in ipairs(failures) do real_io.stderr:write("  - " .. f .. "\n") end
  real_os.exit(1)
end
real_print(string.format("selfcheck OK: %d rows", checks))
