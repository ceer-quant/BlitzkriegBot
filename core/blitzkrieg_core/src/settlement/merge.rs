//! Merge-claim receivables (#461): the collateral pipeline for complete
//! share-pairs — burned at intent time, cashed at on-chain confirmation.
//! Split verbatim from `settlement.rs` (pure code motion, no logic edits) —
//! the file-size ratchet (#780) does not rule content, it rules length.
//!
//! These methods sit on `impl SettlementBook` in a second impl block (same
//! crate); the private fields they touch (`merges`/`merged`/`journal`) are
//! crate-local, so access is unchanged.

use rust_decimal::Decimal;

use super::{JournalLine, MergeClaim, SettlementBook, backoff_ms};

impl SettlementBook {
    // ── Merge receivables (#461) ────────────────────────────────────────────

    /// A merge burned `pairs` complete share-pairs at intent time. The pairs
    /// are now on-chain collateral the wallet has NOT received yet: the claim
    /// goes into the receivable, and cash is credited only when the venue's
    /// merge transaction is confirmed. Durable via the journal, so a restart
    /// between the burn and the receipt never loses the $1/pair. `venue` is
    /// #647's routing attribution — the venue the burned pairs sit on, so the
    /// request drains to that venue's executor.
    pub fn note_merge_burned(
        &mut self,
        condition_id: &str,
        account_id: &str,
        venue: &str,
        neg_risk: bool,
        pairs: Decimal,
        now_ms: i64,
    ) -> String {
        let id = format!("{condition_id}#merge");
        let mut n = 1;
        while self.merges.contains_key(&id) || self.merged.contains(&id) {
            n += 1;
            let next = format!("{condition_id}#merge-{n}");
            if self.merges.contains_key(&next) || self.merged.contains(&next) {
                continue;
            }
            return self.note_merge_burned_inner(
                next,
                condition_id,
                account_id,
                venue,
                neg_risk,
                pairs,
                now_ms,
            );
        }
        self.note_merge_burned_inner(id, condition_id, account_id, venue, neg_risk, pairs, now_ms)
    }

    #[allow(clippy::too_many_arguments)]
    fn note_merge_burned_inner(
        &mut self,
        id: String,
        condition_id: &str,
        account_id: &str,
        venue: &str,
        neg_risk: bool,
        pairs: Decimal,
        now_ms: i64,
    ) -> String {
        let pairs_usd = pairs;
        self.merges.insert(
            id.clone(),
            MergeClaim {
                id: id.clone(),
                condition_id: condition_id.to_string(),
                account_id: account_id.to_string(),
                venue: venue.to_string(),
                neg_risk,
                pairs_usd,
                created_at_ms: now_ms,
                attempts: 0,
                next_attempt_ms: 0,
                manual: false,
                last_error: None,
                tx_hash: None,
                block_number: None,
            },
        );
        if let Some(j) = self.journal.as_ref() {
            j.append(&JournalLine::MergeBurned {
                at_ms: now_ms,
                claim_id: id.clone(),
                condition_id: condition_id.to_string(),
                account_id: account_id.to_string(),
                venue: venue.to_string(),
                neg_risk,
                pairs_usd,
            });
        }
        id
    }

    /// Merge collateral due but not yet in the wallet — rides the SAME
    /// receivable the audit's cash identity carries.
    pub fn merge_receivable_usd(&self) -> Decimal {
        self.merges
            .values()
            .fold(Decimal::ZERO, |a, m| a + m.pairs_usd)
    }

    /// ONE account's merge receivable (same principle as
    /// [`Self::receivable_usd_for`]).
    pub fn merge_receivable_usd_for(&self, account: &str) -> Decimal {
        self.merges
            .values()
            .filter(|m| m.account_id == account)
            .fold(Decimal::ZERO, |a, m| a + m.pairs_usd)
    }

    pub fn pending_merge_count(&self) -> usize {
        self.merges.len()
    }

    /// Merges due for an attempt. Dispatching stamps the backoff, so a lost
    /// result retries instead of spinning.
    pub fn take_merges(&mut self, now_ms: i64) -> Vec<blitzkrieg_market_api::MergeRequest> {
        let due: Vec<String> = self
            .merges
            .values()
            .filter(|m| !m.manual && m.next_attempt_ms <= now_ms)
            .map(|m| m.id.clone())
            .collect();
        let mut out = Vec::new();
        for id in due {
            if let Some(m) = self.merges.get_mut(&id) {
                m.attempts += 1;
                m.next_attempt_ms = now_ms + backoff_ms(m.attempts);
                out.push(m.request());
            }
        }
        out
    }

    /// #647: the per-venue drain — every due merge whose request names
    /// `venue`, plus the UNTAGGED (`""`-venue) merges when `untagged_fallback`
    /// (the requested plugin is the host's active one). Same dispatch
    /// semantics as [`Self::take_merges`]; a sibling executor never sees a
    /// merge this drain handed out.
    pub fn take_merges_for_venue(
        &mut self,
        venue: &str,
        untagged_fallback: bool,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::MergeRequest> {
        let due: Vec<String> = self
            .merges
            .values()
            .filter(|m| {
                !m.manual
                    && m.next_attempt_ms <= now_ms
                    && (m.venue == venue || (untagged_fallback && m.venue.is_empty()))
            })
            .map(|m| m.id.clone())
            .collect();
        let mut out = Vec::new();
        for id in due {
            if let Some(m) = self.merges.get_mut(&id) {
                m.attempts += 1;
                m.next_attempt_ms = now_ms + backoff_ms(m.attempts);
                out.push(m.request());
            }
        }
        out
    }

    /// A merge attempt failed. Same retry vocabulary as a redemption.
    pub fn note_merge_failure(
        &mut self,
        id: &str,
        failure: &blitzkrieg_market_api::RedemptionFailure,
        now_ms: i64,
    ) -> bool {
        let Some(m) = self.merges.get_mut(id) else {
            return false;
        };
        m.last_error = Some(failure.message.clone());
        m.manual = failure.manual;
        if failure.manual {
            m.next_attempt_ms = i64::MAX;
        } else {
            m.next_attempt_ms = now_ms + backoff_ms(m.attempts);
        }
        true
    }

    /// A merge landed: the claim leaves the receivable. Returns
    /// `(account_id, usd)` — the collateral that must be credited (once).
    pub fn note_merge_confirmed(
        &mut self,
        id: &str,
        tx_hash: Option<String>,
        block_number: Option<u64>,
        now_ms: i64,
    ) -> Option<(String, Decimal)> {
        let m = self.merges.remove(id)?;
        self.merged.insert(id.to_string());
        if let Some(j) = self.journal.as_ref() {
            j.append(&JournalLine::MergeConfirmed {
                at_ms: now_ms,
                claim_id: id.to_string(),
                tx_hash,
                block_number,
            });
        }
        Some((m.account_id, m.pairs_usd))
    }
}
