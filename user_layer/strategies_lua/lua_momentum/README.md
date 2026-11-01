# lua_momentum — the official Lua example strategy (E30 / #336), down-leg only

> **⚠️ EXAMPLE / SINGLE-EDGE PACKAGE.** The v2-corpus census after the #419
> locked-top fix (90h replay, official fees + 1 tick slippage, 10-share lot)
> measured the OLD both-legs taker shape at **20,038 closed trades / net
> −$17,575.78 / PF 0.43** (`docs/reports/evolution/RETEST-2026-10-10.md` §4).
> That verdict still describes everything this package no longer does. What
> the 2026-11-01 direction surgery kept is the one entry shape that priced
> positive in the unmanaged live window (2026-10-24→26), maker-ized per
> issue 742. It remains the sandbox/example teaching fixture the §6.3 gate
> pins (`lua-sandbox-check.mjs`).

A small momentum follower that demonstrates the Lua strategy surface inside
the 5.4 sandbox: observe books, confirm the falling DOWN leg, emit maker
entry INTENTS. It never places an order — every return value is adjudicated
by the kernel (entry gates, sizing, risk, signing, submission).

## What it does (after the direction surgery)

* `bk_on_book(update)` — tracks each token's mid; when a token's mid FALLS
  by at least `threshold` percent in one tick the DOWN-leg premise is
  confirmed. An up-move — or any smaller move — clears it. There is no
  "up" state anymore.
* `bk_evaluate()` — for every market whose DOWN token is confirmed falling,
  prices ONE LIMIT entry at that token's live best bid, but ONLY when:
  * the round exists and `time_left_sec > 60` (outside the force-exit
    window), and
  * the book is `fresh == true` (the host's freshness gate) with a bid side.

Everything else (risk, sizing, quotas, submission) belongs to the kernel.

## The direction gate — live evidence (2026-10-24→26, 86 fills, 0 maker)

| leg | fills | WR | net |
|:---|:---|:---|:---|
| DOWN leg — buy the DOWN token while its own price is crashing | 44 | 56.8% | **+$22.79** |
| UP leg — chase the UP token's spike | 40 | 7.5% | −$40.37 |

* The 0.20-0.35 entry band filled at WR 75% on the down leg; the 0.40+ fills
  on the up leg went 17-for-17 losing — chasing a spike buys the top and eats
  mean reversion.
* The up leg is DELETED, not disabled: there is no tunable, no param, no
  config that re-arms it. `bk_evaluate` iterates `market.down_token` only.
* Only the leg whose own price is falling is ever buyable — the confirmed
  premise IS a falling price, and any rebound clears it before the next
  entry.

## Maker entries (issue 742's `entry_maker` declaration)

The manifest declares `"entry_maker": true`, so the host builds every
`OrderRequest` for this strategy as `FillPolicy::Maker` — post-only — and
submits it with `maker_timeout_ms = -1`, the OME's never-escalate clock:

* the strategy quotes the leg's own **best_bid** (it never chases the ask),
  so the order is a resting maker quote, not a crossing one;
* in a live crash the falling price crosses the resting bid — the fill is a
  MAKER fill: no taker fee (taker fees had eaten **17.8%** of the down leg's
  gross profit; the curve peaks at 1.75%/side at p=0.5) plus the maker
  rebate;
* an unfilled bid simply rests to the round's end and dies there —
  **不成交 = 错过机会，不是亏损** (a missed chance, never a loss);
* the contrast with `smart_money_reverse` is the point: stabilizing first
  does NOT work (after an informed crash the book never retests), the edge
  is catching the knife WHILE it is still falling — the entry mechanism
  (price falls through the resting bid) and the edge (the crash continues)
  are the same fact.

## What was removed

* the up-token entry arm (`want = "up"`) — the WR 7.5% leg;
* the per-token `direction` state and the momentum-flip branch — dead once
  the gate landed (a premise that stops falling is simply not a trade);
* no `exits`/`breaks` logic was added: the exit layer is the kernel's
  (`holds_to_settlement` collection; the kernel's own exit config owns
  survival — the live window's 19 `trailing_stop` exits netted +$15.72, 15
  of them on the down leg, and all 3 `take_profit` exits were down-leg).

## The read-only surface it uses

| call | purpose here |
|:---|:---|
| `bk.round()` | round timing; skip entries inside the force-exit window |
| `bk.markets()` | the round's UP/DOWN token pairs (only `down_token` is read) |
| `bk.book(token)` | best bid to rest at, plus the `fresh` verdict |
| `bk.params()` | the `threshold` tunable (manifest default, hot-param overridable) |
| `bk.now_ms()` | available but not needed (the host timestamps everything) |

## Tunables

| name | default | min | max | meaning |
|:---|:---|:---|:---|:---|
| `threshold` | `0.04` | `0.005` | `1` | single-tick mid drop (%) that confirms the DOWN leg |

## Running it

The package lives in the default scan root, so a normal kernel boot finds it:

```sh
cargo run -p blitzkrieg-core -- --engine --readonly
# receipt: lua_momentum@0.1.0 (lua) registered into the engine dispatch (disabled)
```

Then enable it like any other strategy (`strategy.enable` over IPC, the UI
strategy panel, or `--enable-strategy lua_momentum`). To point the scanner
somewhere else: `--lua-strategy-dir <path>` (or `BK_LUA_STRATEGY_DIR`;
`--no-lua-strategy-dir` disables the scan).

Self-check (drives the real `strategy.lua` over a stub `bk` surface and
asserts the direction gate: the up leg never bids, the falling DOWN leg
bids at its best bid):

```sh
lua user_layer/strategies_lua/lua_momentum/selfcheck.lua
```

## Packaging rules (§6.4, enforced at load)

* a package is a DIRECTORY under the scan root containing `manifest.json`;
* directory name MUST equal `manifest.name` (mismatch → refuse);
* `manifest.sha256` MUST be the real SHA-256 of the entry file, recomputed
  and rewritten whenever you edit the script (mismatch → refuse, with both
  values in the error); `shasum -a 256 strategy.lua` prints it;
* `manifest.api` must be `"1.0"`; `entry` must stay inside the directory;
* `README.md` required (missing → warning, still loads).

## Sandbox rules (§6.2/§6.3, enforced by the host)

* allowed: `math`/`string`/`table` plus the base allow-list (`pairs`,
  `ipairs`, `type`, `tostring`, `tonumber`, `setmetatable`, `getmetatable`,
  `pcall`, `error`, `select`, `next`, raw access). NOT loaded: `os`, `io`,
  `debug`, `package`, `coroutine`; nil'd on top of that: `require`,
  `dofile`, `loadfile`, `load`, `loadstring`, `collectgarbage`,
  `string.dump`, `print`, `assert`, `xpcall`, `_G`.
* quotas: 16 MB of Lua memory and 1,000,000 VM instructions per host call.
  Breach either and the state is POISONED: every later call is refused by
  the host, one RISK_ALERT is raised naming this strategy, and the kernel
  keeps running. A plain Lua error (a bug) is NOT poison — the host logs it
  and keeps calling; fix the script and reload.
* decimal values cross as exact STRINGS (`"0.55"`), never floats.
