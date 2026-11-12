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
    // 预填「本会话生效值」（即时生效），持久格/下次会话值不再预填 ——
    // 卡片承诺的是「现在就是这个」。
    get: () => String(pending.value ?? doc.value?.effectiveSec ?? ''),
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

  // ── 配置真相 vs 执行真相（issue 1139）──────────────────────────────
  // wire 的 effective_sec 等于启动解析出的默认网格 = 配置真相（source 字段如实
  // 说明来源：cli/env/runtime/toml/default）。default 账户的执行真相取 per-account
  // 覆盖（round_duration.set 立即写入 .env + 内存，立即生效），缺省回落到
  // 配置网格。二者分裂时卡面必须红条告警，不静默。
  const configSec = computed(() => doc.value?.effectiveSec ?? null)
  const execSec = computed(() => {
    const pa = doc.value?.perAccount?.['default']
    return pa !== undefined ? pa : doc.value?.effectiveSec ?? null
  })
  const split = computed(() =>
    configSec.value !== null && execSec.value !== null && configSec.value !== execSec.value,
  )

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

  async function save(accountId: string = 'default'): Promise<void> {
    if (pending.value === null) return
    busy.value = true
    err.value = null
    msg.value = null
    try {
      const res = await api.roundDurationSet({
        durationSec: pending.value,
        accountId,
        reason: 'webui 面板编辑（轮次时间周期）',
      })
      if (res.error) {
        err.value = res.error
      } else {
        msg.value = `已生效：本会话按 ${res.nextSessionSec}s 轮次运行；在途轮保持原网格结算。`
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
        msg.value = `已生效：面板设置已清除，本会话回落到启动参数解析；在途轮不变。`
        await load()
      }
    } catch (e) {
      err.value = e instanceof Error ? e.message : String(e)
    } finally {
      busy.value = false
    }
  }

  onMounted(load)

  return { doc, err, msg, busy, pending, segments, value, dirty, flagLocked, sourceLabel, configSec, execSec, split, roundLabel, load, save, reset }
}
