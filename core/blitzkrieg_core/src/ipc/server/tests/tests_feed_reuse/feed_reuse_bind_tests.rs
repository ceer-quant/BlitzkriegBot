use super::*;

/// M2 wire test 1 — COLD consumer-only boot. The consumer never booted
/// (boot actives are empty); binding its account at runtime must (a)
/// land the bind, (b) mount the SOURCE's data plane through the reuse
/// path, (c) mount NOTHING for the consumer itself (no feed control
/// under the consumer's key — the fan-out is its registrar), and (d)
/// count exactly the ONE upstream stream's components. The source's
/// EXECUTOR is never in question: enable_venue_at_runtime never mounts
/// executors, and the source reaches it as a data-only registry fetch.
#[tokio::test]
async fn a_consumer_only_bind_mounts_the_source_data_plane_once_and_never_the_consumer() {
    let (core, registry, peer, host, data_plane, source, consumer) =
        reuse_fixture("cold", &[]).await;
    let line = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"account.venue.set","params":{{"accountId":"paper","venue":"{consumer}","reason":"consumer bind"}}}}"#
    );
    let reply = rpc_with_data_plane(&core, &registry, &peer, &host, &data_plane, line).await;
    assert!(reply.get("error").is_none(), "the bind must land: {reply}");
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(true),
        "the consumer's enablement mounted its upstream: {reply}"
    );

    // (b) the source's feed really started — its subscription control is
    // reachable under the SOURCE's key.
    core.lock()
        .await
        .subscribe_feed_tokens(&source, vec!["tok-a".to_string()])
        .await;
    // (d) exactly one upstream feed mount for the reused stream. The
    // start log is a process-global shared cell (sibling tests add their
    // own entries concurrently) — read only THIS fixture's slice: the
    // CountedFeed logs under its owner (= the source's unique tag).
    let starts = COUNTED_FEED_STARTS
        .lock()
        .expect("counted feed starts poisoned")
        .iter()
        .filter(|o| **o == source)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        starts,
        vec![source.clone()],
        "the reused stream's feed mounts EXACTLY once, under the source: {starts:?}"
    );
    // The discovery family rides the source too (feed + discovery are
    // the two components of the ONE upstream stream) — one family (the
    // boot's global 900s), tagged with the source.
    let disc = VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .iter()
        .filter(|(tag, _)| tag == &source)
        .count();
    assert_eq!(
        disc, 1,
        "the ONE upstream stream's discovery family mounts once: {disc}"
    );
    // (c) the consumer mounted no component of its own.
    assert!(
        !data_plane.venue_mounted(&consumer),
        "the consumer's own data plane must never mount — the fan-out is its registrar"
    );
    // The reuse cell carries the declared edge, and the mount counter
    // reads 2 (one feed + one discovery family — the ONE stream's two
    // components). The counter is per-fixture (each test assembles its
    // own cell), so the absolute read is safe.
    let reuse = data_plane.feed_reuse().expect("the boot cell");
    assert!(
        reuse.consumes(&consumer, &source),
        "the runtime bind appended the declared edge"
    );
    assert_eq!(
        reuse.mounts.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "one feed + one discovery = the ONE upstream stream's components"
    );
}

/// M2 wire test 2 — RUNTIME bind onto a RUNNING source. The source
/// booted (its components are live); binding a consumer must append the
/// declared edge and NOT restart anything: the feed start count stays at
/// its boot value, no new discovery instance appears (the per-(source,
/// family) guard), and the enablement still lands.
#[tokio::test]
async fn a_runtime_bound_consumer_on_a_running_source_re_subscribes_nothing() {
    let (core, registry, peer, host, data_plane, source, consumer) =
        reuse_fixture("warm", &["src-warm"]).await;
    let line = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"account.venue.set","params":{{"accountId":"paper","venue":"{consumer}","reason":"consumer bind onto running source"}}}}"#
    );
    let reply = rpc_with_data_plane(&core, &registry, &peer, &host, &data_plane, line).await;
    assert!(reply.get("error").is_none(), "the bind must land: {reply}");
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(true),
        "the consumer's enablement is recorded: {reply}"
    );

    // The running source was NOT restarted: the boot's feed start (if the
    // log even captured it — the fixture mounts at_boot, before this test
    // can observe) is followed by ZERO new starts for this source, and
    // zero new discovery instances. The log is a process-global shared
    // cell — filter to THIS fixture's tag.
    let starts = COUNTED_FEED_STARTS
        .lock()
        .expect("counted feed starts poisoned")
        .iter()
        .filter(|o| **o == source)
        .count();
    assert!(
        starts <= 1,
        "a consumer bind onto a running source must not restart the upstream: {starts}"
    );
    let disc = VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .iter()
        .filter(|(tag, _)| tag == &source)
        .count();
    assert_eq!(
        disc, 0,
        "no discovery instance may re-mount (the fixture's at_boot pre-records the family \
         without starting it): {disc}"
    );
    // The edge appended (the fan-out now re-registers the source's
    // batches under the consumer), and the counter did not move.
    let reuse = data_plane.feed_reuse().expect("the boot cell");
    assert!(reuse.consumes(&consumer, &source));
    assert_eq!(
        reuse.mounts.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "no mount was attempted — the source was already running"
    );
}

/// M2 wire test 3 — 停用不重订 / 重启不重订. After a consumer binds
/// (mounting the source once), UNBINDING the account must not touch the
/// upstream (the source's components keep running — another consumer or
/// a re-bind may need them), and RE-BINDING the same consumer must mount
/// nothing new: the feed start count stays 1 forever, the declared edge
/// stays a single entry.
#[tokio::test]
async fn consumer_unbind_and_rebind_never_re_subscribe_the_upstream() {
    let (core, registry, peer, host, data_plane, source, consumer) =
        reuse_fixture("rebind", &[]).await;
    let bind = |id: u32| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"account.venue.set","params":{{"accountId":"paper","venue":"{consumer}","reason":"lifecycle"}}}}"#
        )
    };
    let unbind = r#"{"jsonrpc":"2.0","id":9,"method":"account.venue.set","params":{"accountId":"paper","venue":"","reason":"unbind"}}"#.to_string();

    // Bind #1: mounts the source once.
    let reply = rpc_with_data_plane(&core, &registry, &peer, &host, &data_plane, bind(1)).await;
    assert!(reply.get("error").is_none(), "bind 1 must land: {reply}");
    // Unbind: lands, upstream untouched.
    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        unbind.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "unbind must land: {reply}");
    // Re-bind: idempotent, no second mount.
    let reply = rpc_with_data_plane(&core, &registry, &peer, &host, &data_plane, bind(2)).await;
    assert!(reply.get("error").is_none(), "re-bind must land: {reply}");

    // The start log is a process-global shared cell — filter to THIS
    // fixture's tag: bind → unbind → re-bind keeps the upstream at ONE
    // start.
    let starts = COUNTED_FEED_STARTS
        .lock()
        .expect("counted feed starts poisoned")
        .iter()
        .filter(|o| **o == source)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        starts,
        vec![source.clone()],
        "bind → unbind → re-bind keeps the upstream at ONE start: {starts:?}"
    );
    let reuse = data_plane.feed_reuse().expect("the boot cell");
    assert_eq!(
        reuse
            .consumers_of(&source)
            .iter()
            .filter(|v| **v == consumer)
            .count(),
        1,
        "the declared edge is ONE entry, never duplicated by the lifecycle"
    );
}
