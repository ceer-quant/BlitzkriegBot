# maker_pair

The both-restable maker pair — issue #871's answer to
`pair_discount_arb`'s structural loss mode. A binary-round pair is quoted
ONLY when every leg can rest as a post-only buy strictly under its own ask,
and a single leg is never chased, topped up, or repaired. Leg dead = leg
dead.

## Mechanism

When the sum of the two RESTABLE maker quotes is under the cap,

```
quote_UP + quote_DOWN < max_pair_cost        (default 0.985, strict)
```

the package emits BOTH legs in one evaluation cycle — EQUAL declared share
counts (default **1 share per leg**), each priced where a post-only buy can
actually **rest**. Otherwise it emits NOTHING. There is no third outcome.

## How it trades

- **Both or nothing** — every leg must carry a book the kernel would accept
  a post-only POST into: fresh, a real ask, visible ask depth, and a quote
  strictly UNDER that ask (the kernel refuses an at-ask post-only buy at
  POST — `#464`, `venue.WouldCross`). ONE leg failing any guard abandons
  the WHOLE attempt: zero entries, not one. An abandoned attempt is not an
  attempt — nothing is armed, and a later cycle with both books back
  re-tests the pair from scratch.
- **The cost test runs on the quotes** — a maker fill prints at its quote
  (at or below it, never worse), and a post-only maker leg pays no fee, so
  the honest all-in cost of the pair is `quote_UP + quote_DOWN`.
  `bk.fees()` is never read: no fee schedule in force is NOT an error here,
  because no fee enters this package's cost model — the manifest declares
  `entry_maker`, so the kernel routes every entry post-only and the taker
  schedule never applies. A venue that charges maker fees must be priced
  into `max_pair_cost` by the operator.
- **No single-leg repair** — one attempt per condition per round
  (`armed[condition] = round slot` on emission). A leg that is rejected at
  POST, unfilled, or partially filled stays exactly as it is: no top-up, no
  re-quote, no taker escalation, no merge intent. The naked remainder rides
  to settlement (`holds_to_settlement`), which is the accepted structural
  loss — and the reason the default size is one share per leg. The kernel's
  naked-leg requote exemption (issues #904/#848) keys on hold-to-settlement
  strategies that RE-EMIT leg intents while holding a single side; this
  package never emits one, so the exemption never flows for it.
- **No collection either** — unlike `pair_discount_arb`, this package emits
  no `merge` intents. A complete pair redeems exactly $1.00 at settlement
  regardless; merging early would only recycle collateral this package
  never reuses (one attempt per condition per round).
- **Quote modes** — `entry_quote_mode` picks the depth of the restable
  quote: 1 = ask − `entry_reprice_ticks` (default — the most aggressive
  RESTABLE quote: the falling ask has to step only one tick onto us), 2 =
  mid floored to the 0.01 grid, 3 = the bid. There is NO at-ask mode: the
  at-ask price is the one price a post-only buy can never rest at (#464),
  and this package's contract is "strictly under the ask".

## Kernel surfaces used (zero kernel changes)

| declaration | effect | source |
|---|---|---|
| `entry_maker: true` | every entry submitted post-only, never escalates to taker | #742 |
| `atomic_batch: true` | the two legs of one cycle are ONE economic ticket for an E-VA pot | #531 |
| `holds_to_settlement: true` | pair-completion entry exemption; exit ladder leaves the legs alone in dry/read-only | existing |
| `trailing_exit` ABSENT | follows the kernel-wide value (the three-state default) | #742 |

`atomic_batch` arms nothing by itself: the pot comes from
`CoreConfig::virtual_limits`, and **no pot ships for this package — the
allowlist stays empty** until the freeze-window replay and soak gates both
say GO.

## 红线 posture

- The package registers DISABLED (the startup scan registers every package
  disabled); enablement is the operator's account binding, nothing else.
- `DRY_RUN` default untouched; no start script; no live enablement.
- Default 1 share per leg: at mid prices a leg is ≈ $0.50, a pair ≈ $1, and
  one order sits inside the 100 USDC config's $2 single-order notional.

## Tunables

| name | default | meaning |
|---|---|---|
| `max_pair_cost` | `0.985` | all-in pair ceiling, tested on the sum of the two quotes (strict `<`) |
| `shares_per_leg` | `1` | per-leg size in shares, identical on both legs by construction |
| `min_time_left_sec` | `30` | entry time floor (a maker quote needs round life left to be walked into) |
| `entry_quote_mode` | `1` | 1 = ask − ticks, 2 = mid floored to the grid, 3 = the bid (no at-ask mode) |
| `entry_reprice_ticks` | `1` | ticks under the ask for mode 1 |

## 回放报告（占位 / replay report placeholder）

**本包尚未跑任何回放；成交率没有被测到。** （No replay has been run for
this package; **the fill rate was NOT measured.**）A resting post-only
quote fills only when the live ask descends onto it — the fill-rate
question is empirical and open until a freeze-window replay on the
`@almach`-style corpus plus a soak gate say GO. Until then:

- the allowlist stays empty (no `virtual_limits` pot, no account binding
  shipped),
- the package stays disabled by default,
- `max_pair_cost` / quote-mode defaults are replay arms to be measured,
  not claims.

## Data notes

- Prices run on scaled integers (the §6.5 decimal-STRING wire rule); every
  trigger comparison is an exact integer comparison. Quotes are floored to
  the venue's 0.01 grid so a live POST never dies on an off-grid limit.
- One pair ATTEMPT per condition per round; a rejected attempt is not
  retried into the same book.
- Both books must be FRESH with a real ask (and visible ask depth) on each
  leg. A stale or phantom leg is not a half-signal, it is an outage.
- Round rollover retires every latch: conditions do not survive their round.
