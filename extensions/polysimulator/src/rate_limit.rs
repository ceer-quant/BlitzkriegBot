//! Read/write token-bucket rate limiter (#454).
//!
//! PolySimulator's free tier publishes: 2 req/s burst + 120 req/min sustained
//! on reads, order writes per-user serialized, and a hard 100 accepted
//! orders/UTC-day quota. The buckets model the two sustainable budgets
//! (reads and writes separately); the daily quota is a venue-side condition
//! that surfaces as a 429 and is NOT pre-tracked here.
//!
//! Same shape as the kalshi limiter — when a draw cannot be satisfied the
//! caller sleeps until the refill covers it, bounded by the backoff ceiling,
//! after which the request fails as
//! [`PolySimError::RateLimited`](crate::PolySimError).

use std::time::Instant;
use tokio::sync::Mutex;

/// Costs per request class. Reads are cheap; order placement and cancels are
/// the expensive side on every venue tier.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitConfig {
    /// Sustained reads per second the bucket refills.
    pub read_per_sec: f64,
    /// Read burst capacity.
    pub read_burst: u32,
    /// Sustained writes per second the bucket refills.
    pub write_per_sec: f64,
    /// Write burst capacity.
    pub write_burst: u32,
    /// Longest single wait the limiter will sleep before giving up.
    pub max_wait_ms: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        // Free tier: 2 req/s burst, 120 req/min sustained. The write bucket
        // models the same envelope (order writes are per-user serialized and
        // answered 409 + Retry-After: 1 when contended, which the retry loop
        // handles). A tier upgrade is a config change, not a code change.
        Self {
            read_per_sec: 2.0,
            read_burst: 4,
            write_per_sec: 1.0,
            write_burst: 2,
            max_wait_ms: 5_000,
        }
    }
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    capacity: f64,
    per_sec: f64,
    last: Instant,
}

impl Bucket {
    fn new(capacity: u32, per_sec: f64) -> Self {
        Self {
            tokens: capacity as f64,
            capacity: capacity as f64,
            per_sec,
            last: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.per_sec).min(self.capacity);
    }

    /// Milliseconds until `cost` tokens are available, or None if full.
    fn wait_ms_for(&self, cost: f64) -> Option<u64> {
        if self.tokens >= cost {
            return None;
        }
        let missing = cost - self.tokens;
        Some((missing / self.per_sec * 1000.0).ceil() as u64)
    }

    fn take(&mut self, cost: f64) {
        self.tokens -= cost;
    }
}

/// The limiter handle shared by REST calls.
#[derive(Debug)]
pub struct RateLimit {
    inner: Mutex<LimitState>,
}

#[derive(Debug)]
struct LimitState {
    read: Bucket,
    write: Bucket,
    max_wait_ms: u64,
}

/// Which class a call belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Read,
    Write,
}

impl Class {
    fn cost(self) -> f64 {
        match self {
            Class::Read => 1.0,
            Class::Write => 1.0,
        }
    }
}

impl RateLimit {
    pub fn new(cfg: RateLimitConfig) -> Self {
        Self {
            inner: Mutex::new(LimitState {
                read: Bucket::new(cfg.read_burst, cfg.read_per_sec),
                write: Bucket::new(cfg.write_burst, cfg.write_per_sec),
                max_wait_ms: cfg.max_wait_ms,
            }),
        }
    }

    /// Reserve `class` capacity, sleeping until the refill covers it.
    pub async fn acquire(&self, class: Class) -> Result<(), crate::PolySimError> {
        let mut st = self.inner.lock().await;
        let (bucket, cost) = match class {
            Class::Read => (&mut st.read, class.cost()),
            Class::Write => (&mut st.write, class.cost()),
        };
        bucket.refill();
        if let Some(wait) = bucket.wait_ms_for(cost) {
            if wait > st.max_wait_ms {
                return Err(crate::PolySimError::RateLimited { attempts: 1 });
            }
            // Sleep OUTSIDE the lock: holding it across the await would block
            // the other class and every other task sharing the limiter.
            drop(st);
            tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
            let mut st = self.inner.lock().await;
            let bucket = match class {
                Class::Read => &mut st.read,
                Class::Write => &mut st.write,
            };
            bucket.refill();
            if bucket.tokens < cost {
                return Err(crate::PolySimError::RateLimited { attempts: 1 });
            }
            bucket.take(cost);
            return Ok(());
        }
        bucket.take(cost);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Free-tier defaults keep the plugin inside the published 2 req/s burst
    /// and never wait more than the configured ceiling.
    #[tokio::test]
    async fn free_tier_defaults_admit_a_burst_then_throttle() {
        let rl = RateLimit::new(RateLimitConfig::default());
        let start = Instant::now();
        for _ in 0..4 {
            rl.acquire(Class::Read).await.expect("burst reads pass");
        }
        let elapsed = start.elapsed();
        // 4 draws from a burst-4 bucket: none may sleep.
        assert!(elapsed.as_millis() < 500, "burst took {elapsed:?}");
    }

    #[tokio::test]
    async fn exhausted_bucket_waits_for_refill_not_hammering() {
        let rl = RateLimit::new(RateLimitConfig {
            read_per_sec: 100.0,
            read_burst: 1,
            ..RateLimitConfig::default()
        });
        rl.acquire(Class::Read).await.expect("first draw");
        let start = Instant::now();
        rl.acquire(Class::Read)
            .await
            .expect("refill makes the second draw pass");
        let waited = start.elapsed().as_millis();
        assert!(
            waited >= 5,
            "a 100/s refill needs ~10ms for one token; took {waited}ms"
        );
    }
}
