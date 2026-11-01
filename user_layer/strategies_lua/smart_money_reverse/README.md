# smart_money_reverse — the panic-dip buyer with an anti-knife door

A 15-minute-round strategy that buys the **crashed** leg of a binary round
once the panic is provably over, and (optionally, default OFF) fades a FOMO
ramp on the opposite leg. Not a direction predictor: it buys the footprint a
fast exit leaves behind, only after the footprint stops deepening and the bid
side visibly thickens.

## The two-scenario state machine (per token)

```
IDLE ──(drop from the crash_window_sec high ≥ crash_drop_pct)──▶ CRASH_ARMED
CRASH_ARMED ──(no new low for settle_quiet_sec AND
               bid_depth ≥ max(stabilize_depth_factor × crash-period
               thinnest depth, min_bid_depth))──▶ STABILIZED
STABILIZED ──(each evaluate)──▶ post-only maker bid @ best_bid + entry_offset
                                (default 0.00 = the touch; entry < ask is
                                structural, never a crossing bid)
STABILIZED ──(fresh low under the crash low)──▶ thesis dead: `breaks` row
                                cancels the resting bid, state re-arms from
                                the new low
HOLDING ──(mid > thesis_break_pct under rebound_anchor = the
           stabilization-time mid)──▶ ONE `exits` row
           { token, reason = "thesis_reversed: ..." } per round
```

Direction gate: only the leg that crashed is ever bought — never the rebound
leg (10-29 corpus verdict: chasing the up-move measured 9% WR). A
thesis-reversed round never re-enters.

Scenario B (fade FOMO, `fomo_enabled`, default `off`): a leg at/above
`fomo_min_price` that ramped ≥ `fomo_ramp_pct` inside `fomo_ramp_sec` arms a
resting bid on the **opposite** leg at `fomo_fade_price`; when the ramp dies
(mid back under `fomo_min_price`) the stale fade bid is cancelled via
`breaks`. It never fires on a leg scenario A is already bidding.

## The seal, and the one deliberate exception

Signal-only, like every strategy on this stack: no sizing, no risk
parameters, no exit **price**. Entries cross as `{ token, price, reason }`
with `shares` omitted — the kernel adjudicates, sizes, funds-gates and posts
them post_only. The strategy emits NO exit price anywhere; the exits wire
carries no price field. The one non-entry row is the **thesis verdict**: once
HOLDING, a mid more than `thesis_break_pct` under the rebound anchor emits
`{ token, reason = "thesis_reversed: ..." }` and the KERNEL prices and
executes it. The manifest declares `"trailing_exit": true`, so the kernel's
own trailing machinery is ON for this strategy and owns survival whenever
there is no verdict (including the case below where the strategy sees
nothing). `scripts/strategy-no-stop-loss-check.mjs` enforces the boundary
statically.

## Tunables (manifest.json; `bk.params()` reads them as strings)

| tunable | default | meaning |
|---|---|---|
| `crash_window_sec` | 60 | window searched for the pre-crash high |
| `crash_drop_pct` | 20 | relative drop from that high that arms the crash |
| `settle_quiet_sec` | 8 | no-new-low duration required before a bid |
| `stabilize_depth_factor` | 1.5 | required bid depth vs the crash period's thinnest observed depth |
| `min_bid_depth` | 500 | floor on the required bid depth |
| `entry_offset` | 0.00 | maker bid = best_bid + this; emission refused if it would cross the ask |
| `thesis_break_pct` | 3 | holding mid this far under the anchor emits the thesis_reversed verdict |
| `min_time_left_sec` | 180 | entry gate on `round.time_left_sec` |
| `spot_max_move_pct` | 0.5 | underlying move over `spot_window_sec` that vetoes an against-the-asset buy |
| `spot_window_sec` | 10 | window of CLOSED sec1 spot bars the veto reads |
| `spot_missing` | block | no spot data = no against-the-asset buy (`pass` for book-only diagnostics) |
| `fomo_enabled` | off | scenario B master switch (`on` to enable) |
| `fomo_min_price` | 0.85 | a ramp only arms at/above this mid |
| `fomo_ramp_pct` | 15 | required gain over the `fomo_ramp_sec` window low |
| `fomo_ramp_sec` | 30 | ramp detection window |
| `fomo_fade_price` | 0.10 | the resting fade bid price on the opposite leg |

## Ceilings (what this strategy cannot know)

- **No whale data source exists.** The `bk.*` surface carries no on-chain
  whale feed — "smart money" is observed only through its market footprint.
  The crash/ramp itself is the proxy trigger, and the strategy cannot
  distinguish a whale dumping from a market maker delisting. It prices only
  the footprint, and says so instead of pretending otherwise.
- **The 6meteor evidence behind scenario B's default.** The traced whale
  bought 0.24 → 0.98 and went 9/9 — chasing ramps wins only at the whale's
  size. Fading the ramp is the inverse bet and ships OFF; corpus evidence
  that flips the verdict is the only thing that should turn it on.
- **Kernel exits semantics.** The `exits` row is a reason, not a price: the
  kernel prices the close. With `trailing_exit: true` the kernel's trailing
  ladder is enabled for this strategy independent of the verdicts; an exit
  row for a token the strategy does not hold is a no-op. When
  `bk.holdings()` answers an empty table (the host pushes this strategy's
  positions each evaluate), the verdict watch simply has nothing to watch.
- **Spot gate data.** The underlying-asset veto reads CLOSED sec1 bars keyed
  by ASSET, fed by the E29 engine aggregator from the Binance spot stream.
  Frozen replay corpora are book+round only: there the gate sees no data and
  `spot_missing = block` refuses the entry (fail closed). A zero-trade
  replay on those windows is a statement about the corpus.
- **Book staleness.** Entries price only off `fresh == true` books; a
  resting bid whose leg's feed dies is cancelled by the kernel's own
  staleness handling, not by this strategy.
