//! The multi-account money truth and its boot/persistence plumbing, moved
//! verbatim from `service.rs` (pure code motion, no logic edits): balance
//! seeding and venue-cash realignment (with the #702 windowed venue-evidence
//! family — `window_cash_facts` / `report_venue_cash_move` / ghost alerts —
//! kept together), the accounts.toml persisting writers, the cross-venue arb
//! config and RFC 568 pair book, the in-kernel accounting audit, the venue
//! reconcile sweep with external-fill folding, the order/position restore
//! helpers and the expiry/order sweeps that guard them.

use crate::ipc::schema::Event;
use crate::ledger::Ledger;
use crate::model::*;
use crate::reconcile::{AuditInput, AuditReport, CashIdentity};
use rust_decimal::Decimal;
use std::collections::HashSet;

use super::helpers::{FREEZE_AUTO_LIFT_MIN_MS, SWEEP_FAILURE_FREEZE, f64_to_decimal};
use super::{AUDIT_HEARTBEAT_MS, PLACE_TIMEOUT_VERDICT_MS, build_lua_scope, fee_quote};
use blitzkrieg_market_api::net::now_ms;

use super::Core;

impl Core {
    /// Venue order ids the OME believes are resting (orphan detection input).
    pub fn known_venue_ids(&self) -> std::collections::HashSet<String> {
        self.ome.known_venue_ids()
    }

    /// Cancel a venue order the OME has no record of. Used by the startup sweep
    /// to clear orphans left by a previous process/crash. No local state exists
    /// for these ids, so this is purely a venue-side action; the caller performs
    /// the actual cancel and reports the id here for logging/visibility.
    pub fn note_orphan_cancelled(&self, venue_order_id: &str) {
        self.emit(Event::RiskAlert {
            code: CoreErrorCode::Internal,
            message: format!("cancelled resting venue order {venue_order_id}"),
        });
    }

    pub fn mode(&self) -> Mode {
        self.config.mode
    }

    /// The instant this core's accounting session began (epoch ms) — see the
    /// field doc. Exposed read-only over `core.ready` so an external audit can
    /// scope the seeded cash identity to the trades this process can be
    /// responsible for (issue #200).
    pub fn started_at_ms(&self) -> i64 {
        self.started_at_ms
    }

    /// Read-only fee quote (#182) — the `core.feeQuote` entry point, so a gate or
    /// a reconcile script asks the kernel for the schedule it is charging instead
    /// of carrying its own copy of the formula. It sits on `Core` rather than
    /// being a bare function because the day the schedule becomes a per-core
    /// setting (the `--taker-fee-legacy` switch in flight) the lookup belongs
    /// here, and only here.
    pub fn fee_quote(&self, price: Option<Decimal>) -> crate::ipc::schema::FeeQuoteResult {
        fee_quote(price)
    }

    /// Seed the ACTIVE account's ledger cash (the boot `dry_seed_balance` and
    /// the reset paths land here). This is the process-default book's funding —
    /// NOT a venue report: venue cash truth flows through
    /// [`Self::realign_venue_cash`], which is where the audit's venue leg is fed.
    pub fn set_balance(&mut self, b: Decimal) {
        self.accounts.active_ledger_mut().set_balance(b);
    }

    /// Fold one venue's reported cash into the account bound to that venue
    /// (multi-account #532: a venue's balance belongs to the account that
    /// trades it, NEVER to whoever is "active" — the single-account-era
    /// routing here once wrote the PolySimulator's 100 USDC over the default
    /// account's real 13.59). #646: with several books on file an UNBOUND
    /// venue's report is REFUSED (warn + risk alert, nothing written) — an
    /// unowned money fact must not silently land on whoever is active. A
    /// single-account deployment keeps the legacy shape: its one book IS the
    /// active ledger, so the report lands there exactly as before.
    pub fn realign_venue_cash(&mut self, venue: &str, b: Decimal) {
        let Some(target) = self.venue_cash_account(venue) else {
            self.refuse_unbound_venue_cash(venue, b, "realign");
            return;
        };
        if self.config.mode == Mode::Live {
            let ts = now_ms();
            let previous = self
                .accounts
                .ledger_of(&target)
                .map(|l| l.balance())
                .unwrap_or_default();
            // 幽灵成交探测器（issue 692/#702）：把「场地在我不知情时动了
            // 钱」变成操作者第一时间可见的事实 —— 判据是残差对账（本次
            // 报数差减去窗口内台账已记账的现金事实），详论与告警见
            // `report_venue_cash_move`。#702： EVERY report feeds the
            // detector — including one equal to the book, which is how the
            // venue's truth baseline and the evidence cursor get established;
            // gating on `previous != b` left the baseline unwritten whenever
            // the seeding report matched the book, and the next real delta
            // was then judged against nothing.
            self.report_venue_cash_move(venue, &target, previous, b, ts);
            if previous != b {
                // The move is recorded PER VENUE so the audit's trade-leg check
                // can name the venue's own correction on ITS book only — a
                // sibling venue's realign must never explain this book's
                // residual.
                self.audit
                    .last_realign
                    .insert(venue.to_string(), (ts, b - previous));
                tracing::info!(
                    account = %target.as_str(),
                    venue = %venue,
                    previous = %previous,
                    reported = %b,
                    "venue cash realigned into its bound account (no resting commitment)"
                );
            }
            self.venue_free_by_venue.insert(venue.to_string(), (ts, b));
        }
        if let Ok(ledger) = self.accounts.get_mut(&target) {
            ledger.set_balance(b);
        }
        tracing::debug!(account = %target.as_str(), venue = %venue, balance = %b, "venue cash routed");
    }

    /// The absolute tolerance a residual may carry and still read as
    /// explained (#702). The reconciliation fold itself quantizes to cents,
    /// so anything under a cent is rounding, not a money fact. (A plain fn:
    /// `Decimal::new` is not const-callable.)
    pub(super) fn residual_eps() -> Decimal {
        Decimal::new(1, 2) // 0.01
    }

    /// #702: the cash facts THIS BOOK journalled inside the current evidence
    /// window, SIGNED — the net the ledger says cash moved since the venue's
    /// truth reports last answered for the book. Debits (buy fills, fees)
    /// are negative, credits (sell fills, settlements, cashflow) positive.
    ///
    /// The window is the [`Self::venue_fact_cursor`]: journal rows at or
    /// below it are reconciled history, rows above it are facts the venue's
    /// current number has not been checked against. A cursor, not a
    /// timestamp — journal rows stamp real wall clock while replay/tests
    /// drive virtual clocks, and comparing the two bases silently swallows
    /// every fact.
    ///
    /// issue 723: the cursor is a GLOBAL row seq, not a row count — the
    /// bounded journal window's length freezes at `JOURNAL_CAP`, so a
    /// count-keyed cursor skipped everything forever after (booked read 0,
    /// every legal correction read as unattributed). `skip` below is the
    /// number of RETAINED rows at or below the cursor — the cursor itself
    /// stopped being an index the moment eviction began.
    ///
    /// Realign rows are excluded (they are the corrections being judged, not
    /// evidence for them), as are Seed/Reset (boot and operator reseeds are
    /// not the venue's story).
    pub(super) fn window_cash_facts(
        &self,
        venue: &str,
        target: &crate::account::AccountId,
    ) -> Decimal {
        let cursor = self.venue_fact_cursor.get(venue).copied().unwrap_or(0);
        let Ok(ledger) = self.accounts.ledger_of(target) else {
            return Decimal::ZERO;
        };
        let snapshot = ledger.journal_snapshot();
        let (total, len) = ledger.journal_seq_basis();
        // snapshot[i] carries seq `total − len + i`; the window is every row
        // with seq > cursor, so the retained prefix to skip is the rows with
        // seq <= cursor (`cursor + len − total`, saturating).
        let skip = cursor.saturating_add(len as u64).saturating_sub(total) as usize;
        snapshot
            .iter()
            .enumerate()
            .skip(skip)
            .filter(|(_, e)| {
                !matches!(
                    e.kind,
                    crate::ledger::JournalKind::Realign
                        | crate::ledger::JournalKind::Seed
                        | crate::ledger::JournalKind::Reset
                )
            })
            .map(|(_, e)| e.amount)
            .sum()
    }

    /// issue 723: how many rows the venue's cursor had NOT yet answered for
    /// that the bounded journal window has since evicted (0 = the window is
    /// contiguous down to the cursor). A positive count means
    /// [`Self::window_cash_facts`] is a lower bound — the residual verdict
    /// must be skipped, not faked (see `report_venue_cash_move`).
    fn journal_evidence_gap(&self, venue: &str, target: &crate::account::AccountId) -> u64 {
        let Some(cursor) = self.venue_fact_cursor.get(venue).copied() else {
            return 0; // no baseline: nothing was consumed, nothing can be lost
        };
        let Ok(ledger) = self.accounts.ledger_of(target) else {
            return 0;
        };
        let (total, len) = ledger.journal_seq_basis();
        // Rows with seq <= total − len are gone; of those, the ones past the
        // cursor were unconsumed evidence. (A cursor beyond `total` — a
        // cursor that met a rebuilt ledger — is the same loss, other end.)
        (total.saturating_sub(len as u64)).saturating_sub(cursor) + cursor.saturating_sub(total)
    }

    /// The venue ledger's newest global row seq — where a cursor write
    /// anchors (issue 723): always on the last row this ledger appended.
    fn journal_head(&self, target: &crate::account::AccountId) -> u64 {
        self.accounts
            .ledger_of(target)
            .map(|l| l.journal_seq_basis().0)
            .unwrap_or(0)
    }

    /// 幽灵成交探测器（issue 692）：the realign channel's alert.
    ///
    /// The 2026-10-28 11:31 incident: the venue's cash moved
    /// (13.596906 → 7.898029) with the kernel knowing NOTHING about it. The
    /// accounting was later proven correct — reconcile found the gap, the
    /// journal `ref=venue` realign row booked it honestly — but the whole chain
    /// was SILENT: this channel used to write the journal row and say nothing,
    /// so the anomaly surfaced only days later through a manual audit. The
    /// correction row is bookkeeping; the venue having moved money the kernel
    /// cannot attribute is a RISK FACT, and it must be loud.
    ///
    /// Same shape as [`Self::emit_late_fill`] (ipc/schema.rs is not this
    /// change's territory): the existing `RiskAlert` event carries a greppable
    /// `unattributed_cash_move …` line, the same fact lands in the audit JSONL,
    /// and `tracing` + stderr say it too.
    ///
    /// #702 决策修订（窗口化证据，替代 #700 的终身订单计数模型）：判据不是
    /// 「这本账有没有交易史」，而是「本次差额能否被当前对账窗口内的账务事实
    /// 解释」。残差 = (reported − previous) − 窗口内台账净记账（fee 零头以
    /// [`Self::residual_eps`] 容忍）；|残差| ≤ ε → 正常修正静默；否则有内核
    /// 解释不了的异动，告警。终身计数被证明过宽（一张零成交订单或很早以前
    /// 的成交会永久豁免之后的所有未知变动）；窗口模型下，已被报数收编的
    /// 旧事实、拒单与零成交取消单（从不进台账），都不再构成免警凭据。
    ///
    /// 去重与重臂（#702 语义）：告警槽按 venue 记「这个残差已经报过」。
    /// 残差一旦变化（新的未知异动）或恢复可解释（清槽），随之重置 —— 同一
    /// 异常只响一次，恢复后的新异常重新响。不靠会话生命周期。
    pub(super) fn report_venue_cash_move(
        &mut self,
        venue: &str,
        target: &crate::account::AccountId,
        previous: Decimal,
        reported: Decimal,
        now_ms: i64,
    ) {
        if previous == Decimal::ZERO && !self.venue_truth_last.contains_key(venue) {
            // The boot adoption, not a move: a LIVE book wakes at zero and its
            // FIRST venue report IS the balance (`realign_venue_cash` is the
            // seeding path — #646's regression pins this silence). Nothing was
            // "moved"; there was nothing to move. The gate fires only for a
            // venue with no prior truth on file — a book folded back to zero
            // mid-session is a REAL move and stays under the detector. The
            // cursor still starts here: everything journalled before this
            // report is the book's opening state, not a move to explain.
            let seen = self.journal_head(target);
            self.venue_fact_cursor.insert(venue.to_string(), seen);
            self.venue_truth_last.insert(venue.to_string(), reported);
            return;
        }
        // The venue's OWN last report is the comparison base — not the ledger
        // balance: a fill the venue already reported moves the book between
        // venue reports, and diffing against the book would read a fully
        // explained correction as an anomaly.
        let previous_venue_report = self.venue_truth_last.get(venue).copied();
        // The cursor read must precede the cursor write: `booked` is the
        // window's evidence, the advance below closes that window. An
        // EQUAL-value report deliberately does not advance — facts booked
        // between two equal reports happened after the venue's last word and
        // belong to the still-open window; a CHANGED report has been judged
        // against everything before it, so the cursor moves past them.
        let changed = previous_venue_report != Some(reported);
        let booked = self.window_cash_facts(venue, target);
        // issue 723: the bounded window may have EVICTED unconsumed rows past
        // the cursor since the last report — `booked` is then a lower bound,
        // and every verdict built on it (silent pass or unattributed alert)
        // would be fiction. Detect BEFORE the cursor write below closes this
        // window.
        let evicted = self.journal_evidence_gap(venue, target);
        if evicted > 0 || changed {
            // A changed report closes a judged window (the advance above);
            // an eviction re-anchors past the void — the old window is
            // unjudgeable and nothing in it may carry into the next one.
            let seen = self.journal_head(target);
            self.venue_fact_cursor.insert(venue.to_string(), seen);
        }
        self.venue_truth_last.insert(venue.to_string(), reported);

        if evicted > 0 {
            // The honest gap handling: a loud, once-per-episode alert (the
            // re-anchor above makes the next report see a contiguous
            // window), and NO residual verdict for this report — the truth
            // still advanced, so the next window's residual starts from here
            // instead of compounding the lost evidence.
            let message = format!(
                "cash_evidence_gap venue={} account={} evicted_rows={} — journal rows the \
                 venue's evidence cursor had not yet answered for were evicted by the \
                 bounded journal window (issue 723); this window's booked evidence is \
                 incomplete, the residual verdict is SKIPPED for this report and the \
                 cursor re-anchors at the newest row",
                venue,
                target.as_str(),
                evicted,
            );
            tracing::error!(
                venue = %venue,
                account = %target.as_str(),
                evicted_rows = evicted,
                "journal window evicted unconsumed rows past the venue's evidence \
                 cursor — window skipped (issue 723)"
            );
            // Static line on stderr; the figures live in the structured
            // channels (tracing fields, RiskAlert, audit JSONL) — the same
            // cleartext-logging posture as the unattributed alert below.
            eprintln!(
                "core: cash_evidence_gap — the venue evidence window lost unconsumed rows to journal eviction; see RiskAlert + audit log (issue 723)"
            );
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: message.clone(),
            });
            self.persist_audit_value(serde_json::json!({
                "atMs": now_ms,
                "event": "cash_evidence_gap",
                "venue": venue,
                "account": target.as_str(),
                "evictedRows": evicted,
            }));
            return;
        }

        // Explanation identity (#702): the venue's move is explained exactly
        // when `reported − its last report == booked` — the venue moved cash
        // by the signed net the ledger booked this window. What is LEFT is
        // the part no kernel fact accounts for, signed in the venue-move
        // direction: negative means the venue is SHORT vs the book's story
        // (the 11:31 shape), positive means extra cash appeared.
        let previous = previous_venue_report.unwrap_or(previous);
        let residual = (reported - previous) - booked;
        if residual.abs() <= Self::residual_eps() {
            // Every cent of the venue's move is accounted for by facts the
            // kernel booked INSIDE this window (fills, fees, settlements,
            // cashflow) — a normal correction. Silence, and a prior alert's
            // anomaly is over: the book is back to explainable.
            self.ghost_cash_alerted.remove(venue);
            return;
        }
        if self.plugin_modes.iter().any(|p| {
            p.name == venue
                && p.modes.iter().any(|m| {
                    m.capabilities
                        .satisfies(blitzkrieg_market_api::MarketCapabilities::SIMULATED)
                })
        }) {
            // A SIMULATED-declared (paper) venue: its reported free cash is
            // the simulation's own bookkeeping (freezes on resting BUYs, never
            // nets reservations back — the #669 arithmetic), not a money fact
            // about anyone's funds. The audit's venue leg already skips it for
            // the same reason; alerting here would re-create that noise. The
            // detector exists for venues that hold REAL cash.
            return;
        }
        // Dedup key = the residual itself, quantized to cents: the same
        // unexplained gap replaying on every reconcile cycle alerts once; a
        // CHANGED residual is a new fact and re-alerts. `round_dp(2)` first
        // — a raw `mantissa()/100` collapses scale-0 integers (−10, −30,
        // +30 all → 0) into one false-equal key.
        let residual_key = residual.round_dp(2).mantissa();
        if self.ghost_cash_alerted.get(venue) == Some(&residual_key) {
            return;
        }
        self.ghost_cash_alerted
            .insert(venue.to_string(), residual_key);
        let delta = reported - previous;
        let message = format!(
            "unattributed_cash_move venue={} account={} previous={} reported={} delta={} \
             booked_in_window={} residual={} — the venue moved cash the kernel cannot \
             explain from THIS window's ledger facts (issue 702 windowed evidence); \
             booked as a realign (ref=venue), attributed to NOBODY",
            venue,
            target.as_str(),
            previous,
            reported,
            delta,
            booked,
            residual,
        );
        tracing::error!(
            venue = %venue,
            account = %target.as_str(),
            previous = %previous,
            reported = %reported,
            delta = %delta,
            booked_in_window = %booked,
            residual = %residual,
            "venue cash move exceeds this window's booked evidence (ghost detector, issues 692+702)"
        );
        // Static line on stderr: the figures live in the structured channels
        // above/below (tracing fields, RiskAlert, audit JSONL). Interpolating
        // them here too is the exact shape CodeQL's cleartext-logging check
        // flags in this file (#693's test-assertion fix, same family) — and
        // stderr adds nothing those three channels don't already carry.
        eprintln!(
            "core: unattributed_cash_move — venue cash moved with zero kernel trade/order records; see RiskAlert + audit log (issue 692)"
        );
        self.emit(Event::RiskAlert {
            code: CoreErrorCode::Internal,
            message: message.clone(),
        });
        self.persist_audit_value(serde_json::json!({
            "atMs": now_ms,
            "event": "unattributed_cash_move",
            "venue": venue,
            "account": target.as_str(),
            "previous": previous.to_string(),
            "reported": reported.to_string(),
            "delta": delta.to_string(),
            "bookedInWindow": booked.to_string(),
            "residual": residual.to_string(),
        }));
    }

    /// The account a venue's cash truth belongs to: the first ACTIVE account
    /// bound to the venue. #646: no binding falls back to the active ledger
    /// only while the book holds EXACTLY ONE account (the legacy
    /// single-account shape — that one book is the active ledger, so the
    /// fallback is the same destination the report always had); with several
    /// books on file an unbound venue has no owner and this returns None —
    /// the callers refuse the report instead of writing into whoever is
    /// active (an unowned money fact never lands on any book).
    pub(super) fn venue_cash_account(&self, venue: &str) -> Option<crate::account::AccountId> {
        if let Some(a) = self.accounts.account_for_venue(venue).map(|a| a.id.clone()) {
            return Some(a);
        }
        // Legacy single-account shape: one book, it is the active ledger.
        (self.accounts.account_list().len() == 1).then(|| self.accounts.active_id().clone())
    }

    /// #646: an unbound venue's cash report in a multi-account deployment —
    /// refused, loudly. Nothing is written (no ledger, no realign/venue-free
    /// slot): the money fact has no owner, and writing it into the account
    /// that happens to be active would let one venue's report mutate a
    /// sibling book's risk input. The operator fixes it by binding the venue
    /// (`account.venue.set`); the data plane recovers on the next report.
    pub(super) fn refuse_unbound_venue_cash(&self, venue: &str, reported: Decimal, path: &str) {
        tracing::warn!(
            venue = %venue,
            reported = %reported,
            path = %path,
            "venue cash report REFUSED: no account bound to this venue — \
             bind one with account.venue.set; nothing written"
        );
        self.emit(Event::RiskAlert {
            code: CoreErrorCode::Internal,
            message: format!(
                "venue cash report [{venue}] refused: no account bound to this \
                 venue (multi-account deployment) — bind one with \
                 account.venue.set; nothing written ({path})"
            ),
        });
    }

    /// The periodic venue-cash fold (`venue_free_balance`): with no resting
    /// commitments the venue's free cash IS the book's total (a fill the bot
    /// missed shows up as cash that left with no fill event — the drift
    /// killer); with resting orders the venue may or may not net them out of
    /// the reported balance, so a wide gap is SURFACED, never auto-corrected
    /// (double-counting risk). Comparison runs against the venue's own bound
    /// account, not the active one.
    pub fn fold_venue_free(&mut self, venue: &str, free: Decimal) {
        let Some(target) = self.venue_cash_account(venue) else {
            self.refuse_unbound_venue_cash(venue, free, "fold");
            return;
        };
        let Ok(ledger) = self.accounts.ledger_of(&target) else {
            return;
        };
        let (balance, reserved) = (ledger.balance(), ledger.reserved());
        if reserved == Decimal::ZERO {
            if balance != free {
                self.realign_venue_cash(venue, free);
                tracing::warn!(venue = %venue, ledger = %balance, venue_free = %free, "venue cash reconciled to the venue's report");
                eprintln!("core: venue cash reconcile — see the warn log line above");
            }
        } else {
            let expected_free = balance - reserved;
            if (expected_free - free).abs() > Decimal::new(2, 2) {
                tracing::warn!(venue = %venue, ledger = %balance, reserved = %reserved, venue_free = %free, "venue cash drift (resting orders; not auto-corrected)");
                eprintln!("core: venue cash drift — see the warn log line above");
            }
        }
    }

    /// The active account's ledger (§9.5). Views and callers with no order
    /// context read this; money-moving sites route by the order's own
    /// `account_id` instead.
    pub fn ledger(&self) -> &Ledger {
        self.accounts.active_ledger()
    }

    /// Whether the ACTIVE account's credentials loaded at boot (§9.4). The
    /// process-level balance view (`ledger.balance`) follows this account, so
    /// its live-mode zero-projection (issue 787) keys on the same fact the
    /// per-account rows carry.
    pub fn active_ledger_credentials_loaded(&self) -> bool {
        self.accounts
            .account(self.accounts.active_id())
            .map(|a| a.credential_keys.loaded)
            .unwrap_or(false)
    }

    /// Whether THIS core's boot replayed durable cash journals (at least one
    /// account's `data/ledger/<account>.journal.jsonl` existed on disk). The
    /// server boot reads it to skip the post-construction seed: a replayed
    /// balance is a boot fact, and re-seeding it would reset the account to
    /// the seed and vaporize the盈亏 history the journal exists to keep.
    pub fn boot_replayed_journal(&self) -> bool {
        self.boot_replayed_journal
    }

    /// #388: the wallet-level cash the core credited from the stream —
    /// `(rebates, rebates_usd, rewards, rewards_usd)`. The replay driver's
    /// report reads this so the report and the ledger cannot disagree about
    /// what was paid. `CoreStats` is private; this is the read window.
    pub(crate) fn cashflow_stats(&self) -> (u64, Decimal, u64, Decimal) {
        (
            self.stats.cashflow_rebates,
            self.stats.cashflow_rebates_usd,
            self.stats.cashflow_rewards,
            self.stats.cashflow_rewards_usd,
        )
    }

    /// Modeled maker rebate credited on MAKER fills (`--maker-rebate`, almach
    /// REPORT §5 失真点 5). The replay driver's report reads this so the
    /// modeled pool-share cash is visible next to #388's real rows.
    pub(crate) fn modeled_maker_rebate_usd(&self) -> Decimal {
        self.stats.modeled_maker_rebate_usd
    }

    /// E28 (§9.3): the account book — IPC `account.*` reads it, the gates
    /// route through it. Money-moving code does NOT go through this handle;
    /// it routes by the order's own `account_id` at each site.
    pub fn accounts(&self) -> &crate::account::AccountLedgers {
        &self.accounts
    }

    /// E28 `account.list` (§12.1): one [`crate::ipc::schema::AccountView`]
    /// per configured account, sorted by id (`AccountLedgers::account_list`
    /// order). Every money field is the account's OWN book — this view is
    /// how cross-account isolation is READ; `dayRealizedUsd` sums the
    /// account's own closes of the current UTC day.
    ///
    /// issue 787（live 无凭证余额显示面）: a LIVE account whose credentials
    /// never loaded has no venue to report its cash — the bridge stays inert
    /// (`spawn_if_configured` refuses without keys), so no realign can ever
    /// land and the only numbers the book can hold are leftovers (a dry-mode
    /// journal replayed at boot, a venue report from before the keys were
    /// pulled). Reading those as "the balance" shows dry seed money in a live
    /// panel — the exact 1000 the operator reported. The projection states the
    /// truth instead: no loaded credentials, no venue truth, ZERO. Dry and
    /// read-only keep the seed (their book is the money there is); a loaded
    /// live account keeps its book (venue facts flow in through
    /// `realign_venue_cash`).
    pub fn account_views(&self, now_ms: i64) -> Vec<crate::ipc::schema::AccountView> {
        let live = self.config.mode == Mode::Live;
        let today = crate::position::utc_day_index(now_ms);
        self.accounts
            .account_list()
            .iter()
            .map(|a| {
                let id = &a.id;
                let ledger = self
                    .accounts
                    .ledger_of(id)
                    .expect("configured account always has a ledger");
                let open_positions = self
                    .positions
                    .open_positions()
                    .iter()
                    .filter(|p| &p.account_id == id)
                    .count();
                let day_realized_usd = self
                    .positions
                    .closed_positions()
                    .iter()
                    .filter(|c| {
                        &c.account_id == id
                            && crate::position::utc_day_index(c.exited_at_ms) == today
                    })
                    .map(|c| c.net_pnl_usd)
                    .sum();
                let (status_tag, status_reason) = match &a.status {
                    crate::account::AccountStatus::Suspended { reason } => {
                        (a.status.wire_tag(), Some(reason.clone()))
                    }
                    _ => (a.status.wire_tag(), None),
                };
                // The zero-fold only ever hits a live account whose
                // credentials did not load — `loaded == false` there. The
                // reserve gate reads the same view, and refusing entries on a
                // zero book is exactly right: an unauthenticated account must
                // not spend anything.
                let unfunded_live = live && !a.credential_keys.loaded;
                let (balance, available) = if unfunded_live {
                    (Decimal::ZERO, Decimal::ZERO)
                } else {
                    (ledger.balance(), ledger.available())
                };
                crate::ipc::schema::AccountView {
                    id: id.as_str().to_string(),
                    name: a.name.clone(),
                    market_type: a.market_type,
                    status: status_tag.to_string(),
                    status_reason,
                    balance,
                    available,
                    reserved: if unfunded_live {
                        Decimal::ZERO
                    } else {
                        ledger.reserved()
                    },
                    credentials_loaded: a.credential_keys.loaded,
                    venue: a.venue.clone(),
                    trading_enabled: a.trading_enabled,
                    strategies: a.strategies.clone(),
                    rounds: a.rounds.clone(),
                    open_positions,
                    day_realized_usd,
                    updated_at_ms: a.updated_at_ms,
                }
            })
            .collect()
    }

    /// E28 `account.status` (§12.1): a runtime call may only TIGHTEN — a
    /// loosening (`frozen -> active`, `read_only -> frozen`) is refused with
    /// INVALID_PARAMS, because an unwind a single IPC call could undo is not
    /// an unwind. Unfreezing is config + restart.
    pub fn tighten_account_status(
        &mut self,
        id: &crate::account::AccountId,
        status: crate::account::AccountStatus,
        now_ms: i64,
    ) -> CoreResult<()> {
        self.accounts.require(id)?;
        let current = self
            .accounts
            .account(id)
            .expect("require checked existence")
            .status
            .clone();
        if !current.can_tighten_to(&status) {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account `{id}` is {current} — changing it to {status} would LOOSEN it; \
                     tighten-only at runtime (unfreeze = config + restart)"
                ),
            ));
        }
        self.accounts.set_status(id, status, now_ms)
    }

    /// The account↔venue binding write (`account.venue.set`): the in-memory
    /// write through [`crate::account::AccountLedgers::set_venue`] and the
    /// durable write into the account book's own file are ONE transaction —
    /// the persist happens FIRST (plan-and-persist before apply: a persist
    /// failure leaves both the file and the memory exactly as they were, so
    /// the two stores can never disagree), the in-memory record moves only
    /// after the file accepted. A core without an
    /// [`CoreConfig::accounts_config_path`] (backtester, most in-process
    /// tests) refuses instead of writing into whatever checkout it happens
    /// to run from — the `runtime_config_path` posture, kept.
    ///
    /// Validation of the venue NAME (a registered plugin, or empty = unbind)
    /// is the caller's job — it needs the compiled-in registry, which the
    /// market-agnostic core deliberately cannot see.
    pub fn set_account_venue_persisting(
        &mut self,
        id: &crate::account::AccountId,
        venue: &str,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> CoreResult<()> {
        use crate::ipc::schema::ACCOUNT_VENUE_SET_NOTE;
        // Existence first, with the standard unknown-account refusal, before
        // anything touches disk.
        self.accounts.require(id)?;
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.venue.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_account_venue(&legacy, &env_path, id.as_str(), venue)
            .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.venue.set: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts.set_venue(id, venue, now_ms)?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            venue = %venue,
            reason = %reason,
            "account venue binding written — orders.place is gated by it \
             immediately{}; the binding re-reads at next boot \
             ({ACCOUNT_VENUE_SET_NOTE})",
            if venue.is_empty() { " (empty venue = unbound)" } else { "" },
        );
        Ok(())
    }

    /// The account round-FAMILY write (`account.rounds.set`, 轮次多族 B2):
    /// validate → persist FIRST (a failure leaves both stores untouched) →
    /// the in-memory book. The venue.set transaction posture, kept: the
    /// persist happens FIRST (plan-and-persist before apply — the two stores
    /// can never disagree), and a core without an
    /// [`CoreConfig::accounts_config_path`] refuses instead of writing into
    /// whatever checkout it happens to run from.
    ///
    /// Validation reuses [`crate::account::config::validate_rounds`] (the
    /// SAME whitelist + dedup the parser runs — one spelling of the rule,
    /// never two) over `Some(rounds)`; an empty list passes through as the
    /// legal unrestricted set and REMOVES the file key (missing == `[]` at
    /// the next boot; the deleted key is the cleaner spelling). The
    /// open-exposure refusal for a shrinking set is the IPC arm's job — it
    /// needs the position book AND the removed-family arithmetic, and its
    /// refusal must move NOTHING (memory and disk exactly as they were).
    pub fn set_account_rounds_persisting(
        &mut self,
        id: &crate::account::AccountId,
        rounds: Vec<i64>,
        actor: &str,
        reason: &str,
        _now_ms: i64,
    ) -> CoreResult<()> {
        use crate::ipc::schema::ACCOUNT_ROUNDS_SET_NOTE;
        // Existence first, with the standard unknown-account refusal, before
        // anything touches disk.
        self.accounts.require(id)?;
        // One validation, the parser's own: the plugins' declared union +
        // dedup, first-seen order kept. `Some(..)` (not `None`) — the wire
        // asked for a set.
        let rounds =
            crate::account::config::validate_rounds(Some(rounds), &self.config.allowed_round_secs)?;
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.rounds.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_account_rounds(
            &legacy,
            &env_path,
            id.as_str(),
            &rounds,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.rounds.set: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        // The write-side re-check inside `set_rounds` runs again (same
        // fail-closed posture at every door) — it cannot refuse what
        // `validate_rounds` passed, but the door stays.
        self.accounts
            .set_rounds(id, rounds.clone(), &self.config.allowed_round_secs)?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            rounds = ?rounds,
            reason = %reason,
            "account round families written — the (venue, duration) admission \
             gate reads the new set immediately{}; the declaration re-reads at \
             next boot ({ACCOUNT_ROUNDS_SET_NOTE})",
            if rounds.is_empty() {
                " (empty set = unrestricted, key removed from accounts.toml)"
            } else {
                ""
            },
        );
        Ok(())
    }

    /// The arb leg configuration in force (`arb.config.get`'s data source).
    /// `None` = unconfigured.
    pub fn arb_leg_config(&self) -> Option<&crate::account::CrossVenueArbConfig> {
        self.arb_leg_config.as_ref()
    }

    /// The arb leg configuration write (`arb.config.set`): validate → persist
    /// FIRST (a failure leaves both stores untouched) → memory → hot-push the
    /// engine. The hot-push is the whole runtime effect: the adapter stamps
    /// entry/exit accounts per token venue, and the strategy reads its
    /// legs/pairs from the `bk.params()` bag merged on the next evaluate.
    ///
    /// Validations (fail-closed, every refusal names its field):
    ///   * both venues non-empty AND distinct;
    ///   * both accounts exist in the book (accounts.toml's `[[account]]`s);
    ///   * when an account carries a venue binding, that binding must admit
    ///     its own leg's venue (an account pinned to venue V cannot trade
    ///     leg venue W — the scope gate would refuse every leg);
    ///   * pairs non-empty.
    pub fn arb_config_set_persisting(
        &mut self,
        cfg: &crate::account::CrossVenueArbConfig,
        actor: &str,
        reason: &str,
        _now_ms: i64,
    ) -> CoreResult<()> {
        let err = |msg: String| CoreError::new(CoreErrorCode::InvalidParams, msg);
        self.arb_config_validate(cfg)?;
        // Persist first: the file's parse layer re-validates coherence (the
        // same rules the boot runs), so a write that would produce a
        // next-boot-refusing file is refused HERE, before memory moved.
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(err(
                "arb.config.set: no accounts config path configured — persistence is \
                 disabled for this core (backtests and in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_cross_venue_arb(&legacy, &env_path, cfg).map_err(
            |e| {
                err(format!(
                    "arb.config.set: persist failed, nothing changed: {}",
                    e.message
                ))
            },
        )?;
        // Memory half, then the hot-push: the engine's adapters read the
        // stamp source, and the strategy reads the params bag — both on the
        // next evaluate, no restart.
        self.arb_config_apply(cfg);
        let (leg_a_venue, leg_a_account, leg_b_venue, leg_b_account) = (
            cfg.leg_a_venue.trim(),
            cfg.leg_a_account.trim(),
            cfg.leg_b_venue.trim(),
            cfg.leg_b_account.trim(),
        );
        tracing::info!(
            target: "risk",
            actor = %actor,
            leg_a = %format!("{leg_a_venue}/{leg_a_account}"),
            leg_b = %format!("{leg_b_venue}/{leg_b_account}"),
            pairs = ?self.arb_leg_config.as_ref().expect("just set").pairs,
            reason = %reason,
            "cross-venue arb legs written — the account stamp and the pairing params are \
             in force on the next evaluation; the section re-reads at next boot"
        );
        Ok(())
    }

    /// The fail-closed validations `arb.config.set` runs BEFORE anything
    /// moves (shared by the persisting write and the memory-only write —
    /// every refusal names its field).
    pub(super) fn arb_config_validate(
        &self,
        cfg: &crate::account::CrossVenueArbConfig,
    ) -> CoreResult<()> {
        let err = |msg: String| CoreError::new(CoreErrorCode::InvalidParams, msg);
        let leg_a_venue = cfg.leg_a_venue.trim();
        let leg_b_venue = cfg.leg_b_venue.trim();
        if leg_a_venue.is_empty() {
            return Err(err(
                "arb.config.set: legAVenue must be a non-empty venue name".into(),
            ));
        }
        if leg_b_venue.is_empty() {
            return Err(err(
                "arb.config.set: legBVenue must be a non-empty venue name".into(),
            ));
        }
        if leg_a_venue == leg_b_venue {
            return Err(err(format!(
                "arb.config.set: legBVenue `{leg_b_venue}` is the same venue as legAVenue — \
                 the two legs must sit on DIFFERENT venues"
            )));
        }
        let leg_a_account = cfg.leg_a_account.trim();
        let leg_b_account = cfg.leg_b_account.trim();
        for (field, id) in [
            ("legAAccount", leg_a_account),
            ("legBAccount", leg_b_account),
        ] {
            let account = self
                .accounts
                .account(&crate::account::AccountId::from(id))
                .ok_or_else(|| {
                    err(format!(
                        "arb.config.set: {field} `{id}` names no configured account — \
                         configured accounts are: {}",
                        self.accounts
                            .account_list()
                            .iter()
                            .map(|a| a.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
            let bound = account.venue.trim();
            let leg_venue = if field == "legAAccount" {
                leg_a_venue
            } else {
                leg_b_venue
            };
            if !bound.is_empty() && bound != leg_venue {
                return Err(err(format!(
                    "arb.config.set: {field} `{id}` is bound to venue `{bound}` — it cannot \
                     trade leg venue `{leg_venue}` (unbind it in accounts.toml or pick another \
                     account)"
                )));
            }
        }
        if cfg.pairs.is_empty() {
            return Err(err(
                "arb.config.set: pairs must name at least one asset — an empty list pairs \
                 nothing"
                    .into(),
            ));
        }
        // The pair list is a SET: a duplicate entry would double-push the
        // same asset's pairing (and a next boot would refuse the same file —
        // keep the set semantics identical to `validate_cross_venue_arb_section`).
        let mut seen_pairs = std::collections::HashSet::new();
        for p in &cfg.pairs {
            let up = p.trim().to_uppercase();
            if up.is_empty() {
                return Err(err(
                    "arb.config.set: pairs must not contain a blank asset name".into(),
                ));
            }
            if !seen_pairs.insert(up) {
                return Err(err(format!(
                    "arb.config.set: pairs `{}` appears twice — the list is a set",
                    p.trim()
                )));
            }
        }
        Ok(())
    }

    /// The memory half of a validated set: the normalized config into the
    /// Core cell, hot-pushed to the engine's adapters (the stamp source and
    /// the `bk.params()` entries both read it on the next evaluate).
    pub(super) fn arb_config_apply(&mut self, cfg: &crate::account::CrossVenueArbConfig) {
        self.arb_leg_config = Some(crate::account::CrossVenueArbConfig {
            leg_a_venue: cfg.leg_a_venue.trim().to_string(),
            leg_a_account: cfg.leg_a_account.trim().to_string(),
            leg_b_venue: cfg.leg_b_venue.trim().to_string(),
            leg_b_account: cfg.leg_b_account.trim().to_string(),
            pairs: cfg
                .pairs
                .iter()
                .map(|p| p.trim().to_uppercase())
                .collect::<Vec<_>>(),
        });
        if let Some(e) = self.engine.as_mut() {
            e.set_arb_leg_config(self.arb_leg_config.clone());
        }
    }

    /// The MEMORY-ONLY leg-configuration write: the same validations and the
    /// same hot-push as [`Self::arb_config_set_persisting`], without the file.
    /// The shape a core without an `accounts.toml` path takes (backtests and
    /// in-process tests) — a restart forgets it, exactly like every other
    /// in-memory knob those cores run.
    pub fn arb_config_set(&mut self, cfg: &crate::account::CrossVenueArbConfig) -> CoreResult<()> {
        self.arb_config_validate(cfg)?;
        self.arb_config_apply(cfg);
        Ok(())
    }

    /// 配对声明受理（engine 提交管线在 arb_precheck 过闸后逐腿调用）。
    /// 经济身份 (asset, slot) 从腿 token 经 engine scanner 的 token→market
    /// 表派生（不是策略自报的）；token 不在本轮任何市场 ⇒ slot 派生不出
    /// ⇒ 拒收（fail-closed）。owner_account 是实例主账户（实例隔离维度），
    /// 策略自报 key 只作可读标签。声明载荷整体从 engine 的 [`crate::engine::PairDeclaration`]
    /// 传入（owner/腿账户/token/shares 一个结构 —— 受理面不再逐参重抄）。
    pub(super) fn pair_declare_leg(&mut self, d: &crate::engine::PairDeclaration, now_ms: i64) {
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        // token→market 表：slot/asset 的派生权威（engine scanner 本轮市场）。
        let (asset, slot) = match engine.round_slot_of(&d.token_id) {
            Some(slot) if slot > 0 => (engine.asset_of(&d.token_id).unwrap_or_default(), slot),
            _ => return, // 不在本轮任何市场：无轮次身份，不配进簿
        };
        let facts = crate::pair::TokenMarketFacts { asset, slot };
        let decl = crate::pair::PairLegDecl {
            token_id: d.token_id.clone(),
            shares: d.shares.unwrap_or(Decimal::ZERO),
            leg: d.decl.leg.clone(),
            // 盖章结果（v2 可 A≠B）——PairBook 的腿账户与投影都以它为准。
            leg_account: d.leg_account.clone(),
        };
        let _ = self.pair_book.declare(
            &d.owner_strategy,
            &d.owner_account,
            &d.decl.key,
            &decl,
            &facts,
            now_ms,
        );
    }

    /// place 成功后的 internal_key 回填（OME 事实事件经它寻址回声明腿）。
    pub(super) fn pair_bind_key(
        &mut self,
        strategy: &str,
        owner_account: &str,
        token_id: &str,
        internal_key: &str,
    ) {
        self.pair_book
            .bind_by_leg(strategy, owner_account, token_id, internal_key);
    }

    /// OME 事实事件喂入（成交/死亡）——幂等，未命中声明腿的事件是空操作。
    pub(super) fn pair_on_order_event(
        &mut self,
        internal_key: &str,
        filled_delta: Decimal,
        dead: bool,
    ) {
        self.pair_book
            .on_order_event(internal_key, filled_delta, dead);
    }

    /// 轮次推进清退：换轮臂（engine_on_data 的 RoundMarkets 非 top-up）调用。
    pub(super) fn pair_retire_before(&mut self, before: i64) {
        self.pair_book.retire_before(before);
    }

    /// `bk.arb.pairs()` 投影的数据源：owner 过滤的配对视图（跨策略/跨账户
    /// 不可见）。`accounts` 是该实例可见的账户白名单（实例主账户 + arb 腿
    /// 账户，与 holdings 视图的拓宽同形）。
    pub(crate) fn pair_views_for(
        &self,
        strategy: &str,
        accounts: &HashSet<String>,
    ) -> Vec<crate::pair::PairView> {
        self.pair_book.views_for(strategy, accounts)
    }

    /// 配对簿的测试/审计只读快照（无过滤）——`pair_book_check.rs` 经它断言
    /// 受理链的落账结果；生产路径一律走 `pair_views_for`（owner 过滤投影）。
    pub fn pair_book_all(&self) -> Vec<crate::pair::PairState> {
        self.pair_book.all().into_iter().cloned().collect()
    }

    /// Unset the arb leg configuration (the panel's 「解除配置」): remove the
    /// file section and the memory mirror, engine hot-pushed. `arb.config.set`
    /// is the only writer in the v2 flow; this is the delete arm the wire
    /// spells with `configured: false` after a successful call.
    pub fn arb_config_clear_persisting(&mut self, actor: &str) -> CoreResult<()> {
        let err = |msg: String| CoreError::new(CoreErrorCode::InvalidParams, msg);
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(err(
                "arb.config.clear: no accounts config path configured — persistence is \
                 disabled for this core (backtests and in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_clear_cross_venue_arb(&legacy, &env_path).map_err(|e| {
            err(format!(
                "arb.config.clear: persist failed, nothing changed: {}",
                e.message
            ))
        })?;
        self.arb_leg_config = None;
        if let Some(e) = self.engine.as_mut() {
            e.set_arb_leg_config(None);
        }
        tracing::info!(
            target: "risk",
            actor = %actor,
            "cross-venue arb legs cleared — pairing falls back to the unified-confirm chain"
        );
        Ok(())
    }

    /// The trading-switch write (`account.trading.set`): memory AND
    /// accounts.toml in one transaction — persist FIRST (a failure leaves
    /// both stores untouched), memory after. The engine mirror needs no
    /// separate push here: the next evaluation cycle re-pushes every
    /// account's trading fact, so the gate is in force within one cycle.
    /// `default` is settable like any account (a single-book deployment may
    /// want its one account's entries off too).
    pub fn set_account_trading_persisting(
        &mut self,
        id: &crate::account::AccountId,
        enabled: bool,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> CoreResult<()> {
        use crate::ipc::schema::ACCOUNT_TRADING_SET_NOTE;
        self.accounts.require(id)?;
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.trading.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_account_trading(
            &legacy,
            &env_path,
            id.as_str(),
            enabled,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.trading.set: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts.set_trading(id, enabled, now_ms)?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            enabled = %enabled,
            reason = %reason,
            "account trading switch written — {} re-reads at next boot \
             ({ACCOUNT_TRADING_SET_NOTE})",
            if enabled { "entries re-open; " } else { "new entries refused (exits flow); " },
        );
        Ok(())
    }

    /// The display-pointer write (显示账户配置化 / `display.account.set`):
    /// memory AND accounts.toml in one transaction — persist FIRST (a failure
    /// leaves both stores untouched), memory after. The refusals are ALL
    /// plan-first: the id must name a CONFIGURED account.
    ///
    /// 2026-10-20 操作者裁决（issue 561）：display 是面板的唯一可信账户
    /// 事实源，切户动作 = 写 display。校验据此放宽为「账户存在即可」，
    /// 不再要求 `status = Active`。display 是显示焦点而非交易授权面：
    /// 把指针指向 frozen / read_only 账户去看它的账本与流水是合法操作
    /// （与 `account.switch` 的 existence-only 姿态同一逻辑 —— posture
    /// 管下单，不管看）。消费侧（行情过滤、策略归属、风控、下单）各自
    /// 按账户自身 posture 做门禁，display 指针不放行任何交易语义。
    ///
    /// 语义边界：this pointer never touches the process-level active
    /// account, no ledger, and no strategy enablement — the snapshot's
    /// `displayAccount`/`displayVenue` (and the issue-561 default chain)
    /// are the only readers.
    pub fn set_display_account_persisting(
        &mut self,
        id: &crate::account::AccountId,
        actor: &str,
        reason: &str,
    ) -> CoreResult<()> {
        use crate::ipc::schema::DISPLAY_ACCOUNT_SET_NOTE;
        self.accounts.require(id)?;
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "display.account.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_display_account(&legacy, &env_path, id.as_str())
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "display.account.set: persist failed, nothing changed: {}",
                        e.message
                    ),
                )
            })?;
        self.accounts.set_display(id)?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            reason = %reason,
            "display pointer written — the snapshot's displayAccount/ \
             displayVenue follow this account; re-reads at next boot \
             ({DISPLAY_ACCOUNT_SET_NOTE})",
        );
        Ok(())
    }

    /// The strategy-assignment write (`account.strategies.set`): memory AND
    /// accounts.toml in one transaction, plus the engine scope rebuild.
    ///
    /// Names are validated against the engine's REGISTERED set (plan-first —
    /// a typo fails here, never as a silently inert assignment). The rebuild:
    /// every (account, strategy) scope NOT in the new assignment and holding
    /// NO open position for this account is torn down (Drop); one with open
    /// positions is kept as an EXITS-ONLY instance (enabled stays whatever it
    /// was, but the engine drops its entry candidates for this account from
    /// the next cycle on — see `Engine::scope_rebuild`) until the account is
    /// flat in it, then a later call (or boot) drops it. New names in the
    /// assignment are instantiated from the SAME lua package the default
    /// scope loaded (own Lua state, own book).
    ///
    /// 账户↔venue 解耦语义: `requested` is the account's strategy
    /// ENABLEMENT set (the 启用按钮's persistence), not an assignment or a
    /// visibility filter — strategies are global. The write is a whole-set
    /// decision: memory AND accounts.toml in one transaction (persist
    /// first), then the engine scopes rebuilt from it (removed-and-flat
    /// drop; removed-and-held demote exits-only; added instantiate enabled).
    ///
    /// Returns the exits-only keep-list for the reply.
    pub fn set_account_strategies_persisting(
        &mut self,
        id: &crate::account::AccountId,
        strategies: Vec<String>,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> CoreResult<Vec<String>> {
        use crate::ipc::schema::ACCOUNT_STRATEGIES_SET_NOTE;
        self.accounts.require(id)?;
        // Dedupe + order-stable: an assignment that names a strategy twice
        // is a caller bug, but the honest handling is collapse, not refusal.
        let mut requested: Vec<String> = Vec::new();
        for s in &strategies {
            let name = s.trim();
            if name.is_empty() {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    "account.strategies.set: an empty strategy name in the \
                     list cannot be addressed"
                        .to_string(),
                ));
            }
            if !requested.iter().any(|r| r == name) {
                requested.push(name.to_string());
            }
        }
        // Plan-first validation: every requested name must be REGISTERED in
        // the engine (any scope). The registered set is the loadable set —
        // the default scope hosts every loaded package.
        if self.engine.is_none() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.strategies.set: no strategy engine attached".to_string(),
            ));
        }
        for name in &requested {
            if !self
                .engine
                .as_ref()
                .expect("engine presence checked above")
                .strategy_registered(name)
            {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "account.strategies.set: strategy `{name}` is not \
                         registered — the assignable set is what \
                         `strategy.list` reports"
                    ),
                ));
            }
        }
        // Positions THIS account holds, by strategy — the exits-only safety
        // net's input.
        let held: Vec<String> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == *id)
            .map(|p| p.strategy.clone())
            .collect();
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.strategies.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_account_strategies(
            &legacy,
            &env_path,
            id.as_str(),
            &requested,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.strategies.set: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts
            .set_strategies(id, requested.clone(), now_ms)?;
        // #537: the default account's writes keep the persisted enabled-set
        // in lockstep. The boot migrates the state file's set into the file
        // ONLY while the default account has no `strategies` key; without
        // this mirror, a set the operator emptied (key removed from
        // accounts.toml) would be re-imported from a stale state file at the
        // next boot — the exact resurrection the default-toggle mirror above
        // closes. Non-default accounts never migrate, so their state file is
        // untouched (the global toggle's file stays theirs alone).
        if *id == crate::account::default_account_id()
            && let Some(state_path) = self.config.strategy_state_path.as_deref()
        {
            crate::strategy_state::save(std::path::Path::new(state_path), &requested);
        }
        // Engine rebuild. Removed-and-flat scopes drop; removed-and-held
        // scopes are marked exits-only; added scopes instantiate. An EMPTY
        // assignment is explicit "this account runs no strategies" — the
        // rebuild then drops every flat scope of the account (the INHERIT
        // reading this used to carry was retired: per-account assignments
        // are the only truth, the default account included).
        let kept_exits_only = self.rebuild_account_scopes(id, &requested, &held);
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            strategies = ?requested,
            exits_only = ?kept_exits_only,
            reason = %reason,
            "account strategy assignment written — engine scopes rebuilt, \
             re-read at next boot ({ACCOUNT_STRATEGIES_SET_NOTE})",
        );
        Ok(kept_exits_only)
    }

    /// The engine half of an enablement-set write: delegate to the engine's
    /// own scope rebuild, handing it a closure that re-instantiates Lua
    /// packages (the only rebuildable source) from their dir. Returns the
    /// names kept EXITS-ONLY for this account.
    pub(super) fn rebuild_account_scopes(
        &mut self,
        id: &crate::account::AccountId,
        requested: &[String],
        held: &[String],
    ) -> Vec<String> {
        let account = id.as_str().to_string();
        let Some(engine) = self.engine.as_mut() else {
            return Vec::new();
        };
        // NOTE: `build` borrows `self` mutably while `engine` is borrowed —
        // impossible directly, so the lua re-loader is a FREE function over
        // captured state (name + source dir come from the DEFAULT scope's
        // registration, read before the engine borrow opens).
        let sources: std::collections::HashMap<String, String> =
            engine.scope_sources().into_iter().collect();
        let mut build = |name: &str, account: &str| build_lua_scope(&sources, name, account);
        engine.rebuild_account_scopes(&account, requested, held, &mut build)
    }

    /// Boot application of the accounts file's strategy ENABLEMENT sets
    /// (账户↔venue 解耦语义) — the production `run()` calls this ONCE after
    /// `install_engine`. For every account with a NON-EMPTY `strategies` key
    /// the named scopes are instantiated and ENABLED; the default account
    /// with NO key and a non-empty persisted enabled-set (strategy-state
    /// .json — the old global toggle's durable form) is MIGRATED once: that
    /// set is materialized as the default account's enablement set in
    /// accounts.toml, so the upgrade keeps the production behaviour exactly
    /// (default 账户 strategies=["pair_discount_arb","spread_arb"] 升级后
    /// 行为不变) and the file is the only truth from that boot on.
    ///
    /// NOT part of `install_engine`: the replay/backtest boot goes through
    /// that path too and its single-default book must keep byte-identical
    /// outcomes (the exit-economics baseline). Production assembly is the
    /// only caller.
    ///
    /// Returns the migrated default enablement set (empty when nothing
    /// migrated) so the caller can report it on stderr.
    pub fn apply_account_strategy_assignments_at_boot(&mut self) -> Vec<String> {
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Vec::new();
        };
        // The persisted enabled-set (strategy-state.json) drives the one-time
        // migration of the default account. Read BEFORE any write.
        let persisted_enabled: Vec<String> = match self.config.strategy_state_path.as_deref() {
            Some(p) => crate::strategy_state::load(std::path::Path::new(p)),
            None => Vec::new(),
        };
        let default_id = crate::account::default_account_id();
        // The default account's assignment key decides whether the boot runs
        // on the file's spelling (migration already happened) or migrates.
        let default_has_key = self
            .accounts
            .account(&default_id)
            .is_some_and(|a| !a.strategies.is_empty());
        let mut migrated: Vec<String> = Vec::new();
        if !default_has_key && !persisted_enabled.is_empty() {
            // One-time migration: the file gains the explicit default
            // assignment, spelling exactly what the old implicit behaviour
            // (inherit the persisted enabled set) produced. The write is
            // best-effort — a read-only tree keeps the implicit shape and
            // the boot proceeds (the persisted set is re-read next boot).
            //
            // #537: the persisted set is also kept in lockstep from here on —
            // the default toggle now rewrites BOTH stores, and a set the
            // operator emptied (key removed from the file, empty state file)
            // must not be resurrected by a stale state file left over from
            // the pre-toggle era. This one-time gate fires only when the file
            // never spelled an assignment; afterwards the accounts file is
            // the sole truth, exactly as the contract above states.
            if crate::account::config::env_persist_account_strategies(
                &legacy,
                &env_path,
                default_id.as_str(),
                &persisted_enabled,
            )
            .is_ok()
            {
                let _ =
                    self.accounts
                        .set_strategies(&default_id, persisted_enabled.clone(), now_ms());
                migrated = persisted_enabled;
                tracing::info!(
                    strategies = ?migrated,
                    "strategy enablement migration: the default account's persisted \
                     enabled set is materialized as its accounts.toml enablement set — \
                     the file is the only truth from here on"
                );
            }
        }
        // Apply every account's enablement set (the default's included —
        // after migration it spells the old behaviour). An empty key is the
        // EXPLICIT "run nothing on this account" set — it never follows the
        // global toggle (INHERIT is retired), so nothing is fanned out here
        // and the account's scopes stay silent until the operator enables
        // strategies for it on the strategies page.
        let assignments: Vec<(String, Vec<String>)> = self
            .accounts
            .account_list()
            .iter()
            .filter(|a| !a.strategies.is_empty())
            .map(|a| (a.id.as_str().to_string(), a.strategies.clone()))
            .collect();
        for (account, strategies) in assignments {
            // A set member's scope is built ENABLED — being in the account's
            // enablement set IS the operator's intent to run it there.
            // Unknown names are skipped with a warning by the rebuild (the
            // registered set is what the strategy dir loaded this boot).
            self.set_account_scopes_at_boot(&account, &strategies);
        }
        migrated
    }

    /// The boot-flavoured scope application: instantiate `strategies` for
    /// `account` and enable every one that landed. No exits-only handling —
    /// at boot the position book is restored AFTER this runs, so nothing is
    /// held yet (the restored positions reattach to the rebuilt scopes by
    /// name; a position whose strategy is not in the account's assignment
    /// keeps getting exit-managed because the position book drives exits, not
    /// the scope set).
    pub(super) fn set_account_scopes_at_boot(&mut self, account: &str, strategies: &[String]) {
        // The engine borrow and the Lua re-loader cannot overlap: read the
        // source map first (same shape as `rebuild_account_scopes`).
        let sources: std::collections::HashMap<String, String> = self
            .engine
            .as_ref()
            .map(|e| e.scope_sources().into_iter().collect())
            .unwrap_or_default();
        let mut build = |name: &str, acct: &str| build_lua_scope(&sources, name, acct);
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        // DEMOTE/DROP phases are irrelevant at boot (no held positions yet);
        // the rebuild's ADD phase instantiates + enables.
        engine.rebuild_account_scopes(account, strategies, &[], &mut build);
    }

    /// `account.create`（账户簿运行时增删的建半）：新账户落内存 AND
    /// accounts.toml，一个事务 —— persist 先行（失败则两个存储都不动），文件
    /// 收下后才动内存，与 [`Self::set_account_venue_persisting`] 同一姿态。
    /// 身份合同在这里钉死：id 非空 / trim 后非空 / 不允许保留名 `default` /
    /// 不允许与簿内现有 id 重复；status 恒 `Active`（运行时铸造一个受限账户
    /// 等于变相放宽，收紧是 `account.status` 的事）；market_type 恒
    /// `Prediction`（0.3 唯一有插件的市场类型，与文件缺省一致）。调用方从
    /// 插件声明传入凭据变量名并与身份一起落盘；值仍由
    /// `market.credentials.set` 写入，永不进入账户请求或响应。
    /// 无 [`CoreConfig::accounts_config_path`] 的内核（回测、多数进程内测试）
    /// 拒绝 —— 同 venue.set 的 "persistence is disabled" 文案形状。
    ///
    /// venue 名的注册校验（plan-first）是调用方的职责 —— 编译进来的插件注册
    /// 表在 IPC 层，市场无关的 core 看不见（同 [`Self::set_account_venue_persisting`]）。
    pub fn create_account_persisting(
        &mut self,
        id: &crate::account::AccountId,
        name: Option<&str>,
        venue: Option<&str>,
        credential_keys: crate::account::CredentialKeys,
        audit: (&str, &str),
        now_ms: i64,
    ) -> CoreResult<()> {
        use crate::ipc::schema::ACCOUNT_CREATE_NOTE;
        let (actor, reason) = audit;
        // Reserved name first — BEFORE the empty-id check, so the refusal
        // names the contract, not the spelling (`"default"` trimmed and
        // whitespace-padded variants land on the same reserved refusal).
        let trimmed = id.as_str().trim();
        if trimmed == crate::account::DEFAULT_ACCOUNT_ID {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.create: id `default` is reserved — the zero-config \
                 deployment and the active-cell fallback both own it; add your \
                 accounts under their own ids"
                    .to_string(),
            ));
        }
        if trimmed.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.create: id is empty — an account without an identity \
                 cannot be addressed or audited"
                    .to_string(),
            ));
        }
        let id = crate::account::AccountId::from(trimmed);
        // Existence (duplicate) with an honest refusal, before disk.
        if self.accounts.account(&id).is_some() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("account.create: account `{id}` already exists"),
            ));
        }
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.create: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_account_create(
            &legacy,
            &env_path,
            id.as_str(),
            name,
            venue,
            &credential_keys,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.create: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts.insert(crate::account::Account {
            id: id.clone(),
            name: name.unwrap_or(id.as_str()).to_string(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys,
            venue: venue.filter(|v| !v.is_empty()).unwrap_or("").to_string(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            updated_at_ms: now_ms,
        })?;
        // A runtime mint on a locally-settling core gets the same seed a boot
        // account would have woken up with. Without this the strategy sizing
        // reads equity 0 and the scope never trades (`policy.skip: equity 0
        // below min_equity_usd`) — and no boot will ever repair it: the boot
        // seed (`seed_all`) is skipped wholesale once ANY account has a
        // journal, which an established deployment always does. LIVE is not
        // seeded: the venue's real cash is the balance, realigned from the
        // venue report. The journal row (kind Seed, ref `account.create`)
        // makes the funding a replayable boot fact for the NEXT boot.
        //
        // The journal only lands if the minted ledger has a journal PATH —
        // boot's `attach_journals` never sees accounts that did not exist at
        // boot, so the path is attached HERE. Without it the funding row stays
        // in the in-memory trail only and a restart wakes the account at zero
        // (生产 probe2 实锤: balance 1000 → restart → 0).
        let ledger = self.accounts.get_mut(&id)?;
        if let Some(dir) = self.config.ledger_journal_dir.as_deref() {
            ledger.set_journal_path(
                std::path::Path::new(dir).join(format!("{}.journal.jsonl", id.as_str())),
            );
        }
        if self.config.mode.settles_locally()
            && self.config.dry_seed_balance > rust_decimal::Decimal::ZERO
        {
            ledger.fund(self.config.dry_seed_balance, "account.create");
        }
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            name = %name.unwrap_or(id.as_str()),
            venue = %venue.unwrap_or(""),
            reason = %reason,
            "account created — ACTIVE prediction account minted and persisted; \
             plugin credential names attached (values ride market.credentials.set) \
             ({ACCOUNT_CREATE_NOTE})",
        );
        Ok(())
    }

    /// `account.remove`（账户簿运行时增删的删半）：账户离开内存 AND
    /// accounts.toml，同一事务姿态。拒绝面：账簿的最后一个账户（空簿会让
    /// 下一次 boot 拒载，删掉它是删库式脚枪）；仍有 open position 的账户
    /// （钱不能变成不可寻址的）；未知 id 标准拒绝。`default` 只是普通账户
    /// —— 所有账户都可删（用户裁决：绑定只在建户时绑定、编辑面已收，
    /// 删户重建是绑定变更的唯一出口；零配置的 default 由下一次 boot 的
    /// 缺文件/缺行路径重新派生）。被删的是进程级 active 账户时自动回切
    /// 剩余账户的第一个（会话级切换器在下次读时照簿解析）。
    pub fn remove_account_persisting(
        &mut self,
        id: &crate::account::AccountId,
        actor: &str,
        reason: &str,
    ) -> CoreResult<()> {
        use crate::ipc::schema::ACCOUNT_REMOVE_NOTE;
        // The LAST account stays: an empty book fails the next boot's parse,
        // so retiring it would be a delete-the-deployment footgun. Every id
        // is otherwise fair game — `default` included (the zero-config
        // fallback re-derives on the next boot when the file lacks a
        // `default` row). Existence refuses first, with the standard
        // unknown-account error.
        self.accounts.require(id)?;
        if self.accounts.account_list().len() <= 1 {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.remove：`{id}` 是账户簿里最后一个账户——至少要保留一个账户 \
                     （下一次启动会拒绝空账户簿；确要清空部署请直接删除 accounts.toml）"
                ),
            ));
        }
        // Open positions pin the account: retiring it would leave inventory
        // with no book, no ledger and no close path — money must not become
        // unaddressable.
        let open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| &p.account_id == id)
            .count();
        if open > 0 {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.remove：`{id}` 还有 {open} 笔未平持仓——请先平仓 \
                     （删掉的账户不带走任何库存，钱必须始终有簿可归）"
                ),
            ));
        }
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.remove: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_account_remove(&legacy, &env_path, id.as_str())
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "account.remove：账户簿落盘失败，两个存储都没有改动：{}",
                        e.message
                    ),
                )
            })?;
        // Journal 收尾（现金流水卡与任何账户对不上的根因修复）：把该账户的
        // 磁盘流水文件删掉，内存 trail 同步清空。同名 id 的下一次
        // `account.create` 是新账本，不是老账户续弦 —— 留着旧文件，下一次
        // boot 的 attach_journals 会把上一世的成交流水整表回放进来（生产
        // kalshi 实锤：6 轮建/删叠出 6 笔幽灵 seed@account.create，Σamount
        // 比真实余额多 5000）。删文件失败则整体拒绝 —— 此时 accounts.toml
        // 已收下了删除（persist 先行的代价），但内存未动、旧 journal 还在，
        // 重试一次即可收敛；拒绝的消息如实告知。
        if !self.accounts.get_mut(id)?.retire_journal_file() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.remove：`{id}` 的流水文件删除失败 —— accounts.toml \
                     已落盘但内存未动，旧流水文件还在；请重试一次，问题持续 \
                     请检查 data/ledger/ 的写权限"
                ),
            ));
        }
        // issue 723: the bound venues' evidence cursors die with the ledger —
        // a same-name re-create is a NEW journal generation with its own seq
        // space, and the previous generation's row numbers must never judge
        // it (a cursor past every seq the new ledger will ever issue reads
        // booked as 0 forever). Collected BEFORE the book shrinks: the
        // venue→account resolution reads the very binding this remove ends.
        let orphaned_venues: Vec<String> = self
            .venue_fact_cursor
            .keys()
            .filter(|v| self.venue_cash_account(v.as_str()).as_ref() == Some(id))
            .cloned()
            .collect();
        self.accounts.remove(id)?;
        for v in orphaned_venues {
            self.venue_fact_cursor.remove(&v);
        }
        // 冻结簿随账户走：旧实现留着悬空冻结项（同名 id 重建即继承上一世
        // 的 kill switch），自愈时间戳同理一并摘除。
        self.account_freezes.remove(id.as_str());
        self.auto_frozen.remove(id.as_str());
        self.accounts.repair_active_after_removal(id);
        // 显示账户配置化: the removal must not orphan the display pointer —
        // the file was rewritten WITHOUT the row above, and the same render
        // re-derived its top-level `display_account` key when it pointed here
        // (persist_account_remove's own edit); the memory cell re-derives the
        // same way so the snapshot never names a removed account.
        self.accounts.repair_display_after_removal(id);
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            reason = %reason,
            "account removed — identity and ledger retired, the active cell \
             re-activates the first surviving account when it named the \
             removed account; the display pointer re-derives the same way \
             when the displayed account left ({ACCOUNT_REMOVE_NOTE})",
        );
        Ok(())
    }

    /// Assemble the audit's input from the core's own state. Cheap enough to run
    /// on the maintenance tick: a handful of sums over live orders, open
    /// positions and the reservation table.
    pub(super) fn build_audit_input(&self, now_ms: i64) -> AuditInput {
        // The audit's cash identity describes the ACTIVE account's book (§9.5,
        // E28): `reserved` reads the active ledger, so the live-BUY notional it
        // is compared against must count only the ACTIVE account's orders. A
        // sibling account's live BUY is spoken for in ITS book — counting it
        // here made `reservations_match_open_buys` drift by that notional
        // whenever any other account held an open order.
        let active_id = self.accounts.active_id().clone();
        let mut open_buy_notional = Decimal::ZERO;
        let mut open_buys = 0usize;
        for o in self.ome.live_orders() {
            if o.side != Side::Buy || o.account_id != active_id {
                continue;
            }
            open_buys += 1;
            open_buy_notional += o.price * (o.size - o.filled_size).max(Decimal::ZERO);
        }
        // A reservation is "stray" when the order behind it is not a tracked live
        // BUY: nobody can ever release it, so it is a permanent phantom hold.
        let stray_reservations: Vec<(OrderId, Decimal)> = self
            .accounts
            .active_ledger()
            .reservations_snapshot()
            .into_iter()
            .filter(|(id, _)| {
                !self
                    .ome
                    .get(id)
                    .map(|o| o.side == Side::Buy && o.status.is_live())
                    .unwrap_or(false)
            })
            .collect();
        // E28 (§9.2): every fold below scopes to the ACTIVE account — the
        // identity describes ONE book, so a sibling account's realized PnL,
        // pending redemption, or open position must not move it (they once
        // did: the global summary/global receivable/all-books positions fold
        // made `trade_ledger_identity` drift by exactly what the sibling
        // account traded).
        let (realized, seen_trades, has_trade_log) = match self.trade_db.as_ref() {
            Some(db) => {
                let s = db.summary_for(active_id.as_str());
                (f64_to_decimal(s.total_net_pnl), s.total_trades, true)
            }
            None => (Decimal::ZERO, 0, false),
        };
        let mut spent = Decimal::ZERO;
        let mut received = Decimal::ZERO;
        for p in self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == active_id)
        {
            spent += p.flows.entry_cost_usd + p.flows.entry_fee_usd;
            received += p.flows.proceeds_usd - p.flows.exit_fee_usd;
            // The modeled maker rebate the ledger already credited rides the
            // position's RECEIVED side until the close folds it into the trade
            // record's net — the identity must cover the cash at every
            // instant, not only after the position settles.
            received += p.flows.maker_rebate_usd;
        }
        let open_positions = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == active_id)
            .count();
        // E28: the audit's cash identity describes the ACTIVE account's book
        // (§9.5). Cross-account isolation is asserted by the dedicated gate
        // (account-cross-isolation-check.mjs), not by widening this identity.
        let active = self.accounts.active_ledger();
        // The venue report comparable to THIS book is the report of the venue
        // the active account is BOUND to (#505): a sibling venue's cash is a
        // different book's truth, and comparing across books once produced a
        // phantom `venue_free_cash` drift of exactly (other venue's cash −
        // this book's balance).
        let active_venue = self.accounts.active_venue().to_string();
        let venue_free = self
            .venue_free_by_venue
            .get(&active_venue)
            .map(|(t, b)| (*t, *b));
        let ledger_realigned = self.audit.last_realign.get(&active_venue).copied();
        // The venue leg compares the ledger against a PAPER site's own figure
        // when this venue declared itself SIMULATED (its plugin's declaration
        // snapshot — the same `set_plugin_modes` injection the strategy
        // handshake reads): the paper site freezes its reported free cash on a
        // resting BUY and never nets the reservation back out, so the leg's
        // `balance − reserved` expectation is fee-level cents off by
        // construction and two strikes halted new entries over arithmetic that
        // could never agree (external operator report, issue #669). The
        // declaration is the plugin's identity — it cannot change mid-session.
        let venue_simulated = self
            .plugin_modes
            .iter()
            .find(|p| p.name == active_venue)
            .map(|p| {
                p.modes.iter().any(|m| {
                    m.capabilities
                        .satisfies(blitzkrieg_market_api::MarketCapabilities::SIMULATED)
                })
            })
            .unwrap_or(false);
        AuditInput {
            now_ms,
            live: self.config.mode == Mode::Live,
            identity: CashIdentity {
                balance: active.balance(),
                // Settled but not yet redeemed: owed to US — this book's us,
                // not the process's (each claim carries the account it was
                // booked from, E28 §9.2).
                receivable: self.settlement.receivable_usd_for(active_id.as_str())
                    + self.settlement.merge_receivable_usd_for(active_id.as_str()),
                realized,
                spent,
                received,
            },
            reserved: active.reserved(),
            ledger_balanced: active.is_balanced(),
            unfunded_reserved: active.unfunded_reserved(),
            open_buy_notional,
            open_buys,
            stray_reservations,
            venue_free: venue_free.map(|(_, b)| b),
            venue_free_at_ms: venue_free.map(|(t, _)| t),
            ledger_realigned,
            open_positions,
            seen_trades,
            has_trade_log,
            venue_simulated,
        }
    }

    /// Run the accounting audit now and act on its verdict: alert, persist, and
    /// block/halt new entries. Returns the report (also kept for the panel).
    ///
    /// Called from `tick` on `audit_interval_sec`, and directly by tests.
    pub fn run_accounting_audit(&mut self, now_ms: i64) -> AuditReport {
        let input = self.build_audit_input(now_ms);
        let report = crate::reconcile::audit_accounting(&input, self.audit.anchor.as_ref());

        // Anchor: taken at the first audit the STRUCTURAL checks pass, then never
        // moved while anything is failing — re-anchoring on a failure would hide
        // exactly the movement the identity exists to expose.
        if self.audit.anchor.is_none() && !report.structural_failure {
            self.audit.anchor = Some(input.identity);
        }
        self.audit.runs += 1;
        if report.ok {
            self.audit.cross_source_failures = 0;
        } else {
            self.audit.failures += 1;
            if report.structural_failure {
                // Local inconsistency: no fetch race to forgive.
                self.audit.cross_source_failures = 0;
            } else {
                self.audit.cross_source_failures += 1;
            }
        }
        let cross_strike = self.audit.cross_source_failures >= 2;
        // A halt is warranted by a structural failure immediately, by a venue
        // mismatch only after it repeats (a single stale read must not stop
        // entries), and only when the operator left the brake enabled.
        let should_halt = !report.ok
            && (report.structural_failure || cross_strike)
            && self.config.audit_halt_entries;
        let was_halted = self.audit.halted;
        let mut resumed = false;

        if should_halt && !was_halted {
            self.audit.halted = true;
            self.audit.halt_reason = report.note.clone();
            let message = format!(
                "accounting audit FAILED — new entries blocked until it passes: {}",
                report.note
            );
            // The reason rides the structured channels (tracing / error slot /
            // RiskAlert); the stream write stays a static literal — kernel
            // text reaching the stream sink is the cleartext-logging shape
            // CodeQL flags (#693 family).
            tracing::error!(drift = %report.drift_usd, "{}", message);
            eprintln!("core: accounting audit FAILED — see the error log line above");
            // The halt is an error of the highest order (entries are blocked), so
            // it also lands in the one panel-visible slot (#180).
            self.note_error(
                &CoreError::new(CoreErrorCode::Internal, message.clone()),
                now_ms,
            );
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message,
            });
        } else if should_halt {
            self.audit.halt_reason = report.note.clone();
        } else if was_halted {
            self.audit.halted = false;
            self.audit.halt_reason.clear();
            resumed = true;
            let message = format!(
                "accounting audit recovered — new entries resumed ({})",
                report.note
            );
            tracing::info!("{message}");
            eprintln!("core: accounting audit recovered — see the info log line above");
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message,
            });
        } else if !report.ok {
            // Failing but not (yet) halting: still loud, still persisted.
            let message = format!("accounting audit drift: {}", report.note);
            tracing::warn!(drift = %report.drift_usd, "{}", message);
            eprintln!("core: accounting audit drift — see the warn log line above");
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message,
            });
        }

        // Persist on every failure, every halt/resume, and otherwise as a
        // heartbeat — a healthy audit is the evidence that the check RAN. The
        // very first audit is written too: a fresh process must be able to prove
        // it checked before anything went wrong.
        let heartbeat = self.audit.last_heartbeat_ms == 0
            || now_ms - self.audit.last_heartbeat_ms >= AUDIT_HEARTBEAT_MS;
        if !report.ok || should_halt || was_halted || resumed || heartbeat {
            self.audit.last_heartbeat_ms = now_ms;
            self.persist_audit_report(&report);
        }
        self.audit.report = Some(report.clone());
        self.audit.last_at_ms = now_ms;
        report
    }

    /// The latest audit verdict, for the panel (and for a caller that wants the
    /// report without running one).
    pub fn accounting_audit(&self) -> Option<&AuditReport> {
        self.audit.report.as_ref()
    }

    /// New entries are blocked by a failing accounting audit (issue #189).
    pub fn audit_blocks_entries(&self) -> bool {
        self.audit.halted
    }

    /// Why new entries are blocked (empty when they are not).
    pub fn audit_halt_reason(&self) -> &str {
        &self.audit.halt_reason
    }

    /// The gate itself: refuses a NEW entry while the audit is failing. Exits
    /// (SELL) never consult this — a drifted ledger must not be able to trap the
    /// bot in a position, which is what routing this through the kill switch did
    /// (#174).
    pub(super) fn audit_entry_gate(&self) -> CoreResult<()> {
        if self.audit.halted {
            return Err(CoreError::new(
                CoreErrorCode::RiskRejected,
                format!(
                    "accounting audit halted new entries: {}",
                    self.audit.halt_reason
                ),
            ));
        }
        Ok(())
    }

    /// Mutable ledger access, for tests that must put the books into the exact
    /// state a bug leaves behind (a stranded reservation, cash that moved with no
    /// trade) and assert the audit NOTICES it. Gated behind the `test-support`
    /// feature the crate enables for its own test targets, so no production build
    /// can reach it.
    #[cfg(feature = "test-support")]
    pub fn ledger_mut(&mut self) -> &mut Ledger {
        // E28: tests poking the books target the ACTIVE account's ledger —
        // the same default a 0.2 deployment has exactly one of.
        self.accounts.active_ledger_mut()
    }

    /// Append one audit verdict (or a late-fill record) to the audit log.
    pub(super) fn persist_audit_value(&self, value: serde_json::Value) {
        let Some(path) = self.audit_log_path() else {
            return;
        };
        crate::jsonl::append(std::path::Path::new(path), &value);
    }

    pub(super) fn persist_audit_report(&self, report: &AuditReport) {
        match serde_json::to_value(report) {
            Ok(v) => self.persist_audit_value(v),
            Err(e) => tracing::warn!(error = %e, "cannot serialize audit report"),
        }
    }

    pub(super) fn audit_log_path(&self) -> Option<&str> {
        self.audit.audit_log_path.as_deref()
    }

    /// Trading-capability self-check result from the live venue bridge. A
    /// failed report freezes trading — the probe exercised the venue paths a
    /// live order shares (authenticated balance, reconciliation sweep), so
    /// failing them means order egress cannot work either.
    ///
    /// 2026-10-25 多场馆雪崩定谳：the freeze is VENUE-SCOPED — the failing
    /// probes are one venue's transport, so only the accounts bound to that
    /// venue stop entering. A global freeze here let polymarket's network
    /// blip veto a healthy polysimulator account's entries for the rest of
    /// the session (the two venues "took turns" being alive — each one's
    /// failure froze the other's survivors). The legacy global
    /// `freeze_trading` stays for causes with no venue dimension (audit
    /// halt, or a venue with no bound account); the `engine.stats.selfCheck`
    /// readout carries the last report regardless of which venue filed it.
    pub fn on_venue_self_check(
        &mut self,
        venue: &str,
        report: blitzkrieg_market_api::SelfCheckReport,
    ) {
        let ok = report.ok;
        let ts = report.ts_ms;
        for item in &report.items {
            eprintln!(
                "core: self-check [{}] {}: {} ({})",
                venue,
                item.name,
                if item.ok { "ok" } else { "FAIL" },
                item.detail
            );
        }
        self.last_self_check = Some(report);
        if !ok {
            // #184: name the failing probe(s) in the message, not just "failed".
            // The items are already in `engine.stats.selfCheck`, but an operator
            // reading the log needs the reason on the line itself.
            let failed = self
                .last_self_check
                .as_ref()
                .map(|r| {
                    r.items
                        .iter()
                        .filter(|i| !i.ok)
                        .map(|i| format!("{}: {}", i.name, i.detail))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let message = if failed.is_empty() {
                format!(
                    "trading self-check failed on {venue} (venue unreachable or credentials rejected)"
                )
            } else {
                format!(
                    "trading self-check failed on {venue}: {}",
                    failed.join("; ")
                )
            };
            tracing::error!(detail = %message, venue = %venue, "trading self-check failed");
            // Root cause into the one slot (code VENUE_ERROR, not the freeze's
            // own wording) — this path has no other carrier of its own.
            self.note_error(
                &crate::model::CoreError::new(
                    blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message.clone(),
                ),
                ts,
            );
            // The freeze lands on the venue's bound accounts (multi-account
            // #532 routing): same contract as `freeze_account` — new entries
            // refused, closes keep flowing.
            for account in self.accounts.account_ids_bound_to_venue(venue) {
                self.freeze_account_auto(&account, message.clone(), ts);
            }
            // No bound account (unbound deployments, dry tests): keep the
            // legacy global freeze so the failure is never silently dropped.
            if !self.accounts.account_ids_bound_to_venue(venue).is_empty() {
                return;
            }
            self.freeze_trading(message, ts);
        }
    }

    /// E31-b: the executor's reconciliation sweep failed three times in a row.
    /// The sweep is the safety net that repairs missed fills, retires ghosts
    /// and cancels orphans; a blind sweep lets the book drift silently, so
    /// trading freezes until the operator or a restart re-probes the channel.
    ///
    /// VENUE-SCOPED (2026-10-25 多场馆雪崩定谳): `venue` names the reporting
    /// plugin; the freeze lands on the accounts bound to it. The per-venue
    /// streak lives in `consecutive_sweep_failures_by_venue` — a healthy
    /// sibling venue's sweeps keep resetting nothing here, and their accounts
    /// keep trading. The legacy global counter/`freeze_trading` path survives
    /// only for callers with no venue attribution (the in-kernel dry sweep).
    pub fn on_venue_reconcile_failed(
        &mut self,
        venue: &str,
        err: crate::model::CoreError,
        now_ms: i64,
    ) {
        let streak = self
            .consecutive_sweep_failures_by_venue
            .entry(venue.to_string())
            .or_default();
        *streak = streak.saturating_add(1);
        let streak = *streak;
        let message = format!("reconciliation sweep failed ({streak}): {}", err.message);
        // #184: the safety net reporting on itself is an ops-critical line, and it
        // belongs at error level from the FIRST failure — not only when the third
        // one finally freezes trading. The streak and the threshold ride along so
        // one line answers "how bad is this and what happens next".
        tracing::error!(
            venue = %venue,
            streak,
            threshold = SWEEP_FAILURE_FREEZE,
            code = ?err.code,
            "{}",
            message
        );
        self.note_error(
            &crate::model::CoreError::new(err.code, message.clone()),
            now_ms,
        );
        if streak >= SWEEP_FAILURE_FREEZE {
            let accounts = self.accounts.account_ids_bound_to_venue(venue);
            if accounts.is_empty() {
                self.freeze_trading(
                    format!(
                        "reconciliation sweep failed {streak} times in a row on {venue}: {}",
                        err.message
                    ),
                    now_ms,
                );
                return;
            }
            for account in accounts {
                self.freeze_account_auto(
                    &account,
                    format!(
                        "reconciliation sweep failed {streak} times in a row on {venue}: {}",
                        err.message
                    ),
                    now_ms,
                );
            }
        }
    }

    /// Legacy global form kept for callers with no venue attribution. The
    /// in-kernel dry sweep and any pre-plugin path land here.
    pub fn on_reconcile_failed(&mut self, err: crate::model::CoreError, now_ms: i64) {
        self.consecutive_sweep_failures = self.consecutive_sweep_failures.saturating_add(1);
        let streak = self.consecutive_sweep_failures;
        let message = format!("reconciliation sweep failed ({streak}): {}", err.message);
        // #184: the safety net reporting on itself is an ops-critical line, and it
        // belongs at error level from the FIRST failure — not only when the third
        // one finally freezes trading. The streak and the threshold ride along so
        // one line answers "how bad is this and what happens next".
        tracing::error!(
            streak,
            threshold = SWEEP_FAILURE_FREEZE,
            code = ?err.code,
            "{}",
            message
        );
        self.note_error(
            &crate::model::CoreError::new(err.code, message.clone()),
            now_ms,
        );
        if streak >= SWEEP_FAILURE_FREEZE {
            self.freeze_trading(
                format!(
                    "reconciliation sweep failed {streak} times in a row: {}",
                    err.message
                ),
                now_ms,
            );
        }
    }

    /// Map a venue order id to a core order (for user-WS events). Returns the
    /// core id when known.
    pub fn core_id_for_venue(&self, venue_or_core: &str) -> Option<String> {
        self.ome
            .by_venue_or_id(venue_or_core)
            .map(|o| o.order_id.clone())
    }

    /// Live orders accepted locally but not yet submitted to the venue (no
    /// venue id bound). The live bridge submits these each tick.
    ///
    /// `place_uncertain` orders are excluded: their POST already timed out, so
    /// the venue's answer is unknown and a re-submit would be a SECOND real
    /// order at any venue without a client-id dedupe. The #462 retirement sweep
    /// (`resolve_place_timeouts`) still sees them and decides their fate.
    pub fn pending_unbound(&self) -> Vec<TrackedOrder> {
        self.ome
            .live_orders()
            .into_iter()
            .filter(|o| o.venue_order_id.is_none() && !o.place_uncertain)
            .cloned()
            .collect()
    }

    /// [`Self::pending_unbound`] filtered to ONE venue: the multi-plugin
    /// executor drain. The routing truth is the ORDER'S OWN execution venue
    /// (`execution_venue_for`: RFC-661 M1 — the placing account's binding
    /// resolves its OWN (venue, token) dual-key row first, so a shared
    /// token's two accounts each route to their own executor; the account
    /// dimension is the AMBIGUITY DISCRIMINATOR, not an override — a
    /// token-only read refuses on two candidate rows and could not pick)
    /// — an order whose execution venue
    /// resolves to nothing, or to a market with no venue label, is UNTAGGED
    /// and routes to the requested plugin only when it is the host's active
    /// plugin — the single-plugin behaviour, unchanged. With the lookup empty
    /// (engine down, replay corpora) every order is untagged, so each executor
    /// sees all of them exactly as the pre-routing bridge did.
    pub fn pending_unbound_for_venue(&self, venue: &str) -> Vec<TrackedOrder> {
        self.pending_unbound()
            .into_iter()
            .filter(|o| self.execution_venue_of(o.account_id.as_str(), &o.token_id) == venue)
            .collect()
    }

    /// The venue that should execute this order: the placing account's
    /// binding rides through [`Engine::execution_venue_for`] — RFC-661 M1:
    /// the (venue, token) dual-key table holds each venue's OWN row for a
    /// byte-identical token, so the binding's own row names the executor.
    /// The account parameter is the AMBIGUITY DISCRIMINATOR (a shared token
    /// carries one row per venue; a token-only read refuses on two
    /// candidates), not a mirror-era leftover — it stays. `""` when the
    /// engine is down or the token belongs to no market: UNTAGGED, never
    /// "venue empty-string" — callers route
    /// untagged work to the active plugin instead of guessing. Fallbacks to
    /// the plain table lookup when the engine is down (same empty answer,
    /// kept total for the host's drain seam, which cannot go through the
    /// engine handle).
    pub fn execution_venue_of(&self, account: &str, token_id: &str) -> String {
        self.engine
            .as_ref()
            .and_then(|e| e.execution_venue_for(account, token_id))
            .unwrap_or_default()
    }

    /// The venue a token belongs to under the live round list, `""` when the
    /// engine is down or the token belongs to no labelled market. Empty means
    /// UNTAGGED, never "venue empty-string": callers route untagged work to
    /// the active plugin instead of guessing.
    pub fn venue_of_token(&self, token_id: &str) -> String {
        self.engine
            .as_ref()
            .and_then(|e| e.venue_of(token_id))
            .unwrap_or_default()
    }

    /// Attach the venue id returned by a successful LIVE POST and mark it live.
    pub fn bind_venue(
        &mut self,
        core_id: &str,
        venue_order_id: String,
        now_ms: i64,
    ) -> CoreResult<()> {
        self.ome.bind_venue(core_id, venue_order_id, now_ms)?;
        self.ome.mark_live(core_id, now_ms)?;
        self.emit_order(core_id);
        Ok(())
    }

    /// Run a reconciliation sweep against a venue snapshot. Applies missed fills
    /// and repairs ghost/partial orders, emitting a report event.
    pub fn reconcile(
        &mut self,
        snap: crate::reconcile::VenueSnapshot,
    ) -> CoreResult<crate::reconcile::ReconcileReport> {
        self.reconcile_for_venue(snap, "")
    }

    /// [`Self::reconcile`] scoped to the REPORTING venue.
    ///
    /// With several plugins live every executor runs its own sweep carrying
    /// only ITS venue's open order ids. Unscoped, each sweep judged every
    /// venue's live orders and force-closed the other venues' resting orders as
    /// ghosts — they kept resting at their own venue, unmanaged, and the orphan
    /// sweep skipped them because `known_venue_ids` still listed them. `""`
    /// keeps the unscoped behaviour (single-venue hosts, in-process tests,
    /// replay).
    pub fn reconcile_for_venue(
        &mut self,
        snap: crate::reconcile::VenueSnapshot,
        venue: &str,
    ) -> CoreResult<crate::reconcile::ReconcileReport> {
        let report = if venue.is_empty() {
            crate::reconcile::reconcile(&mut self.ome, &snap)?
        } else {
            // Orders whose execution venue is a DIFFERENT one are not this
            // sweep's business. An order whose venue cannot be resolved today
            // (engine down, token delisted) stays in scope: no other sweep can
            // prove anything about it either.
            let skip: std::collections::HashSet<crate::model::OrderId> = self
                .ome
                .live_orders()
                .into_iter()
                .filter(|o| {
                    let own = self.execution_venue_of(o.account_id.as_str(), &o.token_id);
                    !own.is_empty() && own != venue
                })
                .map(|o| o.order_id.clone())
                .collect();
            crate::reconcile::reconcile_scoped(&mut self.ome, &snap, &skip)?
        };
        // The sweep channel just proved itself alive — a failed-sweep streak
        // only counts failures in a row (E31-b). The per-venue table clears
        // with it (the snapshot does not carry the reporting venue; one
        // successful sweep anywhere is evidence the transport fabric is
        // alive, and a fresh failure just rebuilds its own streak).
        self.consecutive_sweep_failures = 0;
        self.consecutive_sweep_failures_by_venue.clear();
        // 冻结自愈：a successful sweep proves the transport fabric alive, but
        // the snapshot names no venue — so every AUTO-frozen account gets the
        // same evidence, debounced by FREEZE_AUTO_LIFT_MIN_MS (a fresh freeze
        // rides out the flash outage that caused it before the lift fires).
        // Manual freezes are invisible to this loop by construction.
        for account in self.auto_frozen.keys().cloned().collect::<Vec<_>>() {
            if let Some(t) = self.auto_frozen.get(account.as_str())
                && snap.now_ms - t >= FREEZE_AUTO_LIFT_MIN_MS
            {
                self.auto_lift_if_venue_healthy(
                    account.as_str(),
                    "reconcile sweep succeeded",
                    snap.now_ms,
                );
            }
        }
        // Ledger + position effects for any gap fills the OME just applied.
        for gap in report.actions.iter().filter_map(|a| match a {
            crate::reconcile::ReconcileAction::FilledGap { delta, .. } => Some(delta.clone()),
            _ => None,
        }) {
            self.apply_delta_effects(gap, snap.now_ms);
        }
        // Fills the sweep absorbed into an order that had already ended (#178):
        // the size is booked, the terminal status kept, and the operator told.
        for late in report.late_fills.clone() {
            self.emit_late_fill(&late, snap.now_ms);
        }
        // Trades on orders the OME never issued (manual closes made directly
        // on the venue): fold them into the position book so the position
        // cannot haunt the exit rules forever.
        self.apply_external_fills(&report.unknown_fills, snap.now_ms);
        // A terminal status the sweep just stamped (filled-without-gap,
        // vanished→cancelled, ghost→force-closed) must reach orders.jsonl the
        // same way every other status change does — otherwise a restart
        // re-adopts terminal orders as LIVE and force-closes them all over
        // again. emit_order already persists; these mutations bypass it.
        for id in report.actions.iter().filter_map(|a| match a {
            crate::reconcile::ReconcileAction::MarkedFilled { core_order_id }
            | crate::reconcile::ReconcileAction::MarkedCancelled { core_order_id }
            | crate::reconcile::ReconcileAction::MarkedForceClosed { core_order_id } => {
                Some(core_order_id.as_str())
            }
            crate::reconcile::ReconcileAction::FilledGap { .. } => None,
        }) {
            self.persist_order(id);
        }
        // #462: retire place-timeout orders the venue never gave evidence for
        // (see `resolve_place_timeouts`).
        self.resolve_place_timeouts(&snap);
        if !report.actions.is_empty()
            || !report.suspect_ghost_ids.is_empty()
            || !report.unknown_fills.is_empty()
        {
            let filled = report
                .actions
                .iter()
                .filter(|a| matches!(a, crate::reconcile::ReconcileAction::FilledGap { .. }))
                .count();
            let marked_filled = report
                .actions
                .iter()
                .filter(|a| matches!(a, crate::reconcile::ReconcileAction::MarkedFilled { .. }))
                .count();
            let marked_cancelled = report
                .actions
                .iter()
                .filter(|a| matches!(a, crate::reconcile::ReconcileAction::MarkedCancelled { .. }))
                .count();
            let marked_force_closed = report
                .actions
                .iter()
                .filter(|a| {
                    matches!(
                        a,
                        crate::reconcile::ReconcileAction::MarkedForceClosed { .. }
                    )
                })
                .count();
            // #184: a sweep that had to REPAIR something is a drift report — the
            // local book disagreed with the venue and the safety net caught it.
            // The net working is good news; the divergence is not, and an
            // operator has to be able to see it in the run log without reading
            // the panel. Only repairs and ghost suspicions are errors: external
            // fills alone are a manual close made on the venue (informational).
            if !report.actions.is_empty() || !report.suspect_ghost_ids.is_empty() {
                tracing::error!(
                    filled_gaps = filled,
                    marked_filled,
                    marked_cancelled,
                    marked_force_closed,
                    ghosts = ?report.suspect_ghost_ids,
                    "reconciliation drift repaired: the venue disagreed with the core's book"
                );
            } else {
                tracing::info!(
                    external_fills = report.unknown_fills.len(),
                    "reconciliation: external (manual) fills folded into the position book"
                );
            }
            self.emit(Event::ReconcileReport {
                filled,
                marked_filled,
                marked_cancelled,
                marked_force_closed,
                ghost_ids: report.suspect_ghost_ids.clone(),
            });
        }
        Ok(report)
    }

    /// Fold external fills (venue trades on orders the OME does not track) into
    /// the position book. Only SELLs are applied: a manual close reduces the
    /// matching position by token id, settling the ledger for the cash that
    /// actually moved, and closes the position outright when it empties.
    /// Manual BUYS are skipped — cash truth comes from the periodic
    /// `venue_free_balance` realign, and inventing an unmanaged position here
    /// would fight the engine. Dedup is a persisted watermark: only fills
    /// strictly newer than the last applied one count, so a restart cannot
    /// re-apply what the sweep still reports.
    ///
    /// F2: the venue adapter only forwards legs it proved belong to our
    /// funder, and this loop stays conservative anyway — an unknown SELL is
    /// never booked beyond the inventory the token's position can actually
    /// attribute: size AND proceeds are truncated to what is held, so a
    /// mis-attributed over-sell cannot mint cash.
    pub(super) fn apply_external_fills(
        &mut self,
        fills: &[crate::reconcile::VenueTrade],
        now_ms: i64,
    ) {
        if fills.is_empty() {
            return;
        }
        let fresh: Vec<&crate::reconcile::VenueTrade> = fills
            .iter()
            .filter(|t| t.ts_ms > self.recon_watermark_ms)
            .collect();
        if fresh.is_empty() {
            return;
        }
        let mut applied = 0usize;
        for t in &fresh {
            if t.side != Side::Sell {
                continue;
            }
            let Some((position_id, held, position_account)) = self
                .positions
                .open_positions()
                .iter()
                .find(|p| p.token_id == t.token_id)
                .map(|p| (p.id.clone(), p.shares, p.account_id.clone()))
            else {
                continue;
            };
            // F2: clamp the size to attributable inventory and price the cash
            // on the CLAMPED size — truncating the size while keeping the full
            // notional would still book the counterparty's money.
            let size = t.size.min(held);
            if size <= Decimal::ZERO {
                continue;
            }
            let notional = t.price * size;
            let fee_usd = if t.maker == Some(true) {
                Decimal::ZERO
            } else {
                // The fee follows the schedule in force, under the same
                // replay-only guard the fill choke point uses (#234, item 4).
                (self.live_taker_fee_pct(t.price) / Decimal::ONE_HUNDRED) * notional
            };
            // E28: a manual (external) close settles into the POSITION'S OWN
            // book — the proceeds belong to the account that paid for the
            // shares. An unknown account is logged and skipped (no implicit
            // book, no silent mint).
            if let Ok(l) = self.accounts.get_mut(&position_account) {
                l.settle_sell_fill(notional, fee_usd);
            } else {
                tracing::error!(
                    token = %t.token_id,
                    account = %position_account.as_str(),
                    "external sell for an unknown account dropped — proceeds not booked"
                );
                continue;
            }
            let role = if t.maker == Some(true) {
                crate::model::OrderRole::Maker
            } else {
                crate::model::OrderRole::Taker
            };
            let remaining = self
                .positions
                .apply_exit_fill(&position_id, size, t.price, fee_usd, role, Decimal::ZERO)
                .unwrap_or(Decimal::ZERO);
            if remaining == Decimal::ZERO {
                // F10: a manual full close must hit the SAME close ledger the
                // engine's own fills do — TradeDb, strategy PnL, win/loss and
                // the consecutive-loss breaker. `close()` removes the position
                // from the book, so re-reporting the same trade cannot close
                // it twice: that removal IS the idempotence guard.
                if let Some(closed) = self.positions.close(
                    &position_id,
                    t.price,
                    ExitReason::Manual,
                    t.maker == Some(true),
                    now_ms,
                ) {
                    self.persist_positions();
                    self.on_position_closed(&closed, now_ms);
                }
                // Belt and braces: this path can close without the OME ever
                // issuing an order, and the pending-close reason must not
                // outlive the position it belonged to (issue #190) — the
                // reason is retired here even if no closed record came back.
                self.clear_exit_reason(&t.token_id);
            }
            self.persist_positions();
            applied += 1;
            tracing::info!(
                token = %t.token_id,
                requested_size = %t.size,
                applied_size = %size,
                price = %t.price,
                remaining = %remaining,
                "external close reconciled (manual venue sell folded into the position book)"
            );
        }
        // #462: fold unknown BUYs into the position book — a place-timeout
        // order the venue really filled arrives here (the OME never saw the
        // order id), and skipping it left real shares unmanaged while the
        // cash was already gone. Rebuilt through `apply_delta_effects`, the
        // ONE ledger+position+event path every other fill takes, so the
        // reconstructed position feeds exits/settlement exactly like an
        // engine-placed one. Metadata (strategy/condition/slot/direction)
        // comes from the engine's market registry for the token; a token no
        // live market names stays skipped (no inventable metadata), and the
        // default account takes the position (the ABI carries no account on
        // venue trades — the same default the SELL fold settles into).
        for t in fresh.iter().filter(|t| t.side == Side::Buy) {
            let Some(market) = self.engine.as_ref().and_then(|e| {
                e.scanner()
                    .markets()
                    .iter()
                    .find(|m| m.up_token_id == t.token_id || m.down_token_id == t.token_id)
            }) else {
                continue;
            };
            let direction = if market.up_token_id == t.token_id {
                "up"
            } else {
                "down"
            };
            let role = if t.maker == Some(true) {
                crate::model::OrderRole::Maker
            } else {
                crate::model::OrderRole::Taker
            };
            let d = crate::ome::FillDelta {
                order_id: format!("recon:{}", t.venue_order_id),
                token_id: t.token_id.clone(),
                account_id: crate::model::default_account_id(),
                side: Side::Buy,
                delta: t.size,
                price: t.price,
                cumulative: t.size,
                strategy: "reconciled".to_string(),
                asset: market.asset.clone(),
                direction: direction.to_string(),
                condition_id: market.condition_id.clone(),
                round_slot: market.round_slot,
                mode: crate::model::FillPolicy::Taker,
                role,
                limit_price: t.price,
                trade_id: t.trade_id.clone(),
            };
            tracing::warn!(
                token = %t.token_id,
                venue_order = %t.venue_order_id,
                size = %t.size,
                price = %t.price,
                "external BUY reconciled — position rebuilt (a timed-out place the venue really filled, #462)"
            );
            self.apply_delta_effects(d, now_ms);
        }
        // Sells with no matching position are expected noise; one line per
        // sweep keeps the log honest. BUYs are folded above (#462).
        let unmatched = fresh.iter().filter(|t| t.side == Side::Sell).count() - applied;
        if unmatched > 0 {
            eprintln!("core: external fills not matched to a position: sells={unmatched}");
        }
        let new_mark = fresh
            .iter()
            .map(|t| t.ts_ms)
            .max()
            .unwrap_or(self.recon_watermark_ms);
        self.recon_watermark_ms = self.recon_watermark_ms.max(new_mark);
        if let Some(db) = self.position_db.as_ref() {
            db.save_watermark(self.recon_watermark_ms);
        }
    }

    /// Retire every order a place TIMEOUT left pending (#462) when the venue
    /// never gave evidence for it.
    ///
    /// `note_place_timeout` deliberately does NOT reject: the venue may have
    /// accepted the POST after the client gave up. But "pending forever"
    /// leaks the cash reservation and strands the intent, and the OME-side
    /// ghost sweep cannot judge these orders — they carry no venue id to
    /// match, so it skips them. The three endings all resolve without
    /// adoption:
    ///   really filled   → the unknown BUY fold in `apply_external_fills`
    ///                     books the position; this retirement then frees
    ///                     the now-stale reservation;
    ///   really resting  → the bridge's orphan sweep cancels the unknown
    ///                     venue id (exposure removed at the venue);
    ///   never received  → nothing at the venue; only this retirement
    ///                     closes the local intent.
    /// The budget clears the bridge's sweep cadence (5 s) plus the place
    /// timeout itself, so a POST that is merely slow is never raced, and
    /// the folded position occupies the per-asset slot in the window
    /// before retirement — a strategy re-attempt cannot double the entry.
    pub(super) fn resolve_place_timeouts(&mut self, snap: &crate::reconcile::VenueSnapshot) {
        for (id, submitted_at) in self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| o.venue_order_id.is_none() && o.status == OrderStatus::Pending)
            .map(|o| (o.order_id.clone(), o.submitted_at_ms))
            .collect::<Vec<_>>()
        {
            if snap.now_ms - submitted_at < PLACE_TIMEOUT_VERDICT_MS {
                continue;
            }
            if let Err(e) = self.cancel(&id, snap.now_ms) {
                tracing::warn!(order = %id, error = %e, "place-timeout retirement failed");
            } else {
                tracing::warn!(
                    order = %id,
                    waited_ms = snap.now_ms - submitted_at,
                    "place timed out and the venue never acknowledged it — order retired, reservation released (#462)"
                );
            }
        }
    }

    // Replay sched is excluded wholesale: the sweep mutates the order set (a
    // tradesSha256 input) and replayed archives have no ghost-relevant clock —
    // the historical per-order flows own every transition there.
    pub(super) fn run_expiry_sweep(&mut self, now_ms: i64) {
        if self.replay_sched() {
            return;
        }
        let dur_ms = self.config.round_duration_sec * 1000;
        if dur_ms <= 0 {
            return;
        }
        // B1b: EVERY live order is judged on ITS OWN family clock — the
        // three-state effective duration (own stamp > market declaration >
        // global grid; a `0` family folds to the global grid, exactly the
        // pre-B1b value). Single-family equivalence: a deployment where every
        // market declares the configured grid (or nothing) and every stamp
        // rides the grid resolves eff == cfg here, byte-for-byte the old
        // filter. The expired ids are collected BEFORE the mutation pass (the
        // market lookup reads the engine).
        let mut expired: Vec<(String, i64)> = Vec::new();
        for o in self.ome.live_orders() {
            if o.round_slot <= 0 {
                continue;
            }
            let eff = self.order_eff_round_sec(o);
            if eff > 0 && now_ms >= (o.round_slot + 1) * eff * 1000 {
                expired.push((o.order_id.clone(), eff));
            }
        }
        for (id, eff) in expired {
            // Segment 2 of the wiring template: the lazy idempotent stamp —
            // a `0` family is written once from the resolution the expiry
            // judgment just made; a non-0 stamp is never touched.
            self.ome.backfill_round_duration(&id, eff);
            self.force_close_order(&id, now_ms, "round expired").ok();
        }
        // B1b lazy backfill: positions opened before the family stamp existed
        // (restored legacy snapshots) get their family written the first time
        // the expiry consumer touches them — idempotent
        // (`backfill_round_duration` never overwrites a non-0 stamp), so
        // running this every sweep is free. The families are resolved BEFORE
        // the mutable borrow (the market lookup reads the engine).
        let families: std::collections::HashMap<String, i64> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.round_duration_sec == 0)
            .map(|p| (p.token_id.clone(), self.round_family_of_token(&p.token_id)))
            .collect();
        self.positions
            .backfill_round_families(|tok| families.get(tok).copied().unwrap_or(0));
    }

    /// E-VA companion: the single-leg orphan sweep for ATOMIC pair strategies.
    ///
    /// The atomic batch (E-VA) guarantees pair legs leave the book together —
    /// but only at SUBMISSION. Afterwards the legs live and die on their own:
    /// one leg fills, the partner's resting order is cancelled (manual, the
    /// #457 expiry sweep, a venue reject) and the strategy is left holding a
    /// NAKED leg — exposure the pair economics never priced, earning nothing
    /// and settling at the coin flip. This sweep is the compensation watch:
    /// for every (account, strategy) scope that BOTH declared `atomic_batch`
    /// AND has a governed virtual pot, a position whose partner token has no
    /// sibling position, no live partner BUY order and no in-flight partner
    /// taker past the grace window is the orphan — the sweep first tries to
    /// CHASE the missing cross-venue leg inside the pair's budget (below),
    /// and only when that is refused — the book too dear, a guard or a
    /// placement refusal — alerts once (the existing `RiskAlert` event, no
    /// new wire type) and flattens through the manual exit path (`flatten`,
    /// Taker walk out of the position's own account).
    ///
    /// Scope discipline:
    ///   - zero-config = zero behaviour: a scope with no cap in
    ///     `virtual_limits` is ungoverned and never touched — the replay
    ///     baseline stays byte-identical;
    ///   - live path only (`replay_sched()` skips — the sweep is live-panel
    ///     semantics, the nofast replay loop keeps its own shape);
    ///   - the 5 s grace window covers the serial same-tick leg landing (both
    ///     legs of one batch fill inside one evaluate cycle, and the partner's
    ///     position row can lag the first leg's by that whole pass); a
    ///     constant, not a tunable — the window is a bookkeeping race bound,
    ///     not a policy dial;
    ///   - debounce is structural: a flattened leg leaves the book, so the
    ///     next tick's condition is simply false; a flatten that failed
    ///     because a live SELL already rests for the token is skipped by
    ///     `flatten` itself (no retry storm, no extra ledger).
    ///
    /// Chase compensation (#531, the feasible arm): the missing leg is the
    /// strategy's most recent BUY order on this account for a token that is
    /// NEITHER the naked leg NOR its same-market partner (the cross-venue
    /// counterpart — `partner_token_id` names only the same-market completion
    /// token, and OME history is the only place the dead leg's token, venue
    /// provenance and round identity survive). The budget is the pair's own:
    /// the naked leg's per-share basis (`cost_usd / shares`) plus the chase
    /// price must stay within the $1.00 a UP+DOWN share pair redeems — this
    /// is the same arithmetic `escalation_guard`'s pair-cost cap applies,
    /// but that cap is keyed by `condition_id` and cannot see a cross-venue
    /// counterpart whose legs carry different condition ids, so the sweep
    /// prices the budget ITSELF into the chase limit and hands the guard a
    /// request that cannot trip the cap. A stale or empty book (`None`
    /// walk) and any guard/placement refusal are the INFEASIBLE arm: the
    /// existing alert+flatten fallback below fires unchanged.
    pub(super) fn run_pair_orphan_sweep(&mut self, now_ms: i64) {
        if self.replay_sched() {
            return;
        }
        /// Same-tick serial landing window for a pair's two legs (see the
        /// scope discipline above). Not a tunable.
        const PAIR_LAND_GRACE_MS: i64 = 5_000;
        /// How long a placed chase has to land before the sweep stops giving
        /// it the benefit of the doubt and falls back to alert+flatten. A
        /// dry taker fills inside the same `place` call (zero latency dials)
        /// so the window is really only a LIVE-path patience bound and a
        /// debounce: while it runs, the orphan condition below keeps being
        /// true (the partner still has no position/order) and without the
        /// window the sweep would flatten the leg out from under its own
        /// chase on the very next tick. A constant, not a tunable — the
        /// flatten fallback stays the final answer either way.
        const PAIR_CHASE_WINDOW_MS: i64 = 2_000;
        // The governed atomic scopes: every (account, strategy) pair the pot
        // book has a cap for — the strategy's `atomic_batch` declaration is
        // implied by the pot existing (`virtual_limits` is only wired to
        // atomic strategies in practice; a governed scope for a non-atomic
        // strategy is a config decision we honor the same way, since the
        // orphan condition below is strategy-shape agnostic).
        let governed: Vec<(String, String)> = self.virtual_accounts.scopes();
        for (account_id, strategy) in governed {
            // The live BUY orders this strategy rests on this account, keyed
            // by token: a partner with a still-breathing entry order is not
            // orphaned (its leg is arriving), and the position side of the
            // SAME order set covers the inverse asymmetry (first leg still
            // resting + second leg already held).
            let live_partner_buys: std::collections::HashSet<String> = self
                .ome
                .live_orders()
                .into_iter()
                .filter(|o| {
                    o.side == Side::Buy
                        && o.strategy == strategy
                        && o.account_id.as_str() == account_id.as_str()
                })
                .map(|o| o.token_id.clone())
                .collect();
            let inflight_tokens: std::collections::HashSet<String> = self
                .inflight_takers
                .iter()
                .filter_map(|leg| self.ome.get(&leg.order_id))
                .filter(|o| {
                    o.side == Side::Buy
                        && o.strategy == strategy
                        && o.account_id.as_str() == account_id.as_str()
                })
                .map(|o| o.token_id.clone())
                .collect();
            let orphans: Vec<(String, String, String, String, Decimal, Decimal)> = self
                .positions
                .open_positions()
                .iter()
                .filter(|p| {
                    p.strategy == strategy
                        && p.account_id.as_str() == account_id
                        && !p.partner_token_id.is_empty()
                        // #779: the chase budget prices the per-share basis as
                        // `cost_usd / shares`. A zero-share row (an entry
                        // rolled back to zero, or a shell `drop_if_empty` has
                        // not removed yet) is not a naked leg — dividing by it
                        // PANICS. Never a candidate.
                        && p.shares > Decimal::ZERO
                        // Settlement-dead positions belong to the settlement
                        // ladder, not the unwind desk: a hold-to-settlement
                        // pair rides PAST its round end by design (the same
                        // live-only rule `can_open` and the systemic facts
                        // count by). A naked leg is only unwound at market
                        // prices while its market is still live.
                        && p.expires_at_ms > now_ms
                        && now_ms - p.entered_at_ms >= PAIR_LAND_GRACE_MS
                        && !self.positions.open_positions().iter().any(|sib| {
                            sib.token_id == p.partner_token_id
                                && sib.strategy == strategy
                                && sib.account_id.as_str() == account_id
                        })
                        // The #427 cross-venue completion set: the package's
                        // pair lands as OPPOSITE directions on the same asset
                        // in the same round (one leg per venue), while
                        // `partner_token_id` only ever names the SAME-market
                        // completion token — without this clause a fully
                        // landed pair would read as two orphans and be
                        // flattened whole, destroying the locked profit the
                        // watch exists to protect. An opposite-direction
                        // sibling in the same scope and round window (the
                        // shared `expires_at_ms` is the round identity the
                        // position carries) is the market-neutral set the
                        // strategy built; it is not naked.
                        && !self.positions.open_positions().iter().any(|sib| {
                            sib.strategy == strategy
                                && sib.account_id.as_str() == account_id
                                && sib.asset == p.asset
                                && sib.expires_at_ms == p.expires_at_ms
                                && sib.direction != p.direction
                        })
                        && !live_partner_buys.contains(&p.partner_token_id)
                        && !inflight_tokens.contains(&p.partner_token_id)
                })
                .map(|p| {
                    (
                        p.id.clone(),
                        p.token_id.clone(),
                        p.venue.clone(),
                        p.partner_token_id.clone(),
                        // The chase budget's paid half: the naked leg's
                        // per-share basis. The entry notional spent per
                        // share is what the missing leg's price must fit
                        // under ($1.00 a pair redeems).
                        p.cost_usd / p.shares,
                        p.shares,
                    )
                })
                .collect();
            for (id, token, venue, partner, basis, shares) in orphans {
                // ── Chase arm (#531 feasible): find the dead cross-venue leg
                // in OME history and re-offer it within the pair budget. The
                // infeasible arm keeps the historical alert+flatten shape
                // byte-for-byte.
                let account = AccountId::from(account_id.as_str());
                let chase = self.pair_chase_target(&account, &strategy, &token, &partner);
                let chased = match chase
                    .map(|t| {
                        // The budget cap the chase may state: the naked leg's
                        // basis is the paid half; the missing leg buys at the
                        // counterparty and the PAIR must stay inside $1.00.
                        (t, Decimal::ONE - basis)
                    })
                    .filter(|(_, cap)| *cap > Decimal::ZERO)
                {
                    Some((target, cap)) => self.pair_chase_missing_leg(
                        &target,
                        (&token, &account, &strategy),
                        shares,
                        cap,
                        now_ms,
                    ),
                    None => false,
                };
                if chased {
                    // The chase is out (a filled position or a resting BUY).
                    // The naked leg is not orphaned any more — the next tick's
                    // condition either reads the partner side (filled) or the
                    // live-BUY clause (resting), and the chase window above
                    // keeps this tick's re-judgment off the leg entirely.
                    continue;
                }
                // Chase window debounce: a chase this scope placed within the
                // window is still adjudicating (a live taker, a resting leg).
                // Skip WITHOUT alerting or flattening — the window is the
                // chase's benefit of the doubt; after it, the fallback below
                // fires.
                if let Some((_, at)) = self
                    .pair_chase_attempts
                    .get(&(account_id.clone(), strategy.clone(), token.clone()))
                    .cloned()
                    && now_ms - at < PAIR_CHASE_WINDOW_MS
                {
                    continue;
                }
                let message = format!(
                    "pair leg orphaned: strategy={strategy} account={account_id} \
                     held={token}@{venue} partner_missing={partner} — \
                     flattening the naked leg"
                );
                tracing::warn!(position = %id, token = %token, partner = %partner,
                    "pair leg orphaned — flattening the naked leg");
                self.emit(Event::RiskAlert {
                    code: CoreErrorCode::RiskRejected,
                    message,
                });
                // The manual exit path: SELL Taker walk out of the position's
                // own account (E28 §9.2), ExitReason::Manual — no new enum
                // value, the audit line above carries the real cause. A
                // refusal (a live SELL already resting, a halted breaker) is
                // the sweep's own skip posture: the position stays in the
                // book and the next tick re-judges it, so the result is
                // honestly dropped rather than swallowed.
                let _ = self.flatten(Some(&id), now_ms);
                // RFC 568 阶段一（观察者写点）：flatten 已处置 ⇒ 配对落
                // Closed 终态（判定链零改动 —— sweep 的 orphan 判定照旧直读
                // OME 与持仓，这里只落账）。寻址按 token 形（close_by_token）：
                // 经济键第二元是实例主账户，sweep 手里只有腿的落账账户
                // （v2 A≠B 时两者不同），token 唯一对应本轮一个市场，绕开
                // 这层不对称；幂等（同 token 跨轮旧键也一并闭账）。
                self.pair_book.close_by_token(&strategy, &token);
                // The chase did not save the leg: retire its attempt record so
                // the next tick can offer a fresh chase (a repriced book may
                // have made the pair feasible again) instead of waiting out a
                // stale window.
                self.pair_chase_attempts.remove(&(
                    account_id.clone(),
                    strategy.clone(),
                    token.clone(),
                ));
            }
        }
    }

    /// The chase arm's TARGET: the missing cross-venue leg's token, read out
    /// of the OME's order history. It is this (account, strategy) scope's
    /// most recent BUY for a token that is neither the naked leg nor the
    /// naked leg's same-market partner (`partner_token_id` names only the
    /// same-market completion token; the cross-venue counterpart's token is
    /// nowhere on the position row — the dead ORDER is the only evidence of
    /// which token the pair attempt was completing with). `None` = no
    /// missing-leg order ever existed (the entry died before resting — the
    /// starved-ledger shape) → the sweep goes straight to alert+flatten.
    pub(super) fn pair_chase_target(
        &self,
        account_id: &AccountId,
        strategy: &str,
        naked_token: &str,
        partner: &str,
    ) -> Option<String> {
        // The most recent attempt is the leg the strategy was completing:
        // `by_submitted` is the insertion order, so `.all()` is oldest first
        // — `.rfind()` reads the newest.
        self.ome
            .all()
            .into_iter()
            .filter(|o| {
                o.side == Side::Buy
                    && o.token_id != naked_token
                    && o.token_id != partner
                    && o.strategy == strategy
                    && o.account_id == *account_id
            })
            .rfind(|_| true)
            .map(|o| o.token_id.clone())
    }

    /// Re-offer the missing leg: price it off the CURRENT book (worst fill
    /// price the walk needs, `cap` = the pair budget's chase half), state the
    /// full naked size as a dry-FOK Taker, run the same guard every
    /// escalation runs, and place through `place_escalated` (entry gates off —
    /// this COMPLETES the naked position's pair, exactly the gap-fill case
    /// the one-per-asset gate would otherwise refuse). `Ok(true)` = the leg
    /// is out (filled into a position or resting live); `Ok(false)` or any
    /// error = infeasible, the caller's alert+flatten fallback proceeds.
    ///
    /// The internal key carries the slot so a NEXT round's orphan gets a
    /// fresh key (the OME refuses a duplicate live intent key): the round
    /// identity comes from the historical order, the natural fingerprint of
    /// "this pair attempt".
    /// The naked leg's identity, borrowed: (token, account, strategy) — the
    /// one tuple every chase bookkeeping (request provenance, attempt key,
    /// tracing) reads.
    pub(super) fn pair_chase_missing_leg(
        &mut self,
        missing_token: &str,
        naked: (&str, &AccountId, &str),
        size: Decimal,
        cap: Decimal,
        now_ms: i64,
    ) -> bool {
        let (naked_token, account_id, strategy) = naked;
        let Some((vwap, worst)) = self.marketable_walk(Side::Buy, missing_token, size, cap, now_ms)
        else {
            // The book cannot cover the naked size within the pair budget (or
            // is stale/absent) — the infeasible arm.
            return false;
        };
        // B1b: the chase completes the NAKED leg's pair attempt, so it runs on
        // the naked leg's family clock, not the configured grid — the missing
        // leg is sized/timed against the same round the pair was admitted
        // under. The naked POSITION carries the family (stamped at open,
        // backfilled at the expiry consumer); a `0` stamp folds to the global
        // grid, the pre-B1b slot fallback byte-for-byte.
        let naked_family = self
            .positions
            .open_positions()
            .iter()
            .find(|p| {
                p.token_id == naked_token
                    && p.account_id.as_str() == account_id.as_str()
                    && p.strategy == strategy
            })
            .map(|p| self.position_eff_round_sec(p))
            .filter(|eff| *eff > 0);
        let slot = self.books_round_slot(missing_token).unwrap_or_else(|| {
            let fam = naked_family
                .unwrap_or(self.config.round_duration_sec)
                .max(1);
            now_ms / (fam * 1000).max(1)
        });
        let internal_key = format!("pair-chase:{naked_token}:{missing_token}:{slot}");
        let req = OrderRequest {
            token_id: missing_token.to_string(),
            // The guard's pair-cost cap keys on condition_id and the
            // counterpart legs carry DIFFERENT condition ids (that is why the
            // sweep prices the budget itself); the request carries the naked
            // leg's identity so the accounting and events name the pair
            // attempt, not a fresh condition.
            condition_id: naked_token.to_string(),
            side: Side::Buy,
            mode: FillPolicy::Taker,
            price: worst,
            size,
            internal_key: internal_key.clone(),
            strategy: strategy.to_string(),
            asset: self
                .books_asset(missing_token)
                .unwrap_or_else(|| "BTC".to_string()),
            direction: self
                .books_direction(missing_token)
                .unwrap_or_else(|| "up".to_string()),
            round_slot: slot,
            account_id: account_id.clone(),
        };
        // One attempt per window: stamp BEFORE placing — if the place dies,
        // the stamp is what keeps this tick's fallback and the window's
        // re-judgment from double-firing.
        self.pair_chase_attempts.insert(
            (
                account_id.as_str().to_string(),
                strategy.to_string(),
                naked_token.to_string(),
            ),
            (missing_token.to_string(), now_ms),
        );
        if let Err(reason) = self.escalation_guard(&req, vwap) {
            tracing::info!(
                strategy = %strategy,
                account = %account_id.as_str(),
                token = %missing_token,
                naked = %naked_token,
                price = %worst,
                cap = %cap,
                reason = %reason,
                "pair chase refused by the guard — falling back to alert+flatten"
            );
            return false;
        }
        match self.place_escalated(req, now_ms) {
            Ok((order_id, status)) => {
                // B1b: the chase order inherits the naked leg's family — the
                // same lazy idempotent stamp the placement path's market
                // lookup would have resolved, but the PAIR attempt's family
                // is authoritative (the missing leg's own market may declare
                // a different family than the round the pair was priced
                // under). A non-0 stamp is never overwritten.
                if let Some(fam) = naked_family {
                    self.ome.backfill_round_duration(&order_id, fam);
                }
                tracing::info!(
                    order = %order_id,
                    ?status,
                    strategy = %strategy,
                    account = %account_id.as_str(),
                    token = %missing_token,
                    naked = %naked_token,
                    price = %worst,
                    size = %size,
                    "pair chase placed — the missing leg is re-offered past the counterparty"
                );
                // RFC 568 阶段一（观察者写点）：chase 成功 ⇒ 把 chase 订单的
                // internal_key 绑到配对簿的缺失腿（naked token 的配对腿）——
                // chase 的成交/死亡事实经同一 on_order_event 通道更新配对状
                // 态（Partial → Complete 或 Orphan）。判定链零改动：绑回是
                // 落账，不是输入。
                let key = self
                    .ome
                    .get(&order_id)
                    .map(|o| o.internal_key.clone())
                    .unwrap_or_else(|| format!("pair-chase:{naked_token}:{missing_token}:{slot}"));
                self.pair_book
                    .bind_by_leg(strategy, account_id.as_str(), missing_token, &key);
                true
            }
            Err(e) => {
                tracing::info!(
                    strategy = %strategy,
                    account = %account_id.as_str(),
                    token = %missing_token,
                    naked = %naked_token,
                    reason = %e.message,
                    "pair chase placement failed — falling back to alert+flatten"
                );
                false
            }
        }
    }

    /// #457: close a STILL-LIVE order the kernel judges settlement-dead —
    /// mark `ForceClosed`, release its reservation, and (live mode) queue a
    /// venue cancel so the real venue state converges with the book. Refuses
    /// an order that could still fill: the manual verb is for ghosts, and a
    /// caller that wants to withdraw a live order has `cancel`.
    pub fn force_close_order(&mut self, id: &str, now_ms: i64, reason: &str) -> CoreResult<()> {
        let order = self
            .ome
            .get(id)
            .cloned()
            .ok_or_else(|| CoreError::new(CoreErrorCode::UnknownOrder, id.to_string()))?;
        if !order.status.is_live() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidOrderState,
                format!("order {id} is already terminal ({:?})", order.status),
            ));
        }
        if order.round_slot > 0 {
            // B1b: the round's liveness is judged on the ORDER'S OWN family
            // clock — the three-state eff (own stamp > market declaration >
            // global grid), not the configured grid: a 3600s order must stay
            // live for its full hour even while the core runs 900s rounds.
            // A `0` family folds to the global grid — the pre-B1b value —
            // and the resolution just made is the lazy idempotent stamp.
            let eff = self.order_eff_round_sec(&order);
            let dur_ms = eff * 1000;
            let expires_at = (order.round_slot + 1) * dur_ms;
            self.ome.backfill_round_duration(id, eff);
            if now_ms < expires_at {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidOrderState,
                    format!(
                        "order {id}'s round is still live until {expires_at} — use cancel, not force-close"
                    ),
                ));
            }
        }
        let was_live = order.status.is_live();
        if order.side == Side::Buy
            && let Ok(l) = self.accounts.get_mut(&order.account_id)
        {
            l.release(id);
        }
        // E-VA: expiry/force-close releases the virtual hold too.
        self.virtual_release_order(&order);
        self.ome
            .mark_terminal(id, OrderStatus::ForceClosed, now_ms)?;
        // RFC 568 阶段一：死亡事实（过期强平）喂配对簿。
        self.pair_on_order_event(&order.internal_key, Decimal::ZERO, true);
        if let Some(engine) = self.engine.as_mut() {
            engine.release_pending_token_for_strategy(
                order.account_id.as_str(),
                &order.strategy,
                &order.token_id,
            );
        }
        if self.replay_sched() {
            self.fast_orders_dirty = true;
        }
        if was_live && let Some(vid) = order.venue_order_id {
            let venue = self.execution_venue_of(order.account_id.as_str(), &order.token_id);
            self.queue_venue_cancel(venue, vid);
        }
        tracing::warn!(
            order = %id,
            strategy = %order.strategy,
            token = %order.token_id,
            reason = %reason,
            "order force-closed by the kernel — round expired and no honest terminal receipt arrived (#457)"
        );
        // The audit trail's whole point: "we withdrew" (Cancelled) and "we gave
        // up waiting" (ForceClosed) must stay readable apart on disk.
        self.persist_audit_value(serde_json::json!({
            "kind": "force_close",
            "ts_ms": now_ms,
            "order_id": id,
            "strategy": order.strategy,
            "token_id": order.token_id,
            "round_slot": order.round_slot,
            "reason": reason,
        }));
        self.emit_order(id);
        Ok(())
    }
}
