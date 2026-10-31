// #780 — the contiguous tail of `parse_args`'s flag match, extracted
// block-for-block so the parser body fits the 1500-line review budget.
// Every `--flag` string, help line and error message below is byte-identical
// to its original arm; only the `&mut` derefs are new (the locals now live
// in the caller).
use super::bad_value;

#[allow(clippy::too_many_arguments)]
pub(super) fn parse_replay_and_noop_flags(
    a: &str,
    it: &mut impl Iterator<Item = String>,
    backtest_mode: &mut String,
    verify_latency_ms: &mut i64,
    latency_ms: &mut i64,
    taker_latency_ms: &mut i64,
    taker_rtt_ms: &mut i64,
    fill_prob_bps: &mut Option<u32>,
    maker_depth_share_bps: &mut Option<u32>,
    dry_redeem_fail: &mut u32,
    dry_redeem_manual: &mut bool,
) -> bool {
    match a {
        // #351 backtest mode: `mine` (zero latency, keep real friction),
        // `verify` (a real-latency dial set), `sweep` (latency ladder
        // 0..300). Only meaningful with --backtest; the mode re-prices
        // the fill model's LATENCY dials only — slippage/fees stay as
        // configured, so the friction between modes is comparable. A
        // MISSING value keeps the historic single-run shape (empty mode =
        // no mode runner), which is the documented default, not a
        // fallback a typo could hit.
        "--backtest-mode" => {
            if let Some(raw) = it.next() {
                match raw.as_str() {
                    "mine" | "verify" | "sweep" => *backtest_mode = raw,
                    _ => bad_value(
                        "--backtest-mode",
                        &raw,
                        "want mine|verify|sweep (see --help; requires --backtest)",
                    ),
                }
            }
        }
        // #351 verify mode's one dial: the round-trip the strategy
        // actually trades through (the HK VPS number is 286 ms).
        "--verify-latency-ms" => {
            *verify_latency_ms = num_value!(
                it,
                "--verify-latency-ms",
                i64,
                "want a whole number of milliseconds"
            )
        }
        "--latency-ms" => {
            *latency_ms = num_value!(
                it,
                "--latency-ms",
                i64,
                "want a whole number of milliseconds"
            )
        }
        // #351: taker-leg travel dials (see Args docs above).
        "--taker-latency-ms" => {
            *taker_latency_ms = num_value!(
                it,
                "--taker-latency-ms",
                i64,
                "want a whole number of milliseconds"
            )
        }
        "--taker-rtt-ms" => {
            *taker_rtt_ms = num_value!(
                it,
                "--taker-rtt-ms",
                i64,
                "want a whole number of milliseconds"
            )
        }
        "--fill-prob-bps" => {
            *fill_prob_bps = Some(num_value!(
                it,
                "--fill-prob-bps",
                u32,
                "want a probability in bps of 10000"
            ))
        }
        "--maker-depth-share-bps" => {
            *maker_depth_share_bps = Some(num_value!(
                it,
                "--maker-depth-share-bps",
                u32,
                "want a depth share in bps of 10000"
            ))
        }
        // Dry-mode test hooks (#175 gate). Both are inert unless a mode that
        // simulates the redemption is running, so a live deployment can pass
        // them and see no change at all.
        "--dry-redeem-fail" => {
            *dry_redeem_fail = num_value!(
                it,
                "--dry-redeem-fail",
                u32,
                "want a failure count (0 = off)"
            )
        }
        "--dry-redeem-manual" => *dry_redeem_manual = true,
        // Consumed by the pre-scan; named here so they are not reported as
        // unknown flags.
        "--config" => {
            it.next();
        }
        "--no-config" => {}
        other if other.starts_with("--config=") => {}
        // Read by the pre-scan above, for the same reason (`argv` order must
        // not change what it means) and named here for the same reason.
        blitzkrieg_core::cli::ALLOW_UNKNOWN_ARGS => {}
        _ => return false,
    }
    true
}
