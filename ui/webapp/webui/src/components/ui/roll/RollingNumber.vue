<script setup lang="ts">
/**
 * Odometer digits — each character rolls vertically to its new value.
 *
 * Every digit is its own fixed-width cell whose width is set by a hidden "0"
 * rather than by whichever glyph is showing, so the rendered width depends only
 * on how many characters there are, never on their shapes. That is what lets a
 * neighbour hold its position while the value ticks: measured on the round
 * header, proportional figures put `1111s 已过` and `1000s 已过` 7.95px apart at
 * the same character count, and each extra digit added ~14px more, so the status
 * text to the right slid on every tick.
 *
 * Direction is always the shortest path (`delta` in −5..4), chosen per digit
 * from the value it is currently showing. That matters for a countdown: a
 * seconds digit stepping 0→9 must roll one glyph *down*, not sweep nine glyphs
 * back up, or the most prominent digits twitch once every ten seconds.
 *
 * Each change rebuilds the cell's strip as just the glyphs it travels through —
 * the old digit first, the new one last, at most six of them — and animates that
 * local strip. Because the strip is rebuilt from the current value each time,
 * there is no running offset to keep in range, so nothing drifts over a long
 * session and no timer is needed to re-base it. Re-mounting via `:key` is what
 * restarts the animation; the strip's first frame is the old digit at offset 0,
 * which is exactly what was already on screen, so the restart is invisible.
 *
 * Punctuation (`.`, `,`, `:`, `-`, `+`, `$`, `%`) renders as static text.
 *
 * `prefers-reduced-motion` is honoured: `styles/theme.css` collapses every
 * animation to 0.001ms globally, which lands each strip on its final digit.
 */
import { computed, ref, watch } from 'vue'
import { cn } from '@/lib/utils'

const props = withDefaults(
  defineProps<{
    /** Number to display, or a pre-formatted string (e.g. `00:39`). */
    value: number | string
    /** Fixed decimal places when `value` is a number. */
    decimals?: number
    /** Roll duration. Keep below the update interval so it settles before the next tick. */
    duration?: number
    /** Thousands separators when `value` is a number. */
    grouping?: boolean
    class?: string
  }>(),
  { decimals: 0, duration: 420, grouping: false },
)

/** Signed shortest step between two digits, in −5..4. */
function shortestStep(from: number, to: number): number {
  return ((((to - from + 5) % 10) + 10) % 10) - 5
}

const text = computed<string>(() => {
  const v = props.value
  if (typeof v !== 'number') return v === null || v === undefined ? '—' : String(v)
  if (!Number.isFinite(v)) return '—'
  return props.grouping
    ? v.toLocaleString(undefined, { minimumFractionDigits: props.decimals, maximumFractionDigits: props.decimals })
    : v.toFixed(props.decimals)
})

interface Cell {
  /** The character to render; for a rolling digit this is the digit's new value. */
  ch: string
  /** How far this digit travels this change: −5..4. */
  step: number
  /** Glyphs to lay out on the way, old digit first and new digit last. Empty when static. */
  stack: number[]
}

const digitOf = (ch: string | undefined): number | null =>
  ch !== undefined && ch >= '0' && ch <= '9' ? Number(ch) : null

function build(now: string, before: string): Cell[] {
  // Compare from the RIGHT. A number that gains a place has to carry like an
  // odometer: as `99` becomes `100` the units and tens must roll 9→0 while the
  // new hundreds digit settles. Aligning from the left instead would pit the new
  // `1` against the old `9` and misalign every remaining place.
  const shift = now.length - before.length
  return [...now].map((ch, i) => {
    if (ch < '0' || ch > '9') return { ch, step: 0, stack: [] }
    const to = Number(ch)
    const from = digitOf(before[i - shift])
    // A genuinely new leading place has nothing to roll from, so it starts
    // settled rather than sweeping in from nowhere.
    const step = from === null ? 0 : shortestStep(from, to)
    const dir = Math.sign(step)
    const start = from ?? to
    const stack = step === 0
      ? [to]
      : Array.from({ length: Math.abs(step) + 1 }, (_, k) => ((((start + dir * k) % 10) + 10) % 10))
    return { ch, step, stack }
  })
}

const cells = ref<Cell[]>([])
/** Bumped whenever a digit rolls, to re-mount the strips and restart their animations. */
const gen = ref(0)

// A watcher rather than a computed: each step is measured against what was
// rendered last, and that comparison has to happen exactly once per change.
// Deriving it on demand would measure against a value already overwritten.
watch(
  text,
  (now, before) => {
    const next = build(now, before ?? '')
    if (next.some((c) => c.step !== 0)) gen.value++
    cells.value = next
  },
  { immediate: true },
)
</script>

<template>
  <span :class="cn('roll', props.class)">
    <span class="sr-only">{{ text }}</span>
    <span class="roll-paint" aria-hidden="true">
      <template v-for="(cell, i) in cells" :key="i">
        <span v-if="!cell.stack.length" class="roll-static">{{ cell.ch }}</span>
        <!--
          The two spans inside the cell are written adjacent, with no whitespace
          between them. The sizer is in flow, so a stray text node there would
          count toward the cell's width and break the fixed one-digit column.

          `.roll-sizer` is in-flow, invisible, and one tabular digit wide: it
          gives the cell a real line box to take a baseline from, and sets the
          cell's width without depending on which glyph is showing.

          Clipping lives on `.roll-clip` rather than the cell, because
          `overflow: hidden` on an inline-block forces that box's baseline to its
          bottom margin edge — on the cell that pulled the digits up off the text
          baseline by 5.25px at 42px and 1.5px at 11.5px. On this inner layer it
          costs nothing, since only the cell is a baseline-aligned inline box.

          Travel is always upward by `stack.length - 1`, which parks the LAST
          glyph of the stack in the visible row. The direction of the value change
          only decides which glyphs the stack contains, so the motion stays one
          consistent way instead of reversing between a countdown and an
          increment.
        -->
        <span v-else class="roll-cell"><span class="roll-sizer" aria-hidden="true">0</span><span class="roll-clip"><span
            :key="gen"
            class="roll-strip"
            :style="{ '--roll-to': `${-(cell.stack.length - 1)}em`, animationDuration: `${props.duration}ms` }"
          ><span v-for="(g, gi) in cell.stack" :key="gi" class="roll-glyph">{{ g }}</span></span></span></span>
      </template>
    </span>
  </span>
</template>

<style scoped>
/*
 * Tabular figures are load-bearing, not cosmetic: without them the hidden "0"
 * sizer would not match every other digit's advance and the cells would differ in
 * width — exactly the jitter this component exists to remove.
 */
.roll {
  font-variant-numeric: tabular-nums;
  /*
   * Tracking must be zero. A non-zero value is added after every character
   * including the single-glyph entries in the strip, so the width-defining "0"
   * would be narrower than a real digit (clipping it) or wider (stretching every
   * cell). Fixed-width cells cannot be tracked.
   */
  letter-spacing: 0;
}

/*
 * The digits exist twice: once as selectable text in `.sr-only`, once as glyphs
 * here. Selecting a number across both copies would paste it doubled, so the
 * painted copy is kept out of selections — a copy of a row yields the value
 * exactly once, from the `.sr-only` text.
 */
.roll-paint {
  user-select: none;
}

/* Punctuation rides the surrounding text directly, so it keeps the normal
   baseline without any box of its own. */
.roll-static {
  display: inline;
}

/*
 * One tabular digit wide, one line tall. `overflow` stays VISIBLE here: see
 * `.roll-clip`. An inline-block with a non-visible overflow takes its baseline
 * from its bottom margin edge, which would lift every digit off the text
 * baseline. With a visible overflow and the in-flow sizer above, the cell takes
 * its baseline from that line box instead — the same baseline the surrounding
 * text uses — so digits sit exactly where plain text would.
 */
.roll-cell {
  display: inline-block;
  position: relative;
  height: 1em;
  line-height: 1em;
}

.roll-sizer {
  visibility: hidden;
}

/* The visible window: one line tall, positioned over the cell's content box. */
.roll-clip {
  position: absolute;
  inset: 0;
  overflow: hidden;
}

.roll-strip {
  position: absolute;
  inset: 0 auto auto 0;
  display: flex;
  flex-direction: column;
  animation: roll var(--ease-out-soft) both;
}

.roll-glyph {
  display: block;
  height: 1em;
  line-height: 1em;
  /*
   * Gradient text has to be declared here, and this is not optional.
   *
   * `background-clip: text` only clips an element's background to its OWN text,
   * and these glyphs sit inside the animated strip — so a gradient painted on
   * the root never reaches them, while the `color: transparent` that comes with
   * it does get inherited. The result is four unpainted digits: the geometry is
   * correct and the pixels are simply invisible.
   *
   * `background: inherit` is the obvious fix and does not work — each element
   * along the way would then paint the gradient itself. A custom property is
   * inherited through the whole subtree regardless of positioning or stacking, so
   * the gradient arrives intact and only the glyph clips it. When no gradient is
   * in scope this resolves to `none` and the glyph renders in the inherited
   * colour, which is what the solid-colour age counter relies on.
   */
  background-image: var(--grad-text, none);
  -webkit-background-clip: text;
  background-clip: text;
}

/* Start at 0 — the previous digit, already on screen — and travel to the new one. */
@keyframes roll {
  from {
    transform: translateY(0);
  }
  to {
    transform: translateY(var(--roll-to, 0em));
  }
}
</style>
