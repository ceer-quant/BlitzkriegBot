<script setup lang="ts">
/**
 * 蓝图编辑器 — 用 @vue-flow/core 把蓝图编译链（issue 362）画出来。
 *
 * 三段布局：左侧 8 种节点面板（拖入画布）、中间画布（连线即蓝图边）、
 * 右侧属性面板（编辑选中节点的 params）。底部 Lua 预览调内核
 * `blueprint.compile`（经网关 IPC 代理，UI 永不自己拼 Lua），编辑去抖后自动
 * 重编译；编译拒绝（环 / 无可达 action / 非法字段 / 超 100 节点）原样内联
 * 渲染 —— 消息本身带节点 id（编译链三阶段保证），绝不静默。
 *
 * 边只有两类，跟编译链的语义一一对应：值生产者（data_source/constant）→
 * 条件 是 操作数边（info 色）；条件/逻辑门之间是 守卫链边（primary 色）。
 * 守卫边可翻转极性（`when`，编译链里的 `not (…)`），取反的边用流动动画标出。
 *
 * 状态分两层：画布元素（位置、选中）住在 vue-flow 自己的 store 里（显式
 * flowId 让页面 setup 与 <VueFlow> 命中同一个实例），蓝图语义（type/params/
 * when）另存 bpNodes/bpEdges，两侧用节点 id 对齐。画布位置属于 UI 状态，
 * 永不进蓝图文档。保存 = 组装 blueprint.json → `blueprint.save`（内核校验
 * 名字、现场编译、落盘三件套并回执 sha256）；导出 = 前端把当前蓝图 JSON
 * 下载为文件。UI 从不碰文件系统 —— 落盘是内核的事，UI 只拿回执。
 */
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import type { Component } from 'vue'
import { VueFlow, Handle, Panel, Position, MarkerType, useVueFlow } from '@vue-flow/core'
import type { Edge as FlowEdge, Connection } from '@vue-flow/core'
import type { LucideProps } from 'lucide-vue-next'
import { Database, ToggleLeft, ShoppingCart, Tag, PauseCircle, Hash, GitMerge, Split, Save, Download, Trash2, RefreshCw } from 'lucide-vue-next'
import '@vue-flow/core/dist/style.css'
import '@vue-flow/core/dist/theme-default.css'
import { api, ApiError } from '@/api/client'
import {
  FIELD_WHITELIST, CONDITION_OPS,
  NODE_SPECS, nodeSpec, defaultParams, edgeFamily, isValueProducer,
  starterBlueprint, toBlueprintDoc,
} from '@/lib/blueprint'
import type { BlueprintDoc, BlueprintNodeType, BpNode, BpEdge, NodeFamily } from '@/lib/blueprint'
import { highlightLua } from '@/lib/lua-highlight'
import { usePanelStore } from '@/stores/panel'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Switch from '@/components/ui/switch/Switch.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'

// ── 面板：8 种节点，图标 + 分组 ─────────────────────────────────────────────

/** 面板条目图标 —— 8 种节点一眼可辨，跟 NODE_SPECS 顺序一致。 */
const TYPE_ICONS: Record<BlueprintNodeType, Component<LucideProps>> = {
  data_source: Database,
  condition: ToggleLeft,
  action_buy: ShoppingCart,
  action_sell: Tag,
  action_hold: PauseCircle,
  constant: Hash,
  logic_and: GitMerge,
  logic_or: Split,
}

const FAMILY_LABEL: Record<NodeFamily, string> = {
  source: '值 · 输入',
  condition: '判断',
  gate: '守卫逻辑',
  action: '动作',
}

const paletteGroups = (['source', 'condition', 'gate', 'action'] as NodeFamily[]).map((family) => ({
  family,
  label: FAMILY_LABEL[family],
  types: NODE_SPECS.filter((s) => s.family === family).map((s) => s.type),
}))

// ── 画布 store：显式 id，页面与 <VueFlow> 共用同一实例 ───────────────────────

const FLOW_ID = 'blueprint-canvas'

const {
  nodes: storeNodes, addNodes, addEdges, removeNodes, removeEdges, findNode, screenToFlowCoordinate,
  onConnect, onPaneClick, onNodeClick, onNodeDragStop,
} = useVueFlow(FLOW_ID)

// ── 蓝图语义状态 ───────────────────────────────────────────────────────────

const bpNodes = ref<BpNode[]>([])
const bpEdges = ref<BpEdge[]>([])
const strategyName = ref('starter_dog')
const selectedId = ref<string | null>(null)
let idSeq = 1

function nextId(): string {
  // 复访画布（tab 切回）时 store 里可能已有 n1…n9：从存量里接续编号。
  while (findNode(`n${idSeq}`) !== undefined) idSeq++
  return `n${idSeq++}`
}

const selectedNode = computed(() => bpNodes.value.find((n) => n.id === selectedId.value) ?? null)
const selectedSpec = computed(() => (selectedNode.value ? nodeSpec(selectedNode.value.type) : null))
/** 选中节点连出的边（守卫链极性开关的目标）。 */
const selectedOutEdges = computed(() => bpEdges.value.filter((e) => e.from === selectedNode.value?.id))

function typeOf(nodeId: string): BlueprintNodeType | undefined {
  return bpNodes.value.find((n) => n.id === nodeId)?.type
}

/** 画布边的 id —— store 与语义侧共用同一公式，删/翻极性时对得上。 */
function edgeVisualId(e: BpEdge): string {
  return `e_${e.from}_${e.to}_${e.when ? 't' : 'f'}`
}

/** 语义边 → 画布边：族决定配色（操作数=info / 守卫=primary），取反=流动。 */
function toFlowEdge(e: BpEdge): FlowEdge {
  const operand = edgeFamily(typeOf(e.from) ?? '') === 'operand'
  return {
    id: edgeVisualId(e),
    source: e.from,
    target: e.to,
    class: operand ? 'bp-edge-operand' : 'bp-edge-guard',
    animated: operand ? false : !e.when,
    markerEnd: MarkerType.ArrowClosed,
  }
}

function pushFlowEdge(e: BpEdge): void {
  addEdges([toFlowEdge(e)])
}

// ── 画布事件 ───────────────────────────────────────────────────────────────

onNodeClick(({ node }) => {
  selectedId.value = node.id
})

onPaneClick(() => {
  selectedId.value = null
})

onNodeDragStop(() => {
  // 位置住在 vue-flow store 里，无需回写语义侧 —— 这里只占位以显式声明
  // "拖动不改变蓝图文档"（序列化只读 bpNodes/bpEdges）。
})

/** 连线 = 语义边。源的类型决定它是操作数还是守卫链（编译链自己的分法）。 */
onConnect(({ source, target }: Connection) => {
  if (!source || !target || source === target) return
  const srcType = typeOf(source)
  const dstType = typeOf(target)
  if (!srcType || !dstType) return
  // 重复边不收：同 from→to 在蓝图文档里一义（`when` 在属性面板翻）。
  if (bpEdges.value.some((e) => e.from === source && e.to === target)) return
  const e: BpEdge = { from: source, to: target, when: edgeFamily(srcType) === 'guard' }
  bpEdges.value.push(e)
  pushFlowEdge(e)
  void scheduleCompile()
})

// ── 节点/边增删 ────────────────────────────────────────────────────────────

/** 从面板拖入：drop 坐标 → 画布坐标，落一个带合法默认 params 的新节点。 */
function onDrop(event: DragEvent): void {
  const type = event.dataTransfer?.getData('application/x-bp-type') as BlueprintNodeType | ''
  if (!type || !NODE_SPECS.some((s) => s.type === type)) return
  event.preventDefault()
  const pos = screenToFlowCoordinate({ x: event.clientX, y: event.clientY })
  const id = nextId()
  bpNodes.value.push({ id, type, params: defaultParams(type) })
  addNodes([{ id, type: 'bp', position: pos, data: { bpType: type } }])
  selectedId.value = id
  void scheduleCompile()
}

function removeNode(id: string): void {
  bpNodes.value = bpNodes.value.filter((n) => n.id !== id)
  bpEdges.value = bpEdges.value.filter((e) => e.from !== id && e.to !== id)
  // store.removeNodes 会连带移除挂在节点上的边。
  removeNodes([id])
  if (selectedId.value === id) selectedId.value = null
  void scheduleCompile()
}

function removeEdge(e: BpEdge): void {
  bpEdges.value = bpEdges.value.filter((x) => !(x.from === e.from && x.to === e.to))
  removeEdges([edgeVisualId(e)])
  void scheduleCompile()
}

/** 翻转守卫边极性（when）：语义侧改，画布边换 id/动画重画。 */
function flipWhen(e: BpEdge, when: boolean): void {
  removeEdges([edgeVisualId(e)])
  e.when = when
  pushFlowEdge(e)
  void scheduleCompile()
}

/** 清空画布：空图编译会立刻报"无可达 action"—— 这正是反向验收的样子。 */
function clearCanvas(): void {
  for (const n of [...bpNodes.value]) removeNodes([n.id])
  bpNodes.value = []
  bpEdges.value = []
  selectedId.value = null
  void scheduleCompile()
}

/** 重置为 4 节点示例模板（price ≤ 0.25 → trend_confirmed → buy）。 */
function loadStarter(): void {
  clearCanvas()
  const starter = starterBlueprint()
  strategyName.value = starter.name
  const idMap = new Map<string, string>()
  let y = 40
  for (const n of starter.nodes) {
    const id = nextId()
    idMap.set(n.id, id)
    bpNodes.value.push({ id, type: n.type, params: structuredClone(n.params) })
    addNodes([{ id, type: 'bp', position: { x: 80, y }, data: { bpType: n.type } }])
    y += 110
  }
  for (const e of starter.edges) {
    const mapped: BpEdge = { from: idMap.get(e.from)!, to: idMap.get(e.to)!, when: e.when }
    bpEdges.value.push(mapped)
    pushFlowEdge(mapped)
  }
  selectedId.value = null
  void scheduleCompile()
}

// ── 属性面板的参数编辑 ─────────────────────────────────────────────────────

function setParam(key: string, value: unknown): void {
  const n = selectedNode.value
  if (!n) return
  if (value === '' || value === undefined) delete n.params[key]
  else n.params[key] = value
  void scheduleCompile()
}

function numParam(key: string): string {
  const v = selectedNode.value?.params[key]
  return typeof v === 'number' ? String(v) : ''
}

function parseNumber(raw: string): number | '' {
  const v = Number(raw)
  return raw !== '' && Number.isFinite(v) ? v : ''
}

/** 自由文本参数：数字就存数字，"true"/"false" 存布尔，其余按字符串 —— 内核把关。 */
function parseLoose(raw: string): unknown {
  if (raw === 'true') return true
  if (raw === 'false') return false
  const v = Number(raw)
  return raw !== '' && Number.isFinite(v) ? v : raw
}

// ── 编译（去抖） ───────────────────────────────────────────────────────────

const compiling = ref(false)
const compileError = ref<string | null>(null)
const luaSource = ref<string | null>(null)
const luaLines = computed(() => (luaSource.value ? highlightLua(luaSource.value) : []))
const compileOk = computed(() => luaSource.value !== null && !compileError.value)

let compileTimer: ReturnType<typeof setTimeout> | null = null

function scheduleCompile(): void {
  if (compileTimer) clearTimeout(compileTimer)
  compileTimer = setTimeout(() => { void doCompile() }, 350)
}
onBeforeUnmount(() => {
  if (compileTimer) clearTimeout(compileTimer)
})

/** 当前画布 → 蓝图 JSON 文档（v1 wire 形，字段/枚举全部镜像编译链）。 */
const blueprintJson = computed(() => toBlueprintDoc(strategyName.value, bpNodes.value, bpEdges.value))

async function doCompile(): Promise<void> {
  if (compileTimer) { clearTimeout(compileTimer); compileTimer = null }
  compiling.value = true
  try {
    const doc = await api.blueprintCompile(blueprintJson.value)
    if (doc.error) {
      // 内联渲染、含节点 id 的拒绝消息 —— 反向验收的可见性就在这一行。
      compileError.value = doc.error
      luaSource.value = null
    } else {
      compileError.value = null
      luaSource.value = doc.lua
    }
  } catch (e) {
    compileError.value = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e)
    luaSource.value = null
  } finally {
    compiling.value = false
  }
}

// ── 保存（blueprint.save）与导出 ───────────────────────────────────────────

const saveName = ref('')
const saving = ref(false)
const saveReceipt = ref<string | null>(null)
const saveError = ref<string | null>(null)
const needOverwrite = ref(false)

function syncSaveName(v: string): void {
  strategyName.value = v
  saveName.value = v
}

const canSave = computed(() => saveName.value.trim().length > 0 && !saving.value)

async function doSave(overwrite = false): Promise<void> {
  saving.value = true
  saveError.value = null
  saveReceipt.value = null
  try {
    const doc = await api.blueprintSave({
      name: saveName.value.trim(),
      json: blueprintJson.value,
      overwrite: overwrite || undefined,
    })
    if (doc.error) {
      saveError.value = doc.error
      if (/already exists/i.test(doc.error)) needOverwrite.value = true
      return
    }
    needOverwrite.value = false
    const tunableNote =
      doc.tunables.length > 0
        ? ` · 可进化旋钮 ${doc.tunables.length} 个（${doc.tunables.join('、')}）`
        : ' · 无数值锚点，不可进化'
    saveReceipt.value = [
      `已写入 ${doc.packageDir}`,
      `blueprint.json ${doc.bytes[0]}B · strategy.lua ${doc.bytes[1]}B · manifest.json ${doc.bytes[2]}B`,
      `strategy.lua sha256 ${doc.luaSha256.slice(0, 12)}…${tunableNote}`,
    ].join(' · ')
  } catch (e) {
    saveError.value = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e)
  } finally {
    saving.value = false
  }
}

/** 导出：前端把当前蓝图 JSON 下载为文件（不经内核、不落任何服务器目录）。 */
function doExport(): void {
  const blob = new Blob([blueprintJson.value], { type: 'application/json' })
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = `${strategyName.value.trim() || 'untitled_strategy'}.json`
  a.click()
  URL.revokeObjectURL(url)
}

// 首访加载示例模板：操作员从一个能跑的蓝图开始，而不是空白网格。
// 复访（tab 切回）时 store 里已有节点 —— 保留现场，不重铺。
if (storeNodes.value.length === 0) loadStarter()
syncSaveName(strategyName.value)

// ── issue 393 (⑤): 策略页「编辑策略」的预载 —— blueprint.load（只读 IPC）────────
// 只有 blueprint.save 写出的包才带 blueprint.json；手写包内核会明确拒绝，
// 这里把拒绝原样亮出来（画布保持当前内容），绝不编造一个图冒充「预载成功」。

const store = usePanelStore()
const preloading = ref(false)
const preloadError = ref<string | null>(null)

async function preloadPackage(name: string): Promise<void> {
  preloading.value = true
  preloadError.value = null
  try {
    const doc = await api.blueprintLoad(name)
    if (doc.error) {
      // 手写包没有蓝图文档（内核明确拒绝）—— 用户裁决：编辑入口必须能
      // 打开**所有** Lua 包，所以这个特定拒绝不展示为错误，而是自动落到
      // 源码模式；其余拒绝（名字不合法等）仍然原样亮出来。
      if (/no blueprint document/.test(doc.error)) {
        await openSourceMode(name, '该包是手写策略（没有蓝图文档）—— 已按源码模式打开，画布图只对蓝图编译产物有意义。')
        return
      }
      preloadError.value = doc.error
      return
    }
    let parsed: BlueprintDoc
    try {
      parsed = JSON.parse(doc.json) as BlueprintDoc
    } catch {
      preloadError.value = '蓝图文档不是合法 JSON —— 拒绝预载。'
      return
    }
    if (parsed.version !== 1 || !Array.isArray(parsed.nodes) || !Array.isArray(parsed.edges)) {
      preloadError.value = '蓝图文档不是 v1 版（内核只编译 v1）—— 拒绝预载。'
      return
    }
    // 画布元素 = 蓝图语义的投影：文档里的节点 id 就是画布 id（保存时原样
    // 序列化）。未知类型/重复 id 的行跳过 —— 编译链只产白名单类型，这里
    // 只是防御一个被手改过的文档。
    const seen = new Set<string>()
    const nodes: BpNode[] = []
    for (const n of parsed.nodes) {
      if (!n || typeof n.id !== 'string' || seen.has(n.id)) continue
      if (!NODE_SPECS.some((s) => s.type === n.type)) continue
      seen.add(n.id)
      nodes.push({ id: n.id, type: n.type, params: structuredClone(n.params ?? {}) })
    }
    const knownIds = new Set(nodes.map((n) => n.id))
    clearCanvas() // 去抖编译：清空后的拒绝会被 350ms 内的下一次编译覆盖
    let y = 40
    for (const n of nodes) {
      bpNodes.value.push(n)
      addNodes([{ id: n.id, type: 'bp', position: { x: 80, y }, data: { bpType: n.type } }])
      y += 110
    }
    for (const e of parsed.edges) {
      if (!e || !knownIds.has(e.from) || !knownIds.has(e.to)) continue
      const mapped: BpEdge = { from: e.from, to: e.to, when: !!e.when }
      bpEdges.value.push(mapped)
      pushFlowEdge(mapped)
    }
    strategyName.value = parsed.name || name
    syncSaveName(strategyName.value)
    selectedId.value = null
    void scheduleCompile()
  } catch (e) {
    preloadError.value = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e)
  } finally {
    preloading.value = false
  }
}

onMounted(() => {
  const pre = store.takeBlueprintPreload()
  if (pre) void preloadPackage(pre)
})

// ── 源码模式（blueprint.loadSource / saveSource）────────────────────────────
// 画布图是蓝图编译产物的专属表示；手写包没有蓝图文档，用户裁决：编辑入口
// 必须能打开**所有** Lua 包 —— 预载遇到「no blueprint document」时自动落到
// 这里：展示包的 Lua 源码与 manifest，允许原样改写落盘（内核重封 sha256）。

const sourceMode = ref(false)
const sourceLua = ref('')
const sourceManifest = ref('')
const sourceSha = ref('')
const sourceName = ref('')
const sourceDirty = ref(false)
const sourceSaving = ref(false)
const sourceError = ref<string | null>(null)
const sourceReceipt = ref<string | null>(null)
const sourceNeedOverwrite = ref(false)

async function openSourceMode(name: string, hint: string): Promise<void> {
  preloading.value = true
  try {
    const doc = await api.blueprintLoadSource(name)
    if (doc.error) {
      preloadError.value = doc.error
      return
    }
    preloadError.value = null
    sourceMode.value = true
    sourceName.value = doc.name
    sourceLua.value = doc.lua
    sourceManifest.value = JSON.stringify(doc.manifest, null, 2)
    sourceSha.value = doc.sha256
    sourceDirty.value = false
    sourceError.value = null
    sourceReceipt.value = null
    sourceNeedOverwrite.value = false
    // 预载提示不吞掉：操作员该知道为什么落在源码模式而不是画布。
    sourceNote.value = hint
  } catch (e) {
    preloadError.value = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e)
  } finally {
    preloading.value = false
  }
}

const sourceNote = ref<string | null>(null)

function onSourceInput(): void {
  sourceDirty.value = true
  sourceReceipt.value = null
}

async function doSaveSource(overwrite = false): Promise<void> {
  sourceSaving.value = true
  sourceError.value = null
  sourceReceipt.value = null
  try {
    const doc = await api.blueprintSaveSource({
      name: sourceName.value,
      lua: sourceLua.value,
      overwrite: overwrite || undefined,
    })
    if (doc.error) {
      sourceError.value = doc.error
      if (/must be explicit/i.test(doc.error)) sourceNeedOverwrite.value = true
      return
    }
    sourceNeedOverwrite.value = false
    sourceDirty.value = false
    sourceSha.value = doc.luaSha256
    // manifest 里的 sha256 已被内核重算 —— 重读一份回显，不本地拼接。
    const refreshed = await api.blueprintLoadSource(sourceName.value)
    if (!refreshed.error) sourceManifest.value = JSON.stringify(refreshed.manifest, null, 2)
    sourceReceipt.value = [
      `已写回 ${doc.luaPath}`,
      `strategy.lua ${doc.bytes[0]}B · manifest.json ${doc.bytes[1]}B`,
      `新 sha256 ${doc.luaSha256.slice(0, 12)}…`,
    ].join(' · ')
  } catch (e) {
    sourceError.value = e instanceof Error ? e.message : String(e)
  } finally {
    sourceSaving.value = false
  }
}

function exitSourceMode(): void {
  sourceMode.value = false
  sourceNote.value = null
}

</script>

<template>
  <div class="flex flex-col gap-4">
    <AlertBanner
      v-if="preloadError"
      tone="warn"
      title="未能预载策略包（内核 blueprint.load）"
      dismissible
      @dismiss="preloadError = null"
    >
      <span class="font-mono text-[12px] leading-relaxed break-all">{{ preloadError }}</span>
    </AlertBanner>
    <AlertBanner v-if="preloading" tone="info">正在读取策略包的蓝图文档…</AlertBanner>

    <AlertBanner v-if="compileError" tone="error" title="编译拒绝（内核 blueprint.compile）">
      <span class="font-mono text-[12px] leading-relaxed break-all">{{ compileError }}</span>
    </AlertBanner>

    <!-- 源码模式：手写策略包的编辑面（blueprint.loadSource / saveSource）。 -->
    <Card v-if="sourceMode" dense class="flex flex-col gap-3">
      <div class="flex flex-wrap items-center gap-2">
        <p class="label-micro flex-1">
          源码模式 · {{ sourceName }}
          <span v-if="sourceDirty" class="ml-2 text-[11px] text-gold-400">有未保存修改</span>
        </p>
        <Button variant="ghost" size="sm" @click="exitSourceMode">关闭源码模式</Button>
        <Button
          variant="default"
          size="sm"
          :disabled="sourceSaving || (!sourceDirty && !sourceNeedOverwrite)"
          @click="doSaveSource(false)"
        >保存源码</Button>
        <Button
          v-if="sourceNeedOverwrite"
          variant="default"
          size="sm"
          :disabled="sourceSaving"
          @click="doSaveSource(true)"
        >确认覆盖写回</Button>
      </div>
      <AlertBanner v-if="sourceNote" tone="info" dismissible @dismiss="sourceNote = null">
        {{ sourceNote }}
      </AlertBanner>
      <AlertBanner
        v-if="sourceError"
        tone="error"
        title="写回被拒（内核 blueprint.saveSource）"
      >
        <span class="font-mono text-[12px] leading-relaxed break-all">{{ sourceError }}</span>
      </AlertBanner>
      <AlertBanner v-if="sourceReceipt" tone="info" title="已落盘">
        <span class="font-mono text-[12px] leading-relaxed break-all">{{ sourceReceipt }}</span>
      </AlertBanner>
      <label class="label-micro" for="bp-source-lua">strategy.lua（当前 sha256 {{ sourceSha.slice(0, 12) }}…）</label>
      <textarea
        id="bp-source-lua"
        v-model="sourceLua"
        class="h-80 w-full resize-y rounded-md border border-line bg-panel-2 p-3 font-mono text-[12.5px] leading-relaxed text-fg outline-none focus:border-primary"
        spellcheck="false"
        @input="onSourceInput"
      ></textarea>
      <label class="label-micro" for="bp-source-manifest">manifest.json（只读回显；sha256 由内核在写回时重算）</label>
      <pre
        id="bp-source-manifest"
        class="max-h-40 overflow-auto rounded-md border border-line bg-panel-2 p-3 font-mono text-[11.5px] leading-relaxed text-faint-fg"
      >{{ sourceManifest }}</pre>
    </Card>

    <div class="grid grid-cols-[190px_minmax(0,1fr)_290px] gap-4">
      <!-- 左：8 种节点面板 -->
      <div class="flex flex-col gap-3 self-start">
        <Card v-for="g in paletteGroups" :key="g.family" dense class="!card-pad-sm">
          <p class="label-micro mb-2">{{ g.label }}</p>
          <div class="flex flex-col gap-1.5">
            <button
              v-for="t in g.types"
              :key="t"
              class="flex cursor-grab items-center gap-2 rounded-md border border-line bg-panel-2 px-2.5 py-2 text-left text-[12.5px] text-fg transition hover:border-line-strong active:cursor-grabbing"
              draggable="true"
              @dragstart="($event as DragEvent).dataTransfer?.setData('application/x-bp-type', t)"
            >
              <component :is="TYPE_ICONS[t]" class="size-3.5 text-primary" />
              <span class="font-medium">{{ nodeSpec(t).label }}</span>
            </button>
          </div>
        </Card>
      </div>

      <!-- 中：画布 -->
      <Card dense class="relative h-[560px] overflow-hidden !p-0">
        <div
          class="absolute inset-0"
          @drop="onDrop"
          @dragover.prevent
        >
          <VueFlow
            :id="FLOW_ID"
            :delete-key-code="null"
            :min-zoom="0.4"
            :max-zoom="1.6"
            :snap-to-grid="true"
            :snap-grid="[10, 10]"
            fit-view-on-init
          >
            <template #node-bp="nodeProps">
              <div
                class="bp-node"
                :class="{ 'bp-node-selected': selectedId === nodeProps.id }"
              >
                <Handle type="target" :position="Position.Left" class="bp-handle" />
                <component :is="TYPE_ICONS[nodeProps.data?.bpType as BlueprintNodeType]" class="size-3.5 text-primary" />
                <div class="min-w-0">
                  <p class="text-[12px] font-semibold text-fg leading-tight">{{ nodeSpec((nodeProps.data?.bpType ?? '') as string).label }}</p>
                  <p class="truncate font-mono text-[10.5px] text-muted-fg" :title="nodeProps.id">{{ nodeProps.id }}</p>
                </div>
                <Handle type="source" :position="Position.Right" class="bp-handle" />
              </div>
            </template>
            <Panel position="top-left" class="!m-2 flex items-center gap-2 rounded-md border border-line bg-panel-2/90 px-2.5 py-1.5 backdrop-blur">
              <span class="label-micro">图元</span><span class="stat-num text-[15px]">{{ bpNodes.length }}</span><span class="text-faint-fg text-[11px]">节点</span><span class="stat-num text-[15px]">{{ bpEdges.length }}</span><span class="text-faint-fg text-[11px]">边</span>
              <Badge v-if="compileOk" variant="up" dot>编译通过</Badge>
              <Badge v-else-if="compileError" variant="down">拒绝</Badge>
              <Badge v-else-if="compiling" variant="info">编译中…</Badge>
            </Panel>
            <Panel position="top-right" class="!m-2 flex gap-1.5">
              <Button variant="ghost" size="icon-sm" title="重置为示例模板" @click="loadStarter"><RefreshCw /></Button>
              <Button variant="ghost" size="icon-sm" title="清空画布" @click="clearCanvas"><Trash2 /></Button>
            </Panel>
          </VueFlow>
        </div>
      </Card>

      <!-- 右：属性面板 -->
      <div class="flex flex-col gap-3 self-start">
        <Card dense>
          <CardHeader label="策略名" />
          <Input :model-value="strategyName" placeholder="strategy_name" size="sm" @update:model-value="syncSaveName" />
          <p class="mt-1.5 text-[11px] leading-snug text-faint-fg">
            保存为策略包名：ASCII 字母 / 数字 / _ / -，≤64 字符（内核把关）。
          </p>
        </Card>

        <Card dense>
          <CardHeader label="属性" />
          <template v-if="selectedNode && selectedSpec">
            <div class="mb-2 flex items-center gap-2">
              <component :is="TYPE_ICONS[selectedNode.type]" class="size-3.5 text-primary" />
              <span class="text-[13px] font-semibold text-fg">{{ selectedSpec.label }}</span>
              <span class="font-mono text-[10.5px] text-faint-fg">{{ selectedNode.id }}</span>
            </div>
            <p class="mb-3 text-[11px] leading-snug text-faint-fg">{{ selectedSpec.hint }}</p>

            <div class="flex flex-col gap-2.5">
              <template v-if="selectedNode.type === 'data_source'">
                <label class="flex flex-col gap-1">
                  <span class="text-[11.5px] text-muted-fg">字段（白名单 6 选 1）</span>
                  <select
                    class="h-8 rounded-md border border-line bg-panel-2 px-2 text-[12.5px] text-fg outline-none focus:border-primary/55"
                    :value="String(selectedNode.params.field ?? '')"
                    @change="setParam('field', ($event.target as HTMLSelectElement).value)"
                  >
                    <option v-for="f in FIELD_WHITELIST" :key="f" :value="f">{{ f }}</option>
                  </select>
                </label>
              </template>

              <template v-else-if="selectedNode.type === 'condition'">
                <label class="flex flex-col gap-1">
                  <span class="text-[11.5px] text-muted-fg">比较符</span>
                  <select
                    class="h-8 rounded-md border border-line bg-panel-2 px-2 text-[12.5px] text-fg outline-none focus:border-primary/55"
                    :value="String(selectedNode.params.op ?? '')"
                    @change="setParam('op', ($event.target as HTMLSelectElement).value)"
                  >
                    <option v-for="op in CONDITION_OPS" :key="op" :value="op">{{ op }}</option>
                  </select>
                </label>
                <label v-if="selectedNode.params.op !== 'in'" class="flex flex-col gap-1">
                  <span class="text-[11.5px] text-muted-fg">阈值（数字 / 文本 / true|false）</span>
                  <Input size="sm" :model-value="String(selectedNode.params.value ?? '')" @update:model-value="setParam('value', parseLoose($event))" />
                </label>
                <p v-else class="text-[11px] leading-snug text-faint-fg">`in` 的 value 是数组：先导出 JSON 手填成员再保存（保存按当前画布，数组成员留在导出文件里）。</p>
                <label class="flex flex-col gap-1">
                  <span class="text-[11.5px] text-muted-fg">左操作数字段（可选；留空 = 吃操作数入边）</span>
                  <select
                    class="h-8 rounded-md border border-line bg-panel-2 px-2 text-[12.5px] text-fg outline-none focus:border-primary/55"
                    :value="String(selectedNode.params.field ?? '')"
                    @change="setParam('field', ($event.target as HTMLSelectElement).value)"
                  >
                    <option value="">（用入边）</option>
                    <option v-for="f in FIELD_WHITELIST" :key="f" :value="f">{{ f }}</option>
                  </select>
                </label>
              </template>

              <template v-else-if="selectedNode.type === 'action_buy' || selectedNode.type === 'action_sell'">
                <label class="flex flex-col gap-1">
                  <span class="text-[11.5px] text-muted-fg">限价（数字；留空 = 市价建议）</span>
                  <Input size="sm" :model-value="numParam('price')" placeholder="0.25" @update:model-value="setParam('price', parseNumber($event))" />
                </label>
                <label class="flex flex-col gap-1">
                  <span class="text-[11.5px] text-muted-fg">预算占比 budget_ratio</span>
                  <Input size="sm" :model-value="numParam('budget_ratio')" placeholder="0.1" @update:model-value="setParam('budget_ratio', parseNumber($event))" />
                </label>
              </template>

              <template v-else-if="selectedNode.type === 'constant'">
                <label class="flex flex-col gap-1">
                  <span class="text-[11.5px] text-muted-fg">值（数字 / 文本 / true|false）</span>
                  <Input size="sm" :model-value="String(selectedNode.params.value ?? '')" @update:model-value="setParam('value', parseLoose($event))" />
                </label>
              </template>

              <p v-else-if="selectedNode.type !== 'action_hold'" class="text-[11.5px] leading-snug text-muted-fg">
                逻辑门无参数；把 ≥2 条守卫边接进来，输出接到动作。
              </p>
              <p v-else class="text-[11.5px] leading-snug text-muted-fg">
                持有动作无参数；它只输出守卫，不下单。
              </p>

              <div v-if="!isValueProducer(selectedNode.type) && selectedOutEdges.length > 0" class="flex flex-col gap-1.5 border-t border-line pt-2.5">
                <span class="text-[11.5px] text-muted-fg">出边极性（when）</span>
                <div v-for="e in selectedOutEdges" :key="`${e.from}>${e.to}`" class="flex items-center justify-between gap-2">
                  <span class="font-mono text-[10.5px] text-faint-fg">→ {{ e.to }}</span>
                  <Switch :model-value="e.when" label="取反" @update:model-value="(v: boolean) => flipWhen(e, v)" />
                </div>
              </div>
            </div>

            <Button variant="danger" size="sm" class="mt-3 w-full" @click="removeNode(selectedNode.id)">
              <Trash2 /> 删除节点
            </Button>
          </template>
          <EmptyState v-else text="未选中节点" hint="在画布上点一个节点编辑参数；从左侧拖入新节点。" :icon="false" compact />
        </Card>

        <Card dense>
          <CardHeader label="边（× 删除）" />
          <div v-if="bpEdges.length" class="flex flex-col gap-1">
            <div v-for="e in bpEdges" :key="`${e.from}>${e.to}`" class="flex items-center gap-1.5 text-[11px]">
              <Badge :variant="edgeFamily(typeOf(e.from) ?? '') === 'operand' ? 'info' : 'gold'" class="!px-1.5 !text-[10px]">
                {{ edgeFamily(typeOf(e.from) ?? '') === 'operand' ? '操作数' : '守卫' }}
              </Badge>
              <span class="min-w-0 flex-1 truncate font-mono text-faint-fg">{{ e.from }}→{{ e.to }}</span>
              <button class="text-faint-fg transition hover:text-down" title="删除这条边" @click="removeEdge(e)">×</button>
            </div>
          </div>
          <EmptyState v-else text="还没有边" hint="从节点右侧圆点拖到目标节点左侧圆点。" :icon="false" compact />
        </Card>
      </div>
    </div>

    <!-- 底：Lua 预览 + 保存/导出 -->
    <Card dense>
      <CardHeader label="Lua 预览（内核 blueprint.compile 实时生成）">
        <template #action>
          <Badge v-if="compileOk" variant="up" dot>编译通过</Badge>
          <Badge v-else-if="compiling" variant="info">编译中…</Badge>
        </template>
      </CardHeader>
      <EmptyState v-if="!luaSource && !compileError" text="暂无生成源码" hint="画布上有可编译的图后这里显示生成的 strategy.lua。" :icon="false" compact />
      <pre v-else-if="luaSource" class="max-h-72 overflow-auto rounded-md border border-line bg-panel-2 p-3 font-mono text-[11.5px] leading-[1.55]"><code><template v-for="(line, i) in luaLines" :key="i"><span v-for="(tok, j) in line" :key="j" :class="`lua-${tok.kind}`">{{ tok.text }}</span>{{
  }}</template></code></pre>
      <div class="mt-3 flex flex-wrap items-center gap-2">
        <Input :model-value="saveName" size="sm" class="!w-52" placeholder="保存名（ASCII）" @update:model-value="syncSaveName" />
        <Button variant="gold" size="sm" :disabled="!canSave" @click="doSave(false)"><Save /> 保存策略包</Button>
        <Button variant="default" size="sm" :disabled="!canSave" @click="doExport"><Download /> 导出 JSON</Button>
        <Button v-if="needOverwrite" variant="danger" size="sm" :disabled="saving" @click="doSave(true)">同名已存在 —— 确认覆盖</Button>
      </div>
      <AlertBanner v-if="saveError" tone="error" title="保存被拒（内核 blueprint.save）" class="mt-2.5">
        <span class="font-mono text-[12px] break-all">{{ saveError }}</span>
      </AlertBanner>
      <AlertBanner v-else-if="saveReceipt" tone="info" title="已保存" class="mt-2.5">{{ saveReceipt }}</AlertBanner>
    </Card>
  </div>
</template>

<style>
/* 画布节点/边样式 —— 只用主题 token，暗浅两态都成立。
   （<style> 不加 scoped：vue-flow 的节点渲染在画布自己的子树里，
   scoped 属性选择器够不着它们。） */
.bp-node {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  min-width: 118px;
  padding: 0.4rem 0.7rem;
  border: 1px solid var(--line);
  border-radius: 8px;
  background: var(--panel-2);
  color: var(--fg);
  box-shadow: 0 1px 4px oklch(0 0 0 / 0.18);
  font-size: 12px;
}
.bp-node-selected {
  border-color: oklch(from var(--primary) l c h / 0.7);
  box-shadow: 0 0 0 3px oklch(from var(--primary) l c h / 0.16);
}
.bp-handle {
  width: 9px;
  height: 9px;
  border: 1.5px solid var(--line-strong);
  background: var(--primary);
}
.bp-edge-operand path {
  stroke: var(--info);
}
.bp-edge-guard path {
  stroke: var(--primary);
}
.lua-comment { color: var(--faint-fg); font-style: italic; }
.lua-string { color: var(--up); }
.lua-number { color: var(--primary); }
.lua-keyword { color: var(--primary); font-weight: 600; }
.lua-api { color: var(--info); }
</style>
