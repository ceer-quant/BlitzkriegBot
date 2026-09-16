<script setup lang="ts">
/** Login gate — user/password against the gateway, gold identity, glass card. */
import { ref } from 'vue'
import { KeyRound, User, LogIn } from 'lucide-vue-next'
import { login } from '@/api/client'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import brandMark from '@/assets/logo.png'

const props = defineProps<{
  /**
   * Why this form is on screen, when it is not a cold start — e.g. a session
   * that expired. Empty string means "first visit", which needs no explanation.
   */
  reason?: string
}>()
const emit = defineEmits<{ (e: 'ok'): void }>()

const user = ref('')
const password = ref('')
const busy = ref(false)
const error = ref<string | null>(null)

async function submit(): Promise<void> {
  if (busy.value) return
  busy.value = true
  error.value = null
  try {
    await login(user.value, password.value)
    emit('ok')
  } catch (e) {
    error.value = e instanceof Error ? e.message : '登录失败'
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <div class="grid min-h-dvh place-items-center px-4 py-10">
    <div class="w-full max-w-[392px]">
      <!-- brand -->
      <div class="mb-6 flex flex-col items-center gap-3 text-center">
        <img
          :src="brandMark"
          alt=""
          aria-hidden="true"
          class="size-14 shrink-0 select-none drop-shadow-[0_6px_18px_rgba(0,0,0,0.4)] rise-in"
          draggable="false"
        >
        <div>
          <h1 class="text-[21px] font-bold tracking-[-0.02em]">闪电战机器人</h1>
          <p class="label-micro mt-1" style="letter-spacing: 0.16em">BLITZKRIEG CONTROL PANEL</p>
        </div>
      </div>

      <form class="glass card-pad rise-in" @submit.prevent="submit">
        <div v-if="props.reason" class="mb-3.5">
          <AlertBanner title="需要重新登录">{{ props.reason }}</AlertBanner>
        </div>
        <div class="space-y-3.5">
          <label class="block">
            <span class="label-micro mb-1.5 block">用户名</span>
            <span class="relative block">
              <User class="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-faint-fg" />
              <Input
                v-model="user"
                class="pl-9"
                autocomplete="username"
                placeholder="BLITZKRIEG_PANEL_USER"
              />
            </span>
          </label>

          <label class="block">
            <span class="label-micro mb-1.5 block">密码</span>
            <span class="relative block">
              <KeyRound class="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-faint-fg" />
              <Input
                v-model="password"
                class="pl-9"
                type="password"
                autocomplete="current-password"
                placeholder="BLITZKRIEG_PANEL_PASSWORD"
              />
            </span>
          </label>
        </div>

        <div v-if="error" class="mt-3.5">
          <AlertBanner tone="error">{{ error }}</AlertBanner>
        </div>

        <Button variant="gold" size="lg" class="mt-5 w-full" type="submit" :disabled="busy">
          <LogIn class="size-4" />{{ busy ? '登录中…' : '登 录' }}
        </Button>

        <p class="mt-4 text-center text-[11.5px] leading-relaxed text-faint-fg">
          账号来自网关环境变量<br>
          <code class="rounded border border-line bg-panel-2 px-1.5 py-px text-[10.5px]">BLITZKRIEG_PANEL_USER</code>
          <span class="mx-1">/</span>
          <code class="rounded border border-line bg-panel-2 px-1.5 py-px text-[10.5px]">BLITZKRIEG_PANEL_PASSWORD</code>
          <br>
          <span class="text-faint-fg">未设置时网关会拒绝启动，不会自行生成密码</span>
        </p>
      </form>

      <p class="mt-4 text-center text-[11px] text-faint-fg">
        默认 dry 模式 · Live 交易未启用
      </p>
    </div>
  </div>
</template>
