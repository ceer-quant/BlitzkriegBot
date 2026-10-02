-- spread_arb — the trend-confirmed dip buyer, as a Lua strategy package.
--
-- This is the Lua port of the Rust reference implementation (the cdylib
-- fixture that used to live in user_layer/strategies/spread_arb, algorithm in
-- strategy_logic::{signal::TrendTracker, signal::evaluate_spread_arb}). The
-- repo no longer ships a Rust strategy; THIS package is spread_arb.
--
-- The port is EXACT, not approximate, and the discipline that makes it exact
-- is worth stating:
--
--   * Prices cross as decimal STRINGS (the §6.5 wire rule). Every price
--     computation and comparison here runs on SCALED INTEGERS — the same
--     semantics as the Rust side's `Decimal` — never on floats. In particular
--     `round2` is rust_decimal's `round()` EXACTLY: MidpointNearestEven
--     (banker's rounding), because a resting bid at 0.425 must round to 0.42
--     on BOTH stacks. A float implementation of `mid * 0.88` would flip that
--     boundary on real market data; integers cannot.
--   * The only non-terminating divisions (the above-threshold ratio, the
--     short-window move) run as SINGLE float divisions compared against
--     1–2 decimal thresholds. With market-derived rationals (bounded
--     denominators) a double and rust_decimal's 28-digit quotient can only
--     disagree past the 15th significant digit, which is orders of magnitude
--     below any threshold distance.
--   * The evaluator's quirks are preserved deliberately, including the one
--     where `bk_evaluate` returns early — WITHOUT draining breaks — while no
--     token is trend-confirmed. That is the recorded Rust behaviour; a
--     "fix" here would be an economics change.
--
-- Knobs come from `bk.params()` (the manifest tunables, defaults equal to
-- `SpreadArbConfig::default()`); the kernel's `--spread-arb-*` CLI config
-- does NOT reach Lua strategies (the dylib path's on_params channel) — the
-- manifest is the single knob source for this stack.
--
-- Everything else is the §6.5 contract: read-only `bk.*`, intents returned to
-- the kernel (entries are suggestions; the kernel adjudicates, sizes, gates,
-- signs). No stop-loss logic anywhere — the exit discipline belongs to the
-- kernel (the「止损不归你管」seal).

-- ── exact decimal helpers ───────────────────────────────────────────────────
-- A decimal is `{ m = mantissa, s = scale }` meaning m / 10^s. Mantissas stay
-- far inside 64-bit integers (prices ≤ 1.0 with ≤ 10 decimals).

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
  return d.m * POW10[s - d.s]
end

local function dec_cmp(a, b)
  local s = math.max(a.s, b.s)
  local am, bm = dec_rescale(a, s), dec_rescale(b, s)
  if am < bm then return -1 elseif am > bm then return 1 else return 0 end
end

local function dec_mul(a, b)
  return { m = a.m * b.m, s = a.s + b.s }
end

local function dec_sub(a, b)
  local s = math.max(a.s, b.s)
  return { m = dec_rescale(a, s) - dec_rescale(b, s), s = s }
end

--- rust_decimal `round_dp(2)` — MidpointNearestEven, the exact `round2`.
local function dec_round2(d)
  if d.s <= 2 then return { m = d.m * POW10[2 - d.s], s = 2 } end
  local div = POW10[d.s - 2]
  local q = d.m // div
  local r = d.m - q * div
  local twice = r * 2
  if twice > div or (twice == div and q % 2 == 1) then q = q + 1 end
  return { m = q, s = 2 }
end

local DEC_ONE = { m = 1, s = 0 }
local DEC_005 = { m = 5, s = 2 }
local DEC_090 = { m = 90, s = 2 }

-- ── configuration (bk.params() — manifest tunables) ─────────────────────────
-- Defaults equal `SpreadArbConfig::default()`; the manifest carries the same
-- values. Read fresh every evaluate so a future hot-param cell takes effect
-- without touching this file.

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
    trend_min_price = dec_param(p, "trend_min_price", { m = 55, s = 2 }),
    trend_confirm_sec = int_param(p, "trend_confirm_sec", 60),
    trend_broken_price = dec_param(p, "trend_broken_price", { m = 35, s = 2 }),
    trend_entry_price = dec_param(p, "trend_entry_price", { m = 0, s = 0 }),
    trend_entry_factor = dec_param(p, "trend_entry_factor", { m = 88, s = 2 }),
    trend_max_entry_price = dec_param(p, "trend_max_entry_price", { m = 45, s = 2 }),
    entry_min_obi = dec_param(p, "entry_min_obi", { m = 0, s = 0 }),
    entry_max_spread_pct = dec_param(p, "entry_max_spread_pct", { m = 0, s = 0 }),
    entry_dip_max_pct = dec_param(p, "entry_dip_max_pct", { m = 0, s = 0 }),
    entry_bounce_min_pct = dec_param(p, "entry_bounce_min_pct", { m = 0, s = 0 }),
    entry_bounce_window_sec = int_param(p, "entry_bounce_window_sec", 5),
    -- #176-style long-memory slide gate (mean_reversion's proven lever): a
    -- token whose mid sits `entry_trend_drop_pct`% or more below the high of
    -- the last `entry_trend_window_sec` is in a one-sided slide — every
    -- further dip is another knife. 0 = off (the pre-gate behaviour).
    entry_trend_window_sec = int_param(p, "entry_trend_window_sec", 0),
    entry_trend_drop_pct = dec_param(p, "entry_trend_drop_pct", { m = 30, s = 0 }),
    -- Fixed tracker internals the spread_arb knob set does not expose
    -- (TrendConfig::default() fields the hot bag never carried).
    trend_ratio_threshold = 0.8,
    trend_window_floor_ms = 10000,
  }
end

-- ── the trend tracker (TrendTracker, exactly) ───────────────────────────────
-- states[token] = { phase = "idle"|"building"|"confirmed"|"broken",
--                   samples = { {ts, dec}, ... } } — oldest first, as pushed.

local tracker = { states = {}, round_slot = nil, broken = {} }

local function tracker_on_price(cfg, token, price, now_ms)
  if token == "" or price.m <= 0 then return end
  local confirm_ms = math.max(cfg.trend_confirm_sec * 1000, cfg.trend_window_floor_ms)
  -- The slide gate reads a LONGER memory than confirmation does, so the ring
  -- retains the union of both windows; the confirmation ratio is still computed
  -- over the confirm window only (identical to the pre-gate behaviour, which
  -- pruned to exactly that window).
  local trend_ms = 0
  if cfg.entry_trend_window_sec > 0 then
    trend_ms = math.max(cfg.entry_trend_window_sec * 1000, confirm_ms)
  end
  local retain_ms = math.max(confirm_ms, trend_ms)
  local st = tracker.states[token]
  if not st then
    st = { phase = "idle", samples = {} }
    tracker.states[token] = st
  end
  st.samples[#st.samples + 1] = { now_ms, price }
  local cutoff = now_ms - retain_ms
  local kept, n = {}, 0
  for i = 1, #st.samples do
    if st.samples[i][1] >= cutoff then
      n = n + 1
      kept[n] = st.samples[i]
    end
  end
  st.samples = kept
  -- signal.rs PriceBuffer::push's exact memory cap: at most the newest 2000
  -- samples, the oldest dropped first. Never fires below the cap (the confirm
  -- window alone sits under it); without it a long slide window would retain
  -- an unbounded ring — a memory-model divergence from the Rust gate this
  -- ports, and an O(n) scan on every book update.
  local excess = #st.samples - 2000
  if excess > 0 then
    local trimmed = {}
    for i = excess + 1, #st.samples do
      trimmed[#trimmed + 1] = st.samples[i]
    end
    st.samples = trimmed
  end

  local cutoff_confirm = now_ms - confirm_ms
  local total = 0
  local above = 0
  local oldest_ts = now_ms
  for i = 1, #st.samples do
    local t, p = st.samples[i][1], st.samples[i][2]
    if t >= cutoff_confirm then
      total = total + 1
      if total == 1 then oldest_ts = t end
      if dec_cmp(p, cfg.trend_min_price) >= 0 then above = above + 1 end
    end
  end
  local above_ratio = 0
  if total > 0 then above_ratio = above / total end
  local spanned_ms = now_ms - oldest_ts

  -- Regime change: a confirmed trend broke its floor.
  if st.phase == "confirmed" and dec_cmp(price, cfg.trend_broken_price) < 0 then
    st.phase = "broken"
    tracker.broken[#tracker.broken + 1] = { token, price }
    return
  end

  -- Confirmation needs a full-ish window and a high above-threshold ratio.
  -- (The Rust computes `(window_ms as f64 * 0.9) as i64` — the same float
  -- truncate, mirrored deliberately.)
  if st.phase ~= "confirmed" and spanned_ms >= math.floor(confirm_ms * 0.9)
    and above_ratio >= cfg.trend_ratio_threshold then
    st.phase = "confirmed"
    return
  end

  if st.phase ~= "confirmed" and st.phase ~= "broken" then
    if above_ratio > 0.5 then
      st.phase = "building"
    else
      st.phase = "idle"
    end
  end
end

--- The #176 measure: the current mid's drop (%) off the highest mid inside
--- the LONG trend window. 0 when the gate is off or history is short — a
--- disabled gate and a fresh token both read "no slide" and never block
--- (mean_reversion.rs `trend_drop_pct`, mirrored exactly).
local function tracker_trend_drop_pct(cfg, token, now_ms)
  if cfg.entry_trend_window_sec <= 0 then return 0 end
  local st = tracker.states[token]
  if not st or #st.samples < 2 then return 0 end
  local cutoff = now_ms - cfg.entry_trend_window_sec * 1000
  local hi = { m = 0, s = 0 }
  for i = 1, #st.samples do
    local t, p = st.samples[i][1], st.samples[i][2]
    if t >= cutoff and dec_cmp(p, hi) > 0 then hi = p end
  end
  if hi.m <= 0 then return 0 end
  local cur = st.samples[#st.samples][2]
  local diff = dec_sub(cur, hi)
  -- ((cur - hi) / hi) * 100 — mean_reversion.rs trend_drop_pct's exact
  -- measure. Each factor is O(price), so the double error (~1e-16 relative)
  -- sits far below any 1-2 decimal threshold (header discipline). The naive
  -- diff.m/(POW10[diff.s]*hi.m) form silently drops hi's OWN scale from the
  -- divisor — a 10^hi.s understatement (100x at price scale 2) that read a
  -- -7.27% slide as -0.0727% and never fired the gate.
  return (diff.m / POW10[diff.s]) / (hi.m / POW10[hi.s]) * 100
end

local function tracker_reset_if_new_round(slot)
  if slot ~= tracker.round_slot then
    tracker.round_slot = slot
    tracker.states = {}
  end
end

local function tracker_recent_high(token)
  local st = tracker.states[token]
  if not st then return { m = 0, s = 0 } end
  local hi = { m = 0, s = 0 }
  for i = 1, #st.samples do
    if dec_cmp(st.samples[i][2], hi) > 0 then hi = st.samples[i][2] end
  end
  return hi
end

--- Newest-vs-oldest move over the window, in percent (float — see header).
local function tracker_move_pct(token, window_sec, now_ms)
  local st = tracker.states[token]
  if not st then return 0 end
  local cutoff = now_ms - math.max(window_sec, 1) * 1000
  local oldest, newest
  for i = 1, #st.samples do
    if st.samples[i][1] >= cutoff then
      if not oldest then oldest = st.samples[i][2] end
      newest = st.samples[i][2]
    end
  end
  if not oldest or not newest then return 0 end
  if oldest.m <= 0 then return 0 end
  local diff = dec_sub(newest, oldest)
  -- ((newest - oldest) / oldest) * 100 — signal.rs PriceBuffer::move_pct's
  -- exact measure; same scale-carrying float form as the slide gate (the
  -- naive form drops oldest's own scale from the divisor). Latent until now:
  -- the bounce gate that calls this is OFF by default and off in every
  -- frozen window, so the byte-identity gate never crossed it.
  return (diff.m / POW10[diff.s]) / (oldest.m / POW10[oldest.s]) * 100
end

-- ── the evaluator (evaluate_spread_arb + tracker gates, exactly) ────────────

--- Evaluate ONE token's fresh book. Returns the entry row or nil. The guard
--- chain and its ORDER mirror the Rust line for line.
local function evaluate_token(cfg, confirmed, token, now_ms)
  if not confirmed[token] then return nil end
  local book = bk.book(token)
  if not book or book.fresh ~= true then return nil end
  -- snapshot_from's guards: bid/ask/mid strings must all parse; an absent or
  -- non-positive side is an empty side (mid collapses to zero → skip).
  local bid = dec_parse(book.best_bid)
  local ask = dec_parse(book.best_ask)
  local mid = dec_parse(book.mid)
  if not bid or not ask or not mid then return nil end
  if bid.m <= 0 or ask.m <= 0 or mid.m <= 0 then return nil end

  if cfg.trend_broken_price.m > 0 and dec_cmp(mid, cfg.trend_broken_price) < 0 then
    return nil
  end
  -- High-frequency entry filters (both OFF by default), read off the kernel's
  -- own derived fields — the same values the cdylib recomputed from the same
  -- inputs.
  if cfg.entry_min_obi.m > 0 then
    local obi = dec_parse(book.obi)
    if obi and dec_cmp(obi, cfg.entry_min_obi) < 0 then return nil end
  end
  if cfg.entry_max_spread_pct.m > 0 then
    local sp = dec_parse(book.spread_pct)
    if sp and dec_cmp(sp, cfg.entry_max_spread_pct) > 0 then return nil end
  end

  -- #176 trend gate (mean_reversion's proven lever, ported): a token several
  -- legs into a one-sided slide is being repriced, not oversold — in a double
  -- market the cheap side of a trend keeps cheapening, and every further dip
  -- is another knife. Blocks the entry only; nothing reprices what passes.
  -- The arming guard mirrors the Rust knob validation (5..90): a zero
  -- threshold here would read `drop <= -0` and refuse EVERY entry, so an
  -- unarmed threshold means the gate is off, not always-on.
  local drop = tracker_trend_drop_pct(cfg, token, now_ms)
  if cfg.entry_trend_drop_pct.m > 0 and drop <= -tonumber(dec_fmt(cfg.entry_trend_drop_pct)) then
    return nil
  end

  local raw
  if cfg.trend_entry_price.m > 0 then
    raw = cfg.trend_entry_price
  else
    raw = dec_mul(mid, cfg.trend_entry_factor)
  end
  local entry = raw
  if dec_cmp(entry, DEC_005) < 0 then entry = DEC_005 end
  if dec_cmp(entry, DEC_090) > 0 then entry = DEC_090 end
  entry = dec_round2(entry)
  if bid.m > 0 and dec_cmp(entry, bid) > 0 then
    entry = dec_round2(bid)
  end
  if dec_cmp(entry, mid) >= 0 then return nil end
  if cfg.trend_max_entry_price.m > 0 and dec_cmp(entry, cfg.trend_max_entry_price) > 0 then
    return nil
  end

  return entry, mid
end

--- The tracker-side gates (spread_arb_tracker_gates): turn filter + dip-depth
--- guard. Both only REFUSE; nothing reprices.
local function tracker_gates(cfg, token, mid, now_ms)
  if cfg.entry_bounce_min_pct.m > 0 then
    local mv = tracker_move_pct(token, cfg.entry_bounce_window_sec, now_ms)
    if mv < tonumber(dec_fmt(cfg.entry_bounce_min_pct)) then return false end
  end
  if cfg.entry_dip_max_pct.m > 0 then
    local high = tracker_recent_high(token)
    if high.m > 0 then
      -- floor = high * (1 - dip/100) — /100 is an exact scale shift.
      local one_minus = dec_sub(DEC_ONE, { m = cfg.entry_dip_max_pct.m, s = cfg.entry_dip_max_pct.s + 2 })
      local floor = dec_mul(high, one_minus)
      if dec_cmp(mid, floor) < 0 then return false end
    end
  end
  return true
end

-- ── entry points (§6.5) ─────────────────────────────────────────────────────

--- Every book update feeds the tracker from the kernel's mid — fresh or not,
--- exactly like the Rust `on_book`.
function bk_on_book(book)
  local cfg = read_config()
  local mid = dec_parse(book.mid)
  if mid and mid.m > 0 then
    tracker_on_price(cfg, book.symbol, mid, book.ts_ms)
  end
end

--- Round switch: the tracker's per-round reset.
function bk_on_round(round)
  tracker_reset_if_new_round(round.slot)
end

--- The suggestion surface. Returns { entries, exits, breaks }; the kernel
--- adjudicates everything (§6.4).
function bk_evaluate()
  local cfg = read_config()
  local confirmed = {}
  for token, st in pairs(tracker.states) do
    if st.phase == "confirmed" then confirmed[token] = true end
  end

  local entries, breaks = {}, {}
  -- The Rust returns EARLY — without draining breaks — when nothing is
  -- confirmed. Quirk preserved deliberately (see file header).
  if next(confirmed) == nil then
    return { entries = entries, exits = {}, breaks = breaks }
  end

  local now_ms = bk.now_ms()
  for _, m in ipairs(bk.markets()) do
    for _, token in ipairs({ m.up_token, m.down_token }) do
      local entry, mid = evaluate_token(cfg, confirmed, token, now_ms)
      if entry then
        if tracker_gates(cfg, token, mid, now_ms) then
          -- The reason mirrors the Rust format; the percentage is
          -- informational (it never feeds back into any decision).
          local pct = math.floor((entry.m / POW10[2] / (mid.m / POW10[mid.s])) * 100 + 0.5)
          entries[#entries + 1] = {
            token = token,
            price = dec_fmt(entry),
            reason = string.format(
              "%s trend confirmed (held >%s for >=%ds), resting bid %s (%d%% of mid %s)",
              token == m.up_token and "UP" or "DOWN",
              dec_fmt(cfg.trend_min_price),
              cfg.trend_confirm_sec,
              dec_fmt(entry),
              pct,
              dec_fmt(mid)
            ),
          }
        end
      end
    end
  end
  -- Trend breaks cancel the token's resting entry bids; drained here so each
  -- break is reported exactly once.
  for i = 1, #tracker.broken do
    breaks[#breaks + 1] = {
      token = tracker.broken[i][1],
      broken_price = dec_fmt(tracker.broken[i][2]),
    }
  end
  tracker.broken = {}
  return { entries = entries, exits = {}, breaks = breaks }
end
