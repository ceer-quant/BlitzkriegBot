//! #352 — on-chain data pull + conversion: Polymarket → backtest JSONL.
//!
//! The frozen corpus covers a few windows; a strategy's behaviour under the
//! REAL market distribution needs a wallet's actual history. Polymarket puts
//! every fill on chain and publishes it through three read APIs:
//!
//! * Data API  `data-api.polymarket.com/trades?user=<addr>` — the wallet's
//!   fills, newest first, offset-paged (`limit` ≤ 500). NOTE (#355): this
//!   endpoint answers only a TAKER-SIDE SUBSET of the wallet's fills
//!   (measured 2026-10-02: 17.6% of the same window's buy notional vs the
//!   `/activity` ledger, with 98%+ row-key overlap) — a fill universe built
//!   from it silently drops most rounds. `FillSource::ActivityFile` reads a
//!   pre-fetched `/activity` export as the complete universe instead.
//! * Gamma API `gamma-api.polymarket.com/markets?condition_ids=<id>` — market
//!   metadata; `clobTokenIds[0]`=Up / `[1]`=Down, `endDate` = round close.
//! * CLOB API  `clob.polymarket.com/prices-history?market=<token>&startTs=&endTs=&fidelity=1`
//!   — the published historical price series (~1-min points). No public L2
//!   history exists, so the honest book reconstruction is a top-of-book print
//!   at each point (`k=top`, nominal 1.0 depth — the same convention the
//!   replay-side books built from [`blitzkrieg_data_model::DataEvent::Book`]
//!   use for unknown sizes).
//!   No depth is invented: nothing in a replay of this stream can mint
//!   fillability that the venue did not publish.
//!
//! Layout under `data/onchain/`:
//!
//! ```text
//! <wallet>_<start>_<end>.trades.jsonl    # canonical raw fills (deduped, sorted)
//! <wallet>_<start>_<end>.jsonl           # converted event stream (--backtest input)
//! <wallet>_<start>_<end>.manifest.json   # range/counts/state + SHA256 of both files
//! .cache/conditions/<conditionId>.json   # Gamma metadata (immutable once closed)
//! .cache/prices/<token>_<start>_<end>.json
//! ```
//!
//! Resumability: the pull appends pages to a `.raw.jsonl` side file and
//! records `state.tradesOffset` in the manifest after every page. An
//! interrupted pull resumes from that offset; overlapping or shifted pages
//! are harmless because the finalize step dedupes by canonical fill key.
//! Cache: a completed manifest whose SHA256 verifies answers the same
//! request instantly (`cache_hit`), no network.
//!
//! Everything here is `async` and reports through a progress callback per
//! page/phase — the pull is never a blocking loop without feedback (the
//! #352 reverse gate), and the IPC layer (#353) wraps the same functions.

mod convert;
mod dataset;
mod fetch;
mod pull;
mod request;

pub use dataset::*;
pub use fetch::*;
pub use pull::*;
pub use request::*;

// Shared helpers: the domain modules reach each other and the externalized
// test module through the crate root (`use super::*;`). These re-exports are
// `pub(crate)`, so the public surface stays exactly the `pub use` set above.
pub(crate) use convert::{convert, gamma_tokens, write_events};
pub(crate) use dataset::{
    ConditionVerdict, TimedCashflow, append_line, load_cashflows, load_redemptions, read_json,
    resolve_winner, write_json_atomic,
};
pub(crate) use request::{
    CONVERTER_VERSION, cashflows_path, condition_cache_path, price_cache_path, raw_path,
    redemptions_path, report,
};
// Test-only access to helpers that are otherwise module-private.
#[cfg(test)]
pub(crate) use convert::resolved_pair;
#[cfg(test)]
pub(crate) use dataset::{fold_cashflow, fold_redemption};

#[cfg(test)]
#[path = "lib/tests.rs"]
mod tests;
