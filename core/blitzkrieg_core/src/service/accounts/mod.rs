//! The multi-account money truth and its boot/persistence plumbing, moved
//! verbatim from `service.rs` (pure code motion, no logic edits): balance
//! seeding and venue-cash realignment (with the #702 windowed venue-evidence
//! family — `window_cash_facts` / `report_venue_cash_move` / ghost alerts —
//! kept together), the accounts.toml persisting writers, the cross-venue arb
//! config and RFC 568 pair book, the in-kernel accounting audit, the venue
//! reconcile sweep with external-fill folding, the order/position restore
//! helpers and the expiry/order sweeps that guard them.
//!
//! #780: the file is split into cohesive children — `persist` (accounts.toml
//! writers + scope rebuild), `audit` (the in-kernel accounting audit and its
//! journal), `reconcile` (venue self-check / reconcile bookkeeping) and
//! `sweeps` (expiry, pair-orphan and force-close sweeps). This module keeps
//! the core money truth and account views; the struct fields stay private to
//! `service` and descendants either way.

use crate::ledger::Ledger;
use crate::model::*;
use crate::schema::Event;
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;

use super::{Core, fee_quote};

mod audit;
mod persist;
mod reconcile;
mod sweeps;

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
    pub fn fee_quote(&self, price: Option<Decimal>) -> crate::schema::FeeQuoteResult {
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
            self.freeze_book
                .venue_free_by_venue
                .insert(venue.to_string(), (ts, b));
        }
        if let Ok(ledger) = self.accounts.get_mut(&target) {
            // issue 749: this is VENUE truth, not a local funding number —
            // record it so a live account shows the balance even when it
            // never declared credential env names.
            ledger.apply_venue_report(b);
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
    /// Same shape as [`Self::emit_late_fill`] (schema/mod.rs is not this
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

    /// issue 749: whether a VENUE has reported the ACTIVE account's cash. The
    /// process-level balance view (`ledger.balance`) follows this account, so
    /// its live-mode zero-projection keys on the same fact the per-account
    /// rows carry. `CredentialKeys::loaded` is deliberately NOT consulted: an
    /// account can trade on a plugin that signs with its own env while
    /// declaring nothing, and a real venue report is the honest evidence.
    pub fn active_ledger_venue_reported(&self) -> bool {
        self.accounts.active_ledger().venue_reported()
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

    /// Existence check only (#780 boundary hygiene): the IPC `account.freeze`
    /// path must refuse an UNKNOWN account before the verdict write, and the
    /// blunt mutator ([`Self::accounts_mut`]) is test-only now (#825: cfg
    /// test or the opt-in `test-support` feature, never default) —
    /// production validates through this narrow read of the same
    /// `AccountLedgers::require` gate.
    pub fn require_account(&self, id: &crate::account::AccountId) -> CoreResult<()> {
        self.accounts.require(id)
    }

    /// E28 `account.list` (§12.1): one [`crate::schema::AccountView`]
    /// per configured account, sorted by id (`AccountLedgers::account_list`
    /// order). Every money field is the account's OWN book — this view is
    /// how cross-account isolation is READ; `dayRealizedUsd` sums the
    /// account's own closes of the current UTC day.
    ///
    /// issue 787 + issue 749（live 无场外余额显示面）: a LIVE account whose
    /// book no VENUE has ever reported has no venue truth behind it, so the
    /// only numbers it can hold are leftovers (a dry-mode journal replayed at
    /// boot, a stale seed, a report from before the keys were pulled). Reading
    /// those as "the balance" shows dry seed money in a live panel — the exact
    /// 1000 the operator reported. The projection states the truth instead: no
    /// venue report, ZERO. Dry and read-only keep the seed (their book is the
    /// money there is); a live account whose venue has reported keeps its book
    /// (the fact is set by `realign_venue_cash`).
    ///
    /// The gate is `Ledger::venue_reported`, NOT `CredentialKeys::loaded`
    /// (issue 749): a plugin can sign with its own env while the account
    /// declares nothing, so the credential fact both over- and under-reported.
    /// The live balance is the venue's word, so the venue's word is the gate.
    pub fn account_views(&self, now_ms: i64) -> Vec<crate::schema::AccountView> {
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
                // issue 749: the zero-fold asks "has a VENUE ever reported
                // this book's cash?", not "did the account declare credential
                // env names?". A plugin can sign with its own env (e.g. the
                // polymarket extension) while the account declares nothing, so
                // `credential_keys.loaded` was the wrong proxy: it hid real,
                // venue-reported money from the panel forever. A live book
                // with no venue report yet is still folded — whatever sits
                // there is a replay or a seed, and must not cross the wire as
                // "the balance".
                let unfunded_live = live && !ledger.venue_reported();
                let (balance, available) = if unfunded_live {
                    (Decimal::ZERO, Decimal::ZERO)
                } else {
                    (ledger.balance(), ledger.available())
                };
                crate::schema::AccountView {
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
}
