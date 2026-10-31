//! P0 risk-gate acceptance tests — the ISSUE-level behaviour, not the unit.
//!
//! Four audit findings are pinned here end to end, through the same public
//! entry points the kernel uses at runtime (`Core::place`, `Core::flatten`,
//! `Core::tick`, `PositionManager::check_exits`), because the unit tests inside
//! `risk.rs` / `exit_policy.rs` / `position.rs` each cover only one layer of the
//! chain these bugs travelled through:
//!
//! * **#174** — the kill switch must freeze ENTRIES and never the way out of a
//!   position: the closing SELL has to pass the same risk gate that refuses the
//!   BUY, or a stop-limited loss silently becomes unlimited.
//! * **#177** — the protective stop must fire when the bid is gone, missing or
//!   dislocated on a real decline, and a genuine pin bar that IS withheld must
//!   leave a report behind instead of vanishing.
//! * **#173** — the daily-loss breaker must actually exist: a real day boundary,
//!   a loss that survives a restart, a cap relative to the account, and a trip
//!   the operator can see.
//! * **#202** — one order's notional must be bounded as a SHARE of the account,
//!   because a fixed lot is not a risk statement: on the live 4.8 USDC book the
//!   shipped 10-share band commits 4.00–6.00 USD per order (83%–125% of it).
//!   The cap rejects over-cap orders and still lets every close through.
//!
//! Split by finding in #780 (each file was under the 1500-line rule); Cargo
//! names the target after the directory, so `cargo test --test risk_gates`
//! still runs all of them.

mod daily_loss_tests;
mod exits_tests;
mod sizing_tests;
mod support;
