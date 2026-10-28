<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue'
import { cn } from '@/lib/utils'

const props = withDefaults(
  defineProps<{
    tone?: 'error' | 'warn' | 'info'
    title?: string
    hint?: string
    class?: string
    dismissible?: boolean
    /**
     * Auto-dismiss countdown in ms. 0/undefined = persistent (the default —
     * an urgent banner must not fade itself away while the operator reads it).
     * The countdown RESTARTS whenever the content changes (keyed `autoKey`),
     * so a banner that swaps its message does not vanish mid-read; pause on
     * hover keeps a mouse-held banner up.
     */
    autoDismissMs?: number
    /** Content key: a change restarts the auto-dismiss clock. */
    autoKey?: string | number
  }>(),
  { tone: 'error' },
)
const emit = defineEmits<{ dismiss: [] }>()

const TONES = {
  error: 'border-down/30 bg-down/10 text-down',
  warn: 'border-primary/30 bg-primary/10 text-primary',
  info: 'border-info/30 bg-info/10 text-info',
}

// `computed`, not a one-shot lookup: a caller whose tone follows live state
// (`:tone="netView.tone === 'down' ? 'error' : 'info'"`) would otherwise keep
// the class it was born with and paint the wrong severity once the state moved.
const tones = computed(() => TONES[props.tone])

// ── auto-dismiss ─────────────────────────────────────────────────────────
// Off by default: persistence is the safe answer for everything that predates
// this prop, and the cockpit's red banners opt OUT explicitly, never in.
const remaining = ref(0)
let timer: ReturnType<typeof setInterval> | null = null

const stopTimer = () => {
  if (timer) {
    clearInterval(timer)
    timer = null
  }
}

const startTimer = () => {
  stopTimer()
  if (!props.autoDismissMs || props.autoDismissMs <= 0) {
    remaining.value = 0
    return
  }
  remaining.value = props.autoDismissMs
  // 1s tick instead of one setTimeout: the template can show the countdown
  // ("12s"), and hover-pause is just a flag the tick respects.
  timer = setInterval(() => {
    if (hovering.value) return
    remaining.value -= 1000
    if (remaining.value <= 0) {
      stopTimer()
      emit('dismiss')
    }
  }, 1000)
}

const hovering = ref(false)
watch(() => [props.autoDismissMs, props.autoKey], startTimer)
watch(() => props.dismissible, startTimer)
startTimer()
onBeforeUnmount(stopTimer)
</script>

<template>
  <div
    :class="cn('flex items-start gap-2.5 rounded-md border px-3.5 py-2.5 text-[13px]', tones, props.class)"
    role="alert"
    @mouseenter="hovering = true"
    @mouseleave="hovering = false"
  >
    <svg class="mt-[3px] size-4 shrink-0" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
      <circle cx="12" cy="12" r="9" /><path d="M12 8v5M12 16.5v.01" />
    </svg>
    <div class="min-w-0 flex-1">
      <p v-if="props.title" class="font-semibold">{{ props.title }}</p>
      <p class="break-words opacity-90"><slot /></p>
      <!--
        E11: an error must be actionable, not a shrug — `hint` names the move
        (check the process, wait out the restart, reload the file), and it
        renders as its own line so it cannot be read as part of the raw message.
      -->
      <p v-if="props.hint" class="mt-1 break-words text-[11.5px] opacity-75">
        <span class="font-semibold">下一步</span> · {{ props.hint }}
      </p>
    </div>
    <span
      v-if="remaining > 0"
      class="shrink-0 pt-[3px] text-[11px] tabular-nums opacity-60"
    >{{ Math.ceil(remaining / 1000) }}s</span>
    <button
      v-if="props.dismissible"
      class="shrink-0 rounded px-1 text-current/70 transition hover:text-current"
      aria-label="关闭"
      @click="emit('dismiss')"
    >✕</button>
  </div>
</template>
