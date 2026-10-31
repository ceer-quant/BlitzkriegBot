use crate::support::*;

fn temp_path(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("bk-risk-gates-{}-{name}.json", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

// ── #173: the daily-loss breaker ────────────────────────────────────────────

/// The budget config the position-level tests use: every cooldown off, so the
/// only thing `can_open` can complain about is the daily cap.
fn budget_config(abs: Decimal, pct: Decimal, path: Option<String>) -> PositionConfig {
    PositionConfig {
        max_positions: 8,
        max_daily_loss_usd: abs,
        max_daily_loss_equity_pct: pct,
        daily_pnl_path: path,
        stop_loss_cooldown_sec: 0,
        exit_cooldown_sec: 0,
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        ..Default::default()
    }
}

/// Open `shares` at `entry` and close them at `exit`; returns the realized net
/// PnL the daily budget just absorbed.
fn realize(
    pm: &mut PositionManager,
    tok: &str,
    entry: Decimal,
    shares: Decimal,
    exit: Decimal,
    now: i64,
) -> Decimal {
    let id = position(pm, tok, entry, shares, now);
    pm.close(&id, exit, ExitReason::StopLoss, false, now + 1_000)
        .expect("the position is open")
        .net_pnl_usd
}

#[test]
fn the_cap_is_a_share_of_the_days_opening_equity_and_resets_only_across_days() {
    let mut pm = PositionManager::new(budget_config(Decimal::ZERO, dec!(20), None));
    let roll = pm
        .roll_daily("default", NOW, dec!(100), EquityBasis::Dry)
        .expect("the first tick stamps the day");
    assert_eq!(roll.day_index, utc_day_index(NOW));
    assert_eq!(roll.limit_usd, dec!(20), "20% of the $100 opening equity");

    // A 15 USD loss is inside the budget: entries stay open.
    let net = realize(
        &mut pm,
        "tok",
        dec!(0.40),
        dec!(50),
        dec!(0.10),
        NOW + 1_000,
    );
    assert!(net < dec!(-10), "expected a real loss, got {net}");
    assert!(!pm.daily_loss_tripped_for("default"));
    assert!(pm.can_open("default", None, None, NOW + 2_000).is_ok());

    // A second one crosses it, and the freeze is immediate.
    realize(
        &mut pm,
        "tok2",
        dec!(0.40),
        dec!(50),
        dec!(0.10),
        NOW + 3_000,
    );
    assert!(pm.daily_loss_tripped_for("default"), "the cap must latch");
    let err = pm
        .can_open("default", None, None, NOW + 4_000)
        .expect_err("a tripped cap must refuse new entries");
    assert!(err.contains("Daily loss limit"), "{err}");

    // Same day: a later tick does not clear it.
    assert!(
        pm.roll_daily("default", NOW + 60_000, dec!(70), EquityBasis::Dry)
            .is_none()
    );
    assert!(pm.daily_loss_tripped_for("default"));

    // A real UTC day boundary does — and re-bases the cap on the new equity.
    let roll = pm
        .roll_daily("default", NOW + DAY_MS, dec!(70), EquityBasis::Dry)
        .expect("crossing midnight UTC must roll the budget");
    assert_eq!(roll.previous_day_index, Some(utc_day_index(NOW)));
    assert_eq!(roll.day_index, utc_day_index(NOW + DAY_MS));
    assert!(roll.previous_tripped, "the closed day's state is reported");
    assert!(roll.previous_realized_pnl_usd < dec!(-20));
    assert_eq!(roll.limit_usd, dec!(14), "20% of the new $70 equity");
    assert_eq!(pm.daily_pnl_for("default"), Decimal::ZERO);
    assert!(
        !pm.daily_loss_tripped_for("default"),
        "the new day starts clean"
    );
    assert!(
        pm.can_open("default", None, None, NOW + DAY_MS + 1_000)
            .is_ok()
    );
}

#[test]
fn the_absolute_cap_still_wins_when_it_is_the_tighter_one() {
    // Absolute $5 against a 50% relative cap on $100 → $5.
    let mut pm = PositionManager::new(budget_config(dec!(5), dec!(50), None));
    pm.roll_daily("default", NOW, dec!(100), EquityBasis::Dry);
    assert_eq!(pm.effective_daily_loss_limit_for_public("default"), dec!(5));

    // …and a generous absolute cap never raises the relative one.
    let mut pm = PositionManager::new(budget_config(dec!(500), dec!(10), None));
    pm.roll_daily("default", NOW, dec!(100), EquityBasis::Dry);
    assert_eq!(
        pm.effective_daily_loss_limit_for_public("default"),
        dec!(10)
    );
}

#[test]
fn a_restart_keeps_the_days_loss_and_the_next_day_still_clears_it() {
    let path = temp_path("daily-budget");
    let cfg = budget_config(
        dec!(1),
        Decimal::ZERO,
        Some(path.to_string_lossy().into_owned()),
    );

    let mut first = PositionManager::new(cfg.clone());
    first.roll_daily("default", NOW, dec!(100), EquityBasis::Dry);
    let net = realize(
        &mut first,
        "tok",
        dec!(0.40),
        dec!(50),
        dec!(0.10),
        NOW + 1_000,
    );
    assert!(net < dec!(-1), "expected a loss past the cap, got {net}");
    assert!(first.daily_loss_tripped_for("default"));
    assert!(
        path.exists(),
        "the day's budget must be on disk, not only in memory"
    );
    drop(first);

    // A fresh process (same file) must not be able to launder today's loss.
    let mut second = PositionManager::new(cfg);
    assert_eq!(
        second.daily_state_for("default").day_index,
        Some(utc_day_index(NOW))
    );
    assert!(
        second.daily_loss_tripped_for("default"),
        "the restored budget is already spent"
    );
    let err = second
        .can_open("default", None, None, NOW + 2_000)
        .expect_err("a restored loss must gate the very first entry");
    assert!(err.contains("Daily loss limit"), "{err}");

    // The boundary still lifts it, restored budget and all.
    let roll = second
        .roll_daily("default", NOW + DAY_MS, dec!(85), EquityBasis::Dry)
        .expect("the next UTC day resets it");
    assert!(roll.previous_tripped);
    assert!(!second.daily_loss_tripped_for("default"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_trip_reaches_the_panel_and_the_audit_trail() {
    let path = temp_path("trip-alert");
    let positions = budget_config(
        dec!(1),
        Decimal::ZERO,
        Some(path.to_string_lossy().into_owned()),
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut c = dry_core(positions.clone());
    c.set_event_sink(tx);
    open_long(&mut c, "tok", dec!(0.39), dec!(0.40), dec!(50), NOW);
    let _ = drain_alerts(&mut rx);

    // Crash the book: the stop exits at ~0.05 and realizes a loss past $1.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.05), dec!(1000))],
        vec![(dec!(0.06), dec!(1000))],
        NOW + 5_000,
    );
    c.tick(NOW + 5_000).unwrap();

    assert!(c.positions().daily_pnl_for("default") < dec!(-1));
    assert!(c.positions().daily_loss_tripped_for("default"));
    let alerts = drain_alerts(&mut rx);
    assert!(
        alerts
            .iter()
            .any(|m| m.contains("DAILY LOSS LIMIT REACHED")),
        "the trip must be visible: {alerts:?}"
    );
    let stats = c.engine_stats_at(NOW + 5_000);
    assert_eq!(stats["dailyLoss"]["tripped"].as_bool(), Some(true));
    // The panel serializes decimals as strings (crate::decimal), so compare
    // the rendered value rather than its JSON variant.
    let realized = stats["dailyLoss"]["realizedPnlUsd"].to_string();
    assert!(
        realized.trim_matches('"').starts_with('-'),
        "the panel must carry the loss: {realized}"
    );
    assert_eq!(
        stats["dailyLoss"]["dayIndex"].as_i64(),
        Some(utc_day_index(NOW))
    );

    // A new entry is refused for the rest of the day…
    let err = c
        .place(
            order(
                "tok2",
                Side::Buy,
                dec!(0.40),
                dec!(10),
                "entry:tok2",
                NOW + 6_000,
            ),
            0,
            NOW + 6_000,
        )
        .expect_err("the day's cap is spent");
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
    assert!(err.message.contains("Daily loss limit"), "{}", err.message);

    // …and a restart does not give the day a second budget.
    let mut restarted = dry_core(positions);
    assert!(restarted.positions().daily_loss_tripped_for("default"));
    let err = restarted
        .place(
            order(
                "tok2",
                Side::Buy,
                dec!(0.40),
                dec!(10),
                "entry:tok2",
                NOW + 60_000,
            ),
            0,
            NOW + 60_000,
        )
        .expect_err("a restart must not launder today's loss");
    assert!(err.message.contains("Daily loss limit"), "{}", err.message);

    // Crossing the boundary re-opens entries, and the roll is announced.
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    restarted.set_event_sink(tx2);
    restarted.tick(NOW + DAY_MS).unwrap();
    assert!(!restarted.positions().daily_loss_tripped_for("default"));
    let alerts = drain_alerts(&mut rx2);
    assert!(
        alerts.iter().any(|m| m.contains("daily loss budget")),
        "the roll must be in the audit trail: {alerts:?}"
    );
    restarted.book_snapshot(
        "tok2",
        vec![(dec!(0.39), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW + DAY_MS + 1_000,
    );
    let (_, status) = restarted
        .place(
            order(
                "tok2",
                Side::Buy,
                dec!(0.40),
                dec!(10),
                "entry:tok2",
                NOW + DAY_MS + 1_000,
            ),
            0,
            NOW + DAY_MS + 1_000,
        )
        .expect("the new day's entries are open again");
    assert_eq!(status, OrderStatus::Filled);
    let _ = std::fs::remove_file(&path);
}

// ── #235: the day's base belongs to a BOOK, and a dry→live switch is a
//          different one ────────────────────────────────────────────────────

/// A live-mode core on `balance`: the money is the VENUE's here, so it is seeded
/// through the host path (`set_balance`) rather than `dry_seed_balance` — which
/// is also what makes the day's basis "live" (P1 #235).
fn live_core(positions: PositionConfig, balance: Decimal, position_log: &std::path::Path) -> Core {
    let mut c = Core::new(CoreConfig {
        mode: Mode::Live,
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            ..Default::default()
        },
        round_duration_sec: 900,
        auto_exits_enabled: true,
        positions,
        position_log_path: Some(position_log.to_string_lossy().into_owned()),
        ..Default::default()
    });
    c.set_balance(balance);
    c
}

/// A dry core on a SEEDED book, with the open-position log wired so a test can
/// carry a position across a restart the way the kernel does.
fn dry_core_at(positions: PositionConfig, seed: Decimal, position_log: &std::path::Path) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            ..Default::default()
        },
        dry_seed_balance: seed,
        round_duration_sec: 900,
        auto_exits_enabled: true,
        positions,
        position_log_path: Some(position_log.to_string_lossy().into_owned()),
        ..Default::default()
    });
    c.set_balance(seed);
    c
}

/// Everything `f` logged on this thread, as text. Thread-local, so tests
/// running in parallel do not capture each other's lines.
#[derive(Clone, Default)]
struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogSink;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

thread_local! {
    /// The buffer the current thread is capturing into. `None` outside a
    /// `with_logs` call — including on threads that never capture at all.
    static CAPTURE_SINK: std::cell::RefCell<Option<LogSink>> =
        const { std::cell::RefCell::new(None) };
}

/// The capture subscriber's writer: routes each line to the calling thread's
/// sink, discarding lines from threads that are not capturing.
struct ThreadRoutedWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadRoutedWriter {
    type Writer = LogSink;
    fn make_writer(&'a self) -> Self::Writer {
        CAPTURE_SINK
            .with(|s| s.borrow().clone())
            .unwrap_or_default()
    }
}

/// Install this suite's capture subscriber, once per test process.
///
/// #318: a callsite's interest is PROCESS-global and fixed when it is first
/// registered — and with only scoped dispatchers around, that registration
/// consults the registering thread's default subscriber. A thread with none
/// (any test that touches the callsite without `with_logs`) cached `never` for
/// the callsite, and every later capture dropped the event before any
/// subscriber saw it: the empty `{logs}` this suite flaked into (1/263). A
/// real global subscriber removes the thread without an opinion from the
/// equation — and installing it rebuilds every callsite's interest, healing
/// any that an earlier registration had already poisoned. It is also the shape
/// production runs (`main` installs one global fmt subscriber), so the tests
/// capture through the same path the run logs do.
fn install_capture() {
    static INSTALL: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INSTALL.get_or_init(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(ThreadRoutedWriter)
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        // This test binary installs no other subscriber, so this succeeds;
        // tolerate a foreign one rather than fail the suite.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

fn with_logs<R>(f: impl FnOnce() -> R) -> (R, String) {
    install_capture();
    let sink = LogSink::default();
    /// Clears this thread's route even if `f` panics, so a failed test cannot
    /// leak its sink into whatever runs next on this thread.
    struct Route(());
    impl Drop for Route {
        fn drop(&mut self) {
            CAPTURE_SINK.with(|s| *s.borrow_mut() = None);
        }
    }
    CAPTURE_SINK.with(|s| *s.borrow_mut() = Some(sink.clone()));
    let route = Route(());
    let out = f();
    drop(route);
    let text = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
    (out, text)
}

/// #318: a callsite's interest is PROCESS-global and decided by whoever
/// registers it FIRST. With only scoped dispatchers registered, the
/// registration consults the registering thread's own default subscriber — so a
/// thread with none caches `never` for the callsite, and every later capture
/// drops the event before any subscriber sees it. That is the empty `{logs}`
/// this suite flaked into (1/263). Pin the interleaving deterministically: a
/// bare thread first-touches the callsite while our capture is installed.
#[test]
fn a_bare_thread_registering_a_callsite_first_must_not_silence_the_capture() {
    // A callsite only this test touches, so its first registration is ours to
    // arrange — in the suite, that first registration races instead.
    fn probe() {
        tracing::warn!("probe: bare-thread first touch");
    }

    let (_, logs) = with_logs(|| {
        std::thread::spawn(probe)
            .join()
            .expect("the probe thread ran");
        probe(); // …and the capturing thread must still see its own warning.
    });

    assert!(
        logs.contains("bare-thread first touch"),
        "a first touch from a thread without a subscriber must not absorb the \
         callsite: {logs:?}"
    );
}

/// Read back what the day's budget persisted.
fn persisted_daily(path: &std::path::Path) -> DailyLossState {
    let text = std::fs::read_to_string(path).expect("the day's budget must be on disk");
    // The per-account envelope is the new shape; the legacy bare state (which
    // some fixtures write on purpose) reads as-is — the default book's day.
    if let Ok(file) = serde_json::from_str::<DailyLossFile>(&text) {
        return file
            .accounts
            .get("default")
            .cloned()
            .expect("the default account's day is persisted");
    }
    serde_json::from_str(&text).expect("the persisted budget must parse")
}

/// The issue's headline: `--seed-balance 10000` in dry, then live on a $50 book
/// within the same UTC day. The old code kept the dry base, so 20% of it was a
/// $2 000 cap on a $50 account — a breaker that cannot trip.
#[test]
fn a_same_day_dry_to_live_restart_rebases_the_cap_on_the_live_book() {
    let path = temp_path("basis-switch");
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    // The seeded dry run stamps the day against the simulated book.
    let mut dry = PositionManager::new(cfg.clone());
    let roll = dry
        .roll_daily("default", NOW, dec!(10_000), EquityBasis::Dry)
        .expect("the first tick stamps the day");
    assert_eq!(roll.limit_usd, dec!(2_000), "20% of the simulated $10 000");
    drop(dry);
    let stamped = persisted_daily(&path);
    assert_eq!(stamped.opening_equity_usd, dec!(10_000));
    assert_eq!(
        stamped.equity_basis,
        Some(EquityBasis::Dry),
        "the base must record which book it came from"
    );

    // Same UTC day, the operator switches to live on a $50 book.
    let mut live = PositionManager::new(cfg);
    let (rolled, logs) =
        with_logs(|| live.roll_daily("default", NOW + 60_000, dec!(50), EquityBasis::Live));
    assert!(
        rolled.is_none(),
        "a re-anchor is not a day boundary — nothing was closed"
    );
    assert_eq!(
        live.daily_state_for("default").opening_equity_usd,
        dec!(50),
        "the day's base now belongs to the live book"
    );
    assert_eq!(
        live.daily_state_for("default").equity_basis,
        Some(EquityBasis::Live)
    );
    assert_eq!(
        live.effective_daily_loss_limit_for_public("default"),
        dec!(10),
        "20% of the live $50 — NOT the $2 000 the dry seed implied"
    );

    // The switch is announced, with both numbers and both books.
    assert!(logs.contains("WARN"), "it must be a warning: {logs}");
    assert!(
        logs.contains("opening equity 10000 (dry) -> 50 (live)"),
        "the warning must carry old/new value and old/new basis: {logs}"
    );

    // …and persisted, so the next restart starts from the live book.
    let rebased = persisted_daily(&path);
    assert_eq!(rebased.opening_equity_usd, dec!(50));
    assert_eq!(rebased.equity_basis, Some(EquityBasis::Live));
    let _ = std::fs::remove_file(&path);
}

/// Write the day's state file as a kernel wrote it: `basis = None` produces the
/// PRE-#235 shape, where `openingEquityUsd` is in the file and the book it was
/// measured from is absent.
///
/// That is the shape of the issue's own evidence — the dry run stamped
/// `openingEquityUsd: 10000` and nothing said which account that was — so the
/// compatibility path is exercised by building a real [`DailyLossState`] and
/// dropping the field, rather than by hand-writing JSON that could drift from
/// what the kernel actually persists.
fn write_daily(
    path: &std::path::Path,
    day: i64,
    opening_equity: Decimal,
    basis: Option<EquityBasis>,
) {
    let mut value = serde_json::to_value(DailyLossState {
        day_index: Some(day),
        opening_equity_usd: opening_equity,
        equity_basis: basis,
        opened_at_ms: NOW,
        ..Default::default()
    })
    .expect("the day's state must serialize");
    if basis.is_none() {
        let dropped = value
            .as_object_mut()
            .expect("DailyLossState is a JSON object")
            .remove("equityBasis");
        assert!(
            dropped.is_some(),
            "the fixture must actually drop the basis field"
        );
    }
    std::fs::write(
        path,
        serde_json::to_string(&value).expect("serialize the day's state"),
    )
    .expect("write the day's state file");
}

/// The file the issue's evidence came from: a state file written BEFORE the fix
/// records no basis at all, so its $10 000 base cannot be attributed to any book.
/// An unattributable base is not a licence to keep the old cap — it must be
/// re-anchored onto the live book in front of it, and 20% of that $50 book (not
/// 20% of the $10 000 the file happened to carry) is the day's cap.
#[test]
fn a_legacy_state_file_with_no_basis_rebases_onto_the_live_book() {
    let path = temp_path("legacy-basis-live");
    write_daily(&path, utc_day_index(NOW), dec!(10_000), None);
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    // What the file carries in, and why it is the fail-open the issue reports:
    // 20% of the unattributable base is a $2 000 cap on a $50 account.
    let stale = persisted_daily(&path);
    assert_eq!(stale.day_index, Some(utc_day_index(NOW)));
    assert_eq!(stale.opening_equity_usd, dec!(10_000));
    assert_eq!(stale.equity_basis, None, "the fixture must not name a book");
    assert_eq!(stale.opening_equity_usd * dec!(20) / dec!(100), dec!(2_000));

    // Same UTC day, a live $50 book.
    let mut live = PositionManager::new(cfg);
    let (rolled, logs) =
        with_logs(|| live.roll_daily("default", NOW + 60_000, dec!(50), EquityBasis::Live));
    assert!(
        rolled.is_none(),
        "same UTC day: no day boundary was crossed"
    );
    assert_eq!(
        live.effective_daily_loss_limit_for_public("default"),
        dec!(10),
        "50 x 20% — NOT the $2 000 the unattributable base implied"
    );
    assert_eq!(live.daily_state_for("default").opening_equity_usd, dec!(50));
    assert_eq!(
        live.daily_state_for("default").equity_basis,
        Some(EquityBasis::Live)
    );
    assert!(
        logs.contains("opening equity 10000 (unrecorded) -> 50 (live)"),
        "an unrecorded basis must be named as such: {logs}"
    );
    let rebased = persisted_daily(&path);
    assert_eq!(rebased.opening_equity_usd, dec!(50));
    assert_eq!(rebased.equity_basis, Some(EquityBasis::Live));
    let _ = std::fs::remove_file(&path);
}

/// Where the missing LABEL is the only thing that can act: the book moves by less
/// than the 50% deviation fallback, so a file that names the live book is an
/// ordinary restart and a file that names nothing is re-anchored — on the same
/// $200 → $150 move.
///
/// Without this pair the rule is untested: the headline case drops 99.5%, which
/// the deviation fallback catches whether or not an unrecorded basis counts as a
/// change.
#[test]
fn a_legacy_state_file_rebases_where_a_recorded_one_would_not() {
    let day = utc_day_index(NOW);
    let labeled = temp_path("legacy-basis-control");
    write_daily(&labeled, day, dec!(200), Some(EquityBasis::Live));
    let unlabeled = temp_path("legacy-basis-discriminating");
    write_daily(&unlabeled, day, dec!(200), None);

    // Control: the file says the base came from the live book, and the live book
    // is 25% smaller — inside the deviation band — so nothing moves.
    let mut control = PositionManager::new(budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(labeled.to_string_lossy().into_owned()),
    ));
    control.roll_daily("default", NOW + 60_000, dec!(150), EquityBasis::Live);
    assert_eq!(
        control.daily_state_for("default").opening_equity_usd,
        dec!(200),
        "a restarted live book keeps the day's base"
    );
    assert_eq!(
        control.effective_daily_loss_limit_for_public("default"),
        dec!(40)
    );

    // The same numbers with no basis recorded: it re-anchors onto the live book.
    let mut legacy = PositionManager::new(budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(unlabeled.to_string_lossy().into_owned()),
    ));
    legacy.roll_daily("default", NOW + 60_000, dec!(150), EquityBasis::Live);
    assert_eq!(
        legacy.daily_state_for("default").opening_equity_usd,
        dec!(150),
        "an unattributable base re-anchors even inside the deviation band"
    );
    assert_eq!(
        legacy.effective_daily_loss_limit_for_public("default"),
        dec!(30),
        "20% of the $150 book, not 20% of the $200 the file carried"
    );
    assert_eq!(
        legacy.daily_state_for("default").equity_basis,
        Some(EquityBasis::Live)
    );

    let _ = std::fs::remove_file(&labeled);
    let _ = std::fs::remove_file(&unlabeled);
}

/// The other direction of the same file: a base that UNDERSTATES the book is not
/// re-anchored upward, because re-anchoring may only ever tighten. A same-day
/// switch must never hand the day a bigger budget than it already had, so the
/// smaller base (and its smaller cap) stays.
#[test]
fn a_legacy_state_file_never_loosens_the_cap_when_the_book_is_bigger() {
    let path = temp_path("legacy-basis-grow");
    write_daily(&path, utc_day_index(NOW), dec!(50), None);
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    let mut live = PositionManager::new(cfg);
    let (rolled, logs) =
        with_logs(|| live.roll_daily("default", NOW + 60_000, dec!(200), EquityBasis::Live));
    assert!(
        rolled.is_none(),
        "same UTC day: no day boundary was crossed"
    );
    assert_eq!(
        live.daily_state_for("default").opening_equity_usd,
        dec!(50),
        "the day keeps the tighter base it already had"
    );
    assert_eq!(
        live.effective_daily_loss_limit_for_public("default"),
        dec!(10),
        "…so the cap stays $10, not the $40 the bigger book would allow"
    );
    assert!(
        !logs.contains("re-anchored"),
        "a tightening move that does not happen must not be announced: {logs}"
    );
    let _ = std::fs::remove_file(&path);
}

/// The boundary the fix must not break: the SAME book at (roughly) the same size
/// is an ordinary restart. It must not move the day's base, must not disturb how
/// much of the day's budget is already spent, and must not warn.
#[test]
fn a_same_basis_restart_neither_rebases_the_day_nor_warns() {
    let path = temp_path("basis-same");
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    let mut first = PositionManager::new(cfg.clone());
    first.roll_daily("default", NOW, dec!(10_000), EquityBasis::Dry);
    let realized = realize(
        &mut first,
        "tok",
        dec!(0.40),
        dec!(100),
        dec!(0.10),
        NOW + 1_000,
    );
    assert!(realized < dec!(-10), "expected a real loss, got {realized}");
    drop(first);

    // Same day, same book, 5% smaller (fees and a losing trade): a restart.
    let mut second = PositionManager::new(cfg);
    let (_, logs) = with_logs(|| {
        assert!(
            second
                .roll_daily("default", NOW + 60_000, dec!(9_500), EquityBasis::Dry)
                .is_none()
        );
    });
    assert_eq!(
        second.daily_state_for("default").opening_equity_usd,
        dec!(10_000),
        "the day's base is the day's"
    );
    assert_eq!(
        second.effective_daily_loss_limit_for_public("default"),
        dec!(2_000)
    );
    assert_eq!(
        second.daily_pnl_for("default"),
        realized,
        "a restart must not disturb the day's realized loss"
    );
    assert!(
        logs.trim().is_empty(),
        "a same-basis restart must stay quiet: {logs}"
    );
    let _ = std::fs::remove_file(&path);
}

/// The basis branch ON ITS OWN: the two books happen to be the same size, so
/// only the recorded basis can tell that the day's base came from the other one.
/// A mutation that disables the basis check leaves this test the only one red —
/// the scale-move tests would still pass on the deviation fallback.
#[test]
fn a_same_day_basis_change_rebases_even_when_the_books_are_the_same_size() {
    let path = temp_path("basis-same-size");
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    let mut dry = PositionManager::new(cfg.clone());
    dry.roll_daily("default", NOW, dec!(10_000), EquityBasis::Dry);
    drop(dry);

    // Live on a $9 000 book: 10% smaller, so nothing but the basis moved.
    let mut live = PositionManager::new(cfg);
    let (_, logs) = with_logs(|| {
        live.roll_daily("default", NOW + 60_000, dec!(9_000), EquityBasis::Live);
    });
    assert_eq!(
        live.daily_state_for("default").opening_equity_usd,
        dec!(9_000)
    );
    assert_eq!(
        live.daily_state_for("default").equity_basis,
        Some(EquityBasis::Live)
    );
    assert_eq!(
        live.effective_daily_loss_limit_for_public("default"),
        dec!(1_800),
        "20% of the live $9 000 — the dry seed's number is not this book's"
    );
    assert!(
        logs.contains("opening equity 10000 (dry) -> 9000 (live)"),
        "{logs}"
    );
    let _ = std::fs::remove_file(&path);
}

/// The fallback the basis alone cannot cover: the label is unchanged but the
/// ACCOUNT is not (a re-seeded dry run, a deposit, a withdrawal).
#[test]
fn a_same_basis_but_rescaled_book_rebases_the_day_too() {
    let path = temp_path("basis-rescale");
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    let mut first = PositionManager::new(cfg.clone());
    first.roll_daily("default", NOW, dec!(10_000), EquityBasis::Dry);
    drop(first);

    // Same label, a hundredth of the book: `--seed-balance 50`.
    let mut second = PositionManager::new(cfg);
    let (_, logs) = with_logs(|| {
        second.roll_daily("default", NOW + 60_000, dec!(50), EquityBasis::Dry);
    });
    assert_eq!(
        second.effective_daily_loss_limit_for_public("default"),
        dec!(10),
        "the cap must follow the book that is actually trading"
    );
    assert!(
        logs.contains("opening equity 10000 (dry) -> 50 (dry)"),
        "{logs}"
    );
    let _ = std::fs::remove_file(&path);
}

/// The re-anchor may only ever TIGHTEN. A bigger book keeps the day's smaller
/// base: handing the day a budget it never had is the fail-open direction.
#[test]
fn a_rebase_never_loosens_the_days_cap() {
    let path = temp_path("basis-grow");
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    // A dry $100 day, then a live $50 000 book on the same UTC day.
    let mut first = PositionManager::new(cfg.clone());
    first.roll_daily("default", NOW, dec!(100), EquityBasis::Dry);
    drop(first);

    let mut second = PositionManager::new(cfg);
    second.roll_daily("default", NOW + 60_000, dec!(50_000), EquityBasis::Live);
    assert_eq!(
        second.daily_state_for("default").opening_equity_usd,
        dec!(100),
        "the day keeps the tighter base it already had"
    );
    assert_eq!(
        second.effective_daily_loss_limit_for_public("default"),
        dec!(20)
    );
    let _ = std::fs::remove_file(&path);
}

/// Suggestion 3: a re-anchor that lands UNDER the day's realized loss must
/// freeze the day on the spot — the tightened cap is already spent, and the day
/// must never read as healthy again.
#[test]
fn a_rebase_below_the_days_realized_loss_trips_it_immediately() {
    let path = temp_path("rebase-trip");
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    // Dry, $10 000: a −$30 loss is deep inside the $2 000 cap.
    let mut dry = PositionManager::new(cfg.clone());
    dry.roll_daily("default", NOW, dec!(10_000), EquityBasis::Dry);
    let realized = realize(
        &mut dry,
        "tok",
        dec!(0.40),
        dec!(100),
        dec!(0.10),
        NOW + 1_000,
    );
    assert!(realized <= dec!(-30), "expected ≈ −$30, got {realized}");
    assert!(realized > dec!(-2_000), "…but inside the seeded cap");
    assert!(!dry.daily_loss_tripped_for("default"));
    assert!(dry.can_open("default", None, None, NOW + 2_000).is_ok());
    drop(dry);

    // Live, $50, same UTC day: the cap is now $10 and the day is already past it.
    let mut live = PositionManager::new(cfg.clone());
    let (_, logs) = with_logs(|| {
        live.roll_daily("default", NOW + 60_000, dec!(50), EquityBasis::Live);
    });
    assert_eq!(
        live.effective_daily_loss_limit_for_public("default"),
        dec!(10)
    );
    assert!(
        live.daily_loss_tripped_for("default"),
        "a spent, tightened cap must freeze the day instead of healing it"
    );
    assert!(
        live.daily_state_for("default").tripped,
        "…and the trip must be LATCHED on the spot, not left to be re-derived"
    );
    assert_eq!(
        live.daily_state_for("default").tripped_limit_usd,
        dec!(10),
        "the frozen cap is the live one"
    );
    assert!(
        logs.contains("already spent"),
        "the freeze must be part of the same warning: {logs}"
    );
    let err = live
        .can_open("default", None, None, NOW + 61_000)
        .expect_err("entries must be refused");
    assert!(err.contains("Daily loss limit"), "{err}");

    // …and it is persisted: a further restart cannot launder it either.
    drop(live);
    let again = PositionManager::new(cfg);
    assert!(again.daily_loss_tripped_for("default"));
    assert_eq!(
        again.effective_daily_loss_limit_for_public("default"),
        dec!(10)
    );
    let _ = std::fs::remove_file(&path);
}

/// The same thing end to end, through the kernel's own entry points: a seeded
/// dry run leaves the day stamped at $10 000 AND a position on the book; the
/// live process restores the position, re-anchors the day onto its $50, trips
/// it, refuses the next entry — and still lets the open position out (#174's
/// guarantee, re-checked on this new trip path).
#[test]
fn a_dry_to_live_rebase_trips_the_day_and_still_lets_the_position_out() {
    let day_path = temp_path("rebase-trip-core-day");
    let log_path = temp_path("rebase-trip-core-positions");
    let positions = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(day_path.to_string_lossy().into_owned()),
    );

    // (1) The seeded dry run (`--seed-balance 10000`): the day is stamped
    //     against $10 000, a loss lands well inside that cap, and one position
    //     is left open.
    let mut dry = dry_core_at(positions.clone(), dec!(10_000), &log_path);
    dry.tick(NOW).unwrap();
    assert_eq!(
        dry.positions()
            .daily_state_for("default")
            .opening_equity_usd,
        dec!(10_000)
    );
    open_long(
        &mut dry,
        "tokA",
        dec!(0.39),
        dec!(0.40),
        dec!(100),
        NOW + 1_000,
    );
    dry.book_snapshot(
        "tokA",
        vec![(dec!(0.05), dec!(1000))],
        vec![(dec!(0.06), dec!(1000))],
        NOW + 2_000,
    );
    dry.tick(NOW + 2_000).unwrap();
    let realized = dry.positions().daily_pnl_for("default");
    assert!(
        realized <= dec!(-30) && realized > dec!(-2_000),
        "a loss inside the seeded cap, past the live one: {realized}"
    );
    assert!(!dry.positions().daily_loss_tripped_for("default"));
    open_long(
        &mut dry,
        "tokB",
        dec!(0.39),
        dec!(0.40),
        dec!(10),
        NOW + 3_000,
    );
    assert_eq!(dry.positions().open_positions().len(), 1);
    drop(dry);

    // (2) Same UTC day, live on a $50 book: the open position comes back with
    //     the process (crash recovery); the day comes back carrying the dry
    //     base, which is the window the issue is about.
    let mut c = live_core(positions, dec!(50), &log_path);
    assert_eq!(
        c.restore_positions(),
        1,
        "the open position survives the restart"
    );
    assert_eq!(c.ledger().balance(), dec!(50), "the live book is $50");
    assert_eq!(
        c.positions()
            .effective_daily_loss_limit_for_public("default"),
        dec!(2_000),
        "before the first tick the day still carries the dry base"
    );

    // (3) The first tick re-anchors the day onto the live book, and the day's
    //     realized loss is already past the $10 cap that book allows.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.tick(NOW + 60_000).unwrap();
    assert_eq!(
        c.positions()
            .effective_daily_loss_limit_for_public("default"),
        dec!(10)
    );
    assert_eq!(
        c.positions().daily_state_for("default").opening_equity_usd,
        dec!(50)
    );
    assert_eq!(
        c.positions().daily_state_for("default").equity_basis,
        Some(EquityBasis::Live)
    );
    assert!(
        c.positions().daily_loss_tripped_for("default"),
        "the day must be frozen, not quietly healed"
    );
    let alerts = drain_alerts(&mut rx);
    assert!(
        alerts
            .iter()
            .any(|m| m.contains("DAILY LOSS LIMIT REACHED")),
        "the freeze must reach the operator: {alerts:?}"
    );
    // The panel states the fact too: a $10 cap measured against a LIVE book.
    let stats = c.engine_stats_at(NOW + 60_000);
    assert_eq!(stats["dailyLoss"]["equityBasis"].as_str(), Some("live"));
    assert_eq!(
        stats["dailyLoss"]["openingEquityUsd"]
            .to_string()
            .trim_matches('"'),
        "50"
    );
    assert_eq!(
        stats["dailyLoss"]["limitUsd"].to_string().trim_matches('"'),
        "10"
    );

    // (4) A new entry is refused…
    let err = c
        .place(
            order(
                "tokC",
                Side::Buy,
                dec!(0.40),
                dec!(10),
                "entry:tokC",
                NOW + 61_000,
            ),
            0,
            NOW + 61_000,
        )
        .expect_err("a spent, tightened cap must refuse entries");
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
    assert!(err.message.contains("Daily loss limit"), "{}", err.message);

    // (5) …and the position that was already open still gets out.
    c.book_snapshot(
        "tokB",
        vec![(dec!(0.38), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW + 62_000,
    );
    assert_eq!(
        c.flatten(None, NOW + 62_000).unwrap(),
        1,
        "the closing sell must be submitted while the day is tripped"
    );
    let _ = std::fs::remove_file(&day_path);
    let _ = std::fs::remove_file(&log_path);
}
