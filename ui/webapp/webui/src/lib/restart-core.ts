/**
 * 重启内核的共享流程（issue 488）。
 *
 * next_session 类设置（行情插件切换、轮次周期）写入持久格后要重启才生效。
 * 面板的约定：写入成功 → 10 秒倒计时 → 自动下发网关 `restart` 动词。倒计时
 * 给操作员一个明确的「还有一次反悔机会」的窗口，也避免刚写完格就立刻断开
 * IPC 把回执弄丢；10 秒同时远大于 stop 的 TERM_GRACE(5s)，让上一次断开彻底
 * 落定。
 *
 * 挂在组件级：离开页面即 `cancel()`（onUnmounted 由调用方负责或直接随组件
 * 生命周期），倒计时是每个卡片自己的 ref，不进全局 store —— 两个卡片同时
 * 倒计时的场景没有意义（重启是同一个内核），但也不需要互斥：晚到的那个
 * restart 会落到新内核上再 adopted/重启一次，无害。
 */
import { computed, onUnmounted, ref } from 'vue'
import { api } from '@/api/client'

export const RESTART_DELAY_SEC = 10

export function useRestartCore() {
  /** 剩余秒数；null = 没有在倒计时。 */
  const countdown = ref<number | null>(null)
  const restarting = ref(false)
  const restartErr = ref<string | null>(null)
  const restartMsg = ref<string | null>(null)

  let timer: ReturnType<typeof setInterval> | null = null

  const counting = computed(() => countdown.value !== null)

  function stopTimer(): void {
    if (timer !== null) {
      clearInterval(timer)
      timer = null
    }
  }

  /** 自动倒计时入口：写入成功后调用。10s 后自动下发 restart。 */
  function schedule(): void {
    stopTimer()
    restartErr.value = null
    restartMsg.value = null
    countdown.value = RESTART_DELAY_SEC
    timer = setInterval(() => {
      if (countdown.value === null) {
        stopTimer()
        return
      }
      countdown.value -= 1
      if (countdown.value <= 0) {
        stopTimer()
        countdown.value = null
        void fire()
      }
    }, 1000)
  }

  /** 操作员点掉了倒计时（「现在重启」或「取消」之外的路径不存在）。 */
  function cancel(): void {
    stopTimer()
    countdown.value = null
    restartMsg.value = '已取消自动重启 —— 设置已保存，下次手动重启内核时生效。'
  }

  async function fire(): Promise<void> {
    if (restarting.value) return
    restarting.value = true
    restartErr.value = null
    try {
      const res = await api.command('restart')
      if (res.ok) {
        restartMsg.value = res.message ?? '内核重启中…面板会在内核重新上线后自动恢复读数。'
      } else {
        restartErr.value = res.message ?? 'restart 被拒绝（网关未开 --manage 或内核非本网关启动）。请手动 `blitzkrieg stop` 后重启。'
      }
    } catch (e) {
      // restart 会断开 IPC：连接错误本身不等于失败 —— 内核可能已在重启。
      restartErr.value = `重启命令已下发但回执不可读（内核断连中，属正常）：${e instanceof Error ? e.message : String(e)}`
    } finally {
      restarting.value = false
    }
  }

  /** 立即重启（跳过剩余倒计时）。 */
  function now(): void {
    stopTimer()
    countdown.value = null
    void fire()
  }

  onUnmounted(stopTimer)

  return { countdown, counting, restarting, restartErr, restartMsg, schedule, cancel, now }
}
