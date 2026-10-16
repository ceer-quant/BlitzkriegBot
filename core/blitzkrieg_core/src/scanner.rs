//! Market scanner — round timing, clock offset and tradeable-window gates.
//!
//! Venue-agnostic round math: slot derivation, expiry countdown, the clock
//! offset against the venue's reported end time, and the timing gates. The
//! Polymarket-specific slug construction and Gamma JSON parsing now live in the
//! Polymarket extension's `gamma` module.

use crate::model::CryptoMarket;
use std::collections::HashMap;

/// The clock bucket for markets (and corpora) that carry no venue tag — the
/// pre-multi-plugin behaviour. Empty string == this bucket everywhere the
/// scanner keys by venue.
const DEFAULT_VENUE: &str = "";

/// Duration label for a round length (5m/15m/1h/4h/daily). Kept in the core
/// because it describes round cadence, not a venue; the extension reuses the
/// same mapping when it builds discovery slugs.
pub fn duration_label_for(round_duration_sec: i64) -> Option<&'static str> {
    match round_duration_sec {
        300 => Some("5m"),
        900 => Some("15m"),
        3600 => Some("1h"),
        14400 => Some("4h"),
        86400 => Some("daily"),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct ScannerConfig {
    pub assets: Vec<String>,
    pub round_duration_sec: i64,
    pub min_round_age_sec: i64,
    pub min_time_left_sec: i64,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            assets: vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()],
            round_duration_sec: 900,
            min_round_age_sec: 30,
            min_time_left_sec: 180,
        }
    }
}

/// Why the tradeable window is closed, as data rather than a prose string.
///
/// The distinction matters for E2-b (#27): a strategy may declare an exemption
/// for the two *window* gates, but NOT for [`TimingBlock::NoMarkets`] — a round
/// with no discovered market has no priceable token, so nothing can be exempted
/// into existing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingBlock {
    /// No markets discovered for the current round.
    NoMarkets,
    /// Round younger than `min_round_age_sec`.
    TooYoung { age_sec: i64, min_age_sec: i64 },
    /// Fewer than `min_time_left_sec` seconds remain in the round.
    TooCloseToExpiry {
        time_left_sec: i64,
        min_time_left_sec: i64,
    },
}

impl TimingBlock {
    /// Whether a per-strategy declaration may waive this block. A structural
    /// precondition (no market at all) is never waivable.
    pub fn exemptible(&self) -> bool {
        !matches!(self, TimingBlock::NoMarkets)
    }
}

impl std::fmt::Display for TimingBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimingBlock::NoMarkets => write!(f, "No active markets"),
            TimingBlock::TooYoung {
                age_sec,
                min_age_sec,
            } => {
                write!(f, "Round too young ({age_sec}s < {min_age_sec}s)")
            }
            TimingBlock::TooCloseToExpiry {
                time_left_sec,
                min_time_left_sec,
            } => {
                write!(
                    f,
                    "Too close to expiry ({time_left_sec}s < {min_time_left_sec}s)"
                )
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct RoundState {
    pub slot: i64,
    pub expires_at_ms: i64,
    pub age_sec: i64,
    pub time_left_sec: i64,
    pub markets: Vec<CryptoMarket>,
}

/// [`RoundState`] without the owned markets snapshot — the slot/window clock
/// alone. Hot-path callers (engine evaluation, timing gates) read only these
/// fields; the copy this variant avoids is a heap allocation per market per
/// call (E14).
#[derive(Debug, Clone, Copy)]
pub struct RoundTiming {
    pub slot: i64,
    pub expires_at_ms: i64,
    pub age_sec: i64,
    pub time_left_sec: i64,
}

pub struct Scanner {
    cfg: ScannerConfig,
    markets: Vec<CryptoMarket>,
    /// Venue-reported actual round end (ms), keyed by the market's `venue`
    /// tag. Multi-plugin data planes run one clock per venue: a Kalshi
    /// declaration must not move the Polymarket expiry and vice versa. The
    /// empty-string key is the legacy bucket — markets (or corpora) without a
    /// venue tag — so old replays behave exactly as the single-value clock did.
    actual_end_time_ms: HashMap<String, i64>,
    /// Venue time − local time (ms), keyed by venue like `actual_end_time_ms`.
    clock_offset_ms: HashMap<String, i64>,
}

impl Scanner {
    pub fn new(cfg: ScannerConfig) -> Self {
        Self {
            cfg,
            markets: Vec::new(),
            actual_end_time_ms: HashMap::new(),
            clock_offset_ms: HashMap::new(),
        }
    }

    pub fn set_config(&mut self, cfg: ScannerConfig) {
        self.cfg = cfg;
    }

    pub fn markets(&self) -> &[CryptoMarket] {
        &self.markets
    }

    pub fn market(&self, asset: &str) -> Option<&CryptoMarket> {
        let up = asset.to_uppercase();
        self.markets.iter().find(|m| m.asset == up)
    }

    /// The market a token belongs to (either side). Multi-plugin scanners hold
    /// several venues' markets, and each venue's book has its own clock — the
    /// exit policy reads the token's OWN market's expiry through this.
    pub fn market_for_token(&self, token: &str) -> Option<&CryptoMarket> {
        self.markets
            .iter()
            .find(|m| m.up_token_id == token || m.down_token_id == token)
    }

    pub fn set_markets(&mut self, markets: Vec<CryptoMarket>) {
        self.markets = markets;
    }

    pub fn clock_offset_ms(&self) -> i64 {
        self.clock_offset_ms
            .get(DEFAULT_VENUE)
            .copied()
            .or_else(|| self.clock_offset_ms.values().copied().min())
            .unwrap_or(0)
    }

    /// Duration label used in Gamma slugs (5m/15m/1h/4h/daily).
    pub fn duration_label(&self) -> Option<&'static str> {
        duration_label_for(self.cfg.round_duration_sec)
    }

    /// Current round slot, floored to the round boundary using venue time.
    /// The slot grid is global this deployment (every plugin runs the same
    /// round length); only the expiry refinements are per-venue.
    pub fn current_slot(&self, local_now_ms: i64) -> i64 {
        let offset = self.clock_offset_ms();
        let venue_now = local_now_ms + offset;
        venue_now / 1000 / self.cfg.round_duration_sec
    }

    fn slot_expiry_ms(&self, slot: i64) -> i64 {
        (slot + 1) * self.cfg.round_duration_sec * 1000
    }

    /// The round clock without a markets copy: slot, expiry and the two window
    /// gates. This is the shape the evaluation hot path consumes — it walks the
    /// scanner's live market list by reference, so cloning the owned snapshot
    /// per call (two heap-allocated strings per market, twice per evaluate)
    /// is pure waste there. `round_state` remains for callers that want the
    /// owned markets snapshot.
    ///
    /// With several venues live the global clock is the EARLIEST observed
    /// expiry (grid default when none): a round is tradeable only while every
    /// venue's market is still open, so the most conservative expiry gates the
    /// shared window. With one venue this is exactly the old single-value
    /// behaviour (the observed end replaces the grid, never min'ed with it).
    pub fn round_timing(&self, local_now_ms: i64) -> RoundTiming {
        self.round_timing_for_venue(DEFAULT_VENUE, local_now_ms)
    }

    /// The same clock, refined to ONE venue's observed expiry (empty venue =
    /// the legacy bucket). Callers that already know the venue of the market
    /// they judge (per-candidate gates, per-token exits) use this so one
    /// venue's observed drift never freezes a sibling venue's trading.
    pub fn round_timing_for_venue(&self, venue: &str, local_now_ms: i64) -> RoundTiming {
        let slot = self.current_slot(local_now_ms);
        let grid = self.slot_expiry_ms(slot);
        let earliest_observed = self
            .actual_end_time_ms
            .values()
            .copied()
            .filter(|t| *t > 0)
            .min();
        let effective = match self.actual_end_time_ms.get(venue) {
            // The venue's own declaration is authoritative for its markets.
            Some(t) if *t > 0 => *t,
            // No own declaration: the conservative global clock — the earliest
            // observed expiry across venues, or the grid when none has spoken.
            _ => earliest_observed.unwrap_or(grid),
        };
        let time_left = ((effective - local_now_ms) / 1000).max(0);
        RoundTiming {
            slot,
            expires_at_ms: effective,
            age_sec: self.cfg.round_duration_sec - time_left,
            time_left_sec: time_left,
        }
    }

    pub fn round_state(&self, local_now_ms: i64) -> RoundState {
        let t = self.round_timing(local_now_ms);
        RoundState {
            slot: t.slot,
            expires_at_ms: t.expires_at_ms,
            age_sec: t.age_sec,
            time_left_sec: t.time_left_sec,
            markets: self.markets.clone(),
        }
    }

    /// Tradeable window: markets present, round old enough, not too close to expiry.
    pub fn can_trade(&self, local_now_ms: i64) -> Result<(), String> {
        self.can_trade_reason(local_now_ms)
            .map_err(|b| b.to_string())
    }

    /// The same check as [`Self::can_trade`], as a structured reason so the
    /// engine can tell a waivable window gate from the structural precondition
    /// (E2-b / #27). The expiry gate reads the EARLIEST expiry across venues
    /// (a round whose any-venue market is about to close is not tradeable),
    /// while presence/age stay global — this is the whole-table verdict the
    /// replay wake math relies on.
    pub fn can_trade_reason(&self, local_now_ms: i64) -> Result<(), TimingBlock> {
        let t = self.round_timing(local_now_ms);
        if self.markets.is_empty() {
            return Err(TimingBlock::NoMarkets);
        }
        if t.age_sec < self.cfg.min_round_age_sec {
            return Err(TimingBlock::TooYoung {
                age_sec: t.age_sec,
                min_age_sec: self.cfg.min_round_age_sec,
            });
        }
        if t.time_left_sec < self.cfg.min_time_left_sec {
            return Err(TimingBlock::TooCloseToExpiry {
                time_left_sec: t.time_left_sec,
                min_time_left_sec: self.cfg.min_time_left_sec,
            });
        }
        Ok(())
    }

    /// Per-market form of [`Self::can_trade_reason`]: the expiry gate reads
    /// THE MARKET'S OWN venue clock, so one venue running hot-to-expiry no
    /// longer freezes a sibling venue's candidates (multi-plugin data plane).
    /// Presence and age remain global (they describe the shared round).
    pub fn can_trade_reason_for(
        &self,
        market: &CryptoMarket,
        local_now_ms: i64,
    ) -> Result<(), TimingBlock> {
        if self.markets.is_empty() {
            return Err(TimingBlock::NoMarkets);
        }
        let t = self.round_timing_for_venue(&market.venue, local_now_ms);
        if t.age_sec < self.cfg.min_round_age_sec {
            return Err(TimingBlock::TooYoung {
                age_sec: t.age_sec,
                min_age_sec: self.cfg.min_round_age_sec,
            });
        }
        if t.time_left_sec < self.cfg.min_time_left_sec {
            return Err(TimingBlock::TooCloseToExpiry {
                time_left_sec: t.time_left_sec,
                min_time_left_sec: self.cfg.min_time_left_sec,
            });
        }
        Ok(())
    }

    /// Record the venue-reported end time and derive the clock offset — in the
    /// reporting market's OWN venue slot.
    ///
    /// A declaration more than two rounds out (in either direction) is
    /// REFUSED rather than adopted: a discovery bug feeding a strike-ladder
    /// expiry (years ahead) or a long-stale timestamp would otherwise hijack
    /// the round clock, and every timing decision (`time_left_sec`, the exit
    /// policy's clock, the D-31 floor) drifts with it. The slot grid keeps
    /// running on its own cadence until an in-band declaration arrives.
    pub fn observe_end_time(&mut self, end_ms: i64, local_now_ms: i64) {
        self.observe_end_time_for_venue(DEFAULT_VENUE, end_ms, local_now_ms);
    }

    /// The same observation, stamped into `venue`'s clock slot. An empty
    /// venue lands in the legacy bucket, keeping #498-era corpora identical.
    pub fn observe_end_time_for_venue(&mut self, venue: &str, end_ms: i64, local_now_ms: i64) {
        if end_ms <= 0 {
            return;
        }
        let round_ms = self.cfg.round_duration_sec * 1000;
        if (end_ms - local_now_ms).abs() > 2 * round_ms {
            return;
        }
        self.actual_end_time_ms.insert(venue.to_string(), end_ms);
        let local_slot = (local_now_ms / 1000) / self.cfg.round_duration_sec;
        let local_expected_end = (local_slot + 1) * self.cfg.round_duration_sec * 1000;
        self.clock_offset_ms
            .insert(venue.to_string(), end_ms - local_expected_end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn slot_and_round_state_math() {
        let s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        // 900_000 ms boundary: slot 1, expiry 1_800_000.
        let now = 1_000_000i64;
        assert_eq!(s.current_slot(now), 1);
        let r = s.round_state(now);
        assert_eq!(r.expires_at_ms, 1_800_000);
        assert_eq!(r.time_left_sec, 800);
        assert_eq!(r.age_sec, 100);
    }

    #[test]
    fn clock_offset_from_reported_end_time() {
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        // Local expected end for slot 1 = 1_800_000; the venue says 1_800_500.
        s.observe_end_time(1_800_500, now);
        assert_eq!(s.clock_offset_ms(), 500);
    }

    #[test]
    fn out_of_band_end_time_declarations_are_refused() {
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        let observed = |s: &Scanner| {
            s.actual_end_time_ms
                .get(DEFAULT_VENUE)
                .copied()
                .unwrap_or(0)
        };
        // A strike-ladder expiry years ahead (the kalshi discovery bug fed a
        // 2045 close into this path) must not hijack the round clock.
        s.observe_end_time(now + 10 * 365 * 86_400_000, now);
        assert_eq!(observed(&s), 0, "out-of-band declaration refused");
        assert_eq!(s.clock_offset_ms(), 0);
        // Same for a stale timestamp far in the past.
        s.observe_end_time(now - 3 * 900_000, now);
        assert_eq!(observed(&s), 0);
        // In-band (≤ 2 rounds out) still lands.
        s.observe_end_time(now + 1_800_000, now);
        assert_eq!(observed(&s), now + 1_800_000);
    }

    #[test]
    fn can_trade_enforces_age_and_time_left() {
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            min_round_age_sec: 30,
            min_time_left_sec: 180,
            ..Default::default()
        });
        // No markets → blocked.
        assert!(s.can_trade(1_000_000).is_err());
        s.set_markets(vec![CryptoMarket {
            asset: "BTC".into(),
            condition_id: "c".into(),
            question_id: "q".into(),
            up_token_id: "u".into(),
            down_token_id: "d".into(),
            up_price: dec!(0.5),
            down_price: dec!(0.5),
            expires_at_ms: 1_800_000,
            round_slot: 1,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: String::new(),
            neg_risk: true,
            question: "".into(),
        }]);
        // age 100s, time_left 800s → ok
        assert!(s.can_trade(1_000_000).is_ok());
        // Very close to expiry → blocked.
        assert!(s.can_trade(1_799_900).is_err());
    }

    #[test]
    fn the_timing_block_is_structured_and_keeps_the_prose_form() {
        // E2-b (#27): the engine needs the reason as data (to decide whether a
        // strategy may waive it) while every existing caller keeps the string.
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            min_round_age_sec: 30,
            min_time_left_sec: 180,
            ..Default::default()
        });
        assert_eq!(s.can_trade_reason(1_000_000), Err(TimingBlock::NoMarkets));
        assert_eq!(s.can_trade(1_000_000).unwrap_err(), "No active markets");

        s.set_markets(vec![CryptoMarket {
            asset: "BTC".into(),
            condition_id: "c".into(),
            question_id: "q".into(),
            up_token_id: "u".into(),
            down_token_id: "d".into(),
            up_price: dec!(0.5),
            down_price: dec!(0.5),
            expires_at_ms: 1_800_000,
            round_slot: 1,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: String::new(),
            neg_risk: true,
            question: "".into(),
        }]);
        // Round is 10s old at t=1_000_000 + ... → age 10s < 30s.
        assert_eq!(
            s.can_trade_reason(900_000),
            Err(TimingBlock::TooYoung {
                age_sec: 0,
                min_age_sec: 30
            })
        );
        assert_eq!(
            s.can_trade_reason(1_799_900),
            Err(TimingBlock::TooCloseToExpiry {
                time_left_sec: 0,
                min_time_left_sec: 180
            })
        );
        assert!(s.can_trade_reason(1_000_000).is_ok());
    }

    #[test]
    fn only_the_window_gates_are_exemptible() {
        // A per-strategy declaration may waive the window gates; a round with no
        // market at all is a structural precondition and never waivable.
        assert!(!TimingBlock::NoMarkets.exemptible());
        assert!(
            TimingBlock::TooYoung {
                age_sec: 0,
                min_age_sec: 30
            }
            .exemptible()
        );
        assert!(
            TimingBlock::TooCloseToExpiry {
                time_left_sec: 0,
                min_time_left_sec: 180
            }
            .exemptible()
        );
        assert!(
            TimingBlock::TooCloseToExpiry {
                time_left_sec: 120,
                min_time_left_sec: 180
            }
            .to_string()
            .contains("Too close to expiry (120s < 180s)")
        );
    }
}
