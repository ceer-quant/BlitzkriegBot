-- cross_venue_arb — the cross-venue pair collector, as a Lua strategy
-- package (§6.5 contract). Issue #427's "跨平台套利蓝图", v2 pairing.
--
-- One sentence: when the SAME real-world bet lists on the TWO CONFIGURED
-- venues (the operator's leg configuration — `arb.config.set` — riding the
-- params bag as the `arb_*` keys) and ask_UP@cheaper + ask_DOWN@other +
-- BOTH legs' taker fees costs less than $1.00 by more than `spread_open_pct`,
-- buy BOTH legs at their own asks with EQUAL share counts — the pair is worth
-- exactly $1.00 no matter which venue settles it, so the profit is locked at
-- entry. `spread_close_pct` is the release valve: while HELD, if the same
-- all-in pair cost falls back within `spread_close_pct` of the money (the
-- discount has been repriced away and the position no longer pays for its
-- risk), the legs are offered back via exits at their own bids; the kernel
-- owns the ladder, the fill and the final settlement either way.
--
-- v2 PAIRING (breaking change from v1, disclosed here on purpose):
--   v1 paired through `bk.unified_events()` — only a confirmed, zero-
--   discrepancy mapping's listings were priced, cross-venue by the #427
--   blueprint. v2 pairs from `bk.markets()` DIRECTLY: per configured asset,
--   exactly one row per leg venue (`arb_leg_a_venue` / `arb_leg_b_venue`)
--   must be present; the UP leg goes to the row quoting the CHEAPER UP ask,
--   the DOWN leg to the OTHER row. No confirmed-mapping handshake stands in
--   front of the trade any more — the operator's leg configuration IS the
--   pairing authority (kernel-side `arb.config.set` validated the two
--   venues, the two accounts and the pairs, fail-closed; unconfigured →
--   this strategy pairs NOTHING). ROLLBACK PATH: the v1 pairing lives in
--   this package's git history (restore strategy.lua from the v1.0.0
--   revision, re-run `node scripts/lua-decimal-sync.mjs --apply` to re-sign
--   the manifest, re-assign the package). Venue identity is still read as
--   data — the leg venues are CONFIG STRINGS compared against row venue
--   tags; no concrete venue name appears in code (no-venue-branch-check).
--
-- How the pieces fit:
--   * PAIRING — per asset ∈ `arb_pairs`: exactly one bk.markets() row per
--     leg venue, and both rows' UP books quoting fresh; the cheaper UP ask
--     wins the UP leg, the other row is the DOWN leg. A missing row, a
--     stale book, an unparseable quote — not a discount, an outage: the
--     asset is skipped this cycle (fail-closed).
--   * ENTRY — both legs, each priced AT its own ask, with `shares`
--     DECLARED: capacity is the SMALLER of the two legs' visible ask depth
--     (issue #427: "容量 = 两平台各自深度的最小值"), floored to the 0.01
--     grid and floored again by the kernel's own max-shares budget. The
--     kernel routes entries MakerThenTaker and owns every gate. Per-
--     DIRECTION emission: each leg carries its own `pair={key=,leg=}`
--     declaration (issue 568 阶段一) — the kernel-side PairBook is the
--     dedup authority (one pair per (instance, asset, round slot), same-
--     direction repeats are idempotent no-ops), so a leg the kernel refused
--     or dropped can be re-offered within the round while its sibling
--     stands. The strategy keeps NO cross-cycle Lua state of its own.
--   * FEES — priced from `bk.fees()`, the kernel's ONE schedule
--     (`rate * (p*(1-p))^exponent` per share, summed over BOTH legs). No
--     schedule in force → NO entries and NO exits: the fee is a cost
--     parameter of the same order as the edge, and guessing zero would
--     fabricate profit. Fail-closed by construction.
--   * EXIT — "回落 < Y% 平仓", per direction: a held leg whose all-in pair
--     cost has recovered past `1 − spread_close_pct` is released at its own
--     bid; the leg's OWN bid book gates its own release (the sibling's
--     outage cannot block it). This is an OPINION the kernel is free to
--     refuse, never a stop-loss: no price is sacred, the position is either
--     better closed than held under the SAME pair economics that opened it,
--     or it rides to settlement. The seal (strategy-no-stop-loss-check) is
--     respected — no reserved key is ever emitted, exits carry
--     { token, reason } only.
--   * CAPACITY DISPLAY — the chosen size and the binding depth are named in
--     every entry reason ("cap=min(depth)") so the panel can show capacity
--     provenance without parsing anything.
--
-- Data notes, stated because they change what the gates can see:
--   * Prices run on SCALED INTEGERS (the §6.5 decimal-STRING wire rule,
--     same helpers as flash_arb/pair_discount_arb). Every comparison in the
--     trigger is an exact integer comparison — no float knife-edge anywhere.
--   * PAIR DEDUP is KERNEL-SIDE now (issue 568 阶段一): the strategy reads
--     `bk.arb.pairs()` (its owner-filtered pair-book view, finally-
--     consistent — a reading may lag one beat behind the fills) and skips a
--     leg that already sits on a live pair row for this round's slot; the
--     kernel re-checks on arrival anyway (same (instance, asset, slot) ⇒
--     attach/idempotent-ignore), so the Lua check is an emission guard, not
--     the authority. No `armed` latch, no cross-cycle tables: state lives in
--     the kernel's PairBook or nowhere.
--   * Exit evaluation runs every cycle while a leg is held, keyed by the
--     leg's token from the current pairing.
--   * `bk.holdings()` is THIS strategy's view: a held leg is offered its
--     release; an unheld leg of an open pair is offered its entry (a
--     partially-filled pair can complete within the same round's economics).
--   * NO branch on a specific venue anywhere (issue #427 reverse
--     acceptance, gated by no-venue-branch-check): leg venues arrive as
--     config strings, `venue` is a string carried into the reason line only.

-- ── exact decimal helpers (pair_discount_arb's, unchanged semantics) ────────

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

-- ── the leg configuration (the kernel-pushed `arb_*` keys) ──────────────────

--- The leg configuration, read off the params bag the adapter merged
--- (`arb.config.set` → Core → engine hot-push → st.params). `nil` when ANY
--- of the five keys is missing, empty or malformed — the strategy then
--- pairs NOTHING this cycle (no entries, no exits): no legs spelled, no
--- cross-venue trade. Fail-closed by construction.
local function read_legs(p)
  local a_venue = p.arb_leg_a_venue
  local a_account = p.arb_leg_a_account
  local b_venue = p.arb_leg_b_venue
  local b_account = p.arb_leg_b_account
  local pairs_raw = p.arb_pairs
  if type(a_venue) ~= "string" or a_venue == ""
    or type(a_account) ~= "string" or a_account == ""
    or type(b_venue) ~= "string" or b_venue == ""
    or type(b_account) ~= "string" or b_account == ""
    or type(pairs_raw) ~= "string" or pairs_raw == "" then
    return nil
  end
  local pairs = {}
  for item in string.gmatch(pairs_raw, "[^,]+") do
    local asset = string.upper(item:match("^%s*(.-)%s*$"))
    if asset ~= "" then pairs[#pairs + 1] = asset end
  end
  if #pairs == 0 then return nil end
  return {
    a_venue = a_venue,
    a_account = a_account,
    b_venue = b_venue,
    b_account = b_account,
    pairs = pairs,
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

-- ── the book-quote helpers ──────────────────────────────────────────────────
-- One leg's view of one token's book. A stale / ask-less / unparseable /
-- depth-less book is not a discount, it is an outage: `nil`, and the caller
-- skips the asset (fail-closed).

--- Fresh-book best ask (the pairing decision's input: both rows' UP asks
--- decide which row is the UP leg).
local function book_ask(token)
  local b = bk.book(token)
  if not (b and b.fresh == true) then return nil end
  local a = dec_parse(b.best_ask)
  if not (a and a.m > 0) then return nil end
  return a
end

--- Fresh-book ask + ask depth (the tradable-leg view: a leg without
--- visible depth has no capacity).
local function book_leg(token)
  local b = bk.book(token)
  if not (b and b.fresh == true) then return nil end
  local a = dec_parse(b.best_ask)
  if not (a and a.m > 0) then return nil end
  local d = dec_parse(b.ask_depth)
  if not (d and d.m > 0) then return nil end
  return { ask = a, depth = d }
end

--- Fresh-book best bid (the release path's input).
local function book_bid(token)
  local b = bk.book(token)
  if not (b and b.fresh == true) then return nil end
  local v = dec_parse(b.best_bid)
  if not (v and v.m > 0) then return nil end
  return v
end

-- ── per-asset pairing ───────────────────────────────────────────────────────

--- The two legs of ONE asset across the configured venues: exactly one
--- bk.markets() row per leg venue (a duplicated venue row is ambiguous —
--- `nil`), both rows' UP books quoting fresh; the CHEAPER UP ask wins the
--- UP leg, the other row is the DOWN leg (the #427 blueprint: venue
--- identity is data, the ordering is by price). `nil` unless both leg rows
--- exist and both UP books quote — the asset then simply does not pair
--- this cycle.
local function pair_for_asset(asset, legs)
  local row_a, row_b
  for _, m in ipairs(bk.markets()) do
    if m.venue ~= nil and type(m.asset) == "string" and string.upper(m.asset) == asset then
      if m.venue == legs.a_venue then
        if row_a ~= nil then return nil end
        row_a = m
      elseif m.venue == legs.b_venue then
        if row_b ~= nil then return nil end
        row_b = m
      end
    end
  end
  if row_a == nil or row_b == nil then return nil end
  local ask_a = book_ask(row_a.up_token)
  local ask_b = book_ask(row_b.up_token)
  if ask_a == nil or ask_b == nil then return nil end
  local up_row, up_ask, down_row
  if dec_cmp(ask_a, ask_b) <= 0 then
    up_row, up_ask, down_row = row_a, ask_a, row_b
  else
    up_row, up_ask, down_row = row_b, ask_b, row_a
  end
  return { up_row = up_row, up_ask = up_ask, down_row = down_row }
end

-- ── the pair-book view helpers (issue 568 阶段一) ──────────────────────────

--- This strategy's live pair rows from `bk.arb.pairs()`（owner 过滤已在
--- 内核做完；读数最终一致 —— 附注 2）。返回 { [token]=true }：本round
--- slot 上已登记在任一未闭合配对里的腿 token 集合。读不到视图（宿主未
--- push / 旧内核 feature-detect 为 nil）按「无登记」处理 —— 去重的权威在
--- 内核 PairBook，这里只是发射闸，绝不视为错误。
local function paired_tokens(slot)
  local out = {}
  local ok, rows = pcall(function() return bk.arb.pairs() end)
  if not ok or rows == nil then return out end
  for _, p in ipairs(rows) do
    -- 只关心本轮 slot 上的活配对；closed 是终态（下一轮新键），旧轮
    -- 的行即便投影尚未清退也不拦本轮发射。
    if p.slot == slot and p.status ~= "closed" then
      for _, l in ipairs(p.legs or {}) do
        out[l.token] = true
      end
    end
  end
  return out
end

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

--- The suggestion surface. Per configured asset: a discount wider than
--- `spread_open_pct` emits the pair's legs (equal declared shares = the
--- smaller leg's ask depth), each gated by its own (asset, direction)
--- latch; a HELD leg whose all-in pair cost has fallen back within
--- `spread_close_pct` emits an exit intent at its own bid. `breaks` is
--- always empty (no resting order of ours survives an emission cycle the
--- kernel does not already own).
function bk_evaluate()
  local cfg = read_config()
  local entries, exits = {}, {}

  local round = bk.round()
  if round == nil then
    return { entries = entries, exits = exits, breaks = {} }
  end
  local slot = round.slot

  -- The leg configuration. Unconfigured (no `arb_*` keys in the bag — the
  -- kernel merges them only once `arb.config.set` has landed) or malformed
  -- → the strategy pairs NOTHING this cycle: no entries, no exits.
  -- Fail-closed.
  local legs = read_legs(bk.params())
  if legs == nil then
    return { entries = entries, exits = exits, breaks = {} }
  end

  -- A new round retires every attempt: the kernel-side PairBook is the
  -- state (retire_before 闭账旧轮), the strategy keeps no latch of its own.
  local paired = paired_tokens(slot)

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

  for _, asset in ipairs(legs.pairs) do
    local pair = pair_for_asset(asset, legs)
    if pair then
      -- Both legs' full book views (ask + depth): the pair economics and
      -- the capacity are computed from the SAME quotes the entries carry.
      local up_leg = book_leg(pair.up_row.up_token)
      local down_leg = book_leg(pair.down_row.down_token)
      if up_leg and down_leg then
        local f = pair_fee_per_share(up_leg.ask, down_leg.ask, rate, exponent)
        if f then
          local cost = dec_add(dec_add(up_leg.ask, down_leg.ask), f)
          local held_up = held_shares(pair.up_row.up_token)
          local held_down = held_shares(pair.down_row.down_token)

          if dec_cmp(cost, close_floor) >= 0 then
            -- 回落平仓, per direction: each HELD leg is offered its release
            -- when the pair no longer trades wide enough to pay for itself;
            -- the leg's OWN bid book gates its own release (a sibling's
            -- outage must not trap a closeable leg).
            if held_up.m > 0 then
              local bid = book_bid(pair.up_row.up_token)
              if bid then
                exits[#exits + 1] = {
                  token = pair.up_row.up_token,
                  reason = string.format(
                    "cross-venue release up@%s: pair cost %s back within %s%% of $1",
                    pair.up_row.venue, dec_fmt(cost), dec_fmt(cfg.spread_close_pct)
                  ),
                }
              end
            end
            if held_down.m > 0 then
              local bid = book_bid(pair.down_row.down_token)
              if bid then
                exits[#exits + 1] = {
                  token = pair.down_row.down_token,
                  reason = string.format(
                    "cross-venue release down@%s: pair cost %s back within %s%% of $1",
                    pair.down_row.venue, dec_fmt(cost), dec_fmt(cfg.spread_close_pct)
                  ),
                }
              end
            end
          elseif dec_cmp(cost, open_ceiling) <= 0
            and round.time_left_sec >= cfg.min_time_left_sec then
            -- 价差开仓, per direction: each UNHELD leg of the open pair is
            -- offered its entry — capacity = the SMALLER leg's visible ask
            -- depth (the pair is as good as its thinner leg, issue #427),
            -- and each leg answers to its OWN (asset, direction) latch key.
            local depth = dec_floor2(
              dec_cmp(up_leg.depth, down_leg.depth) < 0
                and up_leg.depth or down_leg.depth
            )
            if dec_cmp(depth, cfg.min_pair_shares) >= 0 then
              local leg_price_ok = function(ask)
                return dec_cmp(ask, cfg.min_leg_price) >= 0
                  and dec_cmp(ask, cfg.max_leg_price) <= 0
              end
              -- pair 键（issue 568 附注 1）：自报可读标签 = asset:slot；
              -- 经济身份 (asset, slot) 在内核从腿 token 派生，这里拼形只
              -- 进投影的 id，防重不依赖它。
              local pair_key = asset .. ":" .. slot
              local reason = string.format(
                "cross-venue pair %.4f (%s up@%s + %s down@%s + fees) vs $1, edge %.2f%%, cap=min(depth)=%s",
                cost.m / POW10[cost.s],
                dec_fmt(up_leg.ask), pair.up_row.venue,
                dec_fmt(down_leg.ask), pair.down_row.venue,
                (DEC_1.m / POW10[DEC_1.s] - cost.m / POW10[cost.s]) * 100,
                dec_fmt(depth)
              )
              -- 发射闸（内核兜底去重）：已登记在活配对里的腿不再 emit；
              -- 未登记的腿带 pair 声明发射（UP/DOWN 各带自己的 leg 标签，
              -- 方向不入经济键）。held 腿不重复开仓（旧语义保留）。
              if not paired[pair.up_row.up_token] and held_up.m <= 0
                and leg_price_ok(up_leg.ask) then
                entries[#entries + 1] = {
                  token = pair.up_row.up_token,
                  price = dec_fmt(up_leg.ask),
                  shares = dec_fmt(depth),
                  reason = reason,
                  pair = { key = pair_key, leg = "up" },
                }
              end
              if not paired[pair.down_row.down_token] and held_down.m <= 0
                and leg_price_ok(down_leg.ask) then
                entries[#entries + 1] = {
                  token = pair.down_row.down_token,
                  price = dec_fmt(down_leg.ask),
                  shares = dec_fmt(depth),
                  reason = reason,
                  pair = { key = pair_key, leg = "down" },
                }
              end
            end
          end
        end
      end
    end
  end
  return { entries = entries, exits = exits, breaks = {} }
end
