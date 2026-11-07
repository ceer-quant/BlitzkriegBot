#![cfg(test)]

use super::*;

// ── #363: the execution policy surface, at the wire ─────────────────────

/// #386 — the fixture contract that killed the flake: two cores NEVER
/// share an audit journal. The CI failure (run 37262356151) had two
/// parallel tests appending to the ONE fixed `data/audit/…` file; their
/// records fused into a single unparseable line and BOTH history counts
/// came back empty (`left: 0, right: 1`). Each fixture now carries its
/// own journal inside its own scratch root — this test pins that a set
/// through one core lands in that core's journal alone, and the other
/// core's history stays empty.
#[tokio::test]
async fn policy_journals_are_private_per_fixture() {
    let (a, reg_a, peer_a, path_a) = policy_fixture().await;
    let (b, reg_b, peer_b, path_b) = policy_fixture().await;
    assert_ne!(path_a, path_b, "each fixture gets its own scratch root");

    let v = rpc(&a, &reg_a, &peer_a,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-x","budgetRatio":"0.05","maxBudgetUsd":"20"}}"#.into())
        .await;
    assert!(v.get("error").is_none(), "set on core A answers: {v}");
    let v = rpc(&b, &reg_b, &peer_b,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-y","budgetRatio":"0.06","maxBudgetUsd":"25"}}"#.into())
        .await;
    assert!(v.get("error").is_none(), "set on core B answers: {v}");

    // Each journal carries exactly its own core's record — no bleed.
    let ja = a.lock().await.execution_policy_audit();
    assert_eq!(ja.len(), 1, "core A's journal: {ja:?}");
    assert_eq!(ja[0].account_id, "acct-x");
    let jb = b.lock().await.execution_policy_audit();
    assert_eq!(jb.len(), 1, "core B's journal: {jb:?}");
    assert_eq!(jb[0].account_id, "acct-y");

    // And the journals are physically inside each fixture's scratch root
    // (the cleanup at the tests' end removes them with the policy file).
    for path in [&path_a, &path_b] {
        let journal = path
            .parent()
            .expect("scratch root")
            .join("data/audit/execution_policy.jsonl");
        assert!(journal.is_file(), "private journal exists");
    }
    let _ = std::fs::remove_dir_all(path_a.parent().unwrap());
    let _ = std::fs::remove_dir_all(path_b.parent().unwrap());
}

/// Issue #965: the KERNEL's own bootstrap template is not a legacy operator
/// file. A fresh deployment bootstraps the template at the default path; the
/// first real `execution_policy.set` must go THROUGH (the template is inert,
/// the .env write is the real face) and sweep the template, so no dead toml
/// lingers beside the env keys that are now the single source.
#[tokio::test]
async fn a_set_sweeps_the_kernel_generated_template_and_writes_the_env() {
    let (core, registry, peer, env_path) = policy_fixture().await;
    let toml_path = {
        let c = core.lock().await;
        std::path::PathBuf::from(c.config().execution_policy_path.clone().unwrap())
    };
    std::fs::write(&toml_path, crate::execution_policy::BOOTSTRAP_TEMPLATE)
        .expect("seed the kernel-generated template");

    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-t","budgetRatio":"0.05","maxBudgetUsd":"20"}}"#.into())
        .await;
    assert!(
        v.get("error").is_none(),
        "the kernel's own template must not block the write: {v}"
    );
    assert!(
        !toml_path.exists(),
        "the template is swept on the first real save"
    );
    let on_file: std::collections::HashMap<String, String> = std::fs::read_to_string(&env_path)
        .expect("the env file")
        .lines()
        .filter_map(crate::env_file::parse_line)
        .collect();
    assert_eq!(
        on_file
            .get("POLICY_BUDGET_RATIO_ACCT_T")
            .map(String::as_str),
        Some("0.05"),
        "the section landed in the env the loader reads: {on_file:?}"
    );
    let _ = std::fs::remove_dir_all(env_path.parent().unwrap());
}

/// Issue #965's guard half: an EDITED file at the policy path is the
/// operator's own — the write still refuses (one source, issue #751), the
/// file stays untouched, and nothing lands in the env.
#[tokio::test]
async fn a_set_still_refuses_an_operator_edited_policy_file() {
    let (core, registry, peer, env_path) = policy_fixture().await;
    let toml_path = {
        let c = core.lock().await;
        std::path::PathBuf::from(c.config().execution_policy_path.clone().unwrap())
    };
    std::fs::write(
        &toml_path,
        "version = 1\n\n[defaults]\nbudget_ratio = 0.9\n",
    )
    .expect("seed an operator-edited file");

    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-o","budgetRatio":"0.05"}}"#.into())
        .await;
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("refusing the policy write"),
        "an operator file blocks the env write by name: {v}"
    );
    assert!(
        toml_path.exists(),
        "a refused write never touches the operator's file"
    );
    let on_file: std::collections::HashMap<String, String> = std::fs::read_to_string(&env_path)
        .expect("the env file")
        .lines()
        .filter_map(crate::env_file::parse_line)
        .collect();
    assert!(
        !on_file
            .keys()
            .any(|k| k.starts_with("POLICY_BUDGET_RATIO_ACCT_O")),
        "nothing landed in the env under the refusal: {on_file:?}"
    );
    let _ = std::fs::remove_dir_all(env_path.parent().unwrap());
}
