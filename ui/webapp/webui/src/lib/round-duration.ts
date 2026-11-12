/**
 * 轮次时间周期切换的共享状态（即时生效，issue 474；#1129/#1141 已取消
 * “下次会话重启生效”的重启链）。
 *
 * 原住民是设置 · 通用的「轮次时间周期」卡；2026-10-24 操作者指令把它
 * 带到行情页倒计时卡 —— 操作者在盯盘时要切周期，跑去设置页断了监视语境。
 * 两处消费同一份读数与同一条写路径（round_duration.get / round_duration.set），
 * 写入立即生效、本会话网格不动、在途轮保持原网格结算；不再有重启倒计时
 * 与“下次会话才生效”的幻觉。cli_or_env 显式参数在场时写入仍会保存但不会
 * 赢 —— 卡面照旧提示。抽成 composable 让两个卡面共享一份逻辑。
 */
import { computed, onMounted, ref } from 'vue'
import { api } from '@/api/client'
import type { RoundDurationDoc } from '@/api/client'

export function useRoundDuration() {
  const doc = ref<RoundDurationDoc | null>(null)
  const err = ref<string | null>(null)
  const msg = ref<string | null>(null)
  const busy = ref(false)
  const pending = ref<number | null>(null)

  const segments = computed(() => {
    const allowed = doc.value?.allowed ?? []
    const seen = new Set<number>()
    const out: { id: string; label: string }[] = []
    for (const o of allowed) {
      if (seen.has(o.sec)) continue
      seen.add(o.sec)
      out.push({ id: String(o.sec), label: o.label ?? `${o.sec}s` })
    }
    return out
  })
  const value = computed<string>({
    // 预填「执行真相」（default 账户 per-account 覆盖，缺省回落 default 网格），
    // 即卡片打开时的当前轮长；持久格/下次会话值不再预填 —— 卡片承诺的是「现在就是这个」。
    get: () => String(pending.value ?? doc.value?.perAccount?.['default'] ?? doc.value?.effectiveSec ?? ''),
    set: (v) => { pending.value = Number(v) },
  })
  const dirty = computed(
    () => pending.value !== null && pending.value !== doc.value?.nextSessionSec,
  )
  const flagLocked = computed(() => doc.value?.source === 'cli_or_env')
  /** 5m/15m/1h/4h/daily，非标档位回落「Xs」。 */
  const roundLabel = (sec: number | null): string => {
    if (sec === null) return '—'
    const labels: Record<number, string> = { 300: '5m', 900: '15m', 3600: '1h', 14400: '4h', 86400: 'daily' }
    return labels[sec] ?? `${sec}s`
  }

  async function load(): Promise<void> {
    busy.value = true
    try {
      doc.value = await api.roundDuration()
      err.value = doc.value.error ?? null
      pending.value = null
    } catch (e) {
      err.value = e instanceof Error ? e.message : String(e)
    } finally {
      busy.value = false
    }
  }

  /** `immediate: true` 时随请求下发立即切换标志；内核会先撤在途单、市价强平仓位，再切网格。 */
  async function save(accountId: string = 'default', { immediate }: { immediate?: boolean } = {}): Promise<void> {
    if (pending.value === null) return
    busy.value = true
    err.value = null
    msg.value = null
    try {
      const res = await api.roundDurationSet({
        durationSec: pending.value,
        accountId,
        immediate,
        reason: 'webui 面板编辑（轮次时间周期）',
      })
      if (res.error) {
        err.value = res.error
      } else {
        msg.value = `下一轮起按 ${res.nextSessionSec}s 运行；当前轮保持出生网格结算。`
        await load()
      }
    } catch (e) {
      err.value = e instanceof Error ? e.message : String(e)
    } finally {
      busy.value = false
    }
  }

  /** 清除面板设置（回落启动参数解析）——设置卡独有的退化路径，同一写单源。 */
  async function reset(accountId: string = 'default'): Promise<void> {
    busy.value = true
    err.value = null
    msg.value = null
    try {
      const res = await api.roundDurationSet({
        reset: true,
        accountId,
        reason: 'webui 面板清除（轮次时间周期卡）',
      })
      if (res.error) {
        err.value = res.error
      } else {
        msg.value = `面板设置已清除，下一轮起回落到启动参数解析；当前轮保持出生网格结算。`
        await load()
      }
    } catch (e) {
      err.value = e instanceof Error ? e.message : String(e)
    } finally {
      busy.value = false
    }
  }

  onMounted(load)

  return { doc, err, msg, busy, pending, segments, value, dirty, flagLocked, roundLabel, load, save, reset }
}
