<script setup lang="ts">
/**
 * E25 (#331) — the decisions table (§13.4). One row per audited suggestion;
 * the columns are FIXED: 时间 / 策略 / 结果 / 关卡 / 详情. `detail` prints the
 * kernel's own `GateTrace.detail` VERBATIM — the UI never re-words a
 * rejection, or the same refusal would read three different ways in the log,
 * the panel and the audit (the refusal-attribution lesson).
 */
import type { IntentAuditRecordView } from '@/api/client'
import Badge from '@/components/ui/badge/Badge.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import { clockTime } from '@/lib/format'

const props = defineProps<{ records: IntentAuditRecordView[] }>()

/** Status → badge tone. Red only ever means the KERNEL refused. */
function statusMeta(r: IntentAuditRecordView): { label: string; tone: 'up' | 'gold' | 'down' } {
  switch (r.decision.status) {
    case 'APPROVED':
      return { label: '已通过', tone: 'up' }
    case 'MODIFIED':
      return { label: '已修改', tone: 'gold' }
    default:
      return { label: '已拒绝', tone: 'down' }
  }
}

/** The gate whose trace this row is about: the rejecting gate, else the last. */
function gateOf(r: IntentAuditRecordView): string {
  if (r.decision.status === 'REJECTED' && r.decision.gate) return r.decision.gate
  const last = r.gates[r.gates.length - 1]
  return last ? last.gate : '—'
}

/** Verbatim kernel detail — no UI-side wording (§13.4). */
function detailOf(r: IntentAuditRecordView): string {
  if (r.decision.status === 'REJECTED') return r.decision.detail ?? ''
  const last = r.gates[r.gates.length - 1]
  return last ? last.detail : ''
}
</script>

<template>
  <div v-if="props.records.length" class="-mx-2 overflow-x-auto">
    <table class="w-full min-w-[680px] text-[13px]">
      <thead>
        <tr class="text-left">
          <th class="label-micro px-2 pb-2.5">时间</th>
          <th class="label-micro px-2 pb-2.5">策略</th>
          <th class="label-micro px-2 pb-2.5">结果</th>
          <th class="label-micro px-2 pb-2.5">关卡</th>
          <th class="label-micro px-2 pb-2.5">详情</th>
        </tr>
      </thead>
      <tbody>
        <tr
          v-for="r in props.records"
          :key="r.intentId"
          class="border-t border-line transition-colors hover:bg-panel-2"
        >
          <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ clockTime(r.tsMs) }}</td>
          <td class="px-2 py-2.5 font-semibold">{{ r.strategy }}</td>
          <td class="px-2 py-2.5">
            <Badge :variant="statusMeta(r).tone" dot>{{ statusMeta(r).label }}</Badge>
          </td>
          <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ gateOf(r) }}</td>
          <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ detailOf(r) }}</td>
        </tr>
      </tbody>
    </table>
  </div>
  <EmptyState v-else title="还没有裁决记录" description="策略提交建议后，内核的四道关卡裁决会出现在这里。" />
</template>
