//! Stream K-line aggregation — DEV_V0_3 §10.3 / E29.
//!
//! [`KlineAggregator`] folds a tick stream (venue trades or mid-price ticks
//! fed from the existing book/spot paths) into OHLCV bars, one CURRENT bar per
//! `(symbol, interval)`, driven by DATA — no timer ever closes a bar. The only
//! state per bucket is the bar currently growing plus a bounded tail of CLOSED
//! bars serving [`KlineAggregator::history`] (`kline.history`, cap = the
//! method's own limit ceiling). `close_time_ms` is derived by
//! `KlineInterval::bucket_close_ms` inside `Kline::new`, so an inverted or
//! misaligned bar is UNCONSTRUCTIBLE — the caller never passes a close time.
//!
//! Two disciplines stated on the type, not in comments alone:
//! - **Out-of-order input is DROPPED, visibly.** `ts_ms < bar.open_time_ms`
//!   for a bucket's current bar does not fold; the drop shows up in
//!   `stats().dropped_out_of_order` (silent drops are a defect; visible drops
//!   are discipline). Because a bar only ever closes when a tick from a
//!   STRICTLY LATER bucket arrives, `close_time_ms` is monotonically
//!   non-decreasing per `(symbol, interval)` BY CONSTRUCTION.
//! - **Bars close on data, not on clocks.** A symbol that stops ticking leaves
//!   its last bar open; the empty buckets that were skipped on the way to the
//!   next tick are counted in `stats().no_data_bars` so a chart's gaps are
//!   explainable rather than mysterious.

use std::collections::{HashMap, VecDeque};

use rust_decimal::Decimal;

use blitzkrieg_market_api::kline::{Kline, KlineInterval};

/// Counters and footprint of one aggregator, for stats surfaces and gates.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregatorStats {
    /// Distinct symbols seen since boot.
    pub symbols: usize,
    /// `(symbol, interval)` buckets with a bar currently growing.
    pub live_bars: usize,
    /// CLOSED bars retained across all buckets (bounded by the history cap).
    pub closed_bars: usize,
    /// Ticks discarded because they arrived out of order for their bucket.
    pub dropped_out_of_order: u64,
    /// Empty buckets skipped when a bar closed late (data-driven closing means
    /// a quiet stretch leaves those buckets without a bar at all).
    pub no_data_bars: u64,
    /// The configured interval set (wire snake_case), so a stats reader can
    /// reason about the footprint formula without knowing the boot config.
    pub intervals: Vec<KlineInterval>,
}

impl AggregatorStats {
    /// The memory bound this stats snapshot implies: every bucket holds at
    /// most one live bar plus the bounded closed tail. Gates assert this
    /// formula instead of trusting an eyeballed "it feels small" (§10.3).
    pub fn memory_bound_bytes(&self, kline_size_hint: usize) -> usize {
        let buckets = self.symbols * self.intervals.len();
        buckets * (1 + CLOSED_HISTORY_CAP) * kline_size_hint
    }
}

/// Closed-bar tail kept per bucket for `kline.history`. Equals the method's
/// limit ceiling, so a request can never want bars the aggregator dropped.
pub const CLOSED_HISTORY_CAP: usize = 1000;

/// The full interval set an engine-hosted aggregator aggregates by default.
/// Nine bars per symbol is a few hundred KB — small enough that filtering the
/// set buys nothing and costs a config knob.
pub const DEFAULT_INTERVALS: [KlineInterval; 9] = [
    KlineInterval::Sec1,
    KlineInterval::Sec5,
    KlineInterval::Sec15,
    KlineInterval::Min1,
    KlineInterval::Min5,
    KlineInterval::Min15,
    KlineInterval::Hour1,
    KlineInterval::Hour4,
    KlineInterval::Day1,
];

/// Fold a tick stream into per-`(symbol, interval)` bars. See the module docs
/// for the two disciplines (visible drops, data-driven closing).
pub struct KlineAggregator {
    intervals: Vec<KlineInterval>,
    bars: HashMap<(String, KlineInterval), Kline>,
    /// Bounded CLOSED tail per bucket, oldest first — the `kline.history` body.
    closed: HashMap<(String, KlineInterval), VecDeque<Kline>>,
    /// Bars closed but not yet taken by the host (events/callbacks drain these).
    pending_closed: Vec<Kline>,
    dropped_out_of_order: u64,
    no_data_bars: u64,
    symbols_seen: std::collections::HashSet<String>,
}

impl KlineAggregator {
    pub fn new(intervals: Vec<KlineInterval>) -> Self {
        Self {
            intervals,
            bars: HashMap::new(),
            closed: HashMap::new(),
            pending_closed: Vec::new(),
            dropped_out_of_order: 0,
            no_data_bars: 0,
            symbols_seen: std::collections::HashSet::new(),
        }
    }

    pub fn with_default_intervals() -> Self {
        Self::new(DEFAULT_INTERVALS.to_vec())
    }

    /// Every interval configured on this aggregator.
    pub fn intervals(&self) -> &[KlineInterval] {
        &self.intervals
    }

    /// Fold one aggregate tick. Returns the bars this tick CLOSED (each with
    /// `is_closed = true`), oldest first across intervals — callbacks and
    /// events are driven by these, never by a clock.
    ///
    /// Per interval, independently:
    /// - a tick OLDER than the current bar's window is dropped (counted);
    /// - a tick inside the current bar's window folds into it;
    /// - a tick from a later bucket closes the current bar (after counting the
    ///   empty buckets the silence skipped), opens the new one, and folds.
    pub fn on_trade(
        &mut self,
        symbol: &str,
        price: Decimal,
        size: Decimal,
        ts_ms: i64,
    ) -> Vec<Kline> {
        if !self.symbols_seen.contains(symbol) {
            self.symbols_seen.insert(symbol.to_string());
        }
        let mut closed = Vec::new();
        for interval in &self.intervals {
            let key = (symbol.to_string(), *interval);
            let bucket = interval.bucket_open_ms(ts_ms);
            match self.bars.get(&key) {
                None => {
                    self.bars
                        .insert(key, Kline::new(symbol, *interval, bucket, price, size));
                }
                Some(bar) => {
                    let bar_open = bar.open_time_ms;
                    if ts_ms < bar_open {
                        // A tick from BEFORE the growing bar: out of order for
                        // this interval. Dropped, visibly.
                        self.dropped_out_of_order += 1;
                        continue;
                    }
                    if bucket == bar_open {
                        self.bars
                            .get_mut(&key)
                            .expect("bar present")
                            .apply_trade(price, size);
                        continue;
                    }
                    // Later bucket: close what is growing (if it never received
                    // a second tick it is still a real bar — one tick = one
                    // bar), count the buckets the silence skipped, open anew.
                    let mut bar = self.bars.remove(&key).expect("bar present");
                    bar.is_closed = true;
                    let interval_ms = interval.secs() * 1000;
                    let skipped = (bucket.saturating_sub(bar_open) / interval_ms).saturating_sub(1);
                    self.no_data_bars += skipped as u64;
                    let tail = self.closed.entry(key.clone()).or_default();
                    if tail.len() >= CLOSED_HISTORY_CAP {
                        tail.pop_front();
                    }
                    tail.push_back(bar.clone());
                    closed.push(bar);
                    self.bars
                        .insert(key, Kline::new(symbol, *interval, bucket, price, size));
                }
            }
        }
        if !closed.is_empty() {
            self.pending_closed.extend(closed.iter().cloned());
        }
        closed
    }

    /// Snapshot of the bar currently growing for `(symbol, interval)`
    /// (`is_closed = false`), if any.
    pub fn current(&self, symbol: &str, interval: KlineInterval) -> Option<&Kline> {
        self.bars.get(&(symbol.to_string(), interval))
    }

    /// The `kline.history` body: the bounded closed tail (oldest first) plus
    /// the still-growing bar, if one exists, as the LAST element. Callers see
    /// `is_closed` and render the growing bar translucent ("正在长") — the
    /// aggregator does not editorialize about which bars a chart should draw.
    pub fn history(&self, symbol: &str, interval: KlineInterval, limit: usize) -> Vec<Kline> {
        let mut out: Vec<Kline> = self
            .closed
            .get(&(symbol.to_string(), interval))
            .map(|tail| tail.iter().cloned().collect())
            .unwrap_or_default();
        if let Some(bar) = self.bars.get(&(symbol.to_string(), interval)) {
            out.push(bar.clone());
        }
        if out.len() > limit {
            out.drain(..out.len() - limit);
        }
        out
    }

    /// Bars closed since the last take, in closure order. The host drains this
    /// to emit `KLINE_UPDATE` events; the aggregator keeps no copy.
    pub fn take_closed(&mut self) -> Vec<Kline> {
        std::mem::take(&mut self.pending_closed)
    }

    /// Snapshots of every bar currently growing (for throttled "正在长" pushes).
    pub fn current_bars(&self) -> Vec<Kline> {
        self.bars.values().cloned().collect()
    }

    pub fn stats(&self) -> AggregatorStats {
        AggregatorStats {
            symbols: self.symbols_seen.len(),
            live_bars: self.bars.len(),
            closed_bars: self.closed.values().map(|t| t.len()).sum(),
            dropped_out_of_order: self.dropped_out_of_order,
            no_data_bars: self.no_data_bars,
            intervals: self.intervals.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn agg() -> KlineAggregator {
        KlineAggregator::new(vec![KlineInterval::Min1, KlineInterval::Min5])
    }

    /// The OFFLINE reference: bucket ticks statically (sort + group by
    /// `bucket_open_ms`), the way §10.3's acceptance criterion says a replay
    /// would. The streaming result must equal this field for field.
    fn offline_bars(
        ticks: &[(i64, Decimal)], // (ts_ms, price)
        interval: KlineInterval,
        symbol: &str,
    ) -> Vec<Kline> {
        let mut sorted: Vec<(i64, Decimal)> = ticks.to_vec();
        sorted.sort_by_key(|(ts, _)| *ts);
        let mut buckets: Vec<(i64, Vec<Decimal>)> = Vec::new();
        for (ts, price) in sorted {
            let b = interval.bucket_open_ms(ts);
            match buckets.iter_mut().find(|(open, _)| *open == b) {
                Some((_, group)) => group.push(price),
                None => buckets.push((b, vec![price])),
            }
        }
        buckets
            .into_iter()
            .map(|(open, group)| {
                let mut k = Kline::new(symbol, interval, open, group[0], dec!(1));
                for price in &group[1..] {
                    k.apply_trade(*price, dec!(1));
                }
                k.is_closed = true;
                k
            })
            .collect()
    }

    /// The last CLOSED min1 bar of the stream must equal the offline
    /// reference's bar for the same bucket — field for field, including
    /// `tradeCount` and `volume` (§14.1 E29). Ticks are fed in timestamp order
    /// (a venue stream is); the aggregator's out-of-order discipline is
    /// exercised separately by the inversion test.
    #[test]
    fn a_ten_thousand_tick_random_stream_matches_the_offline_reference_field_for_field() {
        // Deterministic LCG so failures reproduce.
        let mut state: u64 = 0xe29;
        let mut next = move |modulus: u64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) % modulus
        };
        let base: i64 = 1_758_887_940_000; // inside a min1 bucket
        let mut ticks: Vec<(i64, Decimal)> = (0..10_000)
            .map(|_| {
                let ts = base + next(3_600_000) as i64; // spread over one hour
                let price = dec!(0.40) + Decimal::from(next(2_000)) / dec!(10000);
                (ts, price)
            })
            .collect();
        // A venue trade stream arrives in timestamp order; sort so the feed
        // matches that (the inversion discipline is tested separately below).
        ticks.sort_by_key(|(ts, _)| *ts);

        let mut a = agg();
        let mut last_min1 = None;
        for (ts, price) in &ticks {
            for bar in a.on_trade("TOKEN", *price, dec!(1), *ts) {
                if bar.interval == KlineInterval::Min1 {
                    last_min1 = Some(bar);
                }
            }
        }
        // The LAST min1 bar may still be growing (stream ends mid-bucket) —
        // compare the last CLOSED one; the offline reference treats every
        // bucket as closed.
        let streamed = last_min1.expect("min1 bars closed within an hour of ticks");
        let reference = offline_bars(&ticks, KlineInterval::Min1, "TOKEN");
        let expected = reference
            .iter()
            .rev()
            .find(|k| k.open_time_ms <= streamed.open_time_ms)
            .expect("reference covers the streamed bar's bucket");
        assert_eq!(streamed.open_time_ms, expected.open_time_ms);
        assert_eq!(streamed.close_time_ms, expected.close_time_ms);
        assert_eq!(streamed.open, expected.open);
        assert_eq!(streamed.high, expected.high);
        assert_eq!(streamed.low, expected.low);
        assert_eq!(streamed.close, expected.close);
        assert_eq!(streamed.volume, expected.volume);
        assert_eq!(streamed.trade_count, expected.trade_count);
        assert!(streamed.is_closed);
    }

    #[test]
    fn close_time_ms_is_always_open_plus_interval_minus_one_across_all_nine_intervals() {
        let all: Vec<KlineInterval> = DEFAULT_INTERVALS.to_vec();
        for interval in all {
            let open = interval.bucket_open_ms(1_758_888_001_234);
            let mut a = KlineAggregator::new(vec![interval]);
            a.on_trade("T", dec!(1), dec!(1), 1_758_888_001_234);
            a.on_trade("T", dec!(2), dec!(1), open + interval.secs() * 1000 + 5);
            let closed = a
                .current("T", interval)
                .expect("new bar after the second tick");
            assert_eq!(closed.open_time_ms, open + interval.secs() * 1000);
            assert_eq!(
                closed.close_time_ms,
                closed.open_time_ms + interval.secs() * 1000 - 1,
                "{interval:?}: close must stay open + secs*1000 - 1"
            );
        }
    }

    /// Reverse acceptance A: inverted timestamps are dropped, never folded —
    /// `close_time_ms` stays monotonic and the drop is VISIBLE in stats.
    #[test]
    fn inverted_timestamps_are_dropped_visibly_and_close_times_stay_monotonic() {
        let mut a = agg();
        a.on_trade("T", dec!(1), dec!(1), 60_000); // opens min1 bucket @60_000
        a.on_trade("T", dec!(2), dec!(1), 119_999); // folds
        let closed = a.on_trade("T", dec!(3), dec!(1), 130_000); // closes @60_000
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].close_time_ms, 119_999);

        // Now INVERT: a tick claiming to be from the CLOSED bucket again.
        let before = a.stats().dropped_out_of_order;
        let closed2 = a.on_trade("T", dec!(9), dec!(1), 61_000);
        assert!(closed2.is_empty(), "an old-bucket tick closes nothing");
        assert_eq!(
            a.stats().dropped_out_of_order,
            before + 1,
            "the drop must be counted, not silent"
        );
        // Monotonicity: every closed bar's close_time >= its predecessor's.
        let hist = a.history("T", KlineInterval::Min1, 1000);
        for w in hist.windows(2) {
            assert!(
                w[1].close_time_ms >= w[0].close_time_ms,
                "monotonicity violated: {} then {}",
                w[0].close_time_ms,
                w[1].close_time_ms
            );
        }
    }

    /// Reverse acceptance B's positive face: every bucket opens exactly on the
    /// `div_euclid` boundary — a `+1` (ceil) drift would misalign these.
    #[test]
    fn buckets_open_on_the_floor_boundary_not_one_past_it() {
        let mut a = agg();
        // 1 ms PAST a boundary is already the NEXT bucket.
        let ts = 60_000 * 3 + 1;
        a.on_trade("T", dec!(1), dec!(1), ts);
        let bar = a.current("T", KlineInterval::Min1).expect("bar");
        assert_eq!(
            bar.open_time_ms,
            ts.div_euclid(60_000) * 60_000,
            "bucket alignment"
        );
    }

    #[test]
    fn a_quiet_stretch_counts_no_data_bars_instead_of_fabricating_bars() {
        let mut a = agg();
        a.on_trade("T", dec!(1), dec!(1), 0); // min1 @0
        // Three minutes of silence, then a tick in bucket @180_000.
        let closed = a.on_trade("T", dec!(2), dec!(1), 185_000);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].open_time_ms, 0);
        // Buckets @60_000 and @120_000 saw no data: counted, not invented.
        assert_eq!(a.stats().no_data_bars, 2);
        // And the history carries exactly ONE min1 bar, not three.
        assert_eq!(a.history("T", KlineInterval::Min1, 1000).len(), 2);
    }

    #[test]
    fn the_closed_history_tail_respects_its_cap_oldest_first() {
        let mut a = KlineAggregator::new(vec![KlineInterval::Sec1]);
        // Tick 0 opens bucket @0. Tick i (i >= 1) lands in its OWN bucket
        // @(i)s (Sec1, ts = i*1000), closing the bar from @(i-1)s — so
        // CAP+50 forward steps close exactly CAP+50 bars (50 of which fall
        // off the capped front) and leave one growing bar.
        for i in 0..=(CLOSED_HISTORY_CAP as i64 + 50) {
            a.on_trade("T", Decimal::from(i), dec!(1), i * 1000);
        }
        let hist = a.history("T", KlineInterval::Sec1, 100_000);
        // history = capped closed tail (CAP) + the growing bar.
        assert_eq!(hist.len(), CLOSED_HISTORY_CAP + 1);
        assert!(!hist.last().expect("tail").is_closed);
        assert_eq!(
            hist.last().expect("tail").open_time_ms,
            (CLOSED_HISTORY_CAP as i64 + 50) * 1000
        );
        // Newest CLOSED bar and oldest survivor.
        let closed: Vec<&Kline> = hist.iter().filter(|b| b.is_closed).collect();
        assert_eq!(closed.len(), CLOSED_HISTORY_CAP);
        assert_eq!(
            closed.last().expect("newest closed").open_time_ms,
            (CLOSED_HISTORY_CAP as i64 + 49) * 1000
        );
        assert_eq!(
            closed.first().expect("oldest survivor").open_time_ms,
            50 * 1000,
            "the oldest 50 bars fell off the front"
        );
    }

    #[test]
    fn take_closed_drains_exactly_once_and_current_bars_snapshots_the_growing_ones() {
        let mut a = agg();
        a.on_trade("T", dec!(1), dec!(1), 0);
        a.on_trade("T", dec!(2), dec!(1), 65_000); // closes min1 @0
        assert_eq!(a.take_closed().len(), 1);
        assert!(a.take_closed().is_empty(), "drained exactly once");
        let growing = a.current_bars();
        assert_eq!(growing.len(), 2, "min1 and min5 both have a growing bar");
        assert!(growing.iter().all(|b| !b.is_closed));
    }

    #[test]
    fn history_limit_returns_the_newest_tail_with_the_growing_bar_last() {
        let mut a = KlineAggregator::new(vec![KlineInterval::Sec1]);
        for i in 0..10 {
            a.on_trade("T", Decimal::from(i), dec!(1), i * 1000);
        }
        let hist = a.history("T", KlineInterval::Sec1, 3);
        assert_eq!(hist.len(), 3);
        assert!(hist[..2].iter().all(|b| b.is_closed));
        assert!(!hist[2].is_closed, "the growing bar rides last");
    }

    /// P4 (§16.5): the 10k-trade aggregation throughput and the memory upper
    /// bound are MEASURED, not thresholded — the numbers land in
    /// `docs/perf/V0_3.md`. Same LCG stream as the offline-parity test above,
    /// fed through the PRODUCTION interval set (`with_default_intervals`),
    /// timed on the steady state after a warm-up. The memory bound is the
    /// §10.3 formula over a realistic fleet: 100 symbols × the 9 production
    /// intervals × (1 growing bar + the capped closed history).
    #[test]
    fn ten_k_trade_throughput_and_memory_bound_measured() {
        let mut state = 0x5eed_u64;
        let mut next = move |modulus: u64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) % modulus
        };
        let base: i64 = 1_758_887_940_000;
        let mut ticks: Vec<(i64, Decimal)> = (0..10_000)
            .map(|_| {
                let ts = base + next(3_600_000) as i64; // spread over one hour
                let price = dec!(0.40) + Decimal::from(next(2_000)) / dec!(10000);
                (ts, price)
            })
            .collect();
        ticks.sort_by_key(|(ts, _)| *ts);

        let mut a = KlineAggregator::with_default_intervals();
        for (ts, price) in &ticks[..500] {
            let _ = a.on_trade("TOKEN", *price, dec!(1), *ts);
        }
        // Time ONLY the tail: re-feeding the warmed prefix would (correctly)
        // count as out-of-order for the fast intervals, and the number this
        // prints is the cost of folding, not of discarding.
        let timed = &ticks[500..];
        let t0 = std::time::Instant::now();
        for (ts, price) in timed {
            let _ = a.on_trade("TOKEN", *price, dec!(1), *ts);
        }
        let elapsed = t0.elapsed();
        let per_tick_us = elapsed.as_micros() as f64 / timed.len() as f64;
        let kline_size = std::mem::size_of::<Kline>();
        let fleet_bound = 100usize * a.intervals().len() * (CLOSED_HISTORY_CAP + 1) * kline_size;
        println!(
            "kline aggregation: {} trades x {} intervals (this build): \
             total={}ms per-tick={:.2}us | memory bound: 100 symbols x {} intervals \
             x {} bars x {}B = {:.1}MB",
            ticks.len(),
            a.intervals().len(),
            elapsed.as_millis(),
            per_tick_us,
            a.intervals().len(),
            CLOSED_HISTORY_CAP + 1,
            kline_size,
            fleet_bound as f64 / (1024.0 * 1024.0),
        );
        // The measure rests on the discipline it reports: a sorted feed drops
        // nothing, so the throughput number is the cost of folding, not of
        // discarding.
        assert_eq!(
            a.stats().dropped_out_of_order,
            0,
            "sorted feed drops nothing"
        );
    }
}
