-- hot_side_momentum — the confirmed-leader holder, as a Lua strategy package
-- (§6.5 contract).
--
-- One sentence: when a binary round is 40%-70% through, the Binance spot
-- momentum CONFIRMS the market's leading side (the one ask-priced in
-- [`min_leading_price`, `max_leading_price`] — default 0.60-0.80), buy that
-- side at its ask and HOLD TO SETTLEMENT — the winner redeems $1.00, the
-- loser zero; there is no exit decision, only a collection at expiry.
--
-- The pieces:
--   * PROGRESS — `progress = 1 - time_left_sec/round_sec`, exact integer
--     compare (`time_left*100` vs `30*round_sec` / `60*round_sec`): inside
--     the window the round has a settled leader but enough life left for
--     the momentum to matter. `round_sec` is a tunable (900 default) so the
--     same package runs 15m and 5m rounds (--round-sec passthrough sets it).
--   * MOMENTUM — CLOSED sec1 bars keyed by ASSET ("BTC"/"ETH"/…), oldest-
--     in-window → latest close, |move| >= `mom_threshold_pct` with REAL
--     coverage (span >= window - 1s, so a two-print burst cannot fabricate
--     a trigger). The move must ALIGN with the leader: spot rising confirms
--     an UP leader, falling confirms a DOWN leader. A contradiction is not
--     a trade.
--   * LEADER — determined by the ASKS (the price actually paid): the
--     strictly higher ask is the leader, and only when it sits inside the
--     band. Equal asks = no leader = no trade. Both books must be FRESH
--     with real asks — a stale side is an outage, not a signal.
--   * COLLECTION — the manifest declares `holds_to_settlement`: the position
--     rides to resolution and the winner redeems $1.00/share. This file
--     computes no exit, monitors no exit, expresses no exit (the「止损不归
--     你管」seal): `exits` is always empty, `breaks` always empty.
--   * FEES — not priced here: the trigger is progress+leader+momentum; the
--     entry fee is a cost the kernel charges on the fill and the replay
--     measures net. (The pair-discount edge needed the fee curve; this
--     strategy's acceptance is WR/PF over the whole hold.)
--
-- Data notes:
--   * Prices run on SCALED INTEGERS (the §6.5 decimal-STRING wire rule).
--     Every comparison is an exact integer comparison — no float knife-edge.
--   * One entry ATTEMPT per condition per round (`armed` latch): the kernel
--     dedups per token, and hold-to-settlement has no reason to average in.
--   * No spot data → no trigger → no entries: fail-closed by construction.

-- ── exact decimal helpers (flash_arb's, unchanged semantics) ────────────────

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
local DEC_100 = { m = 100, s = 0 }

-- ── configuration (bk.params() — manifest tunables) ─────────────────────────

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
    round_sec = int_param(p, "round_sec", 900),
    mom_window_sec = int_param(p, "mom_window_sec", 15),
    mom_threshold_pct = dec_param(p, "mom_threshold_pct", { m = 10, s = 2 }),
    mom_max_age_ms = int_param(p, "mom_max_age_ms", 2000),
    min_leading_price = dec_param(p, "min_leading_price", { m = 60, s = 2 }),
    max_leading_price = dec_param(p, "max_leading_price", { m = 80, s = 2 }),
  }
end

-- ── spot closes and the momentum latch ──────────────────────────────────────
-- asset -> { {ts, dec_close}, ... } of CLOSED sec1 bars, oldest first.
-- triggers[asset] = { dir_up, ts, move_pct } — latched by the latest bar
-- whose window move crossed the threshold; staleness is enforced at evaluate
-- time against `now`, never by untriggering. KEYED BY ASSET, deliberately:
-- the engine folds BOTH spot prints (keyed "BTC") and venue book mids (keyed
-- by token id) into the same aggregator — the same reasoning as flash_arb.

local spot_closes = {}
local triggers = {}
-- condition_id -> round_slot, set when the leader entry is emitted; cleared
-- whenever the round slot moves on (one attempt per condition per round).

local armed = {}

function bk_on_kline(k)
  if k.interval ~= "sec1" or k.is_closed ~= true then return end
  local close = dec_parse(k.close)
  if not close or close.m <= 0 then return end
  local cfg = read_config()
  local arr = spot_closes[k.symbol]
  if not arr then arr = {}; spot_closes[k.symbol] = arr end
  arr[#arr + 1] = { ts = k.close_time_ms, p = close }
  local cutoff = k.close_time_ms - (cfg.mom_window_sec + 5) * 1000
  local kept, n = {}, 0
  for i = 1, #arr do
    if arr[i].ts >= cutoff then
      n = n + 1
      kept[n] = arr[i]
    end
  end
  spot_closes[k.symbol] = kept

  -- move over the window = oldest-in-window → this close, with REAL
  -- coverage (span >= window - 1s) so a two-print burst cannot fabricate a
  -- trigger. |move| >= thr% ⟺ |diff| * 100 >= thr * oldest — the exact
  -- scaled-integer compare, no float knife-edge.
  local win_cutoff = k.close_time_ms - cfg.mom_window_sec * 1000
  local oldest, oldest_ts
  for i = 1, #kept do
    if kept[i].ts >= win_cutoff then
      oldest = kept[i].p
      oldest_ts = kept[i].ts
      break
    end
  end
  if not oldest or oldest.m <= 0 then return end
  if k.close_time_ms - oldest_ts < (cfg.mom_window_sec - 1) * 1000 then return end
  local diff = dec_sub(close, oldest)
  local adiff = diff.m < 0 and { m = -diff.m, s = diff.s } or diff
  local lhs = dec_mul(adiff, DEC_100)
  local rhs = dec_mul(oldest, cfg.mom_threshold_pct)
  if dec_cmp(lhs, rhs) >= 0 then
    local move_pct = (diff.m / (POW10[diff.s] * oldest.m)) * 100
    triggers[k.symbol] = { dir_up = diff.m > 0, ts = k.close_time_ms, move = move_pct }
  end
end

-- ── entry points (§6.5) ─────────────────────────────────────────────────────

--- The suggestion surface. At most ONE entry per condition per round: the
--- leading side at its own ask, only while the progress window holds, the
--- momentum confirms the leader, and the leader's ask sits in the band.
--- `exits` is NEVER populated (the seal — collection happens at settlement);
--- there is no resting order, so `breaks` is always empty.
function bk_evaluate()
  local cfg = read_config()
  local entries = {}

  local round = bk.round()
  if round == nil then
    return { entries = entries, exits = {}, breaks = {} }
  end
  local slot = round.slot

  -- A new round retires every latch: conditions do not survive their round.
  for cond, s in pairs(armed) do
    if s ~= slot then armed[cond] = nil end
  end

  -- PROGRESS window, exact integers: 40% through ⟺ time_left <= 60% of the
  -- round; 70% through ⟺ time_left >= 30% of the round. Inclusive ends.
  local t = round.time_left_sec
  local in_progress_window = t >= 0
    and t * 100 <= 60 * cfg.round_sec
    and t * 100 >= 30 * cfg.round_sec
  if not in_progress_window then
    return { entries = entries, exits = {}, breaks = {} }
  end
  local now_ms = bk.now_ms()

  for _, m in ipairs(bk.markets()) do
    if armed[m.condition_id] ~= slot then
      local trig = triggers[m.asset]
      -- The momentum must be FRESH: a stale trigger describes a market that
      -- has already repriced.
      local fresh_trig = trig and (now_ms - trig.ts <= cfg.mom_max_age_ms)
      if fresh_trig then
        local bu = bk.book(m.up_token)
        local bd = bk.book(m.down_token)
        if bu and bu.fresh == true and bd and bd.fresh == true then
          local au = dec_parse(bu.best_ask)
          local ad = dec_parse(bd.best_ask)
          if au and ad and au.m > 0 and ad.m > 0 then
            -- LEADER by the ask (the price actually paid): strictly higher
            -- ask leads; equal asks = no leader.
            local leader_up = nil
            if dec_cmp(au, ad) > 0 then leader_up = true
            elseif dec_cmp(ad, au) > 0 then leader_up = false
            end
            if leader_up ~= nil then
              local leader_ask = leader_up and au or ad
              local in_band = dec_cmp(leader_ask, cfg.min_leading_price) >= 0
                and dec_cmp(leader_ask, cfg.max_leading_price) <= 0
              -- ALIGNMENT: spot rising confirms an UP leader, falling a DOWN
              -- leader. A contradiction is not a trade.
              local aligned = trig.dir_up == leader_up
              if in_band and aligned then
                local tok = leader_up and m.up_token or m.down_token
                local side = leader_up and "UP" or "DOWN"
                entries[#entries + 1] = {
                  token = tok,
                  price = dec_fmt(leader_ask),
                  reason = string.format(
                    "hot %s: spot %s%.2f%% in %ds confirms leader at ask %s, %ds left",
                    side,
                    trig.dir_up and "+" or "-",
                    math.abs(trig.move),
                    cfg.mom_window_sec,
                    dec_fmt(leader_ask),
                    t
                  ),
                }
                -- One attempt per condition per round: hold-to-settlement has
                -- no reason to average in.
                armed[m.condition_id] = slot
              end
            end
          end
        end
      end
    end
  end
  return { entries = entries, exits = {}, breaks = {} }
end
