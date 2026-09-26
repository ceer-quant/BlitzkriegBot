//! Account identity — the shared half of DEV_V0_3 §9.1.
//!
//! `AccountId` is defined here, not in `blitzkrieg_core`, for exactly the
//! reason `Kline` is (see `kline.rs`): the strategy API describes the account a
//! round's intents target (§9.2 row 6) and the strategy API may NOT depend on
//! the core. The core re-exports these names from `crate::account`, so
//! `crate::account::AccountId` and `blitzkrieg_market_api::account::AccountId`
//! are one type, not two mirrors that can drift.
//!
//! The rest of §9 (`Account`, `AccountLedgers`, `AccountStatus`,
//! `CredentialKeys`) is core-domain and lands with E28; only the identity and
//! its default live here because both sides of the ABI need them.

use serde::{Deserialize, Serialize};

/// Account identity. A string newtype, not an integer: accounts come from
/// configuration and operators, and numbering them only makes logs say
/// "account 3" where a human-readable name was available.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub String);

/// Where 0.2 data (rows with no `account_id`) lands, so historical JSONL stays
/// readable without a migration (§9.2: read old, write new).
pub const DEFAULT_ACCOUNT_ID: &str = "default";

/// `serde(default = ...)` target for every `account_id` field added in 0.3 —
/// the wire default equals the old behaviour.
pub fn default_account_id() -> AccountId {
    AccountId(DEFAULT_ACCOUNT_ID.to_string())
}

impl AccountId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AccountId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
