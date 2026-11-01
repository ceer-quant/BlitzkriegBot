-- smart_money_reverse — the panic-dip buyer with a stabilization gate (§6.5).
--
-- One sentence: when ONE leg of a binary round CRASHES (>= crash_drop_pct
-- inside crash_window_sec), wait out the falling knife — no new low for
-- settle_quiet_sec AND a bid side that visibly thickened (bid_depth >=
-- max(stabilize_depth_factor x the crash period's thinnest observed
-- bid_depth, min_bid_depth)) — then rest a post-only maker bid at
-- best_bid + entry_offset (default 0.00, i.e. AT the touch, structurally
-- never crossing the ask) on the CRASHED leg only. Never the rebound leg:
-- the 10-29 corpus verdict measured up-leg chasing at 9% WR. This is not a
-- direction predictor; it is a smart-money-liquidation buyer with an
-- explicit anti-knife door.
--
-- The seal (the「止损不归你管」boundary, gated by strategy-no-stop-loss-check):
-- this file computes no exit price, monitors no exit price, and expresses no
-- exit price. Entries cross as { token, price, reason } suggestions with
-- shares omitted — the kernel adjudicates, sizes, funds-gates and posts them
-- post_only. The ONE non-entry row of scenario A is the thesis verdict: once
-- HOLDING, if the mid falls more than thesis_break_pct under the
-- stabilization-time mid (the rebound_anchor), the strategy emits
-- { token, reason = "thesis_reversed: ..." } into `exits` — a THESIS row,
-- not a price: the kernel prices and executes it (the exits wire gives no
-- price field). The manifest declares "trailing_exit": true, so the kernel's
-- own trailing machinery is ON for this strategy and owns survival whenever
-- this strategy has no verdict. There are no reserved keys, no price math on
-- the way out, no `stop` anything.
--
-- Scenario B (fade FOMO, default OFF via fomo_enabled="off"): when a leg
-- RAMPS (mid >= fomo_min_price and >= fomo_ramp_pct above the fomo_ramp_sec
-- window low), rest a post-only bid on the OPPOSITE leg at fomo_fade_price
-- (default 0.10). Evidence base, stated because the default is a verdict:
-- the 6meteor whale trace chased 0.24 → 0.98 and went 9/9 — chasing ramps
-- wins only for the whale's size; fading the ramp (buying the discarded leg
-- at lottery prices) is the inverse bet and is kept OFF until corpus
-- evidence flips it. When the ramp dies (mid back under fomo_min_price) the
-- resting fade bid is stale and is cancelled via `breaks`.
--
-- Breaks (both scenarios): a resting bid is a thesis, and theses die — a new
-- low under the crash low after stabilization (A) or a dead ramp (B) queues
-- a `break` row { token, broken_price }; the kernel cancels the stale bid.
-- A break also re-arms the crash state from the new low (the panic was not
-- done); a round that already emitted a thesis-reversed exit never re-enters.
--
-- Data ceilings, stated because they change what this strategy can know:
--   * There is NO on-chain whale feed in the bk.* surface. "Smart money" is
--     observed only through its market footprint — the crash/ramp itself is
--     the proxy trigger. The strategy cannot distinguish a whale dumping
--     from a market-maker delisting; it prices only the footprint.
--   * The underlying-asset spot gate reads CLOSED sec1 bars keyed by ASSET
--     ("BTC"/"ETH"/…), fed by the E29 engine aggregator from the Binance
--     spot stream. A live waterfall in the underlying vetoes buying the up
--     leg (and a pump vetoes the down leg) — never catch a needle that the
--     asset itself is still throwing. Frozen replay corpora are book+round
--     only: there the gate sees NO data and `spot_missing` (default
--     "block") decides — a strategy that cannot confirm the asset is calm
--     does not buy the knife. "pass" exists for book-only diagnostics.
--   * `bk.account()` is deliberately not consulted: the kernel's funds path
--     is the real gate. `bk.holdings()` IS host-pushed (lua_loader feeds
--     ctx.held_positions per evaluate) and gates both the exit watch and
--     the no-pyramid rule; an empty view simply means no exit watch.
--
-- Samples are bucketed at 250ms per token (one sample per bucket, ring =
-- max(crash_window_sec, fomo_ramp_sec) + settle_quiet_sec + 2): bounded
-- memory and a bounded per-callback instruction count for the §6.3 sandbox
-- (the 1e6 budget sees O(ring) scans, ~10^4 instructions), with granularity
-- far below every window the gates use.

-- ── exact decimal helpers (common/decimal.lua, byte-for-byte) ───────────────

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
local DEC_001 = { m = 1, s = 2 }

-- ── configuration (bk.params() — manifest tunables) ─────────────────────────

local function read_config()
  local p = bk.params()
  return {
    -- scenario A: crash arm + stabilization + entry. The master switch
    -- ships OFF: the 57-day corpus sweep (bk-session/smr-round/
    -- whale-trigger-spec.md) measured this exact shape at ROI −46% to −65%
    -- on filled resting bids — the stabilization gate selects precisely the
    -- rounds that stay down.
    crash_enabled = p.crash_enabled == "on",
    crash_window_sec = int_param(p, "crash_window_sec", 60),
    crash_drop_pct = dec_param(p, "crash_drop_pct", { m = 20, s = 0 }),
    settle_quiet_sec = int_param(p, "settle_quiet_sec", 8),
    stabilize_depth_factor = dec_param(p, "stabilize_depth_factor", { m = 15, s = 1 }),
    min_bid_depth = dec_param(p, "min_bid_depth", { m = 500, s = 0 }),
    entry_offset = dec_param(p, "entry_offset", { m = 0, s = 2 }),
    thesis_break_pct = dec_param(p, "thesis_break_pct", { m = 3, s = 0 }),
    min_time_left_sec = int_param(p, "min_time_left_sec", 180),
    -- underlying-asset veto (mad_dog's posture: fail closed)
    spot_max_move_pct = dec_param(p, "spot_max_move_pct", { m = 5, s = 1 }),
    spot_window_sec = int_param(p, "spot_window_sec", 10),
    spot_missing = p.spot_missing == "pass" and "pass" or "block",
    -- scenario B: fade FOMO (default OFF — the 6meteor verdict)
    fomo_enabled = p.fomo_enabled == "on",
    fomo_min_price = dec_param(p, "fomo_min_price", { m = 85, s = 2 }),
    fomo_ramp_pct = dec_param(p, "fomo_ramp_pct", { m = 15, s = 0 }),
    fomo_ramp_sec = int_param(p, "fomo_ramp_sec", 30),
    fomo_fade_price = dec_param(p, "fomo_fade_price", { m = 10, s = 2 }),
  }
end

-- ── per-token state (250ms bucket ring, mad_dog's memory discipline) ────────
-- tokens[tok] = {
--   samples        = { {ts, p=mid, d=bid_depth|nil, bucket}, ... } oldest first
--   crash          = { low, low_ts, min_depth } | nil      (armed crash)
--   confirmed      = bool   (stabilization gate passed; entry may flow)
--   anchor         = dec|nil (rebound_anchor: mid at the confirm edge;
--                    survives an unconfirm so a held leg's exit watch reads it)
--   entry_price    = dec|nil (last emitted resting-bid price — break payload)
--   exited         = bool   (thesis-reversed exit emitted this round)
--   fomo           = { fade_token, fade_price } | nil       (scenario B, this leg ramped)
--   pending_breaks = { {price=dec, token=string}, ... } (drained as breaks
--                    rows at evaluate; token = whose resting bid dies)
-- }

local BUCKET_MS = 250
local MAX_PENDING_BREAKS = 8

local tracker = { slot = nil, tokens = {} }

local function token_state(tok)
  local st = tracker.tokens[tok]
  if not st then
    st = {
      samples = {}, crash = nil, confirmed = false, anchor = nil,
      entry_price = nil, exited = false, fomo = nil, pending_breaks = {},
    }
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

local function ring_sec(cfg)
  local w = cfg.crash_window_sec
  if cfg.fomo_ramp_sec > w then w = cfg.fomo_ramp_sec end
  return w + cfg.settle_quiet_sec + 2
end

local function trim_samples(st, now_ms, keep_sec)
  local cutoff = now_ms - keep_sec * 1000
  local kept, n = {}, 0
  for i = 1, #st.samples do
    if st.samples[i].ts >= cutoff then
      n = n + 1
      kept[n] = st.samples[i]
    end
  end
  st.samples = kept
end

local function push_sample(st, ts, mid, depth)
  local bucket = ts // BUCKET_MS
  local n = #st.samples
  if n > 0 and st.samples[n].bucket == bucket then
    st.samples[n] = { ts = ts, p = mid, d = depth, bucket = bucket }
  else
    st.samples[n + 1] = { ts = ts, p = mid, d = depth, bucket = bucket }
  end
end

--- Queue one stale-bid cancellation. `target` names the token whose resting
--- bid must die — NOT necessarily the token whose state queued it (a fade
--- bid lives on the opposite leg of the ramp that died).
local function queue_break(st, price, target)
  if #st.pending_breaks < MAX_PENDING_BREAKS then
    st.pending_breaks[#st.pending_breaks + 1] = { price = price, token = target }
  end
end

-- ── scenario A: crash arming (on_book) ──────────────────────────────────────

--- IDLE: the drop from the crash window's HIGH to the fresh mid must reach
--- crash_drop_pct → arm (the window high is the pre-panic reference the
--- smart money sold into). Armed: every new low ratchets `low` and restarts
--- the quiet clock; if the state was already STABILIZED, the thesis is dead
--- — the resting bid is queued for cancellation and the crash re-arms from
--- the new low (it may re-stabilize; a round printing fresh lows was not
--- done panicking). `anchor` deliberately SURVIVES the unconfirm: the
--- thesis_reversed watch reads it while HOLDING, and fill-then-fresh-low is
--- exactly the sequence that verdict exists for. The crash period's thinnest
--- observed bid_depth ratchets into min_depth — it is the depth baseline
--- the stabilization gate must beat.
local function crash_check(cfg, st, now_ms, mid, depth, tok)
  if not cfg.crash_enabled then return end
  if st.crash then
    if dec_cmp(mid, st.crash.low) < 0 then
      if st.confirmed then
        if st.entry_price then queue_break(st, st.entry_price, tok) end
        st.confirmed = false
        st.entry_price = nil
      end
      st.crash.low = mid
      st.crash.low_ts = now_ms
    end
    if depth and (st.crash.min_depth == nil or dec_cmp(depth, st.crash.min_depth) < 0) then
      st.crash.min_depth = depth
    end
    return
  end
  local cutoff = now_ms - cfg.crash_window_sec * 1000
  local high, high_ts
  for i = 1, #st.samples do
    local s = st.samples[i]
    if s.ts >= cutoff and (not high or dec_cmp(s.p, high) > 0) then
      high = s.p
      high_ts = s.ts
    end
  end
  if not high or high.m <= 0 then return end
  -- drop/high * 100 >= pct  ⟺  drop * 100 >= high * pct  (exact integers)
  if dec_cmp(dec_mul(dec_sub(high, mid), DEC_100), dec_mul(high, cfg.crash_drop_pct)) < 0 then
    return
  end
  local min_depth = depth
  for i = 1, #st.samples do
    local s = st.samples[i]
    if s.ts >= high_ts and s.d and (min_depth == nil or dec_cmp(s.d, min_depth) < 0) then
      min_depth = s.d
    end
  end
  st.crash = { low = mid, low_ts = now_ms, min_depth = min_depth }
end

-- ── scenario B: FOMO arming (on_book, default OFF) ──────────────────────────

--- The ramp from the fomo_ramp_sec window's LOW to the fresh mid must reach
--- fomo_ramp_pct with mid at/above fomo_min_price. While armed, a fall back
--- below fomo_min_price ENDS the episode: a resting fade bid on the opposite
--- leg is stale — queue its cancellation.
local function fomo_check(cfg, st, now_ms, mid, tok)
  if not cfg.fomo_enabled then return end
  if st.fomo then
    if dec_cmp(mid, cfg.fomo_min_price) < 0 then
      if st.fomo.fade_price and st.fomo.fade_token then
        queue_break(st, st.fomo.fade_price, st.fomo.fade_token)
      end
      st.fomo = nil
    end
    return
  end
  local cutoff = now_ms - cfg.fomo_ramp_sec * 1000
  local low
  for i = 1, #st.samples do
    local s = st.samples[i]
    if s.ts >= cutoff and (not low or dec_cmp(s.p, low) < 0) then low = s.p end
  end
  if not low or low.m <= 0 then return end
  if dec_cmp(mid, cfg.fomo_min_price) < 0 then return end
  -- gain/low * 100 >= pct  ⟺  gain * 100 >= low * pct  (exact integers)
  if dec_cmp(dec_mul(dec_sub(mid, low), DEC_100), dec_mul(low, cfg.fomo_ramp_pct)) >= 0 then
    st.fomo = { fade_token = nil, fade_price = nil }
  end
end

-- ── spot closes (the underlying-asset veto, mad_dog's posture) ──────────────
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
--- `spot_missing` policy (default "block": no confirmation, no knife).
---
--- The move comparison is EXACT rational math, not mad_dog's float spelling:
--- dividing the scaled diff mantissa by POW10[diff.s] * oldest.m is only
--- scale-true when the oldest close carries scale 0 (mad_dog's closes at
--- scale s > 0 read POW10[s]x too small and the veto under-fires). Here:
--- move <= -thr  ⟺  |diff| * 100 >= thr * oldest (diff < 0), same for the
--- pump branch. Mantissa domain: closes ≤ 1e5 with ≤ 10 decimals keep every
--- product far inside i64.
local function spot_ok(cfg, asset, dir_up, now_ms)
  local arr = spot_bars[asset]
  local enough = false
  local diff, oldest
  if arr and #arr >= 2 then
    local cutoff = now_ms - cfg.spot_window_sec * 1000
    local oldest_ts, newest
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
      diff = dec_sub(newest, oldest)
    end
  end
  if not enough then return cfg.spot_missing == "pass" end
  if diff.m == 0 then return true end
  local abs_diff = diff.m < 0 and { m = -diff.m, s = diff.s } or diff
  local lhs = dec_mul(abs_diff, DEC_100)
  local rhs = dec_mul(cfg.spot_max_move_pct, oldest)
  if dir_up then
    -- a waterfall (move <= -thr) vetoes the UP buy
    if diff.m < 0 then return dec_cmp(lhs, rhs) < 0 end
    return true
  end
  -- a pump (move >= +thr) vetoes the DOWN buy
  if diff.m > 0 then return dec_cmp(lhs, rhs) < 0 end
  return true
end

-- ── the stabilization gate (the anti-knife door) ────────────────────────────

--- True when, inside an armed crash: (a) no new low for settle_quiet_sec,
--- AND (b) the CURRENT bid side has thickened to stabilize_depth_factor x
--- the crash period's thinnest observed bid_depth, floored at min_bid_depth.
--- Missing depth data is fail-closed (no depth, no bid). Evaluated against
--- the FRESH book at evaluate time — a thickened side must still be there
--- when the bid is actually suggested.
local function stabilized(cfg, st, now_ms, book)
  if not st.crash or st.confirmed then return false end
  if now_ms - st.crash.low_ts < cfg.settle_quiet_sec * 1000 then return false end
  local d = dec_parse(book.bid_depth)
  if not d then return false end
  local thr = cfg.min_bid_depth
  if st.crash.min_depth then
    local scaled = dec_mul(st.crash.min_depth, cfg.stabilize_depth_factor)
    if dec_cmp(scaled, thr) > 0 then thr = scaled end
  end
  return dec_cmp(d, thr) >= 0
end

-- ── entry points (§6.5) ─────────────────────────────────────────────────────

--- Every book update feeds the tracker (fresh or not — the crash state must
--- see the whole round, like mad_dog's tracker).
function bk_on_book(book)
  local cfg = read_config()
  local mid = dec_parse(book.mid)
  if not mid or mid.m <= 0 then return end
  local depth = dec_parse(book.bid_depth)
  local st = token_state(book.symbol)
  push_sample(st, book.ts_ms, mid, depth)
  trim_samples(st, book.ts_ms, ring_sec(cfg))
  crash_check(cfg, st, book.ts_ms, mid, depth, book.symbol)
  fomo_check(cfg, st, book.ts_ms, mid, book.symbol)
end

function bk_on_round(round)
  tracker_reset_if_new_round(round.slot)
end

--- The suggestion surface.
---   breaks — stale resting bids, drained first (valid in every phase).
---   exits  — ONE thesis_reversed verdict per held token per round; the
---            kernel prices it (the exits wire carries no price).
---   entries — scenario A: the crashed leg, only after the stabilization
---            gate; scenario B: the opposite of a ramped leg, at the fixed
---            fade price. Both time-gated by min_time_left_sec.
function bk_evaluate()
  local cfg = read_config()
  local entries, exits, breaks = {}, {}, {}

  local round = bk.round()
  if round == nil then
    return { entries = entries, exits = exits, breaks = breaks }
  end

  -- 1. stale-bid cancellations, any phase (a dead thesis must not leave a
  --    resting bid behind, least of all inside the force-exit window).
  for _, st in pairs(tracker.tokens) do
    for i = 1, #st.pending_breaks do
      local pb = st.pending_breaks[i]
      breaks[#breaks + 1] = { token = pb.token, broken_price = dec_fmt(pb.price) }
    end
    st.pending_breaks = {}
  end

  -- 2. THIS strategy's open positions (host-pushed): the exit watch and the
  --    no-pyramid entry guard both read this set.
  local held = {}
  local holdings = bk.holdings()
  if type(holdings) == "table" then
    for i = 1, #holdings do
      local h = holdings[i]
      if type(h) == "table" and type(h.token_id) == "string" then held[h.token_id] = true end
    end
  end

  local now_ms = bk.now_ms()

  -- 3. thesis-reversal watch (never time-gated): holding + fresh mid more
  --    than thesis_break_pct under the rebound anchor → one verdict. The
  --    watch reads the anchor, not the `confirmed` latch: a fill followed by
  --    a fresh low unconfirms the ENTRY state but the held position's thesis
  --    verdict must still fire.
  --    mid < anchor x (1 - pct)  ⟺  mid * 100 < anchor * (100 - pct).
  for tok, st in pairs(tracker.tokens) do
    if st.anchor and held[tok] and not st.exited then
      local book = bk.book(tok)
      if book and book.fresh == true then
        local mid = dec_parse(book.mid)
        if mid and mid.m > 0 then
          local lhs = dec_mul(mid, DEC_100)
          local rhs = dec_mul(st.anchor, dec_sub(DEC_100, cfg.thesis_break_pct))
          if dec_cmp(lhs, rhs) < 0 then
            st.exited = true
            exits[#exits + 1] = {
              token = tok,
              reason = string.format(
                "thesis_reversed: mid %s is more than %s%% under rebound anchor %s",
                dec_fmt(mid), dec_fmt(cfg.thesis_break_pct), dec_fmt(st.anchor)
              ),
            }
          end
        end
      end
    end
  end

  if round.time_left_sec < cfg.min_time_left_sec then
    return { entries = entries, exits = exits, breaks = breaks }
  end

  -- 4. entries — both scenarios, per market leg.
  for _, m in ipairs(bk.markets()) do
    local legs = { m.up_token, m.down_token }
    for li = 1, 2 do
      local tok = legs[li]
      local st = tracker.tokens[tok]
      if st and not held[tok] and not st.exited then
        local book = bk.book(tok)
        if book and book.fresh == true then
          local bid = dec_parse(book.best_bid)
          local ask = dec_parse(book.best_ask)
          local mid = dec_parse(book.mid)
          if bid and ask and mid and bid.m > 0 and ask.m > 0 and mid.m > 0 then

            -- Scenario A: the crashed leg, after the stabilization gate.
            if st.crash then
              if stabilized(cfg, st, now_ms, book) then
                st.confirmed = true
                st.anchor = mid
              end
              if st.confirmed then
                local dir_up = tok == m.up_token
                if spot_ok(cfg, m.asset, dir_up, now_ms) then
                  -- maker bid at the touch (+ offset): entry < ask is the
                  -- constructive slippage defense, never a crossing bid.
                  local price = dec_round2(dec_add(bid, cfg.entry_offset))
                  if dec_cmp(price, DEC_001) >= 0 and dec_cmp(price, ask) < 0 then
                    st.entry_price = price
                    entries[#entries + 1] = {
                      token = tok,
                      price = dec_fmt(price),
                      reason = string.format(
                        "%s leg crashed to %s (low %s), stabilized %ds, resting bid %s, %ds left",
                        dir_up and "UP" or "DOWN", dec_fmt(mid), dec_fmt(st.crash.low),
                        cfg.settle_quiet_sec, dec_fmt(price), round.time_left_sec
                      ),
                    }
                  end
                end
              end
            end

            -- Scenario B (default OFF): fade the OTHER leg's ramp at the
            -- fixed cheap price; only ever on the opposite leg of the ramp.
            if not st.crash then
              local opp = legs[3 - li]
              local st_opp = opp and tracker.tokens[opp]
              if st_opp and st_opp.fomo then
                local price = cfg.fomo_fade_price
                if dec_cmp(price, DEC_001) >= 0 and dec_cmp(price, ask) < 0 then
                  st_opp.fomo.fade_token = tok
                  st_opp.fomo.fade_price = price
                  entries[#entries + 1] = {
                    token = tok,
                    price = dec_fmt(price),
                    reason = string.format(
                      "fomo fade: opposite leg ramped, resting bid %s, %ds left",
                      dec_fmt(price), round.time_left_sec
                    ),
                  }
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
