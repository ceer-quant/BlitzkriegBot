<script setup lang="ts">
/**
 * 裁决流（E25 / #331，§13.4）— the arbitration audit as the operator sees it:
 * every suggestion the strategies made, and what the KERNEL decided. The row
 * shape is fixed by the design (时间/策略/结果/关卡/详情) and the detail column
 * is the kernel's own words — this page never re-writes a rejection.
 */
import { computed, onMounted, ref } from 'vue'
import { useIntervalFn } from '@vueuse/core'
import { Gavel } from 'lucide-vue-next'
import { api, type IntentAuditRecordView } from '@/api/client'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import SegmentedControl from '@/components/ui/segmented/SegmentedControl.vue'
import DecisionTable from '@/components/DecisionTable.vue'

type FilterId = 'all' | 'approved' | 'modified' | 'rejected'

const records = ref<IntentAuditRecordView[]>([])
const error = ref('')
const filter = ref<FilterId>('all')

const segments: { id: FilterId; label: string }[] = [
  { id: 'all', label: '全部' },
  { id: 'approved', label: '已通过' },
  { id: 'modified', label: '已修改' },
  { id: 'rejected', label: '已拒绝' },
]

async function refresh() {
  try {
    const doc = await api.intentAuditTail({ limit: 200 })
    if (doc.error) {
      error.value = doc.error
      return
    }
    error.value = ''
    records.value = doc.records ?? []
  } catch (e) {
    error.value = String((e as Error)?.message ?? e)
  }
}

/** Server-side vocabulary is uppercase; the filter passes it straight through. */
const filtered = computed(() =>
  filter.value === 'all'
    ? records.value
    : records.value.filter((r) => r.decision.status === filter.value.toUpperCase()),
)

onMounted(refresh)
useIntervalFn(refresh, 4000)
</script>

<template>
  <div class="rise-in">
    <Card dense>
      <CardHeader class="flex-row items-center justify-between">
        <span class="inline-flex items-center gap-2 font-semibold">
          <Gavel class="size-4 text-primary/70" />
          裁决流
          <span class="text-[11.5px] font-normal text-faint-fg">
            策略建议，内核裁决 — 每条建议都留痕
          </span>
        </span>
        <SegmentedControl v-model="filter" :segments="segments" />
      </CardHeader>
      <div v-if="error" class="px-3 pb-2 text-[12px] text-down">{{ error }}</div>
      <div class="px-3 pb-3">
        <DecisionTable :records="filtered" />
      </div>
    </Card>
  </div>
</template>
