-- lua_momentum — the official Lua 5.4 example strategy (E30 / #336),
-- surgically narrowed to its one live-positive leg (2026-11-01 evidence).
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
-- A small momentum follower that shows the whole suggested surface:
--
--   bk_on_book(update)   observe mids, confirm the falling DOWN leg
--   bk_evaluate()        emit entries as INTENT tables
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
local confirmed = {}   -- token -> true while the DOWN-leg premise holds

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
    if round.time_left_sec <= 60 then
        return out -- inside the force-exit window: do not enter
    end

    for _, market in ipairs(bk.markets()) do
        local token = market.down_token
        if confirmed[token] == true then
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
    return out
end
