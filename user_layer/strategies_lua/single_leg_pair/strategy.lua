-- single_leg_pair — the single-leg round participant, as a Lua strategy
-- package (§6.5 contract). Issue #387.
--
-- One sentence: pair_discount_arb can only act when BOTH legs are liftable —
-- 55.1% of the @almach wallet's 15m rounds had only ONE leg worth lifting
-- (5,391 of 9,780), and this package enters those rounds: buy the one side
-- the book actually offers (or the round's declared leader), hold as a
-- NAKED directional position: the kernel owns its survival (settlement or
-- force-exit), the strategy owns entries and a per-round cap.
--
-- The ledger's honesty requirement (issue #387: 「诚实标注单腿轮的赔率分布」)
-- prices the risk boundary — single-leg rounds settled in 2026-08-05..09-30,
-- by the price actually paid:
--   <0.10   4.5% win rate, EV -0.046/share   (lottery: never buy)
--   0.10-0.30 ~10-23%, EV -0.05..-0.13       (lottery: never buy)
--   0.40-0.50 53.5%, EV +0.022               (coin-flip: dead band, skip)
--   0.60-0.70 81.8%, EV +0.198               (the fat band)
--   0.70-0.80 92.5%, EV +0.175               (the fat band)
--   0.80-0.90 96.0%, EV +0.120
--   >=0.90   99.2%, EV +0.065
-- A naked leg bought cheap is a lottery ticket, not a discount — the entry
-- band [0.55, 0.85] deliberately sits on the favourable part of that
-- distribution, and 0.50-0.55 is a dead band the strategy refuses rather
-- than guesses about.
--
-- How the pieces fit:
--   * TRIGGER (the book's own declaration, no spot feed — the 15m corpus
--     carries no spot events, so nothing here reads klines):
--       - sole liftable leg: ask_UP > 0 XOR ask_DOWN > 0 — the round is
--         single-leg BY THE BOOK; buy that leg at its ask when it prices in
--         the entry band (a 0.50-0.55 sole leg is skipped, never guessed).
--       - leader: both legs liftable — the strictly higher ask is the
--         leader (the book's directional verdict); fire when the leader's
--         ask sits in the band AND the raw price gap is at least
--         `min_gap` ticks wide. Equal asks name no leader; a gap thinner
--         than the floor is a pair-shaped book, not a verdict.
--   * NAKED EXPOSURE — the position is one-sided by construction. The pair
--     completion assumption does not exist here, so the protections are
--     declared explicitly:
--       - the manifest does NOT waive any entry gate (a directional bet is
--         not market-neutral; the kernel's timing gate and every risk gate
--         apply at full strength — only the strategy's own time floor
--         `min_time_left_sec` bounds entries further);
--       - `max_open_positions` caps the strategy's own openings per round
--         slot (positions settle within their round, so the cap is
--         a true concurrent-exposure ceiling);
--       - EXITS belong to the kernel (charter 「止损不归你管」, DEV_V0_3
--         §16.4): this package computes no stop, monitors no stop, and
--         expresses no stop — a leg rides its round to settlement, the
--         kernel's force-exit, or an honest hold when no REDEEM evidence
--         exists. The dead-zone deadline exit this round structure wants
--         is a KERNEL exit-policy surface, filed as issue #396; a
--         strategy-side stop is gate-blocked by design.
--   * holds_to_settlement — declared, and its ONE consequence here is that
--     the kernel's automated exit ladder (the generic 12% stop, the maker
--     ladder) leaves these legs alone: this package expresses no exits of
--     its own, so the legs ride their round. Settlement itself is
--     untouched — winning legs redeem $1.00, losing legs zero, exactly as
--     before; a leg with no REDEEM evidence is honestly held, never
--     fabricated into a result. The strategy NEVER emits `reason = "merge"`
--     (it holds no complete pair and never pretends to).
--   * FEES — entries require the kernel's ONE fee schedule in force
--     (`bk.fees()` non-nil and sane). The fee is not part of the trigger
--     inequality here (the band, not a cost cap, is the edge), but a
--     kernel with no schedule is a kernel whose cost regime is unknown —
--     fail-closed: no schedule, no entries.
--   * One ATTEMPT per condition per round (`armed` latch on emission): a
--     rejected attempt is not retried into the same book. When
--     pair_discount_arb runs alongside, its two-leg entries win the
--     same-token race and this package's attempt on that condition is
--     refused by the kernel's own-position gate — benign by design.
--
-- Data notes:
--   * Prices run on SCALED INTEGERS (the §6.5 decimal-STRING wire rule);
--     every comparison in the trigger is an exact integer comparison.
--   * Both books must be FRESH with real asks or real nils. A stale book is
--     an outage, not a signal; a bid-less leg with an ask is liftable.
--   * Round rollover retires every latch: conditions do not survive their
--     round.

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

local function dec_sub(a, b)
  local s = math.max(a.s, b.s)
  return { m = dec_rescale(a, s) - dec_rescale(b, s), s = s }
end

--- Floor to the venue's 0.01 share grid (sizes are positive; integer
--- division floors).
local function dec_floor2(d)
  if d.s <= 2 then return { m = d.m * POW10[2 - d.s], s = 2 } end
  return { m = d.m // POW10[d.s - 2], s = 2 }
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
    min_entry_price = dec_param(p, "min_entry_price", { m = 55, s = 2 }),
    max_entry_price = dec_param(p, "max_entry_price", { m = 85, s = 2 }),
    dead_band_floor = dec_param(p, "dead_band_floor", { m = 50, s = 2 }),
    min_gap = dec_param(p, "min_gap", { m = 2, s = 2 }),
    min_time_left_sec = int_param(p, "min_time_left_sec", 45),
    max_open_positions = int_param(p, "max_open_positions", 3),
    min_shares = dec_param(p, "min_shares", { m = 1, s = 0 }),
  }
end

-- ── latches (per round slot) ────────────────────────────────────────────────
-- armed[condition_id] = slot      — set when this package emits its entry;
--                                   one attempt per condition per round.
-- opened[slot] = n                — openings emitted this slot; the naked
--                                   exposure cap counts these.

local armed = {}
local opened = {}

-- ── entry points (§6.5) ─────────────────────────────────────────────────────

--- The suggestion surface. Per market, at most ONE entry per round (armed
--- latch): the sole liftable leg, or the in-band leader above the gap floor.
--- The strategy expresses NO exits — survival is the kernel's (settlement,
--- force-exit, honest hold; issue #396). Never a sell ladder, never
--- "merge" — no complete pair exists here. There is no resting order, so
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
  for s in pairs(opened) do
    if s ~= slot then opened[s] = nil end
  end

  -- Entry preconditions: inside the strategy's own time floor AND a fee
  -- schedule in force (fail-closed on the cost regime). The kernel's own
  -- timing gate stays in force on top — nothing is waived in the manifest.
  local fees = bk.fees()
  local schedule_ok = false
  if fees ~= nil then
    local rate = dec_parse(fees.rate)
    if type(fees.exponent) == "number" and fees.exponent >= 0
      and rate ~= nil and rate.m >= 0 then
      schedule_ok = true
    end
  end
  local can_enter = round.time_left_sec >= cfg.min_time_left_sec
    and schedule_ok
    and (opened[slot] or 0) < cfg.max_open_positions

  for _, m in ipairs(bk.markets()) do
    if armed[m.condition_id] == slot then
      -- One attempt per condition per round; the attempt already happened.
    elseif can_enter then
      local bu = bk.book(m.up_token)
      local bd = bk.book(m.down_token)
      if bu and bu.fresh == true and bd and bd.fresh == true then
        local au = dec_parse(bu.best_ask)
        local ad = dec_parse(bd.best_ask)
        if au ~= nil and ad ~= nil then
          local au_live = au.m > 0
          local ad_live = ad.m > 0
          local tok, ask, leg, other, why
          if au_live and not ad_live then
            -- SOLE liftable leg: the round is single-leg by the book.
            tok, ask, leg, other = m.up_token, au, "UP", "none"
            why = "sole"
          elseif ad_live and not au_live then
            tok, ask, leg, other = m.down_token, ad, "DOWN", "none"
            why = "sole"
          elseif au_live and ad_live then
            -- LEADER: the strictly higher ask is the book's directional
            -- verdict; equal asks name no leader. A gap below the floor is
            -- a pair-shaped book, not a verdict.
            local gap = dec_sub(au, ad)
            local agap = gap.m < 0 and { m = -gap.m, s = gap.s } or gap
            if dec_cmp(agap, cfg.min_gap) >= 0 then
              if dec_cmp(au, ad) > 0 then
                tok, ask, leg, other = m.up_token, au, "UP", dec_fmt(ad)
                why = "leader"
              else
                tok, ask, leg, other = m.down_token, ad, "DOWN", dec_fmt(au)
                why = "leader"
              end
            end
          end
          if tok ~= nil then
            -- The risk boundary, priced from the ledger's own odds table:
            -- inside the entry band (the favourable part of the
            -- single-leg distribution); the dead band is refused, not
            -- guessed; a sole leg below it is a lottery ticket.
            local in_band = dec_cmp(ask, cfg.min_entry_price) >= 0
              and dec_cmp(ask, cfg.max_entry_price) <= 0
            -- The dead band (just under the floor) is a coin-flip zone the
            -- ledger prices at ~zero EV: refused with a latch burned (one
            -- considered look per round), unlike the lottery zone below it,
            -- which stays silent and re-armable if the book reprices up.
            local dead_band = dec_cmp(ask, cfg.dead_band_floor) > 0
              and dec_cmp(ask, cfg.min_entry_price) < 0
            if in_band then
              local depth = dec_parse(leg == "UP" and bu.ask_depth or bd.ask_depth)
              if depth and depth.m > 0 then
                local shares = dec_floor2(depth)
                if dec_cmp(shares, cfg.min_shares) >= 0 then
                  entries[#entries + 1] = {
                    token = tok,
                    price = dec_fmt(ask),
                    shares = dec_fmt(shares),
                    reason = string.format(
                      "single-leg %s %s ask %s vs %s, %ds left",
                      why, leg, dec_fmt(ask), other, round.time_left_sec
                    ),
                  }
                  -- One attempt per condition per round; the declared share
                  -- count carries the size.
                  armed[m.condition_id] = slot
                  opened[slot] = (opened[slot] or 0) + 1
                  -- The per-round cap is an emission cap: once reached, no
                  -- further entries this slot.
                  if opened[slot] >= cfg.max_open_positions then
                    can_enter = false
                  end
                end
              end
            elseif dead_band then
              -- The coin-flip zone: refused, one look per round.
              armed[m.condition_id] = slot
            else
              -- Above the band's ceiling the residual upside no longer pays
              -- for the naked risk; below the dead band is the lottery zone.
              -- Both stay silent — no latch burned, a repricing into the
              -- band may still fire this round.
            end
          end
        end
      end
    end
  end
  return { entries = entries, exits = exits, breaks = {} }
end
