#![cfg(test)]

use super::*;

use crate::service::Core;
use blitzkrieg_market_api::net::now_ms;
use tokio::sync::Mutex as AsyncMutex;

// ── 行情页重复行：快照装配层的 slot 过滤 + (venue,asset) 唯一化 ─────────

/// 复现一次生产 12 行事故的输入形状：descriptor 的 `round_slot` 是
/// 「到期所属 slot」（正在交易的一轮 = 当前 slot+1），venue discovery
/// 按「现在还开着」列市场，slot 边界附近再下一档（当前+2）与当前轮
/// 并存于 scanner，逐市场行情行不看 slot 时同一 (venue,asset) 两行
/// 同页。负向契约：快照 `marketPrices` 每个 (venue,asset) 恰一行 ——
/// 下一档到期（当前+2）的行不出现，正在交易的一轮（当前+1）才出现。
#[tokio::test]
async fn engine_round_prices_are_unique_per_venue_asset_across_slots() {
    use crate::model::CryptoMarket;
    use rust_decimal_macros::dec;

    let (core, registry, peer) = hot_reload_fixture().await;
    let round_sec = 900i64;
    let now = now_ms();
    let slot = now / 1000 / round_sec;
    let trading_end = (slot + 2) * round_sec * 1000; // the round being traded (slot+1)
    let next_end = (slot + 3) * round_sec * 1000; // the next tier, still "open"

    // One (venue, asset) per row: BTC on polymarket/kalshi for the
    // trading round, plus the same kalshi asset on the NEXT tier's
    // expiry — the shape the venue's "still open" discovery puts on the
    // wire at a slot boundary. Feeds land in the adversarial order the
    // boundary produces (kalshi's 5s poll can put the next tier's batch
    // on the wire BEFORE the trading batch): next tier first, then the
    // trading batch. All through the feed entry (`engine_on_data`), the
    // same path a real plugin's RoundMarkets lands in.
    //
    // E2: per-VENUE batches (the production shape — a discovery lists its
    // own venue's round). The pre-E2 single mixed batch relied on the
    // global-min offset fold to time the kalshi rows: the rollover arm
    // observes only `markets.first()`'s declaration, so kalshi's own
    // clock never spoke and its rows silently rode polymarket's offset.
    // The family-aware filter reads each venue's OWN clock, so the
    // kalshi batch carries its declaration the way a real feed does.
    let mk = |asset: &str, venue: &str, end_ms: i64, up: &str| CryptoMarket {
        asset: asset.to_string(),
        condition_id: format!("cond-{venue}-{asset}-{end_ms}"),
        question_id: format!("q-{venue}-{asset}-{end_ms}"),
        up_token_id: format!("up-{venue}-{asset}-{end_ms}"),
        down_token_id: format!("down-{venue}-{asset}-{end_ms}"),
        up_price: up.parse().unwrap(),
        down_price: dec!(1) - up.parse::<rust_decimal::Decimal>().unwrap(),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / round_sec,
        round_duration_sec: round_sec,
        archive_verdict: false,
        venue: venue.to_string(),
        neg_risk: false,
        question: format!("{asset} up or down"),
    };
    let feed = |core: &std::sync::Arc<AsyncMutex<Core>>, markets: Vec<CryptoMarket>, at: i64| {
        let core = core.clone();
        async move {
            core.lock().await.engine_on_data(
                crate::engine::DataEvent::RoundMarkets {
                    markets,
                    now_ms: at,
                },
                at,
            );
        }
    };
    feed(&core, vec![mk("BTC", "kalshi", next_end, "0.55")], now).await;
    feed(
        &core,
        vec![mk("BTC", "polymarket", trading_end, "0.30")],
        now,
    )
    .await;
    feed(&core, vec![mk("BTC", "kalshi", trading_end, "0.40")], now).await;

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"engine.round","params":{}}"#.to_string(),
    )
    .await;
    let prices = reply["result"]["marketPrices"]
        .as_array()
        .expect("marketPrices array")
        .clone();
    let keys: Vec<(String, String)> = prices
        .iter()
        .map(|p| {
            (
                p["venue"].as_str().unwrap_or_default().to_string(),
                p["asset"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        keys.len(),
        "no (venue,asset) may repeat in the snapshot's price rows: {keys:?}"
    );
    assert_eq!(
        sorted,
        vec![
            ("kalshi".to_string(), "BTC".to_string()),
            ("polymarket".to_string(), "BTC".to_string()),
        ],
        "the trading round's rows only — the next tier must not surface: {prices:?}"
    );
    // The surviving kalshi row is the trading round's quote (0.40), not
    // the next tier's (0.55).
    let kalshi_up = prices
        .iter()
        .find(|p| p["venue"] == "kalshi")
        .expect("kalshi row")["up"]
        .as_str()
        .expect("decimal renders as a string")
        .to_string();
    assert_eq!(
        kalshi_up, "0.40",
        "the trading round's descriptor survives, the next tier's does not"
    );
}

/// `books_view` 是第二个逐市场平铺的数据源（E8-c 盘口深度，快照的
/// `books` 字段）——#560 只修了 `engine.round` 面，同一 slot 边界形状
/// 在这里每资产吐两行、盘口选择器按 (venue,asset) 撞重复 key（生产
/// 2026-10-22 实录：12 行 = PM 4 + kalshi 两轮 8）。负向契约：与
/// `engine.round` 同判据 —— 相邻两轮并存时 `engine.books` 每个
/// (venue,asset) 恰一行，且活的是正在交易的一轮（token 带交易轮的
/// 到期戳）。
#[tokio::test]
async fn engine_books_rows_are_unique_per_venue_asset_across_slots() {
    use crate::model::CryptoMarket;
    use rust_decimal_macros::dec;

    let (core, registry, peer) = hot_reload_fixture().await;
    let round_sec = 900i64;
    let now = now_ms();
    let slot = now / 1000 / round_sec;
    let trading_end = (slot + 2) * round_sec * 1000; // the round being traded (slot+1)
    let next_end = (slot + 3) * round_sec * 1000; // the next tier, still "open"

    let mk = |asset: &str, venue: &str, end_ms: i64| CryptoMarket {
        asset: asset.to_string(),
        condition_id: format!("cond-{venue}-{asset}-{end_ms}"),
        question_id: format!("q-{venue}-{asset}-{end_ms}"),
        up_token_id: format!("up-{venue}-{asset}-{end_ms}"),
        down_token_id: format!("down-{venue}-{asset}-{end_ms}"),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / round_sec,
        round_duration_sec: round_sec,
        archive_verdict: false,
        venue: venue.to_string(),
        neg_risk: false,
        question: format!("{asset} up or down"),
    };
    // Adversarial order, same as the price-row test: the next tier's
    // batch lands first, then the trading round's — per VENUE (the
    // production discovery shape; see the price-row test's E2 note on
    // why the mixed single batch no longer reflects a real feed).
    {
        let mut c = core.lock().await;
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("BTC", "kalshi", next_end)],
                now_ms: now,
            },
            now,
        );
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("BTC", "polymarket", trading_end)],
                now_ms: now,
            },
            now,
        );
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("BTC", "kalshi", trading_end)],
                now_ms: now,
            },
            now,
        );
    }

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"engine.books","params":{}}"#.to_string(),
    )
    .await;
    let rows = reply["result"].as_array().expect("books array");
    let keys: Vec<(String, String)> = rows
        .iter()
        .map(|r| {
            (
                r["venue"].as_str().unwrap_or_default().to_string(),
                r["asset"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        keys.len(),
        "no (venue,asset) may repeat in the depth view: {keys:?}"
    );
    assert_eq!(
        sorted,
        vec![
            ("kalshi".to_string(), "BTC".to_string()),
            ("polymarket".to_string(), "BTC".to_string()),
        ],
        "the trading round's rows only — the next tier must not surface: {rows:?}"
    );
    // The surviving kalshi row carries the TRADING round's tokens, not
    // the next tier's (the depth chart and the K-line card both query by
    // token id — pointing at the wrong expiry would be worse than a dup).
    let kalshi_up = rows
        .iter()
        .find(|r| r["venue"] == "kalshi")
        .expect("kalshi row")["upTokenId"]
        .as_str()
        .expect("token id");
    assert_eq!(
        kalshi_up,
        format!("up-kalshi-BTC-{trading_end}"),
        "the depth view must expose the trading round's tokens"
    );
}

/// RFC-661 §五.7 的 wire 级验收（M3 后的自然行形态）：双 venue 同窗口
/// 同字节 token，各落各的 (venue, token) 行（M1 双键），engine.books
/// 无需任何合成臂就出「两所各一行」——标签是行自带的（venue_of 对共享
/// token 歧义拒绝，反查 fallback 拿不到标签也不再需要），深度各读各的
/// book。若各家 book 深度不同，两行互不串读（§五.6 隔离在显示面的旁证）。
#[tokio::test]
async fn a_shared_token_shows_one_books_row_per_venue_naturally() {
    use crate::model::CryptoMarket;
    use rust_decimal_macros::dec;

    let (core, registry, peer) = hot_reload_fixture().await;
    let round_sec = 900i64;
    let now = now_ms();
    let slot = now / 1000 / round_sec;
    let trading_end = (slot + 2) * round_sec * 1000;

    // 镜像性质本身：两所同窗口 token 逐字节相同（生产 09:30:02 批次
    // 实证的形状），conditionId 同理。
    let mk = |venue: &str, end_ms: i64| CryptoMarket {
        asset: "BTC".to_string(),
        condition_id: format!("cond-{end_ms}"),
        question_id: format!("q-{end_ms}"),
        up_token_id: "mirror-up".to_string(),
        down_token_id: "mirror-down".to_string(),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / round_sec,
        round_duration_sec: round_sec,
        archive_verdict: false,
        venue: venue.to_string(),
        neg_risk: false,
        question: "BTC up or down".to_string(),
    };
    // PM 批先落表，poly 批后到：双键下两所各得各的行（M1 语义——
    // 插入次序无关，无显示簿参与）。
    {
        let mut c = core.lock().await;
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("polymarket", trading_end)],
                now_ms: now,
            },
            now,
        );
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("polysimulator", trading_end)],
                now_ms: now,
            },
            now,
        );
    }

    // 两所各自把 book 打到同一个 token 上（后写的覆盖前写的——book
    // 快照按 token 一份）：这里只喂一份，两行读同一快照，深度逐位同。
    {
        let mut c = core.lock().await;
        c.engine_on_data(
            crate::engine::DataEvent::Book {
                token_id: "mirror-up".to_string(),
                bids: vec![(dec!(0.46), dec!(100))],
                asks: vec![(dec!(0.54), dec!(200))],
                now_ms: now,
            },
            now,
        );
    }

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"engine.books","params":{}}"#.to_string(),
    )
    .await;
    let rows = reply["result"].as_array().expect("books array");
    // 双 venue 各一行：总行数 = 两所之和，没有合成臂也一张不缺。
    let poly_rows: Vec<_> = rows
        .iter()
        .filter(|r| r["venue"] == "polysimulator")
        .collect();
    assert_eq!(
        poly_rows.len(),
        1,
        "the sim venue gets its own depth row from its own table row: {rows:?}"
    );
    assert_eq!(
        poly_rows[0]["upTokenId"], "mirror-up",
        "the sim row exposes the SHARED token — same market, its own tag"
    );
    let pm_rows: Vec<_> = rows.iter().filter(|r| r["venue"] == "polymarket").collect();
    assert_eq!(
        pm_rows.len(),
        1,
        "the PM venue keeps exactly its own row: {rows:?}"
    );
    // 标签是行自带的：venue_of("mirror-up") 对双 venue 语料歧义拒绝
    // （None），token 反查 fallback 拿不到任何标签——行 venue 字段是
    // 唯一来源，两行各自带着正确的所名。
    assert_eq!(
        poly_rows[0]["venue"], "polysimulator",
        "the tag comes from the row itself, never a token reverse-lookup"
    );
    assert_eq!(pm_rows[0]["venue"], "polymarket");
    // 同一份共享 book 的真实读数：两行深度逐位同（book 按 token 一份）。
    assert_eq!(
        poly_rows[0]["up"]["bestBid"], pm_rows[0]["up"]["bestBid"],
        "both rows read the same shared book — real depth, both sides"
    );
}

// ── E2（多族漏网 ③④）：读面过滤按行自身族钟 ─────────────────────────

/// A daily (86400s) family rides the SAME scanner beside the 15m family
/// (polymarket lists 15m, kalshi lists a daily — the multi-family data
/// plane B1d's discovery instances feed). The pre-E2 filter
/// (`current_slot(now)+1` on the GLOBAL grid) dropped the daily row
/// forever — a daily-grid slot number can never equal the global grid's
/// +1 — so `books_view` and the `engine.round` price rows lost every
/// foreign-family row. Both views now judge each row on its OWN (venue,
/// duration) family clock (the shared `is_live_round_row` predicate):
/// the 15m row AND the daily row surface together, both next tiers stay
/// out, and the `round_slot == 0` legacy bridge is untouched.
///
/// 偏移按 venue 单键（B1a 设计：venue 墙钟属性，同 venue 各族共用），
/// 多场馆同格网的向后批次（2026-10-25 poly 轮钟冻结定谳）：kalshi 迟喂
/// 一批「窗口已过期」的旧 slot——收下它会把 polymarket 已落好的新轮行
/// 拖掉（ping-pong 互相覆写，轮钟冻 00:00）。守卫丢过期批次、放行活窗口
/// 批次（未来档先落、当前轮后到的对抗性喂序由上面那三个测试钉住）。
#[tokio::test]
async fn a_stale_expired_batch_is_dropped_and_a_live_one_is_kept() {
    use crate::model::CryptoMarket;
    use rust_decimal_macros::dec;

    let (core, registry, peer) = hot_reload_fixture().await;
    let round_sec = 900i64;
    let now = now_ms();
    let slot = now / 1000 / round_sec;
    let live_end = (slot + 2) * round_sec * 1000; // the round being traded
    let expired_end = slot * round_sec * 1000; // the previous slot's end — already past

    let mk = |venue: &str, end_ms: i64| CryptoMarket {
        asset: "BTC".to_string(),
        condition_id: format!("cond-{venue}-{end_ms}"),
        question_id: format!("q-{venue}-{end_ms}"),
        up_token_id: format!("up-{venue}-{end_ms}"),
        down_token_id: format!("down-{venue}-{end_ms}"),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / round_sec,
        round_duration_sec: round_sec,
        archive_verdict: false,
        venue: venue.to_string(),
        neg_risk: false,
        question: "BTC up or down".to_string(),
    };

    {
        let mut c = core.lock().await;
        // polymarket 先落当前交易轮（活窗口）。
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("polymarket", live_end)],
                now_ms: now,
            },
            now,
        );
        // kalshi 迟喂一批已过期 slot（ping-pong 的那一半）——必须被丢。
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![mk("kalshi", expired_end)],
                now_ms: now,
            },
            now,
        );
    }

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"engine.round","params":{}}"#.to_string(),
    )
    .await;
    let prices = reply["result"]["marketPrices"]
        .as_array()
        .expect("market prices array");
    assert!(
        prices.iter().any(|p| p["venue"] == "polymarket"),
        "the live batch's markets stay registered: {prices:?}"
    );
    assert!(
        !prices.iter().any(|p| p["venue"] == "kalshi"),
        "the expired batch must not drag the table backward: {prices:?}"
    );
}

/// 同 venue 双族的声明偏移互踩是 B1a 既有形状——本牙用跨 venue 双族
/// 隔开偏移，钉的是审计点「外族行在全局 900s 网格上永不等于
/// trading_slot 而被整体滤没」，与行属于哪个 venue 无关（旧谓词对
/// 任何 duration ≠ 全局网格的行一律误杀）。
#[tokio::test]
async fn multi_family_rows_surface_in_both_read_views() {
    use crate::model::CryptoMarket;
    use rust_decimal_macros::dec;

    let (core, _registry, _peer) = hot_reload_fixture().await;
    let now = now_ms();
    let s_slot = now / 1000 / 900i64; // the 900s grid's current slot
    let d_slot = now / 1000 / 86_400i64; // the daily grid's current slot
    let s_trading_end = (s_slot + 2) * 900 * 1000; // the traded 15m round
    let s_next_end = (s_slot + 3) * 900 * 1000; // the 15m next tier (out of band)
    let d_trading_end = (d_slot + 2) * 86_400 * 1000; // the traded daily round
    let d_next_end = (d_slot + 3) * 86_400 * 1000; // the daily next tier (out of band)

    let mk = |asset: &str, venue: &str, end_ms: i64, dur: i64| CryptoMarket {
        asset: asset.to_string(),
        condition_id: format!("cond-{venue}-{asset}-{end_ms}"),
        question_id: format!("q-{asset}-{end_ms}"),
        up_token_id: format!("up-{asset}-{end_ms}"),
        down_token_id: format!("down-{asset}-{end_ms}"),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / dur,
        round_duration_sec: dur,
        archive_verdict: false,
        venue: venue.to_string(),
        neg_risk: false,
        question: format!("{asset} up or down"),
    };
    // Each venue's NEXT tier first (its declaration is out of the
    // ±2-round band → refused), then the traded round — the adversarial
    // order the boundary produces. The DAILY family feeds BEFORE the 15m
    // one: engine.rs's #550 stale-row sweep measures a sibling's lag on
    // its own grid against the FEEDING family's slot, and a daily feed
    // arriving last would mis-measure a live 15m row ~96 rounds "behind"
    // and sweep it (a pre-existing B1c cross-family wrinkle, out of this
    // fix's scope) — the 15m-feeding direction measures the daily row at
    // lag ≈ 0 and keeps it.
    {
        let mut c = core.lock().await;
        for (asset, venue, end, dur) in [
            ("ETH", "kalshi", d_next_end, 86_400),
            ("ETH", "kalshi", d_trading_end, 86_400),
            ("BTC", "polymarket", s_next_end, 900),
            ("BTC", "polymarket", s_trading_end, 900),
        ] {
            c.engine_on_data(
                crate::engine::DataEvent::RoundMarkets {
                    markets: vec![mk(asset, venue, end, dur)],
                    now_ms: now,
                },
                now,
            );
        }
    }

    // ③ the depth view: both FAMILIES' traded rows are in, both next
    // tiers are out, and the traded tokens (not the next tier's) surface.
    let rows = {
        let c = core.lock().await;
        c.books_view(5, now)
    };
    let tokens: Vec<&str> = rows.iter().map(|r| r.up_token_id.as_str()).collect();
    assert!(
        tokens.contains(&&format!("up-BTC-{s_trading_end}")[..]),
        "the 15m traded row surfaces: {rows:?}"
    );
    assert!(
        tokens.contains(&&format!("up-ETH-{d_trading_end}")[..]),
        "the daily traded row must NOT be dropped by the global grid: {rows:?}"
    );
    assert!(
        !tokens.contains(&&format!("up-BTC-{s_next_end}")[..]),
        "the 15m next tier stays filtered (#560): {rows:?}"
    );
    assert!(
        !tokens.contains(&&format!("up-ETH-{d_next_end}")[..]),
        "the daily next tier stays filtered on its OWN grid: {rows:?}"
    );

    // ④ the wire's price rows judge the same predicate — the ENGINE_ROUND
    // assembly's filter keeps exactly the two traded family rows.
    let c = core.lock().await;
    let e = c.engine().expect("engine");
    let scanner = e.scanner();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let surfaced: Vec<String> = scanner
        .round_state(now)
        .markets
        .iter()
        .filter(|m| scanner.is_live_round_row(m, now))
        .filter(|m| seen.insert((m.venue.clone(), m.asset.clone())))
        .map(|m| m.up_token_id.clone())
        .collect();
    assert_eq!(
        surfaced,
        vec![
            format!("up-ETH-{d_trading_end}"),
            format!("up-BTC-{s_trading_end}"),
        ],
        "the engine.round row filter keeps exactly the two traded family rows (table order)"
    );
    // The legacy bridge: a round_slot == 0 row passes unconditionally
    // (the pre-#560 corpora shape).
    let mut legacy = mk("SOL", "polymarket", s_trading_end, 900);
    legacy.round_slot = 0;
    assert!(
        scanner.is_live_round_row(&legacy, now),
        "round_slot == 0 rows ride the unconditional legacy bridge"
    );
}
