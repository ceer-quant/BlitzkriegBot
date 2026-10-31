use super::*;

/// A live core whose account book carries a LEFTOVER balance — the shape the
/// operator reported: the book was seeded by a dry run (or replayed from that
/// dry run's journal) and the process now boots live, so no venue has ever
/// reported this book and the number is not the venue's truth. Fixture: seed
/// the ledger directly (the same end state a dry journal replay lands);
/// `loaded` is carried so the credential state can be varied independently —
/// issue 749: it must NOT decide the projection.
fn live_core_with_leftover_book(balance: Decimal, loaded: bool, mode: Mode) -> Core {
    let book = crate::account::AccountLedgers::new(
        vec![crate::account::Account {
            id: crate::account::AccountId::from("default"),
            name: "default".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys {
                loaded,
                ..Default::default()
            },
            venue: String::new(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            updated_at_ms: 0,
        }],
        crate::account::AccountId::from("default"),
    );
    let mut c = Core::with_account_book(
        CoreConfig {
            mode,
            dry_seed_balance: balance,
            ..Default::default()
        },
        book,
    );
    // Leftover cash on the book: the dry seed (supervisor passes
    // `--seed-balance 1000` in every mode) or a replayed dry journal's last
    // row. Neither is a live fact.
    c.set_balance(balance);
    c
}

#[test]
fn a_live_account_without_loaded_credentials_projects_zero() {
    let c = live_core_with_leftover_book(dec!(1000), false, Mode::Live);
    let views = c.account_views(1);
    assert_eq!(views.len(), 1);
    let v = &views[0];
    assert_eq!(
        v.balance,
        Decimal::ZERO,
        "live + credentials unloaded: the dry seed must not read as the balance"
    );
    assert_eq!(v.available, Decimal::ZERO);
    assert_eq!(v.reserved, Decimal::ZERO);
    assert!(
        !v.credentials_loaded,
        "the zero projection pairs with the honest loaded=false fact"
    );
    // The book itself is untouched — the projection folds the view only, so a
    // later realign (credentials loaded, bridge reports) still has the book.
    assert_eq!(c.ledger().balance(), dec!(1000));
}

#[test]
fn a_live_account_folds_its_book_even_with_loaded_credentials() {
    // issue 749: `loaded` describes a boot probe of DECLARED env names. It is
    // not "a venue told us this book's cash", so it must not open the view —
    // the leftover 1000 is still a leftover.
    let c = live_core_with_leftover_book(dec!(1000), true, Mode::Live);
    let views = c.account_views(1);
    assert!(views[0].credentials_loaded);
    assert_eq!(
        views[0].balance,
        Decimal::ZERO,
        "a loaded credential declaration is not a venue report"
    );
}

#[test]
fn a_venue_report_opens_a_live_accounts_book() {
    // …and the fact that DOES open it is the venue's own word.
    let mut c = live_core_with_leftover_book(dec!(1000), false, Mode::Live);
    c.realign_venue_cash("polymarket", dec!(25));
    let views = c.account_views(1);
    assert!(!views[0].credentials_loaded, "no declaration was needed");
    assert_eq!(
        views[0].balance,
        dec!(25),
        "the venue's number is the balance, credential declaration or not"
    );
}

#[test]
fn a_dry_account_keeps_the_seed_projection() {
    // The seed is the whole point of a locally-settling mode: dry + unloaded
    // credentials must still show the 1000 (the operator's dry runs depend on
    // it). Same book and leftover, mode flipped.
    let c = live_core_with_leftover_book(dec!(1000), false, Mode::Dry);
    let views = c.account_views(1);
    assert_eq!(
        views[0].balance,
        dec!(1000),
        "dry keeps the seed — the zero-fold is live-only"
    );
}

// ── B1c: refuse_unsubscribed_duration — the admission gate's teeth ─────

#[test]
fn an_unsubscribed_family_order_is_refused_with_account_limit() {
    // The gate's core verdict: with a declared subscription set that does
    // NOT contain the order's (venue, duration) family, the order is
    // refused with the AccountLimit vocabulary, naming the family. The
    // chain position (right after the venue binding guard, before risk)
    // means the refusal surfaces before any notional is reserved.
    let mut c = venue_bound_core();
    // The set declares ONLY polymarket 15m; the kalshi token belongs to
    // (kalshi, 900) — unsubscribed. The kalshi attempt runs from the
    // UNBOUND account (the venue binding guard would refuse it first
    // for the bound one — earlier gate in the chain, unchanged); the
    // duration gate fires for it because the empty-venue account still
    // reads the shared staged set.
    c.stage_duration_subscriptions_for_test(HashSet::from([("polymarket".to_string(), 900)]));
    let mut kal = bound_order("kal-up", "k1");
    kal.account_id = crate::account::AccountId::from("paper");
    let err = c.place(kal, 0, 1).unwrap_err();
    assert_eq!(err.code, CoreErrorCode::AccountLimit, "{err:?}");
    assert!(err.message.contains("unsubscribed round family"), "{err}");
    assert!(
        err.message.contains("`kalshi`") && err.message.contains("900"),
        "the refusal names the offending family: {err}"
    );
    assert!(c.list_orders().is_empty(), "nothing reached the book");
    // A family the account DID subscribe passes (from the bound account,
    // its own venue and its declared family).
    c.place(bound_order("poly-up", "k2"), 0, 2).unwrap();
    assert_eq!(c.list_orders().len(), 1);
}

#[test]
fn a_close_intent_is_exempt_from_the_duration_gate() {
    // The #174 hatch, copied verbatim: a position admitted while the
    // family was subscribed must never be trapped by an unsubscribe —
    // closing intents always pass the family gate, even for a family the
    // account never declared.
    let mut c = venue_bound_core();
    // The declared set contains nothing for polymarket's own family — a
    // non-close BUY on poly-up would be refused. The token still belongs
    // to the bound account's own venue, so the earlier venue binding
    // guard has no verdict either: only the duration gate could stop
    // this order, and the close intent must sail past it.
    c.stage_duration_subscriptions_for_test(HashSet::from([("kalshi".to_string(), 900)]));
    // A close SELL on a polymarket token (no open position — the exit for
    // a token with none flows through, same as the cross-account guard's
    // precedent; the gate under test must not be the thing that stops it).
    let mut close = bound_order("poly-up", "exit:probe-1");
    close.side = Side::Sell;
    c.place(close, 0, 1).unwrap();
    assert_eq!(c.list_orders().len(), 1, "the close passed the family gate");
}

#[test]
fn an_empty_subscription_set_admits_everything() {
    // Today's behaviour (and the zero-config shape): no declared set ⇒
    // no restriction ⇒ every family passes. The production gate reads a
    // constant empty set until B1d wires the real assembly, so this is
    // also the byte-compatibility proof for every existing corpus.
    let mut c = venue_bound_core();
    // No staging: the gate reads the empty set. Both families pass —
    // the bound account through its own venue, the unbound one anywhere.
    c.place(bound_order("poly-up", "k1"), 0, 1).unwrap();
    let mut kal = bound_order("kal-up", "k2");
    kal.account_id = crate::account::AccountId::from("paper");
    c.place(kal, 0, 2).unwrap();
    assert_eq!(c.list_orders().len(), 2);
    // Explicitly staged EMPTY set: the same verdict.
    let mut c2 = venue_bound_core();
    c2.stage_duration_subscriptions_for_test(HashSet::new());
    c2.place(bound_order("poly-up", "k3"), 0, 1).unwrap();
    assert_eq!(c2.list_orders().len(), 1);
}

#[test]
fn a_duration_mismatch_within_one_venue_is_refused() {
    // The gate is a (venue, duration) PAIR check, not a venue check:
    // polymarket 15m subscribed does not admit a polymarket 1h token —
    // the duration axis is the new dimension this gate adds.
    let mut c = venue_bound_core();
    c.stage_duration_subscriptions_for_test(HashSet::from([("polymarket".to_string(), 900)]));
    // A 1h polymarket market, same venue, different family.
    let now = 1_000_000;
    let end1h = 2 * 3600 * 1000;
    let market = crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond-poly-3600".into(),
        question_id: "q".into(),
        up_token_id: "poly-1h-up".into(),
        down_token_id: "poly-1h-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end1h,
        round_slot: end1h / 1000 / 3600,
        round_duration_sec: 3600,
        archive_verdict: false,
        venue: "polymarket".into(),
        neg_risk: false,
        question: "?".into(),
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![market],
            now_ms: now,
        },
        now,
    );
    let mut order = bound_order("poly-1h-up", "k1");
    order.token_id = "poly-1h-up".into();
    let err = c.place(order, 0, 1).unwrap_err();
    assert_eq!(err.code, CoreErrorCode::AccountLimit, "{err:?}");
    assert!(
        err.message.contains("3600"),
        "the refusal names the 1h family: {err}"
    );
}

/// The order's venue tag is the venue binding of the account it was
/// placed from — the same fact `account.list` reports. An order placed
/// by an account bound to kalshi reads `venue: "kalshi"`; an order
/// placed by an UNBOUND account carries NO venue key at all on the wire
/// (empty string is skipped), so the panel's venue filter keeps the row
/// visible (an untagged row must not be silently dropped).
#[test]
fn order_views_carry_the_account_venue_binding() {
    let mut c = venue_bound_core();
    // `default` is bound to polymarket: its order tags polymarket.
    c.place(bound_order("poly-up", "k1"), 0, 1).unwrap();
    // `paper` is unbound: its order carries no venue tag.
    let mut paper = bound_order("kal-up", "k2");
    paper.account_id = crate::account::AccountId::from("paper");
    c.place(paper, 0, 2).unwrap();
    assert_eq!(c.list_orders().len(), 2);

    let mut rows = c.list_orders();
    rows.sort_by(|a, b| a.internal_key.cmp(&b.internal_key));
    assert_eq!(rows[0].internal_key, "k1");
    assert_eq!(
        rows[0].venue, "polymarket",
        "the tag is the placing account's binding, not the token's venue"
    );
    assert_eq!(rows[1].internal_key, "k2");
    assert_eq!(rows[1].venue, "", "an unbound account's order is untagged");
    // The wire contract: the empty tag is ABSENT (skip_serializing_if),
    // the non-empty one present. The panel's filterByVenue keeps rows
    // without the key visible.
    let wire: Vec<serde_json::Value> = rows
        .iter()
        .map(|o| serde_json::to_value(o).unwrap())
        .collect();
    assert_eq!(
        wire[0]["venue"],
        serde_json::json!("polymarket"),
        "the bound order's venue key is present"
    );
    assert!(
        wire[1].get("venue").is_none(),
        "the unbound order's wire row carries NO venue key: {}",
        wire[1]
    );
}

/// B1d (轮次多族): the gate's subscription set assembles from the
/// ACCOUNT's own `rounds` declaration (the real seam, not the test
/// override). A bound account's set is (venue × every declared
/// duration); a declared family the account did not list is refused.
#[test]
fn the_duration_gate_assembles_from_the_account_rounds_declaration() {
    let mut c = venue_bound_core();
    // `default` is bound to polymarket; declare ONLY the 900s family —
    // every poly market in this fixture IS 900s, so its orders sail.
    c.accounts_mut()
        .set_rounds(
            &crate::account::AccountId::from("default"),
            vec![900],
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap();
    c.place(bound_order("poly-up", "k1"), 0, 1).unwrap();
    assert_eq!(c.list_orders().len(), 1, "the declared family passes");
    // A poly 1h token is (polymarket, 3600) — declared? No. Refused.
    let now = 1_000_000;
    let end1h = 2 * 3600 * 1000;
    let m = crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond-poly-1h".into(),
        question_id: "q".into(),
        up_token_id: "poly-1h-up".into(),
        down_token_id: "poly-1h-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end1h,
        round_slot: end1h / 1000 / 3600,
        round_duration_sec: 3600,
        archive_verdict: false,
        venue: "polymarket".into(),
        neg_risk: false,
        question: "?".into(),
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![m],
            now_ms: now,
        },
        now,
    );
    let err = c.place(bound_order("poly-1h-up", "k2"), 0, 2).unwrap_err();
    assert_eq!(err.code, CoreErrorCode::AccountLimit, "{err:?}");
    assert!(err.message.contains("3600"), "{err}");
    // Declaring the 1h family too re-opens it — the assembly is the
    // declaration's product, venue × every listed duration.
    c.accounts_mut()
        .set_rounds(
            &crate::account::AccountId::from("default"),
            vec![900, 3600],
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap();
    c.place(bound_order("poly-1h-up", "k3"), 0, 3).unwrap();
    assert_eq!(c.list_orders().len(), 2);
}

/// B1d: an UNBOUND account's `rounds` declaration speaks for EVERY
/// venue the live book currently labels at that duration — the set is
/// the declaration expanded across the live venues, so the account
/// trades its declared families everywhere without naming venues.
#[test]
fn an_unbound_accounts_rounds_span_every_live_venue_at_the_declared_duration() {
    let mut c = venue_bound_core();
    // `paper` is unbound; it declares 900s only.
    c.accounts_mut()
        .set_rounds(
            &crate::account::AccountId::from("paper"),
            vec![900],
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap();
    // Both live venues carry 900s markets: both are admitted.
    let mut p1 = bound_order("poly-up", "k1");
    p1.account_id = crate::account::AccountId::from("paper");
    c.place(p1, 0, 1).unwrap();
    let mut p2 = bound_order("kal-up", "k2");
    p2.account_id = crate::account::AccountId::from("paper");
    c.place(p2, 0, 2).unwrap();
    assert_eq!(c.list_orders().len(), 2, "900s on both venues passes");
    // The poly 1h family is not declared: refused from the same account.
    let now = 1_000_000;
    let end1h = 2 * 3600 * 1000;
    let m = crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond-poly-1h".into(),
        question_id: "q".into(),
        up_token_id: "poly-1h-up".into(),
        down_token_id: "poly-1h-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end1h,
        round_slot: end1h / 1000 / 3600,
        round_duration_sec: 3600,
        archive_verdict: false,
        venue: "polymarket".into(),
        neg_risk: false,
        question: "?".into(),
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![m],
            now_ms: now,
        },
        now,
    );
    let mut p3 = bound_order("poly-1h-up", "k3");
    p3.account_id = crate::account::AccountId::from("paper");
    let err = c.place(p3, 0, 3).unwrap_err();
    assert_eq!(err.code, CoreErrorCode::AccountLimit, "{err:?}");
    // A bound account with an EMPTY rounds set stays unrestricted —
    // the historical shape (no declaration = no gate).
    c.accounts_mut()
        .set_rounds(
            &crate::account::AccountId::from("default"),
            Vec::new(),
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap();
    c.place(bound_order("poly-1h-up", "k4"), 0, 4).unwrap();
    assert_eq!(c.list_orders().len(), 3);
}

/// The venue tag is a DERIVED read, never stored: the persisted order
/// log stays byte-identical to its pre-#427-orders shape (empty tag is
/// skipped on write), and a rebinding changes what `orders.list` reads
/// without touching any stored row.
#[test]
fn the_order_venue_tag_is_derived_not_stored() {
    let dir = std::env::temp_dir().join(format!(
        "bk-order-venue-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut c = venue_bound_core_opts(Some(
        dir.join("orders.jsonl").to_string_lossy().into_owned(),
    ));
    c.place(bound_order("poly-up", "k1"), 0, 1).unwrap();

    // The persisted line carries no venue key (skip_serializing_if).
    let line = std::fs::read_to_string(dir.join("orders.jsonl")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert!(
        doc.get("venue").is_none(),
        "the persisted order line must not gain a venue key: {doc}"
    );

    // Rebinding is a fact about the ACCOUNT, not the order: the same
    // stored order now reads with the new tag, nothing rewritten.
    c.accounts_mut()
        .set_venue(&crate::account::AccountId::from("default"), "kalshi", 2)
        .unwrap();
    assert_eq!(c.list_orders()[0].venue, "kalshi");
    let line2 = std::fs::read_to_string(dir.join("orders.jsonl")).unwrap();
    assert_eq!(
        line, line2,
        "the order log is untouched by a venue rebinding"
    );
}

/// RFC-661 §五.5 回放半边（M1）：单键时代（venue A）落盘的 live 订单
/// 重启恢复（`restore_orders` → OME restore）后，其执行路由仍走
/// `execution_venue_of` 的 token-only 解析。双键时代同一字节 token
/// 若两所各持一行（A→B 换 venue：B 的批次后到，token-only 查找变
/// 歧义），恢复的单绝不能按插入序被路由到先到的 A 执行器 —— 歧义
/// 解析为空（UNTAGGED），UNTAGGED 单只进 active-plugin 桶；账户绑定
/// （B）的单仍路由到它自己的执行器。A→B 不串账，B→A 对称（同一规
/// 则，插入序无关）。
#[test]
fn a_restored_legacy_order_routes_by_binding_not_by_insertion_order() {
    let dir = std::env::temp_dir().join(format!(
        "bk-restore-dualkey-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    // Era 1 (venue A = polymarket only): a live order rests on the shared
    // token and is persisted.
    let mut c = venue_bound_core_opts(Some(
        dir.join("orders.jsonl").to_string_lossy().into_owned(),
    ));
    c.place(bound_order("poly-up", "k1"), 0, 1).unwrap();
    assert_eq!(c.list_orders().len(), 1);
    drop(c);

    // Era 2: the core reboots and restores. The era-2 fixture's round
    // list still names ONLY polymarket's row for the token: the
    // token-only lookup has a UNIQUE candidate and resolves to it — the
    // legacy archive replays byte-identically. (With the engine fully
    // down the same lookup is UNTAGGED `""` — every executor sees the
    // order, the pre-routing bridge behaviour.)
    let mut reborn = venue_bound_core_opts(Some(
        dir.join("orders.jsonl").to_string_lossy().into_owned(),
    ));
    assert_eq!(reborn.restore_orders(), 1, "the live order is re-adopted");
    assert_eq!(
        reborn.pending_unbound_for_venue("polymarket").len(),
        1,
        "a unique candidate routes the restored order to its venue's drain"
    );
    assert!(reborn.pending_unbound_for_venue("kalshi").is_empty());

    // A→B: the B venue (kalshi here, any sibling) now lands a batch
    // carrying byte-identical tokens. The token-only lookup turns
    // AMBIGUOUS — the restored order must NOT ride the insertion-order
    // first-hit into the wrong executor; the ACCOUNT's binding
    // (polymarket) still owns the order through the dual-key row.
    let now = 1_000_000;
    let slot = now / 1000 / 900;
    let twin = crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond-twin".into(),
        question_id: "q".into(),
        up_token_id: "poly-up".into(),
        down_token_id: "poly-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: "twinvenue".into(),
        neg_risk: false,
        question: "?".into(),
    };
    reborn.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![twin],
            now_ms: now + 1,
        },
        now + 1,
    );
    // The binding reaches the engine's scope map on the evaluate tick —
    // the same sync the runtime relies on.
    reborn.engine_evaluate(now + 2);
    assert_eq!(
        reborn.execution_venue_of("default", "poly-up"),
        "polymarket",
        "the account binding reads its OWN dual-key row — ambiguity of the \
         token-only lookup never re-routes a bound account"
    );
    assert_eq!(
        reborn.pending_unbound_for_venue("polymarket").len(),
        1,
        "the bound account's restored order drains to ITS venue"
    );
    assert!(
        reborn.pending_unbound_for_venue("twinvenue").is_empty(),
        "the sibling executor never sees another venue's bound order"
    );
    // The ambiguity itself is real and refused (never first-hit): an
    // UNBOUND account's token-only resolution gets nothing.
    assert_eq!(
        reborn.execution_venue_of("paper", "poly-up"),
        "",
        "an ambiguous token resolves for no unbound account"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// #463: the close-credential journal survives a restart. A push is
/// journalled, a consumption tombstones it, and a fresh Core built on the
/// same data dir replays exactly the still-live credentials — the window
/// between a close fill and the venue's FAILED no longer dies with the
/// process.
#[test]
fn the_close_credential_journal_survives_a_restart() {
    let dir = std::env::temp_dir().join(format!(
        "bk-cred-journal-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let order_log = dir.join("orders.jsonl");
    let journal = dir.join("close-credentials.jsonl");

    let cred = |shares: rust_decimal::Decimal| CloseCredential {
        restored: crate::position::OpenPosition {
            id: "hft-1".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: crate::model::SignalDirection::Up,
            token_id: "tok".into(),
            condition_id: "cond".into(),
            entry_price: dec!(0.43),
            current_price: dec!(0.45),
            prev_price: dec!(0.44),
            shares,
            cost_usd: dec!(4.3),
            was_maker_entry: true,
            entry_fee_pct: rust_decimal::Decimal::ZERO,
            entry_role: crate::model::OrderRole::Maker,
            exit_role: crate::model::OrderRole::Pending,
            target_exit_price: None,
            entered_at_ms: 1000,
            expires_at_ms: 900_000,
            account_id: crate::model::default_account_id(),
            venue: String::new(),
            partner_token_id: String::new(),
            last_book_ts: 0,
            round_duration_sec: 0,
            round_slot: 0,
            state: crate::exit_policy::ExitState::new(dec!(0.43), 1000),
            flows: crate::position::CashFlows {
                entry_cost_usd: dec!(4.3),
                opened_shares: shares,
                ..Default::default()
            },
        },
        closed: crate::position::ClosedPosition {
            id: "hft-1".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: crate::model::SignalDirection::Up,
            token_id: "tok".into(),
            condition_id: "cond".into(),
            entry_price: dec!(0.43),
            exit_price: dec!(0.50),
            shares,
            cost_usd: dec!(4.3),
            was_maker_entry: true,
            was_maker_exit: false,
            entry_fee_pct: rust_decimal::Decimal::ZERO,
            exit_fee_pct: rust_decimal::Decimal::ZERO,
            maker_rebate_usd: rust_decimal::Decimal::ZERO,
            pnl_usd: dec!(0.7),
            pnl_pct: dec!(16.27),
            net_pnl_usd: dec!(0.7),
            net_pnl_pct: dec!(16.27),
            high_pnl_pct: dec!(0),
            low_pnl_pct: dec!(0),
            hold_time_sec: 30,
            exit_reason: crate::model::ExitReason::TakeProfit,
            entered_at_ms: 1000,
            exited_at_ms: 4000,
            entry_role: crate::model::OrderRole::Maker,
            exit_role: crate::model::OrderRole::Taker,
            dust_shares: rust_decimal::Decimal::ZERO,
            account_id: crate::model::default_account_id(),
            venue: String::new(),
        },
        streak_before: 2,
        account_streak_before: 1,
    };

    // Boot one core with persistence on, push two credentials, consume one.
    let mut core = Core::new(CoreConfig {
        order_log_path: Some(order_log.to_string_lossy().to_string()),
        ..Default::default()
    });
    core.push_close_credential("trade-a".into(), cred(dec!(10)));
    core.push_close_credential("trade-b".into(), cred(dec!(4)));
    assert_eq!(core.close_credentials.len(), 2);

    // Consume trade-b the way a FAILED status would: tombstone + remove.
    let pos = core
        .close_credentials
        .iter()
        .position(|(tid, _)| tid == "trade-b")
        .unwrap();
    let _ = core.close_credentials.remove(pos).unwrap();
    if let Some(path) = &core.close_credential_log {
        crate::jsonl::append(
            path,
            &CloseCredentialLine::Consumed {
                trade_id: "trade-b".into(),
            },
        );
    }

    // "Restart": a fresh Core on the same data dir, recovery re-run the way
    // the IPC server does at boot. Only trade-a may re-enter the queue;
    // the tombstone must keep trade-b out.
    let mut reborn = Core::new(CoreConfig {
        order_log_path: Some(order_log.to_string_lossy().to_string()),
        ..Default::default()
    });
    assert!(
        reborn.close_credentials.is_empty(),
        "the constructor reads NOTHING — replay is recovery, not boot"
    );
    reborn.restore_orders();
    assert_eq!(
        reborn.close_credentials.len(),
        1,
        "exactly the unconsumed credential replays"
    );
    assert_eq!(reborn.close_credentials[0].0, "trade-a");
    assert_eq!(reborn.close_credentials[0].1.restored.shares, dec!(10));

    // The journal file exists beside the order log and every line parses.
    let text = std::fs::read_to_string(&journal).unwrap();
    let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
    assert!(lines >= 3, "push, push, tombstone all landed: {lines}");

    std::fs::remove_dir_all(&dir).ok();
}

// ── PR H (账户筛选修复): per-(strategy, account) accounting ──────────────

/// The audit `req` helper, retargeted at an account. Everything else
/// (token, strategy, slot) stays shared — the exact shape that used to
/// merge two accounts into one accounting row.
fn req_for(
    side: Side,
    mode: FillPolicy,
    price: Decimal,
    size: Decimal,
    key: &str,
    account: &str,
) -> OrderRequest {
    let mut r = req(side, mode, price, size, key);
    r.account_id = crate::account::AccountId::from(account);
    r
}

/// Both accounts trade the SAME strategy on the SAME token. Before PR H
/// the accounting key was the bare strategy name, so the second account's
/// fills landed on the FIRST account's counters (merged PnL, doubled
/// closed_trades) and `strategy_stats` could not tell the accounts apart.
/// The fix keys the map by (strategy, \u{1}, account); each slice must
/// carry only its own account's activity.
#[test]
fn two_accounts_same_strategy_keep_separate_accounting_rows() {
    let mut c = two_book_core();

    // default opens at 0.40 and closes at 0.50 (+1.00 gross before fees).
    open_maker_position(&mut c, dec!(0.40), dec!(10), 1);
    c.book_snapshot("tok", vec![(dec!(0.50), dec!(500))], vec![], 3);
    c.place(
        req_for(
            Side::Sell,
            FillPolicy::Taker,
            dec!(0.50),
            dec!(10),
            "exit:d1",
            "default",
        ),
        0,
        4,
    )
    .unwrap();
    assert!(c.positions().open_positions().is_empty());

    // paper opens the same token at 0.60 and closes at 0.40 (−2.00 gross).
    let mut paper_open = req_for(
        Side::Buy,
        FillPolicy::Maker,
        dec!(0.60),
        dec!(10),
        "entry:p1",
        "paper",
    );
    paper_open.internal_key = "entry:p1:paper".into(); // distinct slot key
    let (pid, _) = c.place_pending(paper_open, 10).unwrap();
    c.confirm_live(&pid, 10).unwrap();
    c.ingest_fill(
        fill(
            &pid,
            "e-p1",
            Side::Buy,
            dec!(0.60),
            dec!(10),
            FillStatus::Confirmed,
        ),
        11,
    )
    .unwrap();
    c.book_snapshot("tok", vec![(dec!(0.40), dec!(500))], vec![], 13);
    c.place(
        req_for(
            Side::Sell,
            FillPolicy::Taker,
            dec!(0.40),
            dec!(10),
            "exit:p1",
            "paper",
        ),
        0,
        14,
    )
    .unwrap();

    // Each account's slice: exactly its own trade, its own net.
    let dflt = c
        .strategy_accounting
        .get(&strategy_acc_key(STRATEGY, "default"))
        .expect("default's accounting row");
    assert_eq!(dflt.closed_trades, 1, "default sees only its own close");
    assert_eq!(dflt.wins, 1);
    assert!(
        dflt.net_pnl_usd > Decimal::ZERO,
        "default won: {}",
        dflt.net_pnl_usd
    );

    let paper = c
        .strategy_accounting
        .get(&strategy_acc_key(STRATEGY, "paper"))
        .expect("paper's accounting row");
    assert_eq!(paper.closed_trades, 1, "paper sees only its own close");
    assert_eq!(paper.losses, 1);
    assert!(
        paper.net_pnl_usd < Decimal::ZERO,
        "paper lost: {}",
        paper.net_pnl_usd
    );

    // THE TOOTH: with a bare-name key this lookup returned default's row
    // (first writer) and paper's activity was invisible/doubled. The two
    // slices must never be the same row, and the folded strategy_stats
    // row must name BOTH accounts.
    assert_ne!(
        strategy_acc_key(STRATEGY, "default"),
        strategy_acc_key(STRATEGY, "paper")
    );
    let rows = c.strategy_stats();
    let row = rows
        .iter()
        .find(|r| r["name"] == STRATEGY)
        .expect("one folded row per strategy name");
    let accounts: Vec<String> = serde_json::from_value(row["accounts"].clone()).unwrap();
    assert_eq!(accounts, vec!["default".to_string(), "paper".to_string()]);
    assert_eq!(row["closedTrades"], 2, "the fold sums both slices");
    assert_eq!(row["wins"], 1);
    assert_eq!(row["losses"], 1);

    // The per-account view: paper's page shows paper's slice only.
    let for_paper = c.strategy_stats_for_account("paper");
    let prow = for_paper
        .iter()
        .find(|r| r["name"] == STRATEGY)
        .expect("the strategy row on paper's page");
    assert_eq!(prow["closedTrades"], 1);
    assert_eq!(prow["losses"], 1);
    assert_eq!(prow["wins"], 0);
}

/// 账户↔venue 解耦（投影层牙）: two accounts hold the SAME token at the
/// SAME time. The fill projection keys positions by (ACCOUNT, token) —
/// the old token-only merge landed paper's fill onto default's row
/// (one merged position, paper's book empty), so this tooth holds both
/// legs simultaneously and demands: two rows, each with its own basis,
/// each paid from its own ledger, and each account's close resolving to
/// its OWN row while the sibling's survives untouched.
#[test]
fn two_accounts_holding_the_same_token_keep_separate_position_rows() {
    let mut c = two_book_core();

    // default holds 10 @ 0.40 (cost 4); paper holds 10 @ 0.60 (cost 6) —
    // same token, overlapping holds. Maker fills: no fees anywhere.
    open_maker_position(&mut c, dec!(0.40), dec!(10), 1);
    let mut paper_open = req_for(
        Side::Buy,
        FillPolicy::Maker,
        dec!(0.60),
        dec!(10),
        "entry:p1",
        "paper",
    );
    paper_open.internal_key = "entry:p1:paper".into(); // distinct slot key
    let (pid, _) = c.place_pending(paper_open, 2).unwrap();
    c.confirm_live(&pid, 2).unwrap();
    c.ingest_fill(
        fill(
            &pid,
            "e-p1",
            Side::Buy,
            dec!(0.60),
            dec!(10),
            FillStatus::Confirmed,
        ),
        3,
    )
    .unwrap();

    // THE TOOTH: two rows, one per account, each with its own basis.
    let rows = c.positions().open_positions();
    assert_eq!(
        rows.len(),
        2,
        "same token, two accounts: two rows, never one merged position"
    );
    let dflt = rows
        .iter()
        .find(|p| p.account_id.as_str() == "default")
        .expect("default's own row");
    let paper = rows
        .iter()
        .find(|p| p.account_id.as_str() == "paper")
        .expect("paper's own row");
    assert_eq!(
        dflt.entry_price,
        dec!(0.40),
        "default's basis is default's fill"
    );
    assert_eq!(
        paper.entry_price,
        dec!(0.60),
        "paper's basis is paper's fill — a token-only merge would have stamped 0.60 onto \
         default's (first) row and left paper with nothing"
    );
    // Each book paid for its own leg.
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("default"))
            .unwrap()
            .balance(),
        SEED - dec!(4),
    );
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("paper"))
            .unwrap()
            .balance(),
        SEED - dec!(6),
    );

    // The RIGHT account's close resolves to ITS row: paper closes at 0.50
    // (−1.00 gross, no fee) and default's 0.40 row survives untouched.
    let (sid, _) = c
        .place_pending(
            req_for(
                Side::Sell,
                FillPolicy::Maker,
                dec!(0.50),
                dec!(10),
                "exit:p1",
                "paper",
            ),
            4,
        )
        .unwrap();
    c.confirm_live(&sid, 4).unwrap();
    c.ingest_fill(
        fill(
            &sid,
            "x-p1",
            Side::Sell,
            dec!(0.50),
            dec!(10),
            FillStatus::Confirmed,
        ),
        5,
    )
    .unwrap();
    let rows = c.positions().open_positions();
    assert_eq!(rows.len(), 1, "paper's close removed exactly paper's row");
    assert_eq!(rows[0].account_id.as_str(), "default");
    assert_eq!(
        rows[0].entry_price,
        dec!(0.40),
        "default's row is the survivor, byte-for-byte its own basis"
    );
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("paper"))
            .unwrap()
            .balance(),
        SEED - dec!(1),
        "paper's close settled into paper's book"
    );
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("default"))
            .unwrap()
            .balance(),
        SEED - dec!(4),
        "default's cash never moved for paper's trade"
    );
    // And the close landed on PAPER's accounting slice.
    let acc = c
        .strategy_accounting
        .get(&strategy_acc_key(STRATEGY, "paper"))
        .expect("paper's accounting row");
    assert_eq!(acc.closed_trades, 1);
    assert_eq!(acc.losses, 1);
    assert!(
        !c.strategy_accounting
            .contains_key(&strategy_acc_key(STRATEGY, "default")),
        "default has no closed trade yet — its slice must not exist"
    );
}

/// `trades.summary` with an `accountId` filters the row set before
/// aggregating — the wallet page's headline must belong to the active
/// account, not to every account at once. The no-arg call keeps the
/// persisted global headline byte-for-byte.
#[test]
fn trade_summary_scopes_to_one_account() {
    let mut c = two_book_core();

    open_maker_position(&mut c, dec!(0.40), dec!(10), 1);
    c.book_snapshot("tok", vec![(dec!(0.50), dec!(500))], vec![], 3);
    c.place(
        req_for(
            Side::Sell,
            FillPolicy::Taker,
            dec!(0.50),
            dec!(10),
            "exit:d1",
            "default",
        ),
        0,
        4,
    )
    .unwrap();

    let mut paper_open = req_for(
        Side::Buy,
        FillPolicy::Maker,
        dec!(0.60),
        dec!(10),
        "entry:p1",
        "paper",
    );
    paper_open.internal_key = "entry:p1:paper".into();
    let (pid, _) = c.place_pending(paper_open, 10).unwrap();
    c.confirm_live(&pid, 10).unwrap();
    c.ingest_fill(
        fill(
            &pid,
            "e-p1",
            Side::Buy,
            dec!(0.60),
            dec!(10),
            FillStatus::Confirmed,
        ),
        11,
    )
    .unwrap();
    c.book_snapshot("tok", vec![(dec!(0.40), dec!(500))], vec![], 13);
    c.place(
        req_for(
            Side::Sell,
            FillPolicy::Taker,
            dec!(0.40),
            dec!(10),
            "exit:p1",
            "paper",
        ),
        0,
        14,
    )
    .unwrap();

    // Global: two closed trades (the persisted headline keeps folding
    // everything — the snapshot's `tradeSummary` shape is unchanged).
    assert_eq!(c.trade_summary(None)["totalTrades"], 2);

    // Scoped: one trade each, opposite signs.
    let d = c.trade_summary(Some("default"));
    assert_eq!(d["totalTrades"], 1);
    assert_eq!(d["wins"], 1);
    let p = c.trade_summary(Some("paper"));
    assert_eq!(p["totalTrades"], 1);
    assert_eq!(p["losses"], 1);
    assert!(
        p["totalNetPnl"].as_f64().unwrap() < 0.0,
        "paper's slice is the losing one: {}",
        p["totalNetPnl"]
    );

    // An unknown account answers an EMPTY summary, not the global one.
    let ghost = c.trade_summary(Some("ghost"));
    assert_eq!(ghost["totalTrades"], 0);
}

/// PR H: two scopes of the SAME strategy name (default + paper) must not
/// duplicate the `strategy.list` row — the panel table is per-NAME, and
/// the pre-fix kernel listed the name once per scope (the 伪影 bug). The
/// engine's raw list keeps both (registration order is a tie-break
/// elsewhere); the service layer is where the per-NAME projection lives.
#[test]
fn strategy_names_dedupe_across_account_scopes() {
    let mut c = two_book_core();
    let cfg = c.config.clone();
    cfg.install_engine(&mut c)
        .expect("test engine (no strategy_dir: registers nothing itself)");
    let engine = c.engine.as_mut().expect("test engine");
    crate::strategies::test_support::host_disabled(engine, Default::default(), Default::default());
    // The SAME name again, into the paper scope: the raw engine list now
    // repeats every name.
    let dup = crate::strategies::test_support::TestSpreadArb::new(
        crate::signal::TrendConfig::default(),
        crate::signal::SpreadArbConfig::default(),
    );
    engine
        .register_scoped_strategy(Box::new(dup), "test".into(), "paper")
        .expect("second scope of the same package");

    let raw = engine.supported_strategies();
    assert!(
        raw.len() > raw.iter().collect::<std::collections::BTreeSet<_>>().len(),
        "the raw engine list must repeat names across scopes for this test to have a tooth"
    );
    let names = c.strategy_names();
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        names, sorted,
        "strategy_names must answer each name exactly once: {names:?}"
    );
    // And the stats surface — same projection — shows one row per name.
    assert_eq!(
        c.strategy_stats().len(),
        names.len(),
        "one stats row per distinct strategy"
    );
}

/// 策略全局可见（2026-10 操作者裁决）：boot 把每个包注册 DISABLED，随后
/// 账户启用集把未指派的 scope 整拆（rebuild DROP）。行集合若只读活
/// scope，策略页就缩成「被启用的那几个」——面板的启用按钮（assignable
/// 集合 = `strategy.list`）再也无法点亮其余策略：死锁。本牙钉死：拆除后
/// `strategy_names` / `strategy_stats` 仍报**安装面**全量，拆掉的名字行
/// `accounts` 为空、source 保留 sticky 溯源；enable 集合照旧只数活 scope。
#[test]
fn torn_down_scopes_stay_visible_on_the_strategies_surface() {
    let mut c = two_book_core();
    let cfg = c.config.clone();
    cfg.install_engine(&mut c)
        .expect("test engine (no strategy_dir: registers nothing itself)");
    let engine = c.engine.as_mut().expect("test engine");
    // Three packages installed DISABLED (the boot scan's landing state).
    crate::strategies::test_support::host_disabled(engine, Default::default(), Default::default());
    assert_eq!(c.strategy_names().len(), 3, "installed = visible");
    assert_eq!(c.strategy_stats().len(), 3);

    // The account enablement set keeps ONE strategy: the rebuild's DROP
    // phase tears the other two flat scopes down (the boot fan-out's
    // exact move; held = [] — nothing is demoted). In-process core has no
    // accounts_config_path, so the persisting half is skipped — the book
    // write + the engine rebuild are the semantic body the persisting
    // wrapper shares (persistence has its own teeth in the config tests).
    let default_id = crate::account::AccountId::from("default");
    let held: Vec<String> = Vec::new();
    c.accounts
        .set_strategies(&default_id, vec!["spread_arb".into()], 1)
        .expect("the book write lands");
    let kept = c.rebuild_account_scopes(&default_id, &["spread_arb".to_string()], &held);
    assert!(kept.is_empty(), "nothing held ⇒ no exits-only promotion");

    // The visibility contract: every INSTALLED strategy stays on the
    // surface, enabled or not.
    let names = c.strategy_names();
    assert_eq!(
        names.len(),
        3,
        "torn-down scopes must stay visible (package_sources is the installed set): {names:?}"
    );
    assert_eq!(c.strategy_stats().len(), 3, "same projection for the stats");
    for row in c.strategy_stats() {
        let name = row["name"].as_str().expect("row name");
        let accounts: Vec<&str> = row["accounts"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if name == "spread_arb" {
            assert_eq!(
                accounts,
                vec!["default"],
                "the enabled strategy names its scope"
            );
        } else {
            assert!(
                accounts.is_empty(),
                "torn-down {name} has no live scope ⇒ empty accounts, still visible"
            );
            let source = row["source"].as_str().unwrap_or("");
            assert_eq!(
                source, "test",
                "torn-down {name} keeps its sticky provenance, not a blank source"
            );
        }
    }
    // The engine's own enablement read is untouched: one live scope, on.
    assert_eq!(
        c.enabled_strategy_names(),
        vec!["spread_arb".to_string()],
        "visibility must not resurrect the torn-down scopes as running"
    );
}
