<script setup lang="ts">
/**
 * 生效风控 · 执行策略规则卡（Issue 364；issue 393 ① 起只承载规则这一半）。
 *
 * 基础参数（budget 三元组/最低权益/持仓笔数）移到上方「生效风控」卡就地编辑
 * —— `execution_policy.set` 写的是整个账户段，参数与规则共用编排壳
 * （SettingsRisk）持有的一份缓冲、一个保存动作，两半不可能互相覆盖。
 * 规则列表/构建器/预览/保存条在 PolicyRulesEditor，字段词表在
 * lib/policy-fields.ts。
 */
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import { ShieldCheck } from 'lucide-vue-next'

defineProps<{ version: number; msg?: string | null }>()

defineEmits<{ dismiss: [] }>()
</script>

<template>
  <Card>
    <CardHeader label="生效风控 · 执行策略规则">
      <template #title>
        <ShieldCheck class="size-4 text-faint-fg" />
      </template>
      <template #action>
        <Badge :variant="version > 0 ? 'gold' : 'default'" dot>
          v{{ version }}
        </Badge>
      </template>
    </CardHeader>

    <p class="text-[11.5px] leading-snug text-muted-fg">
      下单前内核按「基础参数（上方卡）→ 规则（priority 升序，取第一个命中）」裁决每一单：
      放行 / 跳过 / 冷静期。生效视图由内核折叠后下发，浏览器不读配置文件；
      保存后内核验证→落盘→重读→落审计才应答，页面渲染的永远是内核确认过的状态。
    </p>

    <!-- issue 393 (①): 规则编辑器由编排壳填入（缓冲与保存在 SettingsRisk）。 -->
    <slot name="rules" />
    <AlertBanner v-if="msg" tone="info" class="mt-2.5" dismissible @dismiss="$emit('dismiss')">
      {{ msg }}
    </AlertBanner>
  </Card>
</template>
