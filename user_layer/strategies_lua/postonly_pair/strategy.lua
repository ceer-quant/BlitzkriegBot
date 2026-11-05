-- postonly_pair — the #872 post-only paired maker with a pairing LOOP, as a
-- Lua strategy package (§6.5 contract).
--
-- One sentence: on Polymarket's crypto 5m/15m Up/Down wheels, run the maker
-- pairing the onboarded wallets (gabagool22 et al.) actually run — BOTH
-- legs of the pair rest as post-only buys quoted strictly UNDER their own
-- asks, aiming at a combined cost under $1; a maker fill pays NO fee (the
-- docs' 20% maker rebate is replay-side modelling only, #741 — this
-- package never prices it, the conservative stance), and a complete pair
-- in hand is merged while a naked remainder either completes or rides to
-- settlement.
--
-- The skeleton is #871's maker_pair (post-only both-or-nothing, one locked
-- attempt, zero exits) — this package trades that rigidity for the pairing
-- LOOP the ticket wants, on the SAME kernel surfaces and ZERO kernel
-- changes:
--
--   * THE PAIR LOOP — an attempt is NOT a single frozen batch: a fresh
--     attempt emits both legs instantly; every LATER cycle the pair-type
--     re-emits BOTH legs again under the per-leg throttle (each leg no
--     more often than every `requote_sec` seconds — the 10–20 s re-quote
--     rhythm the ticket asks for), so a moved book gets a moved quote and
--     a stale entry (cancelled by the venue, filled-then-full-book) gets
--     a replacement attempt. The throttle is per (condition, token); a
--     fresh attempt stamps both legs the instant it fires.
--   * THE PAIR COST TEST — the fill path is the quotes: a maker fill
--     prints at its quote (at or below it, never worse) and a maker leg
--     pays NO fee, so the honest all-in cost of the pair is `quote_UP +
--     quote_DOWN`; the test is strict `<` against `max_pair_cost`. The
--     default `0.985` gives the venue's printed taker fees (shares × 0.07
--     × p(1−p) ≈ 1.75%/leg at the mid) full allowance on a $1 redemption.
--     `bk.fees()` is never read on this path: the manifest declares
--     `entry_maker`, the kernel routes every entry post-only, no taker
--     schedule ever applies to a leg.
--   * THE INVENTORY CAP — a leg's held shares (`bk.holdings()`: THIS
--     strategy's open positions, per-strategy sliced by the engine) count
--     against `max_shares_per_leg`; the pair declares the SMALLER leg's
--     head-room floored to the 0.01 grid (equal legs — the atomic batch's
--     symmetry). When either leg's head-room is zero the condition is
--     DONE: nothing more is quoted into it this round. Un-quoting at cap
--     means the naked remainder's downside is bounded by the cap times
--     the shares actually held — the ticket's hard inventory ceiling.
--   * THE NAKED REMAINDER — one leg filled, the partner still short: the
--     completion leg is re-qualified EVERY cycle by the top-up rule (the
--     increment redeems $1 against a sunk counterpart share, so the
--     ceiling prices the INCREMENTAL all-in = ask + the TAKER fee the
--     fallback would pay — the honest cost if this maker quote never
--     fills and the operator completes through a taker cross instead of
--     walking away; a maker fill can only fill CHEAPER) against
--     `max_top_up_cost`, sized to the exact SHORTFALL (counterpart share
--     − held shares on the short side), and throttled like any leg. The
--     re-entry is #848's: hold-to-settlement releases the naked side's
--     repeat-suppression, and the pair-completion entry exemption lets
--     the second leg through the "already in asset" gate. A shortfall
--     whose incremental all-in reaches `max_top_up_cost` is DEAD: no
--     completion quote, the held leg rides to settlement. The cost test
--     reads the schedule only for this fallback term — a nil schedule
--     prices the fallback fee at ZERO (the ambiguity is resolved down:
--     never a self-invented fee), which makes the gap EASIER to bid.
--   * THE NEAR-EXPIRY PULL (`breaks`) — at `time_left_sec <=
--     pull_before_sec` the condition's lingering entry bids are pulled
--     through the kernel's trend-break channel (a break cancels EVERY
--     resting BUY on the token — the reuse of the exact vocabulary
--     #904's predictors already carry), ONCE per condition per round.
--     `broken_price` is the token's live best bid at the pull instant
--     (the mark being pulled from; a bid-less or missing book falls back
--     to the 0.01 grid floor — an evidence field, not a trigger). After
--     the pull, NOTHING re-quotes that condition: the pull zone returns,
--     the entry floor rejects the rest of the round's ticks. The pull
--     threshold runs time only (no round_slot arithmetic; the kernel
--     sweeps an un-pulled leftover itself at expiry).
--   * THE LATCHES — `armed[cond] = slot` (the fresh attempt exists; the
--     pair-type re-quotes run under the throttle, never off it),
--     `pulled[cond] = slot` (the pull has fired for the round) and
--     `lastq[cond][token] = round clock ms` (the per-leg throttle). A new
--     round retires every latch; conditions do not survive their round.
--   * MERGE COLLECTION — the one exit this package emits: a COMPLETE pair
--     in hand (held UP > 0 and held DOWN > 0) sends `reason = "merge"`
--     for BOTH legs every cycle, the same collection posture
--     pair_discount_arb runs. The kernel intercepts "merge" BEFORE the
--     sell ladder and burns stacked pairs at $1/pair (fee-free); the
--     index-window position partials keep at most one stray $0.01-grid
--     share per leg, which rides to settlement.
--
-- Kernel surfaces this package USES (zero kernel changes):
--   * `entry_maker` (#742) — every entry is submitted post-only with the
--     never-escalate clock; the MakerThenTaker chase does not exist for a
--     declaring package. MAKER FEE = 0 is the entire ticket's premise.
--   * `atomic_batch` (#531) — both legs of one fresh attempt are ONE
--     economic ticket for the E-VA layer. NO virtual pot ships for this
--     package: the allowlist stays empty until replay + soak say GO.
--   * `holds_to_settlement` — the pair-completion entry exemption and the
--     #848 naked-side release (the completion re-quote's re-entry path).
--   * `breaks` — the pull channel (see above).
--   * `trailing_exit` is NOT declared — the three-state default (follow
--     the kernel-wide value) is the #742 contract.
--
-- Data notes:
--   * Prices run on SCALED INTEGERS (the §6.5 decimal-STRING wire rule);
--     every comparison is an exact integer comparison — no float
--     knife-edge anywhere.
--   * Quote modes (`entry_quote_mode`): 1 = ask − `entry_reprice_ticks`
--     (default — the most aggressive RESTABLE quote), 2 = mid floored to
--     the 0.01 grid, 3 = the bid. There is NO at-ask mode: quoting AT the
--     ask is the one price a post-only buy can never rest at (#464), and
--     this package's contract is "strictly under the ask". Any quote the
--     guards reject (at or above the live ask — a locked book; below the
--     0.01 grid) skips the WHOLE attempt: fail closed, no naked quote.

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

local GRID = { m = 1, s = 2 }   -- the venue's 0.01 price/size grid
local ZERO2 = { m = 0, s = 2 }

-- ── configuration (bk.params() — manifest tunables) ─────────────────────────

local function read_config()
  local p = bk.params()
  local cfg = {
    -- The all-in pair ceiling, tested on the SUM OF THE TWO QUOTES (maker
    -- fee = 0 — see the header). Strict `<`: a pair at the cap locks zero
    -- edge and is refused.
    max_pair_cost = dec_param(p, "max_pair_cost", { m = 985, s = 3 }),
    -- The per-leg INVENTORY CAP in shares (held shares count; the fresh
    -- pair's declared per-leg size). Default 1 = the 5m/15m scale, a pair
    -- ≈ $1 of notional.
    max_shares_per_leg = dec_param(p, "max_shares_per_leg", { m = 1, s = 0 }),
    -- A naked remainder's completion leg is chased ONLY while its
    -- incremental all-in (ask + the fallback taker fee — see the header)
    -- stays strictly under this ceiling; past it the gap is dead and the
    -- held leg rides to settlement.
    max_top_up_cost = dec_param(p, "max_top_up_cost", { m = 985, s = 3 }),
    -- Re-quote throttle: a leg's quote re-emits no more often than every
    -- `requote_sec` seconds of round life. 0 disables the throttle (the
    -- pair-type re-quotes every cycle, maker_pair's batch rhythm).
    requote_sec = int_param(p, "requote_sec", 15),
    -- Entry time floor: a maker quote needs round life left to be walked
    -- into. Gates the fresh attempt, the pair-type re-quotes AND the
    -- completion leg.
    min_time_left_sec = int_param(p, "min_time_left_sec", 60),
    -- NEAR-EXPIRY PULL: at `time_left_sec <= pull_before_sec` the
    -- condition's resting bids are pulled via `breaks`, once, and nothing
    -- re-quotes the condition afterwards. Clamped below
    -- `min_time_left_sec` so the quoting window and the pull zone never
    -- overlap (an earlier pull is the safe direction).
    pull_before_sec = int_param(p, "pull_before_sec", 20),
    -- The fill-path policy: WHERE the maker quote rests.
    entry_quote_mode = int_param(p, "entry_quote_mode", 1),
    entry_reprice_ticks = int_param(p, "entry_reprice_ticks", 1),
  }
  if cfg.pull_before_sec >= cfg.min_time_left_sec then
    cfg.pull_before_sec = cfg.min_time_left_sec - 1
  end
  return cfg
end

-- ── the maker quote (the fill path) ─────────────────────────────────────────

local TICK = { m = 1, s = 2 }        -- the venue's 0.01 price grid

--- The restable maker quote for ONE leg's book, or `nil` when no quote the
--- kernel would accept exists. Same contract as maker_pair's: every mode
--- floors onto the 0.01 grid, the result is strictly below the leg's own
--- ask, and `bid == nil` is an outage (fail closed).
local function maker_quote(bid, ask, cfg)
  local q
  if cfg.entry_quote_mode == 2 then
    if bid == nil then return nil end
    q = dec_floor2(dec_mul(dec_add(bid, ask), { m = 5, s = 1 }))
  elseif cfg.entry_quote_mode == 3 then
    if bid == nil then return nil end
    q = dec_floor2(bid)
  else
    q = dec_sub(ask, dec_mul(TICK, { m = math.max(cfg.entry_reprice_ticks, 1), s = 0 }))
    q = dec_floor2(q)
    -- never quote below the grid floor
    if dec_cmp(q, GRID) < 0 then q = GRID end
  end
  -- The post-only gate (#464): at or above the live ask the POST is
  -- refused — and this package's contract is STRICTLY under the ask.
  if dec_cmp(q, ask) >= 0 then return nil end
  return q
end

--- The reason-string label for a quote mode (every entry's reason names
--- its fill path, so a replay log reads the policy).
local function quote_label(cfg)
  if cfg.entry_quote_mode == 2 then return "post-only@mid" end
  if cfg.entry_quote_mode == 3 then return "post-only@bid" end
  return string.format("post-only@ask-%dt", math.max(cfg.entry_reprice_ticks, 1))
end

-- ── per-condition latches (round-scoped) ────────────────────────────────────
-- armed[cond]  = slot — the fresh attempt exists this round (pair-type
--                re-quotes run under the throttle, never off it).
-- pulled[cond] = slot — the near-expiry pull has fired for the round.
-- lastq[cond]  = { [token] = round clock ms } — the per-leg throttle table.

local armed = {}
local pulled = {}
local lastq = {}

-- ── inventory & schedule helpers ────────────────────────────────────────────

--- Shares of `token` held (parsed decimal), or 0 when none (2dp floor).
--- `bk.holdings()` is THIS strategy's open positions (per-strategy sliced
--- by the engine), exactly the gap-fill view #848's re-entry keys on.
local function held_shares(token)
  local hs = bk.holdings and bk.holdings() or {}
  for _, h in ipairs(hs) do
    if h.token_id == token then
      local d = dec_parse(h.shares)
      if d then return d end
    end
  end
  return ZERO2
end

--- Decimals only compare at a shared scale: `1 − p` at p's own scale.
local function one_of(p)
  return { m = POW10[p.s], s = p.s }
end

--- The taker fee per share at price `p` under the schedule in force, or a
--- ZERO fee when none is readable. The fee enters ONLY the naked
--- remainder's fallback cost (the price of completing through a taker
--- cross); a leg's own maker fill pays 0 and the pair test never reads
--- this. A nil schedule prices the fallback fee at 0 — never a
--- self-invented fee.
local function taker_fee_per_share(p)
  local fees = bk.fees()
  if fees == nil then return ZERO2 end
  local rate = dec_parse(fees.rate)
  local exponent = fees.exponent
  if type(exponent) ~= "number" or exponent < 0 or rate == nil or rate.m < 0 then
    return ZERO2
  end
  -- rate × p × (1−p), scaled by 10^exponent (the #741 fee curve).
  local f = dec_mul(rate, dec_mul(p, dec_sub(one_of(p), p)))
  -- 10^exponent divisor, capped (a rate scale past 30 would shift nowhere
  -- meaningful — the LIVE schedule carries 0–3).
  local div = 1
  for _ = 1, math.min(math.floor(exponent), 30) do div = div * 10 end
  if div > 1 then
    return { m = f.m // div, s = f.s }
  end
  return f
end

-- ── the pull mark ───────────────────────────────────────────────────────────

--- One break row per leg: `broken_price` is the token's live best bid at
--- the pull instant (the mark being pulled from); a bid-less or missing
--- book falls back to the 0.01 grid floor. Evidence, not a trigger.
local function pull_marks(m, breaks_out)
  for _, tok in ipairs({ m.up_token, m.down_token }) do
    local mark = GRID
    local b = bk.book(tok)
    if b and b.fresh == true then
      local bid = dec_parse(b.best_bid)
      if bid and bid.m > 0 then mark = bid end
    end
    breaks_out[#breaks_out + 1] = { token = tok, broken_price = dec_fmt(mark) }
  end
end

--- The per-leg throttle: true when the leg may (re-)quote this cycle.
local function throttle_ok(cond, token, now, cfg)
  if cfg.requote_sec <= 0 then return true end
  local t = lastq[cond]
  if t == nil then return true end
  local last = t[token]
  if last == nil then return true end
  return now - last >= cfg.requote_sec * 1000
end

--- Stamp the leg's throttle.
local function stamp(cond, token, now)
  local t = lastq[cond]
  if t == nil then t = {}; lastq[cond] = t end
  t[token] = now
end

--- Emit one fresh/refreshed PAIR (both legs, equal shares, same reason).
local function emit_pair(entries, m, qu, qd, shares, total, cfg, round)
  local template = "maker pair %s (%s+%s) vs cap %s, %s, %ds left"
  entries[#entries + 1] = {
    token = m.up_token,
    price = dec_fmt(qu),
    shares = dec_fmt(shares),
    reason = string.format(
      template,
      dec_fmt(total), dec_fmt(qu), dec_fmt(qd),
      dec_fmt(cfg.max_pair_cost),
      quote_label(cfg),
      round.time_left_sec
    ),
  }
  entries[#entries + 1] = {
    token = m.down_token,
    price = dec_fmt(qd),
    shares = dec_fmt(shares),
    reason = string.format(
      template,
      dec_fmt(total), dec_fmt(qu), dec_fmt(qd),
      dec_fmt(cfg.max_pair_cost),
      quote_label(cfg),
      round.time_left_sec
    ),
  }
end

-- ── entry points (§6.5) ─────────────────────────────────────────────────────

--- The suggestion surface, one round at a time. Per condition, at each
--- evaluation:
---   * at/below the pull threshold — pull BOTH legs' resting bids once
---     (`breaks`), then emit NOTHING for the condition for the round;
---   * above it, outside the entry floor — silent (the pull zone and the
---     quoting window never overlap; see read_config's clamp);
---   * above the floor with BOTH books fresh — exactly one of:
---       a complete pair in hand → merge BOTH legs every cycle;
---       a naked remainder → qualify the completion leg (top-up test,
---         shortfall size, throttle) or drop the gap;
---       nothing held → the pair: fresh attempt (instant) when none
---         exists, else the throttled pair-type re-quote; both paths test
---         the quote-sum against `max_pair_cost` and size to the
---         inventory cap.
--- Exits: ONLY the merge collection. Breaks: ONLY the pull.
function bk_evaluate()
  local cfg = read_config()
  local entries, exits, breaks_out = {}, {}, {}

  local round = bk.round()
  if round == nil then
    return { entries = entries, exits = exits, breaks = breaks_out }
  end
  local slot = round.slot
  local now = round.now_ms

  -- A new round retires every latch: conditions do not survive their round.
  for cond, s in pairs(armed) do
    if s ~= slot then armed[cond] = nil end
  end
  for cond, s in pairs(pulled) do
    if s ~= slot then pulled[cond] = nil end
  end

  -- The pull zone: at/below the threshold, pull once and stop — the
  -- entry floor (strictly above the clamped threshold) rejects every
  -- later tick of the round, so nothing re-quotes after the pull.
  if round.time_left_sec <= cfg.pull_before_sec then
    for _, m in ipairs(bk.markets()) do
      if pulled[m.condition_id] ~= slot then
        pull_marks(m, breaks_out)
        pulled[m.condition_id] = slot
      end
    end
    return { entries = entries, exits = exits, breaks = breaks_out }
  end

  -- The entry floor gates everything below (the maker quote needs round
  -- life left to be walked into).
  if round.time_left_sec < cfg.min_time_left_sec then
    return { entries = entries, exits = exits, breaks = breaks_out }
  end

  for _, m in ipairs(bk.markets()) do
    local bu = bk.book(m.up_token)
    local bd = bk.book(m.down_token)
    -- BOTH books fresh or nothing (the maker_pair guard): a missing/stale
    -- leg is an outage, not a half-signal. (A NAKED remainder still
    -- resolves below — its completion leg needs both books by the same
    -- rule: a quote priced off one leg's book alone is a guess.)
    if bu and bu.fresh == true and bd and bd.fresh == true then
      local au = dec_parse(bu.best_ask)
      local ad = dec_parse(bd.best_ask)
      if au and ad and au.m > 0 and ad.m > 0 then
        local hu = held_shares(m.up_token)
        local hd = held_shares(m.down_token)

        -- ── MERGE COLLECTION: a complete pair in hand redeems ────────
        if hu.m > 0 and hd.m > 0 then
          exits[#exits + 1] = { token = m.up_token, reason = "merge" }
          exits[#exits + 1] = { token = m.down_token, reason = "merge" }

        -- ── NAKED REMAINDER: exactly one leg held ────────────────────
        elseif hu.m > 0 or hd.m > 0 then
          -- The short side is the side whose held count is zero.
          local short_up = hu.m == 0
          local s_tok = short_up and m.up_token or m.down_token
          local s_ask = short_up and au or ad
          -- The increment redeems $1 against the sunk counterpart share,
          -- so the ceiling prices the INCREMENTAL all-in: the ask (this
          -- quote's worst fill prints AT the ask? No — a maker fill
          -- prints AT ITS QUOTE, but the test is on the ASK: the worst
          -- CASE is this quote never filling and the completion happening
          -- through a taker cross at the ask) plus the taker fee the
          -- fallback pays. Conservative: the gap is bid ONLY when even
          -- the expensive path clears the cap.
          local gap_cost = dec_add(s_ask, taker_fee_per_share(s_ask))
          if dec_cmp(gap_cost, cfg.max_top_up_cost) < 0 then
            local s_bid = short_up and dec_parse(bu.best_bid) or dec_parse(bd.best_bid)
            local quote = maker_quote(s_bid, s_ask, cfg)
            if quote then
              -- The shortfall: the counterpart's held count minus the
              -- short side's own (0). The short side still RE-QUOTES at
              -- the pair-type rhythm until the counterpart's full count
              -- is mirrored.
              local other = short_up and hd or hu
              local size = dec_floor2(other)
              if size.m > 0 then
                if throttle_ok(m.condition_id, s_tok, now, cfg) then
                  entries[#entries + 1] = {
                    token = s_tok,
                    price = dec_fmt(quote),
                    shares = dec_fmt(size),
                    reason = string.format(
                      "gap-fill %s leg: top up %s at %s (cost %s vs $1), %ds left",
                      short_up and "up" or "down",
                      quote_label(cfg),
                      dec_fmt(quote), dec_fmt(gap_cost),
                      round.time_left_sec
                    ),
                  }
                  stamp(m.condition_id, s_tok, now)
                end
              end
            end
          end

        -- ── THE PAIR: fresh attempt or throttled pair-type re-quote ──
        else
          local bu_bid = dec_parse(bu.best_bid)
          local bd_bid = dec_parse(bd.best_bid)
          local qu = maker_quote(bu_bid, au, cfg)
          local qd = maker_quote(bd_bid, ad, cfg)
          if qu and qd then
            -- The cost test on the QUOTES (maker fee = 0).
            local total = dec_add(qu, qd)
            if dec_cmp(total, cfg.max_pair_cost) < 0 then
              -- Inventory cap: the SMALLER leg's head-room binds (equal
              -- legs, the atomic batch's symmetry), floored to the grid.
              local held_min = hu.m >= hd.m and hd or hu
              local room = dec_sub(cfg.max_shares_per_leg, held_min)
              local shares = dec_floor2(room)
              if shares.m > 0 then
                if armed[m.condition_id] == slot then
                  -- The attempt exists — the pair-type re-quote under the
                  -- per-leg throttle (both legs move together: a pair is
                  -- stamped as one, so both clear the window together).
                  if throttle_ok(m.condition_id, m.up_token, now, cfg)
                    and throttle_ok(m.condition_id, m.down_token, now, cfg)
                  then
                    emit_pair(entries, m, qu, qd, shares, total, cfg, round)
                    stamp(m.condition_id, m.up_token, now)
                    stamp(m.condition_id, m.down_token, now)
                  end
                else
                  -- The FRESH attempt: instant (the throttle never
                  -- blocks an attempt that has not spoken this round).
                  emit_pair(entries, m, qu, qd, shares, total, cfg, round)
                  armed[m.condition_id] = slot
                  stamp(m.condition_id, m.up_token, now)
                  stamp(m.condition_id, m.down_token, now)
                end
              end
            end
          end
        end
      end
    end
  end
  return { entries = entries, exits = exits, breaks = breaks_out }
end
