# Desktop WebUI (Tauri) — 打包与启动路径

> E6 (#20): `ui/webapp/` 的桌面壳是**独立 workspace**(类似 `rust-executor`),
> 不参与主 workspace 构建 —— 主 CI 无需 GTK 头文件也不会触碰它。

## 布局
- `ui/webapp/src-tauri/` — Tauri v2 壳 crate(`blitzkrieg-webapp`):
  - `src/lib.rs`(逻辑全部在此,bin 只是入口): 应用启动 —— 内嵌一个**只读**
    `ui_kit` `WebServer`(`WebServer::new`,无 dispatcher,无 /api/command)绑在
    自选回环空闲端口上,窗口在运行时指向 `http://127.0.0.1:<port>/panel/`。
    早期版本窗口 url 是 `index.html`(tauri:// 协议),面板里所有
    `fetch('/api/…')` 没有同源服务端可发,整页空白 —— 内嵌服务器正是为恢复
    面板赖以工作的浏览器契约。`BLITZKRIEG_PANEL_URL` 环境变量可把窗口指到
    别处(如 Vite dev server)。另有两个 invoke 命令:
    `desktop_snapshot`(经 headless `AppViewModel` 读快照)、
    `desktop_command`(E5 命令面 dispatcher,lifecycle 关闭)。
  - `tests/chain.rs`: 桌面链路验收 —— 自起一个 dry 内核,直接以普通函数
    调用两条命令体,断言 `desktop_snapshot` 出合法 AppView JSON 且
    `connected:true`、`desktop_command("status")` 出 `action:"status"` 且
    `ok:true`;未设 `BLITZKRIEG_CORE_BIN` 时自动跳过。
  - `tauri.conf.json`: 打包配置。`frontendDist` 指向构建产物 `../webui/dist`
    (E8-d 起固定);窗口列表为空 —— 端口运行时才确定,窗口由 `run()` 创建。
- `ui/webapp/webui/` — 面板前端（Vue 3 + Vite SPA）。
- **零 GUI 依赖契约**: `blitzkrieg-ui-kit` 不含任何 Tauri;`blitzkrieg-ui-kit` 的门禁 `scripts/webapp-check.mjs` 有此断言。

## 开发
```bash
cd ui/webapp
cargo install tauri-cli --version "^2"   # 一次性
# 面板热更新开发:先起网关(51888),Vite dev server 会把 /api 代理过去
cd webui && npm run dev &                # http://127.0.0.1:51889/panel/
cd ../src-tauri
BLITZKRIEG_PANEL_URL=http://127.0.0.1:51889/panel/ cargo tauri dev
```

## 打包
```bash
cd ui/webapp/webui && npm run build       # 先产出 dist/
cd ../src-tauri
cargo tauri build                         # 产物: src-tauri/target/release/bundle/
```
打包仅本地桌面环境执行;不进入常规 CI(Linux 缺 GTK 头文件)。壳内的 WebServer
按 `resolve_socket_path()`(`BLITZKRIEG_SOCKET` 或默认路径)连接内核;它**只读**
—— 不带 dispatcher,无法 start/stop,进程退出也不收走任何内核(浏览器标签页
规则)。

## 网关与鉴权（E6-a → 面板鉴权加固）
桌面 webview 面板的数据后端是已有网关（`ui_kit_web`）：
```bash
target/release/ui_kit_web --socket <core.sock> --addr 127.0.0.1:51888 --manage
```
设计参照 freqtrade 的 REST API（同类程序中最成熟的先例），四点一致：

1. **除探活端点外一律要求鉴权。** freqtrade 只有 `/ping` 免鉴权，这里只有 `/api/ping`。
   **网关模式（`--manage`）始终要求会话**，且凭据只能来自
   `BLITZKRIEG_PANEL_USER` / `_PASSWORD`：两者都未配置时网关**拒绝启动**（exit 2），
   不会自行生成密码 —— 进程自选的密钥操作者既无法轮换也无从审计，把它打印到终端更是
   让终端而非密钥库成为「谁能停掉内核」的答案。只配置一半同样视为未配置（猜操作者
   意图正是面板敞开的成因）。
2. **回环不是信任边界。** 只监听 localhost 挡不住操作者随手打开的网页向 127.0.0.1
   发跨站请求（`<img>` / `<form>` 属 simple request，不触发 CORS 预检）。因此
   `GET /api/command?cmd=stop` 这种跨站形状必须打不到 —— 靠的是会话要求（跨站请求
   无法附带 token），Origin 检查是第二层。
3. **会话会过期。** 绝对 TTL 12h + 空闲超时 30min，存活集合上限 64（超限淘汰最旧）；
   `POST /api/logout` 真正吊销 —— 这点无状态 JWT 做不到。
4. **显式 Origin 白名单。** 非回环来源一律 403；确有需要时用
   `WebServer::set_allowed_origins` 显式加白（对应 freqtrade 的 `CORS_origins`）。

有意偏离 freqtrade 之处：不引入 JWT / refresh token（单个不透明、可吊销的内存 token
更简单且支持真登出），凭据只从环境变量读（与既有 `BLITZKRIEG_*` 约定一致）。

登录页换取会话 token，后续请求携带（query `?token=` / header `X-Auth-Token` /
`Authorization: Bearer` / `bk_session` cookie），否则 401。**会话只存在于内存，
故网关重启即全体失效** —— 面板会退回登录页并说明原因，不会卡在报错页。

面板/命令 verb 与 TUI/web 完全一致（E5 命令面）；无交易下单 API。

## 测试
- `node scripts/webapp-check.mjs` — 门禁：Vue 构建产物存在**且与
  `/panel` 实际下发内容一致**、快照渲染非空、鉴权双向 ACCEPT/REJECT、CORS、
  CSRF（跨站 GET 打不到 lifecycle verb）、`/api/ping` 不泄露状态、登出吊销、
  未配置凭据时网关拒绝启动、且进程不生成任何密码、无 GUI 污染；macOS 上另跑
  `cargo test --test chain`（桌面命令面对真内核全链路）。
- `cargo test -p blitzkrieg-ui-kit --lib web::auth_tests` — 42 项鉴权 / CSRF /
  会话生命周期测试。
- `cd src-tauri && BLITZKRIEG_CORE_BIN=<core 二进制> cargo test --test chain` —
  桌面命令面（`desktop_snapshot` / `desktop_command`）对真 dry 内核的端到端
  链路；不设环境变量则跳过。

## 安全边界
- 未启用 Live;壳crate 与网关均无任何真实凭证读写;dataflow 只读快照 + E5 无交易命令。
- 桌面壳内嵌的服务器**无 dispatcher、无 /api/command、无凭据**——它是与
  「无网关只读适配」同级的表面;网关模式(`--manage`)及其强制凭据语义属于
  `ui_kit_web`,不因桌面打包而放松。
