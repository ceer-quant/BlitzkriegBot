/** Theme + alert-sound preferences, persisted in localStorage. */
import { computed, ref, watch } from 'vue'

/** `system` tracks the OS appearance; `light` / `dark` are explicit overrides. */
export type ThemeMode = 'system' | 'light' | 'dark'
export type ResolvedTheme = 'light' | 'dark'

const LS_THEME = 'blitzkrieg-panel-theme'
const LS_SOUND = 'blitzkrieg-panel-sound'
/** Marker for the one-time move off the old hardcoded-dark default. */
const LS_MIGRATED = 'blitzkrieg-panel-theme-follows-system'

const darkQuery = (): MediaQueryList | null =>
  typeof window !== 'undefined' && typeof window.matchMedia === 'function'
    ? window.matchMedia('(prefers-color-scheme: dark)')
    : null

const systemDark = ref(darkQuery()?.matches ?? true)

function initialTheme(): ThemeMode {
  const stored = localStorage.getItem(LS_THEME)
  const known = stored === 'light' || stored === 'dark' || stored === 'system'
  // `dark` used to be a hardcoded default rather than a choice, so a stored
  // value from that era is not evidence the user wants dark forever — move it
  // to `system` once. A stored `light` could only have come from a click, so
  // it is respected as a deliberate override.
  const legacy = stored === null || stored === 'dark'
  if (!localStorage.getItem(LS_MIGRATED) && legacy) {
    localStorage.setItem(LS_MIGRATED, '1')
    localStorage.setItem(LS_THEME, 'system')
    return 'system'
  }
  localStorage.setItem(LS_MIGRATED, '1')
  return known ? (stored as ThemeMode) : 'system'
}

const theme = ref<ThemeMode>(initialTheme())
const sound = ref(localStorage.getItem(LS_SOUND) !== 'off')

/** What the OS asks for right now, independent of any override. */
const prefersDark = computed(() => systemDark.value)

/** The theme actually painted: an override, else the OS preference. */
const resolved = computed<ResolvedTheme>(() =>
  theme.value === 'system' ? (systemDark.value ? 'dark' : 'light') : theme.value,
)

export function applyTheme(mode: ResolvedTheme): void {
  const root = document.documentElement
  root.classList.toggle('dark', mode === 'dark')
  root.classList.toggle('light', mode === 'light')
  // Native chrome (scrollbars, form controls, autofill) follows `color-scheme`.
  root.style.colorScheme = mode
}
applyTheme(resolved.value)

// Sync, not the default pre-flush: the painted class must always match
// `resolved` within the same tick as the change. An async watcher leaves a
// window where the computed says one theme and the DOM still wears the other.
watch(resolved, (mode) => applyTheme(mode), { flush: 'sync' })

const mq = darkQuery()
if (mq) {
  const onSystemChange = (e: MediaQueryListEvent): void => {
    // Only moves `resolved` when the preference is `system`, so an explicit
    // override stays untouched; the watcher above repaints.
    systemDark.value = e.matches
  }
  // Safari < 14 exposes only the deprecated addListener.
  if (typeof mq.addEventListener === 'function') mq.addEventListener('change', onSystemChange)
  else mq.addListener(onSystemChange)
}

const CYCLE: ThemeMode[] = ['system', 'light', 'dark']

/** Shared singleton — every caller sees the same refs. */
export function useTheme() {
  const isDark = computed(() => resolved.value === 'dark')

  function setTheme(next: ThemeMode): void {
    theme.value = next
    localStorage.setItem(LS_THEME, next)
  }

  /** Cycle 跟随系统 → 浅色 → 深色 → 跟随系统. */
  function cycleTheme(): void {
    const next = CYCLE[(CYCLE.indexOf(theme.value) + 1) % CYCLE.length]
    setTheme(next ?? 'system')
  }

  function setSoundEnabled(on: boolean): void {
    sound.value = on
    localStorage.setItem(LS_SOUND, on ? 'on' : 'off')
  }

  function toggleSound(): void {
    setSoundEnabled(!sound.value)
  }

  return {
    theme, isDark, resolved, prefersDark, sound, soundEnabled: sound,
    setTheme, cycleTheme, setSoundEnabled, toggleSound,
  }
}
