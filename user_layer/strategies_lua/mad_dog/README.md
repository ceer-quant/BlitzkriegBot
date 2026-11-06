# mad_dog — the panic-wick catcher

> **FROZEN pending issue #847 positioning verdict** — the spot waterfall veto
> is fixed (exact-rational), the thesis-death exit is wired (issue #847:
> a HELD leg whose mid falls more than `thesis_break_pct` under its entry
> anchor emits ONE kernel-priced `thesis_reversed` row — the probe2 shape,
> MFE +45.8% rode bare to −11.59%) and the maker-TP exit is wired (issue
> #915 convergence: a HELD leg whose fresh best bid reaches
> `maker_tp_gain_pct` above the anchor — the counterfactual's 1.05 × entry
> line — emits ONE `{ use_maker, price }` row resting post-only one tick
> above the bid), but the structural flaw stands: the extreme-0.02 resting
> bid is pure adverse selection (153 episodes / 1 fill / 0.65%) and the
> post-breakdown edge is zero (0/1,928 settlements); defect 1's ENTRY side
> stays closed under #847/#912/#914. Do not enable without repositioning.

A 15-minute-round strategy that buys panic, not direction: after one side has
held the round (`mid >= 0.55`, by a 20s hold or a 60s rolling mean), a FAST
wick below `0.35` (>= 15% drop inside 3s) arms a resting **maker** bid
`0.02` under the wick low. The bid fills only if the panic continues into it;
if the price recovers above `0.35` first, the strategy cancels the stale bid
(a `break`). All six entry conditions, the tunable defaults and their
meanings are listed in `manifest.json`.

## The seal

Signal-only, like every strategy on this stack: no stop-loss logic, no
sizing, no risk parameters. Entries cross as `{ token, price, reason }`; the
kernel adjudicates, sizes (`shares` is omitted), funds-gates and posts them
`post_only`. Exit rows (at most ONE per ROUND per held token, whichever
watch fires first, never time-gated):
- the thesis-death verdict — a HELD leg more than `thesis_break_pct` under
  its entry anchor → a price-less `{ token, reason }` row the kernel prices
  and executes as a taker sweep;
- the maker-TP graduation (#915) — a HELD leg whose fresh best bid reaches
  `maker_tp_gain_pct` above its anchor → a
  `{ token, reason: take_profit, use_maker, price }` row resting post-only
  one tick above the bid. The kernel still gates it (profit-side reason +
  `strategy_exit_maker_first` + PnL ≥ 0 + the price post-only-safe against
  the live book; an unsafe/absent book falls back to kernel pricing).
The kernel's exit ladder (stop-loss, trailing, settlement) and systemic
breakers stay the real survival layer.
`scripts/strategy-no-stop-loss-check.mjs` enforces this statically.

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
