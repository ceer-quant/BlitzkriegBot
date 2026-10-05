<script setup lang="ts">
/**
 * 执行策略 · 规则编辑器（Issue 364 拆出）：规则卡片（拖拽 = priority 重排）、
 * 条件构建器（表单在 PolicyRuleBuilder）、内核预览重放、保存/回滚动作条。
 * 规则缓冲经 v-model 与页面共享；落盘动作上抛（save/rollback）——写路径仍在
 * 页面手里（set 带「基础参数 + 规则」整体），这里只管编辑与展示。
 */
import { ref } from 'vue'
import type { ExecutionPolicyPreviewDoc, PolicyRuleView, PolicyThenView } from '@/api/client'
import { dateTime } from '@/lib/format'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import PolicyRuleBuilder from './PolicyRuleBuilder.vue'

const rules = defineModel<PolicyRuleView[]>({ required: true })

const props = defineProps<{
  /** 内核 evaluate 重放最近已平仓交易的预览（可能缺席）。 */
  preview: ExecutionPolicyPreviewDoc | null
  /** 版本号 = 审计记录数。 */
  version: number
  busy: boolean
  saving: boolean
}>()

const emit = defineEmits<{
  touch: []
  save: []
  rollback: []
  invalid: [message: string]
}>()

/** 新建/编辑中的规则（null = 构建器收起）。 */
const ruleDraft = ref<PolicyRuleView | null>(null)
const ruleDraftIdx = ref<number | null>(null)
/** 拖拽中的规则下标（HTML5 原生拖放排序优先级）。 */
const dragIdx = ref<number | null>(null)
const dragOverIdx = ref<number | null>(null)

function startAddRule(): void {
  ruleDraftIdx.value = null
  ruleDraft.value = {
    name: '',
    priority: (rules.value.reduce((m, r) => Math.max(m, r.priority ?? 0), 0) || 90) + 10,
    enabled: true,
    when: { field: 'open_positions', op: '>=', value: 2 },
    then: { action: 'skip' },
    reason: '',
  }
}

function startEditRule(i: number): void {
  ruleDraftIdx.value = i
  ruleDraft.value = structuredClone(rules.value[i])
}

function commitDraft(d: PolicyRuleView): void {
  if (ruleDraftIdx.value == null) rules.value.push(d)
  else rules.value[ruleDraftIdx.value] = d
  ruleDraft.value = null
  ruleDraftIdx.value = null
  emit('touch')
}

function removeRule(i: number): void {
  rules.value.splice(i, 1)
  emit('touch')
}

function cancelDraft(): void {
  ruleDraft.value = null
  ruleDraftIdx.value = null
}

/** 优先级重排（HTML5 拖放）：拖到目标位置后，按新顺序把 priority 重写成
 *  10, 20, 30…（升序、互不相等 — 内核按 priority 升序取第一个命中的规则）。 */
function onDrop(target: number): void {
  const from = dragIdx.value
  dragIdx.value = null
  dragOverIdx.value = null
  if (from == null || from === target) return
  const [moved] = rules.value.splice(from, 1)
  rules.value.splice(target, 0, moved)
  rules.value.forEach((r, i) => { r.priority = (i + 1) * 10 })
  emit('touch')
}

function thenText(t: PolicyThenView): string {
  if (t.action === 'skip') return '跳过本单'
  if (t.action === 'cooldown_sec') return `冷静 ${t.cooldown_sec}s`
  if (t.action === 'budget_ratio') return `下注比例 ${t.budget_ratio}`
  if (t.action === 'min_budget_usd') return `最小下注 $${t.min_budget_usd}`
  if (t.action === 'max_budget_usd') return `最大下注 $${t.max_budget_usd}`
  return JSON.stringify(t)
}
function verdictBadge(v: string): { text: string; variant: 'up' | 'down' | 'gold' | 'default' } {
  switch (v) {
    case 'place': return { text: '放行', variant: 'up' }
    case 'skip': return { text: '跳过', variant: 'down' }
    case 'cooldown': return { text: '冷静中', variant: 'gold' }
    default: return { text: v, variant: 'default' }
  }
}
</script>

<template>
  <div>
    <div class="flex items-center justify-between">
      <span class="label-micro">规则（拖拽卡片调整优先级）</span>
      <Button variant="outline" size="sm" :disabled="ruleDraft != null" @click="startAddRule">
        + 新规则
      </Button>
    </div>
    <div class="mt-2 space-y-2">
      <div
        v-for="(rule, i) in rules"
        :key="`${rule.name}-${i}`"
        draggable="true"
        class="rounded-lg border bg-panel-2 px-3 py-2 text-[12px] transition-opacity"
        :class="[
          rule.enabled ? 'border-line' : 'border-line opacity-55',
          dragOverIdx === i && dragIdx !== i ? 'border-primary/50' : '',
        ]"
        @dragstart="dragIdx = i"
        @dragenter.prevent="dragOverIdx = i"
        @dragover.prevent
        @drop.prevent="onDrop(i)"
        @dragend="dragIdx = null; dragOverIdx = null"
      >
        <div class="flex items-center justify-between gap-2">
          <span class="font-semibold">
            <span class="text-faint-fg">#{{ rule.priority }}</span>
            {{ rule.name }}
            <Badge :variant="rule.enabled ? 'up' : 'default'" class="ml-1">
              {{ rule.enabled ? '启用' : '停用' }}
            </Badge>
          </span>
          <span class="flex items-center gap-1">
            <Button variant="ghost" size="sm" @click="startEditRule(i)">编辑</Button>
            <Button variant="ghost" size="sm" @click="removeRule(i)">删除</Button>
          </span>
        </div>
        <div class="mt-1 text-muted-fg">
          WHEN {{ rule.when.field }} {{ rule.when.op }}
          {{ Array.isArray(rule.when.value) ? rule.when.value.join(', ') : rule.when.value }}
          → THEN {{ thenText(rule.then) }}
        </div>
      </div>
      <div v-if="!rules.length" class="rounded-lg border border-dashed border-line px-3 py-3 text-[11.5px] text-faint-fg">
        无规则 —— 只走基础参数（clamp(max(balance × ratio, min), ≤ max)）。
      </div>
    </div>

    <!-- 条件构建器（表单在 PolicyRuleBuilder，校验失败经 invalid 上抛）-->
    <div v-if="ruleDraft" class="mt-3">
      <PolicyRuleBuilder
        :key="ruleDraftIdx ?? 'new'"
        :draft="ruleDraft"
        :index="ruleDraftIdx"
        @commit="commitDraft"
        @cancel="cancelDraft"
        @invalid="emit('invalid', $event)"
      />
    </div>

    <!-- 预览：内核用自己的 evaluate 重放最近已平仓交易 -->
    <div v-if="preview" class="mt-4 rounded-lg border border-line bg-panel-2 px-3 py-2 text-[12px]">
      <div class="flex flex-wrap items-center gap-x-3 gap-y-1">
        <span class="label-micro">预览（最近已平仓交易重放）</span>
        <span class="num">最近 {{ preview.considered }} 单</span>
        <span class="text-muted-fg">跳过 {{ preview.skipped }}</span>
        <span class="text-muted-fg">平均下注 ${{ preview.avgBudgetUsd ?? '—' }}</span>
      </div>
      <div v-if="preview.rows.length" class="mt-1.5 space-y-0.5">
        <div
          v-for="(row, i) in preview.rows"
          :key="`${row.tsMs}-${i}`"
          class="flex items-center justify-between gap-2 text-[11px] text-muted-fg"
        >
          <span class="num">{{ dateTime(row.tsMs) }} · {{ row.symbol }} · 余额 ${{ row.balance }}</span>
          <Badge :variant="verdictBadge(row.verdict).variant">
            {{ verdictBadge(row.verdict).text }}<template v-if="row.detail"> · {{ row.detail }}</template>
          </Badge>
        </div>
      </div>
    </div>

    <!-- 版本与保存 -->
    <div class="mt-4 flex flex-wrap items-center gap-2">
      <Button size="sm" :disabled="saving || busy" @click="emit('save')">
        {{ saving ? '保存中…' : '保存（写入内核）' }}
      </Button>
      <Button
        variant="outline"
        size="sm"
        :disabled="saving || busy || version === 0"
        :title="version === 0 ? '还没有可回滚的历史' : `回滚到 v${version - 1}（取最近一次 set 的 before 状态写回）`"
        @click="emit('rollback')"
      >回滚到 v{{ Math.max(version - 1, 0) }}</Button>
      <span class="text-[11px] text-faint-fg">版本号 = 审计记录数（当前 v{{ version }}）</span>
    </div>
  </div>
</template>
