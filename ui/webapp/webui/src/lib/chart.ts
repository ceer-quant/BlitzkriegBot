/**
 * ECharts theming — diagrams read the same palette as the DOM so the two never
 * drift. `useChart` owns the instance lifecycle (init / resize / theme re-skin).
 */
import * as echarts from 'echarts'
import { onUnmounted, shallowRef, watch, type Ref } from 'vue'
import { useTheme } from './theme'

export interface Palette {
  gold: string
  goldSoft: string
  up: string
  down: string
  info: string
  text: string
  textDim: string
  axis: string
  split: string
  surface: string
  border: string
}

const DARK: Palette = {
  gold: '#f0a02a',
  goldSoft: 'rgba(240,160,42,0.22)',
  up: '#37c98a',
  down: '#f0555b',
  info: '#5b9cf5',
  text: '#f2f2f5',
  textDim: '#8b8b96',
  axis: 'rgba(255,255,255,0.12)',
  split: 'rgba(255,255,255,0.07)',
  surface: 'rgba(24,24,32,0.94)',
  border: 'rgba(255,255,255,0.13)',
}

const LIGHT: Palette = {
  gold: '#d97a06',
  goldSoft: 'rgba(217,122,6,0.18)',
  up: '#17915f',
  down: '#d3373d',
  info: '#2f6fd0',
  text: '#1b1b22',
  textDim: '#6b6b75',
  axis: 'rgba(20,20,30,0.14)',
  split: 'rgba(20,20,30,0.07)',
  surface: 'rgba(255,255,255,0.96)',
  border: 'rgba(20,20,30,0.12)',
}

/** Current palette object — reactive to the theme store via `useChart` watchers. */
export function palette(): Palette {
  return useTheme().isDark.value ? DARK : LIGHT
}

export function tooltipStyle(): Record<string, unknown> {
  const p = palette()
  return {
    backgroundColor: p.surface,
    borderColor: p.border,
    borderWidth: 1,
    padding: [8, 12],
    textStyle: { color: p.text, fontSize: 12 },
    extraCssText: 'border-radius:10px;box-shadow:0 10px 34px rgba(0,0,0,.4)',
  }
}

export function axisX(labels: string[]) {
  const p = palette()
  return {
    type: 'category' as const,
    data: labels,
    boundaryGap: true,
    axisLine: { lineStyle: { color: p.axis } },
    axisTick: { show: false },
    axisLabel: { color: p.textDim, fontSize: 10.5 },
  }
}

export function axisY() {
  const p = palette()
  return {
    type: 'value' as const,
    axisLine: { show: false },
    axisTick: { show: false },
    splitLine: { lineStyle: { color: p.split, type: 'dashed' as const } },
    axisLabel: { color: p.textDim, fontSize: 10.5 },
  }
}

/**
 * Vertical fade under a line, tinted by the series colour. Returns a full
 * `areaStyle`, not a bare gradient: ECharts only reads `areaStyle.color`, so an
 * unwrapped gradient is silently dropped and the area falls back to the default
 * palette (blue) regardless of the line colour.
 */
export function areaFade(hex: string, topAlpha = '5c'): Record<string, unknown> {
  return {
    color: {
      type: 'linear',
      x: 0,
      y: 0,
      x2: 0,
      y2: 1,
      colorStops: [
        { offset: 0, color: `${hex}${topAlpha}` },
        { offset: 1, color: `${hex}00` },
      ],
    },
  }
}

/**
 * Bind an ECharts instance to a container ref. `option` is re-applied on theme
 * change and on every dependency change; the instance is disposed on unmount.
 */
export function useChart(
  el: Ref<HTMLDivElement | null>,
  option: () => echarts.EChartsOption,
  opts: { notMerge?: boolean } = {},
) {
  // shallowRef: the instance is opaque to the reactivity system and its class
  // type carries private members that `ref()`'s deep unwrap would strip.
  const chart = shallowRef<echarts.EChartsType | null>(null)
  const theme = useTheme()

  function ensure(): echarts.EChartsType | null {
    if (!el.value) return null
    if (!chart.value) chart.value = echarts.init(el.value, undefined, { renderer: 'canvas' })
    return chart.value
  }

  function render(): void {
    const c = ensure()
    if (!c) return
    c.setOption(option(), opts.notMerge ?? true)
  }

  watch(el, render, { immediate: true, flush: 'post' })
  watch(option, render, { deep: true })
  watch(theme.isDark, render)

  const ro = typeof ResizeObserver !== 'undefined'
    ? new ResizeObserver(() => chart.value?.resize())
    : null
  watch(el, (n, o) => {
    if (o) ro?.unobserve(o)
    if (n) ro?.observe(n)
  })

  onUnmounted(() => {
    ro?.disconnect()
    chart.value?.dispose()
    chart.value = null
  })

  return { chart, render }
}
