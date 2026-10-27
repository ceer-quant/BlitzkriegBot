#![cfg(test)]

use super::*;
// Order-book batch atomicity: a Batch lands every token at one
// consistent timestamp (RFC-era regression teeth), single-book and
// top-of-book paths keep event timestamps.
// ── RFC-645 A-1（决议③）：pair-book 统计面的声明受理谓词 ─────────────
// 三条腿各一枚钉：声明 + atomic_batch + PairBook 在册条目，缺一即不受理。

/// A declarable probe: flags settable per test leg.
struct PairStatsProbe {
    probe_name: &'static str,
    declares: bool,
    atomic: bool,
}
impl EngineStrategy for PairStatsProbe {
    fn name(&self) -> &str {
        self.probe_name
    }
    fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, _slot: i64, _left: i64, _now: i64) {}
    fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<crate::signal::TradeSignal> {
        Vec::new()
    }
    fn consumes_pair_book_stats(&self) -> bool {
        self.declares
    }
    fn atomic_batch(&self) -> bool {
        self.atomic
    }
}

/// A PairBook with one live (non-Closed) entry under `owner`.
fn pair_book_with_live_entry(owner: &str) -> crate::pair::PairBook {
    use rust_decimal_macros::dec;
    let mut pb = crate::pair::PairBook::default();
    let facts = crate::pair::TokenMarketFacts {
        asset: "BTC".into(),
        slot: 42,
    };
    let decl = crate::pair::PairLegDecl {
        token_id: "up".into(),
        shares: dec!(10),
        leg: "up".into(),
        leg_account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
    };
    pb.declare(
        owner,
        blitzkrieg_market_api::DEFAULT_ACCOUNT_ID,
        "pair-key",
        &decl,
        &facts,
        1_000,
    );
    pb
}

/// 全三腿齐备：声明 + atomic_batch + PairBook 在册条目 ⇒ 受理。
#[test]
fn pair_book_stats_accepted_when_all_three_legs_hold() {
    let mut e = Engine::new(cfg());
    e.register_user_strategy(
        Box::new(PairStatsProbe {
            probe_name: "pair_consumer",
            declares: true,
            atomic: true,
        }),
        "test".into(),
    )
    .expect("probe registers");
    let pb = pair_book_with_live_entry("pair_consumer");
    assert!(
        e.pair_book_stats_accepted("pair_consumer", &pb),
        "declaration + atomic_batch + a live PairBook entry = accepted"
    );
    // The rejection reason is None exactly when accepted (declared).
    assert_eq!(
        e.pair_book_stats_rejection_reason("pair_consumer", &pb),
        None
    );
}

/// 缺声明（trait 默认 false）：不受理 —— 任何既有策略的默认形状逐位不变。
#[test]
fn pair_book_stats_refused_without_the_declaration() {
    let mut e = Engine::new(cfg());
    e.register_user_strategy(
        Box::new(PairStatsProbe {
            probe_name: "undeclared",
            declares: false,
            atomic: true,
        }),
        "test".into(),
    )
    .expect("probe registers");
    let pb = pair_book_with_live_entry("undeclared");
    assert!(
        !e.pair_book_stats_accepted("undeclared", &pb),
        "no declaration = not accepted, even with atomic_batch and a live entry"
    );
    // Not declared ⇒ NOT a rejection: the silent default, no warn reason.
    assert_eq!(e.pair_book_stats_rejection_reason("undeclared", &pb), None);
}

/// 缺 atomic_batch：声明在场却不受理（fail-closed），并给出可读原因。
#[test]
fn pair_book_stats_refused_when_atomic_batch_missing() {
    let mut e = Engine::new(cfg());
    e.register_user_strategy(
        Box::new(PairStatsProbe {
            probe_name: "no_atomic",
            declares: true,
            atomic: false,
        }),
        "test".into(),
    )
    .expect("probe registers");
    let pb = pair_book_with_live_entry("no_atomic");
    assert!(
        !e.pair_book_stats_accepted("no_atomic", &pb),
        "a declaration without atomic_batch is NOT accepted"
    );
    assert_eq!(
        e.pair_book_stats_rejection_reason("no_atomic", &pb),
        Some("atomic_batch not declared"),
        "the warn names the unmet leg"
    );
}

/// 缺 PairBook 在册条目：声明在场却不受理（fail-closed），并给出可读原因。
#[test]
fn pair_book_stats_refused_without_a_live_pair_book_entry() {
    let mut e = Engine::new(cfg());
    e.register_user_strategy(
        Box::new(PairStatsProbe {
            probe_name: "no_entries",
            declares: true,
            atomic: true,
        }),
        "test".into(),
    )
    .expect("probe registers");
    // Empty book: the strategy has never produced a pair.
    let pb = crate::pair::PairBook::default();
    assert!(
        !e.pair_book_stats_accepted("no_entries", &pb),
        "a declaration without a live PairBook entry is NOT accepted"
    );
    assert_eq!(
        e.pair_book_stats_rejection_reason("no_entries", &pb),
        Some("no live PairBook entry under this strategy"),
        "the warn names the unmet leg"
    );
    // A Closed entry is NOT a live entry: the retired history does not
    // re-arm the surface.
    let closed_book = pair_book_with_live_entry("no_entries");
    // force-close through retire_before (every non-Closed slot < before).
    let mut closed_book = closed_book;
    closed_book.retire_before(i64::MAX);
    assert!(
        !e.pair_book_stats_accepted("no_entries", &closed_book),
        "a Closed entry is history, not a live pairing"
    );
}

/// 红测试（修复前）：一个 up+down 批次以两个单事件喂入，策略在第二条
/// on_book 途中读到的 up 是新批次、down 还是上一批 —— 一对不存在的腿间
/// 折价。修复后 Batch 路径一次应用整批：第二条 on_book 读到的 down 已经
/// 是同一批（时间戳一致的）快照，两条观测的时间戳必然相同。
///
/// 反过来单事件路径保持原语义：两条逐个喂入的 Book 观测时间戳不同
/// （它们本来就是两个独立事件），行为逐位不变。
#[test]
fn a_batch_of_books_is_observed_at_one_consistent_timestamp() {
    let (mut e, seen) = batch_engine();
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![two_leg_market(1_800_000)],
        now_ms: 1_000,
    });

    // 上一批（老价格，都是 0.40 的 bid）。
    e.on_data(DataEvent::Books {
        books: vec![
            (
                "up".into(),
                vec![(dec!(0.40), dec!(10))],
                vec![(dec!(0.60), dec!(10))],
            ),
            (
                "down".into(),
                vec![(dec!(0.40), dec!(10))],
                vec![(dec!(0.60), dec!(10))],
            ),
        ],
        now_ms: 10_000,
    });
    seen.lock().unwrap().clear();

    // 新批次：up 涨到 0.50，down 涨到 0.55 —— 同一个批次时间戳 20_000。
    e.on_data(DataEvent::Books {
        books: vec![
            (
                "up".into(),
                vec![(dec!(0.50), dec!(10))],
                vec![(dec!(0.60), dec!(10))],
            ),
            (
                "down".into(),
                vec![(dec!(0.55), dec!(10))],
                vec![(dec!(0.60), dec!(10))],
            ),
        ],
        now_ms: 20_000,
    });

    let obs = seen.lock().unwrap().clone();
    assert_eq!(
        obs.len(),
        2,
        "the batch seeds every strategy once per token: {obs:?}"
    );
    assert!(
        obs.iter().all(|(_, ts, _)| *ts == 20_000),
        "every token of one batch must be observed under the SAME batch \
         timestamp — a torn half-old/half-new read is exactly what this \
         forbids: {obs:?}"
    );
    let up = obs
        .iter()
        .find(|(t, _, _)| *t == "up")
        .expect("up observed");
    let down = obs
        .iter()
        .find(|(t, _, _)| *t == "down")
        .expect("down observed");
    assert_eq!(up.2, 50, "the up leg carries its NEW batch price; {obs:?}");
    assert_eq!(
        down.2, 55,
        "the down leg carries its NEW batch price in the same apply; {obs:?}"
    );
}

/// 撕裂窗口本体的守卫（变异验证的载体）：把「整批一次应用」退化为
/// 「逐 token 独立事件应用」后，批次第二条 on_book 途中 up 已是 20_000
/// 的新价、down 还停在 10_000 的旧价 —— 观测时间戳必然分裂。本测试
/// 断言这个分裂不存在（绿色），变异后它变红。
#[test]
fn a_degenerate_single_token_batch_still_carries_one_timestamp() {
    let (mut e, seen) = batch_engine();
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![two_leg_market(1_800_000)],
        now_ms: 1_000,
    });
    // 同一批 ts 的两个 token 拆成两个 Batch 事件（退化批次）：每次应用
    // 仍各带自己的批次时间戳，一次只动一本 —— 喂入途中不出现跨 token
    // 的一致性假象，也不出现「一个事件两个时间戳」的缝合。
    e.on_data(DataEvent::Books {
        books: vec![(
            "up".into(),
            vec![(dec!(0.50), dec!(10))],
            vec![(dec!(0.60), dec!(10))],
        )],
        now_ms: 20_000,
    });
    e.on_data(DataEvent::Books {
        books: vec![(
            "down".into(),
            vec![(dec!(0.55), dec!(10))],
            vec![(dec!(0.60), dec!(10))],
        )],
        now_ms: 20_000,
    });
    let obs = seen.lock().unwrap().clone();
    assert_eq!(obs.len(), 2);
    assert!(
        obs.iter().all(|(_, ts, _)| *ts == 20_000),
        "each single-token apply stamps its own batch ts: {obs:?}"
    );
}

/// 空 Batch 是显式无效输入：不发任何 on_book，也不碰任何状态。
#[test]
fn an_empty_batch_is_inert() {
    let (mut e, seen) = batch_engine();
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![two_leg_market(1_800_000)],
        now_ms: 1_000,
    });
    let before = e.books.len();
    e.on_data(DataEvent::Books {
        books: Vec::new(),
        now_ms: 30_000,
    });
    assert!(seen.lock().unwrap().is_empty(), "no observation fired");
    assert_eq!(e.books.len(), before, "no book state touched");
}

/// 单事件旧路径守卫：Book 的 timestamp 就是事件自己的 now_ms，逐位不变
/// —— 回放语料（一个 book 一个事件）的基线语义由这里钉住。
#[test]
fn the_single_book_path_keeps_its_event_timestamp() {
    let (mut e, seen) = batch_engine();
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![two_leg_market(1_800_000)],
        now_ms: 1_000,
    });
    e.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.42), dec!(10))],
        asks: vec![(dec!(0.58), dec!(10))],
        now_ms: 11_111,
    });
    let obs = seen.lock().unwrap().clone();
    assert_eq!(obs.len(), 1);
    assert_eq!(
        obs[0].1, 11_111,
        "the single-event path stamps the event's own now_ms, byte-identical"
    );
}

/// 单事件 TopOfBook 旧路径守卫（同上，回放基线的另一半）。
#[test]
fn the_single_top_of_book_path_keeps_its_event_timestamp() {
    let (mut e, seen) = batch_engine();
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![two_leg_market(1_800_000)],
        now_ms: 1_000,
    });
    e.on_data(DataEvent::TopOfBook {
        token_id: "up".into(),
        best_bid: Some(dec!(0.44)),
        best_ask: Some(dec!(0.56)),
        now_ms: 12_222,
    });
    let obs = seen.lock().unwrap().clone();
    assert_eq!(obs.len(), 1);
    assert_eq!(obs[0].1, 12_222);
}
