# whale_follow

The mid-round established-side taker entry for 5-minute binary rounds —
buy the leg the round has ALREADY decided on, at a PLATEAU (never a spike),
once, and hold to settlement.

## Mechanism

When a round is past its open (elapsed inside `[entry_elapsed_min_sec,
entry_elapsed_max_sec]`, default 90-240s of a 300s round) and ONE leg's own
mid has:

1. **Established** — priced at/above `min_entry_price` (0.55) AND moved at
   least `min_established_move` (0.05) off the round-open reference (the
   leg's own first fresh mid of the round);
2. **Plateaued** — reached that level with a `|move over
   `drift_window_sec`|` (default 60s) of at most `max_drift_60s` (0.10) —
   NOT mid-spike;
3. **At the running high** — the current mid is the leg's round running
   extreme (承接在极值上，不在回撤中接);

…lift that leg's ask once (band `[min_entry_price, max_entry_price]`,
default [0.55, 0.88]) and hold: `holds_to_settlement` is declared, `exits`
and `breaks` are always empty, the winner redeems $1.00/share.

One attempt per condition per round, one opening per round slot.

## Why this shape — the discriminating evidence (2026-11-01)

The core question this package answers: **what separates「已确立方向的中段
承接」from lua_momentum's 7.5%-WR top-ticking?** The answer, measured on an
UNBIASED corpus, is the plateau-vs-spike shape of the last 60s — not the
price level, not the timing alone.

Corpus: 450 settled BTC 5m rounds (2026-10-30 ~11:30 → 2026-11-01, fetched
from the public trades API with gamma settlement truth; entry priced at the
next real taker print after the signal; hard 1/0 settlement; both kernel
fee schedules). Scripts and corpus: `bk-session/smr-round/whale-fetch-tape.py`,
`whale-tape-corpus.jsonl`, `whale-tape-analysis.py`.

| rule (same "buy the leader" action) | n | WR | EV/share legacy | EV/share official |
|---|---|---|---|---|
| **EST = this package** (90-240s, mid ≥ 0.55, \|Δopen\| ≥ 0.05, \|Δ60s\| ≤ 0.10, at high) | 161 | **88.8%** | **+0.0743** | **+0.0679** |
| EST, older half | 82 | 87.8% | +0.0524 | +0.0462 |
| EST, newer half | 79 | 89.9% | +0.0970 | +0.0905 |
| CHASE (same window, \|Δ60s\| ≥ 0.15 — enter mid-spike) | 444 | 73.6% | +0.0117 | +0.0034 |
| ALL first-established (no plateau gate) | 450 | 66.0% | +0.0547 | +0.0452 |
| LATE (>240s, settlement-harvest) | 166 | 93.4% | +0.0602 | +0.0555 |

Reading: the plateau gate roughly TRIPLES the edge and takes the official-fee
chase variant to zero. A tighter plateau (0.05) starves (n=30, negative); a
looser one (0.15) halves the edge. 0.10 is the measured sweet spot.

Whale corroboration (same corpus + trackers):

- 6meteor (0xa3e1…752c), 10-29: 9 BTC/ETH 5m rounds, 9/9 main legs won
  (tracker verified against REDEEM truth), entries at offsets 41-175s,
  main-leg VWAP 0.48-0.83, BUY-only with zero sells, hold to settlement.
- 5 independent wallets 8/8 the same day (avg entries 0.49-0.58).
- On the 450-round tape: 267 distinct BUY-only wallets ≥ $300/round; their
  offset-120-240s first entries win **76.4%** vs 63.9% for offset-60-120s —
  the mid-late window this package trades. 28 of the 116 wallets with ≥5
  rounds have perfect records (~1.6x the binomial expectation of ~18).
- @almach's 5m profile (57-day on-chain corpus): BUY-only, 0 sells, offset
  p50 222s, hold-to-settlement, maker splits — the same shape with a
  different (pair-hedge) trigger.

Negative anchor: lua_momentum's up leg chased single-tick momentum into
tops — 7.5% WR over 40 live trades, and the kernel census measured the naive
shape at −$17.6k over 20,038 trades. The anti-chase gate is the wall.

## What it cannot know (ceilings, stated because they bind)

- **No on-chain whale feed exists in `bk.*`.** "Whale-follow" is a metaphor
  for the footprint: an established mid-round level is the proxy trigger.
  The strategy cannot see who accumulated and cannot distinguish a whale
  from any other repricing — it prices only what the whole market sees.
- **The round-open reference is the leg's own first fresh mid.** A strategy
  (re)start mid-round leaves the reference unknown and the leg silently
  unqualified for the establishment gate — fail-closed, never guessed.
- **The drift gate needs a full drift window of ring coverage** (ring =
  drift window + 30s at 250ms buckets). Sparse feeds inside the window
  refuse (fail-closed).
- **The corpus is BTC 5m.** The default filter is `assets="BTC"`,
  `round_sec=300` (which also implicitly excludes longer rounds: their
  computed elapsed never lands in the window). Other horizons/assets are an
  operator rescale and were NOT tested by the corpus.
- Entry is a taker lift of the ask; the corpus measured exactly that price.
  Maker entry was not tested and is not offered.

## Fees

Entries require the kernel's ONE fee schedule in force (`bk.fees()` sane —
fail-closed). The evidence quotes BOTH schedules: legacy_quadratic
(0.125·(p(1-p))²) and official (0.07·p(1-p)); the edge clears both (+0.074 /
+0.068 per share). At the EST band's prices the fee is 0.6%-1.5% of the
entry price — the WR margin (88.8% vs the ~55-70% implied by the price) is
the edge, not fee arbitrage.

## Tunables

| name | default | meaning |
|---|---|---|
| `round_sec` | `300` | round length; elapsed = round_sec − time_left_sec (5m evidence) |
| `assets` | `BTC` | comma-separated allowlist; empty = all (corpus tested BTC only) |
| `entry_elapsed_min_sec` | `90` | window floor (whale offsets 41-175s, almach R1 ≥ 90s) |
| `entry_elapsed_max_sec` | `240` | window ceiling (last 60s left to the harvester) |
| `min_entry_price` | `0.55` | established-side floor on mid and ask |
| `max_entry_price` | `0.88` | band ceiling (≥0.90 is the thin-payoff zone) |
| `min_established_move` | `0.05` | minimum mid move off the round-open reference |
| `max_drift_60s` | `0.10` | THE anti-chase wall: max \|60s move\| into the entry |
| `drift_window_sec` | `60` | the plateau measurement window |

Exemptions: `timing` only (`timing_min_time_left_sec: 60` — the window's own
edge); the kernel's momentum veto stays at full strength and every other
gate applies.

## Data notes

- Prices run on scaled integers (the §6.5 decimal-STRING wire rule); every
  gate comparison is an exact integer comparison.
- Samples are bucketed at 250ms per token (one sample per bucket, ring =
  drift window + 30s): bounded memory, O(ring) per evaluation.
- Round rollover retires every latch: conditions do not survive their round.
- No klines are read; the book's own path is the whole signal.

## Self-check

`selfcheck.lua` runs the strategy through the positive, negative, latch and
seal rows on a stub `bk` surface (pure Lua 5.4, no dependencies):

```sh
lua user_layer/strategies_lua/whale_follow/selfcheck.lua
```

Exit 0 = all 34 rows hold.
