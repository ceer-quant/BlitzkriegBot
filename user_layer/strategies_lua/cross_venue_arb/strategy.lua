-- cross_venue_arb — the cross-venue pair collector, as a Lua strategy
-- package (§6.5 contract). Issue #427's "跨平台套利蓝图".
--
-- One sentence: when the SAME real-world bet lists on two venues (a
-- confirmed `bk.unified_events()` mapping, zero discrepancies) and
-- ask_UP@venueA + ask_DOWN@venueB + BOTH legs' taker fees costs less than
-- $1.00 by more than `spread_open_pct`, buy BOTH legs at their own asks with
-- EQUAL share counts — the pair is worth exactly $1.00 no matter which venue
-- settles it, so the profit is locked at entry. `spread_close_pct` is the
-- release valve: while HELD, if the same all-in pair cost falls back within
-- `spread_close_pct` of the money (the discount has been repriced away and
-- the position no longer pays for its risk), the legs are offered back via
-- exits at their own bids; the kernel owns the ladder, the fill and the
-- final settlement either way.
--
-- How the pieces fit:
--   * PAIRING — only events that arrive through `bk.unified_events()` are
--     priced, and only in `status == "paired"` with an EMPTY discrepancies
--     list (a settlement-source / rules / expiry disagreement is a
--     PseudoHedge: the two venues may not pay the same way — fail closed,
--     exactly the #425→#426 handoff HedgePlan::assess enforces kernel-side).
--     The two legs are picked CROSS-venue by the #427 blueprint: the UP leg
--     from the listing whose ask is cheaper, the DOWN leg from the OTHER
--     listing — venue identity is read as data ("cheapest listing wins the
--     leg"), never branched on by name.
--   * ENTRY — both legs, each priced AT its own ask, with `shares`
--     DECLARED: capacity is the SMALLER of the two legs' visible ask depth
--     (issue #427: "容量 = 两平台各自深度的最小值"), floored to the 0.01
--     grid and floored again by the kernel's own max-shares budget. The
--     kernel routes entries MakerThenTaker and owns every gate.
--   * FEES — priced from `bk.fees()`, the kernel's ONE schedule
--     (`rate * (p*(1-p))^exponent` per share, summed over BOTH legs). No
--     schedule in force → NO entries and NO exits: the fee is a cost
--     parameter of the same order as the edge, and guessing zero would
--     fabricate profit. Fail-closed by construction.
--   * EXIT — "回落 < Y% 平仓": a held pair whose all-in cost has recovered
--     past `1 − spread_close_pct` is no longer a locked discount worth
--     holding against its risks (fill asymmetry, funding, settlement drag),
--     so both legs are offered as exit intents priced at their own bids.
--     This is an OPINION the kernel is free to refuse, never a stop-loss:
--     no price is sacred, the position is either better closed than held
--     under the SAME pair economics that opened it, or it rides to
--     settlement. The seal (strategy-no-stop-loss-check) is respected —
--     no reserved key is ever emitted, exits carry { token, reason } only.
--   * CAPACITY DISPLAY — the chosen size and the binding depth are named in
--     every entry reason ("cap=min(depth)") so the panel can show capacity
--     provenance without parsing anything.
--
-- Data notes, stated because they change what the gates can see:
--   * Prices run on SCALED INTEGERS (the §6.5 decimal-STRING wire rule,
--     same helpers as flash_arb/pair_discount_arb). Every comparison in the
--     trigger is an exact integer comparison — no float knife-edge anywhere.
--   * ONE pair ATTEMPT per event per round (`armed` latch on emission); the
--     kernel-side dedup drops a repeat while an order still rests on a
--     token. A rejected attempt is not retried into the same books.
--   * Exit evaluation runs every cycle while legs are held, keyed by the
--     pair's (up@A, down@B) token ids recorded at entry time.
--   * `bk.holdings()` is THIS strategy's view: a leg with no holding row and
--     no entry emitted this cycle simply has nothing to release.
--   * NO branch on a specific venue anywhere (issue #427 reverse
--     acceptance, gated by no-venue-branch-check): listings are ordered by
--     price, `venue` is a string carried into the reason line only.

-- ── exact decimal helpers (pair_discount_arb's, unchanged semantics) ────────

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
    -- "价差 > X% 开仓": open when the all-in pair cost ≤ 1 − X/100.
    spread_open_pct = dec_param(p, "spread_open_pct", { m = 2, s = 2 }),
    -- "回落 < Y% 平仓": while held, release when the all-in pair cost has
    -- risen back above 1 − Y/100 (the discount is gone; keep no risk).
    spread_close_pct = dec_param(p, "spread_close_pct", { m = 50, s = 2 }),
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

--- Both legs' taker fees for ONE share-pair at the given asks.
local function pair_fee_per_share(ask_up, ask_down, rate, exponent)
  local fu = fee_per_share(ask_up, rate, exponent)
  local fd = fee_per_share(ask_down, rate, exponent)
  if fu == nil or fd == nil then return nil end
  return dec_add(fu, fd)
end

-- ── the book-quote type ─────────────────────────────────────────────────────
-- One venue's listing quoted from the books: { venue, up_token, down_token,
-- ask_up, ask_down, depth_up, depth_down } or nil when any leg's book is
-- stale / ask-less / unparseable. A listing that cannot be fully quoted is
-- not a discount, it is an outage.

local function quote_listing(lua_listing)
  local bu = bk.book(lua_listing.up_token)
  local bd = bk.book(lua_listing.down_token)
  if not (bu and bu.fresh == true and bd and bd.fresh == true) then return nil end
  local au = dec_parse(bu.best_ask)
  local ad = dec_parse(bd.best_ask)
  if not (au and ad and au.m > 0 and ad.m > 0) then return nil end
  local du = dec_parse(bu.ask_depth)
  local dd = dec_parse(bd.ask_depth)
  if not (du and dd and du.m > 0 and dd.m > 0) then return nil end
  return {
    venue = lua_listing.venue,
    up_token = lua_listing.up_token,
    down_token = lua_listing.down_token,
    ask_up = au,
    ask_down = ad,
    depth_up = du,
    depth_down = dd,
  }
end

--- The pair the #427 blueprint trades: UP leg from the listing quoting the
--- cheaper UP ask, DOWN leg from the OTHER listing. `nil` unless exactly two
--- distinct listings quote cleanly. No branch on venue identity — the
--- ordering is by price, the winner is whoever is cheaper.
local function pick_pair(quoted)
  if #quoted ~= 2 then return nil end
  local a, b = quoted[1], quoted[2]
  if a.up_token == b.up_token or a.down_token == b.down_token then
    -- Same token on both rows: not a cross-venue pair.
    return nil
  end
  local up_from_a = dec_cmp(a.ask_up, b.ask_up) <= 0
  local up, down
  if up_from_a then
    up, down = a, b
  else
    up, down = b, a
  end
  return {
    up = up, down = down,
    up_leg = { venue = up.venue, token = up.up_token, ask = up.ask_up, depth = up.depth_up },
    down_leg = { venue = down.venue, token = down.down_token, ask = down.ask_down, depth = down.depth_down },
  }
end

-- ── the attempt latch ───────────────────────────────────────────────────────
-- event_id -> round_slot, set when both legs' entries are emitted; cleared
-- whenever the round slot moves on. Release (exit) evaluation is NOT gated
-- by the latch — a held pair must keep being repriced to the last tick.

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

--- The suggestion surface. Per confirmed paired event: a discount wider
--- than `spread_open_pct` emits BOTH legs (equal declared shares = the
--- smaller leg's ask depth), and arms the pair; a HELD pair whose all-in
--- cost has fallen back within `spread_close_pct` emits exit intents for
--- both legs at their own bids. `breaks` is always empty (no resting order
--- of ours survives an emission cycle the kernel does not already own).
function bk_evaluate()
  local cfg = read_config()
  local entries, exits = {}, {}

  local round = bk.round()
  if round == nil then
    return { entries = entries, exits = exits, breaks = {} }
  end
  local slot = round.slot

  -- A new round retires every latch: events do not survive their round.
  for id, s in pairs(armed) do
    if s ~= slot then armed[id] = nil end
  end

  -- Fail-closed fee schedule: both the entry economics AND the release
  -- verdict price BOTH legs' fees. No schedule → no suggestions at all.
  local fees = bk.fees()
  local rate, exponent
  if fees ~= nil then
    rate = dec_parse(fees.rate)
    exponent = fees.exponent
    if type(exponent) ~= "number" or exponent < 0 or exponent ~= math.floor(exponent)
      or rate == nil or rate.m < 0 then
      rate, exponent = nil, nil
    end
  end
  if rate == nil or exponent == nil then
    return { entries = entries, exits = exits, breaks = {} }
  end

  -- Open threshold: pair cost ≤ 1 − spread_open_pct/100, exact integers.
  local open_ceiling = dec_sub(DEC_1, { m = cfg.spread_open_pct.m, s = cfg.spread_open_pct.s + 2 })
  -- Release threshold: pair cost ≥ 1 − spread_close_pct/100.
  local close_floor = dec_sub(DEC_1, { m = cfg.spread_close_pct.m, s = cfg.spread_close_pct.s + 2 })

  for _, ev in ipairs(bk.unified_events()) do
    -- #425→#426 handoff, strategy-side mirror of HedgePlan::assess: only a
    -- confirmed PAIRED event with ZERO discrepancy marks is hedgeable.
    if ev.status == "paired" and #ev.discrepancies == 0 then
      local quoted = {}
      for _, l in ipairs(ev.listings) do
        local q = quote_listing(l)
        if q then quoted[#quoted + 1] = q end
      end
      local pair = pick_pair(quoted)
      if pair then
        local f = pair_fee_per_share(pair.up_leg.ask, pair.down_leg.ask, rate, exponent)
        if f then
          local cost = dec_add(dec_add(pair.up_leg.ask, pair.down_leg.ask), f)
          local held_up = held_shares(pair.up_leg.token)
          local held_down = held_shares(pair.down_leg.token)
          local holding = held_up.m > 0 or held_down.m > 0

          if holding then
            -- 回落平仓: the pair no longer trades wide enough to pay for
            -- itself — offer both legs back at their own bids. Leg books
            -- are re-quoted fresh (a stale book is not an exit either).
            if dec_cmp(cost, close_floor) >= 0 then
              local bu = bk.book(pair.up_leg.token)
              local bd = bk.book(pair.down_leg.token)
              if bu and bu.fresh == true and bd and bd.fresh == true then
                local bid_up = dec_parse(bu.best_bid)
                local bid_down = dec_parse(bd.best_bid)
                if bid_up and bid_down and bid_up.m > 0 and bid_down.m > 0 then
                  if held_up.m > 0 then
                    exits[#exits + 1] = {
                      token = pair.up_leg.token,
                      reason = string.format(
                        "cross-venue release up@%s: pair cost %s back within %s%% of $1",
                        pair.up_leg.venue, dec_fmt(cost), dec_fmt(cfg.spread_close_pct)
                      ),
                    }
                  end
                  if held_down.m > 0 then
                    exits[#exits + 1] = {
                      token = pair.down_leg.token,
                      reason = string.format(
                        "cross-venue release down@%s: pair cost %s back within %s%% of $1",
                        pair.down_leg.venue, dec_fmt(cost), dec_fmt(cfg.spread_close_pct)
                      ),
                    }
                  end
                end
              end
            end
          elseif round.time_left_sec >= cfg.min_time_left_sec
            and dec_cmp(cost, open_ceiling) <= 0
            and armed[ev.id] ~= slot then
            -- 价差开仓: capacity = the SMALLER leg's visible ask depth —
            -- the pair is as good as its thinner leg (issue #427).
            local depth = dec_floor2(
              dec_cmp(pair.up_leg.depth, pair.down_leg.depth) < 0
                and pair.up_leg.depth or pair.down_leg.depth
            )
            local leg_price_ok = function(ask)
              return dec_cmp(ask, cfg.min_leg_price) >= 0
                and dec_cmp(ask, cfg.max_leg_price) <= 0
            end
            if dec_cmp(depth, cfg.min_pair_shares) >= 0
              and leg_price_ok(pair.up_leg.ask) and leg_price_ok(pair.down_leg.ask) then
              local up_reason = string.format(
                "cross-venue pair %.4f (%s up@%s + %s down@%s + fees) vs $1, edge %.2f%%, cap=min(depth)=%s",
                cost.m / POW10[cost.s],
                dec_fmt(pair.up_leg.ask), pair.up_leg.venue,
                dec_fmt(pair.down_leg.ask), pair.down_leg.venue,
                (DEC_1.m / POW10[DEC_1.s] - cost.m / POW10[cost.s]) * 100,
                dec_fmt(depth)
              )
              entries[#entries + 1] = {
                token = pair.up_leg.token,
                price = dec_fmt(pair.up_leg.ask),
                shares = dec_fmt(depth),
                reason = up_reason,
              }
              entries[#entries + 1] = {
                token = pair.down_leg.token,
                price = dec_fmt(pair.down_leg.ask),
                shares = dec_fmt(depth),
                reason = up_reason,
              }
              -- One attempt per event per round: the declared share count
              -- carries the size.
              armed[ev.id] = slot
            end
          end
        end
      end
    end
  end
  return { entries = entries, exits = exits, breaks = {} }
end
