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
--   * ENTRY — one candidate per leg per condition, priced AT its own ask
--     (the kernel routes entries as MakerThenTaker: the top of the book
--     maker-fills immediately at that price, a remainder escalates), with
--     `shares` DECLARED so both legs match in share count — a pair merges
--     per share-pair, and a naked leg has no collateral claim. Shares size
--     to the smaller leg's visible ask depth (floored to the 0.01 grid);
--     the kernel still caps at its own `max_shares`.
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
--     counterpart is topped up AT ITS CURRENT ASK with the exact shortfall
--     (the declared-size discipline, re-priced to the book that must fill
--     it; the earlier attempt's price is history). The top-up pays for
--     itself whenever the pair still merges under `max_pair_cost` — the
--     SAME discount test as the entry, applied to the incremental cost.
--   * Both books must be FRESH with a real ask on each leg. A stale or
--     bid-less leg is not a discount, it is an outage.

-- ── exact decimal helpers (flash_arb's, unchanged semantics) ────────────────

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
  return { m = d.m // POW10[d.s - 2], s = 2 }
end

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
  }
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

  for _, m in ipairs(bk.markets()) do
    -- COLLECTION: the pair attempt for this round is live — ask the kernel
    -- to merge both legs. Complete pairs pay $1/pair; one-sided remainders
    -- are dropped by the kernel and ride to settlement.
    if armed[m.condition_id] == slot then
      exits[#exits + 1] = { token = m.up_token, reason = "merge" }
      exits[#exits + 1] = { token = m.down_token, reason = "merge" }
      -- GAP FILL: top up a partially-filled leg to its counterpart, at the
      -- CURRENT ask. A top-up share merges against an ALREADY-HELD share of
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
                  -- against a sunk-cost counterpart share.
                  local total = dec_add(a, f)
                  if dec_cmp(total, cfg.max_top_up_cost) < 0 then
                    entries[#entries + 1] = {
                      token = leg.side == "up" and m.up_token or m.down_token,
                      price = dec_fmt(a),
                      shares = dec_fmt(dec_floor2(gap)),
                      reason = string.format(
                        "gap-fill %s leg: %.2f held vs %.2f counterpart, top up at %s (cost %s vs $1)",
                        leg.side,
                        leg.held.m / POW10[leg.held.s],
                        leg.other.m / POW10[leg.other.s],
                        dec_fmt(a), dec_fmt(total)
                      ),
                    }
                  end
                end
              end
            end
          end
        end
      end
    elseif can_enter then
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
            -- redeems $1.00, so the locked edge is 1 − total.
            local total = dec_add(dec_add(au, ad), dec_add(fu, fd))
            if dec_cmp(total, cfg.max_pair_cost) < 0 then
              local du = dec_parse(bu.ask_depth)
              local dd = dec_parse(bd.ask_depth)
              if du and dd and du.m > 0 and dd.m > 0 then
                local shares = dec_floor2(dec_cmp(du, dd) < 0 and du or dd)
                if dec_cmp(shares, cfg.min_pair_shares) >= 0 then
                  local edge_pct = ((1 - total.m / POW10[total.s]) * 100)
                  entries[#entries + 1] = {
                    token = m.up_token,
                    price = dec_fmt(au),
                    shares = dec_fmt(shares),
                    reason = string.format(
                      "pair %.4f (%s+%s+fee) vs $1, edge %.2f%%, %ds left",
                      total.m / POW10[total.s],
                      dec_fmt(au), dec_fmt(ad),
                      edge_pct,
                      round.time_left_sec
                    ),
                  }
                  entries[#entries + 1] = {
                    token = m.down_token,
                    price = dec_fmt(ad),
                    shares = dec_fmt(shares),
                    reason = string.format(
                      "pair %.4f (%s+%s+fee) vs $1, edge %.2f%%, %ds left",
                      total.m / POW10[total.s],
                      dec_fmt(au), dec_fmt(ad),
                      edge_pct,
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
  return { entries = entries, exits = exits, breaks = {} }
end
