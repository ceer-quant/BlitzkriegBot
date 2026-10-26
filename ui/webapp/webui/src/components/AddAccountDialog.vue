<script setup lang="ts">
/**
 * 添加账户弹层（#517）—— 绑定即启用的账户录入面。
 *
 * 流程：选交易所（必选）→ 可选显示名 → 填该交易所执行器要求的凭据字段
 * （字段名来自 `market.credentials.status`，内核单一事实源；值只下行、
 * 从不回显）。确认后两步落盘：`account.create` 建身份（accounts.toml），
 * `market.credentials.set` 写凭据格（值进内核进程环境，重启后注入）。
 * 建户成功但凭据被拒时不回滚账户 —— 面板如实报两步各自的结果。
 *
 * 凭据纪律：组件不持有任何凭据字面量；draft 只存在于输入框，
 * 保存后清空。真正的 env 变量名永远来自内核读数，UI 不写第二张表。
 */
import { computed, ref, watch } from 'vue'
import { Plus, ShieldCheck } from 'lucide-vue-next'
import { api, type PluginCredentialRowDoc } from '@/api/client'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import Badge from '@/components/ui/badge/Badge.vue'

const props = defineProps<{
  open: boolean
  /** 已注册的行情插件名（= 可选交易所集合），来自 store.plugins。 */
  venues: string[]
}>()

const emit = defineEmits<{
  'update:open': [v: boolean]
  /** 建户成功（凭据可能部分失败 —— 见 ok 结果文本）。 */
  created: [id: string]
}>()

const venue = ref('')
const name = ref('')
const credDraft = ref<Record<string, string>>({})
const credRow = ref<PluginCredentialRowDoc | null>(null)
const busy = ref(false)
const error = ref<string | null>(null)
const okMsg = ref<string | null>(null)
/** 建户成功后、凭据失败时的账户 id（提示操作员凭据可后补）。 */
const createdId = ref<string | null>(null)

const canSubmit = computed(() => venue.value.trim() !== '' && !busy.value)

watch(
  () => props.open,
  async (v) => {
    if (!v) return
    venue.value = ''
    name.value = ''
    credDraft.value = {}
    credRow.value = null
    error.value = null
    okMsg.value = null
    createdId.value = null
    if (props.venues.length) {
      venue.value = props.venues[0] ?? ''
      await loadRequirements()
    }
  },
)

watch(venue, () => {
  credDraft.value = {}
  void loadRequirements()
})

/** 该交易所执行器要求的 env 变量名（内核读数，名字不是值）。 */
async function loadRequirements(): Promise<void> {
  credRow.value = null
  if (!venue.value) return
  try {
    const doc = await api.marketCredentials()
    credRow.value = doc.rows.find((r) => r.plugin === venue.value) ?? null
  } catch {
    credRow.value = null
  }
}

function close(): void {
  if (!busy.value) emit('update:open', false)
}

async function submit(): Promise<void> {
  if (!canSubmit.value) return
  busy.value = true
  error.value = null
  okMsg.value = null
  createdId.value = null
  const id = venue.value.trim() // 账户 id 直接用交易所名：一个交易所一个账户
  try {
    const created = await api.accountCreate({
      id,
      name: name.value.trim() || undefined,
      venue: venue.value,
      reason: 'panel add-account',
    })
    if (created.error) throw new Error(created.error)
    createdId.value = id

    // 第二步：凭据（只提交有输入的字段）。失败不回滚账户。
    const vars = Object.fromEntries(
      Object.entries(credDraft.value).filter(([, v]) => v.trim() !== ''),
    )
    if (Object.keys(vars).length) {
      const cred = await api.marketCredentialsSet({
        plugin: venue.value,
        vars,
        reason: `panel add-account ${id}`,
      })
      if (cred.error) {
        okMsg.value = `账户 ${id} 已创建（已落盘）；但凭据写入被拒：${cred.error} —— 稍后在设置页指引里补配后重启生效。`
        emit('created', id)
        return
      }
      okMsg.value = `账户 ${id} 已创建，${cred.written?.length ?? 0} 项凭据已保存 —— 重启后注入生效。`
    } else {
      okMsg.value = `账户 ${id} 已创建（未填凭据 —— 下单执行器启动时会缺凭据，稍后可补）。`
    }
    emit('created', id)
    credDraft.value = {}
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      @click.self="close"
    >
      <div class="w-full max-w-md rounded-lg border border-line bg-panel-solid shadow-lg">
        <div class="flex items-center gap-2 border-b border-line px-4 py-3">
          <Plus class="size-4 text-primary" />
          <h3 class="text-[13.5px] font-semibold">添加账户</h3>
        </div>

        <div class="space-y-3 px-4 py-3">
          <!-- 第一步：选交易所（必选）。可选项 = 已注册的行情插件。 -->
          <div>
            <label class="label-micro" for="add-acct-venue">交易所（必选）</label>
            <select
              id="add-acct-venue"
              v-model="venue"
              class="policy-select mt-1"
              :disabled="busy"
            >
              <option v-for="v in venues" :key="v" :value="v">{{ v }}</option>
            </select>
            <p class="mt-1 text-[11px] text-faint-fg">
              一个交易所一个账户；绑定后该账户只能交易所选交易所（绑定即启用）。
            </p>
          </div>

          <!-- 第二步：可选显示名。 -->
          <div>
            <label class="label-micro" for="add-acct-name">显示名（可选）</label>
            <Input
              id="add-acct-name"
              v-model="name"
              class="mt-1"
              placeholder="默认用交易所名"
              :disabled="busy"
              autocomplete="off"
            />
          </div>

          <!-- 第三步：该交易所执行器要求的凭据。字段名来自内核读数；
               值只下行、从不回显 —— 输入框 type=password，保存后清空。 -->
          <div v-if="venue && credRow && credRow.requiredVars.length">
            <div class="flex items-center gap-1.5">
              <ShieldCheck class="size-3.5 text-faint-fg" />
              <span class="label-micro">API 凭据（{{ credRow.resolvedVars.length }}/{{ credRow.requiredVars.length }} 已解析）</span>
            </div>
            <div class="mt-1 space-y-1.5">
              <div v-for="v in credRow.requiredVars" :key="v">
                <label class="flex items-center gap-1 font-mono text-[11px]" :class="credRow.resolvedVars.includes(v) ? 'text-up' : 'text-muted-fg'" :for="`cred-${v}`">
                  {{ credRow.resolvedVars.includes(v) ? '✓' : '○' }} {{ v }}
                </label>
                <Input
                  :id="`cred-${v}`"
                  v-model="credDraft[v]"
                  type="password"
                  class="mt-0.5 font-mono"
                  autocomplete="off"
                  spellcheck="false"
                  :placeholder="credRow.resolvedVars.includes(v) ? '已解析（留空不覆盖）' : '未配置'"
                  :disabled="busy"
                />
              </div>
            </div>
            <p class="mt-1.5 text-[11px] text-faint-fg">
              值写入 .env（唯一持久凭据格，仅属主可读），启动器下次启动时加载；.env 里已手工写的同名变量始终优先。如何申请见
              <span class="text-primary">设置页 → 如何创建 API</span>。
            </p>
          </div>
          <div v-else-if="venue && !credRow" class="text-[11.5px] text-faint-fg">
            该交易所无需凭据（数据面插件）。
          </div>

          <Badge v-if="okMsg" variant="up" dot>已创建 {{ createdId }}</Badge>          <p v-if="okMsg" class="text-[12px] text-up">{{ okMsg }}</p>
          <p v-if="error" class="break-words text-[12px] text-down">{{ error }}</p>
        </div>

        <div class="flex justify-end gap-2 border-t border-line px-4 py-3">
          <Button variant="ghost" size="sm" :disabled="busy" @click="close">取消</Button>
          <Button variant="gold" size="sm" :disabled="!canSubmit" @click="submit">
            {{ busy ? '创建中…' : (okMsg ? '再建一个' : '创建账户') }}
          </Button>
        </div>
      </div>
    </div>
  </Teleport>
</template>
