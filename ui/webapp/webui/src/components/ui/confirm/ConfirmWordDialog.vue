<script setup lang="ts">
/**
 * 确认词弹层 —— 破坏性操作（#458 交易环境重置、#456 强平）的共用门。
 *
 * 自由文本确认门，不是勾选框：必须逐字输入 `word`（如 RESET）按钮才解锁，
 * 误触不可能被归咎于双击。确认后 `confirm` 事件把执行权交回调用方 —— 弹层
 * 不做任何 IO，忙态/错误由调用方经 `busy` / `error` 注入，成功后由调用方
 * `open = false` 收起。
 */
import { computed, ref, watch } from 'vue'
import { TriangleAlert } from 'lucide-vue-next'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'

const props = defineProps<{
  /** v-model:是否打开。 */
  open: boolean
  /** 必须逐字输入的确认词（如 "RESET"）。 */
  word: string
  /** 弹层标题（如「交易环境重置」）。 */
  title: string
  /** 危险说明：会删什么、留什么。逐行渲染。 */
  lines: string[]
  /** 执行按钮文案（如「确认重置」）。 */
  actionLabel: string
  /** 调用方执行中 —— 按钮禁用并转圈。 */
  busy?: boolean
  /** 调用方执行失败 —— 原样展示，弹层保持打开。 */
  error?: string | null
}>()

const emit = defineEmits<{
  'update:open': [v: boolean]
  /** 用户输入了正确的确认词并按下执行按钮。 */
  confirm: []
}>()

const typed = ref('')
watch(
  () => props.open,
  (v) => {
    if (v) typed.value = ''
  },
)

const armed = computed(() => typed.value === props.word)
const label = computed(() => props.word)

function close(): void {
  if (!props.busy) emit('update:open', false)
}
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      @click.self="close"
    >
      <div class="w-full max-w-md rounded-lg border border-line bg-panel shadow-lg">
        <div class="flex items-center gap-2 border-b border-line px-4 py-3">
          <TriangleAlert class="size-4 text-down" />
          <h3 class="text-[13.5px] font-semibold">{{ title }}</h3>
        </div>

        <div class="px-4 py-3">
          <ul class="space-y-1 text-[12px] leading-snug text-muted-fg">
            <li v-for="(l, i) in lines" :key="i" class="flex gap-1.5">
              <span class="text-down">·</span>
              <span>{{ l }}</span>
            </li>
          </ul>

          <div class="mt-3">
            <label class="label-micro" for="confirm-word-input">
              输入 <span class="font-mono font-semibold text-down">{{ word }}</span> 以确认
            </label>
            <Input
              id="confirm-word-input"
              v-model="typed"
              class="mt-1 font-mono"
              :placeholder="word"
              autocomplete="off"
              spellcheck="false"
              @keydown.enter="armed && !busy && emit('confirm')"
            />
          </div>

          <p v-if="error" class="mt-2 break-words text-[12px] text-down">{{ error }}</p>
        </div>

        <div class="flex justify-end gap-2 border-t border-line px-4 py-3">
          <Button variant="ghost" size="sm" :disabled="busy" @click="close">取消</Button>
          <Button variant="danger-solid" size="sm" :disabled="!armed || busy" @click="emit('confirm')">
            {{ busy ? '执行中…' : actionLabel }}
          </Button>
        </div>
      </div>
    </div>
  </Teleport>
</template>
