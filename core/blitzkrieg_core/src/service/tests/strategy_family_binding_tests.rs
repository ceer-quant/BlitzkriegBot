#![cfg(test)]

//! #1170 (方案 A): the `STRATEGY_<NAME>_ROUND_DURATION_SEC` boot cells —
//! the env grammar parses into the config's binding table (fail-closed on a
//! malformed or undeclared family), and the installed engine stamps the
//! binding onto the hosted scope.

use super::*;

fn binding_core(env_body: &str, allowed: &[i64]) -> Core {
    let dir = std::env::temp_dir().join(format!(
        "bk-strategy-family-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(&env_path, env_body).expect("fixture");
    Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            env_file_path: Some(env_path.to_string_lossy().into_owned()),
            allowed_round_secs: allowed.to_vec(),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    )
}

#[test]
fn the_env_cell_parses_into_the_binding_table() {
    let c = binding_core(
        "ACCOUNT_LIST=default\nACCOUNT_DEFAULT_NAME=\"main\"\n\
         STRATEGY_LUA_MOMENTUM_ROUND_DURATION_SEC=900\n",
        &[900, 3600],
    );
    assert_eq!(
        c.config().strategy_round_families.get("LUA_MOMENTUM"),
        Some(&900),
        "the cell's middle section (the folded package name) keys the table"
    );
}

#[test]
fn an_undeclared_family_refuses_the_boot() {
    let result = std::panic::catch_unwind(|| {
        binding_core(
            "ACCOUNT_LIST=default\nACCOUNT_DEFAULT_NAME=\"main\"\n\
             STRATEGY_LUA_MOMENTUM_ROUND_DURATION_SEC=1800\n",
            &[900, 3600],
        )
    });
    let msg = result
        .err()
        .map(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    assert!(
        msg.contains("STRATEGY_LUA_MOMENTUM_ROUND_DURATION_SEC=1800")
            && msg.contains("declared round family"),
        "fail-closed: the undeclared anchor refuses the boot naming the cell: {msg}"
    );
}

#[test]
fn a_malformed_value_refuses_the_boot() {
    let result = std::panic::catch_unwind(|| {
        binding_core(
            "ACCOUNT_LIST=default\nACCOUNT_DEFAULT_NAME=\"main\"\n\
             STRATEGY_LUA_MOMENTUM_ROUND_DURATION_SEC=fifteen\n",
            &[900, 3600],
        )
    });
    let msg = result
        .err()
        .map(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    assert!(
        msg.contains("is not an i64"),
        "fail-closed: a non-i64 anchor refuses the boot: {msg}"
    );
}

#[test]
fn the_installed_engine_stamps_the_bound_scope() {
    // The full boot path: parse → install → the hosted scope carries the
    // family, so its dispatch surfaces (on_round, ctx clock, ctx markets)
    // are family-scoped from the first tick. The binding lands BEFORE the
    // packages load (install_engine pushes the table first), and the
    // registration stamp picks it up — here exercised with the test host
    // registering after the install (the same relative order production's
    // loader runs).
    let mut c = binding_core(
        "ACCOUNT_LIST=default\nACCOUNT_DEFAULT_NAME=\"main\"\n\
         STRATEGY_SPREAD_ARB_ROUND_DURATION_SEC=900\n",
        &[900, 3600],
    );
    {
        let cfg = c.config().clone();
        cfg.install_engine(&mut c)
            .expect("the zero-request boot installs (no explicit strategy request)");
    }
    crate::strategies::test_support::host_disabled(
        c.engine.as_mut().unwrap(),
        crate::engine::EngineConfig::default().trend,
        crate::engine::EngineConfig::default().spread_arb,
    );
    let engine = c.engine.as_ref().unwrap();
    let bound = engine.hosted_round_family("spread_arb");
    assert_eq!(
        bound,
        Some(900),
        "the scope stamped its family from the boot table at registration"
    );
    // The unbound siblings stay None — the binding is per-package, not global.
    assert!(
        c.strategy_names()
            .into_iter()
            .filter(|n| n != "spread_arb")
            .all(|n| engine.hosted_round_family(&n).is_none()),
        "only the named package binds"
    );
}
