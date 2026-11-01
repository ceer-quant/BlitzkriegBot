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
--   premise    — sub-threshold moves never confirm, a rebound clears a
--               confirmed premise, corrupt params fall back to the default;
--   gates      — a stale book refuses to price, the force-exit window and
--               the between-rounds gap emit nothing;
--   seal       — exits and breaks are always empty (the exit layer is the
--               kernel's).
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
-- state = { now, slot, time_left, params, round_open, books = { [tok] = row } }

local T0 = 1700000000000

local state = {
  now = T0, slot = 1, time_left = 900,
  params = {}, round_open = true,
  books = {},
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

-- the strategy is PERSISTENT across evaluations within a row (last_mid and
-- confirmed must survive); a row starts from reset(). strategy.lua defines
-- GLOBALS the way the kernel loads it — grab them off _ENV and strip again.
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

--- Advance the clock (t_ms = ms since T0) and feed one leg's book row.
local function feed(tok, mid_num, t_ms, fresh)
  if t_ms then state.now = T0 + t_ms end
  local b = book_row(tok, mid_num, fresh)
  state.books[tok] = b
  S.bk_on_book(b)
end

local function evaluate() return S.bk_evaluate() end

-- ═══ DIRECTION GATE — POSITIVE: the crashing DOWN leg ═══════════════════════

-- the one edge: DOWN leg 0.50 → 0.45 (−10% ≥ 4% threshold) confirms, and
-- exactly ONE entry rests at the leg's own best_bid.
do
  reset()
  feed("up-1", 0.50, 0)
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  local r = evaluate()
  ok(#r.entries == 1, "crashing DOWN leg fires exactly one entry, got " .. #r.entries)
  local e = r.entries[1]
  ok(e ~= nil and e.token == "down-1", "entry buys the DOWN leg (down-1)")
  ok(e ~= nil and e.price == "0.44", "entry rests at best_bid 0.44, got "
    .. tostring(e and e.price))
  ok(e ~= nil and tonumber(e.price) < tonumber(state.books["down-1"].best_ask),
    "entry is strictly under the ask (post-only shape — never a chase)")
  ok(e ~= nil and e.shares == nil, "shares omitted — the kernel sizes")
  ok(e ~= nil and e.reason:find("down", 1, true) ~= nil,
    "reason names the direction gate")
end

-- ═══ DIRECTION GATE — NEGATIVE: the UP leg never bids ═══════════════════════

-- the deleted WR-7.5% leg: an UP-token spike confirms nothing, bids nothing.
do
  reset()
  feed("down-1", 0.50, 0)
  feed("up-1", 0.50, 0)
  feed("up-1", 0.60, 1000) -- +20%: the old up arm's exact trigger shape
  local r = evaluate()
  ok(#r.entries == 0, "an UP-token spike bids nothing (the up leg is deleted)")
end

-- the UP token falling is still the wrong token: only the DOWN leg is
-- buyable, ever.
do
  reset()
  feed("up-1", 0.50, 0)
  feed("up-1", 0.40, 1000) -- −20%: a falling UP leg
  local r = evaluate()
  ok(#r.entries == 0, "a falling UP leg bids nothing (only down-1 is read)")
end

-- a RISING DOWN leg is not a falling knife: no premise, no bid.
do
  reset()
  feed("down-1", 0.30, 0)
  feed("down-1", 0.40, 1000) -- +33%: the down leg surging
  local r = evaluate()
  ok(#r.entries == 0, "a rising DOWN leg bids nothing (only the fall is the edge)")
end

-- ═══ PREMISE ════════════════════════════════════════════════════════════════

-- sub-threshold drop: no confirmation, no bid. `threshold` is the single-tick
-- move in PERCENT (default 0.04 = 0.04% — the teaching fixture's original
-- trigger), so the gate is tested with an explicit hot threshold of 5%.
do
  reset()
  state.params = { threshold = "5" }
  feed("down-1", 0.50, 0)
  feed("down-1", 0.49, 1000) -- −2% < 5%
  ok(#evaluate().entries == 0, "a sub-threshold drop never confirms")
end

-- the same −2% shape confirms under a 1% threshold: the tunable gates BOTH
-- ways (not just off).
do
  reset()
  state.params = { threshold = "1" }
  feed("down-1", 0.50, 0)
  feed("down-1", 0.49, 1000) -- −2% ≥ 1%
  ok(#evaluate().entries == 1, "the same shape over the threshold confirms")
end

-- a rebound clears a confirmed premise before the next evaluate.
do
  reset()
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000) -- confirms
  feed("down-1", 0.46, 2000) -- +2.2% up-move: premise cleared
  ok(#evaluate().entries == 0, "a rebound clears the confirmed premise")
end

-- corrupt threshold falls back to the manifest default (0.04): the same
-- −10% shape still arms.
do
  reset()
  state.params = { threshold = "abc" }
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  ok(#evaluate().entries == 1,
    "corrupt params fall back to the default: the same shape still fires")
end

-- a hot threshold tightens the gate: −10% under a 20% threshold arms nothing.
do
  reset()
  state.params = { threshold = "20" }
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  ok(#evaluate().entries == 0,
    "the threshold tunable still gates (hot param honored, nothing armed)")
end

-- ═══ GATES ══════════════════════════════════════════════════════════════════

-- a stale book is not a price.
do
  reset()
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  state.books["down-1"].fresh = false
  ok(#evaluate().entries == 0, "a stale book refuses to price an entry")
end

-- inside the force-exit window: no new bids.
do
  reset()
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  state.time_left = 55
  ok(#evaluate().entries == 0, "inside the force-exit window: no entries")
end

-- between rounds: no round, no trade.
do
  reset()
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  state.round_open = false
  ok(#evaluate().entries == 0, "between rounds: nothing to trade")
end

-- ═══ SEAL ═══════════════════════════════════════════════════════════════════

-- exits and breaks are always empty — the exit layer is the kernel's.
do
  local shapes = {
    { "down-1", 0.50, 0 }, { "down-1", 0.45, 1000 }, { "down-1", 0.40, 2000 },
  }
  reset()
  for i, s in ipairs(shapes) do
    feed(s[1], s[2], s[3])
    local r = evaluate()
    ok(#r.exits == 0 and #r.breaks == 0,
      "shape " .. i .. ": no exits, no breaks (the kernel owns survival)")
    for j = 1, #r.entries do
      ok(r.entries[j].shares == nil, "shape " .. i .. ": entries carry no shares")
      ok(r.entries[j].price ~= nil, "shape " .. i .. ": entries carry a price")
    end
  end
end

-- ── verdict ─────────────────────────────────────────────────────────────────
if #failures > 0 then
  real_io.stderr:write(string.format("selfcheck FAILED: %d/%d rows\n", #failures, checks))
  for _, f in ipairs(failures) do real_io.stderr:write("  - " .. f .. "\n") end
  real_os.exit(1)
end
real_print(string.format("selfcheck OK: %d rows", checks))
