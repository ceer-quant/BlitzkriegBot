//! Read/write token-bucket rate limiter for the predict.fun client (#423).
//!
//! The venue publishes no per-tier numbers, so the defaults are conservative
//! (10 reads/s, 5 writes/s — matched to the Kalshi client's baseline): a
//! measured tier is a config change, not a code change. Reads (market data,
//! portfolio GETs) and writes (orders) draw from separate capacities and
//! refill separately. When a draw cannot be satisfied the caller sleeps until
//! the refill covers it — bounded by the configured ceiling, after which the
//! request fails as [`PredictError::RateLimited`](crate::PredictError)
//! instead of queueing unboundedly.

use std::time::Instant;
use tokio::sync::Mutex;

/// Costs per request class. Reads are cheap; order placement and cancels are
/// the expensive side on every venue.
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
        Self {
            read_per_sec: 10.0,
            read_burst: 20,
            write_per_sec: 5.0,
            write_burst: 10,
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

/// The limiter handle shared by REST and WS-facing calls.
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
            // One request consumes one token from its own bucket; the ratio
            // lives in the per-class rates, not in a per-request cost.
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
    pub async fn acquire(&self, class: Class) -> Result<(), crate::PredictError> {
        let mut st = self.inner.lock().await;
        let (bucket, cost) = match class {
            Class::Read => (&mut st.read, class.cost()),
            Class::Write => (&mut st.write, class.cost()),
        };
        bucket.refill();
        if let Some(wait) = bucket.wait_ms_for(cost) {
            if wait > st.max_wait_ms {
                return Err(crate::PredictError::RateLimited { attempts: 1 });
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
                return Err(crate::PredictError::RateLimited { attempts: 1 });
            }
            bucket.take(cost);
            return Ok(());
        }
        bucket.take(cost);
        Ok(())
    }
}
