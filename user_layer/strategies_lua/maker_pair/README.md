# maker_pair

Maker-only two-sided pair. Both legs rest strictly under their asks with the same share count, or the attempt is dropped. There is no at-ask quote and no one-sided top-up.

The cost test is `quote_up + quote_down < max_pair_cost` with maker fee 0. `bk.fees()` must still be present. Default size is 1 share per leg so a pair stays inside the 100 USDC profile's $2 order cap when each quote is at most $0.99.

`entry_maker` and `atomic_batch` are set. The package is not on any shipped allowlist.
