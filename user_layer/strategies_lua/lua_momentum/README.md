# lua_momentum — the official Lua example strategy (E30 / #336)

> **⚠️ Example package. Not a measured production edge.** A settlement-truth
> replay of the parameters in this package (BTC/ETH/SOL/XRP, 5m and 15m,
> 2026-11-05 through 2026-11-12, official 0/1 outcomes, taker fee
> `shares * 0.07 * p * (1-p)`, one-cent spread, no same-round re-entry)
> did not reach win rate ≥ 80% and profit factor ≥ 1.2 on the held-out
> window. A later window that study had not touched (2026-10-12 through
> 2026-10-26) says the same thing, and the maker rule there is about a 26%
> win rate and a 0.34 profit factor, not ~75% / >1.1. The closest taker
> test (buy an ask of 0.80–0.90 in the last 45–120 seconds, then hold) was
> 87.7% and 1.18 out of sample, short of 1.2, so it was not adopted.
> Defaults were not retuned. Tables:
> `docs/reports/lua-momentum-oos-2026-11-12.md`,
> `docs/reports/lua-momentum-oos-2026-10.md`.
> The v2-corpus census after the #419 locked-top fix still stands for the
> old both-legs taker shape: **20,038 closed / net −$17,575.78 / PF 0.43**
> (`docs/reports/evolution/RETEST-2026-10-10.md` §4).

A momentum follower that demonstrates the Lua strategy surface inside the
5.4 sandbox. It never places an order — every return value is adjudicated
by the kernel (entry gates, sizing, risk, signing, submission). Both legs
are live: a rising leg buys the UP token, a falling leg buys the DOWN token,
each only while its own mid has moved by at least `threshold`. Entries rest
post-only at that leg's best bid. After a sell, the condition is not
re-entered in the same round (`max_entries_per_round` defaults to 1).

## What it does

* `bk_on_book(update)` — tracks each token's mid. A move of at least
  `threshold` (a fraction, default 0.04 = 4%) arms that token: up if the mid
  rose, down if it fell. A smaller move clears the arm. The comparison is
  against the fractional move. An earlier version multiplied the move by
  100 and then compared it with 0.04, which armed on a 0.04% flicker.
* `bk_evaluate()` — buys the UP token only while it is armed up, and the
  DOWN token only while it is armed down. The limit price is that token's
  live best bid, and only when:
  * the round exists and `time_left_sec` is outside the entry cutoff
    (60 seconds, unless `entry_cutoff_pct` says otherwise),
  * `min_time_left_for_entry` (default 120s) and `entry_open_after_pct`
    (default 20) both allow a new entry,
  * the book is `fresh == true` with a bid, and the bid is inside
    `[min_entry_price, max_entry_price]` (defaults 0.15 and 0.50),
  * the opposite token of the same condition is not already held, unless
    `allow_both_sides` is `1`,
  * this condition has not already used its `max_entries_per_round` slot
    (default 1 — no re-entry after a sell).
* The same call emits **exits** for positions `bk.holdings()` reports. A
  sell is judged on the fresh **best bid** after a taker fee, never the
  mid. PolySim's schedule is 0; the decision still haircuts by
  `exit_fee_rate * p * (1-p)` (default `0.07`) so a fee-eaten gain is not
  called a profit. A positive `bk.fees()` schedule is used when it is
  larger, because that is what the kernel will actually charge.

Everything else (risk, sizing, quotas, submission) belongs to the kernel.
A position that hits none of the exit rules still holds to settlement.
`trailing_exit` stays false: these exits are strategy signals, which the
kernel honours even for a holder.

## Maker entries (issue 742's `entry_maker` declaration)

The manifest declares `"entry_maker": true`, so the host builds every
`OrderRequest` for this strategy as `FillPolicy::Maker` — post-only — and
submits it with `maker_timeout_ms = -1`, the OME's never-escalate clock:

* the strategy quotes the leg's own **best_bid** (it never crosses the ask);
* a fill happens only if a later trade reaches that bid. An unfilled bid
  rests until the round ends — a missed chance, never a loss;
* the 2026-11-12 study found that this fill selects tokens that keep
  moving against the position. Settlement win rates sat 10 to 17 points
  below the entry price in every entry-price bucket. That is why tightening
  the take-profit did not produce an 80% win rate.

## The read-only surface it uses

| call | purpose here |
|:---|:---|
| `bk.round()` | round timing; skip entries inside the force-exit window |
| `bk.markets()` | the round's UP/DOWN token pairs (both legs are read) |
| `bk.book(token)` | best bid to rest at, plus the `fresh` verdict |
| `bk.params()` | tunables (manifest defaults, hot-param overridable) |
| `bk.holdings()` | open positions the exit rules and the both-sides guard read |
| `bk.fees()` | the kernel's taker schedule; a zero/nil schedule falls back to `exit_fee_rate` |
| `bk.now_ms()` | the re-entry cooldown clock |

## Tunables

| name | default | min | max | meaning |
|:---|:---|:---|:---|:---|
| `threshold` | `0.04` | `0.005` | `1` | single-tick mid move (fraction, so 0.04 = 4%) that arms the leg |
| `take_profit_pct` | `40` | `0` | `500` | sell when net PnL on the best bid, after the taker fee, reaches this many percent. `0` disables |
| `take_profit_pct_tight` | `0` | `0` | `500` | the band's value at the END of the round: the band slides LINEARLY from `take_profit_pct` at round start to this as the clock drains (time-weighted tightening, 2026-11-06). `0` keeps the flat band exactly |
| `take_profit_price` | `0` | `0` | `0.99` | sell when the best bid reaches this price AND the fee-net proceeds still beat the entry. `0` disables |
| `trail_arm_pct` | `22` | `0` | `500` | arm the trailing stop once peak net PnL reaches this. `0` disables the trail |
| `trail_arm_pct_tight` | `0` | `0` | `500` | `trail_arm_pct`'s end-of-round value (linear slide, same clock). `0` keeps the flat band |
| `trail_giveback_pct` | `15` | `0` | `500` | once armed, sell when the best bid has fallen this many percentage points from the high water mark bid (the same 口径 the kernel measures 让回). `0` disables |
| `trail_giveback_pct_tight` | `0` | `0` | `500` | `trail_giveback_pct`'s end-of-round value (linear slide, same clock). `0` keeps the flat band |
| `exit_time_left_sec` | `0` | `0` | `900` | sell whatever is held once this many seconds remain, even at a loss. `0` disables (hold to settlement) |
| `allow_both_sides` | `0` | `0` | `1` | `1` permits an entry while the opposite token of the same condition is held. Default refuses it |
| `max_entries_per_round` | `1` | `0` | `20` | position attempts per condition in one round. The unfilled bid may be re-quoted; after an exit, a new entry needs another slot. `1` means no re-entry. `0` quotes nothing |
| `reentry_cooldown_sec` | `0` | `0` | `900` | after an exit, wait this long before a remaining slot may be used. `0` waits nothing |
| `exit_fee_rate` | `0.07` | `0` | `1` | realistic taker fee per share, `rate * p * (1-p)`, used when `bk.fees()` is nil or zero |

Set `take_profit_pct`, `take_profit_price`, `trail_arm_pct`, `trail_giveback_pct` and `exit_time_left_sec` to `0` to restore the previous hold-to-settlement behaviour.

## Running it

The package lives in the default scan root, so a normal kernel boot finds it:

```sh
cargo run -p blitzkrieg-core -- --engine --readonly
# receipt: lua_momentum@0.2.1 (lua) registered into the engine dispatch (disabled)
```

Then enable it like any other strategy (`strategy.enable` over IPC, the UI
strategy panel, or `--enable-strategy lua_momentum`). To point the scanner
somewhere else: `--lua-strategy-dir <path>` (or `BK_LUA_STRATEGY_DIR`;
`--no-lua-strategy-dir` disables the scan).

Self-check (drives the real `strategy.lua` over a stub `bk` surface and
asserts the direction gate, the bid-not-mid exits, and the both-sides guard):

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
