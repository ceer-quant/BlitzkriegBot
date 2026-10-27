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
