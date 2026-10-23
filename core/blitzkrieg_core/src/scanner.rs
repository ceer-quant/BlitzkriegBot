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
    /// Venue-reported actual round end (ms), keyed by **(venue, round duration
    /// sec)** — the multi-family clock key (B1a). A venue can run SEVERAL
    /// round-length families at once (PM 15m matches alongside PM 1h markets);
    /// each family's expiry declaration lands in its own (venue, duration)
    /// slot, so a 15m close must never freeze a 1h sibling's countdown.
    /// Multi-plugin data planes keep one clock per venue on top of that: a
    /// Kalshi declaration must not move the Polymarket expiry and vice versa.
    /// The empty-string venue is the legacy bucket — markets (or corpora)
    /// without a venue tag — and the legacy single-duration key is
    /// `(DEFAULT_VENUE, cfg.round_duration_sec)`, so old replays behave
    /// exactly as the per-venue-only clock did.
    actual_end_time_ms: HashMap<(String, i64), i64>,
    /// Venue time − local time (ms), keyed by VENUE only — the offset is the
    /// venue's clock property, not a family's: the same venue's wall clock
    /// serves every round duration it lists.
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
    ///
    /// 单族兼容（B1a 回放逐字节不变的关键）：这里 = `round_timing_for` 于
    /// `(venue, cfg.round_duration_sec)` 的同族折叠。经旧入口写路径
    /// （`observe_end_time_for_venue`）落表的所有观察都带 cfg 的 duration，
    /// 单族语料下同族 min ≡ 旧的全表 min —— 断言钉在
    /// `legacy_venue_entry_equals_pre_b1a_semantics`。
    pub fn round_timing_for_venue(&self, venue: &str, local_now_ms: i64) -> RoundTiming {
        self.round_timing_for(venue, self.cfg.round_duration_sec, local_now_ms)
    }

    /// The multi-family clock (B1a): the round timing of ONE (venue, duration)
    /// family. The slot grid comes from the FAMILY's duration
    /// (`duration` sec per slot, venue-offset adjusted), the family's own
    /// declaration is authoritative, and the conservative fallback folds ONLY
    /// same-duration observations — a PM 1h market's time_left must never be
    /// pinned to 0 by a kalshi 15m sibling's expiry (cross-family decay is
    /// not contagion).
    pub fn round_timing_for(
        &self,
        venue: &str,
        duration_sec: i64,
        local_now_ms: i64,
    ) -> RoundTiming {
        let offset = self.clock_offset_ms.get(venue).copied().unwrap_or(0);
        let venue_now = local_now_ms + offset;
        let slot = venue_now / 1000 / duration_sec;
        let grid = (slot + 1) * duration_sec * 1000;
        // Same-family fold only: the family key carries the duration, so a
        // sibling family's (much nearer) expiry never gates this clock. A
        // venue with no own declaration reads the earliest same-duration
        // expiry — the conservative shared window (a round is tradeable only
        // while every same-duration venue's market is still open); with no
        // declaration at all in the family, the family's own grid. Single-
        // family deployments fold the whole table, byte-identical with the
        // pre-B1a `round_timing_for_venue` behaviour.
        let earliest_in_family = self
            .actual_end_time_ms
            .iter()
            .filter(|((_, d), t)| *d == duration_sec && **t > 0)
            .map(|(_, t)| *t)
            .min();
        let effective = match self
            .actual_end_time_ms
            .get(&(venue.to_string(), duration_sec))
        {
            // The venue's own declaration (in this family) is authoritative
            // for its markets.
            Some(t) if *t > 0 => *t,
            // No own declaration: the conservative family clock — the earliest
            // observed expiry across same-duration venues, or the family grid
            // when none has spoken.
            _ => earliest_in_family.unwrap_or(grid),
        };
        let time_left = ((effective - local_now_ms) / 1000).max(0);
        RoundTiming {
            slot,
            expires_at_ms: effective,
            age_sec: duration_sec - time_left,
            time_left_sec: time_left,
        }
    }

    /// The (venue, duration) clocks that have SPOKEN (an in-band expiry
    /// declaration landed in their slot), as ordered unique pairs — the key
    /// set of `round_timing_for`. The per-venue rows the panel renders walk
    /// this in B1a's wire assembly: one row per family, `durationSec` carried.
    ///
    /// 排序确定（插入序不稳定）：legacy venue（空串）恒排最前，其余按
    /// venue 名字典序、同 venue 内按 duration 升序 —— 门禁可逐字节断言
    /// 行序。`observed_venue_keys`（旧口径）从这里派生 unique venues。
    pub fn observed_clock_keys(&self) -> Vec<(String, i64)> {
        let mut keys: Vec<(String, i64)> = self
            .actual_end_time_ms
            .iter()
            .filter(|(_, t)| **t > 0)
            .map(|((v, d), _)| (v.clone(), *d))
            .collect();
        keys.sort_by(|a, b| {
            if a.0.is_empty() {
                if b.0.is_empty() {
                    a.1.cmp(&b.1)
                } else {
                    std::cmp::Ordering::Less
                }
            } else if b.0.is_empty() {
                std::cmp::Ordering::Greater
            } else {
                a.cmp(b)
            }
        });
        keys
    }

    /// The venues that have SPOKEN (an in-band expiry declaration landed in
    /// their clock slot) plus the legacy bucket when it holds a value — the
    /// key set of `round_timing_for_venue`. Read-only views (`engine.round`
    /// 的 per-venue 轮次行) walk this instead of guessing: a venue without a
    /// declaration has no authoritative clock of its own, and inventing one
    /// for the panel would contradict the conservative window the trading
    /// side actually consumes.
    ///
    /// 排序确定（插入序不稳定，BTreeMap 没用上）：legacy bucket（空串）
    /// 恒排最前，其余按 venue 名字典序 —— 门禁可逐字节断言行序。
    pub fn observed_venue_keys(&self) -> Vec<String> {
        let mut venues: Vec<String> = Vec::new();
        for (v, _) in self.observed_clock_keys() {
            if !venues.contains(&v) {
                venues.push(v);
            }
        }
        venues
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
    ///
    /// B1a 兼容委托：duration 取 cfg 的全局轮长 —— 旧调用方（engine.rs
    /// 的 merge/rollover 观察点）零改动可编译，写入的键落在
    /// `(venue, cfg.round_duration_sec)`，即旧单族语义的精确化身。多族
    /// 写入走 [`Self::observe_end_time_for`]。
    pub fn observe_end_time_for_venue(&mut self, venue: &str, end_ms: i64, local_now_ms: i64) {
        self.observe_end_time_for(venue, self.cfg.round_duration_sec, end_ms, local_now_ms);
    }

    /// Record the venue-reported end time into the (venue, duration) family
    /// slot — the multi-family write path (B1a). The guard semantics are the
    /// per-venue path's, verbatim: non-positive declarations ignored, and a
    /// declaration more than two rounds (of THIS family's duration) out in
    /// either direction REFUSED rather than adopted, so a discovery bug or a
    /// long-stale timestamp cannot hijack the family clock. The clock offset
    /// stays keyed by venue alone — it is the venue's wall-clock property,
    /// shared by every family the venue runs.
    pub fn observe_end_time_for(
        &mut self,
        venue: &str,
        duration_sec: i64,
        end_ms: i64,
        local_now_ms: i64,
    ) {
        if end_ms <= 0 || duration_sec <= 0 {
            return;
        }
        let round_ms = duration_sec * 1000;
        if (end_ms - local_now_ms).abs() > 2 * round_ms {
            return;
        }
        self.actual_end_time_ms
            .insert((venue.to_string(), duration_sec), end_ms);
        let local_slot = (local_now_ms / 1000) / duration_sec;
        let local_expected_end = (local_slot + 1) * duration_sec * 1000;
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
    fn observed_venue_keys_lists_who_spoke_legacy_first_and_deterministically() {
        // #550（症状②）的口径来源：per-venue 轮次行只列「已经开口」的
        // venue —— legacy bucket（空串）恒排最前，其余字典序；未开口的
        // venue（没有 in-band 声明）绝不出现，显示链不发明 trading 侧
        // 不消费的钟。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        assert!(
            s.observed_venue_keys().is_empty(),
            "no declaration → no venue spoke → no rows"
        );
        s.observe_end_time_for_venue("kalshi", 1_800_500, now);
        s.observe_end_time_for_venue("polymarket", 1_799_800, now);
        assert_eq!(
            s.observed_venue_keys(),
            vec!["kalshi".to_string(), "polymarket".to_string()],
            "non-legacy keys are lexicographic"
        );
        // The legacy bucket (empty venue) sorts FIRST when it holds a value.
        s.observe_end_time_for_venue("", 1_800_200, now);
        assert_eq!(
            s.observed_venue_keys(),
            vec![
                String::new(),
                "kalshi".to_string(),
                "polymarket".to_string()
            ],
            "the legacy bucket leads the list"
        );
        // An out-of-band (refused) declaration does not make a venue "speak".
        s.observe_end_time_for_venue("ghost", now + 10 * 365 * 86_400_000, now);
        assert!(
            !s.observed_venue_keys().contains(&"ghost".to_string()),
            "a refused declaration must not open a clock slot"
        );
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
                .get(&(DEFAULT_VENUE.to_string(), 900))
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

    // ── B1a：多族 (venue, duration) 时钟键 ──────────────────────────────

    #[test]
    fn cross_family_clocks_get_their_own_slot_and_expiry() {
        // 同一时刻（无任何观察、无偏移）两个族各推各的 slot/expiry：
        // 900s 族与 3600s 族在 now = 1_000_000（slot 边界 900_000 与
        // 3_600_000 的整倍数相差甚远）必须得出不同的 slot 与到期，证明
        // slot 网格跟族走，不再全局一根尺。
        let s = Scanner::new(ScannerConfig {
            round_duration_sec: 3600,
            ..Default::default()
        });
        let now = 1_000_000i64;
        let f15 = s.round_timing_for("polymarket", 900, now);
        let f1h = s.round_timing_for("polymarket", 3600, now);
        // 900s grid: slot 1, expiry 1_800_000。3600s grid: slot 0, expiry 3_600_000。
        assert_eq!(f15.slot, 1);
        assert_eq!(f15.expires_at_ms, 1_800_000);
        assert_eq!(f15.time_left_sec, 800);
        assert_eq!(f15.age_sec, 100);
        assert_eq!(f1h.slot, 0);
        assert_eq!(f1h.expires_at_ms, 3_600_000);
        assert_eq!(f1h.time_left_sec, 2_600);
        assert_eq!(f1h.age_sec, 1_000);
        assert_ne!(f15.slot, f1h.slot);
        assert_ne!(f15.expires_at_ms, f1h.expires_at_ms);
    }

    #[test]
    fn a_15m_observation_never_pollutes_a_1h_clock() {
        // 族内折叠隔离证明：kalshi@900 的到期声明落表后，polymarket@3600
        // 在「自己没有声明」时的兜底折叠只扫同 duration 键 —— 15m 的
        // observed expiry（近得多）不得把 1h 的 time_left 钉死。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        // A kalshi 15m declaration, in-band for the 900s family.
        s.observe_end_time_for("kalshi", 900, 1_800_500, now);
        // A polymarket 1h declaration too — the 1h family has its OWN voice.
        s.observe_end_time_for("polymarket", 3600, 3_600_200, now);

        // PM@3600 reads its OWN declaration (authoritative per family).
        let pm_1h = s.round_timing_for("polymarket", 3600, now);
        assert_eq!(
            pm_1h.expires_at_ms, 3_600_200,
            "own family declaration wins"
        );
        assert_eq!(pm_1h.time_left_sec, 2_600);

        // A 1h venue that has NOT spoken folds ONLY same-duration clocks:
        // the 15m expiry (1_800_500) must not appear in its fold.
        let other_1h = s.round_timing_for("kalshi", 3600, now);
        assert_eq!(
            other_1h.expires_at_ms, 3_600_200,
            "the 1h fold sees the only 1h declaration, never the 15m one"
        );

        // And the 15m family keeps its own clock despite the 1h presence.
        let k15 = s.round_timing_for("kalshi", 900, now);
        assert_eq!(k15.expires_at_ms, 1_800_500);
        assert_eq!(k15.time_left_sec, 800);
    }
    #[test]
    fn family_fallback_folds_within_the_same_duration_only() {
        // 兜底 earliest_observed 折叠的同族性：唯一声明在 15m 族，1h 族
        // 无任何声明 → 1h 兜底走自己的网格（不是 15m 的 observed）。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        s.observe_end_time_for("kalshi", 900, 1_800_300, now);
        let silent_1h = s.round_timing_for("polymarket", 3600, now);
        assert_eq!(
            silent_1h.expires_at_ms, 3_600_000,
            "a family with no declarations falls back to ITS OWN grid, never a sibling family's expiry"
        );
        assert_eq!(
            silent_1h.slot, 0,
            "the 1h slot grid runs on the family duration"
        );
        // The 15m family's own fold still sees the declaration.
        let fold_15 = s.round_timing_for("polymarket", 900, now);
        assert_eq!(
            fold_15.expires_at_ms, 1_800_300,
            "same-family fold reaches the sibling's declaration"
        );
    }

    #[test]
    fn legacy_venue_entry_equals_pre_b1a_semantics() {
        // 回放逐字节不变的关键证明：多 venue 单族观察表下，旧入口
        // `round_timing_for_venue` 的结果与升级前语义一致。升级前：
        // slot = current_slot（cfg 网格 + venue 偏移），折叠 = 全表
        // `>0` min（升级前全表 = 单族表，同族折叠 ≡ 全表 min），own
        // declaration 权威。这些期望值是升级前语义的手工演算（同一
        // 公式），不是改动后的输出。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 900_000i64;
        s.observe_end_time_for_venue("polymarket", 1_800_500, now);
        s.observe_end_time_for_venue("kalshi", 1_799_800, now);
        s.observe_end_time_for_venue("", 1_800_200, now);

        // Own declaration authoritative; offset-derived slot grid unchanged.
        let pm = s.round_timing_for_venue("polymarket", now);
        assert_eq!(pm.slot, 1, "offset-adjusted slot: (900_000 + 500ms) / 900s");
        assert_eq!(pm.expires_at_ms, 1_800_500);
        assert_eq!(
            pm.time_left_sec, 900,
            "(1_800_500 − 900_000)ms / 1000, floored"
        );
        assert_eq!(pm.age_sec, 0, "age = duration − time_left");

        let k = s.round_timing_for_venue("kalshi", now);
        assert_eq!(k.expires_at_ms, 1_799_800);
        assert_eq!(k.time_left_sec, 899);

        // A venue with no voice folds the whole single-family table's min.
        let ghost = s.round_timing_for_venue("ghost", now);
        assert_eq!(
            ghost.expires_at_ms, 1_799_800,
            "single-family fold ≡ the pre-B1a whole-table min"
        );

        // The same values through the new keyed entry — the legacy entry IS
        // the (venue, cfg.duration) projection.
        let pm2 = s.round_timing_for("polymarket", 900, now);
        assert_eq!(pm.slot, pm2.slot);
        assert_eq!(pm.expires_at_ms, pm2.expires_at_ms);
        assert_eq!(pm.time_left_sec, pm2.time_left_sec);

        // The global entry (`round_timing`, legacy bucket) is byte-untouched:
        // the empty-venue clock folds the same single-family table.
        let global = s.round_timing(now);
        assert_eq!(global.slot, 1);
        assert_eq!(global.expires_at_ms, 1_800_200);
        assert_eq!(global.time_left_sec, 900);
    }

    #[test]
    fn observe_end_time_for_writes_family_keys_and_keeps_offset_per_venue() {
        // 写路径：多族声明各落各族键；偏移仍按 venue 一份（后来族的声明
        // 刷新同一 venue 的偏移，不另立族表）。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        s.observe_end_time_for("polymarket", 900, 1_800_500, now);
        s.observe_end_time_for("polymarket", 3600, 3_600_900, now);

        // The 900s family stores its value under the family key (visible
        // through the read path; the map is private, so assert by clock).
        assert_eq!(
            s.round_timing_for("polymarket", 900, now).expires_at_ms,
            1_800_500
        );
        assert_eq!(
            s.round_timing_for("polymarket", 3600, now).expires_at_ms,
            3_600_900
        );

        // Offset stays per venue: the LAST in-band declaration refreshed the
        // same (single) offset entry — 3_600_900 − local expected 1h end.
        // Local expected end for the 3600s family at now = (1_000_000/1000/
        // 3600 + 1) * 3600 * 1000 = 3_600_000 → offset 900.
        assert_eq!(s.clock_offset_ms(), 900);

        // Guard semantics copied verbatim: non-positive refused.
        s.observe_end_time_for("polymarket", 900, 0, now);
        assert_eq!(
            s.round_timing_for("polymarket", 900, now).expires_at_ms,
            1_800_500,
            "0 refused"
        );
        // Out-of-band (this family's own 2-round band) refused: the ghost's
        // own key stays absent, and its read falls back to the conservative
        // same-family clock — the polymarket declaration that IS in band.
        s.observe_end_time_for("ghost", 900, now + 10 * 365 * 86_400_000, now);
        assert_eq!(
            s.round_timing_for("ghost", 900, now).expires_at_ms,
            1_800_500,
            "the refused declaration leaves no ghost key; the family fold reads the in-band sibling"
        );
        // A non-positive duration is a malformed call — ignored wholesale.
        s.observe_end_time_for("polymarket", 0, 1_800_500, now);
        assert!(
            !s.observed_clock_keys()
                .iter()
                .any(|(v, d)| v == "polymarket" && *d == 0),
            "duration ≤ 0 never opens a family key"
        );
    }

    #[test]
    fn observed_clock_keys_dedupe_and_order_deterministically() {
        // 去重保序：legacy venue 恒最前；其余按 (venue, duration) 字典序；
        // 同 (venue,duration) 只一行（去重）；未开口的 venue/族绝不出现。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        assert!(
            s.observed_clock_keys().is_empty(),
            "no declaration → no rows"
        );
        let now = 1_000_000i64;
        s.observe_end_time_for("polymarket", 3600, 3_600_500, now);
        s.observe_end_time_for("kalshi", 900, 1_800_500, now);
        s.observe_end_time_for("kalshi", 3600, 3_600_200, now);
        s.observe_end_time_for("kalshi", 900, 1_800_600, now); // refresh, not a new key
        s.observe_end_time_for("", 900, 1_800_200, now);
        assert_eq!(
            s.observed_clock_keys(),
            vec![
                (String::new(), 900),
                ("kalshi".to_string(), 900),
                ("kalshi".to_string(), 3600),
                ("polymarket".to_string(), 3600),
            ],
            "legacy first, then (venue, duration) lexicographic, deduped"
        );
        // A refused declaration never opens a key.
        s.observe_end_time_for("ghost", 900, now + 10 * 365 * 86_400_000, now);
        assert!(!s.observed_clock_keys().iter().any(|(v, _)| v == "ghost"));
        // The legacy venue list derives from the clock keys, order intact.
        assert_eq!(
            s.observed_venue_keys(),
            vec![
                String::new(),
                "kalshi".to_string(),
                "polymarket".to_string()
            ],
            "unique venues in the clock-key order"
        );
    }

    #[test]
    fn out_of_band_family_declarations_are_refused_per_family_band() {
        // 守卫带按族轮长算：3600s 族的 ±2 轮 = ±7200s，900s 族拒收的
        // 30 分钟外声明在 1h 族 in-band。
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            ..Default::default()
        });
        let now = 1_000_000i64;
        // 3_600_000ms out: refused by the 900s family (band ±1_800_000)…
        s.observe_end_time_for("kalshi", 900, now + 3_600_000, now);
        assert!(
            !s.observed_clock_keys()
                .contains(&("kalshi".to_string(), 900)),
            "outside the 15m family's ±2-round band"
        );
        // …but in-band for the 3600s family (band ±7_200_000).
        s.observe_end_time_for("kalshi", 3600, now + 3_600_000, now);
        assert_eq!(
            s.round_timing_for("kalshi", 3600, now).expires_at_ms,
            now + 3_600_000,
            "the same declaration is inside the 1h family's band"
        );
    }
}
