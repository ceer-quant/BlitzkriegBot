<script setup lang="ts">
import { ref } from 'vue'
import { login } from '../api/client'

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
  <div class="login-wrap">
    <div class="login-card glass">
      <div class="login-brand">
        <span class="brand-dot"></span>
        <h1>闪电战机器人</h1>
        <p class="sub">登录以接入控制面板</p>
      </div>
      <form @submit.prevent="submit">
        <label class="field">
          <span class="field-name">用户名</span>
          <input v-model="user" class="token-input" autocomplete="username" placeholder="BLITZKRIEG_PANEL_USER" />
        </label>
        <label class="field">
          <span class="field-name">密码</span>
          <input v-model="password" class="token-input" type="password" autocomplete="current-password" placeholder="BLITZKRIEG_PANEL_PASSWORD" />
        </label>
        <p v-if="error" class="login-error">{{ error }}</p>
        <button class="btn gold login-btn" type="submit" :disabled="busy">
          {{ busy ? '登录中…' : '登 录' }}
        </button>
      </form>
      <p class="sub login-hint">
        账号来自网关环境变量 <code>BLITZKRIEG_PANEL_USER</code> / <code>BLITZKRIEG_PANEL_PASSWORD</code>，在启动 ui_kit_web 前设置。
      </p>
    </div>
  </div>
</template>

<style scoped>
.login-wrap {
  min-height: 100vh;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 24px;
}
.login-card {
  width: 100%;
  max-width: 400px;
  padding: 34px 32px 26px;
}
.login-brand { text-align: center; margin-bottom: 22px; }
.login-brand h1 { font-size: 22px; margin: 8px 0 4px; }
.field { display: block; margin-bottom: 14px; }
.field-name {
  display: block;
  font-size: 12px;
  font-weight: 600;
  color: var(--bk-text-dim);
  margin-bottom: 6px;
}
.login-error {
  color: var(--bk-red);
  background: rgba(229, 72, 77, 0.1);
  border-radius: 8px;
  padding: 8px 12px;
  font-size: 13px;
  margin: 0 0 12px;
}
.login-btn { width: 100%; margin-top: 4px; }
.login-hint { font-size: 12px; margin: 16px 0 0; text-align: center; }
code {
  background: rgba(20, 20, 30, 0.06);
  border-radius: 4px;
  padding: 1px 5px;
  font-size: 11px;
}
</style>
