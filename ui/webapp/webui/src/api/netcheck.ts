/**
 * Net-check domain — the kernel's own outbound-path probe (mirror
 * core/market_api + ui_kit core/types.rs + web/mod.rs cache).
 */
import { request } from './core.ts'

// ── 网络自检（mirror core/market_api + ui_kit core/types.rs + web/mod.rs 缓存）─

/** 一次探测：交易链路上的一条网络路径（解析 / TCP / TLS / 一次廉价请求）。 */
export interface NetCheckItem {
  /** `venue-rest` | `venue-ws` | `discovery` | `spot-ws` — 标识符，不是文案。 */
  name: string
  target: string
  ok: boolean
  /**
   * 机器判定：`ok` | `dns_failed` | `tcp_refused` | `tcp_timeout` | `tls_cert`
   * | `tls_error` | `timeout` | `http_error` | `transport_error` | `rejected`
   * | `unsupported`。文案由 `lib/net-check.ts` 给出，未知值原样显示、绝不隐藏。
   */
  status: string
  addrs?: string[]
  /** 解析到的地址全在代理的 fake-IP 段内 — 是解析器的事实，不是路径故障。 */
  fakeIp?: boolean
  ms?: number
  detail?: string
}

export interface NetCheckReport {
  /** 全部通过才为真；空报告与 `unsupported` 都不算通过。 */
  ok: boolean
  tsMs?: number
  /** `ok` | `tls_blocked` | `dns_failed` | `proxy_env` | `fake_ip` | `partial` | `unsupported`。 */
  hintCode?: string
  /** 内核给出的一句话判读。 */
  hint?: string
  /** 探测进程看到的代理变量**名字**（永不含值）。 */
  proxyEnv?: string[]
  items?: NetCheckItem[]
}

/** `GET /api/netcheck` 的响应：缓存里的结果 + 是否有一次探测在跑。 */
export interface NetCheckDoc {
  probing: boolean
  /** 结果年龄（毫秒）；从未探测过为 null。 */
  ageMs: number | null
  report: NetCheckReport | null
  /** 探测失败的原因（内核没应答 / 未上报）；成功为 null。 */
  error: string | null
}

export const netcheckApi = {
  /**
   * 网络自检（`GET /api/netcheck`）：内核拨测它自己出网要走的每条路径。
   *
   * 网关侧是缓存 + 后台探测（20s TTL）：探测要拨真实端点、耗时以秒计，而网关
   * 的 HTTP 接受循环是单线程的 — 内联探测会连带冻住快照轮询。所以这里拿到的是
   * 「最近一次结果 + 是否正有一次在跑」，`probing` 为真时页面显示探测中，而不是
   * 拿旧结果冒充新结果。
   */
  netCheck: () => request<NetCheckDoc>('/netcheck'),
  /**
   * 发起一次新的网络自检（`POST /api/netcheck/probe`）。
   *
   * 返回的还是同一个文档：探测永远在网关侧后台跑（`probing: true`），调用方
   * 继续轮询 `netCheck()` 直到结果落地。如果已经有一次在跑，这次请求不会另起
   * 一次 —— 那次的result 就是答案。
   */
  probeNetCheck: () => request<NetCheckDoc>('/netcheck/probe', { method: 'POST' }),
}
