<script setup lang="ts">
import { computed } from 'vue'
import { usePanelStore } from '../stores/panel'
import { marketTypeLabel, type PluginRow } from '../api/client'

const store = usePanelStore()

const sections = computed(() => {
  const p = store.plugins
  if (!p) return []
  return [
    { title: '策略插件', rows: p.strategies, active: null as string | null },
    { title: '扩展插件', rows: p.extensions, active: null },
    { title: '行情插件', rows: p.marketPlugins, active: p.marketActive },
  ].filter((s) => s.rows.length > 0)
})

/** 身份注明：二元预测市场 / 现货市场 / 合约市场 / 期货实现，未知类型原样展示。 */
function identity(r: PluginRow): string {
  const kval = r.type ?? r.kind
  const label = marketTypeLabel(kval)
  return label || kval || '—'
}

function badge(r: PluginRow): { cls: string; label: string } {
  if (r.status === 'error') return { cls: 'err', label: '错误' }
  if (r.enabled === false) return { cls: 'off', label: '停用' }
  return { cls: 'on', label: '正常' }
}
</script>

<template>
  <div v-if="store.plugins">
    <div v-for="s in sections" :key="s.title" class="glass card">
      <h2 class="card-title">{{ s.title }}（{{ s.rows.length }}）</h2>
      <table>
        <thead>
          <tr><th>名称</th><th>类型</th><th>状态</th><th>说明</th></tr>
        </thead>
        <tbody>
          <tr v-for="r in s.rows" :key="`${s.title}-${r.name}`">
            <td style="font-weight: 600">
              {{ r.name }}
              <span v-if="s.active === r.name" class="badge warn" style="margin-left: 6px">活跃</span>
            </td>
            <td class="sub">{{ identity(r) }}</td>
            <td><span class="badge" :class="badge(r).cls">{{ badge(r).label }}</span></td>
            <td class="sub">{{ r.description ?? '' }}</td>
          </tr>
        </tbody>
      </table>
    </div>
    <div v-if="store.plugins.lastError" class="error-banner">{{ store.plugins.lastError }}</div>
    <div v-if="!sections.length" class="glass card empty">没有已注册的插件</div>
  </div>
  <div v-else class="glass card empty">{{ store.loading ? '加载中…' : '暂无插件数据' }}</div>
</template>
