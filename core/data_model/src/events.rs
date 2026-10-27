//! The event taxonomy — moved VERBATIM from `blitzkrieg_core::engine`
//! (CashflowKind, BatchBook, DataEvent) and `blitzkrieg_core::data_source`
//! (event_at_ms, event_to_json). Zero behaviour change; the kernel re-exports
//! these under the original paths.

use blitzkrieg_market_api::Side;
use rust_decimal::Decimal;
use serde_json::{Value, json};

use crate::market::CryptoMarket;

/// The kind of a wallet-level cash flow (#388). The activity ledger pays them
/// as whole-ledger rows — never per fill — so they ride the stream as they
/// print and the ledger credits each row exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CashflowKind {
    /// MAKER_REBATE: the venue's maker-side rebate, paid per ledger row.
    Rebate,
    /// REWARD: a venue reward program payout (e.g. liquidity rewards).
    Reward,
}

impl CashflowKind {
    /// The archive's `"k":"cashflow"` `which` spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            CashflowKind::Rebate => "rebate",
            CashflowKind::Reward => "reward",
        }
    }

    /// Inverse of [`CashflowKind::as_str`]; `None` is a fail-closed parse
    /// error, never a silent default.
    pub fn parse_kind(s: &str) -> Option<Self> {
        match s {
            "rebate" => Some(CashflowKind::Rebate),
            "reward" => Some(CashflowKind::Reward),
            _ => None,
        }
    }
}

/// One book of a batch: `(token_id, bids, asks)` — the shape
/// [`DataEvent::Books`] and [`Engine::on_data_books`] both carry, named so the
/// signatures stay readable.
pub type BatchBook = (String, Vec<(Decimal, Decimal)>, Vec<(Decimal, Decimal)>);

/// Market-data events the engine consumes (produced by the feed layer or, in
/// tests, driven directly).
#[derive(Debug, Clone)]
pub enum DataEvent {
    /// Full or incremental orderbook update for a token.
    Book {
        token_id: String,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
        now_ms: i64,
    },
    /// 盘口批次原子化：一次 REST 批式拉取（如 polymarket `POST /books`）的
    /// 整批 book，`now_ms` 是这个批次的统一时间戳。整批必须一次应用 ——
    /// 逐 token 发射会在两次锁的间隙让策略读到「一半新一半旧」的撕裂状态
    /// （up 腿是本批、down 腿还是上一批，一对不存在的折价就是从这来的）。
    /// 批内按 `Vec` 顺序应用；每一本仍走与 [`DataEvent::Book`] 完全相同的
    /// apply/notify/kline 路径，只是共享一次锁与一个批次时间戳。
    Books { books: Vec<BatchBook>, now_ms: i64 },
    /// Top-of-book only update (price_change / best_bid_ask).
    TopOfBook {
        token_id: String,
        best_bid: Option<Decimal>,
        best_ask: Option<Decimal>,
        now_ms: i64,
    },
    /// Binance spot price for an asset (e.g. "BTC").
    Spot {
        asset: String,
        price: Decimal,
        now_ms: i64,
    },
    /// Fresh round markets discovered by the scanner.
    RoundMarkets {
        markets: Vec<CryptoMarket>,
        now_ms: i64,
    },
    /// #352: one on-chain print observed on a token — information only.
    /// The engine does not trade on it; replay consumers (e.g. an @almach
    /// benchmark) and the archive completeness counters do.
    Trade {
        token_id: String,
        side: Side,
        price: Decimal,
        size: Decimal,
        now_ms: i64,
    },
    /// #352: a round's markets resolved — the boundary event a replay uses to
    /// close the book on that round. Information only.
    RoundEnd { now_ms: i64 },
    /// #377 (治 #2): one market's on-chain verdict, recovered by the converter
    /// from the wallet's own redemption activity and replayed as a first-class
    /// event. The Core settles from it directly (`on_market_resolution`) —
    /// this is what closes the dry ladder's blind spot, where book evidence
    /// below the coin-flip withheld every settlement and stuck the legs. The
    /// engine itself trades on books and spot only; for it this is a marker.
    Resolution {
        condition_id: String,
        /// `(token_id, payout per share)` in the market's own outcome order —
        /// a binary boundary names BOTH sides explicitly: the winner pays 1,
        /// the loser 0. The core books what the stream says and nothing else.
        payouts: Vec<(String, Decimal)>,
        /// The market settles through the NegRisk adapter.
        neg_risk: bool,
        now_ms: i64,
    },
    /// #388: one wallet-level cash row from the activity ledger
    /// (MAKER_REBATE / REWARD) — money the venue paid the wallet OUTSIDE any
    /// fill, priced in whole-ledger rows. The core credits the active
    /// account's cash at the row's own timestamp; no order, position or
    /// settlement semantics touch it, so a corpus without these rows replays
    /// byte-identically to before.
    Cashflow {
        which: CashflowKind,
        /// The row's `usdcSize` — cash paid to the wallet, USD.
        amount_usd: Decimal,
        now_ms: i64,
    },
    /// #427: an operator-confirmed cross-venue mapping (`k:"unified"`), the
    /// #425 `UnifiedEvent` carried on the event stream. Pure state for the
    /// strategy layer: it tells a strategy WHICH tokens across venues are the
    /// same real-world bet, so a cross-venue view can be assembled from the
    /// venue-tagged books already flowing. The engine does not trade on it
    /// (no book update — same treatment as `Trade`/`Resolution`); a corpus
    /// without these rows replays byte-identically to before.
    UnifiedMapping {
        event: blitzkrieg_market_api::unified::UnifiedEvent,
        now_ms: i64,
    },
}

/// The event's own timestamp (ms).
pub fn event_at_ms(ev: &DataEvent) -> i64 {
    match ev {
        DataEvent::Book { now_ms, .. }
        | DataEvent::Books { now_ms, .. }
        | DataEvent::TopOfBook { now_ms, .. }
        | DataEvent::Spot { now_ms, .. }
        | DataEvent::RoundMarkets { now_ms, .. }
        | DataEvent::Trade { now_ms, .. }
        | DataEvent::Resolution { now_ms, .. }
        | DataEvent::Cashflow { now_ms, .. }
        | DataEvent::UnifiedMapping { now_ms, .. }
        | DataEvent::RoundEnd { now_ms } => *now_ms,
    }
}

/// Encode one event as an archive line value (see the module docs for the schema).
pub fn event_to_json(ev: &DataEvent) -> Value {
    match ev {
        DataEvent::Book {
            token_id,
            bids,
            asks,
            now_ms,
        } => json!({
            "at": now_ms,
            "k": "book",
            "t": token_id,
            "b": levels_json(bids),
            "a": levels_json(asks),
        }),
        // 盘口批次原子化：整批 book 归档为一条 `k:"books"` 批行 —— `ts` 是
        // 批次的统一时间戳，`books` 逐本携带 (t, b, a)。新增的 kind：旧语料
        // 不含它，逐事件回放基线不受影响；含它的语料回到旧核会因未知 kind
        // 被拒绝（fail-closed，正是归档该有的行为）。
        DataEvent::Books { books, now_ms } => json!({
            "at": now_ms,
            "k": "books",
            "ts": now_ms,
            "books": books
                .iter()
                .map(|(t, b, a)| json!({ "t": t, "b": levels_json(b), "a": levels_json(a) }))
                .collect::<Vec<_>>(),
        }),
        DataEvent::TopOfBook {
            token_id,
            best_bid,
            best_ask,
            now_ms,
        } => json!({
            "at": now_ms,
            "k": "top",
            "t": token_id,
            "bb": best_bid.map(|d| d.to_string()),
            "ba": best_ask.map(|d| d.to_string()),
        }),
        DataEvent::Spot {
            asset,
            price,
            now_ms,
        } => json!({
            "at": now_ms,
            "k": "spot",
            "s": asset,
            "p": price.to_string(),
        }),
        DataEvent::RoundMarkets { markets, now_ms } => json!({
            "at": now_ms,
            "k": "round",
            "m": markets,
        }),
        DataEvent::Trade {
            token_id,
            side,
            price,
            size,
            now_ms,
        } => json!({
            "at": now_ms,
            "k": "trade",
            "t": token_id,
            "s": if *side == Side::Buy { "BUY" } else { "SELL" },
            "p": price.to_string(),
            "q": size.to_string(),
        }),
        DataEvent::RoundEnd { now_ms } => json!({
            "at": now_ms,
            "k": "round_end",
        }),
        DataEvent::Resolution {
            condition_id,
            payouts,
            neg_risk,
            now_ms,
        } => json!({
            "at": now_ms,
            "k": "resolution",
            "c": condition_id,
            "p": payouts
                .iter()
                .map(|(t, p)| {
                    Value::Array(vec![Value::String(t.clone()), Value::String(p.to_string())])
                })
                .collect::<Vec<_>>(),
            "n": neg_risk,
        }),
        DataEvent::Cashflow {
            which,
            amount_usd,
            now_ms,
        } => json!({
            "at": now_ms,
            "k": "cashflow",
            "which": which.as_str(),
            "usd": amount_usd.to_string(),
        }),
        DataEvent::UnifiedMapping { event, now_ms } => json!({
            "at": now_ms,
            "k": "unified",
            "e": serde_json::to_value(event).unwrap_or(Value::Null),
        }),
    }
}

fn levels_json(levels: &[(Decimal, Decimal)]) -> Value {
    Value::Array(
        levels
            .iter()
            .map(|(p, s)| {
                Value::Array(vec![
                    Value::String(p.to_string()),
                    Value::String(s.to_string()),
                ])
            })
            .collect(),
    )
}
