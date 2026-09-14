# DeepSeek API 成本优化（成本台账 + 闲时方案）

- **背景**：DeepSeek 日均花费 > 25 元。
- **结论**：**闲时排程不是主因**。25 元/天里最大的一块是"长会话 × 大工具输出被每轮重发"，
  叠加"用 LLM 干确定性 shell 的活"。闲时排程是最后一步的锦上添花（省 ~50% 单价），不是解药。
- **本文档**：记录已落地的改动、待用户决策项、以及后续可做项。

---

## 1. 花钱的根因（按杠杆排序）

| # | 根因 | 为什么贵 | 证据 / 位置 |
|---|---|---|---|
| 1 | **上下文窗口被配成 1,000,000** | harness 以为装得下 → 几乎不触发压缩 → 长会话每轮重发巨量上下文 | `~/.zcode/v2/config.json` provider `345d3549…` `models.deepseek-flash.limit.context = 1000000`（`output=128000` 同理）。真实窗口远小于此 |
| 2 | **大工具输出进入上下文后被反复重发** | 一次 `grep run.log`（72MB）或整文件 `cat` 的输出会留在上下文里，之后**每一轮**都重发一次 | `run.log` 已 72MB（无轮转）；`crash` 排查时易整文件 grep |
| 3 | **用 LLM 跑 2 小时巡检** | 巡检步骤全是 `pgrep`/`curl`/`wc`/`tail`——确定性命令，却起完整 agent 会话 | ZCode 定时任务（原 12 次/天） |
| 4 | **模型/推理档位无路由** | 例行活也走同一模型/档位 | provider 仅 `deepseek-flash` 单模型 |
| 5 | **微信 bot 长开** | 每条消息可能触发一整轮 agent 运行，成本无上限 | `~/.zcode/v2/bot-config.json` provider `weixin` enabled |

**放大器**：DeepSeek 对**重复前缀**有缓存（命中价 ~未命中的 1/4 以下）。上下文反复变动、早期内容被改写，
会持续 miss 缓存；保持会话开头稳定才能吃到折扣。

---

## 2. 闲时窗口（已用官方定价页核实）

来源：`https://api-docs.deepseek.com/quick_start/pricing`——"Off-peak rates are half of the peak rates"。

| 窗口 | 北京时间（UTC+8） | 价格 |
|---|---|---|
| **高峰** | **周一–周五 09:00–12:00、14:00–18:00** | 标准价 |
| **闲时** | 其余全部时间 + 全天周末 | **五折** |

即高峰 = UTC `01:00–04:00` 与 `06:00–10:00`（周一–周五）。本机 local = UTC+8，与北京时间一致。

> ⚠️ 易错点：星期几必须用 `TZ=Asia/Shanghai date +%u` 判定，不要靠口算（本次实践中曾把周一误当周日）。
> `scripts/offpeak.sh` 已用 `TZ=Asia/Shanghai` 独立判定，不受本机时区影响。

---

## 3. 已落地（本次）

### 3.1 `scripts/soak-health.sh` — 零 token 巡检
把巡检的判断逻辑从 LLM 收回给 shell：进程数（内核必须 1 个）、`--round-sec 900` 校验、`/health`、
账本笔数、`holdTimeSec=0 & force_exit`（秒平 bug）、**有界**（`tail -c 200000`）崩溃扫描。
正常输出一行 `OK ...` 退出 0；异常输出 `ANOMALY` + 逐条原因退出 1。**纯 shell，零 API token**。
实测：0.09s，`OK core=1 node=1 soak=1 round=900 trades=79 health=healthy`。

### 3.2 `scripts/offpeak.sh` — 闲时闸门
`--quiet` 用退出码报窗口（0=闲时/1=高峰）；`--run <cmd>` 只在闲时执行 `<cmd>`，高峰自动跳过。
可挂到任何批处理前：`scripts/offpeak.sh --run <costly-job>`。

### 3.3 定时巡检改为「脚本优先 + 仅闲时」
ZCode 定时任务已从 `0 */2 * * *`（12 次/天）改为 `0 0,2,4,6,8,12,18,20,22 * * *`（9 次/天，
**全部落在闲时**），并改为**先跑零 token 脚本**：脚本 OK 则只回一行、不展开任何明细；仅 ANOMALY 才深入。
（下次运行：周一 12:00 北京 = 闲时窗口起点。）

### 3.4 `scripts/launchd/com.blitzkrieg.soak-health.plist` — OS 级调度（本机不可用，见下）
launchd 每 30 分钟跑零 token 脚本的配置。**本机实测不可用**：仓库在外置卷
`/Volumes/Hard Disk`，launchd 加载即 `Load failed: 5: Input/output error` / 退出码 78
（exec 前失败，无输出），属 macOS 对 launchd 访问外置卷的限制。**已卸载，未留残留**。
plist 保留在仓库，供仓库位于内置卷、或已授予调度器完全磁盘访问的环境使用。

---

## 4. 待用户决策

| 项 | 建议 | 说明 |
|---|---|---|
| **把 DeepSeek `context` 从 1,000,000 改为模型真实窗口** | 先试 64K（`deepseek-flash`）/ 128K（`deepseek-v4-pro`） | **全局配置，影响所有项目**，故未擅自改。这是最大杠杆——让压缩在合理点触发 |

## 5. 后续可做（未做，按需）

1. **工具输出边界纪律**：排查日志一律 `tail -c 200k` / `grep -m 50`，禁止整文件 `cat`/`grep`；
   大文件交给子代理只回结论。
2. **`run.log` 轮转**：加 `logrotate` 或按大小切分，避免再涨到 72MB。
3. **清理死日志**：`run-pre-*.log`（~10MB 历史遗留）可删。
4. **会话卫生**：一任务一会话，避免在超长历史里继续。
5. **模型/档位路由**：例行活用便宜档/关高推理；难任务留给 `deepseek-v4-pro`。
6. **确认微信 bot 流量**：若在自动处理消息，考虑停用或收紧 `allowedCommands`。
7. （可选）把零 token 巡检挂到可访问该卷的调度器，把 ZCode 巡检降到 1–2 次/天。

---

## 6. 复现/验证

```bash
cd "/Volumes/Hard Disk/BlitzkriegBot"
./scripts/soak-health.sh                 # 零 token，OK/ANOMALY + 退出码
./scripts/soak-health.sh --quiet
./scripts/offpeak.sh                     # 当前是否闲时 + 退出码
./scripts/offpeak.sh --run <costly-cmd>  # 仅闲时执行
plutil -lint scripts/launchd/com.blitzkrieg.soak-health.plist
```
