import DefaultTheme from 'vitepress/theme'
import { inBrowser, useRoute } from 'vitepress'
import { watch, nextTick } from 'vue'
import './custom.css'

/**
 * 落地页动效：滚动进场 + 数字滚动。
 * 全部只在 `prefers-reduced-motion: no-preference` 下生效，
 * 关掉动效的用户直接得到终态，不会看到半透明内容。
 */
function setupLanding() {
  const sections = document.querySelectorAll<HTMLElement>(
    '.bb-sec, .bb-shot, .bb-stat',
  )
  if (!sections.length) return

  const reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches
  if (reduce) {
    sections.forEach((el) => el.classList.add('is-in'))
  } else {
    const io = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (!entry.isIntersecting) continue
          entry.target.classList.add('is-in')
          io.unobserve(entry.target)
        }
      },
      { rootMargin: '0px 0px -10% 0px', threshold: 0.08 },
    )
    sections.forEach((el, i) => {
      // 同屏内错开 80ms，避免整块同时弹出
      el.style.setProperty('--bb-delay', `${(i % 4) * 80}ms`)
      io.observe(el)
    })
  }

  const counters = document.querySelectorAll<HTMLElement>('.bb-stat b[data-count]')
  if (!counters.length) return
  if (reduce) return

  const countIo = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue
        countIo.unobserve(entry.target)
        const el = entry.target as HTMLElement
        const target = Number(el.dataset.count)
        const suffix = el.dataset.suffix ?? ''
        if (!Number.isFinite(target)) continue
        const start = performance.now()
        const duration = 900
        const tick = (now: number) => {
          const p = Math.min(1, (now - start) / duration)
          // easeOutCubic
          const eased = 1 - Math.pow(1 - p, 3)
          el.textContent = Math.round(target * eased).toLocaleString('en-US') + suffix
          if (p < 1) requestAnimationFrame(tick)
        }
        requestAnimationFrame(tick)
      }
    },
    { threshold: 0.6 },
  )
  counters.forEach((el) => countIo.observe(el))
}

export default {
  extends: DefaultTheme,
  setup() {
    if (!inBrowser) return
    // 在首帧之前就打上标记，避免内容先亮一下再隐藏
    if (!window.matchMedia('(prefers-reduced-motion: reduce)').matches) {
      document.documentElement.classList.add('bb-anim')
    }
    const route = useRoute()
    watch(
      () => route.path,
      () => nextTick(() => window.setTimeout(setupLanding, 80)),
      { immediate: true },
    )
  },
}
