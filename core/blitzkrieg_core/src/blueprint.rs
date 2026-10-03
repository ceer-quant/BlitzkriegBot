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
//! The generated contract is the v1 tick surface: `on_tick(tick)` receives
//! the host's tick table (the whitelisted fields plus the host context
//! `tick.symbol` / `tick.account_id` / `tick.available_balance`) and fires
//! `place_order{...}` (a suggestion the kernel adjudicates, sizes and gates —
//! the same seal every strategy runs under). `declare_modes()` declares the
//! prediction/binary_outcome_wheel mode with capability names from the repo's
//! real §2.3 vocabulary (`blitzkrieg_market_api::modes::CAPABILITY_NAMES`),
//! so a generated package passes the load-time modes handshake instead of
//! being refused for invented capability names.
//!
//! Errors are `String` in the onchain.rs house style: each one names the node
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

/// Compile a blueprint JSON document into Lua source. Every failure names the
/// node id and the reason; success is deterministic (same input → same text).
pub fn compile(json: &str) -> Result<String, String> {
    let bp: Blueprint =
        serde_json::from_str(json).map_err(|e| format!("blueprint parse failed: {e}"))?;
    let graph = validate_structure(&bp)?;
    validate_semantics(&bp, &graph)?;
    generate(&bp, &graph)
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

fn generate(bp: &Blueprint, graph: &Graph) -> Result<String, String> {
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
                value_expr.insert(id.clone(), lua_literal(v));
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
                guard_expr.insert(id.clone(), comparison(&left, &op, v));
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

    let call = order_call(action, &steps);
    let mut out = String::new();
    out.push_str(&format!(
        "-- generated by blitzkrieg-core from blueprint \"{}\" (version 1).\n",
        bp.name
    ));
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
    Ok(out)
}

/// The `place_order{...}` suggestion for a buy/sell action. The limit price
/// comes from the action's own `price` param when present, else from the
/// nearest price-field condition in the guard chain, else the order is
/// emitted without a price as a market suggestion. `budget_ratio` scales the
/// account's available balance exactly as the blueprint states it.
fn order_call(action: &BpNode, steps: &[(String, bool)]) -> String {
    let side = match action.node_type.as_str() {
        "action_buy" => "buy",
        "action_sell" => "sell",
        _ => "hold",
    };
    let price: Option<String> = action.params.get("price").map(lua_literal).or_else(|| {
        // Innermost-first: the last condition on tick.price / tick.mid_price.
        steps
            .iter()
            .rev()
            .find_map(|(expr, _)| price_threshold(expr))
    });
    let budget = action
        .params
        .get("budget_ratio")
        .map(|v| format!("tick.available_balance * {}", lua_literal(v)));

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
mod tests {
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
        assert!(lua.contains("tick.price <= 0.25"), "guard chain: {lua}");
        assert!(
            lua.contains("tick.trend_confirmed == true"),
            "inline-field condition: {lua}"
        );
        assert!(lua.contains("side = \"buy\""), "action translated: {lua}");
        assert!(
            lua.contains("tick.available_balance * 0.1"),
            "budget line: {lua}"
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
            lua.contains("if (tick.mid_price > 0.8 and tick.time_left_sec < 60) then"),
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
            lua.contains("if (tick.obi > 0.5 or not (tick.trend_confirmed != true)) then"),
            "or-gate with one inverted edge: {lua}"
        );
        // action_hold emits the guard but no order.
        assert!(!lua.contains("place_order"), "hold places no order: {lua}");
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
        assert!(lua.contains("0.5 < 0.25"), "constant operand: {lua}");
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
}
