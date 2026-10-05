<script setup lang="ts">
/**
 * 设置 · 版本与更新 —— 内核自述的版本/构建来源（system.version）+ 更新三态
 * 徽章 + 检查更新 / 自动更新 / 暂存下载（issue 379）。
 * 三态读法只有一份（lib/version.ts）：null = 未检查，绝不画成「已是最新」。
 */
import { computed, onUnmounted, ref } from 'vue'
import { RefreshCw, Tag } from 'lucide-vue-next'
import { api, type StageState } from '@/api/client'
import { revisionText, versionBadge } from '@/lib/version'
import { usePanelStore } from '@/stores/panel'
import { dateTime } from '@/lib/format'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Switch from '@/components/ui/switch/Switch.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'

const store = usePanelStore()

// ── 版本与更新 ──────────────────────────────────────────────────────────────
// 数据来自 snapshot 轮询里的 systemVersion（内核自述，非本地猜测）。
// 三态读法只有一份（lib/version.ts）：null = 未检查，绝不画成「已是最新」。
const version = computed(() => store.snapshot?.systemVersion ?? null)
const versionBadgeView = computed(() => versionBadge(version.value))
const versionErr = ref<string | null>(null)
const checkBusy = ref(false)

/**
 * 「检查更新」：网关只是转交（内核拨 GitHub 以秒计，网关绝不等它），所以这里
 * 轮询 snapshot 直到内核的 lastCheckMs 动了；内核明确拒绝（checkEnabled=false）
 * 时 error 原样展示 —— 关闭就是关闭，不静默、不偷跑。
 */
let checkTimer: ReturnType<typeof setTimeout> | null = null
const checkDeadlineAt = ref(0)

async function checkNow(): Promise<void> {
  if (!version.value || checkBusy.value) return
  const before = version.value.lastCheckMs
  try {
    const res = await api.updateCheck()
    if (res.error) {
      versionErr.value = res.error
      return
    }
  } catch (e) {
    versionErr.value = e instanceof Error ? e.message : String(e)
    return
  }
  checkBusy.value = true
  checkDeadlineAt.value = Date.now() + 45_000
  const poll = async (): Promise<void> => {
    await store.refresh()
    const now = store.snapshot?.systemVersion
    const moved = now && now.lastCheckMs !== before && now.lastCheckMs !== null
    if (moved || Date.now() > checkDeadlineAt.value) {
      checkBusy.value = false
      return
    }
    checkTimer = setTimeout(() => void poll(), 1500)
  }
  void poll()
}

/** 「自动更新」开关：状态来源是内核；写盘失败必须报错（不静默回退）。 */
async function toggleAutoUpdate(on: boolean): Promise<void> {
  try {
    const res = await api.updateConfigure(on)
    if (res.ok === false && res.error) {
      versionErr.value = res.error
    }
  } catch (e) {
    versionErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    await store.refresh()
  }
}

// 暂存状态（issue 379）同样来自 snapshot 轮询：旧内核是 null，明说「不支持暂存」。
const stage = computed(() => store.snapshot?.stageState ?? null)
const stageBusy = ref(false)
const stageErr = ref<string | null>(null)

/** 暂存相位的徽章视图：idle/downloading/staged/failed 各自可分辨，绝不塌缩。 */
const stageBadgeView = computed(() => {
  const ph = (stage.value as StageState | null)?.phase
  if (ph === 'staged') return { text: '已暂存，重启可应用', variant: 'up' as const }
  if (ph === 'downloading') return { text: '下载中…', variant: 'default' as const }
  if (ph === 'failed') return { text: '暂存失败', variant: 'down' as const }
  return null
})

/**
 * 「暂存下载」（§7.5）：内核把较新发布下载到暂存目录并校验 SHA256，然后
 * 停在那里 —— 替换二进制是启动器的事，内核永不碰自己正在执行的文件。网关
 * 只转交，这里轮询 snapshot 的 stageState 直到 phase 离开 downloading；
 * 内核拒绝（autoUpdate=false）时 error 原样展示。
 */
let stageTimer: ReturnType<typeof setTimeout> | null = null
const stageDeadlineAt = ref(0)

async function stageNow(): Promise<void> {
  if (stageBusy.value) return
  try {
    const res = await api.updateStage()
    if (res.error) {
      stageErr.value = res.error
      return
    }
  } catch (e) {
    stageErr.value = e instanceof Error ? e.message : String(e)
    return
  }
  stageBusy.value = true
  stageDeadlineAt.value = Date.now() + 150_000
  const poll = async (): Promise<void> => {
    await store.refresh()
    const now = store.snapshot?.stageState
    const settled = now && now.phase !== 'downloading'
    if (settled || Date.now() > stageDeadlineAt.value) {
      stageBusy.value = false
      return
    }
    stageTimer = setTimeout(() => void poll(), 1500)
  }
  void poll()
}

onUnmounted(() => {
  if (checkTimer !== null) clearTimeout(checkTimer)
  if (stageTimer !== null) clearTimeout(stageTimer)
})
</script>

<template>
  <div>
    <!-- ── 版本与更新 ──────────────────────────────────────────────────── -->
    <Card>
      <CardHeader label="版本与更新">
        <template #title>
          <Tag class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <Badge v-if="versionBadgeView" :variant="versionBadgeView.variant" dot>{{ versionBadgeView.text }}</Badge>
          <Badge v-else variant="default" dot>不可用</Badge>
        </template>
      </CardHeader>

      <p class="text-[11.5px] leading-snug text-muted-fg">
        版本是<strong>正在运行</strong>的内核自述的（system.version），与磁盘上安装的是哪一版不是一回事 ——
        分别可问：<code>blitzkrieg version</code>（磁盘）/ <code>blitzkrieg version --core</code>（运行中）。
      </p>

      <!--
        旧内核不认识 system.version：说「不认识」，不画一个空版本号（§5.6）。
        这条与 net.check 的「空报告不算通过」是同一条诚实规则。
      -->
      <AlertBanner v-if="!version && !versionErr" class="mt-3" tone="info">正在读取内核版本…</AlertBanner>
      <AlertBanner v-else-if="!version" class="mt-3" tone="warn">
        这个内核不认识 system.version（{{ versionErr ?? '内核离线或版本过旧' }}）。版本信息不可用。
      </AlertBanner>

      <template v-else>
        <div class="mt-3 grid gap-x-6 gap-y-2 text-[12px] sm:grid-cols-2">
          <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
            <span class="text-faint-fg">版本</span>
            <span class="font-mono">{{ version.version }}</span>
          </div>
          <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
            <span class="text-faint-fg">修订号</span>
            <span class="font-mono">
              {{ revisionText(version.gitHash) }}
              <span v-if="version.gitDirty" class="text-primary">dirty</span>
              <span v-else class="text-faint-fg">clean</span>
            </span>
          </div>
          <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
            <span class="text-faint-fg">构建时间</span>
            <span class="font-mono">{{ version.buildDate }}</span>
          </div>
          <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
            <span class="text-faint-fg">目标平台</span>
            <span class="font-mono">{{ version.target }}</span>
          </div>
          <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
            <span class="text-faint-fg">上次检查</span>
            <span class="num">{{ version.lastCheckMs ? dateTime(version.lastCheckMs) : '—' }}</span>
          </div>
          <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
            <span class="text-faint-fg">出网检查</span>
            <Badge :variant="version.checkEnabled ? 'up' : 'default'">
              {{ version.checkEnabled ? '已允许' : '已关闭' }}
            </Badge>
          </div>
        </div>

        <div class="mt-3 flex flex-wrap items-center gap-2">
          <Button
            variant="outline"
            size="sm"
            :disabled="checkBusy || !version.checkEnabled"
            :title="version.checkEnabled
              ? '让内核询问一次发布源（后台执行，结果自动刷新）'
              : '出网检查已在配置中关闭；开启后此按钮才可用（user_layer/configs/update.toml 或下方开关说明）'"
            @click="checkNow"
          >
            <RefreshCw class="size-3.5" />{{ checkBusy ? '检查中…' : '检查更新' }}
          </Button>
          <Tooltip v-if="!version.checkEnabled" content="INV-3：检查关闭时内核一个包都不发 —— 这不是故障，是默认承诺。">
            <span class="text-[11px] text-faint-fg">检查已关闭</span>
          </Tooltip>

          <Button
            variant="outline"
            size="sm"
            :disabled="stageBusy || !version.autoUpdate || version.updateAvailable !== true"
            :title="!version.autoUpdate
              ? '自动更新已关闭：开启后内核才可下载并校验发布资产到暂存目录（§7.5）'
              : version.updateAvailable !== true
                ? '先检查出更新，才有东西可暂存'
                : '下载发布资产到 data/update/staging/ 并校验 SHA256；替换二进制由启动器在重启路径上完成'"
            @click="stageNow"
          >
            <RefreshCw class="size-3.5" />{{ stageBusy ? '暂存中…' : '暂存下载' }}
          </Button>

          <div class="ml-auto flex items-center gap-2">
            <span class="text-[11px] text-faint-fg">自动更新（默认关闭；开启后安装由启动器校验执行，需重启内核生效）</span>
            <Switch :model-value="version.autoUpdate" @update:model-value="toggleAutoUpdate" />
          </div>
        </div>

        <!-- issue 379：暂存状态行：phase 是内核自己的词，四相各自可分辨 -->
        <div v-if="stage" class="mt-2 flex items-center justify-between gap-3 border-b border-line pb-1.5 text-[12px]">
          <span class="text-faint-fg">暂存（data/update/staging/）</span>
          <span class="flex items-center gap-2">
            <Badge v-if="stageBadgeView" :variant="stageBadgeView.variant" dot>{{ stageBadgeView.text }}</Badge>
            <Badge v-else variant="default" dot>未暂存</Badge>
            <span v-if="stage.version" class="font-mono text-[11px] text-muted-fg">v{{ stage.version }}</span>
            <span v-if="stage.stagedAtMs" class="num text-[11px] text-faint-fg">{{ dateTime(stage.stagedAtMs) }}</span>
          </span>
        </div>
        <p v-if="stage?.detail" class="mt-1 text-[11px] leading-snug text-muted-fg">{{ stage.detail }}</p>
        <p v-if="stageErr" class="mt-1 text-[11px] leading-snug text-down">{{ stageErr }}</p>
        <p v-if="versionErr" class="mt-2 text-[11px] leading-snug text-down">{{ versionErr }}</p>
      </template>
    </Card>
  </div>
</template>
