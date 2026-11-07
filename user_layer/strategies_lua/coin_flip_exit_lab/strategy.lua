-- coin_flip_exit_lab — issue #962 Exit-First 实验台 (LAB ONLY).
--
-- ⚠️ REPLAY/RESEARCH PACKAGE — 禁止进实盘默认配置. This package exists to
-- measure ONE thing: the expectancy contribution of an exit mechanism when
-- the entry contributes NO alpha. Do not add it to user_layer/configs/*;
-- do not enable it live. Every conclusion it produces is a replay number
-- on a frozen corpus (docs/reports/strategies/COIN-FLIP-962.md).
--
-- The operator's philosophy under test (#962): entry direction is a COIN
-- FLIP. A deterministic integer hash of (coin_seed, round_slot,
-- condition_id) picks UP or DOWN — no signal, no momentum, no price floor,
-- no book reading beyond "is there a fresh ask to lift". Entry alpha is
-- zero by construction, so whatever expectancy the lab measures IS the
-- exit mechanism's contribution (minus friction).
--
-- Reproducibility contract (#962 acceptance 1):
--   * the flip is a PURE function: FNV-1a-64 over the bytes of
--     "seed\0slot\0condition_id", then the splitmix64 finalizer for
--     avalanche, then mod 2. Lua 5.4 64-bit integer arithmetic only —
--     no math.random state, no os.time, no wall clock, no call-order
--     dependence. The same seed over the same corpus reproduces the same
--     flips byte-for-byte, and the lab script re-implements the hash in
--     JS to cross-check the replay's realized directions.
--   * the seed travels in the manifest (`coin_seed`, default 962) and in
--     every entry's reason string, so a report names its own seed.
--
-- Round selection (#962 轮次选择, declared not cherry-picked): every
-- round-market observed from its start (the strategy must have seen a tick
-- with time_left_sec >= round_min_seen_time_left, default 885 ≈ the full
-- 900s clock) gets exactly ONE flip and at most one entry per round —
-- max_entries_per_round is 1 by construction and not a knob. A partial
-- round (corpus edge) is never entered; a filled-then-exited condition is
-- never re-entered that slot.
--
-- Entry: taker-style at the chosen token's own best ask (manifest
-- `entry_maker: false` — MakerThenTaker; the lab replays escalate at
-- --entry-maker-timeout-ms 1 so the flip's fill is immediate and the
-- sample is not thinned by unfilled maker bids). A fresh two-sided book
-- is required (§6.6); entries stop inside entry_cutoff_sec (default 90)
-- so every position leaves the stack room to act before expiry.
--
-- The exit stack under measurement — every component a tunable, the grid
-- is run in full and reported without cherry-picking (scripts/
-- coin-flip-lab-replay.mjs):
--   1. hard_stop_loss_pct — the single-trade max-loss hard stop, judged
--      on NET proceeds (fresh best bid minus the exit_fee_rate taker
--      model, the same haircut lua_momentum prices exits with); 0 = off;
--   2. the time-weighted tightening take-profit band take_profit_pct →
--      take_profit_pct_tight, sliding linearly with the round clock (the
--      same tighten() algorithm lua_momentum v0.4.0 shipped — binary
--      pricing converges to {0,1} as the round drains, so a band sized
--      for the full 900s is mispriced late);
--   3. the trailing stop: trail_arm_pct arms on peak net gain,
--      trail_giveback_pct banks the giveback from that peak (the
--      *_tight variants share the band tightener; both default flat);
--   4. exit_time_left_sec — the declared deadline (default 45) that
--      beats the kernel's expiry gate, so no position rides unmeasured
--      past the window;
--   5. thesis death (#961) — see the declaration below: the runtime
--      injects the unconditional full-taker close the tick the predicate
--      fires, superseding this table (禁止扛单).
--
-- The kernel-side environment is NOT part of the treatment and stays
-- identical across arms: holds_to_settlement=true keeps the position
-- walker on the wide 50% holder containment stop and the +100% backstop
-- for BOTH the dynamic stack and the rigid control arm (same seed, same
-- flips, same window, same capital — only the exit params differ, so the
-- A/B difference attributes to exit parameters).
--
-- Wire rules (copied from the lua_momentum example):
--   * prices/sizes cross as exact decimal STRINGS ("0.55"), never floats;
--   * price decisions ONLY off books with `fresh == true`;
--   * the strategy suggests, the kernel adjudicates (§6.4).

-- BEGIN common/decimal.lua
-- common/decimal.lua — the ONE exact-decimal helper block for every Lua
-- strategy package (#439 candidate 2).
--
-- A decimal is `{ m = mantissa, s = scale }` meaning m / 10^s. Mantissas stay
-- far inside 64-bit integers for every value these strategies handle: prices
-- ≤ 1.0 with ≤ 10 decimals, and host-derived division results capped at 18
-- significant digits on parse (see dec_parse).
--
-- The block each package embeds is the text BETWEEN its
-- `-- BEGIN common/decimal.lua` and `-- END common/decimal.lua` markers —
-- byte-for-byte THIS file. The Lua sandbox has no require/dofile (poisoned in
-- user_layer/lua_runtime/src/sandbox.rs), so packages cannot share code at
-- runtime; the loader contract (§6.4: manifest.sha256 covers the whole entry)
-- means a loader-side injection would change a frozen contract. Embedding +
-- `scripts/lua-decimal-check.mjs` (byte-identity gate) is the honest middle:
-- one source of truth, ten verbatim copies, a gate that refuses divergence.
--
-- EDIT THIS FILE, then run `node scripts/lua-decimal-sync.mjs --apply` — it
-- re-embeds every package and re-signs each manifest's sha256. Never edit a
-- copy inside a package: the gate reds on the next CI run.
--
-- Provenance of the hardened variants (the 2026-09-30 rust_decimal 28-digit
-- mantissa incident, #94233fb9): the host's derived fields (mid/obi/
-- spread_pct are rust_decimal DIVISION results) can carry up to 28
-- significant digits. The pre-hardening copies either indexed POW10 past its
-- 18 entries (dec_cmp/dec_round2 → nil arithmetic → the adapter swallowed the
-- crashed evaluation as "no intents") or parsed the mantissa as a float.
-- dec_parse caps at 18 significant digits, dec_rescale truncates toward -inf
-- when the target scale is BELOW the value's, dec_cmp caps the common scale,
-- and dec_round2/dec_floor2 fall back to float exponentiation only past the
-- POW10 table. For every input with ≤ 18 significant digits — the entire
-- domain these strategies were written for — all functions are bit-identical
-- to the pre-hardening copies.

local POW10 = { [0] = 1 }
for i = 1, 18 do POW10[i] = POW10[i - 1] * 10 end

--- Parse a canonical decimal string ("0.4123", "-0.5", "0"). `nil` on
--- anything else — the caller's nil-guard mirrors the Rust `dec()?` chain.
local function dec_parse(str)
  if type(str) ~= "string" then return nil end
  local t = str:match("^%s*(.-)%s*$")
  if t == "" then return nil end
  local neg = false
  local c = t:sub(1, 1)
  if c == "-" then neg = true; t = t:sub(2)
  elseif c == "+" then t = t:sub(2) end
  local int_part, frac_part = t:match("^(%d*)%.?(%d*)$")
  if not int_part or (int_part == "" and frac_part == "") then return nil end
  -- Host decimals can carry up to 28 significant digits (rust_decimal
  -- division output — obi/spread_pct). Keep the mantissa inside i64 by
  -- capping significant digits at 18: drop frac digits beyond what the
  -- integer part leaves room for. The 1e-12-scale wobble this can add is
  -- orders of magnitude below every threshold compared here.
  local keep = 18 - #int_part
  if #frac_part > keep then frac_part = frac_part:sub(1, keep) end
  local m = tonumber(int_part .. frac_part)
  if m == nil then return nil end
  local s = #frac_part
  if neg and m ~= 0 then m = -m end
  return { m = m, s = s }
end

--- rust_decimal `Display`: scale preserved exactly (zero included).
local function dec_fmt(d)
  local m, s = d.m, d.s
  local neg = m < 0
  if neg then m = -m end
  local digits = tostring(m)
  if s > 0 then
    if #digits <= s then digits = string.rep("0", s - #digits + 1) .. digits end
    return (neg and "-" or "") .. digits:sub(1, #digits - s) .. "." .. digits:sub(#digits - s + 1)
  end
  return (neg and "-" or "") .. digits
end

--- The shared alignment cap of dec_add/dec_sub/dec_cmp. The fee curve is the
--- live path here: `fee_per_share` MULTIPLIES scales (rate.s + 2·2·p.s), so a
--- 4-decimal ask (the venues' real quote grid, e.g. 0.3450) against the
--- legacy rate's 3 puts the fee at scale 19 and an unclamped alignment would
--- rescale both operands past POW10[18] — the mantissa wraps NEGATIVE
--- (9.9e18 > i64 max) before `POW10[total.s]` is even indexed, and the
--- wrapped total would slip every `< max_pair_cost` test. Truncation past
--- the 18th significant digit costs at most 1e-18 USD — orders of magnitude
--- below every threshold compared here (the tightest sits at the 3rd
--- decimal, max_pair_cost = 0.995).
local DEC_MAX_SCALE = 18

local function dec_rescale(d, s)
  -- Total: the host's rust_decimal fields (obi, spread_pct, depth sums) are
  -- DIVISION results carrying up to 28 significant digits, so s can sit
  -- BELOW d.s. Truncate toward -inf in that direction (an 1e-18-scale wobble
  -- is orders of magnitude below every threshold compared here); the
  -- multiply path stays exact while the shift fits POW10.
  if s >= d.s then return d.m * POW10[s - d.s] end
  return d.m // POW10[d.s - s]
end

local function dec_add(a, b)
  local s = math.max(math.min(a.s, DEC_MAX_SCALE), math.min(b.s, DEC_MAX_SCALE))
  return { m = dec_rescale(a, s) + dec_rescale(b, s), s = s }
end

local function dec_sub(a, b)
  local s = math.max(math.min(a.s, DEC_MAX_SCALE), math.min(b.s, DEC_MAX_SCALE))
  return { m = dec_rescale(a, s) - dec_rescale(b, s), s = s }
end

local function dec_mul(a, b)
  return { m = a.m * b.m, s = a.s + b.s }
end

local function dec_cmp(a, b)
  -- Cap the common scale (DEC_MAX_SCALE above): aligning a 28-digit obi
  -- against a 2-digit threshold would want POW10[26], which does not exist.
  -- Truncation past the 18th significant digit cannot flip a comparison
  -- whose tightest threshold sits at the 2nd decimal.
  local s = math.max(math.min(a.s, DEC_MAX_SCALE), math.min(b.s, DEC_MAX_SCALE))
  local am, bm = dec_rescale(a, s), dec_rescale(b, s)
  if am < bm then return -1 elseif am > bm then return 1 else return 0 end
end

--- Floor to the venue's 0.01 share grid (sizes are positive; integer
--- division floors).
local function dec_floor2(d)
  if d.s <= 2 then return { m = d.m * POW10[2 - d.s], s = 2 } end
  local div = POW10[d.s - 2] or 10 ^ (d.s - 2)
  return { m = d.m // div, s = 2 }
end

--- rust_decimal `round_dp(2)` — MidpointNearestEven, the exact `round2`.
--- The kernel's derived fields (mid/obi are rust_decimal RATIOS) can carry
--- up to 28 significant decimals, whose mantissa overflows Lua integers and
--- parses as a float — coerce the tiny result mantissa back to an integer so
--- dec_fmt never prints a float ("93.0").
local function dec_round2(d)
  if d.s <= 2 then return { m = d.m * POW10[2 - d.s], s = 2 } end
  local div = POW10[d.s - 2] or 10 ^ (d.s - 2)
  local q = d.m // div
  local r = d.m - q * div
  local twice = r * 2
  if twice > div or (twice == div and q % 2 == 1) then q = q + 1 end
  q = math.tointeger(q) or q
  return { m = q, s = 2 }
end

local function dec_param(params, name, fallback)
  local raw = params[name]
  if type(raw) ~= "string" then return fallback end
  local d = dec_parse(raw)
  return d or fallback
end

local function int_param(params, name, fallback)
  local raw = params[name]
  if type(raw) ~= "string" then return fallback end
  local n = tonumber(raw)
  if n == nil or n ~= math.floor(n) then return fallback end
  return math.floor(n)
end

-- END common/decimal.lua

-- ── state (per state machine; the host runs one Lua VM per strategy) ────────
-- Per-slot state is keyed BY the round's own slot value (not held in one
-- mutable "current slot" cell): the kernel rebuilds its round object around
-- the capture's ~5s poll, and a slot-identity cell that trusts `slot ~= last`
-- measurably resets mid-round (the entry window closed itself the moment
-- time_left first dipped under the threshold). Keyed tables are immune to
-- representation wobble AND keep the flip's budget per (slot, condition).
local seen_left = {}        -- slot -> max time_left_sec observed for it (int)
local attempts = {}         -- condition_id -> re-quotes spent (a condition is ONE round-market)
local filled_seen = {}      -- condition_id -> true: a fill happened
local peak_proceeds = {}    -- token -> best (bid − taker fee) seen while held
local round_duration_cache = nil -- the largest time_left seen (the tighten seed)

-- The re-quote budget: each MakerThenTaker escalation is a fresh FOK at the
-- prevailing ask, so per-tick fill odds are ~coin-flip. The entry window
-- (time_left ≥ round_min_seen_time_left, ~the first 15s of the round's own
-- clock) already bounds the re-quotes; the cap is the belt for a flip whose
-- book died inside the window. A spent budget is a flip the corpus could not
-- fill; it shows up as a rejected intent, never as a silent skip.
local MAX_REQUOTES = 400

-- ── the coin (#962) ─────────────────────────────────────────────────────────
-- FNV-1a-64 constants as HEX literals: Lua 5.4 wraps hex integer literals
-- modulo 2^64 (decimal overflow would become a float — never used here).
local FNV_OFFSET = 0xcbf29ce484222325
local FNV_PRIME = 0x100000001b3

--- Pure deterministic coin: "up" | "down" for (seed, slot, condition).
--- Same inputs → same side, forever, on any machine, in any call order.
local function coin_flip(seed, slot, condition)
    local key = tostring(seed) .. "\0" .. tostring(slot) .. "\0" .. tostring(condition)
    local h = FNV_OFFSET
    for i = 1, #key do
        h = h ~ string.byte(key, i)
        h = h * FNV_PRIME
    end
    -- splitmix64 finalizer (avalanche): consecutive slots/conditions must not
    -- produce correlated flips.
    h = h ~ (h >> 30)
    h = h * 0xbf58476d1ce4e5b9
    h = h ~ (h >> 27)
    h = h * 0x94d049bb133111eb
    h = h ~ (h >> 31)
    if h % 2 == 0 then
        return "up"
    end
    return "down"
end

local function coin_seed()
    local n = tonumber(bk.params().coin_seed)
    if n == nil or n < 0 or n ~= math.floor(n) then
        return 962 -- the declared default; a broken param reverts to it
    end
    return math.tointeger(n) or 962
end

local function int_tunable(name, fallback)
    local n = tonumber(bk.params()[name])
    if n == nil or n < 0 or n ~= math.floor(n) then
        return fallback
    end
    return math.tointeger(n) or fallback
end

-- ── exit economics (best bid, never the mid) ────────────────────────────────
-- `rate * (p*(1-p))^exponent` USD per share — the same expression
-- `FeeSchedule::fee_per_share` settles in. `nil` outside (0, 1): a corrupt
-- quote is not a sell.

local DEC_0 = { m = 0, s = 0 }
local DEC_100 = { m = 100, s = 0 }
local DEFAULT_STOP = { m = 20, s = 0 }
local DEFAULT_TP = { m = 30, s = 0 }
local DEFAULT_TP_TIGHT = { m = 5, s = 0 }
local DEFAULT_ARM = { m = 25, s = 0 }
local DEFAULT_GIVE = { m = 10, s = 0 }
local DEFAULT_FEE = { m = 7, s = 2 } -- 0.07, the published crypto taker rate
local DEC_THOUSAND = { m = 1000, s = 0 }

--- `a / b` truncated to 9 decimal places (nil on zero/negative denominator).
local function dec_div(a, b)
    if b == nil or b.m <= 0 then
        return nil
    end
    local q = (a.m * 1000000000) // b.m
    return { m = q, s = 9 - a.s + b.s }
end

--- Band tightener: base at round start → tight as time_left → 0, linear.
--- Fraction = (duration − time_left) / duration with duration = the largest
--- time_left seen this round. A tight >= base stays flat (a wrong config
--- cannot loosen). Same algorithm as lua_momentum v0.4.0's tighten().
local function tighten(base_pct, tight_pct, round)
    if tight_pct == nil or tight_pct.m <= 0 or round_duration_cache == nil then
        return base_pct
    end
    if dec_cmp(base_pct, tight_pct) <= 0 then
        return base_pct
    end
    local dur = { m = round_duration_cache, s = 0 }
    local left = { m = round.time_left_sec, s = 0 }
    local frac = dec_div(dec_sub(dur, left), dur) -- elapsed 0..1
    if frac == nil then
        return base_pct
    end
    local delta = dec_mul(dec_sub(base_pct, tight_pct), frac)
    local v = dec_sub(base_pct, delta)
    if dec_cmp(v, tight_pct) < 0 then
        return tight_pct
    end
    return v
end

local function fee_per_share(p, rate, exponent)
    local one_minus = dec_sub({ m = 1, s = 0 }, p)
    if p.m <= 0 or one_minus.m <= 0 then
        return nil
    end
    local base = dec_mul(p, one_minus)
    local acc = { m = 1, s = 0 }
    for _ = 1, exponent do
        acc = dec_mul(acc, base)
    end
    return dec_mul(rate, acc)
end

--- The fee the exit decision subtracts from the bid — lua_momentum's
--- decision_fee, same haircut: the strategy's own exit_fee_rate model
--- (default 0.07, exponent 1) vs the live schedule, whichever is larger.
local function decision_fee(bid)
    local params = bk.params()
    local official_rate = dec_param(params, "exit_fee_rate", DEFAULT_FEE)
    if official_rate.m < 0 then
        official_rate = DEFAULT_FEE
    end
    local official = fee_per_share(bid, official_rate, 1)
    local sched = nil
    local fees = bk.fees and bk.fees() or nil
    if type(fees) == "table" then
        local rate = dec_parse(fees.rate)
        local exp = fees.exponent
        if rate and rate.m > 0 and type(exp) == "number" and exp >= 1
            and exp == math.floor(exp) and exp <= 8 then
            sched = fee_per_share(bid, rate, exp)
        end
    end
    if official and sched then
        if dec_cmp(sched, official) > 0 then
            return sched
        end
        return official
    end
    return official or sched
end

--- `(proceeds − entry) / entry * 100 >= pct`, without a division.
local function net_reaches(proceeds, entry, pct)
    if entry.m <= 0 then
        return false
    end
    local lhs = dec_mul(dec_sub(proceeds, entry), DEC_100)
    local rhs = dec_mul(entry, pct)
    return dec_cmp(lhs, rhs) >= 0
end

--- Peak proceeds have given back at least `z` percentage points of net PnL.
local function gave_back(peak, cur, entry, z)
    if entry.m <= 0 then
        return false
    end
    local lhs = dec_mul(dec_sub(peak, cur), DEC_100)
    local rhs = dec_mul(entry, z)
    return dec_cmp(lhs, rhs) >= 0
end

local function held_index()
    local map = {}
    local hs = bk.holdings and bk.holdings() or {}
    if type(hs) ~= "table" then
        return map
    end
    for i = 1, #hs do
        local h = hs[i]
        if type(h) == "table" and type(h.token_id) == "string" then
            map[h.token_id] = h
        end
    end
    return map
end

local function forget_unheld(held)
    for tok in pairs(peak_proceeds) do
        if held[tok] == nil then
            peak_proceeds[tok] = nil
        end
    end
end

local function note_peak(token, proceeds)
    local prev = peak_proceeds[token]
    if prev == nil or dec_cmp(proceeds, prev) > 0 then
        peak_proceeds[token] = proceeds
    end
end

--- One exit suggestion for a held token, or nothing. Priority order (first
--- match wins, one exit per token per evaluate): hard stop → take-profit
--- band → trailing stop → time exit. Thesis death is NOT here: the runtime
--- injects it ahead of this table (see bk_thesis_death below).
local function consider_exit(out, row, round)
    local token = row.token_id
    local entry = dec_parse(row.entry_price)
    if not entry or entry.m <= 0 then
        return
    end
    local book = bk.book(token)
    if book == nil or book.fresh ~= true or book.best_bid == nil then
        return
    end
    local bid = dec_parse(book.best_bid)
    if not bid or bid.m <= 0 then
        return
    end
    local fee = decision_fee(bid)
    if not fee then
        return
    end
    local proceeds = dec_sub(bid, fee)
    note_peak(token, proceeds)
    local peak = peak_proceeds[token]
    local params = bk.params()

    -- Duration seed for the band tightener: the largest time_left seen this
    -- round IS the round length (a healthy replay's first evaluate of a slot
    -- sees the full clock).
    if round_duration_cache == nil or round.time_left_sec > round_duration_cache then
        round_duration_cache = round.time_left_sec
    end

    -- 1. hard stop — the single-trade max loss, on NET proceeds: a nominal
    --    -X% that fees eat to -X-δ% still counts (more conservative than the
    --    nominal bid). 0 = off.
    local stop = dec_param(params, "hard_stop_loss_pct", DEFAULT_STOP)
    if stop.m > 0 then
        -- proceeds <= entry * (100 - X)/100 (integer-scaled, no negative scale)
        local floor_entry = dec_mul(entry, { m = 100 - stop.m, s = 2 })
        if dec_cmp(proceeds, floor_entry) <= 0 then
            out.exits[#out.exits + 1] = { token = token, reason = "hard_stop_loss" }
            return
        end
    end

    -- 2. take-profit band, time-weighted: base → *_tight as the clock drains.
    local tp = tighten(dec_param(params, "take_profit_pct", DEFAULT_TP),
        dec_param(params, "take_profit_pct_tight", DEFAULT_TP_TIGHT), round)
    if tp.m > 0 and net_reaches(proceeds, entry, tp) then
        out.exits[#out.exits + 1] = { token = token, reason = "take_profit", use_maker = true }
        return
    end

    -- 3. trailing stop: armed on peak net gain, banks the giveback.
    local arm = tighten(dec_param(params, "trail_arm_pct", DEFAULT_ARM),
        dec_param(params, "trail_arm_pct_tight", DEC_0), round)
    local give = tighten(dec_param(params, "trail_giveback_pct", DEFAULT_GIVE),
        dec_param(params, "trail_giveback_pct_tight", DEC_0), round)
    if arm.m > 0 and give.m > 0 and peak ~= nil
        and net_reaches(peak, entry, arm)
        and gave_back(peak, proceeds, entry, give) then
        out.exits[#out.exits + 1] = { token = token, reason = "trailing_stop", use_maker = true }
        return
    end

    -- 4. the deadline — beats the kernel's expiry gate so every position
    --    closes INSIDE the measured window (a rigid rule both arms share).
    local tleft = int_tunable("exit_time_left_sec", 45)
    if tleft > 0 and round.time_left_sec <= tleft then
        out.exits[#out.exits + 1] = { token = token, reason = "time_exit" }
    end
end

-- ── thesis death (#961) — the declaration the CI gate scans for ─────────────
--
-- THESIS DEATH DECLARATION (#961)
--   entry thesis : none — the entry is a coin flip on (seed, slot,
--                  condition); the position's only edge claim is "the exit
--                  mechanism harvests after a random entry", so there is no
--                  directional premise to flip.
--   death cond 1 : 盘口瞬时反转 — stdlib default (bk.thesis_death_default):
--                  the held token's own bid retraced >=
--                  thesis_death_reversal_pct (default 25) off entry.
--   death cond 2 : 论点依赖字段消失/超时 — stdlib default: the held token's
--                  book vanished, both sides unquoted, or quote age >
--                  thesis_death_stale_ms (default 30000; 0 disables the
--                  timeout, vanished book/price still fires).
--   death cond 3 : 轮次时间加权越界 — stdlib default: the reversal band
--                  slides linearly to thesis_death_reversal_pct_tight as the
--                  round drains (0 = flat band, the shipped default).
--   enforcement  : runtime-injected thesis_death_exit（不由本策略 exits 表执行）
--
-- A coin flip cannot be "right", so the lab's Form-A predicate adds nothing
-- beyond the stdlib's three reverse conditions — delegating IS the honest
-- declaration (a lab-specific death condition would be entry alpha smuggled
-- back in through the exit door).
function bk_thesis_death(h)
    if bk.thesis_death_default(h) then
        return true
    end
    return false
end

-- ── suggested entry points (§6.5) ───────────────────────────────────────────

-- Round start: nothing to reset — the per-slot state is keyed by the slot
-- value itself and the entry window bounds its lifetime. The flip keys off
-- (seed, slot, condition) alone; nothing here reads prices.
function bk_on_round(_round)
end

-- REQUIRED. Return the intent tables; the kernel adjudicates everything.
function bk_evaluate()
    local out = { entries = {}, exits = {}, breaks = {} }

    local round = bk.round()
    if round == nil then
        return out -- between rounds: nothing to trade
    end

    if round.time_left_sec > (seen_left[round.slot] or -1) then
        seen_left[round.slot] = round.time_left_sec
    end
    if round_duration_cache == nil or round.time_left_sec > round_duration_cache then
        round_duration_cache = round.time_left_sec
    end

    local held = held_index()
    forget_unheld(held)

    -- Which conditions hold something (either leg): a filled condition is
    -- done for this slot (one flip, one position, no re-entry after an exit).
    local held_condition = {}
    for _, h in pairs(held) do
        if type(h.condition_id) == "string" then
            held_condition[h.condition_id] = true
            filled_seen[h.condition_id] = true
        end
    end

    -- Deterministic exit order (#962 acceptance 1): Lua 5.4 seeds string
    -- hashes per VM, so `pairs()` over the token-keyed map iterates in a
    -- process-dependent order — the exit SUGGESTION order (and therefore the
    -- close order, the breaker's loss sequence, and trades_sha256) must not.
    -- Sort by token_id and walk the array; the per-row economics are
    -- order-independent, only the emission sequence needed pinning.
    local held_rows = {}
    for _, row in pairs(held) do
        held_rows[#held_rows + 1] = row
    end
    table.sort(held_rows, function(a, b)
        return a.token_id < b.token_id
    end)
    for _, row in ipairs(held_rows) do
        consider_exit(out, row, round)
    end

    -- Entries stay outside the force-exit window and inside the ENTRY WINDOW:
    -- the first seconds of the round's own clock (time_left ≥
    -- round_min_seen_time_left, default 885 of 900). The kernel's current
    -- round appears when the capture's poll lands (~5s into the slot), so the
    -- window is where a flip is placed and filled — uniformly, every round,
    -- with no price reading. A partial round at a corpus edge never sees the
    -- window and is never traded; the declared round selection is "every
    -- round-market whose entry window the replay observed".
    local cutoff = int_tunable("entry_cutoff_sec", 90)
    if cutoff > 0 and round.time_left_sec <= cutoff then
        return out
    end
    local min_seen = int_tunable("round_min_seen_time_left", 885)
    if round.time_left_sec < min_seen then
        return out
    end

    local seed = coin_seed()
    for _, market in ipairs(bk.markets()) do
        local condition = market.condition_id
        local up_tok = market.up_token
        local down_tok = market.down_token
        if type(condition) == "string" and up_tok ~= nil and down_tok ~= nil
            and not held_condition[condition]
            and filled_seen[condition] == nil
            and (attempts[condition] or 0) < MAX_REQUOTES
        then
            local side = coin_flip(seed, round.slot, condition)
            local token = side == "up" and up_tok or down_tok
            local book = bk.book(token)
            -- §6.6: price ONLY off a fresh, two-sided book; the flip lifts
            -- the token's own ask (taker-style entry, entry_maker=false).
            --
            -- The entry RE-QUOTES every evaluate until the fill is observed:
            -- the MakerThenTaker escalation is a FOK priced at the order's own
            -- limit against the book at the escalation tick (never the
            -- decision-time quote), so an at-the-ask quote dies on any
            -- up-tick — a one-shot entry filled only ~half the flips
            -- (measured). The re-quote refreshes the limit to the prevailing
            -- ask; the fill happens at the price the book actually shows
            -- (FOK VWAP ≤ limit), and the quote AT the ask never crosses as
            -- post-only. The flip itself stays ONE per (slot, condition): the
            -- direction is the hash's, only the limit price refreshes. The
            -- engine cycle runs escalations BEFORE the same-tick evaluate, so
            -- a fill is always visible to the re-quote guard — no double
            -- entry (and the OME dedups a live order's internal key anyway).
            if book ~= nil and book.fresh == true and book.best_ask ~= nil then
                local ask = tonumber(book.best_ask)
                if ask ~= nil and ask > 0 and ask < 1 then
                    attempts[condition] = (attempts[condition] or 0) + 1
                    out.entries[#out.entries + 1] = {
                        token = token,
                        price = book.best_ask,
                        reason = "coin flip " .. side .. " seed=" .. seed,
                    }
                end
            end
        end
    end
    return out
end
