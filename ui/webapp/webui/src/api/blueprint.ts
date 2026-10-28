/**
 * Blueprint domain — the #362 blueprint editor's compile/save/load IPC
 * proxies (mirror core/blitzkrieg_core/src/ipc/schema.rs).
 */
import { request } from './core.ts'

// ── #362 蓝图编辑器（mirror core/blitzkrieg_core/src/ipc/schema.rs）──────────

/** `blueprint.compile` 应答：生成的 Lua 源码。 */
export interface BlueprintCompileDoc {
  lua: string
  error?: string
}

/** `blueprint.save` 的请求体（camelCase wire 与内核 serde 对齐）。 */
export interface BlueprintSaveBody {
  name: string
  json: string
  /** 同名策略包已存在时必须显式为 true。 */
  overwrite?: boolean
}

/** `blueprint.save` 回执：写了什么、写到哪、sha256 可对账。 */
export interface BlueprintSaveDoc {
  name: string
  packageDir: string
  blueprintPath: string
  luaPath: string
  manifestPath: string
  luaSha256: string
  /** blueprint/lua/manifest 三个文件的字节数。 */
  bytes: number[]
  /** 可进化旋钮名（manifest tunables 导出）——空数组表示本蓝图没有数值锚点。 */
  tunables: string[]
  error?: string
}

/** `blueprint.load` 应答（#393）：包里的蓝图文档原文 + 相对路径。 */
export interface BlueprintLoadDoc {
  name: string
  blueprintPath: string
  json: string
  error?: string
}

/** `blueprint.loadSource` 应答：手写包的入口源码 + manifest 原文。 */
export interface BlueprintLoadSourceDoc {
  name: string
  packageDir: string
  luaPath: string
  manifestPath: string
  lua: string
  /** manifest 里当前入口文件的 sha256，hex。 */
  sha256: string
  /** manifest 原文（任意 JSON），编辑器原样展示。 */
  manifest: unknown
  error?: string
}

/** `blueprint.saveSource` 请求体（camelCase wire 与内核 serde 对齐）。 */
export interface BlueprintSaveSourceBody {
  name: string
  lua: string
  /** 磁盘上已有不同源码时必须显式为 true。 */
  overwrite?: boolean
}

/** `blueprint.saveSource` 回执：写了什么、新的 sha256、字节数。 */
export interface BlueprintSaveSourceDoc {
  name: string
  packageDir: string
  luaPath: string
  manifestPath: string
  luaSha256: string
  /** lua/manifest 两个文件的字节数。 */
  bytes: number[]
  error?: string
}

export const blueprintApi = {
  // ── #362 蓝图编辑器（compile 预览 + save 落盘，全程走内核 IPC）────────────
  // 两条都是内核 `blueprint.*` IPC 的纯代理（网关 body 即 params 逐字转发），
  // 内核错误包成 200 + `{error}` — 调用方必须检查 `error` 并内联渲染。

  /**
   * 编译预览（`blueprint.compile`）：把当前画布 JSON 编成 Lua 源码。纯只读；
   * 拒绝消息含节点 id（#361 三阶段编译链保证），必须原样展示，绝不静默。
   */
  blueprintCompile: (json: string) =>
    request<BlueprintCompileDoc>('/blueprint/compile', {
      method: 'POST',
      body: JSON.stringify({ json }),
    }),
  /**
   * 保存策略包（`blueprint.save`）：内核校验名字 → 现场编译 → 写入
   * blueprint.json + strategy.lua + manifest.json。UI 永不碰文件系统；
   * 同名包已存在时必须显式 `overwrite`（回执里带三个文件路径 + sha256）。
   */
  blueprintSave: (body: BlueprintSaveBody) =>
    request<BlueprintSaveDoc>('/blueprint/save', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /**
   * 读回策略包的蓝图文档（`blueprint.load`，#393）：编辑策略入口的预载读 ——
   * `blueprint.save` 的只读镜像。只保存过蓝图编译产物的包才有 blueprint.json，
   * 手写包会被内核以明确拒绝作答（原样展示，绝不编造图）。
   */
  blueprintLoad: (name: string) =>
    request<BlueprintLoadDoc>('/blueprint/load', {
      method: 'POST',
      body: JSON.stringify({ name }),
    }),
  /**
   * 读回手写包的入口源码（`blueprint.loadSource`）：编辑策略入口对**所有**
   * Lua 包开放的那一半 —— 没有 blueprint.json 的包从这条路打开，源码模式
   * 展示 Lua 与 manifest；内核按 manifest.entry 定位文件（同名字闸门）。
   */
  blueprintLoadSource: (name: string) =>
    request<BlueprintLoadSourceDoc>('/blueprint/load-source', {
      method: 'POST',
      body: JSON.stringify({ name }),
    }),
  /**
   * 手写包源码写回（`blueprint.saveSource`）：入口文件原样落盘，sha256 现场
   * 重算进 manifest（装载器照常验证字节）。编辑不创建：包必须已存在；内容
   * 与磁盘不同时必须显式 `overwrite`。
   */
  blueprintSaveSource: (body: BlueprintSaveSourceBody) =>
    request<BlueprintSaveSourceDoc>('/blueprint/save-source', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
}
