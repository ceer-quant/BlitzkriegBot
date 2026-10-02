-- market_maker — the passive spread harvester, as a Lua strategy package
-- (§6.5 contract).
--
-- One sentence: on a 15-minute binary round, a cheap chip (mid 0.20–0.42)
-- whose book is deep, balanced and NOT one-sided earns a resting MAKER bid
-- 0.02 under its mid; when the chip's mid climbs 0.03 over the bid, the
-- spread is banked — suggest the close and re-arm at once. No direction
-- forecast, no knife-catching, no chasing: the profit thesis is volatility
-- paying the spread, never the outcome paying the holder.
--
-- The seal (the「止损不归你管」contract, gated by strategy-no-stop-loss-check):
-- this file computes no exit discipline of its own beyond the one take-profit
-- suggestion above, monitors no adverse excursion, and sends no size. The
-- kernel owns survival: its exit ladder (protective stop, time force-exit,
-- profit trailing — exit_policy.rs / user_layer/configs) runs underneath every
-- suggestion, and E26 owns the circuit breakers. `exits` carries only the
-- target-hit close; nothing else ever populates it.
--
-- The state machine (fill-blind by contract — the §6.5 surface has no fill or
-- position view, so "did the resting bid trade?" is unobservable):
--   nil pending            → one qualifying chip arms ONE resting bid.
--   pending {token, price} → mid >= price + spread → exit suggestion AND a
--                            break. The exit closes the filled world; the
--                            break retires the still-resting bid in the
--                            unfilled world (mid ran away from it — the
--                            thesis is done either way). Both land in one
--                            evaluate, so the next evaluate can re-arm: the
--                            spec's「价差到手就平，立刻开下一笔」.
--                          → the chip turns one-sided → break (cancel the
--                            bid; if it already filled, the KERNEL's ladder
--                            owns that position — this file never sells a
--                            loss).
--                          → round slot changes → break (stale bid).
--   Conditions the spec names that no Lua view exposes are disclosed here
--   once and not faked:
--     * 无同向持仓 — approximated by "one pending thesis at a time"; the
--       kernel's funds/risk path is the real gate.
--     * 未处于冷却期 — kernel-owned (E26 breakers); invisible here.
--     * account available — read only when the host surfaces a view (none
--       does today, same as mad_dog's bk.account note).
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

local function dec_sub(a, b)
  local s = math.max(a.s, b.s)
  return { m = dec_rescale(a, s) - dec_rescale(b, s), s = s }
end

local function dec_add(a, b)
  local s = math.max(a.s, b.s)
  return { m = dec_rescale(a, s) + dec_rescale(b, s), s = s }
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

local DEC_001 = { m = 1, s = 2 }
local DEC_100 = { m = 100, s = 0 }

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

-- ── the pending thesis ──────────────────────────────────────────────────────
-- pending = { token, price (dec), slot } | nil — the one resting bid this
-- strategy has armed. Fill-blind by contract; the kernel retires what did
-- not rest and closes what did.

local pending = nil

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

--- The suggestion surface. Returns { entries, exits, breaks }.
function bk_evaluate()
  local cfg = read_config()
  local entries, exits, breaks = {}, {}, {}

  -- A pending thesis whose round is gone (or replaced) retires its bid.
  local round = bk.round()
  local slot = round and round.slot or nil
  if pending and pending.slot ~= slot then
    breaks[#breaks + 1] = { token = pending.token, broken_price = dec_fmt(pending.price) }
    pending = nil
  end

  if round == nil then
    return { entries = entries, exits = exits, breaks = breaks }
  end

  local now_ms = bk.now_ms()

  -- Pending thesis upkeep, round live: bank the spread when the chip's mid
  -- climbs over bid + spread (exit suggestion + bid retirement in the same
  -- evaluate — see the state-machine note in the header); cancel the bid
  -- when the chip turns one-sided. A held position's survival is NEVER
  -- handled here — the kernel's ladder owns it.
  if pending then
    local book = bk.book(pending.token)
    local mid = book and book_dec(book.mid) or nil
    if book and book.fresh == true and mid and mid.m > 0 then
      local target = dec_add(pending.price, cfg.take_profit_spread)
      if dec_cmp(mid, target) >= 0 then
        exits[#exits + 1] = {
          token = pending.token,
          reason = string.format(
            "target reached: mid %s >= bid %s + %s",
            dec_fmt(mid),
            dec_fmt(pending.price),
            dec_fmt(cfg.take_profit_spread)
          ),
        }
        breaks[#breaks + 1] = { token = pending.token, broken_price = dec_fmt(pending.price) }
        pending = nil
      else
        local st = tracker.tokens[pending.token]
        if st and not chip_calm(cfg, st, now_ms) then
          breaks[#breaks + 1] = { token = pending.token, broken_price = dec_fmt(pending.price) }
          pending = nil
        end
      end
    end
  end

  -- Entries need the time window; the close suggestion above does not
  -- (a target hit in the last two minutes still banks the spread).
  if round.time_left_sec < cfg.min_time_left_sec then
    return { entries = entries, exits = exits, breaks = breaks }
  end

  -- One thesis in flight; the kernel's funds/risk path is the real
  -- position/cooldown gate (disclosed in the header).
  if pending then
    return { entries = entries, exits = exits, breaks = breaks }
  end

  local avail_ok = true
  local acct = bk.account()
  if acct and acct.available ~= nil then
    local a = dec_parse(acct.available)
    if a and dec_cmp(a, cfg.min_available_usd) < 0 then avail_ok = false end
  end

  if avail_ok then
    for _, m in ipairs(bk.markets()) do
      for _, tok in ipairs({ m.up_token, m.down_token }) do
        local book = bk.book(tok)
        local mid = book and book_dec(book.mid) or nil
        if book and book.fresh == true and mid and mid.m > 0
          and dec_cmp(mid, cfg.min_mid_price) >= 0
          and dec_cmp(mid, cfg.max_mid_price) <= 0 then
          local bid = book_dec(book.best_bid)
          local ask = book_dec(book.best_ask)
          if bid and ask and bid.m > 0 and ask.m > 0 then
            local depth_ok = true
            local bd = book_dec(book.bid_depth)
            if bd and dec_cmp(bd, cfg.min_bid_depth) < 0 then depth_ok = false end
            local ad = book_dec(book.ask_depth)
            if ad and dec_cmp(ad, cfg.min_ask_depth) < 0 then depth_ok = false end
            local obi_ok = true
            local o = book_dec(book.obi)
            if o then
              local abs = o.m < 0 and { m = -o.m, s = o.s } or o
              if dec_cmp(abs, cfg.max_abs_obi) > 0 then obi_ok = false end
            end
            local st = tracker.tokens[tok]
            if depth_ok and obi_ok and st and chip_calm(cfg, st, now_ms)
              and spot_ok(cfg, m.asset, tok == m.up_token, now_ms) then
              local entry = dec_round2(dec_sub(mid, cfg.bid_offset))
              if dec_cmp(entry, DEC_001) >= 0 and dec_cmp(entry, ask) < 0 then
                entries[#entries + 1] = {
                  token = tok,
                  price = dec_fmt(entry),
                  reason = string.format(
                    "%s chip mid %s in band, resting maker bid %s (%ds left)",
                    tok == m.up_token and "UP" or "DOWN",
                    dec_fmt(mid),
                    dec_fmt(entry),
                    round.time_left_sec
                  ),
                }
                pending = { token = tok, price = entry, slot = slot }
                break
              end
            end
          end
        end
      end
      if #entries > 0 then break end
    end
  end

  return { entries = entries, exits = exits, breaks = breaks }
end
