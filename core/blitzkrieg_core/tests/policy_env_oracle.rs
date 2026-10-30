//! Test-only oracle for the shell migration test (issues #760 / #759).
//!
//! The migration script's acceptance is not "the .env contains the lines we
//! expected" — a hand-written expectation can agree with the script and with
//! nothing else. It is "the REAL kernel loader reads the migrated `.env` and
//! resolves the same policy". This ignored integration test is that entry
//! point: `scripts/migrate-config-to-env-test.sh` invokes it with
//! `--ignored --nocapture`, points `BK_ORACLE_ENV` at the scratch `.env`, and
//! parses the `ORACLE ...` lines. It is ignored by default so the normal test
//! suite never depends on a scratch path.
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
    let toml_path = std::env::var("BK_ORACLE_TOML")
        .unwrap_or_else(|_| "/nonexistent/execution_policy.toml".to_string());
    let policy = Policy::load_for_reload(Path::new(&toml_path), Path::new(&env_path))
        .expect("the migrated .env must load through the real policy loader");

    println!(
        "ORACLE defaults.max_budget_usd={}",
        policy.defaults.max_budget_usd
    );
    println!("ORACLE defaults.rules={}", policy.defaults.rules.len());

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
        println!("ORACLE {id}.max_budget_usd={}", section.max_budget_usd);
        println!("ORACLE {id}.rules={}", section.rules.len());
        match evaluate(&policy, id, &input(0)).expect("evaluate with no positions open") {
            PolicyOutput::Place { budget_usd, .. } => {
                println!("ORACLE {id}.budget_usd={budget_usd}");
            }
            other => println!("ORACLE {id}.budget_usd={other:?}"),
        }
        match evaluate(&policy, id, &input(2)).expect("evaluate with two positions open") {
            PolicyOutput::Skip { .. } => println!("ORACLE {id}.evaluate=skip"),
            PolicyOutput::Cooldown { .. } => println!("ORACLE {id}.evaluate=cooldown"),
            PolicyOutput::Place { .. } => println!("ORACLE {id}.evaluate=place"),
        }
    }
}
