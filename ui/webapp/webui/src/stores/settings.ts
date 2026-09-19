/**
 * Settings store (E8-d 设置持久化) — the operator's persisted preferences.
 *
 * Backed by VueUse `useStorage`, so every field reads through localStorage and
 * survives reloads. 主题/提示音 keep their own persistence in `lib/theme.ts`
 * (it predates this store and carries a legacy-default migration); this store
 * carries the pacing and interaction preferences the panel grew since.
 */
import { defineStore } from 'pinia'
import { useStorage } from '@vueuse/core'

/** 行情面板 2s 快速轮询开关 — off turns the tab into a watch-only view. */
const LS_FAST_POLL = 'blitzkrieg-panel-fast-poll'

export const useSettingsStore = defineStore('settings', () => {
  const fastPoll = useStorage(LS_FAST_POLL, true)

  function setFastPoll(on: boolean): void {
    fastPoll.value = on
  }

  return { fastPoll, setFastPoll }
})
