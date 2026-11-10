# 从零跑起来

> **这页解决什么**：让你在本机把 BlitzkriegBot 跑起来，看到第一个界面。
> **需要什么前置**：macOS 或 Linux，已安装 Rust 工具链与 Node.js。
> **读完你能做什么**：以 dry（模拟）模式启动，并在 TUI 里看到行情与账户状态。

## 1. 确认工具链

```bash
rustc --version
cargo --version
node --version
```

**预期输出**：三行版本号，rustc 建议 1.80 以上，node 建议 20 以上。任一条报「command not found」，先装对应工具链再继续。

## 2. 拉取代码

```bash
git clone https://github.com/ceer-quant/BlitzkriegBot.git
cd BlitzkriegBot
```

**预期输出**：`Cloning into 'BlitzkriegBot'...` 结束后目录里能看到 `Cargo.toml` 与 `ui/`。

## 3. 准备配置

```bash
cp .env.example .env
```

**预期输出**：命令无输出即为成功。此时 `.env` 里默认是 **dry 模式**，不会下真单。

> 如果你不确定自己在改什么，**不要**把 `*_LIVE` 之类的开关改成 true。

## 4. 编译

```bash
cargo build --release
```

**预期输出**：最后一行类似 `Finished release [optimized] target(s) in ...`。首次编译需要一些时间。

## 5. 启动

```bash
cargo run --release -- ui
```

**预期输出**：终端进入 TUI 界面，顶部显示模式（dry），并能看到行情面板。

## 出问题怎么办

- 编译报错 → 先看 [KNOWN_ISSUES](../docs/KNOWN_ISSUES) 里是否已有记录
- 启动后界面空白 → 确认 `.env` 存在且没有被 `git clone` 覆盖

## 下一步

看懂界面上的数字含义 → [术语表](/glossary)
