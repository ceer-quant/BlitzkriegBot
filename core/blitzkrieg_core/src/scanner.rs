//! Market scanner — round timing, clock offset and tradeable-window gates.
//!
//! Venue-agnostic round math: slot derivation, expiry countdown, the clock
//! offset against the venue's reported end time, and the timing gates. The
//! Polymarket-specific slug construction and Gamma JSON parsing now live in the
//! Polymarket extension's `gamma` module.

use crate::model::{CryptoMarket, SignalDirection};
use rust_decimal::Decimal;

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

#[derive(Debug, Clone)]
pub struct RoundState {
    pub slot: i64,
    pub expires_at_ms: i64,
    pub age_sec: i64,
    pub time_left_sec: i64,
    pub markets: Vec<CryptoMarket>,
}

pub struct Scanner {
    cfg: ScannerConfig,
    markets: Vec<CryptoMarket>,
    /// Actual Polymarket end time (ms) when known — more accurate than the slot.
    actual_end_time_ms: i64,
    /// Polymarket time − local time (ms).
    clock_offset_ms: i64,
}

impl Scanner {
    pub fn new(cfg: ScannerConfig) -> Self {
        Self { cfg, markets: Vec::new(), actual_end_time_ms: 0, clock_offset_ms: 0 }
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

    pub fn set_markets(&mut self, markets: Vec<CryptoMarket>) {
        self.markets = markets;
    }

    pub fn clock_offset_ms(&self) -> i64 {
        self.clock_offset_ms
    }

    /// Duration label used in Gamma slugs (5m/15m/1h/4h/daily).
    pub fn duration_label(&self) -> Option<&'static str> {
        duration_label_for(self.cfg.round_duration_sec)
    }

    /// Current round slot, floored to the round boundary using Polymarket time.
    pub fn current_slot(&self, local_now_ms: i64) -> i64 {
        let poly_now = local_now_ms + self.clock_offset_ms;
        poly_now / 1000 / self.cfg.round_duration_sec
    }

    fn slot_expiry_ms(&self, slot: i64) -> i64 {
        (slot + 1) * self.cfg.round_duration_sec * 1000
    }

    pub fn round_state(&self, local_now_ms: i64) -> RoundState {
        let slot = self.current_slot(local_now_ms);
        if self.actual_end_time_ms > 0 {
            let time_left = ((self.actual_end_time_ms - local_now_ms) / 1000).max(0);
            return RoundState {
                slot,
                expires_at_ms: self.actual_end_time_ms,
                age_sec: self.cfg.round_duration_sec - time_left,
                time_left_sec: time_left,
                markets: self.markets.clone(),
            };
        }
        let expires = self.slot_expiry_ms(slot);
        let time_left = ((expires - local_now_ms) / 1000).max(0);
        RoundState {
            slot,
            expires_at_ms: expires,
            age_sec: self.cfg.round_duration_sec - time_left,
            time_left_sec: time_left,
            markets: self.markets.clone(),
        }
    }

    /// Tradeable window: markets present, round old enough, not too close to expiry.
    pub fn can_trade(&self, local_now_ms: i64) -> Result<(), String> {
        let r = self.round_state(local_now_ms);
        if r.markets.is_empty() {
            return Err("No active markets".into());
        }
        if r.age_sec < self.cfg.min_round_age_sec {
            return Err(format!("Round too young ({}s < {}s)", r.age_sec, self.cfg.min_round_age_sec));
        }
        if r.time_left_sec < self.cfg.min_time_left_sec {
            return Err(format!(
                "Too close to expiry ({}s < {}s)",
                r.time_left_sec, self.cfg.min_time_left_sec
            ));
        }
        Ok(())
    }

    /// Record the venue-reported end time and derive the clock offset.
    pub fn observe_end_time(&mut self, end_ms: i64, local_now_ms: i64) {
        if end_ms <= 0 {
            return;
        }
        self.actual_end_time_ms = end_ms;
        let local_slot = (local_now_ms / 1000) / self.cfg.round_duration_sec;
        let local_expected_end = (local_slot + 1) * self.cfg.round_duration_sec * 1000;
        self.clock_offset_ms = end_ms - local_expected_end;
    }

    /// Update live UP/DOWN prices from WS/feed data.
    pub fn update_price(&mut self, condition_id: &str, up: Decimal, down: Decimal) {
        if let Some(m) = self.markets.iter_mut().find(|m| m.condition_id == condition_id) {
            m.up_price = up;
            m.down_price = down;
        }
    }

    /// Token id for a direction in the current round's market for an asset.
    pub fn token_for(&self, asset: &str, dir: SignalDirection) -> Option<(String, String)> {
        let m = self.market(asset)?;
        let token = match dir {
            SignalDirection::Up => m.up_token_id.clone(),
            SignalDirection::Down => m.down_token_id.clone(),
        };
        if token.is_empty() {
            None
        } else {
            Some((token, m.condition_id.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn slot_and_round_state_math() {
        let s = Scanner::new(ScannerConfig { round_duration_sec: 900, ..Default::default() });
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
        let mut s = Scanner::new(ScannerConfig { round_duration_sec: 900, ..Default::default() });
        let now = 1_000_000i64;
        // Local expected end for slot 1 = 1_800_000; the venue says 1_800_500.
        s.observe_end_time(1_800_500, now);
        assert_eq!(s.clock_offset_ms(), 500);
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
            neg_risk: true,
            question: "".into(),
        }]);
        // age 100s, time_left 800s → ok
        assert!(s.can_trade(1_000_000).is_ok());
        // Very close to expiry → blocked.
        assert!(s.can_trade(1_799_900).is_err());
    }
}
