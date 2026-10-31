/**
 * 引擎控制（启动/停止 + 每次命令的反馈 + 上次退出的说明）—— issue 781：
 * 从 HftPage 的脚本搬出的 domain 逻辑。
 *
 * `engineUp` 仍由页面持有：倒计时块的「引擎上线即重臂」watch 必须先于
 * watch(snap) 建立（前端 watcher 按创建序 flush），状态源留在页面，这里
 * 只消费它。轮次周期切换（useRoundDuration）也留在页面 —— 它与倒计时卡
 * 的档位声明筛共用 activeVenue。
 */
import { computed, ref, type ComputedRef } from 'vue'
import { api, type Snapshot } from '@/api/client'
import { usePanelStore } from '@/stores/panel'
import { controlState, exitNotice } from '@/lib/lifecycle'

export function useEngineControl(
  snap: ComputedRef<Snapshot | null>,
  engineUp: ComputedRef<boolean>,
) {
  const store = usePanelStore()
  const busy = ref(false)
  const cmdMsg = ref<string | null>(null)

  /**
   * What the two controls can actually do here.
   *
   * Reachability alone is not enough to offer a live 停止: a gateway started
   * without `--manage` refuses both verbs, and even with `--manage` it only stops
   * a core it spawned itself (`Supervisor::stop` leaves an adopted core running).
   * `controlState` folds those in so the buttons are disabled with a stated
   * reason instead of being clickable and failing.
   */
  const control = computed(() => controlState(snap.value?.gateway, engineUp.value))

  /**
   * The last core exit, when the operator needs to know about it (E12-c).
   *
   * A crashed core used to be invisible: the gateway cleared its handle and the
   * panel went on showing whatever the socket said, so a core that died and was
   * replaced looked identical to one that never died. The sentence below is the
   * gateway's own classification (`kind`), not something this page infers.
   */
  const exit = computed(() => exitNotice(snap.value?.gateway, engineUp.value))
  const exitText = computed(() => {
    const e = exit.value
    if (!e) return ''
    if (e.kind === 'clean') return `内核已停止：${e.description}`
    if (e.givenUp) {
      return `内核崩溃且已放弃重启（${e.description}）。请检查日志后手动启动。`
    }
    if (engineUp.value && e.restarts > 0) {
      return `内核曾崩溃（${e.description}），已自动重启 ${e.restarts} 次，当前运行中。`
    }
    return `内核崩溃：${e.description}`
  })

  /** Tooltip/title explaining why a control is unavailable (empty when usable). */
  const startHint = computed(() => {
    if (busy.value) return '正在下发命令…'
    if (control.value.canStart) return '启动引擎'
    if (engineUp.value) return '引擎运行中，无需重复启动'
    return control.value.blockedReason ?? '无法启动引擎'
  })
  const stopHint = computed(() => {
    if (busy.value) return '正在下发命令…'
    if (control.value.canStop) return '停止引擎'
    if (!engineUp.value) return '引擎未运行'
    return control.value.blockedReason ?? '无法停止引擎'
  })

  async function send(cmd: 'start' | 'stop'): Promise<void> {
    busy.value = true
    cmdMsg.value = null
    try {
      const res = await api.command(cmd)
      cmdMsg.value = res.message ?? `${cmd} 已下发`
      await store.refresh()
    } catch (e) {
      cmdMsg.value = e instanceof Error ? e.message : String(e)
    } finally {
      busy.value = false
      setTimeout(() => { cmdMsg.value = null }, 4000)
    }
  }

  return { busy, cmdMsg, control, exit, exitText, startHint, stopHint, send }
}
