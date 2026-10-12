-- flash_arb — the spot-to-venue lag catcher, as a Lua strategy package
-- (§6.5 contract).
--
-- One sentence: when the Binance spot price of the round's asset moves
-- >= `mom_threshold_pct` inside `mom_window_sec`, the venue's maker queue is
-- still priced on the OLD probability for a breath — buy the side the move
-- implied, as a POST_ONLY maker at the current opposite (ask) price, while
-- that price is still <= `lag_max_price`. This is not a direction predictor;
-- it does not fade the move — it joins it before the venue reprices.
--
-- The seal (the「止损不归你管」contract, gated by strategy-no-stop-loss-check):
-- this file computes no exit, monitors no exit, expresses no exit, and sends
-- no size. Entries cross as { token, price, reason } suggestions — the kernel
-- adjudicates, sizes (shares omitted → kernel budget), funds-gates and posts
-- them post_only. `exits` is never populated; there is no resting order to
-- break (the entry suggests a fresh post_only quote each evaluation, and the
-- kernel owns order lifecycle), so `breaks` is always empty too.
--
-- Data notes, stated because they change what the gates can see:
--   * Prices run on SCALED INTEGERS (the §6.5 decimal-STRING wire rule, same
--     helpers as spread_arb/mad_dog). The only float divisions are single
--     price-move ratios compared against 1-2 decimal thresholds (market-
--     derived rationals cannot disagree with a double below the 15th
--     significant digit).
--   * The trigger reads CLOSED sec1 bars keyed by ASSET ("BTC"/"ETH"/…),
--     which the E29 engine aggregator feeds from the same data path that
--     reaches `on_data` — in replay, the corpus's `spot` lines. No spot data
--     → no trigger → no entries: fail-closed by construction, so unlike
--     mad_dog there is no `spot_missing` knob to disclose.
--   * The lag window (`lag_max_ms`, default 1500) runs on bar CLOSE times,
--     so its effective granularity is the sec1 cadence: a trigger is seen at
--     its bar close and stays fresh for at most the next two closes.
--   * The lag gate prices the side actually paid (`best_ask`, rounded to the
--     venue tick): a book whose mid still looks cheap but whose ask has
--     already repriced is NOT lagging. `book.mid` is therefore not read.
--   * `bk.account()` is currently populated by no host path, so the
--     available-balance gate applies only when the view is present; the
--     kernel's funds path remains the real gate. Same-direction-position and
--     cooldown checks are kernel-owned (E25 arbitration / E26 breakers).
--   * `kline_stream` is deliberately NOT declared in manifest modes: the
--     plugin seam (extensions/polymarket) declares websocket_feed |
--     level2_snapshot | post_only, and the E29 kline feed is engine-side,
--     not a plugin capability. Declaring it would refuse this strategy at
--     enable time for a bit the seam does not carry.

-- ── exact decimal helpers (spread_arb's, unchanged semantics) ───────────────

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

local function dec_rescale(d, s)
  -- Total: the host's rust_decimal fields (obi, spread_pct, depth sums) are
  -- DIVISION results carrying up to 28 significant digits, so s can sit
  -- BELOW d.s. Truncate toward -inf in that direction (an 1e-18-scale wobble
  -- is orders of magnitude below every threshold compared here); the
  -- multiply path stays exact while the shift fits POW10.
  if s >= d.s then return d.m * POW10[s - d.s] end
  return d.m // POW10[d.s - s]
end

local DEC_CMP_MAX_SCALE = 18

local function dec_cmp(a, b)
  -- Cap the common scale: aligning a 28-digit obi against a 2-digit
  -- threshold would want POW10[26], which does not exist. Truncation past
  -- the 18th significant digit cannot flip a comparison whose tightest
  -- threshold sits at the 2nd decimal.
  local s = math.max(math.min(a.s, DEC_CMP_MAX_SCALE), math.min(b.s, DEC_CMP_MAX_SCALE))
  local am, bm = dec_rescale(a, s), dec_rescale(b, s)
  if am < bm then return -1 elseif am > bm then return 1 else return 0 end
end

local function dec_add(a, b)
  local s = math.max(a.s, b.s)
  return { m = dec_rescale(a, s) + dec_rescale(b, s), s = s }
end

local function dec_sub(a, b)
  local s = math.max(a.s, b.s)
  return { m = dec_rescale(a, s) - dec_rescale(b, s), s = s }
end

local function dec_mul(a, b)
  return { m = a.m * b.m, s = a.s + b.s }
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
local DEC_001 = { m = 1, s = 2 }
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
    mom_window_sec = int_param(p, "mom_window_sec", 2),
    mom_threshold_pct = dec_param(p, "mom_threshold_pct", { m = 25, s = 2 }),
    lag_max_ms = int_param(p, "lag_max_ms", 1500),
    lag_max_price = dec_param(p, "lag_max_price", { m = 55, s = 2 }),
    min_time_left_sec = int_param(p, "min_time_left_sec", 180),
    min_bid_depth = dec_param(p, "min_bid_depth", { m = 500, s = 0 }),
    max_abs_obi = dec_param(p, "max_abs_obi", { m = 30, s = 2 }),
    min_available_usd = dec_param(p, "min_available_usd", { m = 100, s = 2 }),
  }
end

-- ── spot closes and the trigger latch ───────────────────────────────────────
-- asset -> { {ts, dec_close}, ... } of CLOSED sec1 bars, oldest first.
-- triggers[asset] = { dir_up, ts_ms, move_pct } — latched by the latest bar
-- whose window move crossed the threshold; staleness is enforced at evaluate
-- time against `now`, never by untriggering. KEYED BY ASSET, deliberately:
-- the engine folds BOTH spot prints (keyed "BTC") and venue book mids (keyed
-- by token id) into the same aggregator, and an evaluate always trails a
-- book event — a single shared trigger would be token-keyed at exactly the
-- moment an evaluation reads it, and would then never match any market's
-- asset. Token-keyed latches land in `triggers` too but under keys no
-- `m.asset` lookup ever reads.

local spot_closes = {}
local triggers = {}

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

  -- move over the window = oldest-in-window → this close. Real coverage is
  -- required (span >= window - 1s) so a two-print burst cannot fabricate a
  -- trigger; this is mad_dog's spot gate arithmetic, promoted from veto to
  -- signal.
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
  -- |move| >= thr%  ⟺  |diff| * 100 >= thr * oldest — the same scaled-integer
  -- compare as mad_dog's panic-speed gate, so an exact-threshold move cannot
  -- land on the wrong side of a double.
  local adiff = diff.m < 0 and { m = -diff.m, s = diff.s } or diff
  local lhs = dec_mul(adiff, DEC_100)
  local rhs = dec_mul(oldest, cfg.mom_threshold_pct)
  if dec_cmp(lhs, rhs) >= 0 then
    local move_pct = (diff.m / (POW10[diff.s] * oldest.m)) * 100
    triggers[k.symbol] = { dir_up = diff.m > 0, ts = k.close_time_ms, move = move_pct }
  end
end

-- ── entry points (§6.5) ─────────────────────────────────────────────────────
-- No `bk_on_book`: the trigger carries all state this strategy needs, book
-- views are read fresh inside `bk_evaluate` (optional entry points are
-- tolerated by the loader; defining an empty one is noise).

--- The suggestion surface. One entry per triggered asset per evaluation: the
--- side the spot move implied, priced at the opposite (ask) price, only while
--- that price is still inside the lag band. Exits are NEVER emitted (the
--- seal); there is no resting order, so no breaks either.
function bk_evaluate()
  local cfg = read_config()
  local entries = {}

  local round = bk.round()
  if round == nil or round.time_left_sec < cfg.min_time_left_sec then
    return { entries = entries, exits = {}, breaks = {} }
  end
  local now_ms = bk.now_ms()

  for _, m in ipairs(bk.markets()) do
    local trig = triggers[m.asset]
    if trig and now_ms - trig.ts <= cfg.lag_max_ms then
      local tok = trig.dir_up and m.up_token or m.down_token
      local book = bk.book(tok)
      if book and book.fresh == true then
        local ask = dec_parse(book.best_ask)
        if ask and ask.m > 0 then
          local price = dec_round2(ask)
          if dec_cmp(price, DEC_001) >= 0 and dec_cmp(price, cfg.lag_max_price) <= 0 then
            local depth_ok = true
            local d = dec_parse(book.bid_depth)
            if d and dec_cmp(d, cfg.min_bid_depth) < 0 then depth_ok = false end
            local obi_ok = true
            local o = dec_parse(book.obi)
            if o then
              local abs = o.m < 0 and { m = -o.m, s = o.s } or o
              if dec_cmp(abs, cfg.max_abs_obi) > 0 then obi_ok = false end
            end
            local avail_ok = true
            local acct = bk.account()
            if acct and acct.available ~= nil then
              local a = dec_parse(acct.available)
              if a and dec_cmp(a, cfg.min_available_usd) < 0 then avail_ok = false end
            end
            if depth_ok and obi_ok and avail_ok then
              entries[#entries + 1] = {
                token = tok,
                price = dec_fmt(price),
                reason = string.format(
                  "%s spot %s%.2f%% in %ds, %s still at ask %s, %ds left",
                  m.asset,
                  trig.dir_up and "+" or "-",
                  math.abs(trig.move),
                  cfg.mom_window_sec,
                  trig.dir_up and "UP" or "DOWN",
                  dec_fmt(price),
                  round.time_left_sec
                ),
              }
            end
          end
        end
      end
    end
  end
  return { entries = entries, exits = {}, breaks = {} }
end
