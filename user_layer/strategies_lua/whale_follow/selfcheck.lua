-- whale_follow self-check — pure Lua (5.4+), zero dependencies.
--
-- Drives the REAL strategy.lua through a stub `bk` surface (the §6.5 wire
-- shape: decimal strings, fresh flags, fee schedule) and asserts the rows
-- the README promises:
--   positive — an up leg that established (0.50 -> 0.62), plateaued into the
--              entry window and sits at its round high fires exactly ONE
--              ask-priced entry on THAT leg; the mirrored down leg does the
--              same on its own token;
--   negative — a spike into the level (drift 0.25 > 0.10) refuses (the
--              anti-chase wall), early/late elapsed refuse, an ask above the
--              band refuses, an un-established level refuses, a pull-back
--              off the running high refuses, a stale book refuses, a
--              missing fee schedule refuses, a non-BTC market refuses, a
--              ring younger than the drift window refuses, a round longer
--              than round_sec refuses, a mid-round start (first sample at
--              elapsed 60s — no round-open reference) refuses, and two
--              conditions qualifying in the SAME round still leave exactly
--              ONE opening (the per-round cap is re-judged per market);
--   latches  — one attempt per condition per round, one opening per round,
--              a new round retires every latch;
--   seal     — exits and breaks are ALWAYS empty, entries carry no shares.
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
-- state = { now, slot, time_left, params, books = { [tok] = book }, fees,
--           asset, markets (override: two-market rows) }

local T0 = 1700000000000

local state = {
  now = T0, slot = 1, time_left = 300,
  params = {},
  books = {},
  fees = { name = "legacy_quadratic", rate = "0.125", exponent = 2 },
}

local bk = {}
bk.now_ms = function() return state.now end
bk.round = function()
  return { slot = state.slot, time_left_sec = state.time_left, now_ms = state.now }
end
bk.markets = function()
  return state.markets or {
    {
      asset = state.asset or "BTC", condition_id = "c1",
      up_token = "up-1", down_token = "down-1",
      expires_at_ms = state.now + state.time_left * 1000,
      slot = state.slot, neg_risk = false,
    },
  }
end
bk.book = function(sym) return state.books[sym] end
bk.params = function() return state.params end
bk.fees = function() return state.fees end

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
    }
  end
  _ENV.bk_evaluate = nil
  _ENV.bk_on_book = nil
  _ENV.bk_on_round = nil
  S.bk = bk
  _ENV.bk = bk -- the strategy reads the GLOBAL bk inside its closures
  state.now = T0
  state.slot = 1
  state.time_left = 300
  state.params = {}
  state.books = {}
  state.fees = { name = "legacy_quadratic", rate = "0.125", exponent = 2 }
  state.asset = "BTC"
  state.markets = nil
  -- the kernel calls on_round at every round switch — the stub emulates it
  -- before the row's books flow (the strategy clears its state there).
  S.bk_on_round({ slot = state.slot })
end

local function book_row(sym, mid_num)
  return {
    symbol = sym,
    best_bid = string.format("%.2f", mid_num - 0.01),
    best_ask = string.format("%.2f", mid_num + 0.01),
    mid = string.format("%.2f", mid_num),
    obi = "0", spread = "0.02",
    bid_depth = "900", ask_depth = "900",
    ts_ms = state.now, fresh = true,
  }
end

--- Advance the clock (t_ms = ms since T0) and feed BOTH legs (mirrored).
local function feed_pair(up_mid, t_ms)
  if t_ms then state.now = T0 + t_ms end
  local bu = book_row("up-1", up_mid)
  local bd = book_row("down-1", 1.0 - up_mid + 0.0)
  state.books["up-1"] = bu
  state.books["down-1"] = bd
  S.bk_on_book(bu)
  S.bk_on_book(bd)
end

--- Feed only the up leg (down book stays whatever it was).
local function feed_up(up_mid, t_ms)
  if t_ms then state.now = T0 + t_ms end
  local bu = book_row("up-1", up_mid)
  state.books["up-1"] = bu
  S.bk_on_book(bu)
end

--- Feed BOTH markets of a two-market state.markets the mirrored pair shape
--- (up_mid_a for the first market's up leg, up_mid_b for the second's).
local function feed_both_markets(up_mid_a, up_mid_b, t_ms)
  if t_ms then state.now = T0 + t_ms end
  local mids = { [state.markets[1].up_token] = up_mid_a,
                 [state.markets[2].up_token] = up_mid_b }
  for _, m in ipairs(state.markets) do
    local bu = book_row(m.up_token, mids[m.up_token])
    local bd = book_row(m.down_token, 1.0 - mids[m.up_token])
    state.books[m.up_token] = bu
    state.books[m.down_token] = bd
    S.bk_on_book(bu)
    S.bk_on_book(bd)
  end
end

local function evaluate() return S.bk_evaluate() end

-- ═══ POSITIVE — the established plateau fires exactly one ask entry ═════════

do
  reset()
  feed_pair(0.50, 0)
  feed_pair(0.56, 45000)
  feed_pair(0.62, 120000)
  state.time_left = 180 -- elapsed 120s of 300
  local r = evaluate()
  ok(#r.entries == 1, "established plateau fires exactly one entry, got " .. #r.entries)
  local e = r.entries[1]
  ok(e ~= nil and e.token == "up-1", "the entry buys the ESTABLISHED leg (up-1)")
  ok(e ~= nil and e.price == "0.63", "entry lifts the ask (0.63), got "
    .. tostring(e and e.price))
  ok(e ~= nil and e.shares == nil, "shares omitted — the kernel sizes")
  ok(e ~= nil and e.reason:find("whale_follow", 1, true) ~= nil,
    "reason names the strategy shape")
  ok(e ~= nil and r.exits == nil or #r.exits == 0, "no exits — holding is the thesis")
end

-- the mirrored down leg is the same shape on its own token.
do
  reset()
  feed_pair(0.50, 0)
  feed_pair(0.44, 45000)
  feed_pair(0.38, 120000) -- up falls, DOWN leg's own mid rises to 0.62
  state.time_left = 180
  local r = evaluate()
  ok(#r.entries == 1, "the down leg establishing fires exactly one entry")
  ok(r.entries[1] ~= nil and r.entries[1].token == "down-1",
    "the entry buys the down leg at ITS own ask")
  ok(r.entries[1] ~= nil and r.entries[1].price == "0.63",
    "the down entry also lifts the ask (0.63), got "
    .. tostring(r.entries[1] and r.entries[1].price))
end

-- ═══ NEGATIVE — every gate is a real wall ═══════════════════════════════════

-- THE anti-chase wall: same establishment, but the level arrived as a spike
-- (0.50 -> 0.75 with nothing between: 60s drift 0.25 > 0.10) — refused.
do
  reset()
  feed_pair(0.50, 0)
  feed_pair(0.75, 120000)
  state.time_left = 180
  ok(#evaluate().entries == 0,
    "a spike into the level refuses — the anti-chase wall holds")
end

-- too early: the round is younger than the entry window.
do
  reset()
  feed_pair(0.50, 0)
  feed_pair(0.62, 45000)
  state.time_left = 240 -- elapsed 60 < 90
  ok(#evaluate().entries == 0, "inside the first 90s: no entry")
end

-- too late: inside the last minute the window is closed.
do
  reset()
  feed_pair(0.50, 0)
  feed_pair(0.56, 45000)
  feed_pair(0.62, 120000)
  state.time_left = 50 -- elapsed 250 > 240
  ok(#evaluate().entries == 0, "past 240s elapsed: no entry")
end

-- above the band: the residual upside no longer pays for the naked risk.
do
  reset()
  feed_pair(0.50, 0)
  feed_pair(0.85, 50000)
  feed_pair(0.91, 120000) -- ask 0.92 > 0.88
  state.time_left = 180
  ok(#evaluate().entries == 0, "an ask above max_entry_price refuses")
end

-- not established: the level never left the coin-flip zone.
do
  reset()
  feed_pair(0.50, 0)
  feed_pair(0.53, 120000)
  state.time_left = 180
  ok(#evaluate().entries == 0, "a 0.03 move off the open is not establishment")
end

-- pulled back off the running high: the continuation premise is dented.
do
  reset()
  feed_pair(0.50, 0)
  feed_pair(0.62, 60000)
  feed_pair(0.60, 120000)
  state.time_left = 180
  ok(#evaluate().entries == 0,
    "a leg below its round high refuses (承接在极值上)")
end

-- a stale book is an outage, not a signal.
do
  reset()
  feed_pair(0.50, 0)
  feed_pair(0.56, 45000)
  feed_pair(0.62, 120000)
  state.time_left = 180
  state.books["up-1"].fresh = false
  ok(#evaluate().entries == 0, "a stale book refuses to price an entry")
end

-- a kernel with no fee schedule is an unknown cost regime: fail-closed.
do
  reset()
  feed_pair(0.50, 0)
  feed_pair(0.56, 45000)
  feed_pair(0.62, 120000)
  state.time_left = 180
  state.fees = nil
  ok(#evaluate().entries == 0, "no fee schedule: no entries")
end

-- the asset filter: the evidence corpus is BTC; ETH stays silent by default.
do
  reset()
  state.asset = "ETH"
  feed_pair(0.50, 0)
  feed_pair(0.56, 45000)
  feed_pair(0.62, 120000)
  state.time_left = 180
  ok(#evaluate().entries == 0, "assets=BTC default: an ETH market never fires")
end

-- a ring younger than the drift window has no drift measurement: fail-closed.
do
  reset()
  feed_up(0.50, 95000)   -- the strategy joined mid-round
  feed_up(0.56, 115000)
  state.time_left = 180  -- elapsed 120, but coverage is 25s
  ok(#evaluate().entries == 0,
    "drift window not covered: no measurement, no entry (fail-closed)")
end

-- a round longer than round_sec never enters the 5m window.
do
  reset()
  feed_pair(0.50, 0)
  feed_pair(0.56, 45000)
  feed_pair(0.62, 120000)
  state.time_left = 780 -- a 900s round: elapsed would be negative
  ok(#evaluate().entries == 0, "round_sec=300 default: a 15m round never fires")
end

-- ═══ AUDIT REPRO — the two blockers, pinned as negative rows ════════════════

-- Blocker 1 (P1): TWO conditions qualify in the SAME evaluation — each market
-- carries the same established-plateau shape on its up leg. The per-round
-- opening cap is re-judged per market, so exactly ONE entry may leave this
-- evaluation (one opening per round SLOT, not one per condition).
do
  reset()
  state.markets = {
    { asset = "BTC", condition_id = "cA", up_token = "up-a",
      down_token = "down-a", expires_at_ms = 0, slot = 1, neg_risk = false },
    { asset = "BTC", condition_id = "cB", up_token = "up-b",
      down_token = "down-b", expires_at_ms = 0, slot = 1, neg_risk = false },
  }
  feed_both_markets(0.50, 0.50, 0)
  feed_both_markets(0.56, 0.56, 45000)
  feed_both_markets(0.62, 0.62, 120000)
  state.time_left = 180
  local r = evaluate()
  ok(#r.entries == 1,
    "two qualifying conditions, one round: exactly ONE entry, got "
      .. #r.entries)
  -- and the cap stays spent across re-evaluations (cB never latched, but the
  -- re-judged budget holds the round shut):
  feed_both_markets(0.64, 0.64, 125000)
  state.time_left = 175
  ok(#evaluate().entries == 0,
    "the round is still spent after a double-condition evaluation")
end

-- Blocker 2 (P2): the strategy (re)joins at elapsed 60s — its first fresh
-- mid is NOT the round-open reference. Every OTHER gate passes on this tape
-- (established 0.12, drift 0.06, at high, in band); the reference
-- qualification alone must hold the round shut: fail-closed, zero entries.
do
  reset()
  feed_pair(0.50, 60000)  -- first-ever sample: already 60s into the round
  feed_pair(0.56, 105000)
  feed_pair(0.62, 180000)
  state.time_left = 120   -- elapsed 180: inside [90, 240]
  ok(#evaluate().entries == 0,
    "a mid-round start has no round-open reference: fail-closed, zero entries")
end

-- ═══ LATCHES ════════════════════════════════════════════════════════════════

-- one attempt per condition per round; one opening per round.
do
  reset()
  feed_pair(0.50, 0)
  feed_pair(0.56, 45000)
  feed_pair(0.62, 120000)
  state.time_left = 180
  ok(#evaluate().entries == 1, "first evaluation fires")
  feed_pair(0.64, 125000)
  state.time_left = 175
  ok(#evaluate().entries == 0, "the round is spent: no second opening")
end

-- a new round retires every latch.
do
  reset()
  feed_pair(0.50, 0)
  feed_pair(0.56, 45000)
  feed_pair(0.62, 120000)
  state.time_left = 180
  ok(#evaluate().entries == 1, "round 1 fires")
  S.bk_on_round({ slot = 2 })
  state.slot = 2
  state.now = T0 + 1000000
  feed_pair(0.50, 1000000)
  feed_pair(0.56, 1045000)
  feed_pair(0.62, 1120000)
  state.time_left = 180
  local r = evaluate()
  ok(#r.entries == 1, "a new round re-arms and fires again")
  ok(r.entries[1] ~= nil and r.entries[1].token == "up-1",
    "the new round's entry is its own established leg")
end

-- ═══ SEAL ═══════════════════════════════════════════════════════════════════

-- at every shape: exits and breaks are always empty; entries never carry
-- shares; a corrupt param falls back to the manifest default.
do
  local shapes = {
    { 0.50, 0 }, { 0.56, 45000 }, { 0.75, 120000 }, { 0.91, 120000 },
  }
  for i, s in ipairs(shapes) do
    reset()
    feed_pair(0.50, 0)
    if s[2] >= 45000 then feed_pair(0.56, 45000) end
    if s[2] >= 120000 then feed_pair(s[1] == 0.50 and 0.50 or s[1], s[2]) end
    state.time_left = 180
    local r = evaluate()
    ok(#r.exits == 0, "shape " .. i .. ": exits always empty")
    ok(#r.breaks == 0, "shape " .. i .. ": breaks always empty")
    for j = 1, #r.entries do
      ok(r.entries[j].shares == nil, "shape " .. i .. ": entries carry no shares")
      ok(r.entries[j].price ~= nil, "shape " .. i .. ": entries carry a price")
    end
  end
end

do
  reset()
  state.params = {
    round_sec = "abc", min_entry_price = "xyz", max_drift_60s = "",
    entry_elapsed_min_sec = "bad",
  }
  feed_pair(0.50, 0)
  feed_pair(0.56, 45000)
  feed_pair(0.62, 120000)
  state.time_left = 180
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
