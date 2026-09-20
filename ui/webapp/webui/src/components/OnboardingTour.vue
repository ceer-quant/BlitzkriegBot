<script setup lang="ts">
/**
 * 三步首次引导 (E11) — a bottom-right glass card that shows exactly one step:
 * the first thing the operator has not done yet. Completion comes from live
 * panel state (see `lib/onboarding.ts`), so the card retires itself the moment
 * both real steps are satisfied, and the retirement is persisted so it never
 * comes back. 跳过 retires it immediately — a pro user's choice, respected.
 */
import { computed, ref, watch } from 'vue'
import { Sparkles } from 'lucide-vue-next'
import Button from '@/components/ui/button/Button.vue'
import { usePanelStore } from '@/stores/panel'
import {
  currentStep, markOnboardDone, onboardDone, onboardSteps, type OnboardStep,
} from '@/lib/onboarding'

const emit = defineEmits<{ navigate: [tab: 'plugins' | 'strategies'] }>()

const store = usePanelStore()
const retired = ref(onboardDone())

const steps = computed(() =>
  onboardSteps({
    activeMarket: store.plugins?.marketPlugins.find((r) => r.active)?.name ?? null,
    anyStrategyEnabled: store.strategyRows.some((r) => r.enabled),
  }),
)
const step = computed<OnboardStep | null>(() => currentStep(steps.value))

/** Data has landed at least once — never flash the tour over a still-loading panel. */
const dataArrived = computed(
  () => store.snapshot !== null || store.plugins !== null || store.strategyRows.length > 0,
)
const show = computed(() => !retired.value && dataArrived.value && step.value !== null)

// Both real steps satisfied → retire and persist without asking.
watch(step, (s) => {
  if (s === null && !retired.value) {
    retired.value = true
    markOnboardDone()
  }
})

function skip(): void {
  retired.value = true
  markOnboardDone()
}

function go(s: OnboardStep): void {
  if (s.target) emit('navigate', s.target)
}
</script>

<template>
  <div v-if="show && step" class="rise-in fixed bottom-4 right-4 z-40 w-[330px] max-w-[calc(100vw-32px)]">
    <div class="glass card-pad relative">
      <div class="flex items-center gap-1.5">
        <Sparkles class="size-3.5 text-primary" />
        <span class="label-micro">首次引导</span>
      </div>
      <p class="mt-2 text-[13.5px] font-semibold leading-snug">{{ step.title }}</p>
      <p class="mt-1 text-[12px] leading-snug text-muted-fg">{{ step.body }}</p>
      <div class="mt-3 flex items-center justify-between">
        <!-- progress over the two real steps; the wrap step needs no dot -->
        <span class="flex items-center gap-1.5">
          <span
            v-for="(s, i) in steps.slice(0, 2)"
            :key="s.id"
            class="size-1.5 rounded-full transition-colors"
            :class="s.done ? 'bg-up' : i === (step?.index ?? 0) ? 'bg-primary' : 'bg-line-strong'"
          />
          <span class="ml-1 text-[11px] text-faint-fg num">{{ step.index + 1 }} / 3</span>
        </span>
        <span class="flex items-center gap-1.5">
          <Button variant="ghost" size="sm" title="跳过引导，不再显示" @click="skip">跳过</Button>
          <Button v-if="step.cta" variant="gold" size="sm" @click="go(step)">{{ step.cta }}</Button>
        </span>
      </div>
    </div>
  </div>
</template>
