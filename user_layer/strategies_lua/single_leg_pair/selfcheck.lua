-- single_leg_pair self-check (issue #387) — pure Lua 5.4, zero dependencies.
--
-- Drives the REAL strategy.lua through a stub `bk` surface (the §6.5 wire
-- shape: decimal strings, fresh flags, nil sides) and asserts the rows the
-- README promises:
--   positive  — sole liftable leg in the band → ONE entry at its own ask,
--               sized to the leg's ask depth; a leader above the gap floor
--               fires; the stop arms AT the deadline and ladders every
--               entered leg to the last tick;
--   negative  — dead band (one look, then latched), lottery zone (silent,
--               re-armable), above-ceiling (silent), leader gap below the
--               floor, equal asks, stale book, no/corrupt fee schedule,
--               below the entry time floor, over the per-round cap, one
--               attempt per condition per round;
--   seal      — exits are ALWAYS the "single-leg stop" tag (never a merge,
--               never an untagged sell), `breaks` is always empty.
--
-- Run: lua5.4 selfcheck.lua   (exit 0 = all rows hold)

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
-- state = { slot, time_left, fees, markets = {{asset,cid,up,down}},
--           books = { [token] = book-or-stale } }

local function mkbook(ask, depth, fresh)
  if ask == nil then return nil end
  return {
    symbol = "tok", best_bid = "0.01", best_ask = ask, mid = ask,
    obi = "0", spread = "0.01", bid_depth = "1", ask_depth = depth or "100",
    ts_ms = 0, fresh = fresh ~= false,
  }
end

local state = {
  slot = 1, time_left = 200,
  fees = { name = "official", rate = "0.125", exponent = 2 },
  markets = {}, books = {},
}

local bk = {}
bk.now_ms = function() return 1700000000 end
bk.round = function()
  return { slot = state.slot, time_left_sec = state.time_left, now_ms = bk.now_ms() }
end
bk.markets = function()
  local out = {}
  for i, m in ipairs(state.markets) do
    out[i] = {
      asset = m.asset, condition_id = m.cid,
      up_token = m.up, down_token = m.down,
      expires_at_ms = bk.now_ms() + state.time_left * 1000,
      slot = state.slot, neg_risk = false,
    }
  end
  return out
end
bk.book = function(symbol) return state.books[symbol] end
bk.params = function() return {} end
bk.fees = function()
  if state.fees == "none" then return nil end
  return state.fees
end
bk.kline = function() return nil end
bk.account = function() return nil end

-- the strategy is PERSISTENT across evaluations within a row (latches must
-- survive); a row starts from reset(). strategy.lua defines GLOBALS
-- (bk_evaluate etc.) the way the kernel loads it — grab them off _ENV and
-- strip them again so each instance is fresh.
local strategy
local function reset()
  strategy = real_loadfile(here .. "/strategy.lua")()
  if type(strategy) ~= "table" then
    strategy = {
      bk_evaluate = _ENV.bk_evaluate,
      bk_on_kline = _ENV.bk_on_kline,
      bk_on_book = _ENV.bk_on_book,
      bk_on_round = _ENV.bk_on_round,
    }
  end
  _ENV.bk_evaluate = nil
  _ENV.bk_on_kline = nil
  _ENV.bk_on_book = nil
  _ENV.bk_on_round = nil
  strategy.bk = bk
  _ENV.bk = bk -- the strategy reads the GLOBAL bk inside its closures
  return strategy
end

local function evaluate() return strategy.bk_evaluate() end

local function world(markets, books, time_left, fees, slot)
  state.markets = markets
  state.books = books
  state.time_left = time_left or 200
  -- fees: absent = the official schedule; the sentinel "none" = no schedule
  -- in force (bk.fees() answers nil — the fail-closed row).
  if fees == nil then
    fees = { name = "official", rate = "0.125", exponent = 2 }
  end
  state.fees = fees
  state.slot = slot or 1
end

local function cond(id)
  return { asset = "BTC", cid = id, up = "up-" .. id, down = "down-" .. id }
end

-- ═══ POSITIVE ═══════════════════════════════════════════════════════════════

-- sole liftable leg in the band: ONE entry at its own ask, depth-sized.
-- (The "sole" shape = the OTHER leg's book exists but its ask is gone —
-- `best_ask == "0"` or a nil ask on a present book. A wholly-ABSENT book is
-- an outage and refuses: the strategy never trades a one-book round.)
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "250"), ["down-c1"] = mkbook("0", "100") })
  local r = evaluate()
  ok(#r.entries == 1, "sole leg fires exactly one entry, got " .. #r.entries)
  local e = r.entries[1]
  ok(e.token == "up-c1", "sole leg buys the liftable side")
  ok(e.price == "0.66", "entry prices AT the leg's own ask")
  ok(e.shares == "250.00", "sized to the leg's ask depth, got " .. tostring(e.shares))
  ok(e.reason:find("sole", 1, true) ~= nil, "reason names the trigger shape")
end

-- the DOWN side mirrors.
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0", "100"), ["down-c1"] = mkbook("0.71", "80") })
  local r = evaluate()
  ok(#r.entries == 1 and r.entries[1].token == "down-c1",
    "sole DOWN leg fires at its own ask")
  ok(r.entries[1].price == "0.71" and r.entries[1].shares == "80.00",
    "sole DOWN entry priced at its ask, depth-sized")
end

-- a wholly absent opposite book is an outage: refuse.
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "250") })
  ok(#evaluate().entries == 0, "absent opposite book refuses (outage, not sole)")
end

-- leader: strictly higher ask in band, gap >= 0.02 → the leader's side.
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.72", "500"), ["down-c1"] = mkbook("0.25", "500") })
  local r = evaluate()
  ok(#r.entries == 1, "leader fires one entry")
  ok(r.entries[1].token == "up-c1" and r.entries[1].price == "0.72",
    "leader = the strictly higher ask")
  ok(r.entries[1].reason:find("leader", 1, true) ~= nil,
    "reason names the leader shape")
end

-- band edges are inclusive: 0.55 and 0.85 both fire.
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.55", "100"), ["down-c1"] = mkbook("0", "1") })
  ok(#evaluate().entries == 1, "0.55 is inside the band (inclusive)")
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.85", "100"), ["down-c1"] = mkbook("0", "1") })
  ok(#evaluate().entries == 1, "0.85 is inside the band (inclusive)")
end

-- the stop: arms AT the deadline (<=), ladders the entered leg to the last
-- tick, and never fires before it.
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "250"), ["down-c1"] = mkbook("0", "1") })
  ok(#evaluate().entries == 1, "enter at T-200")
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "250"), ["down-c1"] = mkbook("0", "1") }, 26)
  local r = evaluate()
  ok(#r.entries == 0 and #r.exits == 0, "T-26: deadline not reached, nothing fires")
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "250"), ["down-c1"] = mkbook("0", "1") }, 25)
  r = evaluate()
  ok(#r.exits == 1, "deadline emits one stop, got " .. #r.exits)
  ok(r.exits[1].token == "up-c1", "stop targets the entered leg")
  ok(r.exits[1].reason == "single-leg stop", "stop is tagged 'single-leg stop'")
  ok(#r.entries == 0, "no entries at the deadline")
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "250"), ["down-c1"] = mkbook("0", "1") }, 5)
  r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "single-leg stop",
    "the stop ladders to the last tick")
end

-- the per-round cap: 3 conditions fire, the 4th is refused this slot.
do
  reset()
  local markets = { cond("c1"), cond("c2"), cond("c3"), cond("c4") }
  local books = {}
  for _, m in ipairs(markets) do
    books[m.up] = mkbook("0.66", "100")
    books[m.down] = mkbook("0", "1")
  end
  world(markets, books)
  local r = evaluate()
  ok(#r.entries == 3, "max_open_positions caps the slot at 3, got " .. #r.entries)
  local seen = {}
  for _, e in ipairs(r.entries) do seen[e.token] = true end
  ok(seen["up-c1"] and seen["up-c2"] and seen["up-c3"] and not seen["up-c4"],
    "the first three conditions win, the fourth is capped")
end

-- ═══ NEGATIVE ═══════════════════════════════════════════════════════════════

-- the dead band (0.50, 0.55): refuses, WITH the attempt latch burned.
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.52", "100"), ["down-c1"] = mkbook("0", "1") })
  ok(#evaluate().entries == 0, "dead band refuses")
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "100"), ["down-c1"] = mkbook("0", "1") }, 150)
  ok(#evaluate().entries == 0, "dead-band look burns the attempt latch")
end

-- the lottery zone (<= 0.50): refuses SILENTLY (re-armable).
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.35", "100"), ["down-c1"] = mkbook("0", "1") })
  ok(#evaluate().entries == 0, "lottery zone refuses")
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "100"), ["down-c1"] = mkbook("0", "1") }, 150)
  ok(#evaluate().entries == 1, "lottery-zone look does NOT burn the latch")
end

-- above the ceiling: silent refusal (re-armable).
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.90", "100"), ["down-c1"] = mkbook("0", "1") })
  ok(#evaluate().entries == 0, "above the band ceiling refuses")
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.80", "100"), ["down-c1"] = mkbook("0", "1") }, 150)
  ok(#evaluate().entries == 1, "above-ceiling look does NOT burn the latch")
end

-- leader gap below the floor: a pair-shaped book, not a verdict (and no
-- entry on either leg).
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "100"), ["down-c1"] = mkbook("0.65", "100") })
  ok(#evaluate().entries == 0, "0.01 gap is not a leader verdict")
end

-- equal asks name no leader.
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.60", "100"), ["down-c1"] = mkbook("0.60", "100") })
  ok(#evaluate().entries == 0, "equal asks name no leader")
end

-- a stale or missing book is an outage, not a signal.
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "100"),
      ["down-c1"] = mkbook("0.30", "100", false) })
  ok(#evaluate().entries == 0, "stale leg book refuses")
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "100") }) -- DOWN book entirely absent
  ok(#evaluate().entries == 0, "absent leg book refuses")
end

-- no fee schedule → no entries (fail-closed); the stop still runs.
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "250"), ["down-c1"] = mkbook("0", "1") }, 200, "none")
  ok(#evaluate().entries == 0, "no schedule → no entries")
  -- enter while a schedule is in force, then let it vanish: the stop MUST
  -- still arm (a protection never depends on what it protects against).
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "250"), ["down-c1"] = mkbook("0", "1") })
  ok(#evaluate().entries == 1, "enter with the schedule in force")
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "250"), ["down-c1"] = mkbook("0", "1") }, 20, "none")
  local r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "single-leg stop",
    "the stop does not depend on the fee schedule")
end

-- a corrupt schedule (negative rate / bad exponent) refuses too.
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "100"), ["down-c1"] = mkbook("0", "1") }, 200,
    { name = "x", rate = "-1", exponent = 2 })
  ok(#evaluate().entries == 0, "negative rate → no entries")
end
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "100"), ["down-c1"] = mkbook("0", "1") }, 200,
    { name = "x", rate = "0.125", exponent = -1 })
  ok(#evaluate().entries == 0, "negative exponent → no entries")
end

-- below the entry time floor: no NEW entries (and nothing else fires).
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "100"), ["down-c1"] = mkbook("0", "1") }, 44)
  local r = evaluate()
  ok(#r.entries == 0, "below min_time_left_sec no entries fire")
  ok(#r.exits == 0, "with nothing entered, nothing stops")
end

-- one ATTEMPT per condition per round: the second evaluation of the same
-- round emits nothing new (and the deadline block still owns the stops).
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "100"), ["down-c1"] = mkbook("0", "1") })
  ok(#evaluate().entries == 1, "the attempt fires once")
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.66", "100"), ["down-c1"] = mkbook("0", "1") }, 150)
  local r = evaluate()
  ok(#r.entries == 0, "a rejected/armed attempt is not retried")
  ok(#r.exits == 0, "armed-but-not-stopped emits no exits")
end

-- a new round slot retires every latch: the same condition can fire again.
do
  reset()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.52", "100"), ["down-c1"] = mkbook("0", "1") })
  evaluate()
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.52", "100"), ["down-c1"] = mkbook("0", "1") }, 200, "none", 2)
  local r = evaluate()
  ok(#r.entries == 0, "new round, dead band still refuses")
  -- …and the stop of the OLD round does not leak into the new one.
  world({ cond("c1") },
    { ["up-c1"] = mkbook("0.52", "100"), ["down-c1"] = mkbook("0", "1") }, 10, nil, 2)
  ok(#evaluate().exits == 0, "the previous round's stop does not leak")
end

-- ═══ SEAL ═══════════════════════════════════════════════════════════════════

-- exits are ALWAYS the stop tag — never a merge, never an untagged sell —
-- and `breaks` is always empty.
do
  local shapes = {
    { { "0.66", "250" }, nil, 200 },
    { { "0.66", "250" }, nil, 25 },
    { { "0.66", "250" }, nil, 5 },
    { { "0.52", "250" }, nil, 25 },
  }
  for i, s in ipairs(shapes) do
    reset()
    world({ cond("c1") },
      { ["up-c1"] = mkbook(s[1][1], s[1][2]), ["down-c1"] = mkbook("0", "1") }, s[3])
    evaluate()
    world({ cond("c1") },
      { ["up-c1"] = mkbook(s[1][1], s[1][2]), ["down-c1"] = mkbook("0", "1") }, s[3] == 200 and 25 or s[3])
    local r = evaluate()
    for _, e in ipairs(r.exits) do
      ok(e.reason == "single-leg stop",
        "shape " .. i .. ": every exit is the naked stop, got " .. e.reason)
      ok(e.reason ~= "merge", "shape " .. i .. ": no merge is ever emitted")
    end
    ok(#r.breaks == 0, "shape " .. i .. ": breaks always empty")
  end
end

-- ── verdict ─────────────────────────────────────────────────────────────────
if #failures > 0 then
  real_io.stderr:write(string.format("selfcheck FAILED: %d/%d rows\n", #failures, checks))
  for _, f in ipairs(failures) do real_io.stderr:write("  - " .. f .. "\n") end
  real_os.exit(1)
end
real_print(string.format("selfcheck OK: %d rows", checks))
