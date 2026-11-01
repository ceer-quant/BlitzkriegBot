# pair_discount_arb

The pair-discount collector for binary rounds — the @almach mechanism
(85,431 fills, zero sells, 12,570 merges) as a Lua strategy package.

## Mechanism

A complete UP+DOWN share-pair of one condition is worth exactly **$1.00
on-chain** (MERGE into collateral). When

```
ask_UP + ask_DOWN + entry_fees(both legs) < max_pair_cost   (default 0.985)
```

the pair is a riskless discount: buy BOTH legs with EQUAL declared share
counts, then collect. The profit is locked at entry; the exit is a
collection, not a decision.

## How it trades

- **Entry (v1.3 fill path)** — the manifest declares `entry_maker`: every
  entry rests post-only and NEVER escalates to taker (a taker-filled pair
  pays ~3.5% round-trip on a ~2.5% discount — the edge dies in fees; maker
  legs pay no taker fee). v1.3 quotes each leg where a post-only buy can
  actually **rest**: the kernel refuses a post-only BUY whose limit reaches
  the live ask at POST time (`#464`, `venue.WouldCross`), so v1.2's
  at-ask quote was the one price a maker buy can never hold — the 10/15
  replay filled 0/N with 4 orders / 4 `venue.WouldCross` rejections.
  `entry_quote_mode` picks the depth (default 1 = ask − `entry_reprice_ticks`):
  the falling-ask waterfall has to step onto the quote for the leg to fill,
  and a maker fill prints at the quote — at or below it, never worse.
  `shares` are declared so the legs match in share count — a pair merges per
  share-pair. Shares size to the smaller leg's visible ask depth, floored to
  the 0.01 grid; the kernel still caps at its own `max_shares`.
- **The evidence for the depth** — @almach's own fills, booked against the
  archive (scripts/pair-fill-almach-fill-path.mjs, 8 pair legs the archive
  can book): 4 maker prints 1–3 ticks BELOW the then-ask (resting bids the
  falling ask descended into), 4 taker prints AT the ask, 0 above it. The
  default (ask − 1 tick) is the most aggressive RESTABLE quote; `mid`/`bid`
  are the passive arms.
- **Collection** — once a condition's pair attempt is armed, every later
  evaluation emits `reason = "merge"` exit intents for BOTH legs until the
  round ends. The kernel intercepts `merge` intents BEFORE the sell ladder:
  complete pairs burn into $1.00/pair (`PositionManager::merge_condition` +
  `Ledger::credit_merge`), unpaired remainders stay open for settlement, and
  an intent with no pair behind it is dropped. The strategy never sells.
- **Gap fill (v1.3 pricing)** — a partially-filled leg is topped up with the
  exact shortfall (counterpart holding − held), quoted by the SAME
  `entry_quote_mode` policy: an at-ask top-up is refused at POST exactly
  like an at-ask entry. The incremental test is unchanged — `ask + fee` vs
  `max_top_up_cost` on the ASK (the ceiling); the fill prints at the quote
  (cheaper).
- **Fees** — priced from `bk.fees()`, the kernel's ONE schedule
  (`rate * (p*(1-p))^exponent` per share). No schedule in force → NO entries:
  the fee is a cost parameter of the same order as the edge; guessing zero
  would fabricate profit. Fail-closed by construction.
- **Gates** — the manifest declares `holds_to_settlement` (collection
  semantics: the second leg passes the pair-completion entry exemption, the
  exit ladder leaves the legs alone in dry/read-only) and waives timing +
  momentum (a market-neutral pair does not care which way spot leans, or when
  in the round the discount appears; the declared `timing_min_time_left_sec:
  30` still bounds the timing waiver at D-31).

## Tunables

| name | default | meaning |
|---|---|---|
| `max_pair_cost` | `0.985` | fire below this all-in per-pair cost (tested on the asks) |
| `min_leg_price` | `0.01` | refuse legs quoted below (corrupt/degenerate books) |
| `max_leg_price` | `0.99` | refuse legs quoted above |
| `min_pair_shares` | `1` | smallest mergeable pair size worth an order |
| `min_time_left_sec` | `30` | entry time floor (collection runs to the last tick) |
| `max_top_up_cost` | `0.995` | gap-fill ceiling: ask + fee per incremental share vs $1 |
| `entry_window` | `0.5` | NEW pair attempts only in the round's first half (0 = whole round) |
| `round_sec` | `900` | declared round length for the window arithmetic |
| `entry_quote_mode` | `1` | 0 = at ask (v1.2 baseline, refused by the kernel — replay arm only), 1 = ask − `entry_reprice_ticks`, 2 = mid floored to the 0.01 grid, 3 = at the bid |
| `entry_reprice_ticks` | `1` | ticks under the ask for mode 1 |

A leg with no restable quote (locked book, quote below the 0.01 grid or
below `min_leg_price`) skips the whole attempt: fail closed, no naked quote.

## Replay matrix (2026-11-01, scripts/pair-fill-replay.mjs)

Three archive windows × five arms, one staged manifest-default diff per arm,
`--fee-model official --maker-rebate`, real REDEEM verdicts injected, every
number from a `--backtest-report` JSON:

| arm | orders(filled) | rejections | fills | closed | WR | PF | net USD | maker:taker |
|---|---|---|---|---|---|---|---|---|
| pair-baseline (mode 0 = v1.2) | 9(0) | 9 × `venue.WouldCross` | 0 | 0 | — | — | 0.00 | n/a |
| pair-ask-1t (mode 1, default) | 20(14) | 0 | 14 | 13 | 38.5% | 0.34 | −12.40 | 100:0 |
| pair-mid (mode 2) | 20(14) | 0 | 14 | 13 | 38.5% | 0.34 | −12.40 | 100:0 |
| pair-bid (mode 3) | 20(14) | 0 | 14 | 13 | 38.5% | 0.35 | −12.20 | 100:0 |
| pair-ask1t-999 (mode 1 + 0.999) | 26(20) | 0 | 20 | 19 | 42.1% | 0.59 | −10.77 | 100:0 |

Windows: w1015 = 2026-10-15 17:00–19:00Z, w1029 = 2026-10-29 00:00–02:00Z (both
Kalshi 15m wheels — the discount opens), w1012 = 2026-10-12 18:00–20:00Z
(Polymarket 15m wheels — **no in-round discount ever opened**: slot-start pair
totals probed at 1.040–1.045; honest zeros in every arm, not a harness
artifact — the baseline arm reproduces v1.2's WouldCross pattern exactly).

Reading:

- **The fill path is fixed.** v1.2: 0/9 orders rested (every one refused at
  POST — the at-ask quote is the one price a post-only buy can never hold).
  v1.3: 14/20 legs RESTED and filled, 100% maker, zero rejections. Quote depth
  (ask−1t / mid / bid) is inert on this corpus — its book updates jump whole
  levels, so a waterfall that crosses one restable level crosses them all —
  the default stays mode 1: the most aggressive RESTABLE quote gives the
  hedge leg the earliest possible fill in live books with wider spreads.
- **Merged pairs are the profit engine.** Every complete pair collected
  positive: +1.05 (w1015, cost 0.90), +0.77/+0.77/+0.77 across w1029's three
  pairs. The locked-at-entry economics survive contact with the replay.
- **The naked leg is now the whole loss.** 4 of 14 fills were solo legs (all
  UP — the victim side fills in the crash while the hedge side's ask rises)
  that rode to settlement at −99%: −12.78 of the −12.40 net. The gap-fill
  loop cannot help while the unfilled counterpart order still rests (the OME
  refuses a duplicate internal_key), and the doctrine forbids taker or sells.
  The fix is hypothesis 2/3 below (kernel surface), not more quote tuning.
- **0.999 relaxation** opens more windows (26 orders, 20 fills) and improves
  PF 0.34 → 0.59 — still negative on this sample; it stays a contrast arm.

## Pending hypotheses (this round's not-done list)

1. **Re-quote chase** — re-emit each evaluate at the fresh book while a leg
   rests. The kernel's OME refuses a duplicate `internal_key`
   (`strategy:asset:direction:slot`) while any order rests on it
   (ome.rs `has_pending`), so a chase only bites AFTER an order ends; the
   armed latch (one attempt per condition per round) currently never
   re-emits. Measuring "chase after a leg's rejection" needs the latch to
   distinguish rejected-at-POST from resting — kernel-side evidence first.
2. **Leg-asymmetric taker escalation** — first leg maker, second leg taker
   after a deadline (pays the ~1.75% one-leg taker fee instead of ~3.5%).
   `entry_maker` is a PACKAGE-level declaration (submit.rs routes every
   OrderRequest of the strategy to `FillPolicy::Maker`); a per-intent
   fill-policy override does not exist kernel-side. Needs a kernel feature
   (per-intent `fill_policy`) before the strategy can express it. The math:
   net = discount − one-leg taker fee ≈ 2.48% − 1.75% ≈ +0.7% — and the
   matrix shows this is exactly the missing piece: merged pairs all profit,
   naked legs are all of the loss.
3. **Naked-leg exit** — sell a solo leg when its counterpart is unfilled
   near round end (caps the −99% settlement tail at the spread cost). The
   package doctrine ("the strategy never sells") and `holds_to_settlement`
   forbid it today; changing either is a doctrine decision, not a tuning
   knob. Arithmetic from the matrix: the four naked settlements cost
   −15.75 while merged pairs earned +3.35 (+0.41 rebates) — capping the
   naked tail near zero flips the two windows from −12.40 to ≈ +3.8.
4. **`entry_window` posture** — the 10/15 window shows `blocked.timing = 0`
   and the w1012 probe run with `entry_window=1` placed 0 orders: the window
   is not the bottleneck on any window tested (w1012's zeros are no-signal,
   not window-blocked). Re-visit only when a replay shows an opening blocked
   by the window AND an unfilled arm.
5. **`max_pair_cost` relaxation matrix** — the 0.999 arm ran above (PF 0.59,
   net −10.77): more windows, thinner margins, same naked-leg tail. Re-run
   on more windows before touching the default.
6. **Scan-tool caveat** — `pair-fill-replay.mjs --scan` pairs each token's
   LATEST book without expiring tokens at their round end, so wheel-tail
   sums can pair books across two different rounds (cross-round artifacts).
   Treat scan episodes as candidates and confirm per-round before replaying
   (w1012's "0.64 episodes" were exactly this).

## Data notes

- Prices run on scaled integers (the §6.5 decimal-STRING wire rule); every
  trigger comparison is an exact integer comparison. Quotes are floored to
  the venue's 0.01 grid (modes 1–3) so a live POST never dies on an
  off-grid limit.
- One pair ATTEMPT per condition per round; a rejected attempt is not retried
  into the same book.
- Both books must be FRESH with a real ask on each leg. A stale or bid-less
  leg is not a discount, it is an outage.
- Round rollover retires every latch: conditions do not survive their round.
