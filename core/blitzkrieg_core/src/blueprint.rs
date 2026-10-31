//! #361 — blueprint: a JSON decision DAG → three-stage compile → runnable Lua.
//!
//! A blueprint is the no-code strategy surface: a directed acyclic graph of
//! eight node types (`data_source`, `condition`, `action_buy`, `action_sell`,
//! `action_hold`, `constant`, `logic_and`, `logic_or`) that compiles to a
//! human-readable Lua strategy package body (`on_tick(tick)` +
//! `declare_modes()`). The compile is three stages, and the ORDER is the
//! safety property:
//!
//! 1. **Structure** — DAG (no cycles), no orphan nodes, exactly one reachable
//!    action output, ≤ [`MAX_NODES`] nodes and ≤ [`MAX_DEPTH`] depth. A
//!    structurally broken graph is refused before any semantics are read, so
//!    the error names the graph problem, not a symptom three hops away.
//! 2. **Semantics** — every operator is on the [`CONDITION_OPS`] whitelist and
//!    every field on the [`FIELD_WHITELIST`]; a field this compiler does not
//!    know (including anything smelling of `os.`/`io.`/`debug`) is refused BY
//!    NAME with the node id.
//! 3. **Codegen** — whitelisted fields translate to fixed Lua expressions;
//!    user text reaches the output ONLY inside quoted literals (and string
//!    literals carrying `os.`/`io.`/`debug` are refused outright). The
//!    generated source contains no `os.`/`io.`/`debug` by construction —
//!    there is no string interpolation of unvalidated user input anywhere in
//!    the emitter.
//!
//! The generated contract is the v1 tick surface PLUS the runtime bridge
//! (#413): the emitted package carries `on_tick(tick)` (the human-readable
//! guard chain firing `place_order{...}` into its own collector),
//! `bk_evaluate()` — the required §6.5 entry point, which drives `on_tick`
//! once per round token off the host `bk.*` surface and returns the collected
//! entries — and `declare_modes()` declaring the prediction/binary_outcome_wheel
//! mode with capability names from the repo's real §2.3 vocabulary
//! (`blitzkrieg_market_api::modes::CAPABILITY_NAMES`), so a generated package
//! passes the load-time modes handshake instead of being refused for invented
//! capability names. Every suggestion the kernel adopts still runs the same
//! adjudication, sizing and gating seal as a handwritten strategy's.
//!
//! Errors are `String` in the onchain-pull house style: each one names the node
//! id (and the offending value) so a blueprint author can fix the JSON
//! without a debugger.

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

/// Node-count ceiling. A blueprint past it is refused at stage 1 — the gate
/// bounds codegen size (every action emits its full guard chain).
pub const MAX_NODES: usize = 100;
/// Longest node path (in nodes) the compiler accepts. Past it the nested-if
/// emission stops being human-readable, which defeats the point.
pub const MAX_DEPTH: usize = 20;

/// The condition operators a blueprint may ask for. Anything else (`=~`
/// included) is refused with the node id and the offending spelling.
pub const CONDITION_OPS: [&str; 7] = ["<", "<=", ">", ">=", "==", "!=", "in"];

/// The readable field whitelist → the fixed Lua expression it compiles to.
/// `account.available_balance` is projected by the host onto the tick table
/// as `tick.available_balance`, which is what the emitted budget line reads.
pub const FIELD_WHITELIST: [(&str, &str); 6] = [
    ("tick.price", "tick.price"),
    ("tick.mid_price", "tick.mid_price"),
    ("tick.time_left_sec", "tick.time_left_sec"),
    ("tick.trend_confirmed", "tick.trend_confirmed"),
    ("tick.obi", "tick.obi"),
    ("account.available_balance", "tick.available_balance"),
];

/// The three substrings a generated file must never carry (the §6.2 sandbox
/// nils the same globals). Checked on field names, string literals and the
/// blueprint name — the guarantee is by construction, not by review.
const FORBIDDEN_SUBSTRINGS: [&str; 3] = ["os.", "io.", "debug"];

fn field_expr(field: &str) -> Option<&'static str> {
    FIELD_WHITELIST
        .iter()
        .find(|(name, _)| *name == field)
        .map(|(_, expr)| *expr)
}

fn forbidden_substring(s: &str) -> Option<&'static str> {
    FORBIDDEN_SUBSTRINGS
        .iter()
        .find(|bad| s.contains(*bad))
        .copied()
}

// ── wire types ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Blueprint {
    version: u32,
    name: String,
    nodes: Vec<BpNode>,
    #[serde(default)]
    edges: Vec<BpEdge>,
}

#[derive(Debug, Deserialize)]
struct BpNode {
    id: String,
    #[serde(rename = "type")]
    node_type: String,
    #[serde(default)]
    params: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct BpEdge {
    from: String,
    to: String,
    #[serde(default = "default_true")]
    when: bool,
}

fn default_true() -> bool {
    true
}

/// The four node classes the compiler distinguishes (the eight types collapse
/// into them: the three actions behave identically up to `side`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Source,
    Condition,
    Action,
    Gate,
}

impl NodeKind {
    fn parse(node_type: &str, id: &str) -> Result<Self, String> {
        match node_type {
            "data_source" => Ok(Self::Source),
            "condition" => Ok(Self::Condition),
            "action_buy" | "action_sell" | "action_hold" => Ok(Self::Action),
            "constant" => Ok(Self::Source),
            "logic_and" | "logic_or" => Ok(Self::Gate),
            other => Err(format!(
                "node {id}: unknown type '{other}' (supported: data_source, condition, \
                 action_buy, action_sell, action_hold, constant, logic_and, logic_or)"
            )),
        }
    }
}

// ── the public entry point ──────────────────────────────────────────────────

/// One evolvable knob exported from a compiled blueprint (用户裁决：蓝图生产
/// 的策略也要支持进化). Numeric anchors — condition values, constant values,
/// action `price` / `budget_ratio` — compile to `__param("<node>_<param>",
/// <literal>)` reads: the generated Lua serves the operator's hot parameter
/// when the shadow-evolution registry carries one, and falls back to the
/// authored literal when it does not (fail-closed: no params cell → the
/// blueprint behaves exactly as drawn).
///
/// The domain (min/max) is the #393 explicit declaration the evolution
/// machinery demands: absent bounds = not evolvable, and the kernel never
/// invents one — so save-time export ALWAYS ships a coherent window around
/// the authored value, ± half its own magnitude (0 widens to 0..1).
#[derive(Debug, Clone, PartialEq)]
pub struct BlueprintTunable {
    /// The wire name: `<nodeId>_<param>` (e.g. `c1_value`, `a1_price`).
    pub name: String,
    /// The authored value, decimal-string (the tunables wire rule).
    pub default: String,
    /// Lower domain bound, decimal-string.
    pub min: String,
    /// Upper domain bound, decimal-string.
    pub max: String,
}

/// Compile a blueprint JSON document into Lua source. Every failure names the
/// node id and the reason; success is deterministic (same input → same text).
pub fn compile(json: &str) -> Result<String, String> {
    compile_with_tunables(json).map(|(lua, _)| lua)
}

/// [`compile`] plus the evolvable-knob export: the same document yields the
/// same Lua AND the tunables table `blueprint.save` writes into the package
/// manifest so the loader registers evolution knobs for it (#393 shape).
pub fn compile_with_tunables(json: &str) -> Result<(String, Vec<BlueprintTunable>), String> {
    let bp: Blueprint =
        serde_json::from_str(json).map_err(|e| format!("blueprint parse failed: {e}"))?;
    let graph = validate_structure(&bp)?;
    validate_semantics(&bp, &graph)?;
    generate(&bp, &graph)
}

/// The save-time domain rule: ± half the value's own magnitude, floored to a
/// legal Decimal, with 0 widened to the 0..1 probability band. Always
/// coherent (min <= default <= max) by construction, so `specs_from_tunables`
/// registers the knob instead of silently skipping it.
fn tunable_domain(v: f64) -> (String, String) {
    let half = (v.abs() / 2.0).max(1e-6);
    let (min, max) = if v == 0.0 {
        (0.0, 1.0)
    } else {
        (v - half, v + half)
    };
    // 12 significant digits keeps the round-trip readable and far inside
    // rust_decimal's 28-digit ceiling.
    (format!("{:.12}", min), format!("{:.12}", max))
}

// ── stage 1: structure ──────────────────────────────────────────────────────

/// The structural view codegen walks: nodes by id, incoming/outgoing edge
/// lists (in authoring order), and the per-node kind.
struct Graph {
    kind_by_id: HashMap<String, NodeKind>,
    incoming: HashMap<String, Vec<(String, bool)>>,
    outgoing: HashMap<String, Vec<(String, bool)>>,
}

fn validate_structure(bp: &Blueprint) -> Result<Graph, String> {
    if bp.version != 1 {
        return Err(format!(
            "blueprint version {} is not supported (this compiler speaks version 1)",
            bp.version
        ));
    }
    if bp.name.trim().is_empty() {
        return Err("blueprint name must not be empty".into());
    }
    if let Some(bad) = forbidden_substring(&bp.name) {
        return Err(format!(
            "blueprint name {bad:?} substring is refused: the name is embedded in the \
             generated source verbatim"
        ));
    }
    if bp.nodes.is_empty() {
        return Err("blueprint has no nodes".into());
    }
    if bp.nodes.len() > MAX_NODES {
        return Err(format!(
            "blueprint has {} nodes; the ceiling is {MAX_NODES}",
            bp.nodes.len()
        ));
    }

    let mut kind_by_id = HashMap::new();
    for n in &bp.nodes {
        if n.id.trim().is_empty() {
            return Err("a node carries an empty id".into());
        }
        if kind_by_id.contains_key(&n.id) {
            return Err(format!("duplicate node id {}", n.id));
        }
        let kind = NodeKind::parse(&n.node_type, &n.id)?;
        kind_by_id.insert(n.id.clone(), kind);
    }

    let mut incoming: HashMap<String, Vec<(String, bool)>> = HashMap::new();
    let mut outgoing: HashMap<String, Vec<(String, bool)>> = HashMap::new();
    let mut seen_edges: std::collections::HashSet<(String, String)> = Default::default();
    for e in &bp.edges {
        for (role, id) in [("from", &e.from), ("to", &e.to)] {
            if !kind_by_id.contains_key(id) {
                return Err(format!(
                    "edge {}→{}: {role} node {id} does not exist",
                    e.from, e.to
                ));
            }
        }
        if e.from == e.to {
            return Err(format!(
                "edge {}→{} is a self-loop: a cycle is not a strategy",
                e.from, e.to
            ));
        }
        if !seen_edges.insert((e.from.clone(), e.to.clone())) {
            return Err(format!("duplicate edge {}→{}", e.from, e.to));
        }
        outgoing
            .entry(e.from.clone())
            .or_default()
            .push((e.to.clone(), e.when));
        incoming
            .entry(e.to.clone())
            .or_default()
            .push((e.from.clone(), e.when));
    }

    // Orphans: no incoming AND no outgoing edges — a node nothing can reach
    // and that reaches nothing. The single-node blueprint is exempt (a lone
    // action is the trivial hold), everything else is dead weight.
    if bp.nodes.len() > 1 {
        for n in &bp.nodes {
            if !incoming.contains_key(&n.id) && !outgoing.contains_key(&n.id) {
                return Err(format!(
                    "node {} is an orphan: no incoming and no outgoing edges",
                    n.id
                ));
            }
        }
    }

    // Actions are terminal: nothing may hang off them, and their guard must
    // be a condition/gate — a raw data_source in-edge would fire unguarded.
    for n in &bp.nodes {
        if kind_by_id[&n.id] != NodeKind::Action {
            continue;
        }
        if let Some(next) = outgoing.get(&n.id) {
            let to = &next[0].0;
            return Err(format!(
                "action {} must be a terminal node but has an outgoing edge to {to}",
                n.id
            ));
        }
        let Some(ins) = incoming.get(&n.id) else {
            return Err(format!(
                "action {} has no incoming edge: it is never reachable",
                n.id
            ));
        };
        for (src, _) in ins {
            if !matches!(kind_by_id[src], NodeKind::Condition | NodeKind::Gate) {
                return Err(format!(
                    "action {} must be guarded by a condition or logic node, \
                     but its incoming edge comes from {src} ({})",
                    n.id,
                    type_of(bp, src)
                ));
            }
        }
    }

    // Exactly one wired action output. Two wired actions = two behaviours in
    // one strategy; zero = a strategy that decides nothing. An unwired action
    // is already caught by the orphan rule above.
    let wired_actions: Vec<&String> = bp
        .nodes
        .iter()
        .filter(|n| kind_by_id[&n.id] == NodeKind::Action && incoming.contains_key(&n.id))
        .map(|n| &n.id)
        .collect();
    if wired_actions.is_empty() {
        return Err(
            "blueprint has no reachable action output: add one action_buy / action_sell / \
             action_hold fed by a condition or logic chain"
                .into(),
        );
    }
    if wired_actions.len() > 1 {
        let ids: Vec<&str> = wired_actions.iter().map(|s| s.as_str()).collect();
        return Err(format!(
            "blueprint has {} reachable action outputs ({}); exactly one is allowed",
            wired_actions.len(),
            ids.join(", ")
        ));
    }

    // Operand-shape checks. A condition's incoming edges split by WHO its
    // author is: an edge FROM a value producer (data_source / constant) is
    // the OPERAND it compares; an edge FROM a guard producer (condition /
    // gate) is the GUARD CHAIN it sits behind. The two do not mix: a
    // condition with its own `field` param takes no operand edge, and a
    // condition without one takes exactly one operand (zero is an
    // operand-less comparison, two is a second left side — both refused).
    for n in &bp.nodes {
        if kind_by_id[&n.id] != NodeKind::Condition {
            continue;
        }
        let has_field = n.params.contains_key("field");
        let operands: Vec<&String> = incoming
            .get(&n.id)
            .map(|ins| {
                ins.iter()
                    .filter(|(src, _)| kind_by_id[src] == NodeKind::Source)
                    .map(|(src, _)| src)
                    .collect()
            })
            .unwrap_or_default();
        if has_field && !operands.is_empty() {
            let src = &operands[0];
            return Err(format!(
                "condition {} carries a `field` param but also takes an operand \
                 edge from {src}; one operand only",
                n.id
            ));
        }
        if !has_field && operands.len() > 1 {
            let ids: Vec<&str> = operands.iter().map(|s| s.as_str()).collect();
            return Err(format!(
                "condition {} takes {} operand edges ({}); exactly one",
                n.id,
                operands.len(),
                ids.join(", ")
            ));
        }
        if !has_field && operands.is_empty() {
            // No field and no operand edge: the comparison has no left side.
            // (A guard edge from a condition/gate alone still leaves it
            // operand-less — a chain position, not a comparison.)
            let guarded = incoming
                .get(&n.id)
                .map(|ins| {
                    ins.iter()
                        .any(|(src, _)| kind_by_id[src] != NodeKind::Source)
                })
                .unwrap_or(false);
            if !guarded {
                return Err(format!(
                    "condition {} has no operand: add a `field` param or an incoming \
                     data_source/constant edge",
                    n.id
                ));
            }
        }
    }

    // Gates join boolean guards: sources must be conditions or gates, and
    // there must be at least two of them (one `and` is a pass-through).
    for n in &bp.nodes {
        if kind_by_id[&n.id] != NodeKind::Gate {
            continue;
        }
        let Some(ins) = incoming.get(&n.id) else {
            return Err(format!(
                "logic gate {} has no incoming edges: it must join at least two guards",
                n.id
            ));
        };
        if ins.len() < 2 {
            return Err(format!(
                "logic gate {} joins {} input{}; at least two are required",
                n.id,
                ins.len(),
                if ins.len() == 1 { "" } else { "s" }
            ));
        }
        for (src, _) in ins {
            if !matches!(kind_by_id[src], NodeKind::Condition | NodeKind::Gate) {
                return Err(format!(
                    "logic gate {} takes an input from {src} ({}), which produces a \
                     value, not a guard; gates join conditions and gates",
                    n.id,
                    type_of(bp, src)
                ));
            }
        }
    }

    // Cycles: any node that survives an iterative DFS as "in progress" is on
    // a cycle. Iterative (explicit stack) so a 100-deep chain cannot overflow
    // the call stack.
    detect_cycle(bp, &outgoing)?;

    // Depth: longest node path. Computed bottom-up over the same DAG.
    let mut depth: HashMap<String, usize> = HashMap::new();
    let mut order = topo_order(bp, &outgoing)?;
    while let Some(id) = order.pop() {
        let longest_child = outgoing
            .get(&id)
            .map(|outs| {
                outs.iter()
                    .filter_map(|(to, _)| depth.get(to).copied())
                    .max()
            })
            .unwrap_or(None)
            .unwrap_or(0);
        depth.insert(id, 1 + longest_child);
    }
    if let Some((id, d)) = depth.iter().max_by_key(|(_, d)| **d)
        && *d > MAX_DEPTH
    {
        return Err(format!(
            "blueprint depth {d} exceeds the ceiling of {MAX_DEPTH} (deepest chain \
             ends at node {id})"
        ));
    }

    Ok(Graph {
        kind_by_id,
        incoming,
        outgoing,
    })
}

fn type_of(bp: &Blueprint, id: &str) -> &'static str {
    // The id is guaranteed present (structure checks ran first).
    match bp
        .nodes
        .iter()
        .find(|n| n.id == id)
        .map(|n| n.node_type.as_str())
    {
        Some("data_source") => "data_source",
        Some("condition") => "condition",
        Some("constant") => "constant",
        Some("logic_and") => "logic_and",
        Some("logic_or") => "logic_or",
        Some(t) if t.starts_with("action_") => "action",
        _ => "node",
    }
}

/// Iterative three-colour DFS. Returns Err naming one node on the cycle.
fn detect_cycle(
    bp: &Blueprint,
    outgoing: &HashMap<String, Vec<(String, bool)>>,
) -> Result<(), String> {
    #[derive(Clone, Copy, PartialEq)]
    enum Colour {
        White,
        Grey,
        Black,
    }
    let mut colour: HashMap<String, Colour> = bp
        .nodes
        .iter()
        .map(|n| (n.id.clone(), Colour::White))
        .collect();
    // Explicit stack of (node, next child index).
    let mut stack: Vec<(String, usize)> = Vec::new();
    for root in &bp.nodes {
        if colour[&root.id] != Colour::White {
            continue;
        }
        stack.push((root.id.clone(), 0));
        colour.insert(root.id.clone(), Colour::Grey);
        while let Some((id, child_idx)) = stack.last_mut() {
            let outs = outgoing.get(id).cloned().unwrap_or_default();
            if *child_idx >= outs.len() {
                let done = id.clone();
                stack.pop();
                colour.insert(done, Colour::Black);
                continue;
            }
            let child = outs[*child_idx].0.clone();
            *child_idx += 1;
            match colour[&child] {
                Colour::Grey => {
                    return Err(format!("blueprint contains a cycle: node {child} is on it"));
                }
                Colour::White => {
                    colour.insert(child.clone(), Colour::Grey);
                    stack.push((child, 0));
                }
                Colour::Black => {}
            }
        }
    }
    Ok(())
}

/// Kahn topological order (nodes only, edge order preserved for determinism).
/// The cycle check has already run, so this cannot fail in practice; it
/// returns Err rather than looping forever if that assumption ever breaks.
fn topo_order(
    bp: &Blueprint,
    outgoing: &HashMap<String, Vec<(String, bool)>>,
) -> Result<Vec<String>, String> {
    let mut indegree: HashMap<&str, usize> = bp.nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
    for outs in outgoing.values() {
        for (to, _) in outs {
            *indegree.entry(to.as_str()).or_insert(0) += 1;
        }
    }
    let mut queue: Vec<String> = bp
        .nodes
        .iter()
        .filter(|n| indegree[n.id.as_str()] == 0)
        .map(|n| n.id.clone())
        .collect();
    let mut pos = 0;
    while pos < queue.len() {
        let id = queue[pos].clone();
        pos += 1;
        if let Some(outs) = outgoing.get(&id) {
            for (to, _) in outs {
                let deg = indegree.get_mut(to.as_str()).expect("edge into known node");
                *deg -= 1;
                if *deg == 0 {
                    queue.push(to.clone());
                }
            }
        }
    }
    if queue.len() != bp.nodes.len() {
        // Keep the id list stable for the error message.
        let stranded: Vec<&str> = bp
            .nodes
            .iter()
            .map(|n| n.id.as_str())
            .filter(|id| !queue.iter().any(|q| q == id))
            .collect();
        return Err(format!(
            "blueprint contains a cycle through: {}",
            stranded.join(", ")
        ));
    }
    Ok(queue)
}

// ── stage 2: semantics ──────────────────────────────────────────────────────

fn validate_semantics(bp: &Blueprint, graph: &Graph) -> Result<(), String> {
    for n in &bp.nodes {
        match graph.kind_by_id[&n.id] {
            NodeKind::Source if n.node_type == "data_source" => {
                let Some(field) = string_param(n, "field")? else {
                    return Err(format!(
                        "data_source {}: missing required param `field`",
                        n.id
                    ));
                };
                if let Some(bad) = forbidden_substring(&field) {
                    return Err(format!(
                        "data_source {}: field {field:?} contains {bad:?} — refused; \
                         fields come from the whitelist only",
                        n.id
                    ));
                }
                let Some(expr) = field_expr(&field) else {
                    let known: Vec<&str> = FIELD_WHITELIST.iter().map(|(f, _)| *f).collect();
                    return Err(format!(
                        "data_source {}: field {field:?} is not whitelisted (known: {})",
                        n.id,
                        known.join(", ")
                    ));
                };
                let _ = expr;
                for key in n.params.keys() {
                    if key != "field" {
                        return Err(format!(
                            "data_source {}: unknown param {key:?} (allowed: `field`)",
                            n.id
                        ));
                    }
                }
            }
            NodeKind::Source => {
                // constant: `value` is required and scalar; string values are
                // quoted literals but still refuse the forbidden substrings so
                // the generated file carries them by construction never.
                let Some(v) = n.params.get("value") else {
                    return Err(format!("constant {}: missing required param `value`", n.id));
                };
                check_constant(n, v)?;
                for key in n.params.keys() {
                    if key != "value" {
                        return Err(format!(
                            "constant {}: unknown param {key:?} (allowed: `value`)",
                            n.id
                        ));
                    }
                }
            }
            NodeKind::Condition => {
                let op = string_param(n, "op")?
                    .ok_or_else(|| format!("condition {}: missing required param `op`", n.id))?;
                if !CONDITION_OPS.contains(&op.as_str()) {
                    return Err(format!(
                        "condition {}: operator {op:?} is not whitelisted (allowed: {})",
                        n.id,
                        CONDITION_OPS.join(" ")
                    ));
                }
                let Some(v) = n.params.get("value") else {
                    return Err(format!(
                        "condition {}: missing required param `value`",
                        n.id
                    ));
                };
                if op == "in" {
                    let Some(members) = v.as_array() else {
                        return Err(format!(
                            "condition {}: operator `in` needs an array `value`",
                            n.id
                        ));
                    };
                    if members.is_empty() {
                        return Err(format!(
                            "condition {}: operator `in` needs a non-empty array",
                            n.id
                        ));
                    }
                    for m in members {
                        if !m.is_number() && !m.is_string() {
                            return Err(format!(
                                "condition {}: `in` members must be numbers or strings",
                                n.id
                            ));
                        }
                        if let Some(s) = m.as_str()
                            && let Some(bad) = forbidden_substring(s)
                        {
                            return Err(format!(
                                "condition {}: `in` member {s:?} contains {bad:?} — refused",
                                n.id
                            ));
                        }
                    }
                } else if !v.is_number() && !v.is_string() && !v.is_boolean() {
                    return Err(format!(
                        "condition {}: `value` must be a number, string or boolean",
                        n.id
                    ));
                }
                if let Some(s) = v.as_str()
                    && let Some(bad) = forbidden_substring(s)
                {
                    return Err(format!(
                        "condition {}: `value` {s:?} contains {bad:?} — refused",
                        n.id
                    ));
                }
                for key in n.params.keys() {
                    if !matches!(key.as_str(), "op" | "value" | "field") {
                        return Err(format!(
                            "condition {}: unknown param {key:?} (allowed: `op`, `value`, `field`)",
                            n.id
                        ));
                    }
                }
                if let Some(field) = string_param(n, "field")? {
                    if let Some(bad) = forbidden_substring(&field) {
                        return Err(format!(
                            "condition {}: field {field:?} contains {bad:?} — refused; \
                             fields come from the whitelist only",
                            n.id
                        ));
                    }
                    if field_expr(&field).is_none() {
                        let known: Vec<&str> = FIELD_WHITELIST.iter().map(|(f, _)| *f).collect();
                        return Err(format!(
                            "condition {}: field {field:?} is not whitelisted (known: {})",
                            n.id,
                            known.join(", ")
                        ));
                    }
                } else {
                    // No `field` param: the left side must be the ONE operand
                    // edge from a value producer. Absent, the comparison has
                    // no left side — a guard position, not a comparison. Name
                    // the guard chain neighbour so the author sees the shape.
                    let operand = graph.incoming.get(&n.id).and_then(|ins| {
                        ins.iter()
                            .find(|(src, _)| graph.kind_by_id[src] == NodeKind::Source)
                    });
                    if operand.is_none() {
                        let fed_by: Vec<&str> = graph
                            .incoming
                            .get(&n.id)
                            .map(|ins| ins.iter().map(|(src, _)| src.as_str()).collect())
                            .unwrap_or_default();
                        return Err(format!(
                            "condition {}: no operand (field param or a data_source/ \
                             constant edge); it is fed by [{}], which are guards, \
                             not values — a comparison needs a left side",
                            n.id,
                            fed_by.join(", ")
                        ));
                    }
                }
            }
            NodeKind::Gate => {
                if let Some(key) = n.params.keys().next() {
                    return Err(format!(
                        "logic gate {}: unknown param {key:?} (gates take none)",
                        n.id
                    ));
                }
            }
            NodeKind::Action => {
                for key in n.params.keys() {
                    if !matches!(key.as_str(), "price" | "budget_ratio") {
                        return Err(format!(
                            "action {}: unknown param {key:?} (allowed: `price`, `budget_ratio`)",
                            n.id
                        ));
                    }
                }
                if let Some(p) = n.params.get("price")
                    && !p.is_number()
                {
                    return Err(format!("action {}: `price` must be a number", n.id));
                }
                if let Some(b) = n.params.get("budget_ratio")
                    && !b.is_number()
                {
                    return Err(format!("action {}: `budget_ratio` must be a number", n.id));
                }
            }
        }
    }
    Ok(())
}

fn string_param(n: &BpNode, key: &str) -> Result<Option<String>, String> {
    match n.params.get(key) {
        None => Ok(None),
        Some(v) if v.is_string() => Ok(Some(v.as_str().expect("checked").to_string())),
        Some(other) => Err(format!(
            "node {}: param {key:?} must be a string, got {other}",
            n.id
        )),
    }
}

fn check_constant(n: &BpNode, v: &serde_json::Value) -> Result<(), String> {
    if v.is_number() || v.is_boolean() {
        return Ok(());
    }
    if let Some(s) = v.as_str() {
        if let Some(bad) = forbidden_substring(s) {
            return Err(format!(
                "constant {}: string value {s:?} contains {bad:?} — refused",
                n.id
            ));
        }
        return Ok(());
    }
    Err(format!(
        "constant {}: `value` must be a number, string or boolean",
        n.id
    ))
}

// ── stage 3: codegen ────────────────────────────────────────────────────────

fn generate(bp: &Blueprint, graph: &Graph) -> Result<(String, Vec<BlueprintTunable>), String> {
    // The evolvable knobs (用户裁决：蓝图生产的策略也要支持进化)：every
    // numeric anchor registers one, named `<nodeId>_<param>`. Collection
    // happens exactly where the literal is emitted, so the exported table can
    // never drift from what the generated source actually reads.
    let mut tunables: Vec<BlueprintTunable> = Vec::new();

    // Register one numeric knob; `__param` will serve the hot value or this
    // literal at runtime. Non-numeric anchors (strings, booleans, arrays) are
    // behaviour, not calibration — they stay inline literals.
    let register =
        |tunables: &mut Vec<BlueprintTunable>, node: &str, param: &str, v: &serde_json::Value| {
            if let Some(num) = v.as_f64() {
                let (min, max) = tunable_domain(num);
                tunables.push(BlueprintTunable {
                    name: format!("{node}_{param}"),
                    default: lua_literal(v),
                    min,
                    max,
                });
            }
        };

    // Value/guard expressions per node, computed in topological order so a
    // node's inputs are always ready.
    let mut value_expr: HashMap<String, String> = HashMap::new();
    let mut guard_expr: HashMap<String, String> = HashMap::new();
    let order = topo_order(bp, &graph.outgoing)?;
    for id in &order {
        let n = bp.nodes.iter().find(|n| &n.id == id).expect("known node");
        match graph.kind_by_id[id] {
            NodeKind::Source if n.node_type == "data_source" => {
                let field = string_param(n, "field")?.unwrap_or_default();
                value_expr.insert(
                    id.clone(),
                    field_expr(&field)
                        .expect("whitelisted in stage 2")
                        .to_string(),
                );
            }
            NodeKind::Source => {
                let v = n.params.get("value").expect("checked in stage 2");
                register(&mut tunables, &n.id, "value", v);
                value_expr.insert(id.clone(), param_expr(&n.id, "value", v));
            }
            NodeKind::Condition => {
                // The left side: the node's own `field` param, else its ONE
                // operand edge (from a value producer — stage 1 checked the
                // split).
                let left = if let Some(field) = string_param(n, "field")? {
                    field_expr(&field)
                        .expect("whitelisted in stage 2")
                        .to_string()
                } else {
                    let src = graph.incoming[id]
                        .iter()
                        .find(|(src, _)| graph.kind_by_id[src] == NodeKind::Source)
                        .map(|(src, _)| src)
                        .expect("operand checked in stage 1");
                    value_expr[src].clone()
                };
                let op = string_param(n, "op")?.expect("checked in stage 2");
                let v = n.params.get("value").expect("checked in stage 2");
                register(&mut tunables, &n.id, "value", v);
                guard_expr.insert(
                    id.clone(),
                    comparison_tunable(&left, &op, v, &n.id, &mut tunables),
                );
            }
            NodeKind::Gate => {
                let ins = &graph.incoming[id];
                let parts: Vec<String> = ins
                    .iter()
                    .map(|(src, when)| wrap_negation(&guard_expr[src], *when))
                    .collect();
                let joiner = if n.node_type == "logic_and" {
                    " and "
                } else {
                    " or "
                };
                guard_expr.insert(id.clone(), format!("({})", parts.join(joiner)));
            }
            NodeKind::Action => {}
        }
    }

    // The single wired action (validated in stage 1).
    let action = bp
        .nodes
        .iter()
        .find(|n| graph.kind_by_id[&n.id] == NodeKind::Action && graph.incoming.contains_key(&n.id))
        .expect("one wired action");

    // Guard chain: walk backwards from the action through guard producers
    // only (condition / gate — stage 1 already refused a raw source in-edge),
    // collecting (expr, polarity) steps. Innermost step last. A Source node
    // ends the walk by construction: it produces a VALUE, never a guard, so
    // it cannot be on the chain.
    let mut steps: Vec<(String, bool)> = Vec::new();
    let mut cur = action.id.clone();
    loop {
        let srcs = graph.incoming.get(&cur).cloned().unwrap_or_default();
        let next = srcs
            .into_iter()
            .find(|(src, _)| graph.kind_by_id[src] != NodeKind::Source);
        let Some((src, when)) = next else {
            break;
        };
        steps.push((guard_expr[&src].clone(), when));
        cur = src;
    }
    steps.reverse();

    let call = order_call_tunable(action, &steps, &mut tunables);
    let mut out = String::new();
    out.push_str(&format!(
        "-- generated by blitzkrieg-core from blueprint \"{}\" (version 1).\n",
        bp.name
    ));
    // The evolution bridge (用户裁决：蓝图生产的策略也要支持进化)：numeric
    // anchors read `bk.params()[name]` through `__param`, falling back to the
    // authored literal when the registry carries no (or a non-numeric) value.
    // The names and their ±half-magnitude domains are exported as the
    // manifest's `tunables` by blueprint.save, which is what registers them
    // as evolvable knobs (#393 shape, fail-closed: no domain = no evolution).
    out.push_str("-- evolution bridge: numeric anchors read hot params with the authored literal as fallback.\n");
    out.push_str("local function __param(name, fallback)\n");
    out.push_str("  local p = bk.params()\n");
    out.push_str("  local v = p and tonumber(p[name]) or nil\n");
    out.push_str("  if v then return v end\n");
    out.push_str("  return fallback\n");
    out.push_str("end\n");
    // The runtime bridge (#413): the §6.5 loader contract requires
    // `bk_evaluate()` returning the intent tables, and `place_order` is not a
    // sandbox global — so the emitted package carries its OWN collector: each
    // `on_tick(tick)` call that reaches an action appends one entry row built
    // from the same fields the suggestion names. `bk_evaluate` then drives
    // `on_tick` once per round token (a market without a fresh book is
    // skipped — stale data is not a signal) and returns the collected table.
    // `tick.*` values come from the host `bk.*` surface the handwritten
    // strategies read: best_ask as the per-token price (a limit BUY priced at
    // the ask is the fillable side), mid when no ask exists (mid-keyed
    // conditions still evaluate), round timing, obi, and the account view's
    // available balance. The `bk.*` surface speaks decimal STRINGS (the wire
    // rule) while generated guards compare numbers, so `__tick_of` coerces
    // through `tonumber` — a string `<=` a number would error and kill every
    // evaluation. `trend_confirmed` has no kernel-side producer yet —
    // it reads as `false` so a trend-gated blueprint holds instead of trading
    // a made-up true (fail-closed, the blueprint house rule). The balance
    // falls back to 0 so a `budget_ratio` line never multiplies nil.
    out.push_str("-- runtime bridge: intent collection over the per-token book (see header).\n");
    out.push_str("local __orders = {}\n");
    out.push_str("local function __tick_of(m, token)\n");
    out.push_str("  local b = bk.book(token)\n");
    out.push_str("  local r = bk.round()\n");
    out.push_str("  local a = bk.account()\n");
    out.push_str("  local t = {}\n");
    out.push_str("  t.symbol = token\n");
    out.push_str("  t.account_id = a and a.id or nil\n");
    out.push_str("  t.available_balance = tonumber(a and (a.available or a.balance)) or 0\n");
    out.push_str("  t.time_left_sec = r and r.time_left_sec or nil\n");
    out.push_str("  t.trend_confirmed = false\n");
    out.push_str("  if b and b.fresh then\n");
    out.push_str("    t.price = tonumber(b.best_ask) or tonumber(b.mid)\n");
    out.push_str("    t.mid_price = tonumber(b.mid)\n");
    out.push_str("    t.obi = tonumber(b.obi)\n");
    out.push_str("  end\n");
    out.push_str("  return t\n");
    out.push_str("end\n");
    out.push_str("local function place_order(o)\n");
    out.push_str(
        "  if o and o.symbol and o.side == \"buy\" and type(o.price) == \"number\" then\n",
    );
    out.push_str(
        "    __orders[#__orders + 1] = { token = o.symbol, price = string.format(\"%.6g\", o.price), reason = \"blueprint\" }\n",
    );
    out.push_str("  end\n");
    out.push_str("end\n");
    out.push_str("function on_tick(tick)\n");
    let mut indent = 1;
    for (expr, when) in &steps {
        let guarded = wrap_negation(expr, *when);
        out.push_str(&format!("{}if {guarded} then\n", "  ".repeat(indent)));
        indent += 1;
    }
    match action.node_type.as_str() {
        "action_hold" => {}
        _ => {
            for line in call.lines() {
                out.push_str(&format!("{}{line}\n", "  ".repeat(indent)));
            }
        }
    }
    while indent > 1 {
        indent -= 1;
        out.push_str(&format!("{}end\n", "  ".repeat(indent)));
    }
    out.push_str("  return nil\n");
    out.push_str("end\n");
    out.push_str("function bk_evaluate()\n");
    out.push_str("  __orders = {}\n");
    out.push_str("  local r = bk.round()\n");
    out.push_str("  if r == nil then return { entries = {}, exits = {}, breaks = {} } end\n");
    out.push_str("  for _, m in ipairs(bk.markets()) do\n");
    out.push_str("    for _, token in ipairs({ m.up_token, m.down_token }) do\n");
    out.push_str("      local tick = __tick_of(m, token)\n");
    out.push_str("      if tick.price ~= nil then on_tick(tick) end\n");
    out.push_str("    end\n");
    out.push_str("  end\n");
    out.push_str("  return { entries = __orders, exits = {}, breaks = {} }\n");
    out.push_str("end\n");
    out.push_str("function declare_modes()\n");
    out.push_str(
        "  return { { market_type = \"prediction\", structure = \"binary_outcome_wheel\",\n",
    );
    out.push_str("      capabilities = { \"websocket_feed\", \"level2_snapshot\" } } }\n");
    out.push_str("end\n");

    // The construction guarantee, enforced one last time: none of the
    // forbidden substrings survive into the artifact.
    if let Some(bad) = forbidden_substring(&out) {
        return Err(format!(
            "internal: generated Lua would contain {bad:?} — refusing rather than \
             emitting a non-sandboxable artifact"
        ));
    }
    Ok((out, tunables))
}

/// The `place_order{...}` suggestion for a buy/sell action. The limit price
/// comes from the action's own `price` param when present, else from the
/// nearest price-field condition in the guard chain, else the order is
/// emitted without a price as a market suggestion. `budget_ratio` scales the
/// account's available balance exactly as the blueprint states it.
///
/// The tunable variant serves the action's own numeric params through the
/// evolution bridge (`__param`) and registers their knobs. The price
/// inherited from a guard chain is ALREADY a `__param(...)` expression (the
/// condition emitted it), so it passes through untouched.
fn order_call_tunable(
    action: &BpNode,
    steps: &[(String, bool)],
    tunables: &mut Vec<BlueprintTunable>,
) -> String {
    let side = match action.node_type.as_str() {
        "action_buy" => "buy",
        "action_sell" => "sell",
        _ => "hold",
    };
    let price: Option<String> = action
        .params
        .get("price")
        .map(|v| {
            if v.as_f64().is_some() {
                tunables.push(BlueprintTunable {
                    name: format!("{}_price", action.id),
                    default: lua_literal(v),
                    min: tunable_domain(v.as_f64().expect("checked")).0,
                    max: tunable_domain(v.as_f64().expect("checked")).1,
                });
            }
            param_expr(&action.id, "price", v)
        })
        .or_else(|| {
            // Innermost-first: the last condition on tick.price / tick.mid_price.
            steps
                .iter()
                .rev()
                .find_map(|(expr, _)| price_threshold(expr))
        });
    let budget = action.params.get("budget_ratio").map(|v| {
        if v.as_f64().is_some() {
            tunables.push(BlueprintTunable {
                name: format!("{}_budget_ratio", action.id),
                default: lua_literal(v),
                min: tunable_domain(v.as_f64().expect("checked")).0,
                max: tunable_domain(v.as_f64().expect("checked")).1,
            });
        }
        format!(
            "tick.available_balance * {}",
            param_expr(&action.id, "budget_ratio", v)
        )
    });

    let mut fields: Vec<String> = vec![
        "account_id = tick.account_id".into(),
        "market_type = \"prediction\"".into(),
        "structure = \"binary_outcome_wheel\"".into(),
        "symbol = tick.symbol".into(),
        format!("side = \"{side}\""),
    ];
    match price {
        Some(p) => {
            fields.push("order_type = \"limit\"".into());
            fields.push(format!("price = {p}"));
        }
        None => fields.push("order_type = \"market\"".into()),
    }
    if let Some(b) = budget {
        fields.push(format!("budget_usd = {b}"));
    }
    // Wrap like the reference sample: three physical lines, readable diffs.
    format!(
        "place_order {{ {},\n    {},\n    {} }}",
        fields[0],
        fields[1..3].join(", "),
        fields[3..].join(", ")
    )
}

/// Pull the numeric threshold out of a comparison guard on a price field, as
/// `Some("0.25")`. Recognises only the fixed shapes the emitter produces.
fn price_threshold(expr: &str) -> Option<String> {
    for field in ["tick.price", "tick.mid_price"] {
        for op in ["<=", ">=", "==", "<", ">"] {
            let pat = format!("{field} {op} ");
            if let Some(rest) = expr.strip_prefix(&pat) {
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

fn wrap_negation(expr: &str, when: bool) -> String {
    if when {
        expr.to_string()
    } else {
        format!("not ({expr})")
    }
}

/// A numeric anchor served through the evolution bridge: `__param(name,
/// literal)` — the hot parameter when the registry carries one, the authored
/// literal otherwise. Call sites register the knob BEFORE this runs (the
/// exported table must match what the source reads), so the name here is the
/// same `<node>_<param>` the register closure pushed.
fn param_expr(node: &str, param: &str, v: &serde_json::Value) -> String {
    if v.as_f64().is_some() {
        format!(
            "__param({}, {})",
            lua_string(&format!("{node}_{param}")),
            lua_literal(v)
        )
    } else {
        lua_literal(v)
    }
}

/// [`comparison`] with the evolution bridge on the value side: numeric
/// thresholds read `bk.params()`, everything else stays a plain literal.
/// `in` chains keep plain members (a membership set is behaviour, not a
/// calibration knob).
fn comparison_tunable(
    left: &str,
    op: &str,
    value: &serde_json::Value,
    node: &str,
    tunables: &mut Vec<BlueprintTunable>,
) -> String {
    if op == "in" {
        return comparison(left, op, value);
    }
    let _ = tunables; // registered by the caller before this runs
    format!("{left} {op} {}", param_expr(node, "value", value))
}

/// A comparison guard: `left op value`, with `in` expanded to an equality
/// chain (Lua has no `in` operator).
fn comparison(left: &str, op: &str, value: &serde_json::Value) -> String {
    if op == "in" {
        let members: Vec<String> = value
            .as_array()
            .expect("`in` value validated as an array")
            .iter()
            .map(lua_literal)
            .collect();
        let arms: Vec<String> = members.iter().map(|m| format!("{left} == {m}")).collect();
        return format!("({})", arms.join(" or "));
    }
    format!("{left} {op} {}", lua_literal(value))
}

/// A JSON scalar as a Lua literal. Strings are quoted and escaped; numbers
/// keep their shortest round-trip form; booleans pass through.
fn lua_literal(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "nil".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                format!("{}", n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => lua_string(s),
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(lua_literal).collect();
            format!("{{ {} }}", parts.join(", "))
        }
        serde_json::Value::Object(_) => "{}".into(),
    }
}

fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests;
