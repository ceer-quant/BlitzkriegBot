<script setup lang="ts">
/**
 * 设置 · 风控与策略 —— 生效风控读数卡（E26 §4.4）与执行策略编辑器
 * （Issue 364）的编排壳；两个域各自独立成组件。
 */
import SettingsRiskLimits from './SettingsRiskLimits.vue'
import SettingsExecutionPolicy from './SettingsExecutionPolicy.vue'
</script>

<template>
  <div>
    <SettingsRiskLimits />
    <SettingsExecutionPolicy class="mt-3.5" />
  </div>
</template>
