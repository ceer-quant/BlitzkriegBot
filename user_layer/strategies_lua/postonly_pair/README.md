# postonly_pair

Post-only bids on both Up and Down of a crypto Up/Down round. The package is not enabled unless an operator turns it on.

- Quotes sit at least one tick under each ask. There is no at-ask and no taker fallback (`entry_maker`).
- A new pair is sent only when both quotes exist and `quote_up + quote_down < max_pair_cost`. Maker fee is 0. The taker fee `shares * 0.07 * p * (1-p)` is not charged here, and the 20% maker rebate is not assumed.
- Resting quotes are cancelled and replaced on a 15s cadence (tunable 10–20s).
- Unpaired inventory cannot grow past `max_unpaired_shares` (default 1). The short leg is quoted only when it still completes under the cost cap; otherwise that bid is cancelled.
- Resting bids are cancelled in the last `cancel_before_sec` (default 20s). The venue settles on Chainlink TWAP; this package does not forecast that price. A complete pair asks the kernel to merge.
- Closed trades keep `netPnlUsd`. `postonly_pair` maker closes also write `adverseSelectionUsd` and `makerFillRate`. A scorer that only reads `netPnlUsd` still works.

Default size is 1 share per leg.
