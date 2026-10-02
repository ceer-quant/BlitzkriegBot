# pair_discount_arb

The pair-discount collector for binary rounds — the @almach mechanism
(85,431 fills, zero sells, 12,570 merges) as a Lua strategy package.

## Mechanism

A complete UP+DOWN share-pair of one condition is worth exactly **$1.00
on-chain** (MERGE into collateral). When

```
ask_UP + ask_DOWN + entry_taker_fees(both legs) < max_pair_cost   (default 0.995)
```

the pair is a riskless discount: buy BOTH legs at their asks with EQUAL
declared share counts, then collect. The profit is locked at entry; the exit
is a collection, not a decision.

## How it trades

- **Entry** — one candidate per leg, priced AT its own ask (the kernel routes
  entries MakerThenTaker: the top of the book maker-fills immediately, a
  remainder escalates), with `shares` declared so the legs match in share
  count — a pair merges per share-pair. Shares size to the smaller leg's
  visible ask depth, floored to the 0.01 grid; the kernel still caps at its
  own `max_shares`.
- **Collection** — once a condition's pair attempt is armed, every later
  evaluation emits `reason = "merge"` exit intents for BOTH legs until the
  round ends. The kernel intercepts `merge` intents BEFORE the sell ladder:
  complete pairs burn into $1.00/pair (`PositionManager::merge_condition` +
  `Ledger::credit_merge`), unpaired remainders stay open for settlement, and
  an intent with no pair behind it is dropped. The strategy never sells.
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
| `max_pair_cost` | `0.995` | fire below this all-in per-pair cost |
| `min_leg_price` | `0.01` | refuse legs quoted below (corrupt/degenerate books) |
| `max_leg_price` | `0.99` | refuse legs quoted above |
| `min_pair_shares` | `1` | smallest mergeable pair size worth an order |
| `min_time_left_sec` | `30` | entry time floor (collection runs to the last tick) |

## Data notes

- Prices run on scaled integers (the §6.5 decimal-STRING wire rule); every
  trigger comparison is an exact integer comparison.
- One pair ATTEMPT per condition per round; a rejected attempt is not retried
  into the same book.
- Both books must be FRESH with a real ask on each leg. A stale or bid-less
  leg is not a discount, it is an outage.
- Round rollover retires every latch: conditions do not survive their round.
