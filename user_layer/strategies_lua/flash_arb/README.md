# flash_arb — the spot-to-venue lag catcher

A 15-minute-round strategy that trades the seconds between Binance and
Polymarket: when the round asset's spot price moves >= `0.25%` inside
`mom_window_sec` (2s), the strategy buys the side the move implied — UP on a
pump, DOWN on a dump — as a **post_only maker at the current ask**, but only
while that ask is still `<= 0.55` (the venue has not repriced yet) and within
`lag_max_ms` (1.5s) of the triggering bar close. All tunables are listed in
`manifest.json`.

## The seal

Signal-only, like every strategy on this stack: no exit logic, no sizing, no
risk parameters. Entries cross as `{ token, price, reason }`; the kernel
adjudicates, sizes (`shares` is omitted), funds-gates and posts them
`post_only`. The kernel's exit ladder (hard stop, trailing, time exit,
settlement) and systemic breakers own survival.
`scripts/strategy-no-stop-loss-check.mjs` enforces this statically. There is
no resting order of this strategy's own, so it never emits `breaks`.

## Data notes (what the gates can actually see)

- **The trigger** reads CLOSED `sec1` bars keyed by ASSET (`BTC`/`ETH`/…),
  fed by the E29 engine aggregator from the same data path that reaches
  `on_data` — in replay, the corpus's `spot` lines. No spot data → no
  trigger → no entries: fail-closed by construction (there is no
  `spot_missing` knob; unlike mad_dog the spot feed is the signal, not a
  veto).
- **The 1.5s lag window runs on bar close times**, so its effective
  granularity is the sec1 cadence — a trigger is seen at its bar close and
  stays fresh for at most the next two closes. The spec's ">2s refuses"
  bound is subsumed by the 1.5s gate.
- **The lag gate prices the side actually paid** (`best_ask`, venue tick):
  a book whose mid still looks cheap but whose ask has already repriced is
  not lagging.
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
truncates the book to 3 levels per side and drops spot entirely: on it this
strategy can never fire (no spot → no trigger). The spot-bearing corpus
built by `scripts/mad-dog-spot-corpus.mjs` keeps spot lines and full book
depth; the delivery report's arms run on both, and the depth/`OBI` numbers
there are real depth, not the truncated lower bound. See
`docs/reports/flash-arb-delivery.md` for the arm-by-arm numbers.
