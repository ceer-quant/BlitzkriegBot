-- whale_follow — the mid-round established-side taker entry that holds to
-- settlement (§6.5 contract).
--
-- One sentence: when a binary round is PAST its open (elapsed inside the
-- entry window) and the round's FAIR price — the symmetric consensus of both
-- legs, `fair = (up_mid + (1 - down_mid)) / 2` over trailing 10s means — has
-- established a direction (moved at least min_established_move off the
-- round-open fair), sitting AT its round running high, and reached that level
-- as a PLATEAU (|move over the last drift_window_sec| <= max_drift_60s, NOT a
-- spike) — lift the favored leg's ask once and hold: the manifest declares
-- holds_to_settlement and no exit is ever emitted.
--
-- Why FAIR and not the leg's own mid (the 2026-11-01 attribution replay):
--   The README's EST signal is built on the FAIR path (10s buckets of both
--   legs' BUY prints: 0.5*mean(Up) + 0.5*(1 - mean(Down))); v1 of this file
--   used each leg's OWN last mid and took the higher one as the established
--   leg. Re-judging the kernel's 71 fills on the same 450-round tape by the
--   README rule splits them cleanly: 33 fills ARE README-EST (net +$37.08,
--   EV +0.112/share, WR 87.9%) and 38 are NOT (net -$37.09, EV -0.098/share,
--   WR 68.4%). The binding gate is G4 (at round high): 39 pass / 38 fail, and
--   the 38 failures carry -$28.50 of the -$0.01 total. A per-leg reference
--   mistakes "each leg at its own new high" for establishment exactly when
--   the legs diverge (up rises AND down rises: the books are inconsistent,
--   fair does not move) — the symmetric fair path refuses that shape by
--   construction, which is why the README number is reproducible only on it.
--   Gate funnel on the README path over 450 rounds: 169 rounds pass all five
--   gates, 281 are blocked (G4 193, G3 54, G1+G3 5, G5 4, G2 3, rest mixed).
--
-- Why this shape (the evidence, 2026-11-01):
--   * The discriminating experiment ran on an UNBIASED 450-round BTC 5m tape
--     corpus (public API trades + gamma settlement truth, real taker prints
--     as entry prices, hard 1/0 settlement, both kernel fee schedules):
--       EST  (this package: elapsed 90-240s, leg mid >= 0.55, |move from
--             open| >= 0.05, |60s drift| <= 0.10, at running high)
--             n=161  WR 88.8%  EV +0.0743/share (legacy 0.125*(p(1-p))^2)
--                                  EV +0.0679/share (official 0.07*p(1-p))
--             both chronological halves positive (+0.0524 older / +0.0970 newer)
--       CHASE (same action, but entering mid-spike |60s drift| >= 0.15)
--             n=444  WR 73.6%  EV +0.0117 legacy / +0.0034 official
--             — the official-fee edge is ZERO. The plateau-vs-spike
--             distinction IS the edge; a too-tight plateau (0.05) starves
--             (n=30, negative) and a loose one (0.15) halves it.
--   * The whale anchors: 6meteor's 9 BTC/ETH 5m rounds (10-29, tracker
--     verified against REDEEM truth: 9/9 main legs won, entries at offsets
--     41-175s, VWAP 0.48-0.83, BUY-only, zero sells); 5 independent wallets
--     8/8 the same day (avg entry 0.49-0.58); on the 450-round tape, 267
--     distinct BUY-only whales >= $300/round, whose mid-late entries
--     (offset 120-240s) win 76.4% vs 63.9% for early ones — the same
--     offset window this package trades.
--   * The negative anchor it must never become: lua_momentum's up leg
--     chased single-tick momentum and went 7.5% WR / 40 live trades (and
--     the kernel census measured the naive shape at -$17.6k / 20,038
--     trades). The anti-chase gate (max_drift_60s) is the wall between
--     mid-round continuation and top-ticking a spike.
--
-- Data ceilings, stated because they change what this strategy can know:
--   * There is NO on-chain whale feed in the bk.* surface. "Whale" is
--     observed only through its market footprint — an established mid-round
--     level IS the proxy trigger. The strategy cannot see who accumulated;
--     it prices only the footprint the whole market sees.
--   * No klines are read. The round-open reference is the leg's own first
--     fresh mid SAMPLED INSIDE the round's opening window (first 2s): a
--     strategy (re)start mid-round never holds a trusted reference and the
--     leg stays unqualified for that whole round — fail-closed by
--     construction, never guessed.
--   * The 60s drift gate needs a full drift window of ring coverage; a
--     younger ring refuses (fail-closed).
--   * The evidence corpus is BTC 5m rounds; the default assets filter is
--     "BTC" and round_sec 300. Other horizons are an operator decision
--     (round_sec rescales the elapsed window; the corpus did not test them).
--
-- Mechanics (single_leg_pair / smart_money_reverse discipline):
--   * Samples are bucketed at 250ms per token (one sample per bucket, ring
--     = drift window + 5s): bounded memory, O(ring) per evaluation.
--   * One ATTEMPT per condition per round (armed latch) and one OPENING per
--     round slot: the whale shape is one main leg, not a ladder.
--   * Fees: entries require the kernel's ONE fee schedule in force
--     (`bk.fees()` non-nil and sane) — a kernel with no schedule is a
--     kernel whose cost regime is unknown; fail-closed.
--   * holds_to_settlement — declared; the kernel's generic exit ladder
--     leaves the legs alone. This package computes no exit, monitors no
--     exit, and expresses no exit (the「止损不归你管」boundary): the
--     winner redeems $1.00/share, the loser zero, and a leg without
--     REDEEM evidence is honestly held. `exits` and `breaks` are always
--     empty; no `reason = "merge"` is ever emitted (no complete pair).
--   * The manifest waives ONLY the kernel's timing floor
--     (timing_min_time_left_sec = 60 — the entry window's own edge) and
--     keeps the kernel's momentum veto at full strength: spot moving
--     against the bet still blocks, the kernel stays the second pair of
--     eyes.

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

-- ── configuration (bk.params() — manifest tunables) ─────────────────────────

local function read_config()
  local p = bk.params()
  return {
    round_sec = int_param(p, "round_sec", 300),
    assets = p.assets or "BTC",
    entry_elapsed_min_sec = int_param(p, "entry_elapsed_min_sec", 90),
    entry_elapsed_max_sec = int_param(p, "entry_elapsed_max_sec", 240),
    min_entry_price = dec_param(p, "min_entry_price", { m = 55, s = 2 }),
    max_entry_price = dec_param(p, "max_entry_price", { m = 88, s = 2 }),
    min_established_move = dec_param(p, "min_established_move", { m = 5, s = 2 }),
    max_drift_60s = dec_param(p, "max_drift_60s", { m = 10, s = 2 }),
    drift_window_sec = int_param(p, "drift_window_sec", 60),
  }
end

--- "BTC,SOL" -> { BTC=true, SOL=true }; "" / missing -> nil (all assets).
local function asset_set(csv)
  if type(csv) ~= "string" or csv == "" then return nil end
  local set = {}
  for a in csv:gmatch("[^,]+") do
    local t = a:match("^%s*(.-)%s*$")
    if t ~= "" then set[t] = true end
  end
  return (next(set) ~= nil) and set or nil
end

-- ── the 250ms rings: per-leg mids, per-condition FAIR ───────────────────────
-- tokens[tok] = { samples = { {ts, p=mid}, ... }, last_ts }   oldest first
--   The per-leg ring feeds the trailing 10s mean of ONE leg (the fair input).
--
-- fairs[condition_id] = {
--   samples  = { {ts, p=fair}, ... }   fair = (up_mid + (1 - down_mid)) / 2
--   open_ref = dec|nil  (first fair of the round — the round-open reference,
--                        trusted only when sampled inside the opening window)
--   open_ts  = ms|nil   (when open_ref was sampled — its qualification)
--   runmax   = dec|nil  (round running high of FAIR, not of a leg)
--   at_high  = bool     (the latest fair sits at that running high)
--   last_ts  = ms of the latest fair sample
-- }
-- verdicts[condition_id] = {fired, denied, budget}  this round's bucket flags
-- buckets = {no_signal, gate_blocked, budget_spent, fired}  per-round counts
-- latched[condition_id] = slot   (one attempt per condition per round)
-- opened[slot] = count           (one opening per round)

local BUCKET_MS = 250
-- The ring must retain the drift window AND enough slack that the sample at
-- the window's edge survives sparse feeds: a print 75s old (60s gate +
-- snapshot gap) is a legal drift reference; a ring of drift+30s keeps every
-- reference the gate can use and stays bounded (360 buckets).
local RING_PAD_SEC = 30

-- The round-open reference is TRUSTED only when it was sampled inside the
-- round's opening window (first 2s): later than that it is a mid-round price,
-- and an establishment measured from it is a guess, not evidence — the
-- condition stays unqualified for the whole round (fail-closed). 2s absorbs
-- the kernel's 1s time_left quantization plus the first book tick after the
-- open.
local OPEN_REF_QUALIFY_SEC = 2

-- The fair path is the mean of each leg's mids over the LAST 10s (the
-- README's bucket width), summed on a 1e-6 integer grid: no float ever
-- enters the signal (lua-decimal-check's rule), and ten 1e-6 samples stay
-- far inside i64 (a raw micro-mantissa sum of 18-digit parses would not).
local FAIR_WINDOW_SEC = 10
local FAIR_SCALE = 6
local MICRO_ONE = 1000000

local tracker = {
  slot = nil,
  tokens = {},   -- [tok]            -> leg ring
  fairs = {},    -- [condition_id]   -> fair ring + round extremes
  verdicts = {}, -- [condition_id]   -> this round's bucket flags
  buckets = { no_signal = 0, gate_blocked = 0, budget_spent = 0, fired = 0 },
  latched = {},
  opened = {},
}

local function token_state(tok)
  local st = tracker.tokens[tok]
  if not st then
    st = { samples = {}, last_ts = 0 }
    tracker.tokens[tok] = st
  end
  return st
end

local function fair_state(cid)
  local st = tracker.fairs[cid]
  if not st then
    st = { samples = {}, open_ref = nil, open_ts = nil,
           runmax = nil, at_high = false, last_ts = 0 }
    tracker.fairs[cid] = st
  end
  return st
end

local function verdict(cid)
  local v = tracker.verdicts[cid]
  if not v then
    v = { fired = false, denied = false, budget = false }
    tracker.verdicts[cid] = v
  end
  return v
end

--- Close the book on the departing round: every condition the strategy saw
--- lands in exactly ONE bucket. Priority: an opening that happened beats a
--- refusal; a budget refusal (the round's one opening was already spent)
--- beats a qualification refusal. A condition nobody could qualify is
--- `no_signal` — the honest reading, not a failure.
local function settle_round()
  for _, v in pairs(tracker.verdicts) do
    if v.fired then
      tracker.buckets.fired = tracker.buckets.fired + 1
    elseif v.budget then
      tracker.buckets.budget_spent = tracker.buckets.budget_spent + 1
    elseif v.denied then
      tracker.buckets.gate_blocked = tracker.buckets.gate_blocked + 1
    else
      tracker.buckets.no_signal = tracker.buckets.no_signal + 1
    end
  end
end

local function tracker_reset_if_new_round(slot)
  if slot ~= tracker.slot then
    settle_round()   -- counts land in tracker.buckets (they survive the reset)
    tracker.slot = slot
    tracker.tokens = {}
    tracker.fairs = {}
    tracker.verdicts = {}
    tracker.latched = {}
    tracker.opened = {}
  end
end

local function ring_sec(cfg)
  return cfg.drift_window_sec + RING_PAD_SEC
end

local function trim_ring(st, ts, cfg)
  local cutoff = ts - ring_sec(cfg) * 1000
  local kept, m = {}, 0
  for i = 1, #st.samples do
    if st.samples[i].ts >= cutoff then
      m = m + 1
      kept[m] = st.samples[i]
    end
  end
  st.samples = kept
end

--- Append/overwrite the current 250ms bucket on ONE leg's mid ring.
local function push_mid(st, ts, mid, cfg)
  local bucket = ts // BUCKET_MS
  local n = #st.samples
  if n > 0 and st.samples[n].ts // BUCKET_MS == bucket then
    st.samples[n] = { ts = ts, p = mid }
  else
    st.samples[n + 1] = { ts = ts, p = mid }
  end
  st.last_ts = ts
  trim_ring(st, ts, cfg)
end

--- Append/overwrite the current 250ms bucket on the FAIR ring and ratchet the
--- round extremes. at_high is computed BEFORE runmax moves: the sample that
--- SETS a new high IS the at-the-high sample (ties stay inclusive, the
--- README's `f >= runmax` spelling).
local function push_fair(st, ts, fair, cfg)
  local bucket = ts // BUCKET_MS
  local n = #st.samples
  if n > 0 and st.samples[n].ts // BUCKET_MS == bucket then
    st.samples[n] = { ts = ts, p = fair }
  else
    st.samples[n + 1] = { ts = ts, p = fair }
  end
  st.last_ts = ts
  st.at_high = (st.runmax == nil) or dec_cmp(fair, st.runmax) >= 0
  if st.runmax == nil or dec_cmp(fair, st.runmax) > 0 then st.runmax = fair end
  if st.open_ref == nil then st.open_ref = fair; st.open_ts = ts end
  trim_ring(st, ts, cfg)
end

--- Trailing mean of ONE leg's mids over the last FAIR_WINDOW_SEC, on the
--- 1e-6 integer grid. nil when the window holds no sample — the caller reads
--- that as "no priced view" (fail-closed).
local function mean_micro(st, now_ms)
  local cutoff = now_ms - FAIR_WINDOW_SEC * 1000
  local sum, n = 0, 0
  for i = 1, #st.samples do
    local s = st.samples[i]
    if s.ts >= cutoff and s.ts <= now_ms then
      sum = sum + dec_rescale(s.p, FAIR_SCALE)
      n = n + 1
    end
  end
  if n == 0 then return nil end
  return sum // n
end

--- fair = (up_mean + (1 - down_mean)) / 2 — the README's symmetric price.
--- Both legs quote the SAME event from opposite sides, so their consensus is
--- Up's mean averaged with the Down leg's complement. One leg alone has no
--- fair (fail-closed). Integer micro units throughout: no float.
local function fair_now(up_st, down_st, now_ms)
  local mu = mean_micro(up_st, now_ms)
  local md = mean_micro(down_st, now_ms)
  if mu == nil or md == nil then return nil end
  return { m = (mu + (MICRO_ONE - md)) // 2, s = FAIR_SCALE }
end

--- The mid at/before (now - drift_window_sec), or nil when the ring does not
--- reach back that far (fail-closed: no drift measurement, no entry).
local function mid_before(st, now_ms, window_sec)
  local cutoff = now_ms - window_sec * 1000
  local found
  for i = 1, #st.samples do
    if st.samples[i].ts <= cutoff then
      found = st.samples[i].p
    else
      break
    end
  end
  return found
end

--- |dec| — absolute value.
local function dec_abs(d)
  if d.m < 0 then return { m = -d.m, s = d.s } end
  return d
end

-- ── entry points (§6.5) ─────────────────────────────────────────────────────

--- (Re)price the FAIR of every market whose legs are currently quoted, at
--- `ts`: each leg's mids averaged over the trailing 10s, then combined
--- symmetrically. Called on every book tick (so the fair path is built at
--- tick granularity and the round-open reference lands inside the first 2s)
--- and again at each evaluation instant.
local function observe_fair(ts, cfg)
  for _, m in ipairs(bk.markets()) do
    local bu = bk.book(m.up_token)
    local bd = bk.book(m.down_token)
    if bu ~= nil and bd ~= nil then
      local mu = dec_parse(bu.mid)
      local md = dec_parse(bd.mid)
      if mu ~= nil and md ~= nil and mu.m > 0 and md.m > 0 then
        local f = fair_now(token_state(m.up_token), token_state(m.down_token), ts)
        if f ~= nil then push_fair(fair_state(m.condition_id), ts, f, cfg) end
      end
    end
  end
end

function bk_on_book(book)
  local cfg = read_config()
  local mid = dec_parse(book.mid)
  if not mid or mid.m <= 0 then return end
  push_mid(token_state(book.symbol), book.ts_ms, mid, cfg)
  observe_fair(book.ts_ms, cfg)
end

function bk_on_round(round)
  tracker_reset_if_new_round(round.slot)
end

--- The suggestion surface. whale_follow emits ENTRIES only — holding is the
--- thesis (holds_to_settlement), so `exits` and `breaks` are always empty.
function bk_evaluate()
  local cfg = read_config()
  local entries, exits, breaks = {}, {}, {}

  local round = bk.round()
  if round == nil then
    return { entries = entries, exits = exits, breaks = breaks }
  end
  local slot = round.slot
  tracker_reset_if_new_round(slot)

  -- Entry preconditions: the elapsed window, the per-round opening cap, and
  -- a fee schedule in force (fail-closed on the cost regime). The kernel's
  -- own timing and momentum gates stay in force on top — the manifest
  -- waives only the timing floor (its own edge is the window's edge).
  local fees = bk.fees()
  local schedule_ok = false
  if fees ~= nil then
    local rate = dec_parse(fees.rate)
    if type(fees.exponent) == "number" and fees.exponent >= 0
      and rate ~= nil and rate.m >= 0 then
      schedule_ok = true
    end
  end
  local elapsed = cfg.round_sec - round.time_left_sec
  local in_window = elapsed >= cfg.entry_elapsed_min_sec
    and elapsed <= cfg.entry_elapsed_max_sec
  -- in_window is loop-invariant. The per-round opening cap is NOT cached
  -- here: it is re-judged PER MARKET inside the loop (bk.markets() order is
  -- the kernel's, not a best-first ranking), so an intent emitted earlier in
  -- this same evaluation closes the budget for every later market — two
  -- qualifying conditions in one round may not each fire.
  local assets = asset_set(cfg.assets)
  -- The Down side's level floor is the mirror of the Up floor: fair <= 1 - min.
  local one_minus_floor = dec_sub({ m = 1, s = 0 }, cfg.min_entry_price)

  for _, m in ipairs(bk.markets()) do
    if assets and not assets[m.asset or ""] then
      -- asset filter: the evidence corpus is BTC; others stay silent.
    elseif tracker.latched[m.condition_id] == slot then
      -- One attempt per condition per round; the attempt already happened.
    elseif in_window then
      -- Both books must be FRESH with real asks — a stale side is an outage,
      -- not a signal, and the fair is never priced off one leg.
      local bu = bk.book(m.up_token)
      local bd = bk.book(m.down_token)
      local mu = bu and dec_parse(bu.mid) or nil
      local md = bd and dec_parse(bd.mid) or nil
      if bu and bu.fresh == true and bd and bd.fresh == true
        and mu ~= nil and md ~= nil and mu.m > 0 and md.m > 0 then
        -- Advance the FAIR path to this evaluation instant (book ticks push
        -- it too; the 250ms bucket dedupes).
        local f = fair_now(token_state(m.up_token), token_state(m.down_token),
                           round.now_ms)
        if f ~= nil then
          local fs = fair_state(m.condition_id)
          push_fair(fs, round.now_ms, f, cfg)
          -- Every priced market gets a verdict this round, even one whose
          -- shape never forms: a quiet round must settle as no_signal rather
          -- than vanish from the buckets (issue #854).
          local v = verdict(m.condition_id)

          -- The side FAIR favors (README: Up at/above the floor, Down at or
          -- below 1 - floor) — never a leg's own mid — and the price actually
          -- paid: THAT leg's ask.
          local tok, ask
          if dec_cmp(f, cfg.min_entry_price) >= 0 then
            tok, ask = m.up_token, dec_parse(bu.best_ask)
          elseif dec_cmp(f, one_minus_floor) <= 0 then
            tok, ask = m.down_token, dec_parse(bd.best_ask)
          end
          -- gate 4 — the entry band on the price actually paid (this is also
          -- the "a real ask exists" test: an empty side has no ask).
          local in_band = tok ~= nil and ask ~= nil and ask.m > 0
            and dec_cmp(ask, cfg.min_entry_price) >= 0
            and dec_cmp(ask, cfg.max_entry_price) <= 0
          -- gate 3 — at the FAIR running high (承接在极值上, not on a leg's)
          local at_extreme = fs.at_high
          -- gate 2 — the anti-chase wall: the level was reached as a
          -- PLATEAU, not a spike (|fair - fair@window_ago| <= cap). No
          -- window coverage -> no measurement -> no entry.
          local drift_ok, drift_txt = false, "none"
          local past = mid_before(fs, round.now_ms, cfg.drift_window_sec)
          if past ~= nil then
            local drift = dec_abs(dec_sub(f, past))
            drift_txt = dec_fmt(drift)
            drift_ok = dec_cmp(drift, cfg.max_drift_60s) <= 0
          end
          -- The signal SHAPE: everything measurable without the reference.
          if at_extreme and drift_ok and in_band then
            -- gate 0 — reference QUALIFICATION: the reference fair must sit
            -- inside the round's opening window. Past it the reference is a
            -- mid-round price, not the round open — the condition stays
            -- unqualified for this whole round (fail-closed).
            local ref_ok = fs.open_ts ~= nil
              and elapsed - (round.now_ms - fs.open_ts) / 1000
                <= OPEN_REF_QUALIFY_SEC
            -- gate 1 — establishment: |fair - fair_open| >= move
            local established = fs.open_ref ~= nil
              and dec_cmp(dec_abs(dec_sub(f, fs.open_ref)),
                          cfg.min_established_move) >= 0
            if not ref_ok or not schedule_ok then
              v.denied = true    -- 资格门挡: the shape held, a gate refused it
            elseif not established then
              -- the move off the open is too small: no signal, not a refusal
            elseif (tracker.opened[slot] or 0) >= 1 then
              v.budget = true    -- 预算耗尽: this round's one opening is spent
            else
              entries[#entries + 1] = {
                token = tok,
                price = dec_fmt(ask),
                reason = string.format(
                  "whale_follow: fair %s established (open ref %s, drift %s over %ds, at fair high), ask %s, %ds left | buckets no_signal=%d gate_blocked=%d budget_spent=%d",
                  dec_fmt(dec_round2(f)),
                  fs.open_ref ~= nil and dec_fmt(dec_round2(fs.open_ref)) or "none",
                  drift_txt, cfg.drift_window_sec, dec_fmt(ask),
                  round.time_left_sec,
                  tracker.buckets.no_signal, tracker.buckets.gate_blocked,
                  tracker.buckets.budget_spent
                ),
              }
              v.fired = true
              tracker.latched[m.condition_id] = slot
              tracker.opened[slot] = (tracker.opened[slot] or 0) + 1
            end
          end
        end
      end
    end
  end

  return { entries = entries, exits = exits, breaks = breaks }
end
