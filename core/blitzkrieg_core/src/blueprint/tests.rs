use super::*;
use serde_json::json;

/// The spec's four-node example: data_source → condition → condition →
/// action_buy. The happy path every other test mutates.
const DOG_BLUEPRINT: &str = r#"{
    "version": 1,
    "name": "dog_strategy",
    "nodes": [
        { "id": "n1", "type": "data_source", "params": { "field": "tick.price" } },
        { "id": "n2", "type": "condition", "params": { "op": "<=", "value": 0.25 } },
        { "id": "n3", "type": "condition", "params": { "field": "tick.trend_confirmed", "op": "==", "value": true } },
        { "id": "n4", "type": "action_buy", "params": { "price": 0.25, "budget_ratio": 0.10 } }
    ],
    "edges": [
        { "from": "n1", "to": "n2", "when": true },
        { "from": "n2", "to": "n3", "when": true },
        { "from": "n3", "to": "n4", "when": true }
    ]
}"#;

// ── positive ────────────────────────────────────────────────────────────

#[test]
fn a_four_node_blueprint_compiles_to_on_tick_and_declare_modes() {
    let lua = compile(DOG_BLUEPRINT).expect("the spec example compiles");
    assert!(
        lua.contains("function on_tick(tick)"),
        "entry point present"
    );
    assert!(
        lua.contains("function declare_modes()"),
        "mode declaration present"
    );
    assert!(
        lua.contains("tick.price <= __param(\"n2_value\", 0.25)"),
        "guard chain: {lua}"
    );
    assert!(
        lua.contains("tick.trend_confirmed == true"),
        "inline-field condition: {lua}"
    );
    assert!(lua.contains("side = \"buy\""), "action translated: {lua}");
    assert!(
        lua.contains("tick.available_balance * __param(\"n4_budget_ratio\", 0.1)"),
        "budget line: {lua}"
    );
}

/// #413 runtime bridge: the emitted package defines the REQUIRED §6.5
/// entry point `bk_evaluate` plus its own `place_order` collector —
/// without them the loader refuses the package (`bk_evaluate missing`)
/// and an action would hit a nil global at eval time. The collector row
/// is the §6.5 entry wire shape (token/price/reason strings).
#[test]
fn generated_package_carries_the_evaluate_bridge() {
    let lua = compile(DOG_BLUEPRINT).expect("compiles");
    assert!(
        lua.contains("function bk_evaluate()"),
        "the §6.5 entry point must be emitted: {lua}"
    );
    assert!(
        lua.contains("local function place_order("),
        "the collector must be package-local (not a sandbox global): {lua}"
    );
    assert!(
        lua.contains("return { entries = __orders, exits = {}, breaks = {} }"),
        "the evaluate return must be the §6.5 intent-table shape: {lua}"
    );
    // The bk.* surface speaks decimal STRINGS while generated guards
    // compare numbers — without the tonumber coercion every guard on a
    // numeric field would error the evaluation (live-proven: a string
    // `<=` a number raises and the entries come back empty).
    assert!(
        lua.contains("t.price = tonumber(b.best_ask) or tonumber(b.mid)"),
        "price must be coerced to a number: {lua}"
    );
    assert!(
        lua.contains("t.available_balance = tonumber(a and (a.available or a.balance)) or 0"),
        "balance must be coerced and nil-safe for budget_ratio math: {lua}"
    );
    // The loop iterates BOTH legs; each tick must carry the leg's own
    // token and book, not unconditionally the up-token's.
    assert!(
        lua.contains("local tick = __tick_of(m, token)"),
        "the tick must be built from the iterated leg's token: {lua}"
    );
    assert!(
        !lua.contains("t.symbol = m.up_token"),
        "symbol must come from the iterated token: {lua}"
    );
}

/// #361 acceptance: the generated Lua loads in the EXISTING sandbox and
/// both entry points exist afterwards. `blitzkrieg-lua-runtime` is
/// already a (non-dev) dependency of this crate (the E30 loader seam),
/// so the load test runs against the real §6.2 sandbox here — the same
/// surface `LuaStrategy::build` would refuse on.
#[test]
fn generated_lua_loads_in_the_existing_sandbox() {
    let lua = compile(DOG_BLUEPRINT).expect("compiles");
    let sb = blitzkrieg_lua_runtime::sandbox::LuaSandbox::new().expect("sandbox builds");
    sb.load(&lua, "blueprint_test")
        .expect("generated Lua must load in the §6.2 sandbox");
    let on_tick = sb.global_function("on_tick").expect("globals readable");
    assert!(on_tick.is_some(), "on_tick must be defined");
    let modes = sb
        .global_function("declare_modes")
        .expect("globals readable");
    assert!(modes.is_some(), "declare_modes must be defined");
}

/// #413 end-to-end: a generated package passes the REAL load contract —
/// `blitzkrieg_lua_runtime::LuaStrategy::build` is the exact check the
/// E30 loader runs (`bk_evaluate` required). Before the bridge this test
/// refused with "required entry point `bk_evaluate` is not defined".
#[test]
fn generated_package_passes_the_real_lua_strategy_build() {
    let lua = compile(DOG_BLUEPRINT).expect("compiles");
    let strategy = blitzkrieg_lua_runtime::LuaStrategy::build(
        "blueprint_bridge_test".into(),
        "1.0.0".into(),
        Vec::new(),
        &lua,
    );
    assert!(
        strategy.is_ok(),
        "generated package must pass the loader contract: {:?}",
        strategy.err()
    );
}

#[test]
fn logic_gates_join_conditions_into_a_parenthesised_guard() {
    // data_source → (cond A, cond B) → logic_and → action_sell
    let bp = json!({
        "version": 1, "name": "gate_test",
        "nodes": [
            { "id": "s1", "type": "data_source", "params": { "field": "tick.mid_price" } },
            { "id": "c1", "type": "condition", "params": { "op": ">", "value": 0.8 } },
            { "id": "s2", "type": "data_source", "params": { "field": "tick.time_left_sec" } },
            { "id": "c2", "type": "condition", "params": { "op": "<", "value": 60 } },
            { "id": "g1", "type": "logic_and" },
            { "id": "a1", "type": "action_sell" }
        ],
        "edges": [
            { "from": "s1", "to": "c1" }, { "from": "s2", "to": "c2" },
            { "from": "c1", "to": "g1" }, { "from": "c2", "to": "g1" },
            { "from": "g1", "to": "a1" }
        ]
    })
    .to_string();
    let lua = compile(&bp).expect("gate blueprint compiles");
    assert!(
        lua.contains(
            "if (tick.mid_price > __param(\"c1_value\", 0.8) and tick.time_left_sec < __param(\"c2_value\", 60)) then"
        ),
        "and-gate guard: {lua}"
    );
    assert!(lua.contains("side = \"sell\""), "sell side: {lua}");
}

#[test]
fn logic_or_and_an_inverted_edge_compile_to_not_or() {
    let bp = json!({
        "version": 1, "name": "or_test",
        "nodes": [
            { "id": "s1", "type": "data_source", "params": { "field": "tick.obi" } },
            { "id": "c1", "type": "condition", "params": { "op": ">", "value": 0.5 } },
            { "id": "c2", "type": "condition", "params": { "field": "tick.trend_confirmed", "op": "!=", "value": true } },
            { "id": "g1", "type": "logic_or" },
            { "id": "a1", "type": "action_hold" }
        ],
        "edges": [
            { "from": "s1", "to": "c1" },
            { "from": "c1", "to": "g1", "when": true },
            { "from": "c2", "to": "g1", "when": false },
            { "from": "g1", "to": "a1" }
        ]
    })
    .to_string();
    let lua = compile(&bp).expect("or blueprint compiles");
    // c2's edge carries when=false → negated at the join; c1 passes as-is.
    assert!(
        lua.contains(
            "if (tick.obi > __param(\"c1_value\", 0.5) or not (tick.trend_confirmed != true)) then"
        ),
        "or-gate with one inverted edge: {lua}"
    );
    // action_hold emits the guard but no order — the bridge collector
    // exists in every package, so assert no SUGGESTION call site: the
    // on_tick body never appends an order for a hold.
    assert!(
        !lua.contains("side = \"buy\"") && !lua.contains("side = \"sell\""),
        "hold places no order: {lua}"
    );
}

#[test]
fn the_in_operator_expands_to_an_equality_chain() {
    let bp = json!({
        "version": 1, "name": "in_test",
        "nodes": [
            { "id": "s1", "type": "data_source", "params": { "field": "tick.time_left_sec" } },
            { "id": "c1", "type": "condition", "params": { "op": "in", "value": [60, 90] } },
            { "id": "a1", "type": "action_sell" }
        ],
        "edges": [ { "from": "s1", "to": "c1" }, { "from": "c1", "to": "a1" } ]
    })
    .to_string();
    let lua = compile(&bp).expect("in blueprint compiles");
    assert!(
        lua.contains("(tick.time_left_sec == 60 or tick.time_left_sec == 90)"),
        "in expands: {lua}"
    );
}

#[test]
fn a_constant_operand_feeds_a_condition() {
    let bp = json!({
        "version": 1, "name": "const_test",
        "nodes": [
            { "id": "k1", "type": "constant", "params": { "value": 0.5 } },
            { "id": "c1", "type": "condition", "params": { "op": "<", "value": 0.25 } },
            { "id": "a1", "type": "action_buy" }
        ],
        "edges": [ { "from": "k1", "to": "c1" }, { "from": "c1", "to": "a1" } ]
    })
    .to_string();
    let lua = compile(&bp).expect("constant blueprint compiles");
    assert!(
        lua.contains("__param(\"k1_value\", 0.5) < __param(\"c1_value\", 0.25)"),
        "constant operand: {lua}"
    );
}

/// 用户裁决（2026-10-08）：蓝图生产的策略也要支持进化 —— the compiled
/// Lua serves numeric anchors from `bk.params()` with the authored
/// literal as fallback, and the export table carries one coherent domain
/// per anchor so the loader's `specs_from_tunables` registers them
/// (fail-closed #393: no domain = no evolution; here the domain ALWAYS
/// ships). Booleans stay plain literals — behaviour, not calibration.
#[test]
fn numeric_anchors_export_as_tunables_and_read_params_with_fallback() {
    let (lua, tunables) = compile_with_tunables(DOG_BLUEPRINT).expect("compiles");
    // The guard and the budget read params; the boolean condition stays
    // a plain literal.
    assert!(
        lua.contains("tick.price <= __param(\"n2_value\", 0.25)"),
        "condition value via __param: {lua}"
    );
    assert!(
        lua.contains("tick.available_balance * __param(\"n4_budget_ratio\", 0.1)"),
        "budget_ratio via __param: {lua}"
    );
    assert!(
        lua.contains("tick.trend_confirmed == true"),
        "boolean stays a literal: {lua}"
    );
    assert!(
        lua.contains("local function __param(name, fallback)"),
        "the evolution bridge is emitted: {lua}"
    );
    // The export: three knobs (condition value, price, budget_ratio),
    // each with a coherent ±half-magnitude domain containing its default.
    let names: Vec<&str> = tunables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["n2_value", "n4_price", "n4_budget_ratio"],
        "one knob per numeric anchor: {names:?}"
    );
    for t in &tunables {
        let (d, min, max) = (
            t.default.parse::<f64>().expect("default parses"),
            t.min.parse::<f64>().expect("min parses"),
            t.max.parse::<f64>().expect("max parses"),
        );
        assert!(
            min <= d && d <= max,
            "domain must contain the default: {t:?}"
        );
        assert!(min < max, "domain must be coherent: {t:?}");
    }
    // Zero widens to the 0..1 probability band (0 has no magnitude).
    let zero_bp = json!({
        "version": 1, "name": "zero_test",
        "nodes": [
            { "id": "k1", "type": "constant", "params": { "value": 0 } },
            { "id": "c1", "type": "condition", "params": { "op": "==", "value": 0 } },
            { "id": "a1", "type": "action_hold" }
        ],
        "edges": [ { "from": "k1", "to": "c1" }, { "from": "c1", "to": "a1" } ]
    })
    .to_string();
    let (_, tunables) = compile_with_tunables(&zero_bp).expect("compiles");
    let k = tunables
        .iter()
        .find(|t| t.name == "k1_value")
        .expect("zero anchor exports");
    assert_eq!(
        (k.min.as_str(), k.max.as_str()),
        ("0.000000000000", "1.000000000000")
    );
}

// ── negative: structure ─────────────────────────────────────────────────

#[test]
fn a_blueprint_with_a_cycle_fails_structure() {
    let bp = json!({
        "version": 1, "name": "cycle",
        "nodes": [
            { "id": "n1", "type": "data_source", "params": { "field": "tick.price" } },
            { "id": "n2", "type": "condition", "params": { "op": "<", "value": 1 } },
            { "id": "n3", "type": "condition", "params": { "op": ">", "value": 0 } },
            { "id": "n4", "type": "action_buy" }
        ],
        "edges": [
            { "from": "n1", "to": "n2" },
            { "from": "n2", "to": "n3" },
            { "from": "n3", "to": "n2" },
            { "from": "n3", "to": "n4" }
        ]
    })
    .to_string();
    let err = compile(&bp).expect_err("a cycle must fail");
    assert!(err.contains("cycle"), "names the problem: {err}");
    assert!(err.contains("n2"), "names a node id on the cycle: {err}");
}

#[test]
fn a_self_loop_is_refused_as_a_cycle() {
    let bp = json!({
        "version": 1, "name": "self_loop",
        "nodes": [
            { "id": "n1", "type": "data_source", "params": { "field": "tick.price" } },
            { "id": "n2", "type": "action_buy" }
        ],
        "edges": [
            { "from": "n1", "to": "n2" },
            { "from": "n2", "to": "n2" }
        ]
    })
    .to_string();
    let err = compile(&bp).expect_err("a self-loop must fail");
    assert!(err.contains("self-loop") && err.contains("n2"), "{err}");
}

#[test]
fn an_orphan_node_is_refused() {
    let bp = json!({
        "version": 1, "name": "orphan",
        "nodes": [
            { "id": "n1", "type": "data_source", "params": { "field": "tick.price" } },
            { "id": "n2", "type": "condition", "params": { "op": "<", "value": 1 } },
            { "id": "n3", "type": "action_buy" },
            { "id": "dead", "type": "condition", "params": { "op": ">", "value": 0 } }
        ],
        "edges": [
            { "from": "n1", "to": "n2" },
            { "from": "n2", "to": "n3" }
        ]
    })
    .to_string();
    let err = compile(&bp).expect_err("an orphan must fail");
    assert!(err.contains("orphan") && err.contains("dead"), "{err}");
}

#[test]
fn a_blueprint_with_no_action_output_fails() {
    let bp = json!({
        "version": 1, "name": "no_action",
        "nodes": [
            { "id": "n1", "type": "data_source", "params": { "field": "tick.price" } },
            { "id": "n2", "type": "condition", "params": { "op": "<", "value": 1 } }
        ],
        "edges": [ { "from": "n1", "to": "n2" } ]
    })
    .to_string();
    let err = compile(&bp).expect_err("no action must fail");
    assert!(err.contains("no reachable action output"), "{err}");
}

#[test]
fn two_reachable_action_outputs_fail() {
    let bp = json!({
        "version": 1, "name": "two_actions",
        "nodes": [
            { "id": "n1", "type": "data_source", "params": { "field": "tick.price" } },
            { "id": "n2", "type": "condition", "params": { "op": "<", "value": 1 } },
            { "id": "n3", "type": "action_buy" },
            { "id": "n4", "type": "action_sell" }
        ],
        "edges": [
            { "from": "n1", "to": "n2" },
            { "from": "n2", "to": "n3" },
            { "from": "n2", "to": "n4" }
        ]
    })
    .to_string();
    let err = compile(&bp).expect_err("two actions must fail");
    assert!(
        err.contains("2 reachable action outputs") && err.contains("n3") && err.contains("n4"),
        "{err}"
    );
}

#[test]
fn an_action_with_an_outgoing_edge_is_refused() {
    let bp = json!({
        "version": 1, "name": "action_continues",
        "nodes": [
            { "id": "n1", "type": "data_source", "params": { "field": "tick.price" } },
            { "id": "n2", "type": "action_buy" },
            { "id": "n3", "type": "action_sell" }
        ],
        "edges": [ { "from": "n1", "to": "n2" }, { "from": "n2", "to": "n3" } ]
    })
    .to_string();
    let err = compile(&bp).expect_err("a non-terminal action must fail");
    assert!(err.contains("n2") && err.contains("terminal"), "{err}");
}

#[test]
fn more_than_max_nodes_is_refused() {
    // 101 conditions chained: a straight line, no cycle, one action.
    let mut nodes = vec![json!({
        "id": "s0", "type": "data_source", "params": { "field": "tick.price" }
    })];
    let mut edges = Vec::new();
    for i in 1..=100 {
        nodes.push(json!({
            "id": format!("c{i}"), "type": "condition",
            "params": { "op": "<", "value": 1 }
        }));
        edges.push(json!({
            "from": if i == 1 { "s0".to_string() } else { format!("c{}", i - 1) },
            "to": format!("c{i}")
        }));
    }
    // 101 nodes total: s0 + 100 conditions. Swap the last condition for
    // the action so the shape stays legal up to the node ceiling.
    let last = nodes.last_mut().expect("non-empty");
    *last = json!({ "id": "c100", "type": "action_buy" });
    edges.pop();
    edges.push(json!({ "from": "c99", "to": "c100" }));
    let bp =
        json!({ "version": 1, "name": "too_many", "nodes": nodes, "edges": edges }).to_string();
    let err = compile(&bp).expect_err("101 nodes must fail");
    assert!(
        err.contains("101 nodes") && err.contains(MAX_NODES.to_string().as_str()),
        "{err}"
    );
}

#[test]
fn depth_past_the_ceiling_is_refused() {
    // A 21-node chain: legal node count, illegal depth.
    let mut nodes = vec![json!({
        "id": "s0", "type": "data_source", "params": { "field": "tick.price" }
    })];
    let mut edges = Vec::new();
    for i in 1..=20 {
        let ty = if i == 20 { "action_buy" } else { "condition" };
        nodes.push(json!({
            "id": format!("c{i}"), "type": ty,
            "params": if ty == "condition" {
                json!({ "op": "<", "value": 1 })
            } else {
                json!({})
            }
        }));
        edges.push(json!({
            "from": if i == 1 { "s0".to_string() } else { format!("c{}", i - 1) },
            "to": format!("c{i}")
        }));
    }
    let bp =
        json!({ "version": 1, "name": "too_deep", "nodes": nodes, "edges": edges }).to_string();
    let err = compile(&bp).expect_err("depth 21 must fail");
    assert!(err.contains("depth 21") && err.contains("20"), "{err}");
}

#[test]
fn an_unknown_node_type_is_refused_with_the_id() {
    let bp = json!({
        "version": 1, "name": "bad_type",
        "nodes": [
            { "id": "n1", "type": "data_source", "params": { "field": "tick.price" } },
            { "id": "n2", "type": "send_email" }
        ],
        "edges": [ { "from": "n1", "to": "n2" } ]
    })
    .to_string();
    let err = compile(&bp).expect_err("unknown type must fail");
    assert!(err.contains("n2") && err.contains("send_email"), "{err}");
}

#[test]
fn a_wrong_version_is_refused() {
    let bp = json!({
        "version": 2, "name": "future",
        "nodes": [ { "id": "a1", "type": "action_hold" } ],
        "edges": []
    })
    .to_string();
    let err = compile(&bp).expect_err("version 2 must fail");
    assert!(err.contains("version"), "{err}");
}

// ── negative: semantics ─────────────────────────────────────────────────

#[test]
fn a_non_whitelisted_field_fails_semantics() {
    let bp = json!({
        "version": 1, "name": "bad_field",
        "nodes": [
            { "id": "n1", "type": "data_source", "params": { "field": "tick.close" } },
            { "id": "n2", "type": "condition", "params": { "op": "<", "value": 1 } },
            { "id": "n3", "type": "action_buy" }
        ],
        "edges": [ { "from": "n1", "to": "n2" }, { "from": "n2", "to": "n3" } ]
    })
    .to_string();
    let err = compile(&bp).expect_err("tick.close is not whitelisted");
    assert!(err.contains("n1") && err.contains("tick.close"), "{err}");
}

#[test]
fn a_non_whitelisted_operator_fails_semantics() {
    let bp = json!({
        "version": 1, "name": "bad_op",
        "nodes": [
            { "id": "n1", "type": "data_source", "params": { "field": "tick.price" } },
            { "id": "n2", "type": "condition", "params": { "op": "=~", "value": 1 } },
            { "id": "n3", "type": "action_buy" }
        ],
        "edges": [ { "from": "n1", "to": "n2" }, { "from": "n2", "to": "n3" } ]
    })
    .to_string();
    let err = compile(&bp).expect_err("=~ must be refused");
    assert!(err.contains("n2") && err.contains("=~"), "{err}");
}

#[test]
fn a_field_smelling_of_os_execute_is_refused() {
    for field in ["os.execute", "io.open", "debug.getinfo", "tick.os.execute"] {
        let bp = json!({
            "version": 1, "name": "escape",
            "nodes": [
                { "id": "n1", "type": "data_source", "params": { "field": field } },
                { "id": "n2", "type": "condition", "params": { "op": "<", "value": 1 } },
                { "id": "n3", "type": "action_buy" }
            ],
            "edges": [ { "from": "n1", "to": "n2" }, { "from": "n2", "to": "n3" } ]
        })
        .to_string();
        let err = compile(&bp).expect_err("a forbidden field must fail");
        assert!(
            err.contains("n1") && err.contains("refused"),
            "{field}: {err}"
        );
    }
}

#[test]
fn a_forbidden_substring_in_a_string_literal_is_refused() {
    let bp = json!({
        "version": 1, "name": "sneaky",
        "nodes": [
            { "id": "k1", "type": "constant", "params": { "value": "os.execute('rm -rf /')" } },
            { "id": "c1", "type": "condition", "params": { "op": "==", "value": "x" } },
            { "id": "a1", "type": "action_hold" }
        ],
        "edges": [ { "from": "k1", "to": "c1" }, { "from": "c1", "to": "a1" } ]
    })
    .to_string();
    let err = compile(&bp).expect_err("an os.-bearing constant must fail");
    assert!(err.contains("k1"), "{err}");
}

#[test]
fn a_condition_with_no_operand_source_is_refused() {
    let bp = json!({
        "version": 1, "name": "no_operand",
        "nodes": [
            { "id": "c1", "type": "condition", "params": { "op": "<", "value": 1 } },
            { "id": "a1", "type": "action_buy" }
        ],
        "edges": [ { "from": "c1", "to": "a1" } ]
    })
    .to_string();
    let err = compile(&bp).expect_err("an operand-less condition must fail");
    assert!(err.contains("c1") && err.contains("operand"), "{err}");
}

#[test]
fn a_condition_fed_by_another_condition_is_refused() {
    let bp = json!({
        "version": 1, "name": "bool_operand",
        "nodes": [
            { "id": "s1", "type": "data_source", "params": { "field": "tick.price" } },
            { "id": "c1", "type": "condition", "params": { "op": "<", "value": 1 } },
            { "id": "c2", "type": "condition", "params": { "op": "<", "value": 2 } },
            { "id": "a1", "type": "action_buy" }
        ],
        "edges": [
            { "from": "s1", "to": "c1" }, { "from": "c1", "to": "c2" },
            { "from": "c2", "to": "a1" }
        ]
    })
    .to_string();
    let err = compile(&bp).expect_err("a boolean operand must fail");
    assert!(err.contains("c2") && err.contains("c1"), "{err}");
}

#[test]
fn a_gate_with_one_input_is_refused() {
    let bp = json!({
        "version": 1, "name": "lone_gate",
        "nodes": [
            { "id": "s1", "type": "data_source", "params": { "field": "tick.price" } },
            { "id": "c1", "type": "condition", "params": { "op": "<", "value": 1 } },
            { "id": "g1", "type": "logic_and" },
            { "id": "a1", "type": "action_buy" }
        ],
        "edges": [ { "from": "s1", "to": "c1" }, { "from": "c1", "to": "g1" }, { "from": "g1", "to": "a1" } ]
    })
    .to_string();
    let err = compile(&bp).expect_err("a one-input gate must fail");
    assert!(err.contains("g1") && err.contains("at least two"), "{err}");
}

#[test]
fn a_gate_fed_by_a_value_source_is_refused() {
    let bp = json!({
        "version": 1, "name": "gate_value_input",
        "nodes": [
            { "id": "s1", "type": "data_source", "params": { "field": "tick.price" } },
            { "id": "c1", "type": "condition", "params": { "op": "<", "value": 1 } },
            { "id": "g1", "type": "logic_and" },
            { "id": "a1", "type": "action_buy" }
        ],
        "edges": [
            { "from": "s1", "to": "c1" }, { "from": "s1", "to": "g1" },
            { "from": "c1", "to": "g1" }, { "from": "g1", "to": "a1" }
        ]
    })
    .to_string();
    let err = compile(&bp).expect_err("a value input to a gate must fail");
    assert!(err.contains("g1") && err.contains("s1"), "{err}");
}

#[test]
fn an_unguarded_action_is_refused() {
    let bp = json!({
        "version": 1, "name": "raw_source_action",
        "nodes": [
            { "id": "s1", "type": "data_source", "params": { "field": "tick.price" } },
            { "id": "a1", "type": "action_buy" }
        ],
        "edges": [ { "from": "s1", "to": "a1" } ]
    })
    .to_string();
    let err = compile(&bp).expect_err("a source-fed action must fail");
    assert!(err.contains("a1") && err.contains("guarded"), "{err}");
}

// ── the codegen guarantee ───────────────────────────────────────────────

/// The acceptance artifact assertion: whatever compiles, the output never
/// carries the sandbox's forbidden namespaces.
#[test]
fn generated_lua_never_contains_os_io_or_debug() {
    for src in [DOG_BLUEPRINT, &logic_or_blueprint_json()] {
        let lua = compile(src).expect("blueprint compiles");
        for bad in ["os.", "io.", "debug"] {
            assert!(!lua.contains(bad), "generated Lua carries {bad:?}:\n{lua}");
        }
    }
}

/// The sandbox-side twin of the artifact assertion: a chunk that TRIES
/// `os.execute` is refused by the existing §6.2 sandbox (this proves the
/// wall the generated code is built to never approach). The sandbox test
/// module owns the canonical version of this proof; this one pins the
/// same behaviour from the compiler's side so both halves stay honest.
#[test]
fn the_sandbox_refuses_an_os_execute_attempt() {
    let sb = blitzkrieg_lua_runtime::sandbox::LuaSandbox::new().expect("sandbox builds");
    sb.load(
        "function on_tick(tick) return os.execute(\"true\") end",
        "evil",
    )
    .expect("defining the function is legal (os is only nil at CALL time)");
    let on_tick = sb
        .global_function("on_tick")
        .expect("globals readable")
        .expect("on_tick present");
    let err = sb
        .invoke("tick", |_| on_tick.call::<()>(()))
        .expect_err("os.execute must be a deterministic nil-call error");
    // `os` is not merely uncallable but ABSENT: indexing it is the
    // deterministic refusal the §6.2 nil-blacklist produces.
    assert!(
        err.message().contains("attempt to index a nil value") && err.message().contains("os"),
        "the sandbox refused the escape: {}",
        err.message()
    );
}

fn logic_or_blueprint_json() -> String {
    json!({
        "version": 1, "name": "or_codegen",
        "nodes": [
            { "id": "s1", "type": "data_source", "params": { "field": "tick.obi" } },
            { "id": "c1", "type": "condition", "params": { "op": ">", "value": 0.5 } },
            { "id": "c2", "type": "condition", "params": { "field": "tick.trend_confirmed", "op": "==", "value": true } },
            { "id": "g1", "type": "logic_or" },
            { "id": "a1", "type": "action_sell", "params": { "budget_ratio": 0.2 } }
        ],
        "edges": [
            { "from": "s1", "to": "c1" }, { "from": "c1", "to": "g1" },
            { "from": "c2", "to": "g1" }, { "from": "g1", "to": "a1" }
        ]
    })
    .to_string()
}
