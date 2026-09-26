//! Account identity and status — DEV_V0_3 §9.1 / §14.0.
//!
//! Wave 0 lands the four frozen names (issue #329 row 6): `AccountId`,
//! `DEFAULT_ACCOUNT_ID`, `AccountStatus`, `CredentialKeys`. E25's audit
//! records, E28's account book and E27's `market.list` all read them, so they
//! must exist once — later Epics must not define private mirrors.
//!
//! Split by ownership (§11.1): `AccountId` / `DEFAULT_ACCOUNT_ID` /
//! `default_account_id()` live in `blitzkrieg_market_api::account` because the
//! strategy API has to name the account a round targets and may NOT depend on
//! the core; they are re-exported here so `blitzkrieg_core::account::AccountId`
//! is the SAME type both sides see. `AccountStatus` / `CredentialKeys` are
//! core-domain and are defined here directly.
//!
//! The rest of §9 — `Account`, `AccountLedgers` (per-account `Ledger`
//! instances), config loading and the credential reader — lands with **E28**.
//! Wave 0 has no behavior: no file is read, no credential is loaded, nothing
//! here runs.

use serde::{Deserialize, Serialize};

pub use blitzkrieg_market_api::account::{AccountId, DEFAULT_ACCOUNT_ID, default_account_id};

/// Account lifecycle state (DEV_V0_3 §9.1). Wire = `snake_case`.
///
/// Enforced at the arbitration pipeline's Gate 2 (E25/E28): only `Active`
/// places new entries. Tightening (`Active -> anything`) is allowed at
/// runtime via `account.status`; loosening is NOT — an unwind that a single
/// IPC call could undo is not an unwind (§12.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    /// May place entries (still subject to risk gates and the kill switch).
    Active,
    /// Read-only: view and reconcile, but any place is refused at Gate 2
    /// (`AccountLimit`).
    ReadOnly,
    /// No new entries; **closing is exempt** — same posture as the kill
    /// switch. A freeze must never be the reason a position is trapped.
    Frozen,
    /// Same as `Frozen` plus an operator-facing reason string.
    Suspended { reason: String },
}

/// Names of the environment variables an account's credentials come from —
/// **never the values** (§9.4 / §14.0 row 6). Wire = `camelCase`.
///
/// This struct is safe to serialize by construction: no path that constructs
/// it ever stores a credential value. The kernel process is the only reader
/// (from its own environment); IPC answers `credentialsLoaded: bool` and
/// nothing else, and the `account:credential-check` gate greps every response
/// for a planted sentinel value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialKeys {
    pub api_key_env: Option<String>,
    pub secret_env: Option<String>,
    pub passphrase_env: Option<String>,
    pub private_key_env: Option<String>,
    /// Whether the keys above were read successfully at startup — a boolean,
    /// never a value.
    pub loaded: bool,
}
