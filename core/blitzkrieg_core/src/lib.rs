//! blitzkrieg-core: the Rust trading core.
//!
//! All funds / orders / risk / state consistency live in this crate. Node is a
//! thin shell that drives the binary over a Unix-domain-socket JSON-RPC channel.
//!
//! P0 module map:
//!  - `decimal`  money-safe wire (de)serialisation
//!  - `model`    orders, fills, statuses, structured error codes, events
//!  - `ome`      authoritative order state machine + idempotent fill ledger
//!  - `ledger`   USDC reservation / release (prevents overspend)
//!  - `risk`     kill switch + per-order notional gate
//!  - `sim`      dry-mode book-crossing matcher (shares OME with live)
//!  - `service`  typed command layer assembling ome/ledger/risk/sim
//!  - `ipc`      UDS JSON-RPC 2.0 schema + transport

// The engine.stats view (service.rs) is one huge `serde_json::json!` literal
// — each new key deepens the macro's expansion, and the accountFreezes key
// (account-scoped kill switch) blew past the default 128.
#![recursion_limit = "256"]

pub mod account;
/// v0.3 E25 (#331): the four-gate suggestion→decision pipeline (§3). The
/// strategy advises; the kernel adjudicates; every suggestion is audited.
pub mod arbitration;
pub mod backtest;
/// #353: the in-kernel backtest job registry — the WebUI's 拉数据→配置→回测→
/// 看结果 flow runs as tokio tasks inside the core process, addressed over
/// IPC; no subprocess, no filesystem writes from the WebUI.
pub mod backtest_jobs;
/// #361: the blueprint JSON → three-stage compile → Lua strategy surface.
pub mod blueprint;
pub mod cli;
pub mod config;
pub mod data_lock;
pub mod data_source;
/// The kernel-side `.env` writer (market.credentials.set landing). The read
/// side is the launcher's `ui_kit_panel::env_file` — same file format, one
/// contract.
pub mod env_file;
/// .env file watcher for hot reload.
pub use blitzkrieg_market_api::decimal;
pub mod engine;
/// #363: the account-isolated execution policy rule engine. Strategy emits
/// signals; THIS module decides whether an entry may place and with what
/// budget, per account, under the operator's `execution_policy.toml`.
pub mod execution_policy;
pub mod exit_policy;
pub mod extension;
/// #426: cross-venue hedge execution — the atomic two-leg state machine,
/// the fully-fee-loaded feasibility gate, and the hedge audit trail. The
/// stage-3 mapping (`blitzkrieg_market_api::unified`) is the ONLY way in:
/// an event not confirmed (or discrepancy-marked) by the ledger never
/// reaches a hedge.
pub mod hedge;
pub mod ipc;
pub(crate) mod jsonl;
pub mod kline;
pub mod ledger;
pub mod ledger_api;
pub mod logging;
pub mod market;
pub mod marketdata;
pub mod model;
pub mod ome;
// #648 C: the #352 on-chain pull (Polymarket Data/Gamma/CLOB APIs → the
// backtest JSONL event stream, with resume + cache + SHA256 manifests) MOVED
// to the `blitzkrieg-onchain-pull` crate. The kernel keeps only the
// `--onchain-pull` CLI entry (main.rs) and the IPC pull jobs
// (backtest_jobs.rs); both call into the crate.
pub mod order;
pub mod order_db;
/// RFC 568 阶段一：内核侧跨所套利配对状态簿（PairBook）。事实事件驱动的
/// 观察者账本：declare 受理策略层声明、on_order_event 吸收 OME 事实、
/// retire_before 轮次清退；经济身份 = (asset, slot)，进程内 BTreeMap，
/// 不持久化（#559 3c 的口径见模块注释）。
pub mod pair;
pub mod position;
pub mod position_db;
/// 环境差异排查探针（任务二 1-3）：SNTP 时钟偏差 / tick 断流 / spot 失效。
/// 只观测：日志 + 计数器；NTP 网络任务仅生产 `server::run()` 启动。
pub mod probe;
pub mod reconcile;
pub mod regime_eval;
pub mod risk;
pub mod risk_context;
pub mod scanner;
pub mod service;
pub mod settlement;
pub mod shadow;
pub mod shadow_evolution;
pub mod signal;
pub mod sim;
pub mod strategies;
pub mod strategy_engine;
pub mod strategy_state;
#[cfg(test)]
pub(crate) mod test_fixtures;
pub mod trade_db;

// The one runtime spelling of the build provenance (VERSIONING.md §3.4):
// stamped by build.rs into blitzkrieg-build-info, re-exported here so every
// consumer (IPC, --version, the banner) names the same code.
pub use blitzkrieg_build_info::BUILD_INFO;
pub const CORE_VERSION: &str = BUILD_INFO.version;
