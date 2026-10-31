use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

pub const DATA_API_BASE: &str = "https://data-api.polymarket.com";
pub const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";
pub const CLOB_API_BASE: &str = "https://clob.polymarket.com";
/// Runaway guard on total fills appended across every segment: 100k is far
/// past any single-wallet window we replay; past it the pull refuses rather
/// than spin forever against a paging bug.
pub const MAX_FILLS: i64 = 100_000;
/// The venue's own `offset` ceiling on `/trades` (probed live 2026-10-02:
/// `offset=10000` still answers, `offset=10500` errors with "max historical
/// trades offset of 10000 exceeded"). A segment never requests past it —
/// when a full page lands at the ceiling, the walk recurses on the window's
/// older half (`start → min_ts`), where offsets restart from 0.
pub const TRADES_OFFSET_CAP: i64 = 10_000;
/// #355: bounded concurrency for the metadata/price phases. The HttpFetcher
/// paces SENDS globally (one per `delay_ms`), so concurrency pipelines
/// round-trips behind the next send slot — it never raises the request rate;
/// it stops every fetch from serialising behind the previous response.
pub const META_CONCURRENCY: usize = 8;
pub const PRICE_CONCURRENCY: usize = 8;

// ── fetcher ─────────────────────────────────────────────────────────────────

/// Object-safe JSON GET. The indirection exists so the pull logic is tested
/// against a routing mock — the tests pin behaviour, never the network.
pub trait JsonFetcher: Send + Sync {
    fn get_json<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;
}

/// The real fetcher: rustls reqwest, polite inter-request delay, bounded
/// retries on 429/5xx (these APIs rate-limit; a pull of a big wallet is a
/// marathon, not a sprint).
pub struct HttpFetcher {
    client: reqwest::Client,
    delay_ms: u64,
    /// Global send schedule: the instant the NEXT request may go out. A
    /// serial walk paces per request; a concurrent walk pipelines (hiding
    /// round-trips) without ever raising the request rate above 1/delay_ms —
    /// the semaphore bounds what is in flight, this bounds what is SENT.
    next_send: std::sync::Mutex<Option<tokio::time::Instant>>,
}

impl HttpFetcher {
    pub fn new(delay_ms: u64) -> Self {
        Self {
            client: reqwest::Client::new(),
            delay_ms,
            next_send: std::sync::Mutex::new(None),
        }
    }

    /// Reserve the next send slot and wait for it. Locks only synchronously —
    /// no await under the lock, so concurrent callers serialise on the
    /// schedule, not on the lock.
    async fn pace(&self) {
        let when = {
            let mut g = self.next_send.lock().unwrap();
            let now = tokio::time::Instant::now();
            let when = match *g {
                Some(t) if t > now => t,
                _ => now,
            };
            *g = Some(when + std::time::Duration::from_millis(self.delay_ms));
            when
        };
        if when > tokio::time::Instant::now() {
            tokio::time::sleep_until(when).await;
        }
    }
}

impl JsonFetcher for HttpFetcher {
    fn get_json<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move {
            // Global pacing: concurrent callers take send slots 1/delay_ms
            // apart — pipelining hides round-trips, it never raises the
            // request rate.
            self.pace().await;
            let mut last = String::new();
            for attempt in 0..3 {
                if attempt > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(1000 << attempt)).await;
                }
                let resp = match self.client.get(url).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        last = format!("GET {url}: {e}");
                        continue;
                    }
                };
                let status = resp.status();
                let body = resp.text().await.map_err(|e| format!("read {url}: {e}"))?;
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                    last = format!("GET {url}: HTTP {status}");
                    continue;
                }
                if !status.is_success() {
                    let head: String = body.chars().take(200).collect();
                    return Err(format!("GET {url}: HTTP {status}: {head}"));
                }
                return serde_json::from_str(&body).map_err(|e| format!("decode {url}: {e}"));
            }
            Err(last)
        })
    }
}
