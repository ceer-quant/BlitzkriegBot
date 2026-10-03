/**
 * The blueprint editor's data model — the eight node types, the whitelist
 * vocabularies, and the JSON document the kernel's `blueprint.compile`
 * (core/blitzkrieg_core/src/blueprint.rs, issue 361) accepts.
 *
 * Everything here MIRRORS the Rust compiler; nothing invents a field. The two
 * edge families are the compiler's own distinction, read straight off its
 * operand/guard split: an edge FROM a value producer (data_source / constant)
 INTO a condition is its OPERAND; an edge between guard producers
 * (condition / logic gate) is the GUARD CHAIN the codegen nests as `if`s.
 * The canvas colours them differently so the shape an author draws is the
 * shape the compiler reads — it adds no new concept and never gates anything.
 */

/** The eight node types the v1 compiler accepts (blueprint.rs NodeKind). */
export type BlueprintNodeType =
  | 'data_source'
  | 'condition'
  | 'action_buy'
  | 'action_sell'
  | 'action_hold'
  | 'constant'
  | 'logic_and'
  | 'logic_or'

/** Node families for palette grouping + edge semantics. */
export type NodeFamily = 'source' | 'condition' | 'action' | 'gate'

/** The six whitelisted fields (FIELD_WHITELIST) — the data_source dropdown. */
export const FIELD_WHITELIST = [
  'tick.price',
  'tick.mid_price',
  'tick.time_left_sec',
  'tick.trend_confirmed',
  'tick.obi',
  'account.available_balance',
] as const
export type WhitelistedField = (typeof FIELD_WHITELIST)[number]

/** The seven whitelisted operators (CONDITION_OPS) — the condition dropdown. */
export const CONDITION_OPS = ['<', '<=', '>', '>=', '==', '!=', 'in'] as const
export type WhitelistedOp = (typeof CONDITION_OPS)[number]

/** One node in the editor graph. `label` is the node's kind + its edit state. */
export interface BpNode {
  id: string
  type: BlueprintNodeType
  params: Record<string, unknown>
}

/** One edge; `when` is the compiler's polarity (`false` → `not (…)`) on guard edges. */
export interface BpEdge {
  from: string
  to: string
  when: boolean
}

/** The version-1 blueprint document — exactly the wire shape the kernel parses. */
export interface BlueprintDoc {
  version: 1
  name: string
  nodes: BpNode[]
  edges: BpEdge[]
}

/** Params a node type exposes to the property panel (in panel order). */
export interface NodeSpec {
  type: BlueprintNodeType
  label: string
  family: NodeFamily
  /** One-line panel hint. */
  hint: string
}

export const NODE_SPECS: NodeSpec[] = [
  { type: 'data_source', label: '数据源', family: 'source', hint: '白名单字段（下拉可选，共 6 个）' },
  { type: 'condition', label: '条件', family: 'condition', hint: 'op + value；左操作数 = field 或入边' },
  { type: 'action_buy', label: '买入', family: 'action', hint: 'price + budget_ratio；有 price 即限价单' },
  { type: 'action_sell', label: '卖出', family: 'action', hint: 'price + budget_ratio；有 price 即限价单' },
  { type: 'action_hold', label: '持有', family: 'action', hint: '只输出守卫，不下单' },
  { type: 'constant', label: '常量', family: 'source', hint: '数字 / 字符串 / 布尔' },
  { type: 'logic_and', label: '且', family: 'gate', hint: '≥2 条守卫入边' },
  { type: 'logic_or', label: '或', family: 'gate', hint: '≥2 条守卫入边' },
]

export function nodeSpec(type: string): NodeSpec {
  return NODE_SPECS.find((s) => s.type === type) ?? NODE_SPECS[0]
}

/** Value producers feed operand edges; guard producers feed guard-chain edges. */
export function isValueProducer(type: string): boolean {
  return type === 'data_source' || type === 'constant'
}

/** The edge family a NEW connection belongs to, decided by its SOURCE type. */
export function edgeFamily(fromType: string): 'operand' | 'guard' {
  return isValueProducer(fromType) ? 'operand' : 'guard'
}

/**
 * The empty params a dropped node starts with — every required param present
 * with a legal default, so a fresh node is never one typo from refusing.
 *
 * Actions carry NO `order_type`: the kernel's stage-2 validation allows only
 * `price`/`budget_ratio` on an action (blueprint.rs "allowed: `price`,
 * `budget_ratio`") and derives limit-vs-market from whether a price exists —
 * sending `order_type` would make every fresh action refuse to compile.
 */
export function defaultParams(type: BlueprintNodeType): Record<string, unknown> {
  switch (type) {
    case 'data_source':
      return { field: 'tick.price' }
    case 'condition':
      return { op: '<', value: 0.25 }
    case 'action_buy':
    case 'action_sell':
      return { price: 0.25, budget_ratio: 0.1 }
    case 'action_hold':
      return {}
    case 'constant':
      return { value: 0.5 }
    case 'logic_and':
    case 'logic_or':
      return {}
  }
}

/**
 * The 4-node template the spec demands: price ≤ 0.25 → trend_confirmed → buy.
 * Fresh canvases open HERE — the first compile already runs, so the operator
 * starts from a runnable state and a visible Lua preview, not a blank grid.
 */
export function starterBlueprint(): BlueprintDoc {
  return {
    version: 1,
    name: 'starter_dog',
    nodes: [
      { id: 'src_price', type: 'data_source', params: { field: 'tick.price' } },
      { id: 'cond_cheap', type: 'condition', params: { op: '<=', value: 0.25 } },
      {
        id: 'cond_trend',
        type: 'condition',
        params: { field: 'tick.trend_confirmed', op: '==', value: true },
      },
      {
        id: 'act_buy',
        type: 'action_buy',
        params: { price: 0.25, budget_ratio: 0.1 },
      },
    ],
    edges: [
      { from: 'src_price', to: 'cond_cheap', when: true },
      { from: 'cond_cheap', to: 'cond_trend', when: true },
      { from: 'cond_trend', to: 'act_buy', when: true },
    ],
  }
}

/**
 * Serialize the editor graph into the version-1 document.
 *
 * Canvas-only state (positions) never reaches the kernel; `when` rides guard
 * edges as the polarity toggle in the property panel sets it. Nothing is
 * auto-pruned: the kernel is the judge of structure, and a refusal must point
 * at what the author actually drew.
 */
export function toBlueprintDoc(name: string, nodes: BpNode[], edges: BpEdge[]): string {
  const doc: BlueprintDoc = {
    version: 1,
    name: name.trim() || 'untitled_strategy',
    nodes: nodes.map((n) => ({ id: n.id, type: n.type, params: n.params })),
    edges: edges.map((e) => ({ from: e.from, to: e.to, when: e.when })),
  }
  return JSON.stringify(doc, null, 2)
}

/**
 * The save request body — the exact params object `blueprint.save` accepts
 * (`{name, json, overwrite?}`). `overwrite` is the editor's explicit
 * acknowledgement that a package of this name already exists.
 */
export interface BlueprintSaveBody {
  name: string
  json: string
  overwrite?: boolean
}
