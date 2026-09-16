/**
 * Alert sounds — ported verbatim from ui/hft.html (Wire Audio API, no asset
 * files). One audio context + master gain + compressor; five cues:
 *
 *   ding   880→1760Hz sine sweep        风控恢复 / 通用确认
 *   dang   220→110Hz sine sweep        亏损单笔
 *   profit C5-E5-G5-C6 triangle arpeggio 盈利单笔
 *   order  880→1320Hz square blip       新开仓/挂单
 *   wuwu   400/600Hz saw+square siren   风控熔断
 *
 * Web Audio requires a user gesture to unlock the context: the first click
 * anywhere resumes it (button edges are the natural source).
 */
let ctx: AudioContext | null = null
let master: GainNode | null = null
// default ON; persisted so the user's mute survives reloads
let enabled = localStorage.getItem('blitzkrieg-panel-sound') !== 'off'

export function soundEnabled(): boolean {
  return enabled
}
export function setSoundEnabled(next: boolean): void {
  enabled = next
  localStorage.setItem('blitzkrieg-panel-sound', next ? 'on' : 'off')
}

function getCtx(): AudioContext | null {
  if (typeof window === 'undefined') return null
  if (!ctx) {
    const AC = window.AudioContext ?? (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext
    if (!AC) return null
    try {
      // master volume ×4 through a compressor, exactly like hft.html
      ctx = new AC()
      master = ctx.createGain()
      master.gain.value = 4
      const comp = ctx.createDynamicsCompressor()
      comp.threshold.value = -10; comp.knee.value = 10
      comp.ratio.value = 12; comp.attack.value = 0.003; comp.release.value = 0.25
      master.connect(comp); comp.connect(ctx.destination)
    } catch {
      return null
    }
  }
  if (ctx.state === 'suspended') ctx.resume().catch(() => {})
  return ctx
}

function osc(type: OscillatorType, freq: number, gain: number, dur: number, rampTo?: number, up = true): void {
  const c = getCtx(); if (!c || !master || !enabled) return
  const o = c.createOscillator(); const g = c.createGain()
  o.connect(g); g.connect(master)
  o.type = type
  const t = c.currentTime
  if (up) {
    o.frequency.setValueAtTime(freq, t)
    o.frequency.exponentialRampToValueAtTime(freq * 2, t + dur * 0.2)
  } else {
    o.frequency.setValueAtTime(freq, t)
    o.frequency.exponentialRampToValueAtTime(freq / 2, t + dur * 0.3)
  }
  g.gain.setValueAtTime(gain, t)
  g.gain.exponentialRampToValueAtTime(0.01, t + dur)
  o.start(t); o.stop(t + dur)
}

export function playDing(): void {
  osc('sine', 880, 0.3, 0.3, undefined)
}
export function playDang(): void {
  osc('sine', 220, 0.4, 0.5, undefined, false)
}
export function playProfit(): void {
  const c = getCtx(); if (!c || !master || !enabled) return
  const notes = [523.25, 659.25, 783.99, 1046.5]
  notes.forEach((freq, i) => {
    const o = c.createOscillator(); const g = c.createGain()
    o.connect(g); g.connect(master!)
    o.type = 'triangle'
    const t = c.currentTime + i * 0.09
    o.frequency.setValueAtTime(freq, t)
    g.gain.setValueAtTime(0.0001, t)
    g.gain.exponentialRampToValueAtTime(0.32, t + 0.02)
    g.gain.exponentialRampToValueAtTime(0.01, t + 0.35)
    o.start(t); o.stop(t + 0.4)
  })
}
export function playOrder(): void {
  osc('square', 880, 0.18, 0.13, undefined)
}
export function playWuwu(): void {
  const c = getCtx(); if (!c || !master || !enabled) return
  // 0.8s siren: sawtooth 400↔600Hz against square 300↔500Hz, like hft.html
  const o1 = c.createOscillator(); const o2 = c.createOscillator(); const g = c.createGain()
  o1.connect(g); o2.connect(g); g.connect(master)
  o1.type = 'sawtooth'; o2.type = 'square'
  const t = c.currentTime
  o1.frequency.setValueAtTime(400, t)
  o2.frequency.setValueAtTime(300, t)
  for (let i = 1; i <= 4; i++) {
    const at = t + i * 0.2
    const hi = i % 2 === 1
    o1.frequency.linearRampToValueAtTime(hi ? 600 : 400, at)
    o2.frequency.linearRampToValueAtTime(hi ? 500 : 300, at)
  }
  g.gain.setValueAtTime(0.2, t)
  g.gain.linearRampToValueAtTime(0.01, t + 0.85)
  o1.start(t); o2.start(t); o1.stop(t + 0.85); o2.stop(t + 0.85)
}

/** Unlock the audio context on the first user gesture. */
export function primeAudioOnFirstGesture(): void {
  const handler = () => { getCtx(); window.removeEventListener('pointerdown', handler) }
  window.addEventListener('pointerdown', handler, { once: true })
}
