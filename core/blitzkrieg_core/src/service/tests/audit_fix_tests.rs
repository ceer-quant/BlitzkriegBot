#![cfg(test)]
//! ── Audit F2/F10/F4: reverse acceptance tests ───────────────────────────────
//!
//! One test per audit finding, each driving the exact counterexample from
//! `BlitzkriegBot_核心逻辑审计.md` through the REAL production path and
//! asserting the account does NOT move the way the bug moved it:
//!
//!  - F2: a counterparty's 100-share SELL must never book against a 10-share
//!    inventory — the clamp truncates size AND proceeds; nothing attributable
//!    to inventory is never booked at all.
//!  - F4: a fully-closing SELL that later arrives FAILED must restore the
//!    position inventory and reverse the profit (TradeDb, strategy PnL, daily
//!    PnL, breaker) instead of leaving phantom profit behind.
//!  - F10: a manual (external) full close at a LOSS must reach the SAME close
//!    ledger the engine's own fills do — TradeDb record, strategy net PnL,
//!    win/loss counts and the PositionClosed event.

use super::*;
use rust_decimal_macros::dec;

const SEED: Decimal = dec!(1000);
const STRATEGY: &str = "acc";

fn core_with_trade_log(path: Option<std::path::PathBuf>) -> Core {
    let mut cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: SEED,
        auto_exits_enabled: false,
        ..Default::default()
    };
    cfg.trade_log_path = path.map(|p| p.to_string_lossy().into_owned());
    let mut c = Core::new(cfg);
    c.set_balance(SEED);
    c
}

fn core() -> Core {
    core_with_trade_log(None)
}

fn tmp_trade_log(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // TradeDb keeps summary.json beside the log, so each fixture needs its
    // own DIRECTORY — a bare file in the shared temp root would make every
    // test load (and persist onto) the same summary.json.
    let dir = std::env::temp_dir().join(format!("bk-audit-{tag}-{nanos}"));
    dir.join("trades.jsonl")
}

fn req(side: Side, mode: FillPolicy, price: Decimal, size: Decimal, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side,
        mode,
        price,
        size,
        internal_key: key.into(),
        strategy: STRATEGY.into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

fn fill(
    order_id: &str,
    trade: &str,
    side: Side,
    price: Decimal,
    size: Decimal,
    status: FillStatus,
) -> Fill {
    Fill {
        order_id: order_id.into(),
        trade_id: Some(trade.into()),
        token_id: "tok".into(),
        side,
        price,
        size,
        status,
        ts_ms: 0,
        tx_hash: None,
        // Maker-mode orders with no venue report → the policy decides, and
        // these fixtures are exactly what the policy would produce.
        maker: None,
        account_id: crate::model::default_account_id(),
    }
}

/// Open a maker position of `size` shares at `price` through the REAL
/// live fill path (register → venue ack → user-WS fill).
fn open_maker_position(c: &mut Core, price: Decimal, size: Decimal, now: i64) {
    let (id, _) = c
        .place_pending(req(Side::Buy, FillPolicy::Maker, price, size, "entry"), now)
        .unwrap();
    c.confirm_live(&id, now).unwrap();
    c.ingest_fill(
        fill(&id, "e1", Side::Buy, price, size, FillStatus::Confirmed),
        now + 1,
    )
    .unwrap();
}

fn external_sell(size: Decimal, price: Decimal, ts_ms: i64) -> crate::reconcile::VenueSnapshot {
    crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![crate::reconcile::VenueTrade {
            venue_order_id: "0xmanual".into(),
            trade_id: "t-manual".into(),
            token_id: "tok".into(),
            side: Side::Sell,
            size,
            price,
            ts_ms,
            tx_hash: None,
            maker: Some(true),
        }],
        now_ms: ts_ms + 1_000,
    }
}

/// A two-account core built through the injection constructor — no test
/// touches the real `user_layer/configs/accounts.toml` the cwd decides.
/// Both books are seeded (§9.3: separate wallets, not aliases).
fn two_book_core() -> Core {
    let book = crate::account::AccountLedgers::new(
        vec![
            crate::account::Account {
                id: crate::account::AccountId::from("default"),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
            crate::account::Account {
                id: crate::account::AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("default"),
    );
    let cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: SEED,
        auto_exits_enabled: false,
        // The cwd-relative default would silently read a REAL data/trades
        // log if one happens to sit beside the crate (the summary tests
        // must count their own closes, not history) — same discipline as
        // `core_with_trade_log`.
        trade_log_path: None,
        ..Default::default()
    };
    Core::with_account_book(cfg, book)
}

/// A two-account core where `default` is PINNED to venue `polymarket` and
/// `paper` is unbound. The engine carries two labelled round markets so
/// the token→venue lookup has something real to read.
fn venue_bound_core() -> Core {
    venue_bound_core_opts(None)
}

/// [`venue_bound_core`] with a durable order log — the persistence-path
/// tests read what `place` appends.
fn venue_bound_core_opts(order_log_path: Option<String>) -> Core {
    let book = crate::account::AccountLedgers::new(
        vec![
            crate::account::Account {
                id: crate::account::AccountId::from("default"),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: "polymarket".into(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
            crate::account::Account {
                id: crate::account::AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("default"),
    );
    let mut c = Core::with_account_book(
        CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: SEED,
            auto_exits_enabled: false,
            // These fixtures PLACE orders: the default config's relative
            // data/ log paths would append into whatever the process cwd
            // is at this instant (parallel tests move it), re-dirtying
            // the reset tests' self-check. A fixture that trades carries
            // no durable logs — same contract as `hot_reload_fixture`.
            trade_log_path: None,
            order_log_path,
            position_log_path: None,
            ..Default::default()
        },
        book,
    );
    c.enable_engine(crate::engine::Engine::new(crate::engine::EngineConfig {
        scanner: crate::scanner::ScannerConfig {
            assets: vec!["BTC".into()],
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 0,
        },
        ..Default::default()
    }));
    let now: i64 = 1_000_000;
    let slot = now / 1000 / 900;
    let market = |venue: &str, up: &str, down: &str| crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: format!("cond-{venue}"),
        question_id: "q".into(),
        up_token_id: up.into(),
        down_token_id: down.into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: false,
        question: "?".into(),
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![
                market("polymarket", "poly-up", "poly-down"),
                market("kalshi", "kal-up", "kal-down"),
            ],
            now_ms: now,
        },
        now,
    );
    c
}

fn bound_order(token: &str, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: token.into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Maker,
        price: dec!(0.40),
        size: dec!(1),
        internal_key: key.into(),
        strategy: STRATEGY.into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

// #780 — the file is now a directory: the shared fixtures stay here,
// and the tests are grouped by behaviour into child modules.
mod audit_account_scope_tests;
mod audit_reconcile_tests;
