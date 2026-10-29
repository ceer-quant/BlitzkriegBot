-- market_maker — the passive spread harvester, now double-sided where the
-- kernel allows it (§6.5 contract).
--
-- One sentence: on a 15-minute binary round, qualifying cheap chips (mid
-- 0.20–0.42) whose books are deep, balanced and NOT one-sided earn resting
-- MAKER bids — one per qualifying side of ONE market, so a genuinely
-- two-sided cheap book arms both UP and DOWN — and every fill is then
-- worked out of the position with inventory skew: the held side's bid is
-- retired, the opposite bid survives only while the pair stays
-- merge-profitable, the taker close fires only above a fee-cleared profit
-- band, and a completed pair exits through the kernel's fee-free merge.
-- No direction forecast, no knife-catching, no chasing: the profit thesis
-- is volatility paying the spread, never the outcome paying the holder.
--
-- The seal (the「止损不归你管」contract, gated by strategy-no-stop-loss-check):
-- this file computes no exit discipline of its own beyond the take-profit
-- suggestion above (its fee band is a take-profit FLOOR, not a loss rule),
-- monitors no adverse excursion, and sends no size. The kernel owns
-- survival: its exit ladder (protective stop, time force-exit, profit
-- trailing — exit_policy.rs / user_layer/configs) runs underneath every
-- suggestion, and E26 owns the circuit breakers. `exits` carries only the
-- target-hit close and the merge collection; nothing else ever populates it.
--
-- Kernel channel facts this shape is built on (verified in core, disclosed
-- here once — the strategy layer cannot change them):
--   * `entries` land as Buy + MakerThenTaker (the engine hard-codes the
--     side), so a Lua package CANNOT rest a sell-side maker quote. The
--     dual side of this strategy is therefore the UP+DOWN pair of maker
--     BIDs, not bid+ask on one token.
--   * strategy exit intents are executed as TAKER sells (the kernel's
--     strategy-exit path pins use_maker=false and prices a marketable
--     limit). There is no maker exit on the exits channel — so the taker
--     exit below is guarded by a minimum-profit band: it fires only when
--     mid clears entry + spread + the round-trip taker fee, otherwise the
--     spread it banks would be handed straight back to the fee.
--   * an exit intent with reason exactly "merge" is intercepted BEFORE the
--     sell path: complete UP+DOWN share pairs of one condition burn into
--     their collateral at 1.00/pair with NO fee (a merge is not a market
--     order). That is the maker-grade exit this strategy owns, and the
--     reason a completed pair never touches the taker path.
--
-- The state machine (fill-blind by contract — §6.5 has no fill stream;
-- `bk.holdings()` is the ONE position observation, refreshed by the host
-- before every evaluation):
--   nil thesis            → scan markets; arm a maker bid for EACH
--                           qualifying side of the first qualifying market
--                           (one or two entries; the pairs book's complement
--                           keeps both mids in the cheap band only when the
--                           book genuinely prices the pair under par).
--   thesis, holdings FLAT → the fill-blind upkeep, per armed side:
--                             mid >= bid + spread → the bid's thesis ran
--                               away: break (retire resting bids; there is
--                               no position to close, so no exit is faked),
--                               then re-arm at once —「价差到手就平，立刻
--                               开下一笔」.
--                             the chip turns one-sided → break.
--   thesis/one side HELD  → single-side maintenance (inventory skew):
--                             the held side's resting bid is BROKEN, not
--                             re-priced — one internal key per
--                             (strategy, asset, direction, slot) means a
--                             resting bid cannot be moved, so cancellation
--                             is the honest「bid 价下移」(no second entry
--                             while inventory is open);
--                             the opposite bid survives only while
--                             entry_held + bid_other <= 1.00 -
--                             merge_min_profit — it IS the exit leg (its
--                             fill completes a pair the merge burns
--                             fee-free), so it must stay worth resting;
--                             the taker close fires only above the fee
--                             band: mid >= entry + take_profit_spread +
--                             exit_fee_margin_pct% of the round-trip
--                             taker fee (fee curve read live via
--                             bk.fees(); no schedule → fail closed, the
--                             position rides the kernel ladder instead).
--   BOTH legs HELD        → reason="merge" intents for both tokens; the
--                           kernel burns complete pairs at 1.00/pair
--                           fee-free (remainders ride its ladder and
--                           settlement — never this file's problem), both
--                           bids are broken, and no new thesis is armed
--                           until the holdings actually clear.
--   slot change           → break everything armed; holdings from an older
--                           slot are ignored (the kernel's ladder and the
--                           settlement path own them).
--   An own-slot holding WITHOUT a thesis (a restart between fill and
--   close) still gets the skew + fee-band upkeep — holdings, not the
--   thesis, are the source of truth for maintaining a position.
--
-- Conditions the spec names that no Lua view exposes are disclosed here
-- once and not faked:
--   * 未处于冷却期 — kernel-owned (E26 breakers); invisible here.
--   * account available — read only when the host surfaces a view (none
--     does today, same as mad_dog's bk.account note).
--   * a resting bid cannot be re-priced toward mid when inventory opens
--     (the duplicate-key rule above) — "ask 更靠近 mid" is therefore
--     realized as: keep the merge-completion bid resting at its original
--     aggressive price, cancel everything else.
--
-- Data notes (they change what the gates can see):
--   * Prices run on SCALED INTEGERS (the §6.5 decimal-STRING wire rule, same
--     helpers as spread_arb/mad_dog). The only float divisions are single
--     price-move ratios compared against 1–2 decimal thresholds.
--   * The underlying gate reads CLOSED sec1 bars keyed by ASSET from the E29
--     engine aggregator (Binance spot). Frozen replay corpora are book+round
--     only, so there the gate sees NO data; `spot_missing` decides that case
--     (default "block" — a strategy that cannot confirm the underlying is
--     calm does not buy; "pass" exists for book-only diagnostics and is
--     disclosed as such).
--   * `kline_stream` is deliberately NOT declared in manifest modes (same
--     reason as mad_dog: engine-side bit, not a plugin seam capability —
--     declaring it would refuse enable).
--   * The chip-trend gate needs 30s of the token's OWN mid history; a token
--     tracked for less than that has no "not one-sided" proof and stays
--     disarmed (fail-closed, no knob).
--
-- Samples are bucketed at 250ms per token (one sample per bucket), bounded
-- memory and bounded per-callback work for the §6.3 sandbox.

-- ── exact decimal helpers (spread_arb/mad_dog's, unchanged semantics) ───────

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
local DEC_001 = { m = 1, s = 2 }
local DEC_100 = { m = 100, s = 0 } -- the percent multiplier, NOT par
local DEC_ONE = { m = 1, s = 0 }   -- par ($1.00): fee base and merge cap

--- A kernel-derived book decimal (mid/obi are rust_decimal division results,
--- up to 28 decimals) rounded to the 2dp decision grid before any comparison
--- — keeps every downstream mantissa a small exact integer.
local function book_dec(str)
  local d = dec_parse(str)
  if not d then return nil end
  return dec_round2(d)
end

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
  local merge_min_profit = dec_param(p, "merge_min_profit", { m = 2, s = 2 })
  if merge_min_profit.m < 0 then merge_min_profit = { m = 2, s = 2 } end
  local fee_margin = dec_param(p, "exit_fee_margin_pct", { m = 100, s = 0 })
  if fee_margin.m < 0 then fee_margin = { m = 100, s = 0 } end
  return {
    min_mid_price = dec_param(p, "min_mid_price", { m = 20, s = 2 }),
    max_mid_price = dec_param(p, "max_mid_price", { m = 42, s = 2 }),
    bid_offset = dec_param(p, "bid_offset", { m = 2, s = 2 }),
    take_profit_spread = dec_param(p, "take_profit_spread", { m = 3, s = 2 }),
    min_time_left_sec = int_param(p, "min_time_left_sec", 120),
    min_bid_depth = dec_param(p, "min_bid_depth", { m = 300, s = 0 }),
    min_ask_depth = dec_param(p, "min_ask_depth", { m = 300, s = 0 }),
    max_abs_obi = dec_param(p, "max_abs_obi", { m = 4, s = 1 }),
    trend_window_sec = int_param(p, "trend_window_sec", 30),
    trend_max_move_pct = dec_param(p, "trend_max_move_pct", { m = 5, s = 0 }),
    spot_window_sec = int_param(p, "spot_window_sec", 30),
    spot_max_move_pct = dec_param(p, "spot_max_move_pct", { m = 15, s = 1 }),
    spot_missing = p.spot_missing == "pass" and "pass" or "block",
    min_available_usd = dec_param(p, "min_available_usd", { m = 100, s = 2 }),
    -- Minimum pair discount the merge-exit leg must keep: while one leg is
    -- held, the opposite bid survives only while
    --   entry_held + bid_other <= 1.00 - merge_min_profit
    -- (the completed pair redeems at exactly 1.00/pair, fee-free — the
    -- discount IS the pair's gross profit and this file's skew amplitude).
    -- Default 0.02: just under the single-side bank (take_profit_spread
    -- 0.03) because the merge leg does double duty — its fill ALSO retires
    -- the held leg's exposure at par, so the pair discount understates the
    -- leg-level economics and may sit a tick below the fresh-trade spread.
    merge_min_profit = merge_min_profit,
    -- Percent of the kernel's own round-trip taker fee added to the profit
    -- band (the minimum-profit guard on the only exit this file can
    -- suggest). 100 = the exact fee curve the kernel charges, zero padding —
    -- the honest default. Raise it to pad for schedule drift (a replay may
    -- run --fee-model with a different schedule than the live default).
    -- The curve itself: fee_usd_per_share = rate * (p*(1-p))^exponent — the
    -- kernel's ONE fee schedule (core exit_policy.rs FeeSchedule; shipped
    -- default legacy_quadratic 0.125*(p*(1-p))^2, Polymarket's published
    -- crypto curve 0.07*p*(1-p)) — read LIVE per evaluation via bk.fees(),
    -- never restated as constants here.
    exit_fee_margin_pct = fee_margin,
  }
end

-- ── per-token mid ring (the chip-trend gate's input) ────────────────────────
-- tokens[tok] = { samples = { {ts, p, bucket}, ... } (oldest first, one per
-- 250ms bucket) }, wiped on round switch.

local BUCKET_MS = 250

local tracker = { slot = nil, tokens = {} }

local function token_state(tok)
  local st = tracker.tokens[tok]
  if not st then
    st = { samples = {} }
    tracker.tokens[tok] = st
  end
  return st
end

local function tracker_reset_if_new_round(slot)
  if slot ~= tracker.slot then
    tracker.slot = slot
    tracker.tokens = {}
  end
end

local function trim_samples(st, now_ms, ring_sec)
  local cutoff = now_ms - ring_sec * 1000
  local kept, n = {}, 0
  for i = 1, #st.samples do
    if st.samples[i].ts >= cutoff then
      n = n + 1
      kept[n] = st.samples[i]
    end
  end
  st.samples = kept
end

local function push_sample(st, ts, dec)
  local bucket = ts // BUCKET_MS
  local n = #st.samples
  if n > 0 and st.samples[n].bucket == bucket then
    st.samples[n] = { ts = ts, p = dec, bucket = bucket }
  else
    st.samples[n + 1] = { ts = ts, p = dec, bucket = bucket }
  end
end

--- Condition 2 (venue side): the chip's own mid must NOT be in a one-sided
--- decline. One-sided = the net move across the window is a fall of more
--- than `trend_max_move_pct` percent. Fewer than two samples, or a window
--- that does not yet span `trend_window_sec`, is NO proof of calm → refuse
--- (fail-closed; the book stream itself feeds this ring, so in production
--- the ring warms within seconds of the tracker seeing the token).
local function chip_calm(cfg, st, now_ms)
  local s = st.samples
  local cutoff = now_ms - cfg.trend_window_sec * 1000
  local oldest, oldest_ts, newest
  for i = 1, #s do
    if s[i].ts >= cutoff then
      if not oldest then
        oldest = s[i].p
        oldest_ts = s[i].ts
      end
      newest = s[i].p
    end
  end
  if not oldest or not newest or oldest.m <= 0 then return false end
  if now_ms - oldest_ts < (cfg.trend_window_sec - 1) * 1000 then return false end
  -- fall/oldest * 100 > pct  ⟺  (oldest - newest) * 100 > oldest * pct
  local lhs = dec_mul(dec_sub(oldest, newest), DEC_100)
  local rhs = dec_mul(oldest, cfg.trend_max_move_pct)
  return dec_cmp(lhs, rhs) <= 0
end

-- ── spot closes (condition 2, underlying side) ──────────────────────────────
-- asset -> { {ts, dec_close}, ... } of CLOSED sec1 bars, oldest first.

local spot_bars = {}

function bk_on_kline(k)
  if k.interval ~= "sec1" or k.is_closed ~= true then return end
  local close = dec_parse(k.close)
  if not close or close.m <= 0 then return end
  local cfg = read_config()
  local arr = spot_bars[k.symbol]
  if not arr then arr = {}; spot_bars[k.symbol] = arr end
  arr[#arr + 1] = { ts = k.close_time_ms, p = close }
  local cutoff = k.close_time_ms - (cfg.spot_window_sec + 5) * 1000
  local kept, n = {}, 0
  for i = 1, #arr do
    if arr[i].ts >= cutoff then
      n = n + 1
      kept[n] = arr[i]
    end
  end
  spot_bars[k.symbol] = kept
end

--- True when the underlying does not contradict the side being bought: a
--- waterfall (move <= -spot_max_move_pct over the window) vetoes an UP buy,
--- a pump vetoes a DOWN buy. Data missing or window not covered → the
--- `spot_missing` policy (default "block": no confirmation, no buy).
local function spot_ok(cfg, asset, dir_up, now_ms)
  local arr = spot_bars[asset]
  local move, enough = 0, false
  if arr and #arr >= 2 then
    local cutoff = now_ms - cfg.spot_window_sec * 1000
    local oldest_ts, oldest, newest
    for i = 1, #arr do
      if arr[i].ts >= cutoff then
        if not oldest then
          oldest = arr[i].p
          oldest_ts = arr[i].ts
        end
        newest = arr[i].p
      end
    end
    -- window coverage within 1s (sec1 bars close at most ~1s behind now)
    if oldest and newest and oldest.m > 0 and (now_ms - oldest_ts) >= (cfg.spot_window_sec - 1) * 1000 then
      enough = true
      local diff = dec_sub(newest, oldest)
      move = (diff.m / (POW10[diff.s] * oldest.m)) * 100
    end
  end
  if not enough then return cfg.spot_missing == "pass" end
  local thr = tonumber(dec_fmt(cfg.spot_max_move_pct))
  if dir_up then
    return not (move <= -thr)
  end
  return not (move >= thr)
end

-- ── the thesis ──────────────────────────────────────────────────────────────
-- thesis = { condition_id, up_token, down_token, up = dec|nil, down =
-- dec|nil, slot } | nil — the ONE market this strategy has armed, with the
-- resting bid price per ARMED side (nil = that side did not qualify).
-- Fill-blind by contract; `bk.holdings()` — not the thesis — decides the
-- position states; the kernel retires what did not rest and closes what did.

local thesis = nil

-- ── the kernel's taker-fee curve, read live (bk.fees()) ─────────────────────

--- The active schedule as exact decimals, guarded into the safe mantissa
--- domain. `nil` = no schedule in force (or an unusable one) — a strategy
--- that prices fees must fail closed (skip the exit, never assume zero),
--- exactly the bk.fees() contract. Guards: rate mantissa ≤ 1e4 (real rates
--- are ≤ 1) and exponent ≤ 3 (the shipped curves are 1 and 2) keep the worst
--- exact product — base mantissa ≤ 2500 (a 2dp price ≤ 0.50 gives
--- p*(1-p) ≤ 0.25) cubed, times the rate, times the 4-digit margin ratio —
--- far inside i64; anything beyond the guards is refused rather than
--- wrapped.
local function fee_model()
  local f = bk.fees and bk.fees() or nil
  if f == nil then return nil end
  local rate = dec_parse(f.rate)
  if not rate or rate.m < 0 or rate.m > 10000 then return nil end
  local e = f.exponent
  if type(e) ~= "number" or e ~= math.floor(e) or e < 0 or e > 3 then return nil end
  return { rate = rate, exponent = math.floor(e) }
end

--- fee_usd per ONE share at price `p` (2dp decision grid):
---   rate * (p*(1-p))^exponent
--- the kernel's own charge curve (exit_policy.rs FeeSchedule), spelled in
--- exact decimals — the same numbers the kernel charges the fill with.
local function fee_per_share(model, p)
  if p.m <= 0 or p.m >= DEC_ONE.m then return { m = 0, s = 2 } end
  local base = dec_mul(p, dec_sub(DEC_ONE, p))
  local acc = { m = 1, s = 0 }
  for _ = 1, model.exponent do acc = dec_mul(acc, base) end
  return dec_mul(model.rate, acc)
end

--- The fee-cleared profit band: entry + spread + margin% of the round-trip
--- taker fee (entry fill + exit fill). `model == nil` → fail closed (never
--- fires). The taker exit fires only when mid clears the WHOLE band —
--- otherwise a banked spread is handed straight back to the fee (the smart-
--- money forensics this rework answers to: net losses were ~100% fees).
local function fee_band(cfg, model, entry, mid)
  if not model then return nil end
  -- pct → fraction, exactly (scale + 2).
  local ratio = { m = cfg.exit_fee_margin_pct.m, s = cfg.exit_fee_margin_pct.s + 2 }
  local round_trip = dec_add(fee_per_share(model, entry), fee_per_share(model, mid))
  return dec_add(dec_add(entry, cfg.take_profit_spread), dec_mul(ratio, round_trip))
end

-- ── holdings (the one position observation) ─────────────────────────────────

--- THIS slot's open positions from `bk.holdings()` (the host refreshes the
--- view before every evaluation; rows carry decimal STRINGS). Older-slot
--- rows are deliberately invisible here: their survival belongs to the
--- kernel's ladder and the settlement path, never to this file.
local function slot_holdings(slot)
  local out = {}
  local hs = bk.holdings and bk.holdings() or {}
  for _, h in ipairs(hs) do
    if h.round_slot == slot then
      local sh = dec_parse(h.shares)
      local en = dec_parse(h.entry_price)
      if sh and en and sh.m > 0 then
        out[h.token_id] = { shares = sh, entry = dec_round2(en), condition_id = h.condition_id }
      end
    end
  end
  return out
end

local function market_of(condition_id)
  for _, m in ipairs(bk.markets()) do
    if m.condition_id == condition_id then return m end
  end
  return nil
end

-- ── entry points (§6.5) ─────────────────────────────────────────────────────

--- Every book update feeds the chip's mid ring (fresh or not — the trend
--- gate must see the whole round, like spread_arb's tracker).
function bk_on_book(book)
  local cfg = read_config()
  local mid = book_dec(book.mid)
  if not mid or mid.m <= 0 then return end
  local st = token_state(book.symbol)
  push_sample(st, book.ts_ms, mid)
  trim_samples(st, book.ts_ms, cfg.trend_window_sec + 2)
end

function bk_on_round(round)
  tracker_reset_if_new_round(round.slot)
end

--- One side's arming verdict: every spec gate, evaluated per token. Returns
--- the resting bid price (2dp) or nil.
local function qualify_chip(cfg, m, tok, now_ms)
  local book = bk.book(tok)
  if not book or book.fresh ~= true then return nil end
  local mid = book_dec(book.mid)
  if not mid or mid.m <= 0 then return nil end
  if dec_cmp(mid, cfg.min_mid_price) < 0 then return nil end
  if dec_cmp(mid, cfg.max_mid_price) > 0 then return nil end
  local bid = book_dec(book.best_bid)
  local ask = book_dec(book.best_ask)
  if not bid or not ask or bid.m <= 0 or ask.m <= 0 then return nil end
  local bd = book_dec(book.bid_depth)
  if bd and dec_cmp(bd, cfg.min_bid_depth) < 0 then return nil end
  local ad = book_dec(book.ask_depth)
  if ad and dec_cmp(ad, cfg.min_ask_depth) < 0 then return nil end
  local o = book_dec(book.obi)
  if o then
    local abs = o.m < 0 and { m = -o.m, s = o.s } or o
    if dec_cmp(abs, cfg.max_abs_obi) > 0 then return nil end
  end
  local st = tracker.tokens[tok]
  if not st or not chip_calm(cfg, st, now_ms) then return nil end
  if not spot_ok(cfg, m.asset, tok == m.up_token, now_ms) then return nil end
  local entry = dec_round2(dec_sub(mid, cfg.bid_offset))
  if dec_cmp(entry, DEC_001) < 0 or dec_cmp(entry, ask) >= 0 then return nil end
  return entry
end

--- Retire every ARMED side's resting bids (the kernel cancels each once —
--- a break for an order that already filled is a benign no-op).
local function break_thesis(breaks)
  if not thesis then return end
  if thesis.up then
    breaks[#breaks + 1] = { token = thesis.up_token, broken_price = dec_fmt(thesis.up) }
  end
  if thesis.down then
    breaks[#breaks + 1] = { token = thesis.down_token, broken_price = dec_fmt(thesis.down) }
  end
end

--- The suggestion surface. Returns { entries, exits, breaks }.
function bk_evaluate()
  local cfg = read_config()
  local entries, exits, breaks = {}, {}, {}

  -- A thesis whose round is gone (or replaced) retires its armed bids.
  local round = bk.round()
  local slot = round and round.slot or nil
  if thesis and thesis.slot ~= slot then
    break_thesis(breaks)
    thesis = nil
  end

  if round == nil then
    return { entries = entries, exits = exits, breaks = breaks }
  end

  local now_ms = bk.now_ms()
  local held = slot_holdings(slot)

  -- BOTH legs of one condition held: the completed pair exits through the
  -- kernel's merge channel — fee-free at 1.00/pair, the maker-grade exit.
  -- Intents repeat every cycle until the burn lands (the kernel drops an
  -- intent with no complete pair behind it; pair_discount_arb's pattern).
  -- No new thesis is armed while a merge is pending: the position is not
  -- closed yet, and the kernel's funds/risk path is the real gate.
  local merge_tokens = nil
  local merge_condition = nil
  for tok, h in pairs(held) do
    if not merge_condition then
      local other = nil
      for tok2, h2 in pairs(held) do
        if tok2 ~= tok and h2.condition_id == h.condition_id then other = tok2 end
      end
      if other then
        merge_condition = h.condition_id
        merge_tokens = { tok, other }
      end
    end
  end
  if merge_tokens then
    for _, tok in ipairs(merge_tokens) do
      exits[#exits + 1] = { token = tok, reason = "merge" }
    end
    if thesis and thesis.condition_id == merge_condition then
      break_thesis(breaks)
      thesis = nil
    end
    return { entries = entries, exits = exits, breaks = breaks }
  end

  -- ONE leg held: single-side maintenance (the inventory skew). Keyed off
  -- the HOLDING, not the thesis — a restart between fill and close still
  -- gets the same upkeep.
  local held_tok, held_row = nil, nil
  for tok, h in pairs(held) do
    held_tok, held_row = tok, h
    break
  end
  if held_tok then
    local m = market_of(held_row.condition_id)
    if m then
      -- Skew, held side: the resting bid is BROKEN, never re-priced — one
      -- internal key per (strategy, asset, direction, slot) makes a resting
      -- bid immovable, and no second entry fires while inventory is open.
      breaks[#breaks + 1] = { token = held_tok, broken_price = dec_fmt(held_row.entry) }
      -- Skew, opposite side: the surviving bid IS the exit leg (its fill
      -- completes the fee-free merge), so it rests only while the pair
      -- keeps its minimum discount. Failing the cap retires it.
      if thesis and thesis.condition_id == m.condition_id then
        local other = held_tok == m.up_token and m.down_token or m.up_token
        local opp_price = other == thesis.up_token and thesis.up or thesis.down
        if opp_price then
          local cap = dec_sub(DEC_ONE, cfg.merge_min_profit)
          if dec_cmp(dec_add(held_row.entry, opp_price), cap) > 0 then
            breaks[#breaks + 1] = { token = other, broken_price = dec_fmt(opp_price) }
            if other == thesis.up_token then thesis.up = nil else thesis.down = nil end
          end
        end
      end
      -- The taker close, above the fee band only. No schedule in force →
      -- fail closed: the position rides the kernel's ladder instead.
      local book = bk.book(held_tok)
      local mid = book and book.fresh == true and book_dec(book.mid) or nil
      if mid and mid.m > 0 then
        local band = fee_band(cfg, fee_model(), held_row.entry, mid)
        if band and dec_cmp(mid, band) >= 0 then
          exits[#exits + 1] = {
            token = held_tok,
            reason = string.format(
              "target reached: mid %s >= entry %s + %s + fee band %s",
              dec_fmt(mid),
              dec_fmt(held_row.entry),
              dec_fmt(cfg.take_profit_spread),
              dec_fmt(dec_sub(band, dec_add(held_row.entry, cfg.take_profit_spread)))
            ),
          }
          break_thesis(breaks)
          thesis = nil
        end
      end
    end
    -- A position is open: arm nothing (the kernel's funds/risk path is the
    -- real gate, disclosed in the header).
    return { entries = entries, exits = exits, breaks = breaks }
  end

  -- FLAT: the fill-blind thesis upkeep. Per ARMED side: mid cleared
  -- bid + spread → the thesis ran away from its bid; there is NO position
  -- to close (holdings are flat), so the old positionless exit is not
  -- faked — the bids retire and the SAME evaluate may re-arm. A one-sided
  -- chip retires the bids the same way.
  if thesis then
    local ran_away = false
    for _, side in ipairs({ { thesis.up_token, thesis.up }, { thesis.down_token, thesis.down } }) do
      local tok, price = side[1], side[2]
      if price then
        local book = bk.book(tok)
        local mid = book and book.fresh == true and book_dec(book.mid) or nil
        if book and mid and mid.m > 0 then
          if dec_cmp(mid, dec_add(price, cfg.take_profit_spread)) >= 0 then
            ran_away = true
          else
            local st = tracker.tokens[tok]
            if st and not chip_calm(cfg, st, now_ms) then ran_away = true end
          end
        end
      end
    end
    if ran_away then
      break_thesis(breaks)
      thesis = nil
    end
  end

  -- Arming needs the time window (a fee-band exit above does not — a target
  -- hit in the last two minutes still banks the spread, and the merge
  -- collection never stops: it runs to the last tick by design).
  if round.time_left_sec < cfg.min_time_left_sec then
    return { entries = entries, exits = exits, breaks = breaks }
  end

  -- One thesis in flight; the kernel's funds/risk path is the real
  -- position/cooldown gate (disclosed in the header).
  if thesis then
    return { entries = entries, exits = exits, breaks = breaks }
  end

  local avail_ok = true
  local acct = bk.account()
  if acct and acct.available ~= nil then
    local a = dec_parse(acct.available)
    if a and dec_cmp(a, cfg.min_available_usd) < 0 then avail_ok = false end
  end

  if avail_ok then
    -- Arm EACH qualifying side of the first qualifying market: the dual
    -- quote this file can actually rest (the engine takes Buy-side limits
    -- only). Both mids sit in the cheap band only when the book genuinely
    -- prices the pair under par — then the merge exit is live from birth.
    for _, m in ipairs(bk.markets()) do
      local up_entry = qualify_chip(cfg, m, m.up_token, now_ms)
      local down_entry = qualify_chip(cfg, m, m.down_token, now_ms)
      if up_entry or down_entry then
        if up_entry then
          entries[#entries + 1] = {
            token = m.up_token,
            price = dec_fmt(up_entry),
            reason = string.format(
              "UP chip mid %s in band, resting maker bid %s (%ds left)",
              dec_fmt(book_dec(bk.book(m.up_token).mid)),
              dec_fmt(up_entry),
              round.time_left_sec
            ),
          }
        end
        if down_entry then
          entries[#entries + 1] = {
            token = m.down_token,
            price = dec_fmt(down_entry),
            reason = string.format(
              "DOWN chip mid %s in band, resting maker bid %s (%ds left)",
              dec_fmt(book_dec(bk.book(m.down_token).mid)),
              dec_fmt(down_entry),
              round.time_left_sec
            ),
          }
        end
        thesis = {
          condition_id = m.condition_id,
          up_token = m.up_token,
          down_token = m.down_token,
          up = up_entry,
          down = down_entry,
          slot = slot,
        }
        break
      end
    end
  end

  return { entries = entries, exits = exits, breaks = breaks }
end
