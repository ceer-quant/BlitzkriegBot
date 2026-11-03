-- maker_pair — the both-restable maker pair, as a Lua strategy package
-- (§6.5 contract).
--
-- One sentence: when the sum of the two RESTABLE maker quotes is under
-- `max_pair_cost`, emit BOTH legs of the binary pair as post-only buys —
-- each strictly UNDER its own ask, EQUAL declared share counts — and then
-- never touch the pair again. Issue #871's premise: `pair_discount_arb`
-- gap-fills a partially filled leg, and a single-leg position is that
-- package's structural loss; a 100 USDC account cannot eat a naked leg.
-- This package's answer is discipline, not repair:
--
--   * BOTH OR NOTHING — every leg must carry a book the kernel would accept
--     a post-only POST into: fresh, a real ask, visible ask depth, and a
--     quote strictly UNDER that ask (the kernel refuses an at-ask post-only
--     buy at POST — #464 `venue.WouldCross`). ONE leg failing any guard
--     abandons the WHOLE attempt: zero entries, not one. An abandoned
--     attempt is not an attempt — nothing is armed, and a later cycle with
--     both books back re-tests the pair from scratch.
--   * THE COST TEST RUNS ON THE QUOTES — a maker fill prints at its quote
--     (at or below it, never worse), and a post-only maker leg pays NO fee,
--     so the honest all-in cost of the pair is `quote_UP + quote_DOWN`.
--     At or above `max_pair_cost` (the test is strict `<`) the attempt is
--     abandoned whole. `bk.fees()` is never read: no fee schedule in force
--     is NOT an error here, because no fee enters this package's cost model
--     — the manifest declares `entry_maker`, so the kernel routes every
--     entry post-only and the taker schedule never applies. A venue that
--     charges maker fees must be priced into `max_pair_cost` by the
--     operator; guessing a fee here would double-count a cost the fill
--     path never pays.
--   * NO SINGLE-LEG REPAIR — one attempt per condition per round. A leg
--     that is rejected at POST, unfilled, or partially filled stays exactly
--     as it is: no top-up, no re-quote, no taker escalation, no merge
--     intent. Leg dead = leg dead; the remainder rides to settlement
--     (`holds_to_settlement`), which is the accepted structural loss and
--     the reason the default size is one share per leg. The kernel's
--     naked-leg requote exemption (issues #904/#848) keys on
--     hold-to-settlement strategies holding a single side and merely
--     RELEASES the counterpart token's repeat-suppression — this package
--     emits no leg intent after its one attempt, so nothing ever flows
--     through that exemption.
--   * EQUAL SHARES BY CONSTRUCTION — both legs declare the SAME
--     `shares_per_leg` (default 1: at mid prices a leg is ≈ $0.50, a pair
--     ≈ $1, and one order sits inside the 100 USDC config's $2 single-order
--     notional). The kernel still caps at its own `max_shares`.
--   * THE LATCH — `armed[condition] = round slot` set on emission. An
--     armed condition emits NOTHING on later evaluations: no collection,
--     no top-up, no re-quote — the batch WAS the attempt. A new round
--     retires every latch; conditions do not survive their round.
--
-- Kernel surfaces this package USES (this ticket made ZERO kernel changes):
--   * `entry_maker` (#742) — every entry is submitted post-only with the
--     never-escalate clock; the MakerThenTaker chase does not exist for a
--     declaring package.
--   * `atomic_batch` (#531) — the two legs of one evaluation cycle are ONE
--     economic ticket for the E-VA layer: an over-cap pot cancels the
--     batch with zero legs out. The declaration arms nothing by itself —
--     the pot comes from `virtual_limits`, and NO pot ships for this
--     package: the allowlist stays empty until the replay + soak gates
--     say GO.
--   * `holds_to_settlement` — the legs are meant to be collected at
--     expiry; the pair-completion entry exemption lets the second leg
--     through the "already in asset" gate when the first filled first,
--     and the exit ladder leaves the legs alone in dry/read-only.
--   * `trailing_exit` is NOT declared — the three-state default (follow
--     the kernel-wide value) is the #742 contract.
--
-- Data notes:
--   * Prices run on SCALED INTEGERS (the §6.5 decimal-STRING wire rule,
--     same helpers as pair_discount_arb). Every comparison in the trigger
--     is an exact integer comparison — no float knife-edge anywhere.
--   * Quote modes (`entry_quote_mode`): 1 = ask − `entry_reprice_ticks`
--     (default — the most aggressive RESTABLE quote: the falling ask has
--     to step only one tick onto us), 2 = mid floored to the 0.01 grid,
--     3 = the bid (deepest, fills only in a real waterfall). There is NO
--     at-ask mode: quoting AT the ask is the one price a post-only buy can
--     never rest at (#464), and this package's contract is "strictly under
--     the ask" — an at-ask quote is not a quote, it is a refusal waiting
--     to happen. Any quote the guards reject (at or above the live ask —
--     a locked book; below the 0.01 grid) skips the WHOLE attempt: fail
--     closed, no naked quote.

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

local function read_config()
  local p = bk.params()
  return {
    -- The all-in pair ceiling, tested on the SUM OF THE TWO QUOTES (maker
    -- fee = 0 — see the header). Strict `<`: a pair that costs exactly the
    -- cap locks zero edge and is refused.
    max_pair_cost = dec_param(p, "max_pair_cost", { m = 985, s = 3 }),
    -- The per-leg size in shares, identical on both legs by construction.
    -- Default 1 keeps one order inside the 100 USDC config's $2 single-order
    -- notional at any price a pair can pass the cost test with.
    shares_per_leg = dec_param(p, "shares_per_leg", { m = 1, s = 0 }),
    -- Entry time floor: a maker quote needs round life left to be walked
    -- into. Entries only — there is no armed lifecycle to keep running.
    min_time_left_sec = int_param(p, "min_time_left_sec", 30),
    -- The fill-path policy: WHERE the maker quote rests (see the header).
    -- 1 = ask − `entry_reprice_ticks` (default), 2 = mid floored to the
    -- 0.01 grid, 3 = the bid. Anything unrecognised reads as the default.
    entry_quote_mode = int_param(p, "entry_quote_mode", 1),
    entry_reprice_ticks = int_param(p, "entry_reprice_ticks", 1),
  }
end

-- ── the maker quote (the fill path) ─────────────────────────────────────────

local TICK = { m = 1, s = 2 }        -- the venue's 0.01 price grid
local GRID_FLOOR = { m = 1, s = 2 }  -- no buy may quote below 0.01

--- The restable maker quote for ONE leg's book, or `nil` when no quote the
--- kernel would accept exists (a locked book leaves nothing strictly under
--- the ask; a bid-less leg prices nothing in modes 2–3). `ask` is a
--- non-nil decimal — the caller guards; `bid` may be nil. Exact integer
--- arithmetic throughout; every mode floors onto the 0.01 grid so a live
--- POST never dies on an off-grid limit.
local function maker_quote(bid, ask, cfg)
  local q
  if cfg.entry_quote_mode == 2 then
    -- (bid + ask) / 2 then floor to the grid — both price off the bid:
    -- a bid-less leg is an outage (fail closed).
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
  -- The post-only gate (#464): at or above the live ask the POST is
  -- refused — and this package's contract is STRICTLY under the ask.
  if dec_cmp(q, ask) >= 0 then return nil end
  return q
end

--- The reason-string label for a quote mode (discipline: every entry's
--- reason names its fill path, so a replay log reads the policy).
local function quote_label(cfg)
  if cfg.entry_quote_mode == 2 then return "post-only@mid" end
  if cfg.entry_quote_mode == 3 then return "post-only@bid" end
  return string.format("post-only@ask-%dt", math.max(cfg.entry_reprice_ticks, 1))
end

-- ── the pair-attempt latch ──────────────────────────────────────────────────
-- condition_id -> round_slot, set when BOTH legs' entries are emitted in one
-- cycle (the atomic batch). Cleared whenever the round slot moves on. An
-- armed condition emits NOTHING afterwards — no collection, no top-up, no
-- re-quote: the batch was the whole attempt (leg dead = leg dead).

local armed = {}

-- ── entry points (§6.5) ─────────────────────────────────────────────────────

--- The suggestion surface. Per market: BOTH legs or NOTHING. Cost is the
--- sum of the two quotes (maker fee = 0) against `max_pair_cost`, strict.
--- One missing/stale book, one guard failure, one leg without a restable
--- quote, or a sum at/over the cap abandons the whole attempt without
--- arming anything. Exits are NEVER emitted — the legs ride to settlement
--- (`holds_to_settlement`); `breaks` is always empty.
function bk_evaluate()
  local cfg = read_config()
  local entries = {}

  local round = bk.round()
  if round == nil then
    return { entries = entries, exits = {}, breaks = {} }
  end
  local slot = round.slot

  -- A new round retires every latch: conditions do not survive their round.
  for cond, s in pairs(armed) do
    if s ~= slot then armed[cond] = nil end
  end

  -- The entry time floor gates the WHOLE surface: a quote with no round
  -- life left to be walked into is noise. There is no armed lifecycle to
  -- keep running past it (unlike pair_discount_arb's collection).
  if round.time_left_sec < cfg.min_time_left_sec then
    return { entries = entries, exits = {}, breaks = {} }
  end

  for _, m in ipairs(bk.markets()) do
    if armed[m.condition_id] == slot then
      -- One attempt per condition per round: the batch was the attempt.
      -- No collection, no top-up, no re-quote — leg dead = leg dead.
    else
      local bu = bk.book(m.up_token)
      local bd = bk.book(m.down_token)
      -- BOTH books or nothing: a missing, stale or phantom leg is not a
      -- half-signal, it is an outage — the whole attempt is abandoned
      -- without arming anything.
      if bu and bu.fresh == true and bd and bd.fresh == true then
        local au = dec_parse(bu.best_ask)
        local ad = dec_parse(bd.best_ask)
        local du = dec_parse(bu.ask_depth)
        local dd = dec_parse(bd.ask_depth)
        if au and ad and au.m > 0 and ad.m > 0
          and du and dd and du.m > 0 and dd.m > 0 then
          local bu_bid = dec_parse(bu.best_bid)
          local bd_bid = dec_parse(bd.best_bid)
          -- Both quotes FIRST: every leg must carry a quote the kernel
          -- would accept (strictly under its own ask) before anything is
          -- emitted — one legless quote abandons the whole pair.
          local qu = maker_quote(bu_bid, au, cfg)
          local qd = maker_quote(bd_bid, ad, cfg)
          if qu and qd then
            -- The cost test runs on the QUOTES (the fill path), maker fee
            -- = 0 — a maker fill prints at its quote, never worse.
            local total = dec_add(qu, qd)
            if dec_cmp(total, cfg.max_pair_cost) < 0 then
              local shares = dec_floor2(cfg.shares_per_leg)
              if shares and shares.m > 0 then
                local edge = dec_sub(DEC_1, total)
                local edge_pct = (edge.m / POW10[edge.s]) * 100
                local template = "maker pair %s (%s+%s) vs cap %s, edge %.2f%%, %s, %ds left"
                entries[#entries + 1] = {
                  token = m.up_token,
                  price = dec_fmt(qu),
                  shares = dec_fmt(shares),
                  reason = string.format(
                    template,
                    dec_fmt(total),
                    dec_fmt(qu), dec_fmt(qd),
                    dec_fmt(cfg.max_pair_cost),
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
                    dec_fmt(qu), dec_fmt(qd),
                    dec_fmt(cfg.max_pair_cost),
                    edge_pct,
                    quote_label(cfg),
                    round.time_left_sec
                  ),
                }
                -- One attempt per condition per round: the batch WAS the
                -- attempt; a rejected or unfilled leg is never retried.
                armed[m.condition_id] = slot
              end
            end
          end
        end
      end
    end
  end
  return { entries = entries, exits = {}, breaks = {} }
end
