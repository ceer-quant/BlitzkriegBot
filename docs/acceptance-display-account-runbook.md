# display_account 与 operator 状态 — 真线验收 runbook

覆盖四环交付：#561（面板 displayVenue 身份唯一源）/ #562（内核 display_account
配置化）/ #563（default 账户空集死锁修复）/ #565（upgrade 不再覆盖
accounts.toml）。前置：主 checkout ff 到含全部四拍、`sh scripts/upgrade.sh`
升级生产完成。

## 前置

```sh
cd /Volumes/Hard\ Disk/BlitzkriegBot
git log --oneline -5          # 应见 #565/#564/#563/#562/#561 的 squash
sh scripts/upgrade.sh         # 升级生产（重启内核+网关）
```

登录（token 随重启失效，必须重新登录）：

```sh
TOKEN=$(curl -s -X POST http://127.0.0.1:51888/api/login \
  -H 'Content-Type: application/json' \
  -d '{"user":"BlitzkriegBot","password":"…"}' | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
```

## A. display_account 闭环（#562 内核）

```sh
# 1. 当前指针读数（快照顶层字段，老内核无此两字段）
curl -s http://127.0.0.1:51888/api/snapshot -H "X-Auth-Token: $TOKEN" \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("displayAccount"), d.get("displayVenue"))'

# 2. 切换指针 → kalshi
curl -s -X POST http://127.0.0.1:51888/api/display-account/set \
  -H "X-Auth-Token: $TOKEN" -H 'Content-Type: application/json' \
  -d '{"accountId":"kalshi"}'

# 3. 闭环断言：快照 displayAccount=kalshi、displayVenue=kalshi；
#    且 accounts.toml 顶层出现 display_account = "kalshi"（persist 先行）
grep display_account user_layer/configs/accounts.toml

# 4. fail-closed：不存在的 id 被拒（HTTP 200 + {"error": …} 信封）
curl -s -X POST http://127.0.0.1:51888/api/display-account/set \
  -H "X-Auth-Token: $TOKEN" -H 'Content-Type: application/json' \
  -d '{"accountId":"ghost"}'
```

面板侧：账户切换器切到 kalshi → 行情页徽章/轮次/卡片全部跟随 kalshi；
切回 polymarket 同理。**纯展示指针**：切换不改变记账/下单的会话默认账户
之外任何语义。

## B. 重复行情根除（#560）

```sh
curl -s http://127.0.0.1:51888/api/snapshot -H "X-Auth-Token: $TOKEN" \
  | python3 -c '
import json,sys
d=json.load(sys.stdin)
keys=[(r["venue"],r["asset"]) for r in d["prices"]]
dup=[k for k in set(keys) if keys.count(k)>1]
print("rows:",len(keys),"dups:",dup or "none")'
```

判据：同 (venue, asset) 不得出现两行（此前 kalshi 5s 轮询把再下一档轮落表，
ENGINE_ROUND 装配不看 slot → 8 卡里 4 对重复）。

### B2. books 面（盘口深度卡，#567 补漏）

`round.prices` 只是两个平铺面之一；`engine.books`（webui 快照 `books` 字段，
盘口深度卡数据源）是第二个——#560 曾漏修此面（生产实录：books 12 行 =
PM 4 + kalshi 相邻两轮 8，token `…211215-15` vs `…211230-30`）。

```sh
curl -s http://127.0.0.1:51888/api/snapshot -H "X-Auth-Token: $TOKEN" \
  | python3 -c '
import json,sys
d=json.load(sys.stdin)
pairs=[(b.get("venue",""),b["asset"]) for b in (d.get("books") or [])]
dup=[k for k in set(pairs) if pairs.count(k)>1]
print("books rows:",len(pairs),"dups:",dup or "none")'
```

判据：books 行数 = 8（4 资产 × 2 所），无 (venue,asset) 重复；且 kalshi 行
的 `upTokenId` 带当前交易轮的到期戳（不是再下一档）。

## C. default 空集后可启停（#563）

1. 面板策略页把 default 账户全部策略停用（第一次 disable 拆掉 boot 注册的
   flat scope——这就是死锁的形成步）。
2. 再点任一策略的启用：**必须变成绿色「已启用」且读数翻转为运行中**。
   修复前回执是 `{found:true, applied:false}` 短路——永远无法再启用。
3. 内核日志佐证：`grep "enablement written" /tmp/blitzkrieg-run.log` 应见
   scoped 写真正落盘（修复前零日志）。

## D. toggle 假成功根除（#564）

scoped 通道内核拒绝时（如把某账户 strategies 集合写坏后强行启用），策略页
必须弹**红色** flash 并逐字透传 `rpc -32000: …`，不再闪绿色「已启用」。
（正向路径无法在真线安全触发内核拒绝，此条以门禁 teeth
`strategies-global-visibility.check.mjs` 的 `if (res.error) throw` 断言 +
沙盒 mock 网关为凭。）

## E. upgrade 不再洗状态（#565）

```sh
grep display_account user_layer/configs/accounts.toml   # 记住当前值
sh scripts/upgrade.sh --check                            # 漂移报告应含：
# = accounts.toml (operator state kept — the running copy wins)
```

下一次真升级后复核：`display_account` / 各账户 `strategies` 键仍在，
`data/strategy-state.json` 与文件一致。 propagation 门禁
（`sh scripts/upgrade-propagate-test.sh`，38 passed）已钉运行副本赢 /
drift 标注 / 回滚不碰 / 缺失种子四条路径。
