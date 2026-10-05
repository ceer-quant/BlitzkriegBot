<script setup lang="ts">
/**
 * 执行策略 · 规则构建器表单（Issue 364 拆出）：一条规则的 WHEN/THEN 编辑。
 * 词表来自 lib/policy-fields.ts；「确定」在这里做值类型折叠（数字字段转
 * number、symbol/in 为文本、in 拆逗号），校验失败经 `invalid` 事件上抛给
 * 页面显示，不自己渲染错误条。
 */
import { ref } from 'vue'
import type { PolicyRuleView, PolicyThenView, PolicyWhenView } from '@/api/client'
import { CONDITION_FIELDS, CONDITION_OPS, THEN_ACTIONS, conditionFieldMeta } from '@/lib/policy-fields'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'

const props = defineProps<{
  /** 编辑中的规则（已 structuredClone，本地可变）。 */
  draft: PolicyRuleView
  /** null = 新建；否则是被编辑规则的在列下标。 */
  index: number | null
}>()

const emit = defineEmits<{
  commit: [rule: PolicyRuleView]
  cancel: []
  invalid: [message: string]
}>()

/** 构建器的 when.value 以文本编辑：数字、符号，或 `in` 的逗号分隔列表。 */
const valueText = ref(valueFor(props.draft.when.value))
const thenArg = ref('')

function valueFor(v: PolicyWhenView['value']): string {
  if (Array.isArray(v)) return v.join(',')
  return v == null ? '' : String(v)
}

const fieldMeta = () => conditionFieldMeta(props.draft.when.field)

function emitInvalid(message: string): boolean {
  emit('invalid', message)
  return true
}

/** 表单 → 规则对象：值类型跟字段走（数字字段转 number，symbol/in 为文本）。 */
function commit(): void {
  const d = props.draft
  let value: PolicyWhenView['value']
  if (d.when.op === 'in') {
    value = valueText.value.split(',').map((s) => s.trim()).filter(Boolean)
    if (!value.length) { emitInvalid('in 条件至少要一个标的'); return }
  } else if (fieldMeta().numeric) {
    const n = Number(valueText.value)
    if (!Number.isFinite(n)) { emitInvalid('条件值必须是数字'); return }
    value = n
  } else {
    value = valueText.value.trim()
    if (!value) { emitInvalid('条件值不能为空'); return }
  }
  d.when.value = value
  const action = d.then.action ?? 'skip'
  const then: PolicyThenView = { action }
  if (action !== 'skip') {
    const arg = thenArg.value.trim()
    if (action === 'cooldown_sec') {
      const secs = Number(arg)
      if (!Number.isInteger(secs) || secs <= 0) { emitInvalid('冷静期必须是正整数秒'); return }
      then.cooldown_sec = secs
    } else {
      if (!arg) { emitInvalid('该动作需要一个数值'); return }
      then[action] = arg
    }
  }
  d.then = then
  d.name = d.name.trim()
  if (!d.name) { emitInvalid('规则需要一个名字'); return }
  emit('commit', d)
}
</script>

<template>
  <div class="rounded-lg border border-primary/30 bg-primary/5 px-3 py-3">
    <div class="text-[12px] font-semibold">
      {{ index == null ? '新规则' : `编辑规则：${draft.name}` }}
    </div>
    <div class="mt-2 grid gap-2 sm:grid-cols-2">
      <label class="block">
        <span class="label-micro">规则名</span>
        <Input v-model="draft.name" class="mt-1" type="text" placeholder="如 大额冷静" />
      </label>
      <label class="block">
        <span class="label-micro">WHEN 字段</span>
        <select v-model="draft.when.field" class="policy-select mt-1">
          <option v-for="f in CONDITION_FIELDS" :key="f.value" :value="f.value">{{ f.label }}</option>
        </select>
      </label>
      <label class="block">
        <span class="label-micro">比较符</span>
        <select v-model="draft.when.op" class="policy-select mt-1">
          <option v-for="op in CONDITION_OPS" :key="op" :value="op">{{ op }}</option>
        </select>
      </label>
      <label class="block">
        <span class="label-micro">
          条件值{{ fieldMeta().unit ? `（${fieldMeta().unit}）` : '' }}
          <span v-if="draft.when.op === 'in'" class="text-faint-fg">（逗号分隔多个标的）</span>
        </span>
        <Input v-model="valueText" class="mt-1" type="text" placeholder="如 2 或 BTC,ETH" />
      </label>
      <label class="block">
        <span class="label-micro">THEN 动作</span>
        <select v-model="draft.then.action" class="policy-select mt-1">
          <option v-for="a in THEN_ACTIONS" :key="a.value" :value="a.value">{{ a.label }}</option>
        </select>
      </label>
      <label v-if="draft.then.action && draft.then.action !== 'skip'" class="block">
        <span class="label-micro">动作参数（{{ draft.then.action === 'cooldown_sec' ? '秒' : 'USD / 比例' }}）</span>
        <Input v-model="thenArg" class="mt-1" type="text" />
      </label>
      <label class="block sm:col-span-2">
        <span class="label-micro">备注（可选，写进审计）</span>
        <Input :model-value="draft.reason ?? ''" class="mt-1" type="text" placeholder="为什么要有这条规则" @update:model-value="draft.reason = $event" />
      </label>
    </div>
    <div class="mt-3 flex items-center gap-2">
      <Button size="sm" @click="commit">确定</Button>
      <Button variant="outline" size="sm" @click="emit('cancel')">取消</Button>
    </div>
  </div>
</template>
