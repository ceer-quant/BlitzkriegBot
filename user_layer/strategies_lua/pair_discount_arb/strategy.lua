-- pair_discount_arb — the pair-discount collector, as a Lua strategy package
-- (§6.5 contract).
--
-- One sentence: when ask_UP + ask_DOWN + taker_fees < `max_pair_cost`, a
-- complete UP+DOWN share-pair of the condition is worth exactly $1.00
-- on-chain (MERGE) — buy BOTH legs at their asks with EQUAL share counts and
-- collect by merging. This is the @almach mechanism (85,431 fills, SELL=0):
-- the profit is locked at entry, the exit is not a decision but a collection.
--
-- How the pieces fit:
--   * ENTRY — one candidate per leg per condition, priced where a post-only
--     buy can actually REST: strictly UNDER the leg's own ask (`entry_quote_mode`,
--     default 1 = ask − 1 tick; 2 = mid floored to the grid; 3 = the bid;
--     0 = v1.2's at-ask, which the kernel REFUSES at POST — #464 — and which
--     is kept only as the replay baseline). The manifest declares
--     `entry_maker`: the kernel submits these legs as post-only maker quotes
--     at the quoted price — join the book, never cross — and they NEVER
--     escalate to taker; an unfilled leg simply rests to the round's own
--     end-of-life and the kernel's round-expiry sweep closes it. v1.2 moved
--     the strategy to the maker side of the book: at the current fee schedule
--     a taker-filled pair pays ~3.5% round-trip on a ~2.5% discount — the
--     edge dies in fees; maker legs pay no taker fee. v1.3 moved the quote
--     INTO the fill path: v1.2 quoted AT the ask, and a post-only buy whose
--     limit reaches the live ask is refused on POST (10/15 replay: 4 orders,
--     4 `venue.WouldCross`, 0 rests) — the one price a maker buy can never
--     rest at. The @almach corpus (the real profitable pair collector, 8
--     pair legs booked against the archive book): maker prints land 1–3
--     ticks BELOW the then-ask (resting bids the falling ask descended
--     into), taker prints AT the ask, never above. A/B vs any doctrine:
--     set the tunables (maker_mode equivalents = `entry_quote_mode`),
--     reload the package).
--     `shares` are DECLARED so both legs match in share count — a pair
--     merges per share-pair, and a naked leg has no collateral claim. Shares
--     size to the smaller leg's visible ask depth (floored to the 0.01 grid);
--     the kernel still caps at its own `max_shares`. The DISCOUNT TEST runs
--     on the asks — a maker fill always prints at or below its quote, so
--     realized pair cost only improves on the tested one. A leg with no
--     restable quote (locked book, quote below the 0.01 grid or below
--     `min_leg_price`) skips the attempt: fail closed, no naked quote.
--   * ENTRY WINDOW — `entry_window` (fraction of the round, 0.5 default)
--     confines NEW pair attempts to the round's FIRST half: `time_left_sec`
--     must be at or past `(1 − entry_window) × round_sec` still to run.
--     The @almach evidence: in-round fill density concentrates early and
--     late-round discounts are more expensive (pair cost median 1.0257 in
--     the tail vs the early window). Exact-integer compare, the
--     hot_side_momentum `round_sec` precedent:
--     `t*100 >= (100 − entry_window*100) * round_sec`. `0` = the whole
--     round. The window gates ATTEMPTS only — an armed condition keeps
--     collecting (merge + gap-fill) to the last tick.
--   * COLLECTION — once a condition's pair attempt is armed, every later
--     evaluation emits `reason = "merge"` exit intents for BOTH legs until
--     the round ends. The kernel intercepts "merge" intents BEFORE the sell
--     ladder: complete pairs burn into $1.00/pair of collateral
--     (`PositionManager::merge_condition` + `Ledger::credit_merge`), one-
--     sided or partial remainders stay open for settlement, and an intent
--     with no position behind it is dropped. The strategy never sells.
--   * FEES — priced from `bk.fees()`, the kernel's ONE schedule
--     (`rate * (p*(1-p))^exponent` per share, summed over both legs). No
--     schedule in force → NO entries: the fee is a cost parameter of the
--     same order as the edge, and guessing zero would fabricate profit.
--     Fail-closed by construction.
--   * GATES — the manifest declares `holds_to_settlement` (collection
--     semantics: the second leg passes the pair-completion entry exemption,
--     and the exit ladder leaves the legs alone in dry/read-only) and
--     waives timing + momentum (a market-neutral pair does not care which
--     way spot leans, or when in the round the discount appears; the
--     kernel's time-left floor still applies to the waived timing gate).
--
-- Data notes:
--   * Prices run on SCALED INTEGERS (the §6.5 decimal-STRING wire rule,
--     same helpers as flash_arb/spread_arb). Every comparison in the trigger
--     is an exact integer comparison — no float knife-edge anywhere.
--   * ONE pair ATTEMPT per condition per round (`armed` latch on emission):
--     a rejected attempt is not retried into the same book; the declared
--     share count carries the size.
--   * GAP FILL — a partial fill on one leg leaves a naked remainder that
--     rides to settlement (the strategy's only structural loss mode). The
--     kernel releases a token's repeat-suppression once its entry order
--     ends (filled, cancelled or rejected), so an ARMED condition reads
--     `bk.holdings()` every cycle: a leg holding fewer shares than its
--     counterpart is topped up with the exact shortfall, quoted by the SAME
--     fill-path policy (strictly under its current ask — an at-ask top-up
--     is refused at POST like any other; the earlier attempt's price is
--     history). The top-up pays for itself whenever the pair still merges
--     under `max_top_up_cost` — the incremental-cost test runs on the ASK
--     (the ceiling) while the fill prints at the quote (cheaper).
--   * Both books must be FRESH with a real ask on each leg. A stale or
--     bid-less leg is not a discount, it is an outage.

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
local DEC_1 = { m = 1, s = 0 }

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
    max_pair_cost = dec_param(p, "max_pair_cost", { m = 995, s = 3 }),
    -- A top-up share merges against a sunk-cost counterpart share, so its
    -- ceiling is per-$1 economics, not the fresh-pair test. Default leaves
    -- a 0.5¢ margin under $1; raising it past 1.00 would buy guaranteed
    -- losses and is rejected by the same `<` test.
    max_top_up_cost = dec_param(p, "max_top_up_cost", { m = 995, s = 3 }),
    min_leg_price = dec_param(p, "min_leg_price", { m = 1, s = 2 }),
    max_leg_price = dec_param(p, "max_leg_price", { m = 99, s = 2 }),
    min_pair_shares = dec_param(p, "min_pair_shares", { m = 1, s = 0 }),
    min_time_left_sec = int_param(p, "min_time_left_sec", 30),
    -- Entry window: only the FIRST `entry_window` fraction of the round may
    -- arm a NEW pair attempt (0 = the whole round). Read as an exact integer
    -- percent so the comparison never touches floats.
    entry_window_pct = int_param(p, "entry_window", 50),
    -- The round's total length, the hot_side_momentum precedent: bk.round()
    -- carries only `time_left_sec`, so progress needs the declared total
    -- (900 = the 15m wheel; a 5m family sets 300 via --round-sec).
    round_sec = int_param(p, "round_sec", 900),
    -- The fill-path policy (v1.3): WHERE the maker quote rests. The kernel
    -- refuses a post-only BUY whose limit reaches the live ask at POST time
    -- (#464, `venue.WouldCross`) — v1.2's at-ask quote is the ONE price a
    -- post-only buy can never rest at, so every leg died at submission
    -- (10/15 replay: 4 orders, 4 WouldCross, 0 rests, 0 fills). The @almach
    -- corpus (8 pair legs booked against the archive book,
    -- scripts/pair-fill-almach-fill-path.mjs): 4 maker prints 1–3 ticks
    -- BELOW the then-ask (resting bids the falling ask descended into) and
    -- 4 taker prints AT the ask, 0 above it. A restable quote is anything
    -- strictly under the ask; the mode picks how deep:
    --   0 = at the ask (v1.2 baseline; the kernel refuses the POST — kept
    --       only as the replay baseline arm, never as a default)
    --   1 = ask − `entry_reprice_ticks` (default: the most aggressive
    --       RESTABLE quote — the waterfall has to step only one tick onto us)
    --   2 = mid floored to the 0.01 grid (half a spread deep)
    --   3 = at the bid (the almach path: deepest; fills only in a real
    --       waterfall, and fills cheapest when it does)
    -- Any quote the guards reject (>= the live ask — a locked book; below
    -- the 0.01 grid; below `min_leg_price`) skips the leg: no restable
    -- quote, no attempt — fail closed. The DISCOUNT TEST stays on the asks:
    -- a maker fill always prints at or below its quote, so the realized
    -- pair cost can only improve on the tested one. The same policy prices
    -- gap-fill top-ups — an at-ask top-up is refused exactly like an at-ask
    -- entry (the doctrine is package-level: every entry of this package is
    -- a pure maker quote-pull).
    entry_quote_mode = int_param(p, "entry_quote_mode", 1),
    entry_reprice_ticks = int_param(p, "entry_reprice_ticks", 1),
  }
end

-- ── the maker quote (the fill path) ─────────────────────────────────────────

local TICK = { m = 1, s = 2 }      -- the venue's 0.01 price grid
local GRID_FLOOR = { m = 1, s = 2 } -- no buy may quote below 0.01

--- The restable maker quote for ONE leg's book, or `nil` when no quote the
--- kernel would accept exists (a locked book leaves nothing under the ask;
--- a floor-clamped quote that reaches the ask is a refusal, not a quote;
--- a bid-less leg prices nothing in modes 2–3). `ask` is a non-nil decimal
--- — the caller guards; `bid` may be nil. Exact integer arithmetic
--- throughout; modes 1–3 floor onto the 0.01 grid (the venue's tick) so a
--- live POST never dies on an off-grid limit.
local function maker_quote(bid, ask, cfg)
  -- Mode 0 is the v1.2 baseline VERBATIM (quote at the ask; the kernel's
  -- post-only gate refuses the POST — #464): no guards, so the replay
  -- baseline reproduces v1.2's exact emit-then-refuse behaviour.
  if cfg.entry_quote_mode == 0 then return ask end
  local q
  if cfg.entry_quote_mode == 2 then
    -- (bid + ask) / 2 then floor to the grid — dec_floor2 is the same
    -- floor-to-scale-2 the share grid uses, and a 0.01 price grid IS scale 2.
    -- Both price off the bid: a bid-less leg is an outage (fail closed).
    if bid == nil then return nil end
    q = dec_floor2(dec_mul(dec_add(bid, ask), { m = 5, s = 1 }))
  elseif cfg.entry_quote_mode == 3 then
    if bid == nil then return nil end
    q = dec_floor2(bid)
  else
    q = dec_sub(ask, dec_mul(TICK, { m = math.max(cfg.entry_reprice_ticks, 1), s = 0 }))
    q = dec_floor2(q)
    -- never quote below the grid floor
    if dec_cmp(q, GRID_FLOOR) < 0 then q = GRID_FLOOR end
  end
  -- The post-only gate (#464): at or above the live ask the POST is refused.
  -- The package's own price floor keeps the quote inside min_leg_price.
  if dec_cmp(q, ask) >= 0 then return nil end
  if dec_cmp(q, cfg.min_leg_price) < 0 then return nil end
  return q
end

--- The reason-string label for a quote mode (discipline: every entry's
--- reason names its fill path, so a replay log reads the policy).
local function quote_label(cfg)
  if cfg.entry_quote_mode == 0 then return "post-only@ask" end
  if cfg.entry_quote_mode == 2 then return "post-only@mid" end
  if cfg.entry_quote_mode == 3 then return "post-only@bid" end
  return string.format("post-only@ask-%dt", math.max(cfg.entry_reprice_ticks, 1))
end

-- ── the fee curve (bk.fees() — the kernel's ONE schedule) ───────────────────

--- rate * (p*(1-p))^exponent USD per share, exact scaled integers — the
--- same expression `FeeSchedule::fee_per_share` settles in. `nil` for a
--- price outside (0, 1): the kernel charges nothing there, but this
--- strategy treats such a print as a corrupt quote and refuses to price.
local function fee_per_share(p, rate, exponent)
  local one_minus = dec_sub(DEC_1, p)
  if one_minus.m <= 0 then return nil end
  local acc = { m = 1, s = 0 }
  for _ = 1, exponent do
    acc = dec_mul(acc, dec_mul(p, one_minus))
  end
  return dec_mul(rate, acc)
end

-- ── the pair-attempt latch ──────────────────────────────────────────────────
-- condition_id -> round_slot, set when both legs' entries are emitted;
-- cleared whenever the round slot moves on. Collection (merge intents) fires
-- for armed conditions regardless of the entry time floor — the collection
-- must run to the last tick, entries must not. A top-up is NOT a new
-- attempt: an armed condition may re-emit a shortfall leg every cycle
-- (kernel-side dedup drops a repeat while any order is still resting on
-- that token), so the latch stays set.

local armed = {}

-- ── holdings helpers ────────────────────────────────────────────────────────

--- Shares of ONE token currently held, from `bk.holdings()` (this
--- strategy's view, decimal STRING wire). `nil`-safe: no view pushed or a
--- foreign token reads as zero.
local function held_shares(token)
  local hs = bk.holdings and bk.holdings() or {}
  for _, h in ipairs(hs) do
    if h.token_id == token then
      local d = dec_parse(h.shares)
      if d then return d end
    end
  end
  return { m = 0, s = 2 }
end

-- ── entry points (§6.5) ─────────────────────────────────────────────────────

--- The suggestion surface. Per market: a discounted pair emits BOTH legs
--- (equal declared shares, each at its own ask) and arms collection; an
--- armed condition emits `reason = "merge"` intents for both legs every
--- evaluation until the round ends. Exits are NEVER sells — the kernel
--- intercepts "merge" before the ladder; there is no resting order, so
--- `breaks` is always empty.
function bk_evaluate()
  local cfg = read_config()
  local entries, exits = {}, {}

  local round = bk.round()
  if round == nil then
    return { entries = entries, exits = exits, breaks = {} }
  end
  local slot = round.slot

  -- A new round retires every latch: conditions do not survive their round.
  for cond, s in pairs(armed) do
    if s ~= slot then armed[cond] = nil end
  end

  -- Entry preconditions: inside the time floor AND a fee schedule in force.
  -- Both gate ENTRIES only; an armed condition keeps collecting to the end.
  local fees = bk.fees()
  local rate, exponent
  if fees ~= nil then
    rate = dec_parse(fees.rate)
    exponent = fees.exponent
    if type(exponent) ~= "number" or exponent < 0 or rate == nil or rate.m < 0 then
      rate, exponent = nil, nil
    end
  end
  local can_enter = round.time_left_sec >= cfg.min_time_left_sec
    and rate ~= nil and exponent ~= nil

  -- The in-round entry window: a NEW pair attempt needs time_left strictly
  -- past the window's floor — `(100 − pct)%` of the round still to run.
  -- Exact integers (`t*100` against `k*round_sec`), inclusive of the boundary
  -- instant; `pct <= 0` disables the window (the whole round enters). The
  -- window gates ATTEMPTS only: an armed condition's merge collection and
  -- gap-fill top-ups run to the last tick regardless.
  local window_open = cfg.entry_window_pct <= 0
    or round.time_left_sec * 100 >= (100 - cfg.entry_window_pct) * cfg.round_sec

  for _, m in ipairs(bk.markets()) do
    -- COLLECTION: the pair attempt for this round is live — ask the kernel
    -- to merge both legs. Complete pairs pay $1/pair; one-sided remainders
    -- are dropped by the kernel and ride to settlement.
    if armed[m.condition_id] == slot then
      exits[#exits + 1] = { token = m.up_token, reason = "merge" }
      exits[#exits + 1] = { token = m.down_token, reason = "merge" }
      -- GAP FILL: top up a partially-filled leg to its counterpart, quoted
      -- by the SAME fill-path policy as a fresh pair (strictly under its
      -- current ask — an at-ask top-up is refused at POST like any other).
      -- A top-up share merges against an ALREADY-HELD share of
      -- the other leg (sunk cost), so its economics are ask + fee vs $1.00 —
      -- cheaper than a fresh pair, and the ceiling is one minus a floor
      -- margin, NOT `max_pair_cost` (the pair test prices two fresh legs).
      -- `max_top_up_cost` keeps the discipline explicit and tunable; a
      -- shortfall is never chased past it — an uneconomical gap rides to
      -- settlement exactly as it does today. No holdings view (or a book
      -- that is gone) means no top-up: fail closed, never guess a shortfall.
      if can_enter then
        local bu = bk.book(m.up_token)
        local bd = bk.book(m.down_token)
        if bu and bu.fresh == true and bd and bd.fresh == true then
          local hu = held_shares(m.up_token)
          local hd = held_shares(m.down_token)
          local legs = {
            { book = bu,  held = hu, other = hd, side = "up" },
            { book = bd,  held = hd, other = hu, side = "down" },
          }
          for _, leg in ipairs(legs) do
            local gap = dec_sub(leg.other, leg.held)
            if gap.m > 0 then
              local a = dec_parse(leg.book.best_ask)
              if a and a.m > 0 then
                local f = fee_per_share(a, rate, exponent)
                if f then
                  -- Incremental all-in per share: ask + fee, redeeming $1
                  -- against a sunk-cost counterpart share. The TEST stays on
                  -- the ask (the cost ceiling); the quote below can only fill
                  -- cheaper — and an at-ask quote would be refused at POST
                  -- like any other (#464), so the top-up rests under the ask
                  -- under the SAME fill-path policy as a fresh pair.
                  local total = dec_add(a, f)
                  if dec_cmp(total, cfg.max_top_up_cost) < 0 then
                    local leg_bid = dec_parse(leg.book.best_bid)
                    local quote = maker_quote(leg_bid, a, cfg)
                    if quote then
                      entries[#entries + 1] = {
                        token = leg.side == "up" and m.up_token or m.down_token,
                        price = dec_fmt(quote),
                        shares = dec_fmt(dec_floor2(gap)),
                        reason = string.format(
                          "gap-fill %s leg: %.2f held vs %.2f counterpart, top up %s at %s (cost %s vs $1)",
                          leg.side,
                          leg.held.m / POW10[leg.held.s],
                          leg.other.m / POW10[leg.other.s],
                          quote_label(cfg),
                          dec_fmt(quote), dec_fmt(total)
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
    elseif can_enter and window_open then
      local bu = bk.book(m.up_token)
      local bd = bk.book(m.down_token)
      if bu and bu.fresh == true and bd and bd.fresh == true then
        local au = dec_parse(bu.best_ask)
        local ad = dec_parse(bd.best_ask)
        if au and ad and au.m > 0 and ad.m > 0
          and dec_cmp(au, cfg.min_leg_price) >= 0 and dec_cmp(au, cfg.max_leg_price) <= 0
          and dec_cmp(ad, cfg.min_leg_price) >= 0 and dec_cmp(ad, cfg.max_leg_price) <= 0 then
          local fu = fee_per_share(au, rate, exponent)
          local fd = fee_per_share(ad, rate, exponent)
          if fu and fd then
            -- All-in cost per pair: both asks + both entry fees. The merge
            -- redeems $1.00, so the locked edge is 1 − total. The TEST stays
            -- on the asks (the worst case); the quotes below can only fill
            -- cheaper.
            local total = dec_add(dec_add(au, ad), dec_add(fu, fd))
            if dec_cmp(total, cfg.max_pair_cost) < 0 then
              local du = dec_parse(bu.ask_depth)
              local dd = dec_parse(bd.ask_depth)
              if du and dd and du.m > 0 and dd.m > 0 then
                local shares = dec_floor2(dec_cmp(du, dd) < 0 and du or dd)
                if dec_cmp(shares, cfg.min_pair_shares) >= 0 then
                  -- v1.3 fill path: quote each leg where a post-only buy can
                  -- actually REST (strictly under its ask — #464 refuses the
                  -- at-ask POST). A leg with no restable quote skips the
                  -- whole attempt: a pair leg is worthless naked.
                  local bu_bid = dec_parse(bu.best_bid)
                  local bd_bid = dec_parse(bd.best_bid)
                  local qu = maker_quote(bu_bid, au, cfg)
                  local qd = maker_quote(bd_bid, ad, cfg)
                  if qu and qd then
                    local edge_pct = ((1 - total.m / POW10[total.s]) * 100)
                    local template = "pair %s (%s+%s+fee) vs $1, edge %.2f%%, maker %s, %ds left"
                    entries[#entries + 1] = {
                      token = m.up_token,
                      price = dec_fmt(qu),
                      shares = dec_fmt(shares),
                      reason = string.format(
                        template,
                        dec_fmt(total),
                        dec_fmt(au), dec_fmt(ad),
                        edge_pct,
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
                        dec_fmt(total),
                        dec_fmt(au), dec_fmt(ad),
                        edge_pct,
                        quote_label(cfg),
                        round.time_left_sec
                      ),
                    }
                    -- One attempt per condition per round: the declared share
                    -- count carries the size, a rejected attempt is not retried
                    -- into the same book.
                    armed[m.condition_id] = slot
                  end
                end
              end
            end
          end
        end
      end
    end
  end
  return { entries = entries, exits = exits, breaks = {} }
end
