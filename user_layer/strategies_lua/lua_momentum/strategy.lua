-- lua_momentum — the official Lua 5.4 example strategy (E30 / #336).
--
-- ⚠️ EXAMPLE ONLY, NOT FOR PRODUCTION: the v2-corpus census after #419
-- (docs/reports/evolution/RETEST-2026-10-10.md §4) measured 20,038 closed
-- trades at net −$17,575.78 — single-tick confirmation + naked legs to
-- settlement. Kept as the sandbox teaching fixture; do not enable.
--
-- A small momentum follower that shows the whole suggested surface:
--
--   bk_on_book(update)   observe mids, classify per-token momentum
--   bk_evaluate()        emit entries/breaks as INTENT tables
--
-- It never places an order: every return value is adjudicated by the kernel
-- (gates, sizing, risk, signing, submission). Everything it reads comes from
-- the read-only `bk.*` surface; everything it computes stays in this file.
-- It uses ONLY the sandboxed allow-list (math/string/table + the base
-- allow-list) — `require`, `os`, `io`, `debug` do not exist in here.
--
-- Wire rules worth copying into your own strategy:
--   * prices/sizes cross as exact decimal STRINGS ("0.55"), never floats;
--   * price entries ONLY off books with `fresh == true` (the host's
--     freshness gate, §6.6) — a stale book is not a price;
--   * check `bk.round().time_left_sec` — an entry inside the force-exit
--     window would be exited on the next tick.

-- ── state (per state machine; the host runs one Lua VM per strategy) ────────
local last_mid = {}    -- token -> last mid seen (string)
local direction = {}   -- token -> "up" | "down"
local confirmed = {}   -- token -> true while momentum holds

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
    if math.abs(move_pct) < threshold() then
        confirmed[token] = nil
        return
    end
    local dir = move_pct > 0 and "up" or "down"
    if confirmed[token] == true and direction[token] ~= dir then
        -- Momentum flipped: the old entry premise is broken. bk_evaluate
        -- turns this into a `breaks` row and the kernel cancels the bids.
        direction[token] = dir
        return
    end
    direction[token] = dir
    confirmed[token] = true
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
    -- Per-token state stays: momentum may carry across rounds for the same
    -- token id; nothing here is position state, so no reset is needed.
end

-- REQUIRED. Return the intent tables; the kernel adjudicates everything.
--   entries = { { token=, price=, reason=, shares? } }   price: decimal STRING
--   exits   = { { token=, reason= } }                    kernel prices the exit
--   breaks  = { { token=, broken_price= } }              kernel cancels bids
function bk_evaluate()
    local out = { entries = {}, exits = {}, breaks = {} }

    local round = bk.round()
    if round == nil then
        return out -- between rounds: nothing to trade
    end
    if round.time_left_sec <= 60 then
        return out -- inside the force-exit window: do not enter
    end

    for _, market in ipairs(bk.markets()) do
        local legs = {
            { token = market.up_token,   want = "up" },
            { token = market.down_token, want = "down" },
        }
        for _, leg in ipairs(legs) do
            if confirmed[leg.token] == true and direction[leg.token] == leg.want then
                local book = bk.book(leg.token)
                -- §6.6: price ONLY off a fresh, two-sided book.
                if book ~= nil and book.fresh == true and book.best_bid ~= nil then
                    out.entries[#out.entries + 1] = {
                        token = leg.token,
                        price = book.best_bid,
                        reason = "lua momentum " .. leg.want,
                    }
                end
            end
        end
    end
    return out
end
