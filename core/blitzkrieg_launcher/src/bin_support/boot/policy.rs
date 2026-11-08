// #780 — two more sections of `parse_args`, moved verbatim out of `args.rs`
// so no file exceeds the 1500-line review budget (pure code motion): the
// issue #991 unverified-expiry settlement policy and the NTP clock-offset
// probe. Each takes the pieces of `parse_args` scope it touched as
// parameters and returns what the `Args` tail consumes; no flag string,
// help line, echo line or error message changed.
use blitzkrieg_core::config::{Sourced, pick};

use super::EnvVars;

/// The issue #991 policy block (`args.rs` 1112-1192): parse the boot-only
/// settlement policy + the receivable aging budget, push the engine-report
/// lines, and build the boot echo. Returns the policy, the aging budget
/// (still `Sourced`, so the `Args` tail reads `.value` unchanged), and the
/// echo lines.
pub(crate) fn resolve_unverified_expiry_policy(
    env: &EnvVars,
    report: &mut Vec<String>,
) -> (
    blitzkrieg_core::service::UnverifiedExpiryPolicy,
    Sourced<i64>,
    Vec<String>,
) {
    // ── issue #991: the unverified-expiry settlement policy ─────────────────
    // Read at boot only — NOT hot-reloadable: a mid-session switch would
    // re-price claims already journaled under the other policy, so honoring a
    // change requires a restart (documented in .env.example).
    let policy_raw = env.text("BK_UNVERIFIED_EXPIRY_POLICY");
    let unverified_expiry_policy = match policy_raw.as_deref().map(str::trim) {
        None => blitzkrieg_core::service::UnverifiedExpiryPolicy::Cash,
        Some(raw) => match blitzkrieg_core::service::UnverifiedExpiryPolicy::parse(raw) {
            Some(policy) => {
                report.push(format!(
                    "engine.unverified_expiry_policy={} (env)",
                    policy.as_str()
                ));
                policy
            }
            None => {
                eprintln!(
                    "blitzkrieg-core: BK_UNVERIFIED_EXPIRY_POLICY={raw:?} is not a \
                         known policy — expected \"cash\" or \"receivable\""
                );
                std::process::exit(2);
            }
        },
    };
    // Overdue-receivable aging budget for the receivable policy. The compiled
    // default (48 h) rides `RECEIVABLE_MAX_AGE_MS_DEFAULT`; 0 disables the warn.
    let receivable_max_age = pick(
        None::<i64>,
        env.num::<i64>("BK_RECEIVABLE_MAX_AGE_MS"),
        blitzkrieg_core::service::RECEIVABLE_MAX_AGE_MS_DEFAULT,
    );
    if receivable_max_age.is_explicit() {
        report.push(format!(
            "engine.receivable_max_age_ms={} ({})",
            receivable_max_age.value,
            receivable_max_age.source.as_str()
        ));
    }
    if receivable_max_age.value < 0 {
        eprintln!(
            "blitzkrieg-core: BK_RECEIVABLE_MAX_AGE_MS={} is negative — the aging \
             budget cannot be negative (0 = warn disabled)",
            receivable_max_age.value
        );
        std::process::exit(2);
    }
    // Boot echo carries the policy always (its default is a posture statement)
    // and the aging budget only when the receivable policy is actually armed.
    let receivable_echo = vec![
        format!(
            "expiry settlement policy: {} ({}) — unverified-expiry closes {}",
            match unverified_expiry_policy {
                blitzkrieg_core::service::UnverifiedExpiryPolicy::Cash => "cash",
                blitzkrieg_core::service::UnverifiedExpiryPolicy::Receivable => "receivable",
            },
            if policy_raw.is_some() {
                "env"
            } else {
                "default"
            },
            match unverified_expiry_policy {
                blitzkrieg_core::service::UnverifiedExpiryPolicy::Cash => {
                    "credit spendable cash immediately at the fallback price (flag-off behavior)"
                }
                blitzkrieg_core::service::UnverifiedExpiryPolicy::Receivable => {
                    "book a receivable claim redeemed at venue truth on resolution/reconcile; \
                     the estimated amount is excluded from day-budget and loss-breaker headroom"
                }
            },
        ),
        format!(
            "receivable aging budget: {} ms ({}) — overdue receivables warn amber once per episode{}",
            receivable_max_age.value,
            receivable_max_age.source.as_str(),
            if receivable_max_age.value == 0 {
                "; warn DISABLED"
            } else {
                ""
            },
        ),
    ];
    (
        unverified_expiry_policy,
        receivable_max_age,
        receivable_echo,
    )
}

/// The NTP probe block (`args.rs` 1194-1241): resolve the host/interval via
/// the CLI > env > default chain and build the boot echo. Returns the host,
/// the interval (still `Sourced`), and the echo lines.
pub(crate) fn resolve_ntp(
    env: &EnvVars,
    ntp_host: Option<String>,
    ntp_interval_secs: Option<i64>,
) -> (String, i64, Vec<String>) {
    // ── 任务二 1-3: the NTP clock-offset probe ──────────────────────────────
    // CLI > env > default. The empty string is the documented "off" spelling:
    // `probe::should_spawn_ntp` refuses it, so a configured-off probe spawns
    // ZERO network tasks (the replay-safe posture). An explicit --ntp-host ""
    // and an absent host are pinned apart by the snapshot test.
    let ntp = pick(
        ntp_host,
        env.text("BK_NTP_HOST"),
        blitzkrieg_core::probe::DEFAULT_NTP_HOST.to_string(),
    );
    let ntp_interval = pick(
        ntp_interval_secs,
        env.num::<i64>("BK_NTP_INTERVAL_SECS"),
        blitzkrieg_core::probe::DEFAULT_NTP_INTERVAL_SECS,
    );
    if ntp_interval.value < 0 {
        // Parse arms already refuse a negative CLI value; a negative from the
        // env layer lands here (same fail-loud posture, never a silent default).
        eprintln!(
            "blitzkrieg-core: BK_NTP_INTERVAL_SECS={} is negative — the NTP probe \
             interval cannot be negative (0 = off)",
            ntp_interval.value
        );
        std::process::exit(2);
    }
    let ntp_echo = vec![if blitzkrieg_core::probe::should_spawn_ntp(&ntp.value)
        && ntp_interval.value > 0
    {
        format!(
            "environment probes: NTP clock-offset probe ON — every {} s against \
             {} ({}/{}); |offset| >= 50 ms warns, >= 100 ms flags probe.ntp.skewRed",
            ntp_interval.value,
            ntp.value,
            ntp.source.as_str(),
            ntp_interval.source.as_str()
        )
    } else {
        format!(
            "environment probes: NTP clock-offset probe OFF (host={}, interval={}s) — \
             zero network tasks spawned; stats probe.ntp stays cold",
            if ntp.value.trim().is_empty() {
                "(empty = off)"
            } else {
                ntp.value.as_str()
            },
            ntp_interval.value
        )
    }];
    (ntp.value, ntp_interval.value, ntp_echo)
}
