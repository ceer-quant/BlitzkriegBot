//! Test-only oracle for the shell migration test (issues #760 / #759).
//!
//! The migration script's acceptance is not "the .env contains the lines we
//! expected" — a hand-written expectation can agree with the script and with
//! nothing else. It is "the REAL kernel loader reads the migrated `.env` and
//! resolves the same policy". This ignored integration test is that entry
//! point: `scripts/migrate-config-to-env-test.sh` invokes it with
//! `--ignored`, points `BK_ORACLE_ENV` at the scratch `.env` and
//! `BK_ORACLE_OUT` at a file, then reads the `ORACLE ...` lines back. It is
//! ignored by default so the normal test suite never depends on a scratch path.
//!
//! The lines go to `BK_ORACLE_OUT` instead of stdout on purpose: a budget is a
//! money figure, and CodeQL's `rust/cleartext-logging` query (rightly) flags
//! money figures interpolated into a logging macro even in a test. A file the
//! test owns is not a log sink.
//!
//! It uses [`Policy::load_for_reload`] — the kernel's own `.env`-only reload
//! reader (the boot path's `load_single_source` also consults the process
//! environment; the reload reader is the exact "read the file, not ambient
//! env" face this oracle needs). A nonexistent legacy toml is deliberate:
//! post-migration the `.env` IS the whole policy surface.

use blitzkrieg_core::execution_policy::{Policy, PolicyInput, PolicyOutput, evaluate};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::path::Path;

#[test]
#[ignore = "shell-test oracle; invoked explicitly by scripts/migrate-config-to-env-test.sh"]
fn policy_env_oracle() {
    let env_path =
        std::env::var("BK_ORACLE_ENV").expect("BK_ORACLE_ENV must name the migrated .env");
    let out_path = std::env::var("BK_ORACLE_OUT").expect("BK_ORACLE_OUT must name the report file");
    let toml_path = std::env::var("BK_ORACLE_TOML")
        .unwrap_or_else(|_| "/nonexistent/execution_policy.toml".to_string());
    // The third argument is the same-process IPC write snapshot (#757). The
    // oracle is a fresh process reading only the file, so it is empty: the
    // migrated `.env` must stand on its own.
    let written = std::collections::BTreeMap::new();
    let policy = Policy::load_for_reload(Path::new(&toml_path), Path::new(&env_path), &written)
        .expect("the migrated .env must load through the real policy loader");

    let mut lines = Vec::new();
    lines.push(format!(
        "ORACLE defaults.max_budget_usd={}",
        policy.defaults.max_budget_usd
    ));
    lines.push(format!(
        "ORACLE defaults.rules={}",
        policy.defaults.rules.len()
    ));

    let input = |open_positions: u32| PolicyInput {
        available_balance: Decimal::from(1000),
        total_equity: Decimal::from(1000),
        open_positions,
        current_price: dec!(0.50),
        time_left_sec: 300,
        symbol: "BTC".to_string(),
        recent_pnl_1h: Decimal::ZERO,
        consecutive_losses: 0,
    };

    let accounts =
        std::env::var("BK_ORACLE_ACCOUNTS").unwrap_or_else(|_| "default,paper".to_string());
    for id in accounts.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let section = policy.section_for(id);
        lines.push(format!(
            "ORACLE {id}.max_budget_usd={}",
            section.max_budget_usd
        ));
        lines.push(format!("ORACLE {id}.rules={}", section.rules.len()));
        let verdict = match evaluate(&policy, id, &input(0)).expect("evaluate at zero positions") {
            PolicyOutput::Place { budget_usd, .. } => format!("budget_usd={budget_usd}"),
            other => format!("budget_usd={other:?}"),
        };
        lines.push(format!("ORACLE {id}.{verdict}"));
        let gate = match evaluate(&policy, id, &input(2)).expect("evaluate at two positions") {
            PolicyOutput::Skip { .. } => "skip",
            PolicyOutput::Cooldown { .. } => "cooldown",
            PolicyOutput::Place { .. } => "place",
        };
        lines.push(format!("ORACLE {id}.evaluate={gate}"));
    }
    std::fs::write(&out_path, lines.join("\n") + "\n").expect("write the oracle report");
}
