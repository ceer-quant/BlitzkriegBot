import { createApp } from 'vue'
import { createPinia } from 'pinia'
import App from './App.vue'
import { primeAudioOnFirstGesture } from './composables/alertSounds'
import './styles/theme.css'

const app = createApp(App)
app.use(createPinia())
app.mount('#app')

primeAudioOnFirstGesture()
