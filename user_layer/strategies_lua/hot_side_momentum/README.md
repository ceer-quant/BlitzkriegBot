# hot_side_momentum

The confirmed-leader holder for binary rounds — buy the side the market AND
the spot tape agree is winning, then hold to settlement.

## Mechanism

When a binary round is **40%-70% through** (exact integer compare:
`time_left*100` vs `30*round_sec` / `60*round_sec`), and the Binance spot
momentum of the round's asset **confirms the market's leading side**, buy
that side at its ask and hold:

- **Leader** — the strictly higher ask; only when it sits in
  `[min_leading_price, max_leading_price]` (default 0.60-0.80). Equal asks
  name no leader.
- **Momentum** — CLOSED sec1 bars keyed by ASSET, oldest-in-window → latest
  close, `|move| >= mom_threshold_pct` (default 0.10%) with REAL coverage
  (span >= window − 1s). The trigger must be FRESH (`mom_max_age_ms`).
- **Alignment** — spot rising confirms an UP leader, falling confirms a DOWN
  leader. A contradiction is not a trade.
- **Collection** — the manifest declares `holds_to_settlement`: the position
  rides to resolution and the winner redeems $1.00/share. No exit is ever
  emitted (the「止损不归你管」seal); `exits` and `breaks` are always empty.

One entry ATTEMPT per condition per round. No spot data → no trigger → no
entries: fail-closed by construction.

## Tunables

| name | default | meaning |
|---|---|---|
| `round_sec` | `900` | round length (5m replays set 300) |
| `mom_window_sec` | `15` | spot momentum window |
| `mom_threshold_pct` | `0.10` | minimum window move to confirm |
| `mom_max_age_ms` | `2000` | trigger staleness bound |
| `min_leading_price` | `0.60` | leader ask lower bound |
| `max_leading_price` | `0.80` | leader ask upper bound |

## Data notes

- Prices run on scaled integers (the §6.5 decimal-STRING wire rule); every
  comparison is an exact integer comparison.
- Round rollover retires every latch: conditions do not survive their round.
- Both books must be FRESH with real asks — a stale side is an outage, not a
  signal.
