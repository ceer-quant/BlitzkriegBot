-- lua_momentum — the official Lua 5.4 example strategy (E30 / #336),
-- surgically narrowed to its one live-positive leg (2026-11-01 evidence),
-- with configurable exits (2026-11-02).
--
-- ⚠️ EXAMPLE / SINGLE-EDGE PACKAGE: the v2-corpus census after #419
-- (docs/reports/evolution/RETEST-2026-10-10.md §4) measured the OLD
-- both-legs taker shape at net −$17,575.78 over 20,038 closed trades. The
-- unmanaged live window (2026-10-24→26) then split the legs apart: buying
-- the DOWN leg while its own price was crashing filled 44 trades at WR
-- 56.8% (net +$22.79; the 0.20-0.35 entry band filled at WR 75%), while
-- chasing the UP leg filled 40 trades at WR 7.5% (net −$40.37; the 17
-- fills at 0.40+ all lost). The up leg is DELETED below — it never trades
-- again, and no tunable re-arms it.
--
-- The one edge this package keeps: when a DOWN token's own mid crashes by
-- `threshold` percent in one tick, rest a PURE MAKER bid at its best_bid
-- (manifest `entry_maker: true`, issue 742 — post-only, never escalates to
-- taker). In a live crash the falling price crosses the resting bid, so a
-- fill is a maker fill (no taker fee, plus the maker rebate); an unfilled
-- bid simply rests to the round's end — a missed chance, never a loss.
-- Catching the knife WHILE it is still falling is the trade; chasing a
-- spike is not (the smart_money_reverse falsification round proved the
-- stabilize-then-bid variant negative: after an informed crash the book
-- does not retest).
--
-- Exits (2026-11-02). The package used to return empty `exits` and declare
-- `holds_to_settlement`, so on PolySim (the kernel settles locally) the
-- policy ladder SKIPS the position, and the only profit rule that would
-- still have applied on a live ladder is the +100% backstop. A winner that
-- ran +50% and gave it all back before settlement never produced a sell.
-- `bk_evaluate` now emits strategy-signal exits the kernel honours even for
-- a holder. Every exit is judged on the fresh BEST BID minus a taker fee
-- (official `rate * p * (1-p)` per share; a zero PolySim schedule does not
-- zero the model). The mid is never a sell price. A position that never
-- hits a rule still rides to settlement — the down-leg edge is the
-- collection, the new rules only bank a giveback.
--
-- Holding both sides of one round is NOT the trade. `holds_to_settlement`
-- waives the kernel's "Already in {asset}" gate so a pair collector can
-- buy the other leg; this package is not a pair collector. Unless
-- `allow_both_sides` is 1, an entry is refused while the opposite token of
-- the same condition is already held.
--
-- Wire rules worth copying into your own strategy:
--   * prices/sizes cross as exact decimal STRINGS ("0.55"), never floats;
--   * price entries ONLY off books with `fresh == true` (the host's
--     freshness gate, §6.6) — a stale book is not a price;
--   * check `bk.round().time_left_sec` — an entry inside the force-exit
--     window would be exited on the next tick.

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

-- ── state (per state machine; the host runs one Lua VM per strategy) ────────
local last_mid = {}       -- token -> last mid seen (string)
local confirmed = {}      -- token -> true while the DOWN-leg premise holds
local peak_proceeds = {}  -- token -> best (bid − taker fee) seen while held

-- The tunable from manifest.json (`tunables.threshold.default`), overridable
-- by hot parameters (host config / shadow evolution) via `bk.params()`.
local function threshold()
    local raw = bk.params().threshold
    local t = tonumber(raw)
    if t == nil or t <= 0 then
        t = 0.04
    end
    return t
end

local function classify(token, mid)
    local prev = last_mid[token]
    last_mid[token] = mid
    if prev == nil then
        return -- first observation: nothing to compare yet
    end
    local pm = tonumber(prev)
    local cm = tonumber(mid)
    if pm == nil or cm == nil or pm <= 0 then
        return
    end
    local move_pct = (cm - pm) / pm * 100.0
    -- Direction gate: only a FALLING leg is ever buyable. An up-move — or
    -- any move smaller than `threshold` — clears the confirmation. There is
    -- no "up" state anymore: the up leg was deleted (live WR 7.5%, net
    -- −$40.37), so a premise that stops falling is simply not a trade.
    if move_pct >= 0 or math.abs(move_pct) < threshold() then
        confirmed[token] = nil
        return
    end
    confirmed[token] = true
end

-- ── exit economics (best bid, never the mid) ────────────────────────────────
--
-- `rate * (p*(1-p))^exponent` USD per share — the same expression
-- `FeeSchedule::fee_per_share` settles in. `nil` outside (0, 1): a corrupt
-- quote is not a sell.

local DEC_0 = { m = 0, s = 0 }
local DEC_100 = { m = 100, s = 0 }
local DEFAULT_TP = { m = 40, s = 0 }
local DEFAULT_ARM = { m = 20, s = 0 }
local DEFAULT_GIVE = { m = 15, s = 0 }
local DEFAULT_FEE = { m = 7, s = 2 } -- 0.07, the published crypto taker rate

local function fee_per_share(p, rate, exponent)
    local one_minus = dec_sub({ m = 1, s = 0 }, p)
    if p.m <= 0 or one_minus.m <= 0 then
        return nil
    end
    local base = dec_mul(p, one_minus)
    local acc = { m = 1, s = 0 }
    for _ = 1, exponent do
        acc = dec_mul(acc, base)
    end
    return dec_mul(rate, acc)
end

--- The fee the exit decision subtracts from the bid.
--- A positive `bk.fees()` schedule is what the kernel will actually charge
--- (replay `--fee-model`). PolySim publishes rate 0; a nil schedule is the
--- same fact. Either way the decision still haircuts by `exit_fee_rate`
--- (default 0.07, exponent 1) so a sim that charges nothing cannot call a
--- fee-eaten gain a profit. When the live schedule is larger, that larger
--- number wins — the sell has to clear the fee that will actually be taken.
local function decision_fee(bid)
    local params = bk.params()
    local official_rate = dec_param(params, "exit_fee_rate", DEFAULT_FEE)
    if official_rate.m < 0 then
        official_rate = DEFAULT_FEE
    end
    local official = fee_per_share(bid, official_rate, 1)
    local sched = nil
    local fees = bk.fees and bk.fees() or nil
    if type(fees) == "table" then
        local rate = dec_parse(fees.rate)
        local exp = fees.exponent
        if rate and rate.m > 0 and type(exp) == "number" and exp >= 1
            and exp == math.floor(exp) and exp <= 8 then
            sched = fee_per_share(bid, rate, exp)
        end
    end
    if official and sched then
        if dec_cmp(sched, official) > 0 then
            return sched
        end
        return official
    end
    return official or sched
end

--- `(proceeds − entry) / entry * 100 >= pct`, without a division.
local function net_reaches(proceeds, entry, pct)
    if entry.m <= 0 then
        return false
    end
    local lhs = dec_mul(dec_sub(proceeds, entry), DEC_100)
    local rhs = dec_mul(entry, pct)
    return dec_cmp(lhs, rhs) >= 0
end

--- Peak proceeds have given back at least `z` percentage points of net PnL.
local function gave_back(peak, cur, entry, z)
    if entry.m <= 0 then
        return false
    end
    local lhs = dec_mul(dec_sub(peak, cur), DEC_100)
    local rhs = dec_mul(entry, z)
    return dec_cmp(lhs, rhs) >= 0
end

local function held_index()
    local map = {}
    local hs = bk.holdings and bk.holdings() or {}
    if type(hs) ~= "table" then
        return map
    end
    for i = 1, #hs do
        local h = hs[i]
        if type(h) == "table" and type(h.token_id) == "string" then
            map[h.token_id] = h
        end
    end
    return map
end

local function forget_unheld(held)
    for tok in pairs(peak_proceeds) do
        if held[tok] == nil then
            peak_proceeds[tok] = nil
        end
    end
end

local function note_peak(token, proceeds)
    local prev = peak_proceeds[token]
    if prev == nil or dec_cmp(proceeds, prev) > 0 then
        peak_proceeds[token] = proceeds
    end
end

--- One exit suggestion for a held token, or nothing. The kernel prices the
--- sell off the fresh best bid (strategy-signal path); this function only
--- decides. No fresh bid → no exit, including the time rule: a remembered
--- mid is not a buyer.
local function consider_exit(out, row, round)
    local token = row.token_id
    local entry = dec_parse(row.entry_price)
    if not entry or entry.m <= 0 then
        return
    end
    local book = bk.book(token)
    if book == nil or book.fresh ~= true or book.best_bid == nil then
        return
    end
    local bid = dec_parse(book.best_bid)
    if not bid or bid.m <= 0 then
        return
    end
    local fee = decision_fee(bid)
    if not fee then
        return
    end
    local proceeds = dec_sub(bid, fee)
    note_peak(token, proceeds)
    local peak = peak_proceeds[token]
    local params = bk.params()

    local tp = dec_param(params, "take_profit_pct", DEFAULT_TP)
    if tp.m > 0 and net_reaches(proceeds, entry, tp) then
        out.exits[#out.exits + 1] = { token = token, reason = "take_profit" }
        return
    end

    -- Absolute bid target. Still has to clear the fee: a target the taker
    -- fee turns into a loss is not a take-profit (the time rule is the
    -- loss-capable exit).
    local px = dec_param(params, "take_profit_price", DEC_0)
    if px.m > 0 and dec_cmp(bid, px) >= 0 and net_reaches(proceeds, entry, DEC_0) then
        out.exits[#out.exits + 1] = { token = token, reason = "take_profit_price" }
        return
    end

    local arm = dec_param(params, "trail_arm_pct", DEFAULT_ARM)
    local give = dec_param(params, "trail_giveback_pct", DEFAULT_GIVE)
    if arm.m > 0 and give.m > 0 and peak ~= nil
        and net_reaches(peak, entry, arm)
        and gave_back(peak, proceeds, entry, give) then
        out.exits[#out.exits + 1] = { token = token, reason = "trailing_stop" }
        return
    end

    local tleft = int_param(params, "exit_time_left_sec", 0)
    if tleft > 0 and round.time_left_sec <= tleft then
        out.exits[#out.exits + 1] = { token = token, reason = "time_exit" }
    end
end

-- ── suggested entry points (§6.5) ───────────────────────────────────────────

-- Optional: observe every book tick. `update` is the §6.5 book row:
-- { symbol=, best_bid=, best_ask=, mid=, obi=, spread=, bid_depth=,
--   ask_depth=, ts_ms=, fresh= } (decimal fields are STRINGS).
function bk_on_book(update)
    if update.mid ~= nil then
        classify(update.symbol, update.mid)
    end
end

-- Optional: called when a round starts — reset per-round state here.
function bk_on_round(round)
    -- Per-token momentum state stays: a premise may carry across rounds for
    -- the same token id. Peak proceeds are position state and are dropped
    -- when the holding disappears (`forget_unheld`), not on the rollover.
end

-- REQUIRED. Return the intent tables; the kernel adjudicates everything.
--   entries = { { token=, price=, reason=, shares? } }   price: decimal STRING
--   exits   = { { token=, reason= } }                    kernel prices the exit
--   breaks  = { { token=, broken_price= } }              kernel cancels bids
--
-- Direction gate: the DOWN leg only, and only while IT is the leg whose
-- price is falling (`confirmed` is set by nothing else). The entry price is
-- the leg's own best_bid — a resting maker quote the crash crosses, never a
-- chase (the manifest's `entry_maker: true` makes every entry post-only and
-- never-escalating).
function bk_evaluate()
    local out = { entries = {}, exits = {}, breaks = {} }

    local round = bk.round()
    if round == nil then
        return out -- between rounds: nothing to trade
    end

    local held = held_index()
    forget_unheld(held)
    for _, row in pairs(held) do
        consider_exit(out, row, round)
    end

    -- Entries stay outside the force-exit window. Exits above do not: a
    -- winner can still be banked in the last minute, and the time rule is
    -- only meaningful there.
    if round.time_left_sec <= 60 then
        return out
    end

    local allow_both = int_param(bk.params(), "allow_both_sides", 0) == 1
    local exiting = {}
    for i = 1, #out.exits do
        exiting[out.exits[i].token] = true
    end

    for _, market in ipairs(bk.markets()) do
        local token = market.down_token
        -- A token we are already asking to sell is not re-bought this cycle.
        if confirmed[token] == true and not exiting[token] then
            -- The kernel's hold-to-settlement exemption would ADMIT the
            -- opposite leg ("Already in {asset}" waived). This strategy is
            -- not collecting a pair, so the second side is refused unless
            -- the operator sets allow_both_sides = 1.
            local opposite_held = market.up_token ~= nil and held[market.up_token] ~= nil
            if allow_both or not opposite_held then
                local book = bk.book(token)
                -- §6.6: price ONLY off a fresh, two-sided book.
                if book ~= nil and book.fresh == true and book.best_bid ~= nil then
                    out.entries[#out.entries + 1] = {
                        token = token,
                        price = book.best_bid,
                        reason = "lua momentum down",
                    }
                end
            end
        end
    end
    return out
end
