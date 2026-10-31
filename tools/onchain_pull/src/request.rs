use std::path::{Path, PathBuf};

use super::*;

// ── request / progress / outcome ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PullRequest {
    /// 0x… wallet address (the Data API's `user`).
    pub wallet: String,
    /// Inclusive window, MILLISECONDS.
    pub start_ms: i64,
    pub end_ms: i64,
    /// `Some("BTC")` filters to one asset's updown markets (slug prefix).
    pub asset: Option<String>,
    /// `Some("15m")` filters to one round duration (slug duration).
    pub market: Option<String>,
    /// Output root; `data/onchain` in production.
    pub out_dir: PathBuf,
    pub page_limit: i64,
    /// Where the fill universe comes from. Default [`FillSource::TradesApi`]
    /// keeps the #352 behaviour; [`FillSource::ActivityFile`] (#355) trusts a
    /// pre-fetched `/activity` export instead — the Data API's `/trades`
    /// endpoint answers with a TAKER-SIDE SUBSET of the wallet's fills
    /// (measured 2026-10-02: 17.6% of the same window's buy notional, with
    /// 98%+ row-key overlap), so a universe built from it silently drops
    /// ~82% of the rounds. The activity ledger is complete; its rows carry
    /// every field the converter reads (slug/conditionId/asset/size/price/
    /// timestamp).
    pub fill_source: FillSource,
}

/// The source of the pull's fill universe (see [`PullRequest::fill_source`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FillSource {
    /// Walk `GET /trades` live (the original #352 path: offset-paged,
    /// offset-cap-aware segmented walk, resumable).
    #[default]
    TradesApi,
    /// Read a pre-fetched `/activity` JSONL export (one row per line, same
    /// field names) as the complete fill ledger, write it through the
    /// canonical dedupe, and build the universe from it. `page_limit` is
    /// unused.
    ActivityFile(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullPhase {
    Trades,
    Markets,
    Prices,
    Convert,
    Done,
}

impl std::fmt::Display for PullPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            PullPhase::Trades => "trades",
            PullPhase::Markets => "markets",
            PullPhase::Prices => "prices",
            PullPhase::Convert => "convert",
            PullPhase::Done => "done",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone)]
pub struct PullProgress {
    pub phase: PullPhase,
    pub fetched: u64,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct PullOutcome {
    pub trades_path: PathBuf,
    pub events_path: PathBuf,
    pub manifest_path: PathBuf,
    pub trades: u64,
    pub events: u64,
    pub cache_hit: bool,
}

pub type ProgressCb<'a> = dyn Fn(&PullProgress) + Send + Sync + 'a;

pub(crate) fn report(cb: &ProgressCb<'_>, phase: PullPhase, fetched: u64, detail: String) {
    cb(&PullProgress {
        phase,
        fetched,
        detail,
    });
}

// ── paths / naming ──────────────────────────────────────────────────────────

fn stem(req: &PullRequest) -> String {
    // The filters are part of the request identity: an asset=BTC pull and an
    // unfiltered pull of the same window are different datasets, so they must
    // not answer each other's cache.
    let mut s = format!(
        "{}_{}_{}",
        req.wallet.trim_start_matches("0x").to_lowercase(),
        req.start_ms / 1000,
        req.end_ms / 1000
    );
    if let Some(a) = &req.asset {
        s.push_str(&format!("-{}", a.to_lowercase()));
    }
    if let Some(m) = &req.market {
        s.push_str(&format!("-{}", m.to_lowercase()));
    }
    if let FillSource::ActivityFile(p) = &req.fill_source {
        // The fill source is part of the request identity: an activity-ledger
        // pull and a /trades walk of the same window are different datasets
        // (~5x universe size) and must never answer each other's cache — and
        // the ledger's identity is its CONTENT, so a re-fetched export with
        // more history gets a fresh dataset instead of a stale cache hit.
        let tag = sha256_file(p)
            .map(|h| h[..12].to_string())
            .unwrap_or_else(|_| "missing".into());
        s.push_str(&format!("-act-{tag}"));
    }
    s
}

pub fn trades_path(req: &PullRequest) -> PathBuf {
    req.out_dir.join(format!("{}.trades.jsonl", stem(req)))
}
pub fn events_path(req: &PullRequest) -> PathBuf {
    req.out_dir.join(format!("{}.jsonl", stem(req)))
}
pub fn manifest_path(req: &PullRequest) -> PathBuf {
    req.out_dir.join(format!("{}.manifest.json", stem(req)))
}
pub(crate) fn raw_path(req: &PullRequest) -> PathBuf {
    req.out_dir.join(format!("{}.raw.jsonl", stem(req)))
}
pub(crate) fn condition_cache_path(out_dir: &Path, condition_id: &str) -> PathBuf {
    out_dir
        .join(".cache")
        .join("conditions")
        .join(format!("{condition_id}.json"))
}
pub(crate) fn price_cache_path(
    out_dir: &Path,
    token: &str,
    start_sec: i64,
    end_sec: i64,
) -> PathBuf {
    out_dir
        .join(".cache")
        .join("prices")
        .join(format!("{token}_{start_sec}_{end_sec}.json"))
}

/// Bump when the converter's OUTPUT SEMANTICS change in a way old cached
/// datasets must not inherit: the manifest pins this number, and a cache hit
/// with a different (or missing — pre-verdict archives) value re-converts
/// from the cached fills instead of answering stale.
///
/// #388: bumped to 2 — the converter now emits `cashflow` events from the
/// ledger's MAKER_REBATE / REWARD rows. A v1 dataset carries none, so
/// answering it from cache would replay the wallet's rebates blind forever.
pub(crate) const CONVERTER_VERSION: u64 = 2;

pub(crate) fn redemptions_path(req: &PullRequest) -> PathBuf {
    req.out_dir.join(format!("{}.redemptions.jsonl", stem(req)))
}

/// #388: the wallet-level cash rows (MAKER_REBATE / REWARD) ride their own
/// side file, exactly like the REDEEM rows do — never the fill stream. The
/// converter folds each row into one `cashflow` event; `0`-amount rows are
/// kept (an honest row is a row), and the window is the FILLS' window: a
/// rebate is paid at trade time, not minutes after expiry like a redemption.
pub(crate) fn cashflows_path(req: &PullRequest) -> PathBuf {
    req.out_dir.join(format!("{}.cashflows.jsonl", stem(req)))
}

// ── slug / time ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct RoundSlug {
    pub asset: String,
    pub duration_sec: i64,
    pub start_sec: i64,
}

/// Parse an updown round slug: `btc-updown-15m-1790784900` →
/// asset BTC, 900 s round starting at that epoch second. Anything that does
/// not match the shape (non-updown markets a wallet also touched) is None —
/// the converter skips it and counts it.
pub fn parse_round_slug(slug: &str) -> Option<RoundSlug> {
    let parts: Vec<&str> = slug.split('-').collect();
    if parts.len() < 4 || parts[1] != "updown" {
        return None;
    }
    let asset = parts[0].to_uppercase();
    let dur = parts[parts.len() - 2];
    let epoch = parts[parts.len() - 1];
    let mult = match dur.chars().last()? {
        'm' => 60,
        'h' => 3600,
        _ => return None,
    };
    let n: i64 = dur[..dur.len() - 1].parse().ok()?;
    let start_sec: i64 = epoch.parse().ok()?;
    Some(RoundSlug {
        asset,
        duration_sec: n * mult,
        start_sec,
    })
}

/// `YYYY-MM-DD` (UTC) or bare epoch seconds → epoch seconds. The CLI-facing
/// parser; the library only deals in epochs.
pub fn parse_time_arg(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() {
        return Ok(n);
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return Err(format!("expected YYYY-MM-DD or epoch seconds, got `{s}`"));
    }
    let y: i64 = parts[0].parse().map_err(|_| format!("bad year in `{s}`"))?;
    let m: i64 = parts[1]
        .parse()
        .map_err(|_| format!("bad month in `{s}`"))?;
    let d: i64 = parts[2].parse().map_err(|_| format!("bad day in `{s}`"))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(format!("bad date `{s}`"));
    }
    // Howard Hinnant's days_from_civil — UTC, no leap-second fantasies.
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Ok(days * 86_400)
}
