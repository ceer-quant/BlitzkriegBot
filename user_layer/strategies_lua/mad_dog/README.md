# mad_dog — the panic-wick catcher

A 15-minute-round strategy that buys panic, not direction: after one side has
held the round (`mid >= 0.55`, by a 20s hold or a 60s rolling mean), a FAST
wick below `0.35` (>= 15% drop inside 3s) arms a resting **maker** bid
`0.02` under the wick low. The bid fills only if the panic continues into it;
if the price recovers above `0.35` first, the strategy cancels the stale bid
(a `break`). All six entry conditions, the tunable defaults and their
meanings are listed in `manifest.json`.

## The seal

Signal-only, like every strategy on this stack: no exit logic, no sizing, no
risk parameters. Entries cross as `{ token, price, reason }`; the kernel
adjudicates, sizes (`shares` is omitted), funds-gates and posts them
`post_only`. The kernel's exit ladder (stop-loss, trailing, settlement) and
systemic breakers own survival. `scripts/strategy-no-stop-loss-check.mjs`
enforces this statically.

## Data notes (what the gates can actually see)

- **Underlying momentum (condition 3)** reads CLOSED `sec1` bars keyed by
  ASSET (`BTC`/`ETH`/…), fed by the E29 engine aggregator from the Binance
  spot stream. The frozen replay corpora are book+round only, so in those
  replays this gate sees no data; `spot_missing` (default `block`) decides
  that case. `pass` exists for book-only diagnostics.
- **`bk.account()`** is currently populated by no host path, so the
  available-balance gate applies only when the view is present; the kernel's
  funds path is the real gate. Same for same-direction-position and cooldown
  checks: kernel-owned (E25 arbitration / E26 breakers).
- **`kline_stream` is deliberately not declared** in `modes`: the polymarket
  plugin seam declares `websocket_feed | level2_snapshot | post_only`, and
  the kline feed is engine-side (E29), not a plugin capability. Declaring it
  would refuse the strategy at enable time for a bit the seam does not
  carry.

## Replay caveat

The committed 4-window corpus (`docs/reports/data/mean-reversion-gate`)
truncates the book to 3 levels per side and drops spot entirely. The
`min_bid_depth = 500` and `max_abs_obi` gates therefore read a LOWER BOUND of
true depth there, and the spot gate is data-less. A zero-trade replay on
those windows is a statement about the corpus, not proof the gates never
fire; see the mad_dog delivery report for the arm-by-arm numbers.
