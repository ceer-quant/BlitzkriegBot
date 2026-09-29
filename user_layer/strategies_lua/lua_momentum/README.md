# lua_momentum — the official Lua example strategy (E30 / #336)

A small momentum follower that demonstrates the full Lua strategy surface
inside the 5.4 sandbox: observe books, classify per-token momentum, emit
entry/break INTENTS. It never places an order — every return value is
adjudicated by the kernel (entry gates, sizing, risk, signing, submission).

## What it does

* `bk_on_book(update)` — tracks each token's mid; when a token moves more
  than `threshold` percent in one tick it is marked confirmed in that
  direction (a direction flip while confirmed becomes a break).
* `bk_evaluate()` — for every confirmed token whose direction still holds,
  prices a LIMIT entry at the token's live best bid, but ONLY when:
  * the round exists and `time_left_sec > 60` (outside the force-exit
    window), and
  * the book is `fresh == true` (the host's freshness gate) with a bid side.

Everything else (risk, sizing, quotas, submission) belongs to the kernel.

## The read-only surface it uses

| call | purpose here |
|:---|:---|
| `bk.round()` | round timing; skip entries inside the force-exit window |
| `bk.markets()` | the round's UP/DOWN token pairs |
| `bk.book(token)` | best bid to price at, plus the `fresh` verdict |
| `bk.params()` | the `threshold` tunable (manifest default, hot-param overridable) |
| `bk.now_ms()` | available but not needed (the host timestamps everything) |

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
