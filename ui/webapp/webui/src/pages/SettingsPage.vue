<script setup lang="ts">
/**
 * 设置 —— 指令与配置的壳。issue 381 IA 重构：1364 行单文件拆成四个域子页，
 * 域内不再相互依赖，壳只管「当前在哪个域」与各域的懒挂载（lazy 挂载保证
 * 子页的生命周期回调照常触发）。
 *
 *   通用        —— 会话与 token、外观与刷新节奏
 *   网关与网络  —— Gateway 指令台、网络诊断
 *   版本与更新  —— system.version、检查更新、自动更新、暂存下载（issue 379）
 *   风控与策略  —— 生效风控读数、执行策略编辑器（Issue 364）
 *
 * 壳只做展示与指令；凭证与下单原语永不出内核。
 */
import { ref } from 'vue'
import SegmentedControl from '@/components/ui/segmented/SegmentedControl.vue'
import SettingsGeneral from './settings/SettingsGeneral.vue'
import SettingsGateway from './settings/SettingsGateway.vue'
import SettingsVersion from './settings/SettingsVersion.vue'
import SettingsRisk from './settings/SettingsRisk.vue'

type SectionId = 'general' | 'gateway' | 'version' | 'risk'

const section = ref<SectionId>('general')

const sections = [
  { id: 'general', label: '通用' },
  { id: 'gateway', label: '网关与网络' },
  { id: 'version', label: '版本与更新' },
  { id: 'risk', label: '风控与策略' },
]

const sectionPages = {
  general: SettingsGeneral,
  gateway: SettingsGateway,
  version: SettingsVersion,
  risk: SettingsRisk,
} as const
</script>

<template>
  <div class="rise-in">
    <nav class="mb-4">
      <SegmentedControl v-model="section" :segments="sections" />
    </nav>
    <!-- 每次切换重新挂载：域卡的 onMounted 探测 / onUnmounted 停轮询与拆分前
         单页的进页才跑、离页即停语义一致，不做 KeepAlive 缓存，避免不可见的
         后台轮询。 -->
    <component :is="sectionPages[section]" />
  </div>
</template>
