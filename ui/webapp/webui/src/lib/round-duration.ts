/**
 * 轮次时间周期切换的共享状态（next_session 语义，issue 474）。
 *
 * 原住民是设置 · 通用 的「轮次时间周期」卡；2026-10-24 操作者指令把它
 * 带到行情页倒计时卡 —— 操作者在盯盘时要切周期，跑去设置页断了监视语境。
 * 两处消费同一份读数与同一条写路径（round_duration.get / round_duration.set，
 * 写入成功后 10s 自动重启内核），抽成 composable 让两个卡面共享一份逻辑、
 * 各自一份 restart 倒计时（restart-core 的组件级约定原样保留）。
 *
 * 语义不变（轮次网格是坐标系）：切换写入持久格、本会话网格不动、重启后
 * 生效；cli_or_env 显式参数在场时写入仍会保存但不会赢 —— 卡面照旧提示。
 */
import { computed, onMounted, ref } from 'vue'
import { api } from '@/api/client'
import type { RoundDurationDoc } from '@/api/client'
import { useRestartCore } from '@/lib/restart-core'

export function useRoundDuration() {
  const doc = ref<RoundDurationDoc | null>(null)
  const err = ref<string | null>(null)
  const msg = ref<string | null>(null)
  const busy = ref(false)
  const pending = ref<number | null>(null)
  const restart = useRestartCore()

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
    // 预填「下次会话将跑的值」（持久格优先），不是本会话生效值 —— 卡片承诺
    // 的是「重启后是什么」。
    get: () => String(pending.value ?? doc.value?.nextSessionSec ?? ''),
    set: (v) => { pending.value = Number(v) },
  })
  const dirty = computed(
    () => pending.value !== null && pending.value !== doc.value?.nextSessionSec,
  )
  const flagLocked = computed(() => doc.value?.source === 'cli_or_env')
  const sourceLabel = computed(() => {
    switch (doc.value?.source) {
      case 'cli_or_env': return '命令行/环境变量'
      case 'runtime': return '面板设置'
      case 'toml': return '配置文件'
      case 'env': return '环境变量'
      default: return '出厂默认'
    }
  })

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

  async function save(): Promise<void> {
    if (pending.value === null) return
    busy.value = true
    err.value = null
    msg.value = null
    try {
      const res = await api.roundDurationSet({
        durationSec: pending.value,
        reason: 'webui 面板编辑（轮次时间周期）',
      })
      if (res.error) {
        err.value = res.error
      } else {
        msg.value = `已保存：重启后按 ${res.nextSessionSec}s 轮次运行；本会话网格不变。`
        await load()
        restart.schedule()
      }
    } catch (e) {
      err.value = e instanceof Error ? e.message : String(e)
    } finally {
      busy.value = false
    }
  }

  /** 清除面板设置（回落启动参数解析）——设置卡独有的退化路径，同一写单源。 */
  async function reset(): Promise<void> {
    busy.value = true
    err.value = null
    msg.value = null
    try {
      const res = await api.roundDurationSet({
        reset: true,
        reason: 'webui 面板清除（轮次时间周期卡）',
      })
      if (res.error) {
        err.value = res.error
      } else {
        msg.value = `已清除面板设置：重启后回落到启动参数解析（命令行 > 环境变量 > 配置文件 > 出厂默认）。`
        await load()
        restart.schedule()
      }
    } catch (e) {
      err.value = e instanceof Error ? e.message : String(e)
    } finally {
      busy.value = false
    }
  }

  onMounted(load)

  return { doc, err, msg, busy, pending, segments, value, dirty, flagLocked, sourceLabel, load, save, reset, restart }
}
