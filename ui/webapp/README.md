# Desktop WebUI (Tauri) — 打包与启动路径

> E6 (#20): `ui/webapp/` 的桌面壳是**独立 workspace**(类似 `rust-executor`),
> 不参与主 workspace 构建 —— 主 CI 无需 GTK 头文件也不会触碰它。

## 布局
- `ui/webapp/src-tauri/` — Tauri v2 壳 crate(`blitzkrieg-webapp`):
  - `src/main.rs`: 两条 invoke 命令 —— `desktop_snapshot`(经 headless `AppViewModel` 读快照)、`desktop_command`(E5 命令面 dispatcher,lifecycle 关闭)。
  - `tauri.conf.json`: 窗口/打包配置,`frontendDist` 指向静态面板页。
- `ui/webapp/webui/index.html`: 面板前端(纯静态)。
- **零 GUI 依赖契约**: `blitzkrieg-ui-kit` 不含任何 Tauri;`blitzkrieg-ui-kit` 的门禁 `scripts/webapp-check.mjs` 有此断言。

## 开发
```bash
cd ui/webapp
cargo install tauri-cli --version "^2"   # 一次性
cargo tauri dev                          # 开发窗口(macOS 已验: cdecl cargo check 通过)
```

## 打包
```bash
cd ui/webapp
cargo tauri build                        # 产物: src-tauri/target/release/bundle/
```
打包仅本地桌面环境执行;不进入常规 CI(Linux 缺 GTK 头文件)。

## 网关与鉴权(E6-a)
桌面 webview 面板的数据后端是已有网关(`ui_kit_web`):
```bash
target/release/ui_kit_web --socket <core.sock> --addr 127.0.0.1:51888 --manage
```
- 每次启动生成**一次性随机 token**(40 hex),仅打印一次;所有请求必须携带
  (query `?token=` / header `X-Auth-Token` / Basic auth user),否则 401。
- CORS 默认拒绝;仅回环 Origin 放行(403)。
- 面板/命令 verb 与 TUI/web 完全一致(E5 命令面);无交易下单 API。

## 测试
- `npm run ui:webapp` — 11 项门禁(鉴权/命令/CORS/无 GUI 污染)。
- `cargo test -p blitzkrieg-ui-kit --lib auth` — 5 项鉴权单元测试。

## 安全边界
- 未启用 Live;壳crate 与网关均无任何真实凭证读写;dataflow 只读快照 + E5 无交易命令。
