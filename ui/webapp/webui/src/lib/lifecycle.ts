/**
 * What the 启动 / 停止 controls can actually do.
 *
 * Two independent things decide it, and conflating them is what produced a pair
 * of buttons that looked live and answered `lifecycle control disabled; start
 * the gateway with --manage to enable start/stop`:
 *
 *   1. The gateway must accept the verbs at all. `ui_kit_web` started without
 *      `--manage` refuses `start`/`stop` outright — the panel is read-only.
 *   2. Even with `--manage`, 停止 only kills a core *this gateway spawned*. A
 *      core already served on the socket is **adopted** (`Supervisor::start`
 *      sets `owns = false`) and `Supervisor::stop` leaves it running, by design.
 *      So `stop` is genuinely unavailable for an adopted core, and saying
 *      otherwise would be a lie the button cannot honour.
 *
 * `start`, by contrast, IS useful without `--manage` only when the gateway is
 * managing: it would spawn a core. When driven by a read-only gateway the verb
 * is refused, so both controls are inert and the panel says so.
 */
import type { Snapshot } from '@/api/client'

export type LifecycleBlock = Snapshot['gateway']

export interface ControlState {
  /** The gateway accepts lifecycle verbs (started with `--manage`). */
  enabled: boolean
  /** The running core is this gateway's own child, so it can be stopped. */
  canStop: boolean
  /** 启动 can spawn a core: the verbs are enabled and none is reachable. */
  canStart: boolean
  /** Why the controls are inert, for the panel to show verbatim. Null when live. */
  blockedReason: string | null
  /** True when some control is available. */
  usable: boolean
}

/**
 * @param gateway the snapshot's `gateway` block (absent → read-only adapter)
 * @param connected whether a core answers on the socket
 */
export function controlState(
  gateway: LifecycleBlock | null | undefined,
  connected: boolean,
): ControlState {
  // A missing block is a gateway that did not tell us, which is indistinguishable
  // from one that cannot: never assume control we were not granted.
  const enabled = gateway?.lifecycleEnabled === true
  const managed = gateway?.managed === true

  if (!enabled) {
    return {
      enabled: false,
      canStop: false,
      canStart: false,
      blockedReason: '当前网关未开启进程控制（启动时需加 --manage），本面板为只读模式。',
      usable: false,
    }
  }

  // Past the early return the gateway does accept the verbs. With `--manage`
  // but an adopted core, 停止 is the one that cannot act: the gateway did not
  // spawn it, so it will not signal it. 启动 is pointless while a core answers.
  const canStop = connected && managed
  const canStart = !connected
  const blockedReason = connected && !managed
    ? '内核由其他进程启动，本网关只接管读取、不会停止它；如需在此停止，请改由网关启动内核。'
    : null

  return { enabled, canStop, canStart, blockedReason, usable: canStop || canStart }
}

/** What the panel should say about the last core this gateway owned. */
export interface ExitNotice {
  /** "crash" | "clean" — the gateway's classification, not a guess from the pid. */
  kind: 'crash' | 'clean'
  /** Operator-readable description, straight from the gateway. */
  description: string
  /** Replacements already made; 0 means the crash was reported only. */
  restarts: number
  /** The restart budget is spent — the core will not come back on its own. */
  givenUp: boolean
}

/**
 * The last exit as a notice to show the operator, or `null` when there is
 * nothing worth saying.
 *
 * Two rules, and both exist because the alternative is a panel that lies:
 *
 *   1. A **clean** exit is only worth a notice while the core is down. Once it
 *      is up again the operator acted (or a restart happened) and a stale
 *      "stopped" line would read as a current problem.
 *   2. A **crash** is worth saying even after a restart succeeded: silently
 *      recovering from a crash is how a flapping core looks healthy. The
 *      notice changes wording rather than disappearing — `restarts > 0` and the
 *      core answering means it recovered, and `givenUp` means it did not.
 */
export function exitNotice(
  gateway: LifecycleBlock | null | undefined,
  connected: boolean,
): ExitNotice | null {
  const exit = gateway?.lastExit
  if (!exit) return null

  const restarts = gateway?.restarts ?? 0
  const givenUp = gateway?.restartGivenUp === true
  if (exit.kind === 'clean' && connected) return null

  return { kind: exit.kind, description: exit.description, restarts, givenUp }
}
