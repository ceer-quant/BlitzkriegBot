//! The round-market shape — moved VERBATIM from `blitzkrieg_core::model`.
//! Zero behaviour change; the kernel re-exports it under the original path.

use blitzkrieg_market_api::{TokenId, decimal};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A binary UP/DOWN market for one asset in the current round.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CryptoMarket {
    pub asset: String,
    pub condition_id: String,
    pub question_id: String,
    pub up_token_id: TokenId,
    pub down_token_id: TokenId,
    #[serde(with = "decimal")]
    pub up_price: Decimal,
    #[serde(with = "decimal")]
    pub down_price: Decimal,
    pub expires_at_ms: i64,
    pub round_slot: i64,
    /// #354 (5.4): the round's own duration in seconds, as the venue declares
    /// it (slug `5m` → 300, `15m` → 900). `0` = undeclared (old archives): the
    /// replay then falls back to the configured grid and the cadence gate
    /// stays silent. Declared durations that disagree with the configured
    /// grid are what the #354 fail-closed gate refuses — a 5m corpus replayed
    /// on the 15m assumption ages every round 3× too slowly and the window
    /// gates mis-time every exit.
    #[serde(default)]
    pub round_duration_sec: i64,
    /// #377 (治 #2): the archive carries the on-chain verdict for this market
    /// (a `Resolution` event is coming), so the dry settlement ladder must not
    /// synthesize a competing verdict from book evidence. `false` on live and
    /// old archives — the serde default keeps byte-compatibility for corpora
    /// built before the field existed.
    #[serde(default)]
    pub archive_verdict: bool,
    /// #427: which venue listed this market — pure DATA, never logic (#425
    /// model.rs: no code may branch on a specific venue). Empty string =
    /// undeclared (old archives, the single-plugin world the serde default
    /// keeps byte-compatible); a cross-venue corpus declares it per market so
    /// two venues' markets coexist in one round and one book table. The
    /// disambiguator between same-asset markets on two venues is the
    /// venue-tagged token id itself, not this field's value.
    #[serde(default)]
    pub venue: String,
    pub neg_risk: bool,
    pub question: String,
}
