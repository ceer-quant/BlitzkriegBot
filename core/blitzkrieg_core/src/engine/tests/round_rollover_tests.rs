#![cfg(test)]

use super::*;
use crate::scanner::TimingBlock;
use std::sync::{Arc, Mutex};
// Round feed ingestion: #498 staggered venue listings merge, same-slot
// top-ups, per-venue rollover, mirror/dual-key rows, B1c multi-family
// rounds and the E3 per-family gate clocks.

// ── #498: staggered venue listings must MERGE, not rollover ─────────────

/// A probe that counts `on_round` fires and reports the scanner's market
/// count — the exact state a same-slot top-up must and must not touch.
struct RoundProbe {
    rounds: Arc<Mutex<Vec<i64>>>,
}
impl EngineStrategy for RoundProbe {
    fn name(&self) -> &str {
        "round_probe"
    }
    fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, slot: i64, _left: i64, _now: i64) {
        self.rounds.lock().unwrap().push(slot);
    }
    fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<crate::signal::TradeSignal> {
        Vec::new()
    }
}

fn eth_market(end_ms: i64) -> CryptoMarket {
    CryptoMarket {
        asset: "ETH".into(),
        condition_id: "cond-eth".into(),
        question_id: "q-eth".into(),
        up_token_id: "eth-up".into(),
        down_token_id: "eth-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / 900,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: String::new(),
        neg_risk: true,
        question: "ETH up or down".into(),
    }
}

#[test]
fn a_same_slot_round_feed_merges_without_a_rollover() {
    let rounds = Arc::new(Mutex::new(Vec::new()));
    let mut e = Engine::new(cfg());
    e.register_user_strategy(
        Box::new(RoundProbe {
            rounds: rounds.clone(),
        }),
        "test".into(),
    )
    .unwrap();
    let end = 1_800_000; // slot 2000
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(end)],
        now_ms: 1_000,
    });
    assert_eq!(*rounds.lock().unwrap(), vec![2]);

    // The venue lists the ETH market a minute into the round: a same-slot
    // feed. It must grow the scanner to 2 markets and NOT re-fire
    // on_round (the round never changed).
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(end), eth_market(end)],
        now_ms: 61_000,
    });
    assert_eq!(
        e.scanner.markets().len(),
        2,
        "the late listing joins the round's market list"
    );
    assert_eq!(
        *rounds.lock().unwrap(),
        vec![2],
        "a same-slot top-up is not a round transition"
    );

    // A pure duplicate (both markets already known) is inert.
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(end), eth_market(end)],
        now_ms: 62_000,
    });
    assert_eq!(e.scanner.markets().len(), 2);
    assert_eq!(*rounds.lock().unwrap(), vec![2]);
}

// ── 盘口批次原子化：Batch 应用路径 ─────────────────────────────────────

#[test]
fn a_same_slot_top_up_keeps_pending_tokens_and_the_clock() {
    let mut e = Engine::new(cfg());
    let end = 1_800_000;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(end)],
        now_ms: 1_000,
    });
    // Simulate an in-flight entry: the rollover path clears this set
    // (re-arming duplicate entries mid-round); a top-up must not.
    e.pending_tokens.insert((
        blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        "up".into(),
    ));
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(end), eth_market(end)],
        now_ms: 61_000,
    });
    assert!(
        e.pending_tokens.contains(&(
            blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            "up".into()
        )),
        "the top-up must not re-arm an entry that is already pending"
    );

    // The next real rollover still clears it.
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(2_700_000)],
        now_ms: 1_800_000,
    });
    assert!(
        !e.pending_tokens.contains(&(
            blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            "up".into()
        )),
        "a genuine rollover keeps its historic semantics"
    );
    assert_eq!(e.scanner.markets().len(), 1, "a new slot replaces the list");
}

fn venue_market(end_ms: i64, venue: &str) -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: format!("cond-{venue}"),
        question_id: format!("q-{venue}"),
        up_token_id: format!("{venue}-up"),
        down_token_id: format!("{venue}-down"),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / 900,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: true,
        question: "BTC up or down".into(),
    }
}

#[test]
fn two_venues_same_slot_merge_and_rollover_per_venue() {
    // Multi-plugin data plane: polymarket and kalshi both list slot 2
    // (staggered). Each feed is a top-up for ITS venue only; a different
    // slot from ONE venue rolls over only that venue's markets.
    let rounds = Arc::new(Mutex::new(Vec::new()));
    let mut e = Engine::new(cfg());
    e.register_user_strategy(
        Box::new(RoundProbe {
            rounds: rounds.clone(),
        }),
        "test".into(),
    )
    .unwrap();
    let end = 1_800_000; // slot 2 (1_800_000 ms / 900 s grid)
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![venue_market(end, "polymarket")],
        now_ms: 1_000,
    });
    assert_eq!(*rounds.lock().unwrap(), vec![2]);

    // Kalshi lists the same slot a minute later: it must JOIN the scanner
    // (top-up), and the polymarket markets must survive the merge.
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![venue_market(end, "kalshi")],
        now_ms: 61_000,
    });
    assert_eq!(
        e.scanner.markets().len(),
        2,
        "the second venue's same-slot feed is a merge, not a rollover"
    );
    assert!(
        e.scanner.markets().iter().any(|m| m.venue == "polymarket"),
        "the first venue's markets are not wiped by the sibling feed"
    );
    assert_eq!(
        *rounds.lock().unwrap(),
        vec![2],
        "a same-slot cross-venue feed is not a round transition"
    );
    // The merge path must ALSO observe the second venue's own expiry —
    // the merge comment's "the first feed already observed the round's
    // one expiry" is a single-venue assumption. In a mixed-venue table
    // the staggered top-up is usually the ONLY declaration the second
    // venue ever makes (its first feed hits the merge, never the
    // rollover below), so skipping the observation leaves it out of
    // `observed_venue_keys()` forever: no panel `venueRounds` row (the
    // countdown rebind waits for it and shows 00:00), and
    // `round_timing_for_venue` falls back to the sibling's clock.
    assert!(
        e.scanner()
            .observed_venue_keys()
            .contains(&"kalshi".to_string()),
        "the second venue's same-slot top-up must observe ITS OWN expiry \
         (its first feed never reaches the rollover path)"
    );

    // Kalshi lists the NEXT slot while polymarket still sits in slot 2:
    // kalshi's rollover fires on_round and replaces ITS OWN markets, while
    // the polymarket markets stay live in the table.
    let next_end = 2_700_000; // slot 3
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![venue_market(next_end, "kalshi")],
        now_ms: 1_800_000,
    });
    assert_eq!(
        *rounds.lock().unwrap(),
        vec![2, 3],
        "the single-venue slot change is a real rollover"
    );
    let venues: Vec<&str> = e
        .scanner
        .markets()
        .iter()
        .map(|m| m.venue.as_str())
        .collect();
    assert_eq!(
        venues,
        vec!["polymarket", "kalshi"],
        "the rollover replaces the feeding venue's list; the sibling survives"
    );
    // ...and the polymarket clock/expiry observation survives in its own
    // slot, reachable for the sibling's timing decisions.
    assert!(
        e.scanner()
            .round_timing_for_venue("polymarket", 1_000_001)
            .expires_at_ms
            > 0,
        "the sibling venue's clock slot survives the kalshi rollover"
    );
}

/// 2026-10-25 poly 轮钟冻结定谳（生产档案取证）：PolySimulator 镜像
/// Polymarket —— 同窗口 condition/token id 逐字节相同。PM 批先落表，
/// poly 批同 slot 到达时 merge 臂的 fresh 集为空 → 旧代码静默 return，
/// (polysimulator, 900) 轮钟永不落地（venueRounds 缺行、面板倒计时
/// 永远绑不上、切账户永久「加载中」）、poly 行也永远进不了表。
/// 修复语义：fresh 空的 merge 批次仍要 (a) 给自己的 (venue, duration)
/// 钟开口（observed 幂等检查挡真正的同所重复申报），(b) 把整批存进
/// 镜像显示簿，行情装配层据此合成带它标签的行情行。表保持 token 唯一
/// —— 镜像即同一市场，交易面一份行就够。
fn mirror_market(end_ms: i64, venue: &str) -> CryptoMarket {
    // Same tokens for both venues — the mirror property itself.
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond-mirror".into(),
        question_id: "q-mirror".into(),
        up_token_id: "mirror-up".into(),
        down_token_id: "mirror-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / 900,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: false,
        question: "BTC up or down".into(),
    }
}

#[test]
fn a_mirror_venues_same_slot_batch_lands_its_own_dual_key_rows() {
    // RFC-661 M1 取代（原断言：表保持 token 唯一 1 行 + 整批进 shadow
    // 显示簿 —— #627「token 全局唯一一份行」不变式被 §4.3 双键正面废
    // 除）：同字节 token 在两个 venue 下是两个独立标的。PM 批先落，
    // poly 批同 slot 到达时按 (venue, token) 双键各落各的行 —— 插入
    // 次序无关；M3 拆壳后双键表是唯一市场存储，没有「被遮蔽所」。
    let mut e = Engine::new(cfg());
    let end = 1_800_000; // slot 2
    // Polymarket lands first: a rollover puts its row in the table and
    // speaks its clock.
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(end, "polymarket")],
        now_ms: 1_000,
    });
    assert_eq!(e.scanner.markets().len(), 1);

    // The mirror venue declares the SAME round — byte-identical tokens,
    // a different venue tag. Its batch is a same-slot merge whose fresh
    // set (venue-qualified) is ITS OWN tokens — the rows land.
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(end, "polysimulator")],
        now_ms: 61_000,
    });
    // Dual-key: TWO rows, one per venue, both in the table.
    assert_eq!(
        e.scanner.markets().len(),
        2,
        "the same-byte token under two venues is two instruments — each venue's row lands"
    );
    assert!(
        e.scanner
            .market_for_token_on_venue("polymarket", "mirror-up")
            .is_some(),
        "the first venue keeps its own row"
    );
    assert!(
        e.scanner
            .market_for_token_on_venue("polysimulator", "mirror-up")
            .is_some(),
        "the second venue gains its own row — insertion order irrelevant"
    );
    // ...and the second venue's clock MUST have spoken: the panel's
    // venueRounds row and the countdown rebind wait for it.
    assert!(
        e.scanner()
            .observed_clock_keys()
            .iter()
            .any(|(v, d)| v == "polysimulator" && *d == 900),
        "the second venue's same-slot batch must open its own (venue, duration) clock"
    );
    // A genuine duplicate re-feed of the SAME venue stays idempotent —
    // the same-venue merge semantics survive verbatim: no third row, no
    // churn.
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(end, "polysimulator")],
        now_ms: 91_000,
    });
    assert_eq!(
        e.scanner.markets().len(),
        2,
        "same-venue re-feed adds nothing"
    );

    // The next window: PM rolls over (its rows move), the mirror venue's
    // new-window batch merges ITS OWN rows — both venues track the live
    // window, each on its own (venue, duration) clock.
    let next_end = 2_700_000; // slot 3
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(next_end, "polymarket")],
        now_ms: 1_800_000,
    });
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(next_end, "polysimulator")],
        now_ms: 1_800_001,
    });
    assert_eq!(
        e.scanner.markets().len(),
        2,
        "both venues' rows stay in the table"
    );
    assert!(
        e.scanner
            .markets()
            .iter()
            .all(|m| m.round_slot == next_end / 1000 / 900),
        "the table tracks the live window for BOTH venues"
    );
    assert_eq!(
        e.scanner
            .round_timing_for("polysimulator", 900, 1_800_001)
            .expires_at_ms,
        next_end,
        "the second venue's clock follows its own new-window declaration"
    );
    assert_eq!(
        e.scanner
            .round_timing_for("polymarket", 900, 1_800_001)
            .expires_at_ms,
        next_end,
        "the first venue's clock follows its own declaration, independent"
    );
}

#[test]
fn a_shared_token_is_ambiguous_to_the_token_only_lookup() {
    // RFC-661 M1 取代（原断言：token-only 查找首中先到的 PM 行 ——
    // 「first venue wins」正是 §五.5 钉死废除的插入序解析）：双键表下
    // 两个 venue 各持同字节 token 的行，token-only 查找遇到歧义必须
    // 拒绝（None），绝不按插入顺序首中；venue 限定查找各得各行。
    let mut e = Engine::new(cfg());
    let end = 1_800_000;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(end, "polymarket")],
        now_ms: 1_000,
    });
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(end, "polysimulator")],
        now_ms: 61_000,
    });
    assert!(
        e.scanner.market_for_token("mirror-up").is_none(),
        "two venues hold the token — the token-only lookup refuses the ambiguity"
    );
    assert_eq!(
        e.venues_holding_token("mirror-up"),
        vec!["polymarket".to_string(), "polysimulator".to_string()],
        "the ambiguity diagnostic names BOTH candidate venues (sorted)"
    );
    assert_eq!(
        e.venue_of("mirror-up"),
        None,
        "venue_of refuses the ambiguous token, never first-hit"
    );
    // Each venue-qualified lookup resolves to its OWN row.
    assert_eq!(
        e.scanner
            .market_for_token_on_venue("polymarket", "mirror-up")
            .map(|m| m.venue.as_str()),
        Some("polymarket"),
    );
    assert_eq!(
        e.scanner
            .market_for_token_on_venue("polysimulator", "mirror-up")
            .map(|m| m.venue.as_str()),
        Some("polysimulator"),
    );
}

#[test]
fn a_venue_that_missed_a_full_round_loses_its_stale_rows_at_the_next_rollover() {
    // #550 复现缺陷（症状①）的根因之一：换轮臂只替换 FEEDING venue 的
    // 行，兄弟行「存活到自己的 feed 来换」。存活是 staggered 上架的
    // 桥接语义（兄弟在轮界刚过、自己还没重报的几秒里不能丢行），
    // 但没有边界 —— 兄弟停喂（插件死/凭证失效）后，它的行带着已过期
    // 的 expiry 永久留在表里：行情页渲染已结束轮次的行，它的
    // observed expiry 还通过全局轮钟的 earliest_observed 把
    // time_left 钉死 0（全体新开仓被 Timing 门拦死）。
    //
    // 裁决语义：错满一整轮（slot 距离 ≥ 2）的兄弟行在下一次换轮时
    // 清出表；只差一格（刚过轮界还没重报）的兄弟行照旧桥接 ——
    // `two_venues_same_slot_merge_and_rollover_per_venue` 钉死的
    // 「sibling survives」在它的时序（slot 距离 1）下原样成立。
    let mut e = Engine::new(cfg());
    let end = 1_800_000; // slot 2
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![venue_market(end, "polymarket")],
        now_ms: 1_000,
    });
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![venue_market(end, "kalshi")],
        now_ms: 61_000,
    });
    assert_eq!(e.scanner.markets().len(), 2);

    // kalshi rolls to slot 3 at the exact slot boundary: polymarket has
    // NOT re-fed yet — one slot behind, the bridge window. Its row must
    // SURVIVE (the pinned staggered semantics).
    let next_end = 2_700_000; // slot 3
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![venue_market(next_end, "kalshi")],
        now_ms: 1_800_000,
    });
    assert!(
        e.scanner.markets().iter().any(|m| m.venue == "polymarket"),
        "a sibling one slot behind is inside the staggered bridge and survives"
    );

    // kalshi rolls again and polymarket still has not spoken (dead feed):
    // two slots behind = it missed a full round. Its finished-round row
    // must now leave the table.
    let end4 = 3_600_000; // slot 4
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![venue_market(end4, "kalshi")],
        now_ms: 2_700_000,
    });
    let venues: Vec<&str> = e
        .scanner
        .markets()
        .iter()
        .map(|m| m.venue.as_str())
        .collect();
    assert_eq!(
        venues,
        vec!["kalshi"],
        "a venue that missed a full round must not keep its finished-round row: {venues:?}"
    );
}

#[test]
fn a_shared_token_rolls_over_per_venue_without_cross_venue_pollution() {
    // RFC-661 §五.6（M1 半边）：同字节 token 双键行在场时，一个 venue
    // 的换轮（rollover）只替换它自己那一族的行 —— 兄弟 venue 的同
    // token 行是另一个标的，必须原样存活；换轮后两所各自的行各追各
    // 的新窗口，token-only 查找依旧歧义拒绝。
    let mut e = Engine::new(cfg());
    let end = 1_800_000; // slot 2
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(end, "polymarket")],
        now_ms: 1_000,
    });
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(end, "polysimulator")],
        now_ms: 61_000,
    });
    assert_eq!(e.scanner.markets().len(), 2, "both venues' rows live");

    // polymarket rolls to slot 3: only ITS row moves; the simulator's
    // slot-2 row (byte-identical tokens) must survive untouched.
    let next_end = 2_700_000; // slot 3
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(next_end, "polymarket")],
        now_ms: 1_800_000,
    });
    let pm = e
        .scanner
        .market_for_token_on_venue("polymarket", "mirror-up")
        .expect("the rolling venue keeps its own row");
    assert_eq!(
        pm.round_slot,
        next_end / 1000 / 900,
        "the rolling venue's row tracks the new window"
    );
    let sim = e
        .scanner
        .market_for_token_on_venue("polysimulator", "mirror-up")
        .expect("the sibling venue's row survives the sibling rollover");
    assert_eq!(
        sim.round_slot,
        end / 1000 / 900,
        "the sibling venue's row is NOT dragged along by the sibling rollover"
    );
    assert_eq!(
        e.scanner.markets().len(),
        2,
        "the rollover replaces the feeding venue's row, never the sibling's"
    );

    // The simulator rolls too (its feed was just late): both rows now
    // track the live window, each through its own feed.
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(next_end, "polysimulator")],
        now_ms: 1_800_001,
    });
    assert_eq!(
        e.scanner.markets().len(),
        2,
        "the late feed lands its own row"
    );
    assert!(
        e.scanner
            .markets()
            .iter()
            .all(|m| m.round_slot == next_end / 1000 / 900),
        "both venues track the live window on their own feeds"
    );
    // Ambiguity persists exactly as before — the token stays shared.
    assert!(
        e.scanner.market_for_token("mirror-up").is_none(),
        "the token-only lookup still refuses: two venues hold the token"
    );
    // The per-venue clocks each followed their own declaration.
    assert_eq!(
        e.scanner
            .round_timing_for("polymarket", 900, 1_800_001)
            .expires_at_ms,
        next_end,
    );
    assert_eq!(
        e.scanner
            .round_timing_for("polysimulator", 900, 1_800_001)
            .expires_at_ms,
        next_end,
    );
}

#[test]
fn the_reverse_landing_order_lands_the_same_dual_key_rows() {
    // RFC-661 §五.1 的对称半边：sim 批先落、PM 批后落，两所各得各的
    // 行 —— 双键表对插入次序完全无关（§五.1「先 PM 批后 sim 批、先
    // sim 批后 PM 批双向喂」验收的 B→A 向）。行内容、歧义拒绝、时钟
    // 开口与 A→B 向（`a_mirror_venues_same_slot_batch_lands_its_own_
    // dual_key_rows`）逐断言对称。
    let mut e = Engine::new(cfg());
    let end = 1_800_000; // slot 2
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(end, "polysimulator")],
        now_ms: 1_000,
    });
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(end, "polymarket")],
        now_ms: 61_000,
    });
    assert_eq!(
        e.scanner.markets().len(),
        2,
        "the same-byte token under two venues is two instruments, in either landing order"
    );
    assert!(
        e.scanner
            .market_for_token_on_venue("polysimulator", "mirror-up")
            .is_some(),
        "the FIRST venue keeps its own row"
    );
    assert!(
        e.scanner
            .market_for_token_on_venue("polymarket", "mirror-up")
            .is_some(),
        "the SECOND venue gains its own row — insertion order irrelevant"
    );
    assert!(
        e.scanner
            .observed_clock_keys()
            .iter()
            .any(|(v, d)| v == "polymarket" && *d == 900),
        "the second venue's same-slot batch opened its own (venue, duration) clock"
    );
    assert!(
        e.scanner.market_for_token("mirror-up").is_none(),
        "the token-only lookup refuses the ambiguity regardless of landing order"
    );
    assert_eq!(
        e.venue_of("mirror-up"),
        None,
        "venue_of never resolves to the first lander"
    );
    // The next window, same reverse order: both venues track it.
    let next_end = 2_700_000; // slot 3
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(next_end, "polysimulator")],
        now_ms: 1_800_000,
    });
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(next_end, "polymarket")],
        now_ms: 1_800_001,
    });
    assert_eq!(e.scanner.markets().len(), 2);
    assert!(
        e.scanner
            .markets()
            .iter()
            .all(|m| m.round_slot == next_end / 1000 / 900),
        "both venues' rows track the live window, reverse landing order included"
    );
}

#[test]
fn a_unique_candidate_resolves_every_token_only_read_ambiguity_refuses() {
    // RFC-661 §五.5 的读取面全表：token-only 的每个入口（market_for_
    // token、venue_of、partner_of、neg_risk_of、round_slot_of、
    // asset_of、direction_of、venue_of_token 的引擎侧）都走同一条
    // 「唯一候选才解析」规则 —— 唯一候选解析到该行自己的值，歧义
    // （≥2 所持行）全部拒绝，绝不按插入序首中。
    let mut e = Engine::new(cfg());
    let end = 1_800_000;
    // The unique candidate: only polymarket lists the token.
    let mut pm = venue_market(end, "polymarket");
    pm.up_token_id = "shared-up".into();
    pm.down_token_id = "shared-down".into();
    pm.neg_risk = true;
    e.scanner.set_markets(vec![pm.clone()]);
    assert_eq!(
        e.scanner
            .market_for_token("shared-up")
            .map(|m| m.venue.as_str()),
        Some("polymarket"),
        "a unique candidate resolves the token-only lookup"
    );
    assert_eq!(e.venue_of("shared-up").as_deref(), Some("polymarket"));
    assert_eq!(e.partner_of("shared-up").as_deref(), Some("shared-down"));
    assert_eq!(e.neg_risk_of("shared-up"), Some(true));
    assert_eq!(e.round_slot_of("shared-up"), Some(end / 1000 / 900));
    assert_eq!(e.asset_of("shared-up").as_deref(), Some("BTC"));
    assert_eq!(e.direction_of("shared-up"), Some(SignalDirection::Up));
    assert_eq!(
        e.direction_of("shared-down"),
        Some(SignalDirection::Down),
        "the partner side reads its own leg label"
    );

    // The ambiguity: a sibling venue now holds byte-identical rows.
    let mut sim = pm.clone();
    sim.venue = "polysimulator".into();
    sim.condition_id = "cond-sim".into();
    e.scanner.set_markets(vec![pm, sim]);
    assert!(e.scanner.market_for_token("shared-up").is_none());
    assert_eq!(e.venue_of("shared-up"), None);
    assert_eq!(e.partner_of("shared-up"), None);
    assert_eq!(e.neg_risk_of("shared-up"), None);
    assert_eq!(e.round_slot_of("shared-up"), None);
    assert_eq!(e.asset_of("shared-up"), None);
    assert_eq!(e.direction_of("shared-up"), None);
}

#[test]
fn a_legacy_untagged_row_still_admits_every_bound_scope_next_to_dual_key_rows() {
    // The pre-tag corpus case survives beside the dual-key rows: an
    // untagged market (venue "" — serde default) carries no venue
    // evidence a binding could disagree with, so it still admits every
    // bound scope, while the sibling venues' tagged rows keep their own
    // admission verdicts.
    let mut e = Engine::new(cfg());
    let end = 1_800_000;
    e.scanner.set_markets(vec![
        venue_market(end, "polymarket"),
        mirror_market(end, "polysimulator"), // shares no tokens with PM
        eth_market(end),                     // untagged legacy row
    ]);
    e.set_account_venue("sim", "polysimulator");
    assert!(
        e.scope_admits_token("sim", "mirror-up"),
        "the binding's own dual-key row admits"
    );
    assert!(
        e.scope_admits_token("sim", "eth-up"),
        "an untagged legacy row admits every bound scope, as before"
    );
    assert!(
        !e.scope_admits_token("sim", "polymarket-up"),
        "another venue's tagged row still refuses — the gate keeps its teeth"
    );
}

// ── B1c: multi-family rounds — the engine plane ────────────────────────

/// A market row carrying an EXPLICIT duration family: same shape as
/// [`venue_market`], but the row stamps its own `round_duration_sec` and
/// its slot is computed on ITS OWN grid (a 1h row's slot numbers and a
/// 15m row's slot numbers are drawn from different rulers).
fn family_market(end_ms: i64, venue: &str, duration_sec: i64) -> CryptoMarket {
    let asset = if duration_sec == 900 { "BTC" } else { "ETH" };
    CryptoMarket {
        asset: asset.into(),
        condition_id: format!("cond-{venue}-{duration_sec}"),
        question_id: format!("q-{venue}-{duration_sec}"),
        up_token_id: format!("{venue}-{duration_sec}-up"),
        down_token_id: format!("{venue}-{duration_sec}-down"),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end_ms,
        round_slot: end_ms / 1000 / duration_sec,
        round_duration_sec: duration_sec,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: true,
        question: "up or down".into(),
    }
}

#[test]
fn a_same_slot_number_from_another_family_is_a_rollover_not_a_merge() {
    // B1c merge discipline: slot equality alone is venue-blind AND
    // family-blind — the 900s grid's slot N and the 3600s grid's slot N
    // are two different wall-clock rounds (thousands of seconds apart).
    // A 1h feed whose slot NUMBER collides with the resident 15m slot
    // must take the rollover arm (replacing ITS family's rows), never
    // the merge arm (which would graft a 1h market into a live 15m
    // round and skip its on_round).
    let rounds = Arc::new(Mutex::new(Vec::new()));
    let mut e = Engine::new(cfg());
    e.register_user_strategy(
        Box::new(RoundProbe {
            rounds: rounds.clone(),
        }),
        "test".into(),
    )
    .unwrap();
    // The 15m family feeds first: slot 2 (1_800_000 ms / 900 s).
    let end15 = 1_800_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end15, "polymarket", 900)],
        now_ms: 1_000,
    });
    assert_eq!(*rounds.lock().unwrap(), vec![2]);

    // The 1h family feeds a market whose SLOT NUMBER is also 2 — the
    // 3600s grid's slot 2 covers [7200s, 10800s), hours away from the
    // 15m slot 2 ([1800s, 2700s)). It must NOT merge: it is a real
    // rollover of its own family.
    let end1h = 2 * 3600 * 1000; // 3600s-grid slot 2 covers [7200s, 10800s)
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end1h, "polymarket", 3600)],
        now_ms: 1_000,
    });
    assert_eq!(
        *rounds.lock().unwrap(),
        vec![2, 2],
        "the colliding slot NUMBER from another family is a real rollover \
         of that family — on_round refires"
    );
    assert_eq!(
        e.scanner.markets().len(),
        2,
        "the 1h row joins the table (its family is new); the 15m row stays"
    );
    assert!(
        e.scanner
            .markets()
            .iter()
            .any(|m| m.round_duration_sec == 900),
        "the 15m row survives the 1h family's rollover"
    );
}

#[test]
fn a_same_slot_same_family_feed_still_merges() {
    // The green half of the merge pair: within ONE family the slot
    // number is the round identity — a staggered top-up from the same
    // (venue, duration) family still merges (the #498 semantics the
    // duration check must not break).
    let rounds = Arc::new(Mutex::new(Vec::new()));
    let mut e = Engine::new(cfg());
    e.register_user_strategy(
        Box::new(RoundProbe {
            rounds: rounds.clone(),
        }),
        "test".into(),
    )
    .unwrap();
    let end15 = 1_800_000i64; // 900s-grid slot 2
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end15, "polymarket", 900)],
        now_ms: 1_000,
    });
    assert_eq!(*rounds.lock().unwrap(), vec![2]);
    // The SAME family's second asset, listed a minute into the round —
    // its own tokens (the top-up filter grafts by unknown token ids).
    let eth15 = CryptoMarket {
        asset: "ETH".into(),
        condition_id: "cond-poly-900-eth".into(),
        question_id: "q-poly-900-eth".into(),
        up_token_id: "polymarket-900-eth-up".into(),
        down_token_id: "polymarket-900-eth-down".into(),
        question: "ETH up or down".into(),
        ..family_market(end15, "polymarket", 900)
    };
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![eth15],
        now_ms: 61_000,
    });
    assert_eq!(
        e.scanner.markets().len(),
        2,
        "a same-family same-slot top-up merges"
    );
    assert_eq!(
        *rounds.lock().unwrap(),
        vec![2],
        "a same-family top-up is not a round transition"
    );
}

#[test]
fn family_a_rolling_over_does_not_reset_family_b_feeding_state() {
    // B1c rollover discipline: the feeding-state reset (pending tokens,
    // staged candidates) used to be global — ANY venue's rollover wiped
    // EVERY family's pending entries and re-armed duplicates mid-round.
    // The reset is now scoped to the feeding family: a 1h rollover must
    // leave a 15m token's pending latch and staged candidate untouched.
    let mut e = Engine::new(cfg());
    let end15 = 1_800_000i64; // 900s-grid slot 2
    let end1h = 2 * 3600 * 1000; // 3600s-grid slot 2 covers [7200s, 10800s)
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![
            family_market(end15, "polymarket", 900),
            family_market(end1h, "polymarket", 3600),
        ],
        now_ms: 1_000,
    });
    // An in-flight 15m entry (the state the old whole-clear wiped) plus
    // a staged 15m candidate (Issue #390's replay staging).
    e.pending_tokens.insert((
        blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        "polymarket-900-up".into(),
    ));
    e.last_candidates = vec![test_signal("polymarket-900-up")];
    e.candidates_stable = true;

    // The 1h family rolls over its next slot: the feeding family is
    // (polymarket, 3600) — the 15m family's feeding state must survive.
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end1h + 3_600_000, "polymarket", 3600)],
        now_ms: end1h,
    });
    assert!(
        e.pending_tokens.contains(&(
            blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            "polymarket-900-up".into()
        )),
        "family A's rollover must not re-arm family B's pending entry"
    );
    assert!(
        e.last_candidates
            .iter()
            .any(|sig| sig.token_id == "polymarket-900-up"),
        "family A's rollover must not retire family B's staged candidate"
    );
    assert!(
        e.candidates_stable,
        "the surviving staged set keeps its stability flag"
    );

    // The 15m family's OWN rollover still clears it — the historic
    // semantics, scoped.
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end15 + 900_000, "polymarket", 900)],
        now_ms: end15,
    });
    assert!(
        !e.pending_tokens.contains(&(
            blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            "polymarket-900-up".into()
        )),
        "the family's own rollover keeps its reset semantics"
    );
    assert!(
        !e.last_candidates
            .iter()
            .any(|sig| sig.token_id == "polymarket-900-up"),
        "the family's own rollover retires its staged candidates"
    );
    assert!(
        !e.candidates_stable,
        "an emptied staged set is not stable — the historic whole-clear verdict"
    );
}

#[test]
fn a_1h_rollover_does_not_reset_the_clock_of_the_15m_family() {
    // The clock half of the family-scoped rollover: the feeding batch's
    // expiry declaration lands in ITS (venue, duration) family slot and
    // the time_left handed to on_round comes from that family's clock —
    // a 1h rollover must never speak into the 15m family's clock (the
    // B1a keying the engine reset now aligns with).
    let mut e = Engine::new(cfg());
    let end15 = 1_800_000i64;
    let end1h = 2 * 3600 * 1000; // 3600s-grid slot 2 covers [7200s, 10800s)
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end15, "polymarket", 900)],
        now_ms: 1_000,
    });
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end1h, "polymarket", 3600)],
        now_ms: 1_000,
    });
    // Both families spoke; the 15m clock still reads ITS family's expiry.
    assert!(
        e.scanner()
            .observed_clock_keys()
            .contains(&("polymarket".to_string(), 900)),
        "the 15m family's declaration stays keyed (polymarket, 900)"
    );
    assert!(
        e.scanner()
            .observed_clock_keys()
            .contains(&("polymarket".to_string(), 3600)),
        "the 1h family's declaration lands in its own key"
    );
    assert_eq!(
        e.scanner()
            .round_timing_for("polymarket", 900, 1_000)
            .expires_at_ms,
        end15,
        "the 1h rollover never moves the 15m family's clock"
    );
}

#[test]
fn stale_rows_are_measured_on_their_own_duration_grid() {
    // #550 on multi-family tables: the "missed a full round" distance
    // used ONE global ruler (slot-number subtraction), so a 1h sibling's
    // slot N sat arbitrarily far from a 15m feed's slot N and its row
    // was judged by numbers from two different grids. Each row is now
    // measured against the feeding round's start CONVERTED onto the
    // row's own grid.
    let mut e = Engine::new(cfg());
    // A 1h row resident at 3600s-grid slot 2 (covers [7200s, 10800s)).
    let end1h = 2 * 3600 * 1000; // 3600s-grid slot 2 covers [7200s, 10800s)
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(end1h, "polymarket", 3600)],
        now_ms: 1_000,
    });
    // A 15m feed deep into the 1h round — 900s-grid slot 10 starts at
    // 9000s, still INSIDE the 1h slot-2 round (which runs to 10800s).
    // On each row's own grid: the 1h row's lag = (10 × 900 / 3600) − 2
    // = 2 − 2 = 0 — well inside the bridge, the row MUST survive.
    let feed15 = 10 * 900 * 1000;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(feed15, "kalshi", 900)],
        now_ms: 9_000_000,
    });
    assert!(
        e.scanner
            .markets()
            .iter()
            .any(|m| m.round_duration_sec == 3600),
        "the 1h row is mid-round on ITS OWN grid and must not be judged \
         stale by 15m slot arithmetic"
    );
    assert!(
        e.scanner
            .markets()
            .iter()
            .any(|m| m.round_duration_sec == 900),
        "the feeding 15m row is in the table"
    );
}

#[test]
fn legacy_zero_duration_rows_keep_the_global_grid_verdict() {
    // Byte-identical duty: rows and feeds that declare NO duration (the
    // pre-B1a corpus, `round_duration_sec == 0`) resolve to the global
    // grid, so the family-scoped stale-row arithmetic degenerates to the
    // historic `slot - round_slot < 2` for every row — the existing
    // corpus replays unchanged.
    let mut e = Engine::new(cfg());
    let mut row = venue_market(1_800_000, "kalshi");
    row.round_duration_sec = 0; // the legacy shape
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![row],
        now_ms: 1_000,
    });
    let mut late = venue_market(1_800_000, "polymarket");
    late.round_duration_sec = 0;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![late], // same slot: merges (the #498 bridge)
        now_ms: 61_000,
    });
    assert_eq!(
        e.scanner.markets().len(),
        2,
        "zero-duration rows merge on the global grid, exactly as before"
    );
    // Two slots behind → dropped, the #550 boundary unchanged: the ghost
    // row sits in slot 2 and the next feed (polymarket, a DIFFERENT
    // venue) rolls to slot 4 — the ghost missed a full round and its
    // zero-duration rows leave the table on the global grid, exactly as
    // the pre-B1c #550 verdict did.
    let mut dead = venue_market(1_800_000, "ghost");
    dead.round_duration_sec = 0;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![dead],
        now_ms: 1_000,
    });
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![venue_market(3_600_000, "polymarket")], // slot 4
        now_ms: 2_700_000,
    });
    assert!(
        !e.scanner.markets().iter().any(|m| m.venue == "ghost"),
        "the global-grid verdict for zero-duration rows is untouched"
    );
    assert!(
        e.scanner.markets().iter().any(|m| m.venue == "polymarket"),
        "the feeding venue's own rows survive, as before"
    );
}

#[test]
fn one_venue_expiring_does_not_freeze_the_other_venue() {
    // Both venues observed their (identical-grid) expiries. At a clock
    // inside polymarket's force-exit window, a polymarket candidate is
    // refused while a kalshi candidate (same grid, later observed expiry)
    // is not — the per-market timing gate reads the market's OWN venue
    // clock instead of one table-wide verdict.
    let mut s = Scanner::new(ScannerConfig {
        round_duration_sec: 900,
        min_round_age_sec: 0,
        min_time_left_sec: 180,
        ..Default::default()
    });
    let poly_end = 1_800_000i64;
    let kalshi_end = 1_950_000i64; // a later observed close, in-band
    s.observe_end_time_for_venue("polymarket", poly_end, 900_000);
    s.observe_end_time_for_venue("kalshi", kalshi_end, 900_000);
    let poly = venue_market(poly_end, "polymarket");
    let kalshi = venue_market(kalshi_end, "kalshi");
    s.set_markets(vec![poly.clone(), kalshi.clone()]);

    // t = 1_630_000: 170s to the POLYMARKET close (< 180 → blocked), 320s
    // to the KALSHI close (≥ 180 → ok).
    let now = 1_630_000i64;
    assert_eq!(
        s.can_trade_reason_for(&poly, now),
        Err(TimingBlock::TooCloseToExpiry {
            time_left_sec: 170,
            min_time_left_sec: 180
        }),
        "the polymarket market is inside its own force-exit window"
    );
    assert!(
        s.can_trade_reason_for(&kalshi, now).is_ok(),
        "the kalshi market must NOT be frozen by the polymarket window"
    );
    // The whole-table verdict stays conservative (earliest expiry).
    assert!(
        s.can_trade_reason(now).is_err(),
        "the global window keeps the earliest-expiry semantics"
    );
}

#[test]
fn a_venue_bound_scope_admits_untagged_markets() {
    // The exit-economics regression: a factory account book binds default
    // to a venue, the corpus predates #427's venue tagging (serde default
    // = empty), and `scope_admits_token` refused EVERY token — signals
    // 89975 → 0, closes 13 → 0 across all four windows. An untagged
    // market says nothing the binding could disagree with (the single-
    // plugin world's venue was implicit), so it admits every scope; the
    // WRONG tagged venue still refuses, which is the gate's actual job.
    let mut e = Engine::new(cfg());
    let end = 1_800_000i64;
    e.scanner.set_markets(vec![
        venue_market(end, "polymarket"),
        venue_market(end, "kalshi"),
        eth_market(end), // untagged (serde default), like old corpora
    ]);
    e.set_account_venue("default", "polymarket");

    assert!(
        e.scope_admits_token("default", "polymarket-up"),
        "the bound venue's own tagged market admits"
    );
    assert!(
        e.scope_admits_token("default", "eth-up"),
        "an UNTAGGED market admits a bound scope — the pre-tag corpus case"
    );
    assert!(
        !e.scope_admits_token("default", "kalshi-up"),
        "another venue's tagged market still refuses — the gate keeps its teeth"
    );
    assert!(
        !e.scope_admits_token("default", "no-such-token"),
        "an unknown token (no market) refuses, as before"
    );
    assert!(
        e.scope_admits_token("unbound", "kalshi-up"),
        "an unbound scope admits everything, as before"
    );
}

#[test]
fn a_shared_tokens_dual_key_rows_admit_each_own_binding_without_cross_talk() {
    // The polysimulator account's zero-signal root cause, and how the
    // dual-key table dissolves it WITHOUT any mirror fact: the table used
    // to be token-unique and the PM batch landed first, so a
    // polysimulator-bound scope looked the row up, saw "another venue",
    // and silently dropped every candidate (signals=0, blocked=0). Under
    // RFC-661 M1 each venue holds its OWN (venue, token) row — the sim
    // row admits its own binding, the PM row its own, and the two never
    // cross. No declaration table is consulted (M3 retired it).
    let mut e = Engine::new(cfg());
    let end = 1_800_000;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(end, "polymarket")],
        now_ms: 1_000,
    });
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![mirror_market(end, "polysimulator")],
        now_ms: 61_000,
    });
    e.set_account_venue("poly", "polysimulator");
    e.set_account_venue("pm", "polymarket");

    assert!(
        e.scope_admits_token("poly", "mirror-up"),
        "the sim binding admits through the sim venue's OWN dual-key row"
    );
    assert!(
        e.scope_admits_token("pm", "mirror-up"),
        "the PM binding admits through the PM venue's own row, as before"
    );
    assert!(
        !e.scope_admits_token("poly", "polymarket-up"),
        "a NON-shared token of another venue still refuses — the gate keeps its teeth"
    );
    assert!(
        e.execution_venue_for("poly", "mirror-up") == Some("polysimulator".into()),
        "execution follows the placing account's binding, not the table row"
    );
    assert!(
        e.execution_venue_for("pm", "mirror-up") == Some("polymarket".into()),
        "the mirrored venue's binding reads its own row, unchanged"
    );
    // RFC-661 §五.5: with BOTH venues holding the byte-identical token,
    // the unbound account's token-only resolution REFUSES — the old
    // first-hit answer ("polymarket", the row that landed first) is the
    // insertion-order semantics the dual-key table supersedes.
    assert_eq!(
        e.execution_venue_for("unbound", "mirror-up"),
        None,
        "an ambiguous token resolves for NO unbound account — never first-hit by landing order"
    );

    // The unique-candidate half: drop the second venue's rows (its feed
    // dies) and the surviving polymarket row is the ONLY holder — the
    // token-only lookup resolves again, to the same venue the old table
    // happened to answer by landing order.
    let kept: Vec<CryptoMarket> = e
        .scanner
        .markets()
        .iter()
        .filter(|m| m.venue != "polysimulator")
        .cloned()
        .collect();
    e.scanner.set_markets(kept);
    assert_eq!(
        e.execution_venue_for("unbound", "mirror-up"),
        Some("polymarket".into()),
        "a UNIQUE candidate resolves the token-only lookup for an unbound account"
    );
}

// ── E3 (轮次多族收尾): strategy-visible round clocks follow the market's
//    own declared family ────────────────────────────────────────────────

#[test]
fn each_family_reads_its_own_clock_in_the_gate_trail() {
    // The audit's E3 finding: the strategy-visible round clock (the D-31
    // waiver line, the near-miss/block telemetry, the emitted order's
    // slot) read the GLOBAL grid, so a 1h market's candidate was judged
    // by 15m numbers. Two families, one venue, one evaluation: the 15m
    // candidate is inside ITS force-exit window (172s left on the 900s
    // grid) while the 1h candidate has 3772s left on ITS grid — the same
    // `timing: true` declaration must block one and waive the other,
    // each on its OWN family clock.
    //
    // Fixture arithmetic (cfg grid 900s, min_time_left 180s):
    //   feed 1 @ 7_000_000ms: pm/900 row, expiry 7_200_000 (900s slot 7's
    //   grid end; offset[pm] ← 0). Feed 2 @ 7_028_000ms: pm/3600 row,
    //   expiry 10_800_000 (offset[pm] ← 3_600_000, the 1h schedule).
    //   Legacy-bucket clocks (offsetless "") at 7_028_000:
    //     900s family → slot 7, effective earliest same-duration
    //     declaration 7_200_000 → time_left 172 < 180 (BLOCK);
    //     3600s family → slot 1, effective 10_800_000 → time_left 3_772
    //     ≥ 180 (WAIVE). The pre-E3 code read the global clock for both
    //     waivers: 172 < 180 → BOTH blocked, zero orders.
    let mut cfg = cfg();
    cfg.scanner.min_time_left_sec = 180;
    let mut e = Engine::new(cfg);
    e.register_user_strategy(
        Box::new(DipBuyer::exempting(
            "btc_fade",
            dec!(0.45),
            &["BTC"],
            GateExemptions {
                timing: true,
                momentum: false,
                timing_min_time_left_sec: None,
            },
        )),
        "test".into(),
    )
    .unwrap();
    e.register_user_strategy(
        Box::new(DipBuyer::exempting(
            "eth_fade",
            dec!(0.45),
            &["ETH"],
            GateExemptions {
                timing: true,
                momentum: false,
                timing_min_time_left_sec: None,
            },
        )),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("btc_fade", true));
    assert!(e.set_strategy_enabled("eth_fade", true));

    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(7_200_000, "polymarket", 900)],
        now_ms: 7_000_000,
    });
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![family_market(10_800_000, "polymarket", 3600)],
        now_ms: 7_028_000,
    });
    dip_only(&mut e, "polymarket-900-up", 7_028_000);
    dip_only(&mut e, "polymarket-3600-up", 7_028_000);

    let orders = e.evaluate(7_028_000);
    // The 1h candidate enters (its family says 3772s left); the 15m one
    // is blocked (its family says 172s left).
    assert_eq!(orders.len(), 1, "{orders:?}");
    assert_eq!(orders[0].strategy, "eth_fade");
    assert_eq!(
        orders[0].round_slot, 1,
        "the emitted order stamps the 3600s FAMILY's slot, not the global grid's"
    );
    assert_eq!(
        orders[0].internal_key, "eth_fade:ETH:up:1",
        "the internal key's round segment is the candidate family's slot"
    );
    let blocked = e.last_blocked();
    assert_eq!(blocked.len(), 1, "{blocked:?}");
    assert_eq!(blocked[0].strategy, "btc_fade");
    assert_eq!(blocked[0].reason, BlockReason::Timing);
    assert_eq!(
        blocked[0].time_left_sec, 172,
        "the near-miss/block telemetry reads the 900s family's time_left"
    );
    // The audit trail: the honoured waiver is the 1h candidate's, and its
    // `time_left_sec` is the FAMILY's number (3772s), not the global 172
    // that used to deny the waiver.
    let ex = e.last_exemptions();
    assert_eq!(ex.len(), 1, "{ex:?}");
    assert_eq!(ex[0].strategy, "eth_fade");
    assert_eq!(ex[0].gate, "timing");
    assert_eq!(ex[0].time_left_sec, 3_772);
    // E25 projection: the cycle's table-family clock (first row = the
    // 900s family on the legacy bucket).
    assert_eq!(e.last_time_left_sec(), 172);
}

#[test]
fn single_family_tables_read_the_global_numbers_verbatim() {
    // Byte-identical duty (the replay-baseline red line): every corpus is
    // single-family, where `row_family_duration` resolves EVERY row to the
    // configured grid and the legacy-bucket read degenerates to the
    // global `round_timing()` — so the waiver record, the emitted
    // order's slot/key and the E25 projection must equal the scanner's
    // global clock exactly. A zero-duration row (the pre-B1a corpus
    // shape) resolves the same way.
    for declared in [900i64, 0] {
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000; // force the TooYoung block
        cfg.scanner.min_time_left_sec = 180;
        let mut e = Engine::new(cfg);
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "fade",
                dec!(0.45),
                &["BTC"],
                GateExemptions {
                    timing: true,
                    momentum: false,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("fade", true));
        let mut row = market(1_800_000);
        row.round_duration_sec = declared;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![row],
            now_ms: 1_400_000,
        });
        dip_only(&mut e, "up", 1_500_000);

        let orders = e.evaluate(1_500_000);
        // The scanner's global clock — the exact numbers the pre-E3 code
        // fed every one of these sites.
        let global = e.scanner().round_timing(1_500_000);
        assert_eq!(orders.len(), 1, "declared {declared}: {orders:?}");
        assert_eq!(orders[0].round_slot, global.slot);
        assert_eq!(
            orders[0].internal_key,
            format!("fade:BTC:up:{}", global.slot)
        );
        let ex = e.last_exemptions();
        assert_eq!(ex.len(), 1, "declared {declared}: {ex:?}");
        assert_eq!(ex[0].gate, "timing");
        assert_eq!(
            ex[0].time_left_sec, global.time_left_sec,
            "declared {declared}: the waiver line reads the global clock's time_left"
        );
        assert_eq!(e.last_time_left_sec(), global.time_left_sec);
    }
}
