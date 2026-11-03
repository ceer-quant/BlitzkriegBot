use super::*;

const STRATEGY_LUA: &str =
    "function bk_evaluate() return { entries = {}, exits = {}, breaks = {} } end\n";

fn write_package(dir: &Path, name: &str, manifest_overrides: &[(&str, String)]) -> PathBuf {
    std::fs::create_dir_all(dir).expect("mkdir");
    std::fs::write(dir.join("strategy.lua"), STRATEGY_LUA).expect("write entry");
    std::fs::write(dir.join("README.md"), "# test package\n").expect("write readme");
    let digest = super::super::loader::sha256_file(&dir.join("strategy.lua")).expect("sha");
    let mut manifest = serde_json::json!({
        "name": name,
        "version": "0.1.0",
        "api": "1.0",
        "entry": "strategy.lua",
        "sha256": digest,
        "author": "test",
        "description": "test",
    });
    if let serde_json::Value::Object(map) = &mut manifest {
        for (k, v) in manifest_overrides {
            map.insert((*k).to_string(), serde_json::Value::String(v.clone()));
        }
    }
    std::fs::write(dir.join("manifest.json"), manifest.to_string()).expect("write manifest");
    dir.to_path_buf()
}

/// The happy path: a well-formed package loads and reports its identity.
#[test]
fn well_formed_package_loads() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-ok-{}", std::process::id()));
    let pkg = write_package(&tmp.join("lua_ok"), "lua_ok", &[]);
    let loaded = load_lua_package(&pkg).expect("loads");
    assert_eq!(loaded.name, "lua_ok");
    assert_eq!(loaded.version, "0.1.0");
    assert!(loaded.declared_modes.is_empty());
    std::fs::remove_dir_all(&tmp).ok();
}

/// The three-state trailing verdict rides the manifest verbatim: a JSON
/// boolean lands as `Some`, an absent key stays `None` (follow the
/// kernel-wide value) — never a silent default-to-on.
#[test]
fn manifest_trailing_exit_is_three_state() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-trail-{}", std::process::id()));
    let pkg_dir = tmp.join("trail_true");
    std::fs::create_dir_all(&pkg_dir).expect("mkdir");
    std::fs::write(pkg_dir.join("strategy.lua"), STRATEGY_LUA).expect("write entry");
    let digest = super::super::loader::sha256_file(&pkg_dir.join("strategy.lua")).expect("sha");
    let manifest = serde_json::json!({
        "name": "trail_true",
        "version": "0.1.0",
        "api": "1.0",
        "entry": "strategy.lua",
        "sha256": digest,
        "author": "test",
        "description": "test",
        "trailing_exit": true,
    });
    std::fs::write(pkg_dir.join("manifest.json"), manifest.to_string()).expect("write manifest");
    let loaded = load_lua_package(&pkg_dir).expect("loads");
    assert_eq!(
        loaded.trailing_exit,
        Some(true),
        "a declared boolean lands as Some"
    );

    // Absent key = None (the "follow the kernel value" state).
    let pkg_absent = write_package(&tmp.join("trail_absent"), "trail_absent", &[]);
    let loaded = load_lua_package(&pkg_absent).expect("loads");
    assert_eq!(
        loaded.trailing_exit, None,
        "an absent key stays None — never a silent default"
    );
    std::fs::remove_dir_all(&tmp).ok();
}

/// The hold-to-settlement declaration rides the manifest verbatim: a
/// declared `true` lands on the package (the pair-completion exemption
/// AND the wide containment stop both key off it), absent stays `false`
/// — never a silent default-to-holder.
#[test]
fn manifest_holds_to_settlement_is_read_verbatim() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-holds-{}", std::process::id()));
    let pkg_dir = tmp.join("holds_true");
    std::fs::create_dir_all(&pkg_dir).expect("mkdir");
    std::fs::write(pkg_dir.join("strategy.lua"), STRATEGY_LUA).expect("write entry");
    let digest = super::super::loader::sha256_file(&pkg_dir.join("strategy.lua")).expect("sha");
    let manifest = serde_json::json!({
        "name": "holds_true",
        "version": "0.1.0",
        "api": "1.0",
        "entry": "strategy.lua",
        "sha256": digest,
        "author": "test",
        "description": "test",
        "holds_to_settlement": true,
    });
    std::fs::write(pkg_dir.join("manifest.json"), manifest.to_string()).expect("write manifest");
    let loaded = load_lua_package(&pkg_dir).expect("loads");
    assert!(
        loaded.holds_to_settlement,
        "a declared true must land as a holder declaration"
    );

    // Absent key = false (the historical per-order exit ladder).
    let pkg_absent = write_package(&tmp.join("holds_absent"), "holds_absent", &[]);
    let loaded = load_lua_package(&pkg_absent).expect("loads");
    assert!(
        !loaded.holds_to_settlement,
        "an absent key stays false — never a silent default-to-holder"
    );
    std::fs::remove_dir_all(&tmp).ok();
}

/// §6.4: directory name ≠ manifest name → refuse (two identities).
#[test]
fn directory_name_mismatch_refuses() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-name-{}", std::process::id()));
    let pkg = write_package(&tmp.join("dir_a"), "manifest_b", &[]);
    let err = load_lua_package(&pkg).expect_err("refused");
    assert!(err.contains("two identities"), "got: {err}");
    std::fs::remove_dir_all(&tmp).ok();
}

/// Reverse acceptance F: a tampered sha256 refuses with BOTH values in
/// the message.
#[test]
fn sha256_mismatch_refuses_with_expected_and_actual() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-sha-{}", std::process::id()));
    let fake = "0".repeat(64);
    let pkg = write_package(&tmp.join("lua_sha"), "lua_sha", &[("sha256", fake.clone())]);
    let err = load_lua_package(&pkg).expect_err("refused");
    assert!(err.contains("sha256 mismatch"), "got: {err}");
    assert!(err.contains(&fake), "expected value present: {err}");
    // The ACTUAL digest is the real one of the untouched script.
    let actual = super::super::loader::sha256_file(&pkg.join("strategy.lua")).expect("sha");
    assert!(err.contains(&actual), "actual value present: {err}");
    std::fs::remove_dir_all(&tmp).ok();
}

/// A non-hex "sha256" is refused as malformed (not as a mismatch).
#[test]
fn malformed_sha256_refuses() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-badsha-{}", std::process::id()));
    let pkg = write_package(
        &tmp.join("lua_bad"),
        "lua_bad",
        &[("sha256", "nope".into())],
    );
    let err = load_lua_package(&pkg).expect_err("refused");
    assert!(err.contains("not a sha256 hex digest"), "got: {err}");
    std::fs::remove_dir_all(&tmp).ok();
}

/// An entry path escaping the package is refused.
#[test]
fn entry_path_escape_refuses() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-esc-{}", std::process::id()));
    let pkg = write_package(
        &tmp.join("lua_esc"),
        "lua_esc",
        &[("entry", "../other.lua".into())],
    );
    let err = load_lua_package(&pkg).expect_err("refused");
    assert!(err.contains("escapes the package"), "got: {err}");
    std::fs::remove_dir_all(&tmp).ok();
}

/// A wrong api version is refused.
#[test]
fn unsupported_api_refuses() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-api-{}", std::process::id()));
    let pkg = write_package(&tmp.join("lua_api"), "lua_api", &[("api", "2.0".into())]);
    let err = load_lua_package(&pkg).expect_err("refused");
    assert!(err.contains("unsupported"), "got: {err}");
    std::fs::remove_dir_all(&tmp).ok();
}

/// A script without `bk_evaluate` is refused (the required entry point).
#[test]
fn missing_evaluate_refuses() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-noeval-{}", std::process::id()));
    let pkg = write_package(&tmp.join("lua_no"), "lua_no", &[]);
    std::fs::write(pkg.join("strategy.lua"), "x = 1\n").expect("rewrite");
    // The manifest sha256 now matches the REWRITTEN file only if
    // recomputed — rewrite it so the refusal is about the entry point.
    let digest = super::super::loader::sha256_file(&pkg.join("strategy.lua")).expect("sha");
    let manifest_path = pkg.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("read"))
            .expect("json");
    manifest["sha256"] = serde_json::Value::String(digest);
    std::fs::write(&manifest_path, manifest.to_string()).expect("write");
    let err = load_lua_package(&pkg).expect_err("refused");
    assert!(err.contains("bk_evaluate"), "got: {err}");
    std::fs::remove_dir_all(&tmp).ok();
}

/// Discovery lists only manifest-carrying directories, sorted.
#[test]
fn discovery_lists_packages_only() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-disc-{}", std::process::id()));
    std::fs::create_dir_all(tmp.join("plain_dir")).expect("mkdir");
    write_package(&tmp.join("zeta"), "zeta", &[]);
    write_package(&tmp.join("alpha"), "alpha", &[]);
    std::fs::write(tmp.join("loose.txt"), "not a package").expect("write");
    let found = discover_lua_packages(&tmp);
    let names: Vec<String> = found
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["alpha", "zeta"], "sorted, packages only");
    std::fs::remove_dir_all(&tmp).ok();
}

/// #393: rewrite one test package's manifest through `f` after the base
/// write (the entry file is untouched, so the sha256 stays valid).
fn with_manifest(dir: &Path, f: impl FnOnce(&mut serde_json::Value)) {
    let manifest_path = dir.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("read"))
            .expect("json");
    f(&mut manifest);
    std::fs::write(&manifest_path, manifest.to_string()).expect("write");
}

/// #393: a tunable that declares a coherent domain opts the strategy into
/// evolution — it becomes a knob with that domain, the factory is present,
/// and the factory builds an independent twin of the SAME strategy seeded
/// with the counterfactual values. A tunable without a domain (here the
/// string one) must NOT become a knob.
#[test]
fn tunables_with_domains_become_evolvable() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-evo-{}", std::process::id()));
    let pkg = write_package(&tmp.join("lua_evo"), "lua_evo", &[]);
    with_manifest(&pkg, |m| {
        m["tunables"] = serde_json::json!({
            "entry_max_price": { "type": "decimal", "default": "0.40", "min": "0.05", "max": "0.95" },
            "label": { "type": "string", "default": "x" }
        });
    });
    let loaded = load_lua_package(&pkg).expect("loads");
    let adapter = LuaEngineAdapter::new(loaded.strategy, loaded.tunables);
    let knobs = adapter.evolvable_knobs();
    assert_eq!(
        knobs.len(),
        1,
        "only the domain-declaring tunable is a knob"
    );
    assert_eq!(knobs[0].name, "entry_max_price");
    assert_eq!(knobs[0].min, rust_decimal_macros::dec!(0.05));
    assert_eq!(knobs[0].max, rust_decimal_macros::dec!(0.95));

    let factory = adapter.shadow_factory().expect("factory present");
    let mut params = StrategyParams::new();
    params.set("entry_max_price", rust_decimal_macros::dec!(0.55));
    let twin = factory.make(&params).expect("twin builds");
    assert_eq!(twin.name(), "lua_evo", "the twin is the same strategy");
    std::fs::remove_dir_all(&tmp).ok();
}

/// #393: a tunable with NO domain stays a plain `bk.params()` entry — the
/// package remains not-evolvable, exactly as every shipped package is
/// today (fail-closed; the kernel never invents a domain).
#[test]
fn tunables_without_domains_stay_not_evolvable() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-noevo-{}", std::process::id()));
    let pkg = write_package(&tmp.join("lua_noevo"), "lua_noevo", &[]);
    with_manifest(&pkg, |m| {
        m["tunables"] = serde_json::json!({
            "entry_max_price": { "type": "decimal", "default": "0.40" }
        });
    });
    let loaded = load_lua_package(&pkg).expect("loads");
    let adapter = LuaEngineAdapter::new(loaded.strategy, loaded.tunables);
    assert!(adapter.evolvable_knobs().is_empty(), "no domain, no knob");
    assert!(adapter.shadow_factory().is_none(), "no knobs, no factory");
    std::fs::remove_dir_all(&tmp).ok();
}

/// #393: an incoherent domain (`min > max`) is skipped, not patched — the
/// declaration is the author's, and a bounds the guard would refuse is
/// not silently turned into one it would accept.
#[test]
fn incoherent_domain_is_skipped() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-baddom-{}", std::process::id()));
    let pkg = write_package(&tmp.join("lua_baddom"), "lua_baddom", &[]);
    with_manifest(&pkg, |m| {
        m["tunables"] = serde_json::json!({
            "entry_max_price": { "type": "decimal", "default": "0.40", "min": "0.95", "max": "0.05" }
        });
    });
    let loaded = load_lua_package(&pkg).expect("loads");
    let adapter = LuaEngineAdapter::new(loaded.strategy, loaded.tunables);
    assert!(
        adapter.evolvable_knobs().is_empty(),
        "incoherent domain is not a knob"
    );
    assert!(adapter.shadow_factory().is_none());
    std::fs::remove_dir_all(&tmp).ok();
}

/// The blueprint.save export shape (用户裁决：蓝图生产的策略也要支持进化)
/// lands as evolvable knobs end to end: the compiler emits
/// `{"<node>_<param>": {type: "decimal", default, min, max}}` — a NUMBER
/// default serialized as a string — and the loader must register each
/// coherent one, name for name, so a saved blueprint is evolution-ready
/// without a hand-written manifest.
#[test]
fn blueprint_save_tunables_register_as_evolvable_knobs() {
    let tmp = std::env::temp_dir().join(format!("bk-lua-bptun-{}", std::process::id()));
    let pkg = write_package(&tmp.join("lua_bptun"), "lua_bptun", &[]);
    with_manifest(&pkg, |m| {
        m["tunables"] = serde_json::json!({
            "n2_value": { "type": "decimal", "default": "0.25", "min": "0.125", "max": "0.375" },
            "n4_price": { "type": "decimal", "default": "0.25", "min": "0.125", "max": "0.375" },
            "n4_budget_ratio": { "type": "decimal", "default": "0.1", "min": "0.05", "max": "0.15" },
        });
    });
    let loaded = load_lua_package(&pkg).expect("loads");
    let adapter = LuaEngineAdapter::new(loaded.strategy, loaded.tunables);
    let names: Vec<String> = adapter
        .evolvable_knobs()
        .into_iter()
        .map(|k| k.name)
        .collect();
    assert_eq!(
        names,
        vec!["n2_value", "n4_budget_ratio", "n4_price"],
        "BTreeMap order: every blueprint anchor is a knob"
    );
    assert!(adapter.shadow_factory().is_some(), "knobs ⇒ twin factory");
    std::fs::remove_dir_all(&tmp).ok();
}
