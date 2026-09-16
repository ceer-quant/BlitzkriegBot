/** Theme + alert-sound preferences, persisted in localStorage. */
import { computed, ref } from 'vue'

export type ThemeMode = 'dark' | 'light'

const LS_THEME = 'blitzkrieg-panel-theme'
const LS_SOUND = 'blitzkrieg-panel-sound'

const storedTheme = (localStorage.getItem(LS_THEME) as ThemeMode | null) ?? 'dark'
const theme = ref<ThemeMode>(storedTheme)
const sound = ref(localStorage.getItem(LS_SOUND) !== 'off')

export function applyTheme(mode: ThemeMode): void {
  const root = document.documentElement
  root.classList.toggle('dark', mode === 'dark')
  root.classList.toggle('light', mode === 'light')
}
applyTheme(theme.value)

/** Shared singleton — every caller sees the same refs. */
export function useTheme() {
  const isDark = computed(() => theme.value === 'dark')

  function setTheme(next: ThemeMode): void {
    theme.value = next
    localStorage.setItem(LS_THEME, next)
    applyTheme(next)
  }

  function toggleTheme(): void {
    setTheme(isDark.value ? 'light' : 'dark')
  }

  function setSoundEnabled(on: boolean): void {
    sound.value = on
    localStorage.setItem(LS_SOUND, on ? 'on' : 'off')
  }

  function toggleSound(): void {
    setSoundEnabled(!sound.value)
  }

  return { theme, isDark, sound, soundEnabled: sound, setTheme, toggleTheme, setSoundEnabled, toggleSound }
}
