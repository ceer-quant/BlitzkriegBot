-- late_favorite — buy the 0.80-0.90 favorite in the last 45-120 seconds
-- and hold it to settlement. One attempt per condition per round. A
-- cancelled or filled attempt is not re-entered.
--
-- maker_entry 0 (default): cross the ask as a taker. The order reason
-- starts with "late-taker" so the kernel submits FillPolicy::Taker.
-- maker_entry 1: rest a post-only bid at the touch. The quote is cancelled
-- once fewer than min_time_left seconds remain.
--
-- The loader does not require a lua_ prefix. The registry id is the
-- directory name, which equals manifest.name: late_favorite.
--
-- THESIS DEATH DECLARATION (#961)
--   entry thesis : the side priced inside the ask/bid band with 45-120s
--                  left is the favorite and is held for the $1 redemption
--   death cond 1 : not applied — an intra-round bid drop is the variance
--                  the 0.80 entry already prices, not a dead thesis
--   death cond 2 : not applied — settlement does not need a live quote
--   death cond 3 : not applied — there is no time-tightened reversal band
--   enforcement  : bk_thesis_death returns false; the runtime injects no exit

function bk_thesis_death(_h)
    return false
end

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

local quoted = {}
local slot_seen = nil

-- int_param comes from the embedded common/decimal.lua block above (strict
-- integral-string parse, fallback on anything else). Only num_param is new
-- here: a plain-float read for the price-band tunables.

local function num_param(params, name, fallback)
    local n = tonumber(params[name])
    if n == nil then
        return fallback
    end
    return n
end

local function key_of(slot, cond)
    return tostring(slot) .. "\0" .. (cond or "")
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
            map[h.token_id] = true
        end
    end
    return map
end

function bk_on_round(round)
    if round ~= nil and round.slot ~= nil and round.slot ~= slot_seen then
        quoted = {}
        slot_seen = round.slot
    end
end

function bk_evaluate()
    local out = { entries = {}, exits = {}, breaks = {} }
    local round = bk.round()
    if round == nil or round.time_left_sec == nil or round.slot == nil then
        return out
    end
    if slot_seen ~= round.slot then
        quoted = {}
        slot_seen = round.slot
    end
    local params = bk.params()
    local maker = int_param(params, "maker_entry", 0) == 1
    local btc_only = int_param(params, "btc_only", 1) == 1
    local lo = num_param(params, "min_px", 0.80)
    local hi = num_param(params, "max_px", 0.90)
    local t_min = int_param(params, "min_time_left", 45)
    local t_max = int_param(params, "max_time_left", 120)
    local tl = round.time_left_sec
    local held = held_index()

    for _, market in ipairs(bk.markets()) do
        local cond = market.condition_id
        if cond ~= nil and (not btc_only or market.asset == "BTC" or market.asset == "btc") then
            local k = key_of(round.slot, cond)
            local st = quoted[k]
            local up_held = market.up_token ~= nil and held[market.up_token]
            local dn_held = market.down_token ~= nil and held[market.down_token]
            if st ~= nil and st.token ~= nil and held[st.token] then
                st.filled = true
            end
            if tl < t_min or tl > t_max then
                if maker and st ~= nil and st.token ~= nil and not st.filled and not st.cancelled then
                    out.breaks[#out.breaks + 1] = {
                        token = st.token,
                        broken_price = st.price,
                    }
                    st.cancelled = true
                end
            elseif st == nil and not up_held and not dn_held then
                local best = nil
                local legs = { market.up_token, market.down_token }
                for i = 1, #legs do
                    local token = legs[i]
                    local book = token ~= nil and bk.book(token) or nil
                    if book ~= nil and book.fresh == true then
                        local px = maker and book.best_bid or book.best_ask
                        local n = tonumber(px)
                        if n ~= nil and n >= lo and n <= hi and (best == nil or n > best.n) then
                            best = { token = token, price = px, n = n }
                        end
                    end
                end
                if best ~= nil then
                    quoted[k] = { token = best.token, price = best.price, filled = false, cancelled = false }
                    out.entries[#out.entries + 1] = {
                        token = best.token,
                        price = best.price,
                        reason = maker and "late-maker" or "late-taker",
                    }
                end
            end
        end
    end
    return out
end
