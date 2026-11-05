# postonly_pair

The post-only paired maker WITH a pairing loop — issue #872's answer, one
step beyond `maker_pair` (#871). Both legs of a binary-round pair rest as
post-only buys quoted strictly under their own asks, aiming at a combined
cost under $1 — the chain pairing the onboarded wallets actually run — and
the book gets RE-QUOTED on a throttle instead of being frozen into one
batch. Maker fee = 0 is the premise.

## Mechanism

When the sum of the two RESTABLE maker quotes is under the cap,

```
quote_UP + quote_DOWN < max_pair_cost        (default 0.985, strict)
```

the package emits BOTH legs — EQUAL declared share counts, each priced
where a post-only buy can actually **rest** (`#464`: the at-ask POST is
refused; there is no at-ask mode). The cost test runs on the QUOTES: a
maker fill prints at its quote (at or below it, never worse) and a maker
leg pays NO fee, so `quote_UP + quote_DOWN` IS the all-in pair cost.
`bk.fees()` is read ONLY for the naked-remainder's fallback term (below).

## How it trades (and where it leaves maker_pair behind)

- **The pair loop** — a fresh attempt emits both legs instantly; every
  later cycle re-emits BOTH legs under the per-leg `requote_sec` throttle
  (default 15 s — the ticket's 10–20 s rhythm). A moved book gets a moved
  quote; armed ≠ finished.
- **The inventory cap** — held shares (`bk.holdings()`: THIS strategy's
  open positions) count against `max_shares_per_leg`; the pair declares
  the SMALLER leg's head-room floored to the 0.01 grid. At cap the
  condition is DONE — nothing more is quoted into it this round. This is
  the ticket's hard single-side ceiling.
- **The naked remainder** — one leg filled, the partner still short: the
  completion leg re-quotes every cycle (throttled like any leg) after the
  top-up qualification: incremental all-in = ask + the fallback taker fee
  (the cost of completing through a taker cross if this maker quote NEVER
  fills — a maker fill can only fill cheaper) strictly under
  `max_top_up_cost`, sized to the exact shortfall. The re-entry is
  #848's: hold-to-settlement releases the naked side's
  repeat-suppression. A gap past the ceiling is DEAD — the held leg rides
  to settlement.
- **The near-expiry pull** — at `time_left_sec <= pull_before_sec`
  (default 20, clamped strictly below the entry floor) the condition's
  lingering entry bids are pulled through the kernel's `breaks` channel
  (a break cancels EVERY resting BUY on the token), ONCE per condition
  per round; a bid-less book reports the 0.01 grid floor as the mark.
  After the pull, nothing re-quotes that condition: the quotation window
  closes strictly above the pull zone (the clamp in `read_config`).
- **Merge collection** — a COMPLETE pair in hand emits `reason = "merge"`
  for BOTH legs every cycle (the same posture pair_discount_arb runs);
  the kernel intercepts merge BEFORE the sell ladder and burns stacked
  pairs at $1/pair, fee-free.
- **Quote modes** — `entry_quote_mode`: 1 = ask − `entry_reprice_ticks`
  (default), 2 = mid floored to the grid, 3 = the bid. No at-ask mode
  (#464). Any quote the guards reject skips the WHOLE attempt: fail
  closed, no naked quote.

## Kernel surfaces used (zero kernel changes)

| declaration | effect | source |
|---|---|---|
| `entry_maker: true` | every entry submitted post-only, never escalates to taker; maker fee = 0 | #742 |
| `atomic_batch: true` | both legs of one fresh attempt are ONE economic ticket for an E-VA pot | #531 |
| `holds_to_settlement: true` | pair-completion entry exemption; the #848 naked-side release | #848/#904 |
| `trailing_exit` ABSENT | follows the kernel-wide value (the three-state default) | #742 |
| `breaks` (hook) | the near-expiry pull — cancels resting BUYs on both legs | existing ingest path |

No pot ships for this package: the `virtual_limits` allowlist stays empty
until the replay + soak gates both say GO.

## 红线 posture

- The package registers DISABLED (the startup scan registers every package
  disabled); enablement is the operator's account binding, nothing else.
  Not in any default-enabled set (issue #872 非目标：不因此打开实盘).
- `DRY_RUN` default untouched; no third-party Polymarket bot dependency.
- Default 1 share per leg: a pair ≈ $1 notional at mid.
- Replay stance is conservative by construction: the fill model's
  `trade-through` maker rule is what a replay should run with (only a
  market that goes strictly through the quote fills this bid), the
  single-le-goes-to-settlement economics is the package's own math, and
  the 20% maker rebate is #741's replay flag — never priced inside this
  package.

## Tunables

| name | default | meaning |
|---|---|---|
| `max_pair_cost` | `0.985` | all-in pair ceiling on the sum of the two quotes (strict `<`) |
| `max_shares_per_leg` | `1` | per-leg inventory cap in shares (held + to-be-declared) |
| `max_top_up_cost` | `0.985` | naked-remainder ceiling: incremental all-in (ask + fallback taker fee), strict `<` |
| `requote_sec` | `15` | per-leg re-quote throttle in seconds (0 = every cycle) |
| `min_time_left_sec` | `60` | entry time floor |
| `pull_before_sec` | `20` | near-expiry pull threshold (clamped < `min_time_left_sec`) |
| `entry_quote_mode` | `1` | 1 = ask − ticks, 2 = mid floored, 3 = the bid (no at-ask mode #464) |
| `entry_reprice_ticks` | `1` | ticks under the ask for mode 1 |

## Data notes

- Prices run on scaled integers (the §6.5 decimal-STRING wire rule); every
  comparison is an exact integer comparison.
- Both books must be FRESH with a real ask on each leg. A stale leg is an
  outage, not a half-signal.
- Round rollover retires every latch: conditions do not survive their
  round.

## 契约测试（issue #872 判据三态）

- `core/blitzkrieg_core/tests/postonly_pair_strategy_check.rs` — 包级：
  双腿整批挂出（适配器不打收费表 = maker 0 费成本模型钉死）、报价和
  超限整笔放弃、armed 后 throttle 节奏重报（未到间隔零重报 / 到点 /
  移书给移价）、库存帽收工 + 裸腿补齐（缺口规模、top-up 判据、gap-fill
  实名）、完整对两腿 merge、缺书放弃不武装、`time_left <= 20` 两腿各
  一条 breaks 且 latch 次周零重发、manifest 三态红线、注册即 disabled。
- `core/blitzkrieg_core/src/service/tests/postonly_entry_maker_tests.rs`
  — 引擎级（OME 全链）：t1 双腿皆 `FillPolicy::Maker` + `maker_timeout_ms
  = -1`、无 `:escalated`、无双腿以外的任何单；t2 UP 腿成交后裸腿补齐
  重报依然 Maker 永不升级（含 #464 拒镜不追）；t3 pull 区经 ingest 撤
  单路径收掉全部 resting BUY，区次周零新单。
