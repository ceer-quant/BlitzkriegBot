//! Round discovery — Rust-native (P4/P5).
//!
//! Discovers the current round's UP/DOWN markets for each configured asset by
//! querying Gamma with the slug pattern `${asset}-updown-${label}-${slotStart}`,
//! then feeds the markets to the engine and subscribes their tokens on the
//! orderbook feed. On rollover it re-discovers and re-subscribes.
//!
//! #1165 L3 (发现层全族覆盖): the loop walks EVERY round family the plugin
//! declares (`supported_round_secs` — the same table `round_labels` spells),
//! one Gamma query per (asset, family) per poll, and feeds one sub-batch per
//! family. The family table never narrows with the boot grid: a runtime grid
//! override (#1157) selects among families whose rows are ALREADY in the
//! round table — the target family's rows land before the switch and its
//! books are subscribed, so the switch recovers on its own (L5 自动恢复)
//! instead of starving on a family no instance ever fetched (#1162 root
//! cause A).
//!
//! With this running, Node is out of the market-data path entirely: the core
//! finds its own markets, pulls its own books/spot, decides and trades.

use crate::gamma::{duration_label, slug_for};
use blitzkrieg_bk_contract::net::now_ms;
use blitzkrieg_bk_contract::{MarketDescriptor, MarketHost};
use polymarket_client_sdk_v2::gamma::Client as GammaClient;
use polymarket_client_sdk_v2::gamma::types::request::MarketsRequest;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Spawn the discovery loop. `assets` are e.g. ["BTC","ETH"]; `families` is
/// the declared round-family table in seconds (the plugin's
/// `supported_round_secs` — e.g. [300, 900, 3600, 14400, 86400]). Returns
/// immediately.
pub fn spawn(
    host: Arc<dyn MarketHost>,
    assets: Vec<String>,
    families: Vec<i64>,
    poll_sec: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = match GammaClient::new("https://gamma-api.polymarket.com") {
            Ok(c) => c,
            Err(e) => {
                eprintln!("polymarket-extension: gamma client init failed: {e}");
                return;
            }
        };
        if families.is_empty() {
            eprintln!("polymarket-extension: no declared round families; discovery disabled");
            return;
        }
        // Every family must be a declared label family — the slug builder's
        // own vocabulary. An unsupported entry is skipped with one line, not
        // a disabled loop: the declared table is validated by the anti-drift
        // test (plugin.rs), so this only fires on a genuine declaration bug.
        let families: Vec<i64> = families
            .into_iter()
            .filter(|f| {
                let ok = duration_label(*f).is_some();
                if !ok {
                    eprintln!("polymarket-extension: skipping unsupported round family {f}s");
                }
                ok
            })
            .collect();
        if families.is_empty() {
            eprintln!("polymarket-extension: no supported round families; discovery disabled");
            return;
        }

        // #548：每族最近一次实际喂入的 (slot, 批签名)。顶部逐 poll 重查 +
        // 这个闸替代了旧「slot == last_slot → continue」守卫（它曾让 #498
        // 轮内补查门永不可达）：同 slot 批未变跳过重喂；Gamma 补上架/换发
        // token 改变签名 → 重喂（engine 同 slot merge 按 token 去重，只增
        // 不炸）。#1165 L3：按族分键——各族自己的批各自判重。
        let mut last_fed: HashMap<i64, (i64, String)> = HashMap::new();
        // #1165 项5（族分布直方图回归门禁）：每 poll 的族 × 资产发现计数，
        // 组成变化时打一条（限频：不变不刷屏）。这行日志是「全族覆盖没有
        // 随 boot 网格缩窄」的可 grep 证据——某个族连续缺席即回归。
        let mut last_mix: Vec<(i64, usize)> = Vec::new();
        let mut tick = tokio::time::interval(Duration::from_secs(poll_sec.max(3)));
        loop {
            tick.tick().await;
            let now = now_ms();
            let now_sec = now / 1000;
            // The poll's family mix: (family, assets-found) per declared
            // family, logged on change (the item-5 regression gate).
            let mut mix: Vec<(i64, usize)> = Vec::new();

            for fam in &families {
                let label = duration_label(*fam).expect("families pre-filtered to declared labels");
                let slot = now_sec / fam;

                let mut markets: Vec<MarketDescriptor> = Vec::new();
                // #498: which configured assets actually landed a market THIS
                // poll — the venue can list rounds staggered, and the partial
                // note below decides whether the family keeps re-polling
                // inside the slot.
                let mut found_assets: Vec<String> = Vec::new();
                for asset in &assets {
                    let Some(slug) = slug_for(asset, *fam, now_sec) else {
                        continue;
                    };
                    let req = MarketsRequest::builder().slug(vec![slug.clone()]).build();
                    let found = match client.markets(&req).await {
                        Ok(m) => m,
                        Err(e) => {
                            eprintln!("polymarket-extension: gamma query failed for {slug}: {e}");
                            continue;
                        }
                    };
                    let Some(m) = found.into_iter().next() else {
                        continue;
                    };
                    if m.closed.unwrap_or(false) || !m.active.unwrap_or(false) {
                        continue;
                    }
                    let Some(condition_id) = m.condition_id.map(|c| c.to_string()) else {
                        continue;
                    };
                    let outcomes = m.outcomes.clone().unwrap_or_default();
                    let tokens = m.clob_token_ids.clone().unwrap_or_default();
                    let prices = m.outcome_prices.clone().unwrap_or_default();
                    if outcomes.len() < 2 || tokens.len() < 2 {
                        continue;
                    }
                    let up_idx = outcomes.iter().position(|o| {
                        let o = o.to_lowercase();
                        o == "up" || o == "yes"
                    });
                    let down_idx = outcomes.iter().position(|o| {
                        let o = o.to_lowercase();
                        o == "down" || o == "no"
                    });
                    let (Some(ui), Some(di)) = (up_idx, down_idx) else {
                        continue;
                    };

                    let end_ms = m.end_date.map(|d| d.timestamp_millis()).unwrap_or(0);
                    if end_ms <= now {
                        continue; // already expired
                    }
                    found_assets.push(asset.clone());
                    markets.push(MarketDescriptor {
                        asset: asset.to_uppercase(),
                        condition_id,
                        question_id: m.question_id.map(|q| q.to_string()).unwrap_or_default(),
                        up_token_id: tokens[ui].to_string(),
                        down_token_id: tokens[di].to_string(),
                        up_price: prices
                            .get(ui)
                            .copied()
                            .unwrap_or_else(|| rust_decimal::Decimal::new(5, 1)),
                        down_price: prices
                            .get(di)
                            .copied()
                            .unwrap_or_else(|| rust_decimal::Decimal::new(5, 1)),
                        expires_at_ms: end_ms,
                        round_slot: end_ms / 1000 / fam,
                        // The slug was built from this duration. The archive
                        // has to carry it or a replay cannot tell a 5m
                        // capture from a 15m one (both used to land as
                        // roundDurationSec 0).
                        round_duration_sec: *fam,
                        // #427: this plugin's own identity, declared as data.
                        venue: "polymarket".to_string(),
                        neg_risk: m.neg_risk.unwrap_or(true),
                        question: m.question.clone().unwrap_or_default(),
                    });
                }
                mix.push((*fam, found_assets.len()));

                if markets.is_empty() {
                    continue; // retry next tick (market may not be live yet)
                }
                // #498: same shape as the kalshi gate — Gamma can publish the
                // round's asset markets at different instants, so keep polling
                // inside the slot until every configured asset has landed (or
                // only assets the slug builder itself rejects remain missing,
                // which is a configuration fact, not a late listing).
                let found: std::collections::HashSet<&str> =
                    found_assets.iter().map(|s| s.as_str()).collect();
                let missing: Vec<&String> = assets
                    .iter()
                    .filter(|a| !found.contains(a.as_str()) && slug_for(a, *fam, now_sec).is_some())
                    .collect();

                // #548：批未增长且 slot 已喂过 → 跳过重喂。已集齐的 slot 之后
                // 每 poll 仍会重查（换发检测靠它），但喂过同一批就不必再打扰
                // 引擎。#1165 L3：闸按族分键——一族的批重不重喂与别族无关。
                // 披露：这是「集合相等」比较，Gamma 同轮换发 token 时集合不
                // 同、会重喂——engine 同 slot merge 以 token 去重，旧 token
                // 行保留（partner/持仓键控在旧 token 上，行内替换是方向 C
                // 的雷，不做）。
                let signature = markets
                    .iter()
                    .map(|m| format!("{}:{}|{}", m.asset, m.up_token_id, m.down_token_id))
                    .collect::<Vec<_>>()
                    .join(",");
                if last_fed
                    .get(fam)
                    .is_some_and(|&(s, ref sig)| s == slot && *sig == signature)
                {
                    continue;
                }

                let count = markets.len();
                let label_summary = markets
                    .iter()
                    .map(|m| m.asset.clone())
                    .collect::<Vec<_>>()
                    .join(",");

                // Registers the round with the engine AND subscribes its tokens
                // on the running data feed (both inside the host). One sub-batch
                // PER FAMILY: the B2 soft-unload gate resolves a batch's family
                // from its own descriptors, so a mixed batch would let one
                // unloaded family drag the others down with it.
                host.on_round_markets(markets).await;
                let note = if missing.is_empty() {
                    String::new()
                } else {
                    format!(
                        " [partial: {} still unlisted, re-polling within the slot]",
                        missing
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                };
                eprintln!(
                    "polymarket-extension: discovered {label} round slot={slot} ({count} markets: {label_summary}){note}"
                );
                last_fed.insert(*fam, (slot, signature));
            }

            // #1165 项5：族分布直方图（回归门禁）。组成不变不打——5s poll
            // 不刷屏；变化（某族上线/掉线）即一条，grep
            // `family mix` 可直接判定全族覆盖是否随某次改动缩窄。
            if mix != last_mix {
                let hist = mix
                    .iter()
                    .map(|(f, n)| format!("{}={}", duration_label(*f).unwrap_or("?"), n))
                    .collect::<Vec<_>>()
                    .join(" ");
                eprintln!(
                    "polymarket-extension: family mix (assets found per declared family): {hist}"
                );
                last_mix = mix;
            }
        }
    })
}
