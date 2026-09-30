-- mad_dog — the panic-wick catcher, as a Lua strategy package (§6.5 contract).
--
-- One sentence: after one direction has held the round (mid >= 0.55 by a 20s
-- hold or a 60s rolling mean), a FAST panic wick below 0.35 arms a resting
-- MAKER bid 0.02 under the wick low — buy the panic-sold dominant side, never
-- chase. This is not a direction predictor; it is a liquidity-vacuum buyer.
--
-- The seal (the「止损不归你管」contract, gated by strategy-no-stop-loss-check):
-- this file computes no exit, monitors no exit, expresses no exit, and sends
-- no size. Entries cross as { token, price, reason } suggestions — the kernel
-- adjudicates, sizes (shares omitted → kernel budget), funds-gates and posts
-- them post_only. `exits` is never populated; the only non-entry row this
-- strategy emits is a `break` when the panic leg ENDS by recovery (mid back
-- above the broken level), which cancels the now-stale resting bid — the
-- mirror image of spread_arb's cancel-on-break.
--
-- Data notes, stated because they change what the gates can see:
--   * Prices run on SCALED INTEGERS (the §6.5 decimal-STRING wire rule, same
--     helpers as spread_arb). The only float divisions are single
--     price-move ratios compared against 1-2 decimal thresholds (same
--     argument as spread_arb's header: market-derived rationals cannot
--     disagree with a double below the 15th significant digit).
--   * The underlying-asset momentum gate (condition 3) reads CLOSED sec1
--     bars keyed by ASSET ("BTC"/"ETH"/…), which the E29 engine aggregator
--     feeds from the Binance spot stream (engine.rs `DataEvent::Spot`). The
--     frozen replay corpora are book+round only, so in those replays the
--     spot gate sees NO data; `spot_missing` decides that case (default
--     "block" — a strategy that cannot confirm the underlying is calm does
--     not buy the knife; "pass" exists for book-only diagnostics and is
--     disclosed as such).
--   * `bk.account()` is currently populated by no host path, so the
--     available-balance gate (condition 6) applies only when the view is
--     present; the kernel's own funds/risk path remains the real gate.
--   * `kline_stream` is deliberately NOT declared in manifest modes: the
--     plugin seam (extensions/polymarket) declares websocket_feed |
--     level2_snapshot | post_only, and the E29 kline feed is engine-side,
--     not a plugin capability. Declaring it would refuse this strategy at
--     enable time for a bit the seam does not carry.
--
-- Samples are bucketed at 250ms per token (one sample per bucket, ring =
-- dominant_mean_sec): bounded memory and a bounded per-callback instruction
-- count for the §6.3 sandbox, with granularity far below every window the
-- gates use.

-- ── exact decimal helpers (spread_arb's, unchanged semantics) ───────────────

local POW10 = { [0] = 1 }
for i = 1, 18 do POW10[i] = POW10[i - 1] * 10 end

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

local function dec_add(a, b)
  local s = math.max(a.s, b.s)
  return { m = dec_rescale(a, s) + dec_rescale(b, s), s = s }
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
    dominant_min_price = dec_param(p, "dominant_min_price", { m = 55, s = 2 }),
    dominant_hold_sec = int_param(p, "dominant_hold_sec", 20),
    dominant_mean_sec = int_param(p, "dominant_mean_sec", 60),
    broken_price = dec_param(p, "broken_price", { m = 35, s = 2 }),
    dip_speed_pct = dec_param(p, "dip_speed_pct", { m = 15, s = 0 }),
    dip_speed_sec = int_param(p, "dip_speed_sec", 3),
    spot_max_move_pct = dec_param(p, "spot_max_move_pct", { m = 5, s = 1 }),
    spot_window_sec = int_param(p, "spot_window_sec", 10),
    spot_missing = p.spot_missing == "pass" and "pass" or "block",
    min_time_left_sec = int_param(p, "min_time_left_sec", 180),
    min_bid_depth = dec_param(p, "min_bid_depth", { m = 500, s = 0 }),
    max_abs_obi = dec_param(p, "max_abs_obi", { m = 5, s = 1 }),
    min_available_usd = dec_param(p, "min_available_usd", { m = 100, s = 2 }),
    wick_offset = dec_param(p, "wick_offset", { m = 2, s = 2 }),
  }
end

-- ── per-token book tracker ──────────────────────────────────────────────────
-- tokens[tok] = { samples = { {ts, dec}, ... } (oldest first, one per 250ms
-- bucket), dominant = bool (latched for the round), episode = { extreme } |
-- nil, pending_break = dec | nil }

local BUCKET_MS = 250

local tracker = { slot = nil, tokens = {} }

local function token_state(tok)
  local st = tracker.tokens[tok]
  if not st then
    st = { samples = {}, dominant = false, episode = nil, pending_break = nil }
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

--- Condition 1, both spellings the spec allows, LATCHED for the round:
---   (a) hold  — a sample at/before `now - dominant_hold_sec` and every
---               sample since it is >= dominant_min_price;
---   (b) mean  — the mean of the samples inside the dominant_mean_sec
---               window (at least two) is >= dominant_min_price.
local function dominance_check(cfg, st, now_ms)
  local s = st.samples
  -- (a) the anchor is the NEWEST sample at or before the hold cutoff; the
  -- hold is proven when the anchor itself and everything after it are above
  -- the floor.
  local hold_cutoff = now_ms - cfg.dominant_hold_sec * 1000
  local anchor = 0
  for i = 1, #s do
    if s[i].ts <= hold_cutoff then anchor = i else break end
  end
  if anchor > 0 then
    local all_above = true
    for i = anchor, #s do
      if dec_cmp(s[i].p, cfg.dominant_min_price) < 0 then all_above = false; break end
    end
    if all_above then return true end
  end
  -- (b) rolling mean over the mean window, which must hold real coverage
  -- (span >= dominant_hold_sec) so a two-print burst cannot latch.
  local mean_cutoff = now_ms - cfg.dominant_mean_sec * 1000
  local total, cnt = { m = 0, s = 2 }, 0
  local oldest_ts, newest_ts
  for i = 1, #s do
    if s[i].ts >= mean_cutoff then
      total = dec_add(total, s[i].p)
      cnt = cnt + 1
      if not oldest_ts then oldest_ts = s[i].ts end
      newest_ts = s[i].ts
    end
  end
  if cnt >= 2 and newest_ts - oldest_ts >= cfg.dominant_hold_sec * 1000 then
    -- mean >= min  ⟺  total >= min * cnt
    if dec_cmp(total, dec_mul(cfg.dominant_min_price, { m = cnt, s = 0 })) >= 0 then
      return true
    end
  end
  return false
end

--- Condition 2: inside the broken zone, the drop INTO it must have been fast
--- (>= dip_speed_pct over the last dip_speed_sec, measured from the oldest
--- sample still inside that window — the pre-wick price). Once the episode
--- is armed, the extreme ratchets down with every new low; no speed is
--- demanded to CONTINUE an armed episode.
local function panic_check(cfg, st, now_ms, mid)
  if not st.episode then
    local cutoff = now_ms - cfg.dip_speed_sec * 1000
    local ref
    for i = 1, #st.samples do
      if st.samples[i].ts >= cutoff then ref = st.samples[i].p; break end
    end
    if not ref or ref.m <= 0 then return end
    -- drop/ref * 100 >= pct  ⟺  drop * 100 >= ref * pct
    local lhs = dec_mul(dec_sub(ref, mid), DEC_100)
    local rhs = dec_mul(ref, cfg.dip_speed_pct)
    if dec_cmp(lhs, rhs) < 0 then return end
    st.episode = { extreme = mid }
  elseif dec_cmp(mid, st.episode.extreme) < 0 then
    st.episode.extreme = mid
  end
end

-- ── spot closes (condition 3) ───────────────────────────────────────────────
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

-- ── entry points (§6.5) ─────────────────────────────────────────────────────

--- Every book update feeds the tracker (fresh or not — the dominance latch
--- must see the whole round, like spread_arb's tracker).
function bk_on_book(book)
  local cfg = read_config()
  local mid = dec_parse(book.mid)
  if not mid or mid.m <= 0 then return end
  local st = token_state(book.symbol)
  push_sample(st, book.ts_ms, mid)
  trim_samples(st, book.ts_ms, cfg.dominant_mean_sec + 2)
  if not st.dominant and dominance_check(cfg, st, book.ts_ms) then
    st.dominant = true
  end
  -- Recovery closes the panic leg and queues the cancel of its resting bid.
  if st.episode and dec_cmp(mid, cfg.broken_price) >= 0 then
    st.pending_break = st.episode.extreme
    st.episode = nil
  end
  if st.dominant then
    panic_check(cfg, st, book.ts_ms, mid)
  end
end

function bk_on_round(round)
  tracker_reset_if_new_round(round.slot)
end

--- The suggestion surface. Entries are resting maker bids 0.02 under the
--- armed episode's extreme; exits are NEVER emitted (the seal); breaks
--- cancel the stale bid of a panic leg that ended by recovery.
function bk_evaluate()
  local cfg = read_config()
  local entries, breaks = {}, {}

  local round = bk.round()
  if round == nil or round.time_left_sec < cfg.min_time_left_sec then
    return { entries = entries, exits = {}, breaks = breaks }
  end

  local now_ms = bk.now_ms()

  for tok, st in pairs(tracker.tokens) do
    if st.pending_break then
      breaks[#breaks + 1] = { token = tok, broken_price = dec_fmt(st.pending_break) }
      st.pending_break = nil
    end
  end

  for _, m in ipairs(bk.markets()) do
    for _, tok in ipairs({ m.up_token, m.down_token }) do
      local st = tracker.tokens[tok]
      if st and st.episode then
        local book = bk.book(tok)
        if book and book.fresh == true then
          local bid = dec_parse(book.best_bid)
          local ask = dec_parse(book.best_ask)
          local mid = dec_parse(book.mid)
          if bid and ask and mid and bid.m > 0 and ask.m > 0 and mid.m > 0 then
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
            local dir_up = tok == m.up_token
            if depth_ok and obi_ok and avail_ok and spot_ok(cfg, m.asset, dir_up, now_ms) then
              local entry = dec_round2(dec_sub(st.episode.extreme, cfg.wick_offset))
              if dec_cmp(entry, DEC_001) >= 0 and dec_cmp(entry, ask) < 0 then
                entries[#entries + 1] = {
                  token = tok,
                  price = dec_fmt(entry),
                  reason = string.format(
                    "%s panic wick below %s (low %s, resting bid %s, %ds left)",
                    dir_up and "UP" or "DOWN",
                    dec_fmt(cfg.broken_price),
                    dec_fmt(st.episode.extreme),
                    dec_fmt(entry),
                    round.time_left_sec
                  ),
                }
              end
            end
          end
        end
      end
    end
  end
  return { entries = entries, exits = {}, breaks = breaks }
end
