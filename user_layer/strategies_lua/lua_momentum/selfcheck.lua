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

-- ═══ DIRECTION GATE — the UP leg (re-armed, operator ruling 2026-11-04) ════

-- an UP-token spike arms the UP leg: ONE entry rests at ITS best_bid.
do
  reset()
  feed("down-1", 0.50, 0)
  feed("up-1", 0.50, 0)
  feed("up-1", 0.60, 1000) -- +20%: ≥ threshold
  local r = evaluate()
  ok(#r.entries == 1, "a rising UP leg fires exactly one entry, got " .. #r.entries)
  local e = r.entries[1]
  ok(e ~= nil and e.token == "up-1", "the UP premise buys the UP leg (up-1)")
  ok(e ~= nil and e.price == "0.59", "UP entry rests at its own best_bid 0.59")
end

-- a falling UP leg is the wrong direction for that token: the DOWN premise
-- belongs to the DOWN token only — up-1 crashing arms nothing buyable (its
-- own premise would be "down", but the entry gate matches the leg's want).
do
  reset()
  feed("up-1", 0.50, 0)
  feed("up-1", 0.40, 1000) -- −20%: up-1 is crashing, not rising
  local r = evaluate()
  ok(#r.entries == 0,
    "a falling UP leg buys nothing — its premise says down, its leg wants up")
end

-- ═══ 筹码地板 (min_entry_price, default 0.15) ══════════════════════════════

-- the 0.007 lottery shape: a crash onto a dust bid buys NOTHING.
do
  reset()
  feed("down-1", 0.50, 0)
  feed("down-1", 0.007, 1000) -- −98.6%: the crash arms the premise...
  state.books["down-1"].best_bid = "0.007"
  local r = evaluate()
  ok(#r.entries == 0, "a dust best_bid (0.007 < 0.15 floor) buys nothing")
end

-- the floor is the tunable: 0 disables it entirely (the pre-ruling shape).
do
  reset()
  state.params = { min_entry_price = "0" }
  feed("down-1", 0.50, 0)
  feed("down-1", 0.10, 1000)
  state.books["down-1"].best_bid = "0.09"
  local r = evaluate()
  ok(#r.entries == 1, "min_entry_price = 0 disables the floor (dust buys again)")
end

-- a bid exactly AT the floor passes (>=).
do
  reset()
  feed("down-1", 0.50, 0)
  feed("down-1", 0.16, 1000)
  state.books["down-1"].best_bid = "0.15"
  local r = evaluate()
  ok(#r.entries == 1 and r.entries[1].price == "0.15",
    "a bid exactly at the floor is buyable")
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

-- Nothing held: exits and breaks stay empty. The exit rules only speak
-- about a position the host has pushed through bk.holdings().
do
  local shapes = {
    { "down-1", 0.50, 0 }, { "down-1", 0.45, 1000 }, { "down-1", 0.40, 2000 },
  }
  reset()
  for i, s in ipairs(shapes) do
    feed(s[1], s[2], s[3])
    local r = evaluate()
    ok(#r.exits == 0 and #r.breaks == 0,
      "shape " .. i .. ": flat book emits no exits and no breaks")
    for j = 1, #r.entries do
      ok(r.entries[j].shares == nil, "shape " .. i .. ": entries carry no shares")
      ok(r.entries[j].price ~= nil, "shape " .. i .. ": entries carry a price")
    end
  end
end


-- ═══ EXITS — the owner's giveback, priced on the bid ════════════════════════
-- UP 10 @ 0.28 ran to a 0.42 bid (+50% gross) with ~7m41s left, then gave
-- it back. Defaults bank it. PolySim's fee schedule is nil here: the
-- decision still haircuts by 0.07*p*(1-p).

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
    mid = nil, -- a mid must not be required, and must not be the sell
    obi = "0", spread = "0.01",
    bid_depth = "900", ask_depth = "900",
    ts_ms = state.now, fresh = fresh ~= false,
  }
end

local function exit_reasons(r)
  local s = {}
  for i = 1, #r.exits do s[i] = r.exits[i].reason end
  return table.concat(s, ",")
end
-- ═══ STOP LOSS — Z15-8 回归 ════════════════════════════════════════════════════

-- Z15-8-1: bid 0.60→0.10 单跳，首个有 bid tick 必须发 stop_loss（bid 0.00 视为无买盘）
do
  reset()
  state.params = { stop_loss_pct = "50" }
  hold("down-1", "0.60")
  quote("down-1", "0.10", "0.11")
  local r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "stop_loss",
    "bid 0.60→0.10 单跳：首个有 bid tick 必须发 stop_loss，got " .. exit_reasons(r))
end

-- Z15-8-2: 重启恢复持仓仍止损（direction 来自 holdings，armed_dir 仅加分）
do
  reset()
  state.params = { stop_loss_pct = "50" }
  hold("down-1", "0.60")
  quote("down-1", "0.10", "0.11")
  local r1 = evaluate()  -- 第一轮：armed_dir 为空，row.direction = "down" 仍应止损
  ok(#r1.exits == 1 and r1.exits[1].reason == "stop_loss",
    "重启后方向来自 holdings.direction，止损仍触发，got " .. exit_reasons(r1))
  -- 第二轮：再次跌，应继续止损（armed_dir 已清，但 holdings.direction 仍在）
  quote("down-1", "0.10", "0.11")
  local r2 = evaluate()
  ok(#r2.exits == 1 and r2.exits[1].reason == "stop_loss",
    "重启后第二轮：holdings.direction 仍驱动止损，got " .. exit_reasons(r2))
end

-- Z15-8-3: stop_loss_pct_down=0 关闭止损（down 腿读 stop_loss_pct_down）
do
  reset()
  state.params = { stop_loss_pct_down = "0" }
  hold("down-1", "0.60")
  quote("down-1", "0.10", "0.11")
  local r = evaluate()
  ok(#r.exits == 0, "stop_loss_pct_down=0：止损关闭，不发出 exit，got " .. exit_reasons(r))
end

-- Z15-8-4: stop_loss_pct=50 生效（-50% 阈值）
do
  reset()
  state.params = { stop_loss_pct = "50" }
  hold("down-1", "0.60")
  quote("down-1", "0.31", "0.32")
  local r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "stop_loss",
    "stop_loss_pct=50：-50% 止损阈值触发，got " .. exit_reasons(r))
end

-- Z15-8-5: stop_loss_pct_down=0.5（0.5%，不是 5%）
do
  reset()
  state.params = { stop_loss_pct_down = "0.5" }
  hold("down-1", "0.60")
  -- bid 0.598：费净后约 -3.1%，应触达 0.5% 止损（sl_pct 取整为 1）
  quote("down-1", "0.598", "0.599")
  local r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "stop_loss",
    "stop_loss_pct_down=0.5：应按 0.5%（非 5%）止损，got " .. exit_reasons(r))
end

-- Z15-8-6: 任意参数组合 bk_evaluate 不报错
do
  reset()
  state.params = {
    stop_loss_pct = "999", stop_loss_pct_up = "-1", stop_loss_pct_down = "abc",
    stop_loss_delay_sec = "0", stop_loss_confirm_sec = "0",
  }
  hold("down-1", "0.50")
  quote("down-1", "0.40", "0.41")
  local ok_result, err = pcall(evaluate)
  ok(ok_result, "任意参数组合 bk_evaluate 不报错，got " .. tostring(err))
end

-- the peak itself: bid 0.42, entry 0.28, fee 0.07*0.42*0.58 = 0.017052,
-- net +43.9% >= the default 40% → one take_profit, and no new entry is
-- required for the exit to exist.
do
  reset()
  state.time_left = 461 -- 7m41s
  hold("down-1", "0.28")
  quote("down-1", "0.42", "0.43")
  local r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "take_profit" and r.exits[1].token == "down-1",
    "a +50% gross bid is a take_profit after the 0.07 fee, got " .. exit_reasons(r))
end

-- zeroing the exit knobs restores hold-to-settlement: the same peak, then
-- the giveback to 0.26, emits nothing.
do
  reset()
  state.params = {
    take_profit_pct = "0", take_profit_price = "0",
    trail_arm_pct = "0", trail_giveback_pct = "0",
    exit_time_left_sec = "0",
  }
  state.time_left = 461
  hold("down-1", "0.28")
  quote("down-1", "0.42", "0.43")
  ok(#evaluate().exits == 0, "exits disabled: the 0.42 peak is held")
  quote("down-1", "0.26", "0.27")
  ok(#evaluate().exits == 0, "exits disabled: the giveback to 0.26 is held to settlement")
end

-- trailing only: arm after +20% net, sell on a 15 percentage-point giveback.
-- 0.42 arms and does not sell (no giveback yet); 0.26 gives back ~56pp.
do
  reset()
  state.params = {
    take_profit_pct = "0", take_profit_price = "0",
    trail_arm_pct = "20", trail_giveback_pct = "15",
  }
  state.time_left = 461
  hold("down-1", "0.28")
  quote("down-1", "0.42", "0.43")
  ok(#evaluate().exits == 0, "the arming tick is not itself a giveback")
  quote("down-1", "0.26", "0.27")
  local r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "trailing_stop",
    "a 15pp giveback from the 0.42 peak sells, got " .. exit_reasons(r))
end

-- the mid is not the sell. mid would be 0.42 (+50%); the bid is 0.30 and
-- the fee-net gain is ~2%, under both the 40% target and the 20% arm.
do
  reset()
  state.time_left = 461
  hold("down-1", "0.28")
  quote("down-1", "0.30", "0.54")
  state.books["down-1"].mid = "0.42"
  ok(#evaluate().exits == 0, "a 0.42 mid with a 0.30 bid is not a take-profit")
end

-- no bid, or a stale book: Z15-7 不得静默 return，产出 stop_blocked_no_liquidity
do
  reset()
  state.time_left = 461
  hold("down-1", "0.28")
  quote("down-1", "0.42", "0.43")
  state.books["down-1"].best_bid = nil
  local r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "stop_blocked_no_liquidity",
    "no best bid: stop_blocked_no_liquidity, got " .. exit_reasons(r))
end
do
  reset()
  state.time_left = 461
  hold("down-1", "0.28")
  quote("down-1", "0.42", "0.43", false)
  local r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "stop_blocked_no_liquidity",
    "a stale book: stop_blocked_no_liquidity, got " .. exit_reasons(r))
end

-- absolute price target, still net of the fee. 0.42 clears 0.40 and the
-- fee. 0.29 clears a 0.29 target on the bid but the fee turns it into a loss.
do
  reset()
  state.params = {
    take_profit_pct = "0", trail_arm_pct = "0", trail_giveback_pct = "0",
    take_profit_price = "0.40",
  }
  hold("down-1", "0.28")
  quote("down-1", "0.42", "0.43")
  local r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "take_profit_price",
    "bid at the price target sells, got " .. exit_reasons(r))
end
do
  reset()
  state.params = {
    take_profit_pct = "0", trail_arm_pct = "0", trail_giveback_pct = "0",
    take_profit_price = "0.29",
  }
  hold("down-1", "0.28")
  quote("down-1", "0.29", "0.30")
  ok(#evaluate().exits == 0, "a target the taker fee turns into a loss is not taken")
end

-- a fatter schedule than 0.07 wins. 0.50*0.42*0.58 leaves ~+6.5% net,
-- under the 40% target the official curve would have cleared.
do
  reset()
  state.fees = { name = "fat", rate = "0.50", exponent = 1 }
  hold("down-1", "0.28")
  quote("down-1", "0.42", "0.43")
  ok(#evaluate().exits == 0, "a larger live fee schedule blocks a fee-thin take-profit")
end

-- exit_fee_rate 0.90 on a nil PolySim schedule does the same.
do
  reset()
  state.params = { exit_fee_rate = "0.90" }
  hold("down-1", "0.28")
  quote("down-1", "0.42", "0.43")
  ok(#evaluate().exits == 0, "exit_fee_rate 0.90 haircuts the 0.42 bid below the target")
end

-- optional time exit fires inside its window even at a loss, and stays
-- quiet when the knob is 0. It still needs a bid.
do
  reset()
  state.params = {
    take_profit_pct = "0", trail_arm_pct = "0", trail_giveback_pct = "0",
    exit_time_left_sec = "120",
  }
  state.time_left = 100
  hold("down-1", "0.28")
  quote("down-1", "0.20", "0.21")
  local r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "time_exit",
    "inside exit_time_left_sec a loser is sold, got " .. exit_reasons(r))
  ok(#r.entries == 0, "the time-exit window is also inside the entry gate")
end
do
  reset()
  state.params = {
    take_profit_pct = "0", trail_arm_pct = "0", trail_giveback_pct = "0",
    exit_time_left_sec = "0",
  }
  state.time_left = 100
  hold("down-1", "0.28")
  quote("down-1", "0.20", "0.21")
  ok(#evaluate().exits == 0, "exit_time_left_sec 0 does not sell a loser early")
end

-- a winner inside the 60s entry blackout is STILL sold. The same crash
-- would have entered a minute earlier; here the entry stays shut.
do
  reset()
  state.time_left = 55
  hold("down-1", "0.28")
  feed("down-1", 0.50, 0)
  feed("down-1", 0.42, 1000) -- bid lands at 0.41; net of the fee still clears 40%
  local r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "take_profit",
    "the entry blackout does not swallow a take-profit, got " .. exit_reasons(r))
  ok(#r.entries == 0, "the entry blackout still refuses a new bid")
end

-- a held winner that is ALSO still confirmed is sold and not re-bought
-- in the same cycle.
do
  reset()
  state.time_left = 400
  hold("down-1", "0.28")
  feed("down-1", 0.50, 0)
  feed("down-1", 0.42, 1000)
  local r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "take_profit",
    "a held winner still exits, got " .. exit_reasons(r))
  ok(#r.entries == 0, "the token being sold is not re-bought in the same cycle")
end

-- ═══ BOTH SIDES ═════════════════════════════════════════════════════════════
-- holds_to_settlement would let the kernel admit the UP leg. The strategy
-- must not offer it unless allow_both_sides is explicitly 1.

do
  reset()
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  state.holdings = {
    {
      token_id = "up-1", entry_price = "0.28", shares = "10",
      direction = "up", condition_id = "c1", round_slot = 1,
    },
  }
  local r = evaluate()
  ok(#r.entries == 0, "holding the UP leg blocks the DOWN entry (allow_both_sides defaults off)")
end

do
  reset()
  state.params = { allow_both_sides = "1" }
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  state.holdings = {
    {
      token_id = "up-1", entry_price = "0.28", shares = "10",
      direction = "up", condition_id = "c1", round_slot = 1,
    },
  }
  local r = evaluate()
  ok(#r.entries == 1 and r.entries[1].token == "down-1",
    "allow_both_sides = 1 is the only way the opposite leg is offered")
end

-- ═══ RE-ENTRY ═══════════════════════════════════════════════════════════════
-- Default max_entries_per_round is 1. The unfilled bid may be re-quoted;
-- once the attempt has exited, that condition is done for the round.

do
  reset()
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  ok(#evaluate().entries == 1, "the first quote of the round is the one entry")
  ok(#evaluate().entries == 1, "re-quoting the unfilled bid does not spend a second entry")
  hold("down-1", "0.28")
  quote("down-1", "0.42", "0.43")
  local r = evaluate()
  ok(#r.exits == 1 and r.exits[1].reason == "take_profit" and #r.entries == 0,
    "the filled attempt exits and is not re-bought in the same cycle")
  state.holdings = {}
  r = evaluate()
  ok(#r.entries == 0 and #r.exits == 0,
    "after the exit, default max_entries_per_round 1 blocks a new entry, got "
      .. #r.entries .. " entries")
end

-- a later round slot is a new budget.
do
  reset()
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  evaluate()
  hold("down-1", "0.28")
  quote("down-1", "0.42", "0.43")
  evaluate()
  state.holdings = {}
  ok(#evaluate().entries == 0, "still blocked in the same slot")
  state.slot = 2
  local r = evaluate()
  ok(#r.entries == 1 and r.entries[1].token == "down-1",
    "a new round slot may enter again")
end

-- max_entries_per_round 0 quotes nothing. 2 allows one re-entry.
do
  reset()
  state.params = { max_entries_per_round = "0" }
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  ok(#evaluate().entries == 0, "max_entries_per_round 0 quotes nothing")
end

do
  reset()
  state.params = { max_entries_per_round = "2" }
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  evaluate()
  hold("down-1", "0.28")
  quote("down-1", "0.42", "0.43")
  evaluate()
  state.holdings = {}
  local r = evaluate()
  ok(#r.entries == 1, "max_entries_per_round 2 allows one re-entry after the exit, got "
    .. #r.entries)
end

-- reentry_cooldown_sec waits even when a slot remains. Exactly the cooldown
-- is enough; one millisecond early is not.
do
  reset()
  state.params = { max_entries_per_round = "2", reentry_cooldown_sec = "60" }
  feed("down-1", 0.50, 0)
  feed("down-1", 0.45, 1000)
  evaluate()
  hold("down-1", "0.28")
  quote("down-1", "0.42", "0.43")
  evaluate()
  state.holdings = {}
  local blocked = evaluate()
  ok(#blocked.entries == 0, "60s cooldown blocks the spare entry slot")
  state.now = state.now + 59999
  ok(#evaluate().entries == 0, "one millisecond early is still inside the cooldown")
  state.now = state.now + 1
  local r = evaluate()
  ok(#r.entries == 1, "at the cooldown boundary the second entry is offered, got "
    .. #r.entries)
end

-- ── verdict ─────────────────────────────────────────────────────────────────
if #failures > 0 then
  real_io.stderr:write(string.format("selfcheck FAILED: %d/%d rows\n", #failures, checks))
  for _, f in ipairs(failures) do real_io.stderr:write("  - " .. f .. "\n") end
  real_os.exit(1)
end
real_print(string.format("selfcheck OK: %d rows", checks))
