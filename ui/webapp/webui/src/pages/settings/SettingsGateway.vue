<script setup lang="ts">
/**
 * 设置 · 网关与网络 —— Gateway 指令台（网关身份 + status/start/stop，沿用
 * lifecycle 的能力闸）与网络诊断（内核拨测出网路径，读法在 lib/net-check.ts）。
 */
import { computed, onMounted, onUnmounted, ref } from 'vue'
import {
  RefreshCw, ShieldCheck, TerminalSquare, Play, Square, Info, Server, Network,
} from 'lucide-vue-next'
import { api, type CommandDoc, type NetCheckDoc } from '@/api/client'
import { readNetCheck } from '@/lib/net-check'
import { usePanelStore } from '@/stores/panel'
import { controlState, exitNotice } from '@/lib/lifecycle'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'

const store = usePanelStore()

// ── gateway console ─────────────────────────────────────────────────────────
const gateway = computed(() => store.snapshot?.gateway ?? null)
const engineUp = computed(() => store.snapshot?.connected ?? false)
/** Reuse the lifecycle rules verbatim — the console must not invent its own. */
const control = computed(() => controlState(gateway.value, engineUp.value))
const exit = computed(() => exitNotice(gateway.value, engineUp.value))

const busy = ref(false)
const cmdMsg = ref<string | null>(null)
async function send(cmd: 'start' | 'stop' | 'status'): Promise<void> {
  busy.value = true
  cmdMsg.value = null
  try {
    const res: CommandDoc = await api.command(cmd)
    cmdMsg.value = res.message ?? `${cmd} 已下发`
    await store.refresh()
  } catch (e) {
    cmdMsg.value = e instanceof Error ? e.message : String(e)
  } finally {
    busy.value = false
    setTimeout(() => { cmdMsg.value = null }, 6000)
  }
}

// ── 网络诊断 ────────────────────────────────────────────────────────────────
const netDoc = ref<NetCheckDoc | null>(null)
/** 读法（标签、计数、空/探测中/失败三态）全部来自 lib/net-check.ts。 */
const netView = computed(() => readNetCheck(netDoc.value))
const netErr = ref<string | null>(null)
const netBusy = ref(false)
/** 探测在网关侧后台跑；这里只轮询缓存，直到 `probing` 落下。 */
let netTimer: ReturnType<typeof setTimeout> | null = null

function scheduleNetPoll(): void {
  if (netTimer !== null) {
    clearTimeout(netTimer)
    netTimer = null
  }
  if (netDoc.value?.probing) {
    netTimer = setTimeout(() => void loadNet(), 1200)
  }
}

async function loadNet(): Promise<void> {
  try {
    netDoc.value = await api.netCheck()
    // 「面板调不到网关」与「内核没应答探测」是两件事，分开报：前者是这一行，
    // 后者是 netView.hint（网关把内核的错误原样带在 error 里）。
    netErr.value = null
  } catch (e) {
    netErr.value = e instanceof Error ? e.message : String(e)
  }
  scheduleNetPoll()
}

async function probeNet(): Promise<void> {
  netBusy.value = true
  try {
    netDoc.value = await api.probeNetCheck()
    netErr.value = null
  } catch (e) {
    netErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    netBusy.value = false
    scheduleNetPoll()
  }
}

onMounted(() => void loadNet())
onUnmounted(() => {
  if (netTimer !== null) clearTimeout(netTimer)
})

const netBadge = computed(() => {
  switch (netView.value.tone) {
    case 'up': return { text: '全部通过', variant: 'up' as const }
    case 'down': return { text: '有失败', variant: 'down' as const }
    default: return { text: '探测中', variant: 'default' as const }
  }
})
</script>

<template>
  <div>
    <!-- ── Gateway 指令台 ──────────────────────────────────────────────── -->
    <Card>
      <CardHeader label="Gateway 指令台">
        <template #title>
          <TerminalSquare class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <Badge :variant="engineUp ? 'up' : 'down'" dot>{{ engineUp ? '内核在线' : '内核离线' }}</Badge>
        </template>
      </CardHeader>

      <div v-if="gateway" class="grid gap-x-6 gap-y-2 text-[12px] sm:grid-cols-2">
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">socket</span>
          <span class="truncate text-right num" :title="gateway.socket">{{ gateway.socket }}</span>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">进程控制（--manage）</span>
          <Badge :variant="gateway.lifecycleEnabled ? 'up' : 'default'">
            {{ gateway.lifecycleEnabled ? '已开启' : '未开启（只读）' }}
          </Badge>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">内核归属</span>
          <span>{{ gateway.managed ? '本网关启动（可停止）' : '外部进程（只接管读取）' }}</span>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">内核 PID</span>
          <span class="num">
            <template v-if="gateway.corePid != null"><RollingNumber :value="gateway.corePid" /></template>
            <template v-else>—</template>
          </span>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">崩溃自动重启次数</span>
          <span class="num"><RollingNumber :value="gateway.restarts ?? 0" /></span>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">已放弃重启</span>
          <Badge :variant="gateway.restartGivenUp ? 'down' : 'default'">{{ gateway.restartGivenUp ? '是' : '否' }}</Badge>
        </div>
      </div>
      <p v-else class="flex items-center gap-2 text-[12px] text-faint-fg">
        <Server class="size-3.5" />当前网关未上报进程控制信息（旧版网关或只读适配器），指令台不可用。
      </p>

      <!--
        A crash is an alert, so it wears the kit's alert tint rather than a
        page-local copy of it (issue 260): the hand-rolled `border-down/35 bg-down/8`
        was the same semantics at a second concentration, which is how the panel
        ended up with two reds.
      -->
      <AlertBanner
        v-if="exit"
        class="mt-3"
        :tone="exit.kind === 'crash' ? 'error' : 'info'"
      >{{ exit.kind === 'crash' ? `内核曾崩溃：${exit.description}` : `内核已停止：${exit.description}` }}</AlertBanner>

      <div class="mt-3 flex flex-wrap items-center gap-2">
        <Button variant="default" size="sm" :disabled="busy" title="查询网关与内核状态" @click="send('status')">
          <RefreshCw class="size-3.5" />状态查询
        </Button>
        <Button
          variant="up"
          size="sm"
          :disabled="busy || !control.canStart"
          :title="control.canStart ? '启动内核' : (control.blockedReason ?? '内核已在运行')"
          @click="send('start')"
        >
          <Play class="size-3.5" />启动内核
        </Button>
        <Button
          variant="danger"
          size="sm"
          :disabled="busy || !control.canStop"
          :title="control.canStop ? '停止本网关启动的内核' : (control.blockedReason ?? '内核未运行')"
          @click="send('stop')"
        >
          <Square class="size-3.5" />停止内核
        </Button>
        <span class="ml-auto text-[11px] text-faint-fg">指令经 /api/command 下发，token 鉴权；本面板不下发任何交易参数</span>
      </div>

      <div v-if="!control.usable && control.blockedReason" class="mt-3 flex items-start gap-2 rounded-md border border-line bg-panel-2 px-3 py-2 text-[11.5px] leading-snug text-muted-fg">
        <Info class="mt-px size-3.5 shrink-0 text-faint-fg" />
        <span>{{ control.blockedReason }}</span>
      </div>
      <div v-if="cmdMsg" class="mt-2.5 rounded-md border border-line bg-panel-2 px-3 py-2 text-[12px] text-muted-fg">
        {{ cmdMsg }}
      </div>
    </Card>

    <!-- ── 网络诊断 ────────────────────────────────────────────────────── -->
    <Card class="mt-3.5">
      <CardHeader label="网络诊断">
        <template #title>
          <Network class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <Badge :variant="netBadge.variant" dot>{{ netBadge.text }}</Badge>
        </template>
      </CardHeader>

      <p class="text-[11.5px] leading-snug text-muted-fg">
        内核拨测它自己出网用到的每条路径（解析 → TCP → TLS → 一次廉价请求）。
        读数是内核进程本身看到的网络，不是浏览器这一侧 —— 这正是「是网络还是交易所」的分界。
      </p>

      <div v-if="netView.rows.length" class="mt-3">
        <div
          v-for="(row, i) in netView.rows"
          :key="row.name"
          class="flex flex-wrap items-center gap-x-2 gap-y-1 py-2"
          :class="i > 0 ? 'border-t border-line' : ''"
        >
          <Badge :variant="row.ok ? 'up' : 'down'" dot>{{ row.label }}</Badge>
          <span class="text-[12.5px] font-semibold">{{ row.name }}</span>
          <span class="min-w-0 truncate text-[11.5px] text-faint-fg" :title="row.target">{{ row.target }}</span>
          <span class="ml-auto flex items-center gap-2 text-[11.5px] text-muted-fg">
            <!--
              The TUI paints this fact in its WARN colour, so it may not sit in
              the panel's faintest tier here: "the resolver answered with a
              fake IP" changes how a failure below should be read, and the two
              faces must not disagree about how loud it is (issue 260). `text-primary`
              is the panel's existing warn token — the same one AlertBanner's
              `warn` tone uses.
            -->
            <Tooltip v-if="row.fakeIp" content="解析到的地址全在代理的 fake-IP 段内：这是解析器的事实，不是这张路径的故障。">
              <span class="text-primary">fake-IP</span>
            </Tooltip>
            <span v-if="row.ms != null" class="num">{{ row.ms }} ms</span>
          </span>
          <p v-if="row.detail" class="w-full text-[11px] leading-snug text-faint-fg">{{ row.detail }}</p>
        </div>
      </div>

      <EmptyState
        v-else
        compact
        :loading="netView.tone === 'default'"
        :text="netView.summary"
        :hint="netView.emptyHint ?? undefined"
      />

      <!--
        The verdict strip is an alert, so it is the kit's `AlertBanner` and not a
        page-local tint (issue 260). `info` covers both "all paths OK, here is the
        reading" and "no verdict yet" — the same tone the Plugins page uses for a
        normal state; only a real failure gets the error tint.
      -->
      <AlertBanner
        class="mt-3"
        :tone="netView.tone === 'down' ? 'error' : 'info'"
        :title="netView.title"
        :hint="netView.hint"
      >{{ netView.summary }}</AlertBanner>

      <!-- Same fact, same volume as the TUI's WARN-coloured proxy line (issue 260):
           a proxy in front of the probe is the explanation for half the
           readings below, so it may not be the faintest text on the card. -->
      <p v-if="netView.proxyNote" class="mt-2 text-[11px] leading-snug text-primary">{{ netView.proxyNote }}</p>
      <p v-if="netErr" class="mt-2 text-[11px] leading-snug text-down">面板调用失败：{{ netErr }}</p>

      <div class="mt-3 flex flex-wrap items-center gap-2">
        <Button
          variant="outline"
          size="sm"
          :disabled="netBusy || netView.tone === 'default'"
          title="让内核重新拨测一遍（结果在网关侧缓存 20 秒）"
          @click="probeNet"
        >
          <RefreshCw class="size-3.5" />重新探测
        </Button>
        <span class="text-[11px] text-faint-fg">
          <template v-if="netView.ageNote">结果时间：{{ netView.ageNote }}；探测在网关后台运行，页面自动刷新</template>
          <template v-else>还没有探测结果</template>
        </span>
      </div>
    </Card>
  </div>
</template>
