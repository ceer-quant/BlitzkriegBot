use super::super::typed;
use super::prelude::*;
use crate::ipc::schema::*;

// The `engine` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_engine(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    registry: &crate::market::registry::MarketPluginRegistry,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::ENGINE_BOOK => {
            typed(params, |p: BookSnapshotParams| {
                let core = core.clone();
                async move {
                    let bids: Vec<(Decimal, Decimal)> =
                        p.bids.into_iter().map(|l| (l.price, l.size)).collect();
                    let asks: Vec<(Decimal, Decimal)> =
                        p.asks.into_iter().map(|l| (l.price, l.size)).collect();
                    let now = now_ms();
                    let mut c = core.lock().await;
                    // Engine feed only: no `book_snapshot`, so no DRY maker-fill
                    // simulation. This is the path the Rust-native feed drives, and
                    // the one the archive/replay pair must agree with.
                    if c.has_engine() {
                        c.engine_on_data(
                            crate::engine::DataEvent::Book {
                                token_id: p.token_id,
                                bids,
                                asks,
                                now_ms: now,
                            },
                            now,
                        );
                    }
                    Ok::<_, CoreError>(serde_json::json!({ "ok": true }))
                }
            })
            .await
        }
        method::ENGINE_STATS => {
            // PR H (账户筛选修复)：可选 `accountId` 把 per-strategy 计数行
            // 切到单个账户（`strategy_stats_for_account` — 同形状，counters
            // 只读该账户 slice）。缺席 = 全局 fold，老客户端逐字节不变。
            let account = params.get("accountId").and_then(|v| v.as_str());
            match account {
                Some(a) => {
                    let c = core.lock().await;
                    let mut stats = c.engine_stats();
                    stats["strategies"] = serde_json::json!(c.strategy_stats_for_account(a));
                    Ok(stats)
                }
                None => Ok(core.lock().await.engine_stats()),
            }
        }
        method::ENGINE_ROUND => {
            let c = core.lock().await;
            let mut doc = serde_json::to_value(c.round_view()).unwrap_or(Value::Null);
            // 逐市场行（venue 维度），替代 view 里按 asset 去重的
            // `marketPrices`。引擎缺席时市场清单为空 → 空数组，前端照旧
            // 渲染"无行情"。
            //
            // 行情页重复行（同一 (venue,asset) 两行、价格不同）的根因：
            // descriptor 的 `round_slot` 是「到期所属 slot」（end_ms/1000/
            // round_sec），正在交易的一轮其行永远标当前 slot+1；两所
            // discovery 都按「现在还开着」列市场，slot 边界附近再下一档
            // （当前+2）与当前轮（当前+1）并存于 scanner，而这里的逐市场
            // 行从不看 slot —— 边界窗口内同一 (venue,asset) 两行同页。
            // 修在快照装配层（本方法是行情行的唯一数据源）：按「到期
            // slot = 当前 slot + 1」过滤，只留正在交易的一轮；
            // round_slot == 0 是老语料/老插件的缺省（serde default），永不
            // 命中 +1，照旧放行桥接。再按 (venue,asset) 唯一化（先到先得，
            // 行序稳定）：任何上游组合再送进同一 (venue,asset) 的重复行也
            // 只成一行。
            //
            // E2（多族读面）：过滤谓词升级为 scanner 的共用纯函数
            // `is_live_round_row` —— 每行按自己的 (venue, duration) 族钟
            // 判定（行声明 `round_duration_sec`，0 = 老语料 → 全局网格），
            // 与 `books_view` 同一谓词：外族行（如 86400s discovery 按自家
            // 轮长盖的 round_slot）在全局 900s 网格上永不等于 +1，旧谓词
            // 会把 B1d 多族实例喂出的行整体滤没。单族部署下族钟 ≡ 全局
            // 网格，判定与升级前逐位一致。
            let now = now_ms();
            let markets: Vec<serde_json::Value> = c
                .engine()
                .map(|e| {
                    let scanner = e.scanner();
                    let mut seen: std::collections::HashSet<(String, String)> =
                        std::collections::HashSet::new();
                    let rows: Vec<serde_json::Value> = scanner
                        .round_state(now)
                        .markets
                        .iter()
                        .filter(|m| scanner.is_live_round_row(m, now))
                        .filter(|m| seen.insert((m.venue.clone(), m.asset.clone())))
                        .map(|m| {
                            let up = e
                                .book_snapshot(&m.up_token_id)
                                .map(|b| b.mid_price)
                                .filter(|p| *p > Decimal::ZERO)
                                .unwrap_or(m.up_price);
                            let down = e
                                .book_snapshot(&m.down_token_id)
                                .map(|b| b.mid_price)
                                .filter(|p| *p > Decimal::ZERO)
                                .unwrap_or(m.down_price);
                            serde_json::json!({
                                "asset": m.asset,
                                "up": up,
                                "down": down,
                                "venue": m.venue,
                            })
                        })
                        .collect();
                    // RFC-661 M3: natural rows only — the dual-key table
                    // carries every venue's OWN rows (M1), so the retired
                    // shadow-batch synthesis arm has nothing left to add.
                    rows
                })
                .unwrap_or_default();
            doc["marketPrices"] = serde_json::json!(markets);
            // per-venue 轮次钟行（#550 症状②；B1a 升级为逐族一行）。来源
            // = `observed_clock_keys()`（已经开口的 (venue,duration) 钟 ×
            // 全部）× `round_timing_for` —— 与 trading 侧的多族读同一口径，
            // 显示链不自立尺度。老内核没有该键 → 前端 feature-detect 缺失
            // 时走现行为；`durationSec` 为向后兼容增量字段（见 PR 正文
            // 契约段），旧面板忽略该字段，新面板 feature-detect 在场才用。
            let venue_rounds: Vec<serde_json::Value> = c
                .engine()
                .map(|e| {
                    e.scanner()
                        .observed_clock_keys()
                        .into_iter()
                        .map(|(venue, duration)| {
                            let t = e.scanner().round_timing_for(&venue, duration, now);
                            serde_json::json!({
                                "venue": venue,
                                "durationSec": duration,
                                "slot": t.slot,
                                "ageSec": t.age_sec,
                                "timeLeftSec": t.time_left_sec,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            doc["venueRounds"] = serde_json::json!(venue_rounds);
            Ok(doc)
        }
        method::ENGINE_BOOKS => {
            let c = core.lock().await;
            let mut view = serde_json::to_value(c.books_view(15, now_ms())).unwrap_or(Value::Null);
            if let Some(rows) = view.as_array_mut() {
                for row in rows.iter_mut() {
                    // 行自带 venue（books_view 填的，含镜像盘合成行）就用
                    // 行的；只有无标签行（老语料/旧路径）才按 token 反查。
                    // 镜像行与被镜像所共享 token，反查会首中被镜像所。
                    let venue = match row.get("venue").and_then(Value::as_str) {
                        Some(v) if !v.is_empty() => v.to_string(),
                        _ => row
                            .get("upTokenId")
                            .and_then(Value::as_str)
                            .map(|tok| {
                                c.engine()
                                    .and_then(|e| e.venue_of(tok))
                                    .unwrap_or_default()
                            })
                            .unwrap_or_default(),
                    };
                    row["venue"] = serde_json::json!(venue);
                }
            }
            Ok(view)
        }
        method::ENGINE_MARKETS => {
            typed(params, |p: EngineMarketsParams| {
                let core = core.clone();
                async move {
                    let now = now_ms();
                    let venue = p
                        .markets
                        .first()
                        .map(|m| m.venue.clone())
                        .unwrap_or_default();
                    let tokens: Vec<String> = p
                        .markets
                        .iter()
                        .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
                        .collect();
                    let mut c = core.lock().await;
                    if c.has_engine() {
                        c.engine_on_data(
                            crate::engine::DataEvent::RoundMarkets {
                                markets: p.markets,
                                now_ms: now,
                            },
                            now,
                        );
                    }
                    // P4: if Rust-native feeds are on, subscribe the round's tokens so
                    // Node no longer pushes books for them. Multi-plugin data
                    // plane: tokens route to the FEEDING venue's own control —
                    // the tokens carry the venue tag, and untagged (legacy
                    // corpus / Node bridge) lists fall back to the registry's
                    // active plugin, the single-plugin behaviour.
                    let plugin = if venue.is_empty() {
                        registry.active().unwrap_or_default()
                    } else {
                        venue
                    };
                    c.subscribe_feed_tokens(&plugin, tokens).await;
                    Ok::<_, CoreError>(serde_json::json!({ "ok": true }))
                }
            })
            .await
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}
