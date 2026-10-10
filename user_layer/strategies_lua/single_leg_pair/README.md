# single_leg_pair

The single-leg round participant for binary rounds — issue #387: 55.1% of
the @almach wallet's 15m rounds (5,391 of 9,780) had only ONE leg worth
lifting, structurally outside pair_discount_arb's both-legs action space.
This package enters those rounds as a NAKED directional position with its
own stop and its own cap.

## Mechanism

A round exposes its verdict in the book. Two shapes fire:

- **Sole leg** — one side still has a liftable ask (`> 0`) while the OTHER
  side's present book has none (`best_ask == "0"` or nil): the round is
  single-leg BY THE BOOK; buy the liftable leg at its ask. A wholly absent
  opposite book is an outage and refuses — the strategy never trades a
  one-book round.
- **Leader** — both legs liftable: the strictly higher ask is the book's
  directional verdict; fire when the leader's ask prices in the band and
  the raw gap is at least `min_gap` wide. Equal asks name no leader; a
  thinner gap is a pair-shaped book, not a verdict. (The 15m @almach corpus
  never prints a one-sided book — the converter rebuilds both legs from the
  same 1-min series — so on that replay the leader path is the live one;
  the sole path covers true one-sided books, the 55.1% ledger phenomenon.)

The risk boundary is priced from the wallet's own single-leg settlement
distribution (2026-08-05..09-30, by VWAP actually paid):

| band | rounds | win rate | EV/share |
|---|---|---|---|
| <0.10 | 398 | 4.5% | −0.046 |
| 0.10–0.30 | 1,128 | ~10–23% | −0.05…−0.13 |
| 0.40–0.50 | 385 | 53.5% | +0.022 |
| **0.60–0.70** | 489 | **81.8%** | **+0.198** |
| **0.70–0.80** | 670 | **92.5%** | **+0.175** |
| 0.80–0.90 | 670 | 96.0% | +0.120 |
| >=0.90 | 374 | 99.2% | +0.065 |

A naked leg bought cheap is a lottery ticket, not a discount. The default
band [0.55, 0.88] is the 2026-10-10 post-#419 replay re-calibration
(re-census corpus, 7.75 days of 15m rounds): the 0.55–0.60 slice (+$35.06
across 1,907 trades) and the 0.85–0.88 slice (+$3.90 across 8) are both
net-positive margins ON THIS KERNEL, while the 0.50–0.55 slice is a
measured NEGATIVE margin (−$2.17 across 85) and stays refused — the
coin-flip dead band claim survived the kernel fix; the sub-0.50 lottery
zone refuses silently (re-armable if the book reprices). The ledger's own
odds table above stays the structural reason the band's floor is not
lower; the replay margins are the per-kernel confirmation. The timing
hypothesis from the @almach maker fill structure (enter early, the U-shape)
was tested and FALSIFIED on this taker corpus: `min_time_left_sec` 300
(−$0.84) and 750 (−$36.03) both lose money against the shipped 45 — our
89.8% of approved entries already land in the round's first 150s.

## How it trades

- **Entry** — one candidate per condition, priced AT the leg's own ask,
  shares sized to that leg's visible ask depth (floored to the 0.01 grid);
  the kernel still caps at its own `max_shares` and judges every gate.
- **The naked stop** — from `stop_deadline_sec` (default 25) seconds-left to
  the last tick, every evaluation emits a `single-leg stop` exit intent per
  entered leg. The kernel routes it as `StrategySignal`: priced at the book,
  risk-checked, deduped, and executed even though the manifest declares
  `holds_to_settlement` (holder filtering applies to the kernel's AUTOMATED
  ladder, never to explicit strategy intents). An intent with no position
  behind it is dropped. Riding a naked leg into settlement hoping is not
  the design: at the deadline the leg leaves at the book, or it was already
  gone (settled leg → intent dropped, the intent block retires with the
  round).
- **The exposure cap** — `max_open_positions` (default 10) caps the
  strategy's own openings per round slot; positions settle or stop within
  their round, so the cap is a true concurrent-exposure ceiling. The
  2026-10-10 replay found the shipped 3 was BINDING: lifting it to the
  domain ceiling 10 is worth +$72.79 across 2,242 extra closed trades
  (band-cap10 arm, $259.77 vs $172.52 baseline).
- **Nothing is waived** — `gate_exemptions` is empty: the kernel's timing
  gate (with the engine's `--min-time-left 0` in replays), the momentum
  gate, and every risk gate apply at full strength. A directional bet is
  not market-neutral; it does not inherit the pair's exemptions.
- **holds_to_settlement** — declared, and its ONE consequence is that the
  kernel's generic exit ladder (the 12% backstop, the maker ladder) leaves
  the legs alone: the stop above is this strategy's own exit policy, not
  the ladder's. Settlement semantics are untouched — the winner redeems
  $1.00/share, the loser zero, and a leg without REDEEM evidence is
  honestly held (fail-closed, the #387 constraint). The strategy NEVER
  emits `reason = "merge"` — it holds no complete pair and never pretends to.
- **Fees** — entries require the kernel's ONE fee schedule in force
  (`bk.fees()` non-nil and sane). The band, not a cost cap, is the edge,
  but a kernel with no schedule is a kernel whose cost regime is unknown —
  fail-closed: no schedule, no entries. Stops are exits and run regardless.

## Tunables

| name | default | meaning |
|---|---|---|
| `min_entry_price` | `0.55` | band floor (0.55–0.60 is a positive replay margin; 0.50–0.55 is a measured NEGATIVE one) |
| `max_entry_price` | `0.88` | band ceiling (0.85–0.88 adds a thin but positive margin on the post-#419 kernel) |
| `dead_band_floor` | `0.50` | coin-flip zone start — one considered look, then latched |
| `min_gap` | `0.10` | leader verdict requires at least this raw price gap |
| `min_time_left_sec` | `45` | entry time floor (stops run past it); 2026-10-10 A/B: raising it LOSES money — timing is already early |
| `stop_deadline_sec` | `25` | seconds-left where the naked stop arms |
| `max_open_positions` | `10` | strategy openings per round slot (the shipped 3 was binding: +$72.79 at 10) |
| `min_shares` | `1` | smallest liftable size worth an order |

## Calibration record (2026-10-10, post-#419 kernel `f678296c`, 15m-act corpus)

Serial replay A/B, one arm per row, `--fee-model official --slippage-ticks 1`,
kernel timing gates zeroed to match the re-census recipe
(`--round-sec 900 --min-round-age 0 --min-time-left 0`):

| arm | closed | WR% | PF | net $ | Δ vs baseline |
|---|---|---|---|---|---|
| baseline (shipped: 0.60–0.85, cap 3) | 3,681 | 70.96 | 1.2438 | 172.52 | — |
| `min_time_left_sec=300` | 3,678 | 70.94 | 1.2426 | 171.69 | −0.84 |
| `min_time_left_sec=750` | 3,342 | 70.08 | 1.2086 | 136.50 | −36.03 |
| `min_entry_price=0.55` (adds 0.55–0.60) | 5,588 | 65.62 | 1.1754 | 207.59 | +35.06 |
| `min_entry_price=0.50` (adds 0.50–0.60) | 5,673 | 65.24 | 1.1698 | 205.41 | +32.89 |
| `max_entry_price=0.88` (adds 0.85–0.88) | 3,689 | 70.99 | 1.2512 | 176.42 | +3.90 |
| band 0.55–0.88 | 5,590 | 65.69 | 1.1819 | 213.87 | +41.35 |
| `max_open_positions=10` | 3,758 | 71.15 | 1.2505 | 180.05 | +7.53 |
| **band 0.55–0.88 + cap 10 (ADOPTED)** | 6,031 | 66.42 | 1.2090 | **259.77** | **+87.25** |

Marginal anatomy: the 0.50–0.55 slice alone is NEGATIVE (−$2.17 across 85
trades — the dead band survives the kernel fix), which is why the floor is
0.55 and not 0.50. Overfitting guard — the corpus split chronologically
into a 60.5% train / 39.5% val half: the adopted defaults move train
+$81.29 (+60%) and val +$5.96 (+16%), same direction on both halves, but
val PF degrades 1.125 → 1.081 and val WR −5.0pp (more volume, thinner
quality). Honest hold: BTC-up-only filtering was evaluated and REJECTED —
it is worth only $0.81 on the adopted arm and requires a .lua change;
the @almach maker-structure hypotheses (early-entry U-shape, skewed pair
sizing, insurance leg) are maker-side phenomena that do not transfer to
this taker/ask-lift corpus.

## Data notes

- Prices run on scaled integers (the §6.5 decimal-STRING wire rule); every
  trigger comparison is an exact integer comparison.
- One ATTEMPT per condition per round (`armed` latch): a rejected attempt is
  not retried into the same book. When pair_discount_arb runs alongside, its
  two-leg entries win the same-token race and this package's attempt on that
  condition is refused by the kernel's own-position gate — benign by design.
- The trigger reads no klines: the 15m corpus carries no spot events, so
  the book's own shape is the whole signal.
- Both books must be FRESH. A stale book is an outage, not a signal.
- Round rollover retires every latch: conditions do not survive their round.

## Self-check

`selfcheck.lua` runs the strategy through the positive, negative and
boundary rows on a stub `bk` surface (pure Lua 5.4, no dependencies):

```sh
lua5.4 user_layer/strategies_lua/single_leg_pair/selfcheck.lua   # or: lua
```

Exit 0 = all rows hold. The kernel-side acceptance is the replay A/B:
`scripts/single-leg-ab.mjs` runs the off/on pair through the same binary,
the same corpus and the same cwd, then overlays both reports against the
@almach ledger (coverage / ROI / PF / closed + the single-leg odds table).
