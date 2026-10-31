//! Data abstraction (P-1.2/P-1.3): one market-data event stream shared by live
//! and backtest.
//!
//! Live path: every event the engine consumes passes through
//! [`Core::engine_on_data`](crate::service::Core::engine_on_data), which is the
//! single choke point for Book / TopOfBook / Spot / RoundMarkets. With
//! `--event-archive <path>` the core mirrors each event into an [`EventArchive`]
//! ([`DataSink`]) while it trades.
//!
//! Backtest path: [`ReplaySource`] ([`DataSource`]) reads that same archive back
//! in timestamp order and feeds it to the backtester, so both sides speak one
//! format and one event taxonomy.
//!
//! Archive format — JSONL, one event per line, Decimals as strings (exact, unlike
//! the f64-number convention used on the IPC wire):
//!
//! ```text
//! {"at":1757851200123,"k":"book","t":"<token>","b":[["0.42","100"]],"a":[["0.43","50"]]}
//! {"at":1757851200150,"k":"top","t":"<token>","bb":"0.42","ba":"0.43"}
//! {"at":1757851200200,"k":"spot","s":"BTC","p":"62850.12"}
//! {"at":1757851200000,"k":"round","m":[{...CryptoMarket camelCase...}]}
//! {"at":1757851200300,"k":"trade","t":"<token>","s":"BUY","p":"0.42","q":"100"}
//! {"at":1757851200000,"k":"resolution","c":"<conditionId>","p":[["<winner>","1"],["<loser>","0"]],"n":false}
//! {"at":1757851200500,"k":"cashflow","which":"rebate","usd":"331.2175"}
//! {"at":1757851500000,"k":"round_end"}
//! ```
//!
//! `trade` / `round_end` (#352) are INFORMATION events: the onchain converter
//! emits them so a replay carries the observed wallet prints and round
//! boundaries. The engine does not trade on them; they exist so a replay is
//! complete (no skipped lines) and a benchmark can read the wallet's fills.
//!
//! `resolution` (#377, 治 #2) is a SETTLEMENT event: the converter recovered
//! the market's on-chain winner from the wallet's own redemption rows and
//! stamps it at the round's expiry instant. The core settles positions from
//! it directly — this is what closes the dry ladder's blind spot (book
//! evidence below the coin-flip withheld every settlement and stuck the
//! legs). Markets it covers are flagged `archiveVerdict` in their
//! `CryptoMarket`, so the dry synthesis never races it.
//!
//! `cashflow` (#388) is a CASH event: the converter carries each activity
//! ledger MAKER_REBATE / REWARD row — money the venue paid the wallet outside
//! any fill, priced in whole-ledger rows — at the row's own timestamp. The
//! core credits the active account's ledger at that instant; orders,
//! positions and settlements never see it, and a corpus without cashflow
//! rows replays exactly as before.
//!
//! `at` is the event's own `now_ms` (venue time when available) — the same value
//! the live core stamped the event with, so a replay reconstructs the identical
//! decision clock.

use crate::engine::DataEvent;
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

// #648 C: `event_at_ms` / `event_to_json` / `event_from_json` moved verbatim
// to `blitzkrieg_data_model` (the leaf the offline `tools/onchain_pull` crate
// also compiles against, with the replay readers those tests drive); these
// re-exports keep every `crate::data_source::event_*` spelling in the kernel
// compiling unchanged. The archive WRITER below stays kernel-side: recording
// is a live-core concern (locks, rotation, disk guards); reading a stream is
// the leaf's contract, duplicated here verbatim because the kernel's
// ReplaySource pairs with EventArchive's segment naming and lock semantics.
pub use blitzkrieg_data_model::{event_at_ms, event_from_json, event_to_json};

/// One market-data event plus the timestamp it must be replayed at.
#[derive(Debug, Clone)]
pub struct TimedEvent {
    pub at_ms: i64,
    pub event: DataEvent,
}

/// Counters a source reports into a backtest report (zeros when a source has no
/// parser, e.g. an in-memory list).
#[derive(Debug, Clone, Default, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SourceStats {
    pub events: u64,
    /// Lines that could not be parsed (skipped, not fatal).
    pub malformed_lines: u64,
    /// Events whose timestamp went backwards relative to the previous event.
    pub out_of_order_events: u64,
    /// #354 (5.2): Spot ticks the source parsed — the corpus-completeness
    /// anchor for the spot-driven strategies. A corpus that lost its spot rows
    /// reports a count far below the builder's tally instead of replaying
    /// "successfully" blind.
    pub spot_events: u64,
}

/// Pull side: a timestamp-ordered supply of market-data events.
pub trait DataSource {
    /// Next event, or `None` when the source is exhausted.
    fn next_event(&mut self) -> Option<TimedEvent>;
    /// Human-readable provenance for logs/reports (path, counts, …).
    fn describe(&self) -> String;
    /// Parser/stream counters for the report.
    fn stats(&self) -> SourceStats {
        SourceStats::default()
    }
}

/// Push side: where market-data events are recorded (live → archive).
pub trait DataSink {
    fn record(&mut self, ev: &DataEvent);
    fn flush(&mut self);
    /// Human-readable status for logs/diagnostics.
    fn describe(&self) -> String;
}

// ── archive writer (live → disk) ────────────────────────────────────────────

/// Append-only JSONL archive of the events the engine consumed.
///
/// Two independent bounds, both of which only ever *stop writing* — nothing is
/// ever deleted or truncated, an operator decides what to do with old data:
///
/// * `max_bytes` — session cap. `0` means unlimited. Reached → recording stops
///   and `dropped` counts the rest.
/// * `rotate_bytes` — segment size for 24/7 capture. `0` means never rotate.
///   Reached → the current file is renamed to a UTC-stamped sibling
///   (`events.jsonl` → `events.20260914T210000Z.jsonl`) and a fresh `path` is
///   opened, so a long-running archive stays in replayable chunks instead of
///   either growing unbounded or hitting the session cap and going dark. Renames
///   only: the number of segments is unbounded and no segment is ever removed.
pub struct EventArchive {
    path: PathBuf,
    file: BufWriter<File>,
    max_bytes: u64,
    rotate_bytes: u64,
    /// Stop before the filesystem drops below this many free bytes (0 = no guard).
    min_free_bytes: u64,
    /// Session totals (survive rotation).
    bytes: u64,
    events: u64,
    dropped: u64,
    /// Current segment totals (reset by rotation).
    segment_bytes: u64,
    segments: u64,
    /// Venue time of the last recorded event, used to name rotated segments.
    last_at_ms: i64,
    stopped: bool,
    /// Why recording stopped (`"cap"`/`"disk"`/`"io"`), for diagnostics.
    stopped_reason: Option<String>,
    /// Caller clock (ms) of the last periodic flush, for `flush_if_due`.
    last_flush_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveStatus {
    pub path: String,
    pub events: u64,
    pub bytes: u64,
    pub dropped: u64,
    pub recording: bool,
    /// Size bound of one segment in bytes (`0` = rotation disabled).
    pub rotate_bytes: u64,
    /// Bytes in the segment currently being written.
    pub segment_bytes: u64,
    /// How many segments were completed by rotation (0 = still the first).
    pub segments: u64,
    /// Free bytes on the archive's filesystem, re-read after each rotation and
    /// whenever the guard trips (`0` = not measured, e.g. non-unix).
    pub free_bytes: u64,
    /// Why recording stopped, if it did: `"cap"` | `"disk"` | `"io"`.
    pub stopped_reason: Option<String>,
}

impl EventArchive {
    /// Open (append/create) an archive at `path` that never rotates. Prefer
    /// [`EventArchive::open_with_rotation`] for a long-running capture.
    pub fn open(path: &Path, max_bytes: u64) -> std::io::Result<Self> {
        Self::open_with_rotation(path, max_bytes, 0)
    }

    /// Open (append/create) an archive with `rotate_bytes` per segment.
    pub fn open_with_rotation(
        path: &Path,
        max_bytes: u64,
        rotate_bytes: u64,
    ) -> std::io::Result<Self> {
        Self::open_full(path, max_bytes, rotate_bytes, 0)
    }

    /// Full form: also refuse to spend the last `min_free_bytes` of the volume.
    ///
    /// A 24/7 capture writes tens of GB/day, so "cap the file" is not enough — a
    /// rotating archive with no session cap would happily fill the disk. The guard
    /// is checked once per rotation (cheap: one `statvfs`), and stops recording
    /// before the volume runs out rather than after some other process fails.
    pub fn open_full(
        path: &Path,
        max_bytes: u64,
        rotate_bytes: u64,
        min_free_bytes: u64,
    ) -> std::io::Result<Self> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        let free_bytes = free_bytes_at(path).unwrap_or(0);
        if min_free_bytes > 0 && free_bytes > 0 && free_bytes <= min_free_bytes {
            tracing::warn!(
                path = %path.display(),
                free_mb = free_bytes / (1024 * 1024),
                min_free_mb = min_free_bytes / (1024 * 1024),
                "event archive: volume already below the free-space floor — not recording"
            );
        }
        let owned = claim(&file);
        let mut archive = Self {
            path: path.to_path_buf(),
            file: BufWriter::new(file),
            max_bytes,
            rotate_bytes,
            min_free_bytes,
            bytes,
            events: 0,
            dropped: 0,
            segment_bytes: bytes,
            segments: 0,
            last_at_ms: 0,
            stopped: false,
            stopped_reason: None,
            last_flush_ms: i64::MIN / 2,
        };
        if !owned {
            archive.stop(
                "locked",
                "another process is already recording this archive",
            );
        }
        Ok(archive)
    }

    pub fn status(&self) -> ArchiveStatus {
        ArchiveStatus {
            path: self.path.display().to_string(),
            events: self.events,
            bytes: self.bytes,
            dropped: self.dropped,
            recording: !self.stopped,
            rotate_bytes: self.rotate_bytes,
            segment_bytes: self.segment_bytes,
            segments: self.segments,
            free_bytes: free_bytes_at(&self.path).unwrap_or(0),
            stopped_reason: self.stopped_reason.clone(),
        }
    }

    /// Stop recording and remember why (surfaced in `engine.stats.archive`).
    fn stop(&mut self, reason: &str, detail: &str) {
        if !self.stopped {
            self.stopped = true;
            self.stopped_reason = Some(reason.to_string());
            tracing::warn!(path = %self.path.display(), reason, detail, "event archive stopped recording");
        }
    }

    /// Close the current segment under a UTC-stamped name and start a fresh one
    /// at the configured path. Rename-only: the rotated file is never altered.
    fn rotate(&mut self) {
        let rotated = next_segment_path(&self.path, self.last_at_ms);
        let wrote = self.segment_bytes;
        // 1. Close the current segment (fd must be released before the rename).
        let old = std::mem::replace(&mut self.file, BufWriter::new(sink_file()));
        let _ = old.into_inner().map(|mut f| f.flush());
        // 2. Move it aside.
        if let Err(e) = std::fs::rename(&self.path, &rotated) {
            tracing::warn!(
                path = %self.path.display(),
                target = %rotated.display(),
                error = %e,
                "event archive: rotate failed; keeping the current segment open"
            );
            return;
        }
        // 3. Reopen the configured path for the next segment.
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(f) => {
                if !claim(&f) {
                    // Someone else grabbed the path between the rename and here.
                    // Appending anyway would interleave two writers' lines.
                    self.stop("locked", "another process took the archive during rotate");
                    return;
                }
                self.file = BufWriter::new(f);
                self.segment_bytes = 0;
                self.segments += 1;
                tracing::info!(
                    path = %self.path.display(),
                    rotated = %rotated.display(),
                    bytes = wrote,
                    segment = self.segments,
                    "event archive rotated"
                );
            }
            Err(e) => {
                // Nothing can be written any more; stop loudly rather than
                // silently dropping the rest of the stream.
                self.stop("io", &format!("cannot reopen after rotate: {e}"));
            }
        }
    }

    /// Whether the volume still has room for another segment, judged once per
    /// rotation. A guard that cannot read the filesystem never blocks recording.
    fn disk_ok(&self) -> bool {
        if self.min_free_bytes == 0 {
            return true;
        }
        match free_bytes_at(&self.path) {
            Some(free) => free > self.min_free_bytes,
            None => true,
        }
    }

    /// Record `ev` at an explicit replay timestamp (`at`), overriding the event's
    /// own `now_ms`. The live core stamps this with the event's venue time (the
    /// clock the engine decided on); `now_ms` is only a fallback for a source
    /// that left the event timestamp unset.
    pub fn record_at(&mut self, at: i64, ev: &DataEvent) {
        let stamped = stamped_event(at, ev);
        self.record(&stamped);
    }

    /// Flush at most once per `interval_ms` of the caller's clock, so a reader of
    /// the live file (or an operator tailing it) sees events without waiting for
    /// the BufWriter to fill. Returns whether a flush happened.
    pub fn flush_if_due(&mut self, now_ms: i64, interval_ms: i64) -> bool {
        if self.stopped || self.events == 0 {
            return false;
        }
        if now_ms.saturating_sub(self.last_flush_ms) < interval_ms.max(0) {
            return false;
        }
        self.last_flush_ms = now_ms;
        if let Err(e) = self.file.flush() {
            tracing::warn!(error = %e, path = %self.path.display(), "event archive: flush failed");
        }
        true
    }
}

/// A copy of `ev` carrying `at` as its `now_ms`.
fn stamped_event(at: i64, ev: &DataEvent) -> DataEvent {
    let mut out = ev.clone();
    match &mut out {
        DataEvent::Book { now_ms, .. }
        | DataEvent::Books { now_ms, .. }
        | DataEvent::TopOfBook { now_ms, .. }
        | DataEvent::Spot { now_ms, .. }
        | DataEvent::RoundMarkets { now_ms, .. }
        | DataEvent::Trade { now_ms, .. }
        | DataEvent::Resolution { now_ms, .. }
        | DataEvent::Cashflow { now_ms, .. }
        | DataEvent::UnifiedMapping { now_ms, .. }
        | DataEvent::RoundEnd { now_ms } => *now_ms = at,
    }
    out
}

/// Does `name` match the exact shape `next_segment_path` produces —
/// `{stem}.{UTC-stamp|ms}[-NNNN].jsonl`? The old prefix-only rule
/// (`{stem}.` + any tail) swallowed sibling datasets like
/// `<stem>.trades.jsonl` (#352: the onchain raw-fills side file) as
/// phantom replay segments, so every raw line was counted malformed.
fn is_rotated_segment(stem: &str, name: &str) -> bool {
    let Some(body) = name
        .strip_prefix(&format!("{stem}."))
        .and_then(|b| b.strip_suffix(".jsonl"))
    else {
        return false;
    };
    let body = match body.rsplit_once('-') {
        // Trailing `-NNNN` is the rotate-clobber disambiguator.
        Some((base, seq)) if !seq.is_empty() && seq.chars().all(|c| c.is_ascii_digit()) => base,
        _ => body,
    };
    // The timestamp itself is a fixed-width UTC stamp (`YYYYMMDDTHHMMSSZ`:
    // digits with one `T` at index 8 and a trailing `Z`) or the numeric
    // fallback (venue ms). Anything else is a different dataset, not a
    // segment of this one.
    if !body.is_empty() && body.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    body.len() == 16
        && body.as_bytes()[8] == b'T'
        && body.ends_with('Z')
        && body
            .char_indices()
            .all(|(i, c)| c.is_ascii_digit() || i == 8 || i == 15)
}

/// Sort key for an archive segment name: rotated segments first (in write order),
/// the live path last (it is the segment still being written). Within the rotated
/// group, `base` is the timestamp in the name — a fixed-width UTC stamp, or the
/// numeric fallback — so comparing it as text still matches chronological order.
fn segment_sort_key(stem: &str, name: &str, live: Option<&str>) -> (u8, String, u32) {
    if live == Some(name) {
        return (1, String::new(), 0);
    }
    let body = name
        .strip_prefix(&format!("{stem}."))
        .unwrap_or(name)
        .strip_suffix(".jsonl")
        .unwrap_or(name);
    match body.rsplit_once('-') {
        // `20250914T120000Z-0002` → ("20250914T120000Z", 2)
        Some((base, seq)) if !seq.is_empty() && seq.chars().all(|c| c.is_ascii_digit()) => {
            (0, base.to_string(), seq.parse().unwrap_or(0))
        }
        _ => (0, body.to_string(), 0),
    }
}

/// A writable sink used to park the file handle while a segment is renamed. It
/// is never written to (only swapped out again on the next open), so a temp path
/// is fine and keeps the rename off any shared state.
fn sink_file() -> File {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join(format!("bk-archive-sink-{}", std::process::id())))
        .unwrap_or_else(|_| {
            File::create(std::env::temp_dir().join("bk-archive-sink")).expect("temp sink")
        })
}

/// Take an exclusive advisory lock on the archive file. Two cores pointed at one
/// archive would interleave lines and (worse) rotate the file out from under each
/// other, so capture is single-writer by construction.
///
/// Returns false only when another process holds the lock. A platform or
/// filesystem without the lock API counts as "not held" — losing the guard is
/// better than silently recording nothing.
fn claim(file: &File) -> bool {
    match file.try_lock() {
        Ok(()) => true,
        Err(std::fs::TryLockError::WouldBlock) => false,
        Err(std::fs::TryLockError::Error(_)) => true,
    }
}

/// Free bytes on the filesystem holding `path` (None when unknowable, e.g. a
/// non-unix target or a stat failure). Used to keep a 24/7 capture from filling
/// the volume.
fn free_bytes_at(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        // SAFETY: `c` is a valid NUL-terminated path and `st` is only read after
        // statvfs reports success.
        unsafe {
            let mut st: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c.as_ptr(), &mut st) != 0 {
                return None;
            }
            Some(st.f_bavail as u64 * st.f_frsize as u64)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// `events.jsonl` + venue time 1757851200123 → `events.20250914T140000Z.jsonl`
/// (UTC). Falls back to a numeric stamp when the time cannot be decoded.
fn next_segment_path(path: &Path, at_ms: i64) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "events".into());
    let ext = path.extension().map(|s| s.to_string_lossy().to_string());
    let name = match utc_stamp(at_ms) {
        Some(stamp) => format!("{stem}.{stamp}"),
        None => format!("{stem}.{at_ms}"),
    };
    let file = match ext {
        Some(e) if !e.is_empty() => format!("{name}.{e}"),
        _ => name,
    };
    let candidate = path.with_file_name(file);
    // Two rotations can land in the same second (a small threshold, or a burst),
    // and the second rename would silently clobber the first segment. Disambiguate
    // until the name is free — a rename target must never exist. The suffix is
    // zero-padded so a lexicographic sort of segment names still matches write
    // order (`-0010` must not sort before `-0002`).
    if !candidate.exists() {
        return candidate;
    }
    for n in 2..10_000u32 {
        let alt = match (
            utc_stamp(at_ms),
            path.extension().map(|s| s.to_string_lossy().to_string()),
        ) {
            (Some(stamp), Some(e)) if !e.is_empty() => format!("{stem}.{stamp}-{n:04}.{e}"),
            (Some(stamp), _) => format!("{stem}.{stamp}-{n:04}"),
            (None, Some(e)) if !e.is_empty() => format!("{stem}.{at_ms}-{n:04}.{e}"),
            (None, _) => format!("{stem}.{at_ms}-{n:04}"),
        };
        let alt = path.with_file_name(alt);
        if !alt.exists() {
            return alt;
        }
    }
    candidate
}

/// `YYYYMMDDTHHMMSSZ` for a unix-ms instant. Hand-rolled UTC conversion so the
/// core keeps no date-library dependency for one filename.
///
/// `pub(crate)` since #199: the data-directory lock file records a human-readable
/// `started_at` for whoever reads it during an incident, and that is the same
/// one-spelling rule as the archive's segment names — one UTC formatter in the
/// crate, not two that can drift apart.
pub(crate) fn utc_stamp(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    let secs = ms / 1000;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    // Howard Hinnant's civil_from_days, shifted to the 1970 epoch.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    Some(format!("{y:04}{m:02}{d:02}T{h:02}{mi:02}{s:02}Z"))
}

impl DataSink for EventArchive {
    fn record(&mut self, ev: &DataEvent) {
        if self.stopped {
            self.dropped += 1;
            return;
        }
        let line = match serde_json::to_string(&event_to_json(ev)) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "event archive: encode failed; event dropped");
                self.dropped += 1;
                return;
            }
        };
        let n = line.len() as u64 + 1;
        if let Err(e) = self
            .file
            .write_all(line.as_bytes())
            .and_then(|_| self.file.write_all(b"\n"))
        {
            let msg = format!("write failed: {e}");
            self.dropped += 1;
            self.stop("io", &msg);
            return;
        }
        self.events += 1;
        self.bytes += n;
        self.segment_bytes += n;
        self.last_at_ms = event_at_ms(ev);

        // Rotation first: a long capture must stay in replayable chunks instead of
        // growing until the session cap silences it.
        if self.rotate_bytes > 0 && self.segment_bytes >= self.rotate_bytes {
            self.rotate();
            // The free-space guard is evaluated at the one cheap boundary we have.
            if !self.disk_ok() {
                let free = free_bytes_at(&self.path).unwrap_or(0);
                self.stop("disk", &format!("free space {free} bytes below the floor"));
            }
        }
        if self.stopped {
            return;
        }
        if self.max_bytes > 0 && self.bytes >= self.max_bytes {
            let _ = self.file.flush();
            let bytes = self.bytes;
            self.stop("cap", &format!("session cap {bytes} bytes reached"));
        }
    }

    fn flush(&mut self) {
        if let Err(e) = self.file.flush() {
            tracing::warn!(error = %e, path = %self.path.display(), "event archive: flush failed");
        }
        self.last_flush_ms = i64::MIN / 2;
    }

    fn describe(&self) -> String {
        let s = self.status();
        format!(
            "archive {} ({} events, {} bytes{})",
            s.path,
            s.events,
            s.bytes,
            if s.recording {
                ""
            } else {
                ", recording stopped"
            }
        )
    }
}

// ── replay reader (disk → backtest) ─────────────────────────────────────────

/// Streams an [`EventArchive`] back in file order (which the live writer produced
/// in timestamp order).
///
/// Malformed lines are skipped and counted (one warn per run, not per line) so a
/// single corrupt tail cannot abort a long replay. Out-of-order timestamps are
/// counted and surfaced: the backtester clamps them to the current clock.
pub struct ReplaySource {
    path: PathBuf,
    reader: BufReader<File>,
    buf: String,
    line_no: u64,
    events: u64,
    skipped: u64,
    out_of_order: u64,
    /// #354 (5.2): Spot ticks parsed — the completeness counter.
    spots: u64,
    last_at_ms: Option<i64>,
    warned: bool,
}

impl ReplaySource {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            reader: BufReader::new(file),
            buf: String::new(),
            line_no: 0,
            events: 0,
            skipped: 0,
            out_of_order: 0,
            spots: 0,
            last_at_ms: None,
            warned: false,
        })
    }

    pub fn skipped_lines(&self) -> u64 {
        self.skipped
    }
    pub fn out_of_order_events(&self) -> u64 {
        self.out_of_order
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl DataSource for ReplaySource {
    fn next_event(&mut self) -> Option<TimedEvent> {
        loop {
            self.buf.clear();
            match self.reader.read_line(&mut self.buf) {
                Ok(0) => return None,
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, path = %self.path.display(), "replay source: read failed");
                    return None;
                }
            }
            self.line_no += 1;
            let line = self.buf.trim();
            if line.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    self.skipped += 1;
                    if !self.warned {
                        tracing::warn!(line = self.line_no, error = %e, "replay source: skipping malformed lines");
                        self.warned = true;
                    }
                    continue;
                }
            };
            let event = match event_from_json(&value) {
                Ok(ev) => ev,
                Err(e) => {
                    self.skipped += 1;
                    if !self.warned {
                        tracing::warn!(line = self.line_no, error = %e, "replay source: skipping malformed lines");
                        self.warned = true;
                    }
                    continue;
                }
            };
            if matches!(event, DataEvent::Spot { .. }) {
                self.spots += 1;
            }
            let at_ms = event_at_ms(&event);
            if let Some(prev) = self.last_at_ms
                && at_ms < prev
            {
                self.out_of_order += 1;
            }
            self.last_at_ms = Some(at_ms.max(self.last_at_ms.unwrap_or(at_ms)));
            self.events += 1;
            return Some(TimedEvent { at_ms, event });
        }
    }

    fn describe(&self) -> String {
        // Streaming counters come from `stats()` after the run: a label printed
        // before the first read must not claim the archive is empty.
        format!("replay {}", self.path.display())
    }

    fn stats(&self) -> SourceStats {
        SourceStats {
            events: self.events,
            malformed_lines: self.skipped,
            out_of_order_events: self.out_of_order,
            spot_events: self.spots,
        }
    }
}

/// Convenience: open a replay source, mapping the io error to a message.
pub fn open_replay(path: &str) -> Result<ReplaySource, String> {
    ReplaySource::open(Path::new(path)).map_err(|e| format!("cannot open archive {path}: {e}"))
}

/// A 24/7 capture rotates, so a replay must be able to read a whole directory of
/// segments in time order rather than one file. Segments are the archive plus its
/// rotated siblings (`events.jsonl` + `events.<UTC>.jsonl`); ordering is by the
/// UTC stamp in the name, which is also the order they were written in.
pub struct SegmentSource {
    sources: Vec<ReplaySource>,
    current: usize,
    events: u64,
    skipped: u64,
    out_of_order: u64,
    /// #354 (5.2): Spot ticks folded in from exhausted segments.
    spots: u64,
    last_at_ms: Option<i64>,
    label: String,
}

impl SegmentSource {
    /// Open every `.jsonl` segment that belongs to `path` (the archive itself plus
    /// its UTC-stamped siblings), in write order. Returns an error when nothing
    /// matched, so a typo in the path is still reported.
    pub fn open_dir(path: &Path) -> Result<Self, String> {
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "events".into());
        let live = path.file_name().map(|s| s.to_string_lossy().to_string());

        let mut names: Vec<String> = Vec::new();
        let entries = std::fs::read_dir(dir)
            .map_err(|e| format!("cannot read archive dir {}: {e}", dir.display()))?;
        for e in entries.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !name.ends_with(".jsonl") {
                continue;
            }
            // Belongs to this archive when it is the archive itself, or a rotated
            // sibling in the exact shape the writer produces (`stem.<stamp>[-N].jsonl`).
            // A loose prefix rule would swallow sibling datasets like the #352
            // onchain `<stem>.trades.jsonl` and count every raw line malformed.
            let is_mine = Some(&name) == live.as_ref() || is_rotated_segment(&stem, &name);
            if is_mine {
                names.push(name);
            }
        }
        if names.is_empty() {
            return Err(format!(
                "no archive segments matching {} in {}",
                path.display(),
                dir.display()
            ));
        }
        // Order by write time, derived from the name, not by raw bytes: a rotated
        // segment is `stem.<stamp>[-<seq>].jsonl` and the live path is `stem.jsonl`.
        // Raw lexicographic order would put `-0002` before `.jsonl` and the live
        // path in the middle — replay would then mix different instants together.
        names.sort_by(|a, b| {
            segment_sort_key(&stem, a, live.as_deref()).cmp(&segment_sort_key(
                &stem,
                b,
                live.as_deref(),
            ))
        });

        let mut sources = Vec::with_capacity(names.len());
        for name in &names {
            let p = dir.join(name);
            sources.push(
                ReplaySource::open(&p)
                    .map_err(|e| format!("cannot open segment {}: {e}", p.display()))?,
            );
        }
        let label = format!("{} ({} segment(s))", path.display(), sources.len());
        Ok(Self {
            sources,
            current: 0,
            events: 0,
            skipped: 0,
            out_of_order: 0,
            spots: 0,
            last_at_ms: None,
            label,
        })
    }

    pub fn segments(&self) -> usize {
        self.sources.len()
    }
}

impl DataSource for SegmentSource {
    fn next_event(&mut self) -> Option<TimedEvent> {
        loop {
            let src = self.sources.get_mut(self.current)?;
            if let Some(te) = src.next_event() {
                // Carry the counters and the cross-segment clock clamp forward, so
                // a boundary that goes backwards is still reported like any other
                // out-of-order arrival.
                let at_ms = te.at_ms;
                if let Some(prev) = self.last_at_ms
                    && at_ms < prev
                {
                    self.out_of_order += 1;
                }
                self.last_at_ms = Some(at_ms.max(self.last_at_ms.unwrap_or(at_ms)));
                self.events += 1;
                return Some(te);
            }
            // Exhausted: fold in this segment's parse counters and move on.
            self.skipped += src.skipped_lines();
            self.spots += src.stats().spot_events;
            self.current += 1;
        }
    }

    fn describe(&self) -> String {
        format!("replay {}", self.label)
    }

    fn stats(&self) -> SourceStats {
        SourceStats {
            events: self.events,
            malformed_lines: self.skipped,
            out_of_order_events: self.out_of_order,
            spot_events: self.spots,
        }
    }
}

/// Open a replay that covers an archive and (when present) its rotated segments.
/// A single-file archive and a directory of segments both work; the returned
/// source reports aggregate counters either way.
pub fn open_replay_all(path: &str) -> Result<SegmentSource, String> {
    SegmentSource::open_dir(Path::new(path))
}

#[cfg(test)]
mod tests;
