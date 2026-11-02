-- postonly_pair — post-only bids on both Up and Down, or on neither.
--
-- Maker fee is 0. The taker formula shares*0.07*p*(1-p) is not added, and
-- the published 20% maker rebate is not counted. A fee schedule must still
-- be loaded so a silent "no fees" boot cannot pretend the edge is free.
-- Chainlink TWAP is how the venue settles; this package does not estimate
-- it. Complete pairs ask the kernel to merge. A naked leg is not chased
-- past max_unpaired_shares.

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

local TICK = { m = 1, s = 2 }
local GRID_FLOOR = { m = 1, s = 2 }
local ZERO = { m = 0, s = 2 }

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

local function read_config()
  local p = bk.params()
  return {
    max_pair_cost = dec_param(p, "max_pair_cost", { m = 985, s = 3 }),
    min_leg_price = dec_param(p, "min_leg_price", { m = 1, s = 2 }),
    max_leg_price = dec_param(p, "max_leg_price", { m = 99, s = 2 }),
    min_pair_shares = dec_param(p, "min_pair_shares", { m = 1, s = 0 }),
    max_pair_shares = dec_param(p, "max_pair_shares", { m = 1, s = 0 }),
    max_unpaired_shares = dec_param(p, "max_unpaired_shares", { m = 1, s = 0 }),
    min_time_left_sec = int_param(p, "min_time_left_sec", 30),
    cancel_before_sec = int_param(p, "cancel_before_sec", 20),
    requote_ms = int_param(p, "requote_ms", 15000),
    entry_reprice_ticks = int_param(p, "entry_reprice_ticks", 1),
    entry_window_pct = int_param(p, "entry_window_pct", 50),
    round_sec = int_param(p, "round_sec", 900),
  }
end

local function maker_quote(ask, cfg)
  local ticks = math.max(cfg.entry_reprice_ticks, 1)
  local q = dec_floor2(dec_sub(ask, dec_mul(TICK, { m = ticks, s = 0 })))
  if dec_cmp(q, GRID_FLOOR) < 0 then q = GRID_FLOOR end
  if dec_cmp(q, ask) >= 0 then return nil end
  if dec_cmp(q, cfg.min_leg_price) < 0 then return nil end
  return q
end

local function fee_schedule_live()
  local fees = bk.fees()
  if fees == nil then return false end
  local rate = dec_parse(fees.rate)
  local exponent = fees.exponent
  return rate ~= nil and rate.m >= 0 and type(exponent) == "number" and exponent >= 0
end

local function holding(token)
  local hs = bk.holdings and bk.holdings() or {}
  for _, h in ipairs(hs) do
    if h.token_id == token then
      local shares = dec_parse(h.shares) or ZERO
      local entry = dec_parse(h.entry_price) or ZERO
      return shares, entry
    end
  end
  return ZERO, ZERO
end

local function break_token(breaks, token)
  breaks[#breaks + 1] = { token = token, broken_price = "0" }
end

local armed = {}
local posted_at = {}

function bk_evaluate()
  local cfg = read_config()
  local entries, exits, breaks = {}, {}, {}
  local round = bk.round()
  if round == nil then
    return { entries = entries, exits = exits, breaks = breaks }
  end
  local slot = round.slot
  local now_ms = round.now_ms or bk.now_ms()
  for cond, s in pairs(armed) do
    if s ~= slot then
      armed[cond] = nil
      posted_at[cond] = nil
    end
  end

  local window_open = cfg.entry_window_pct <= 0
    or round.time_left_sec * 100 >= (100 - cfg.entry_window_pct) * cfg.round_sec
  local can_enter = round.time_left_sec >= cfg.min_time_left_sec
    and round.time_left_sec > cfg.cancel_before_sec
    and fee_schedule_live()
    and window_open

  for _, m in ipairs(bk.markets()) do
    local hu, eu = holding(m.up_token)
    local hd, ed = holding(m.down_token)
    if hu.m > 0 and hd.m > 0 then
      exits[#exits + 1] = { token = m.up_token, reason = "merge" }
      exits[#exits + 1] = { token = m.down_token, reason = "merge" }
    end

    if round.time_left_sec <= cfg.cancel_before_sec then
      if armed[m.condition_id] == slot then
        break_token(breaks, m.up_token)
        break_token(breaks, m.down_token)
        armed[m.condition_id] = nil
        posted_at[m.condition_id] = nil
      end
    elseif armed[m.condition_id] == slot then
      local age = now_ms - (posted_at[m.condition_id] or now_ms)
      if age >= cfg.requote_ms then
        break_token(breaks, m.up_token)
        break_token(breaks, m.down_token)
        armed[m.condition_id] = nil
        posted_at[m.condition_id] = nil
      end
    elseif can_enter then
      local up_ahead = dec_cmp(hu, hd) > 0
      local down_ahead = dec_cmp(hd, hu) > 0
      local gap = up_ahead and dec_sub(hu, hd) or dec_sub(hd, hu)
      local at_cap = hu.m > 0 or hd.m > 0
      if at_cap and dec_cmp(gap, cfg.max_unpaired_shares) >= 0 then
        -- Do not grow the heavy side. The light side may complete the pair
        -- only when the sunk cost plus the new quote stays under the cap.
        local bu = bk.book(up_ahead and m.down_token or m.up_token)
        local sunk = up_ahead and eu or ed
        local light_token = up_ahead and m.down_token or m.up_token
        local ask = bu and bu.fresh == true and dec_parse(bu.best_ask) or nil
        local quote = ask and maker_quote(ask, cfg) or nil
        if quote and dec_cmp(dec_add(sunk, quote), cfg.max_pair_cost) < 0 then
          entries[#entries + 1] = {
            token = light_token,
            price = dec_fmt(quote),
            shares = dec_fmt(dec_floor2(gap.m > 0 and gap or cfg.min_pair_shares)),
            reason = "postonly complete the short leg",
            pair = { key = m.condition_id .. ":" .. tostring(slot), leg = up_ahead and "down" or "up" },
          }
        else
          break_token(breaks, light_token)
        end
      elseif hu.m == 0 and hd.m == 0 then
        local bu = bk.book(m.up_token)
        local bd = bk.book(m.down_token)
        if bu and bu.fresh == true and bd and bd.fresh == true then
          local au = dec_parse(bu.best_ask)
          local ad = dec_parse(bd.best_ask)
          if au and ad and au.m > 0 and ad.m > 0
            and dec_cmp(au, cfg.min_leg_price) >= 0 and dec_cmp(au, cfg.max_leg_price) <= 0
            and dec_cmp(ad, cfg.min_leg_price) >= 0 and dec_cmp(ad, cfg.max_leg_price) <= 0 then
            local qu = maker_quote(au, cfg)
            local qd = maker_quote(ad, cfg)
            if qu and qd and dec_cmp(dec_add(qu, qd), cfg.max_pair_cost) < 0 then
              local du = dec_parse(bu.ask_depth)
              local dd = dec_parse(bd.ask_depth)
              if du and dd and du.m > 0 and dd.m > 0 then
                local depth = dec_floor2(dec_cmp(du, dd) < 0 and du or dd)
                local shares = dec_cmp(depth, cfg.max_pair_shares) < 0 and depth or cfg.max_pair_shares
                shares = dec_floor2(shares)
                if dec_cmp(shares, cfg.min_pair_shares) >= 0 then
                  local pair_key = m.condition_id .. ":" .. tostring(slot)
                  local reason = string.format(
                    "postonly pair %s (%s+%s) fee 0, %s shares, %ds left",
                    dec_fmt(dec_add(qu, qd)), dec_fmt(qu), dec_fmt(qd),
                    dec_fmt(shares), round.time_left_sec
                  )
                  entries[#entries + 1] = {
                    token = m.up_token, price = dec_fmt(qu), shares = dec_fmt(shares),
                    reason = reason, pair = { key = pair_key, leg = "up" },
                  }
                  entries[#entries + 1] = {
                    token = m.down_token, price = dec_fmt(qd), shares = dec_fmt(shares),
                    reason = reason, pair = { key = pair_key, leg = "down" },
                  }
                  armed[m.condition_id] = slot
                  posted_at[m.condition_id] = now_ms
                end
              end
            end
          end
        end
      end
    end
  end
  return { entries = entries, exits = exits, breaks = breaks }
end
