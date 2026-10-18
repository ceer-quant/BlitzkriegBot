//! E-VA: strategy-level VIRTUAL accounts — a risk-layer quota book on the
//! `(account_id, strategy)` axis, fully separate from the real cash ledger.
//!
//! Why it exists: the REAL ledger's reservation book (`Ledger::reserve`, E28)
//! is per-ACCOUNT cash — one strategy's big entry commits real money and a
//! second strategy on the same account can find the spendable balance gone.
//! Both strategies were promised their own budget, but the promise has no
//! book to live in. This module is that book: each `(account, strategy)` pot
//! holds its strategy's outstanding VIRTUAL reservations and enforces its own
//! configured cap, so strategy A's consumption never moves strategy B's
//! headroom — at the risk layer, without touching a single cash number.
//!
//! Invariants (the same posture `Ledger` holds at line 14 of ledger.rs):
//!  - reserve/release NEVER touch `balance` — they only move numbers inside
//!    this book. No journal, no event, no cash fact.
//!  - A reservation is keyed by the order's `internal_key` (the intent
//!    identity the OME already dedups on, stable BEFORE `place()` mints the
//!    order id), so every terminal transition of that order finds exactly the
//!    reservation it owns — and releases it by the precise amount the state
//!    machine says was consumed, not the order's face value.
//!  - Atomicity: `reserve_batch` is check-then-hold for a WHOLE candidate
//!    batch under one `&mut self` borrow (the kernel's single memory
//!    transaction — `Core` has no interior mutability across these paths).
//!    Any leg over the pot's cap refuses the WHOLE batch and mutates
//!    nothing: pair legs go out together or not at all.
//!
//! Default-off: an `VirtualAccounts` with no configured cap answers `Ok` to
//! everything and stays empty on a core that never opts in — byte-identical
//! bypass for undeclared strategies (the replay-baseline posture).

use rust_decimal::Decimal;
use std::collections::HashMap;

/// One strategy's virtual quota for one account: the cap plus the
/// reservations currently outstanding against it.
#[derive(Debug, Default, Clone)]
struct VirtualPot {
    /// Outstanding virtual reservations, keyed by internal_key (one entry per
    /// reserved intent — a pair batch contributes one entry per leg).
    reservations: HashMap<String, Decimal>,
    /// Sum of the reservation values (maintained so reads are O(1) and the
    /// map can never disagree with the total mid-batch).
    outstanding: Decimal,
}

impl VirtualPot {
    fn used(&self) -> Decimal {
        self.outstanding
    }

    fn reserve(&mut self, key: &str, amount: Decimal) {
        if amount <= Decimal::ZERO {
            return;
        }
        self.reservations.insert(key.to_string(), amount);
        self.outstanding += amount;
    }

    /// Release EXACTLY `amount` of the reservation `key` holds (the state
    /// machine's consumed share), dropping the entry when it empties. A key
    /// with no reservation — already released, never reserved (SELL legs),
    /// or an undeclared bypass — releases nothing: the pot cannot go
    /// negative on a book it does not know.
    fn release(&mut self, key: &str, amount: Decimal) {
        if amount <= Decimal::ZERO {
            return;
        }
        if let Some(held) = self.reservations.get_mut(key) {
            let taken = (*held).min(amount);
            *held -= taken;
            self.outstanding -= taken;
            if *held <= Decimal::ZERO {
                self.reservations.remove(key);
            }
        }
    }

    /// Drop whatever `key` still holds (a terminal order's full remaining
    /// reservation — the cancel/expire/reject path).
    fn release_all(&mut self, key: &str) {
        if let Some(held) = self.reservations.remove(key) {
            self.outstanding -= held;
        }
    }
}

/// The strategy-virtual-account book. Lives on `Core` next to the account
/// book; every call is a plain `&mut self`/`&self` method — the lock boundary
/// is the kernel's own single-threaded order path (check-then-hold inside one
/// borrow, no await point between).
#[derive(Debug, Default, Clone)]
pub struct VirtualAccounts {
    /// `(account_id, strategy) → pot`. A pot is created only when a
    /// reservation is actually held or a cap is configured; a core that never
    /// opts in allocates nothing.
    pots: HashMap<(String, String), VirtualPot>,
    /// `(account_id, strategy) → cap`. Read from `CoreConfig::virtual_limits`
    /// at construction; a missing entry = no virtual quota (always permits).
    caps: HashMap<(String, String), Decimal>,
}

impl VirtualAccounts {
    /// Build the book from the configured caps. The caps are COPIED so a later
    /// `CoreConfig` mutation cannot move the quota mid-session — the same
    /// posture `RiskGate` takes toward its snapshot config.
    pub fn new(caps: &HashMap<String, crate::service::VirtualAccountLimit>) -> Self {
        let mut pots = HashMap::new();
        let mut configured = HashMap::new();
        for (strategy, limit) in caps {
            if let Some(cap) = limit.max_batch_notional_usd {
                for account in &limit.accounts {
                    let key = (account.clone(), strategy.clone());
                    configured.insert(key.clone(), cap);
                    pots.insert(key, VirtualPot::default());
                }
            }
        }
        Self {
            pots,
            caps: configured,
        }
    }

    /// Whether THIS (account, strategy) pot has a configured cap at all. The
    /// single default-off gate every call site reads: `false` = the virtual
    /// layer is transparent (no reservation, no check, no release).
    pub fn governed(&self, account_id: &str, strategy: &str) -> bool {
        self.caps
            .contains_key(&(account_id.to_string(), strategy.to_string()))
    }

    fn pot_mut(&mut self, account_id: &str, strategy: &str) -> &mut VirtualPot {
        let key = (account_id.to_string(), strategy.to_string());
        self.pots.entry(key).or_default()
    }

    /// The pot's outstanding virtual reservation (0 when unconfigured — the
    /// readout must not invent occupancy the book does not hold).
    pub fn used(&self, account_id: &str, strategy: &str) -> Decimal {
        self.pots
            .get(&(account_id.to_string(), strategy.to_string()))
            .map(|p| p.used())
            .unwrap_or(Decimal::ZERO)
    }

    /// The configured cap (`None` when this scope is not governed — "no cap"
    /// and "no quota" must stay distinguishable on the readout).
    pub fn cap(&self, account_id: &str, strategy: &str) -> Option<Decimal> {
        self.caps
            .get(&(account_id.to_string(), strategy.to_string()))
            .copied()
    }

    /// ATOMIC batch reservation: try the WHOLE batch against the SAME pot,
    /// commit all only when everything fits. `committed_elsewhere` is the
    /// strategy's occupancy that lives OUTSIDE this book (open positions'
    /// cost basis — filled legs converted to real holdings); resting BUY
    /// orders are NOT passed here, because their commitment IS their batch
    /// reservation (counting both would double-charge the pot). Returns
    /// `Err` with the over-cap detail when the batch does not fit; nothing
    /// is mutated in that case — pair legs go out together or not at all.
    pub fn reserve_batch(
        &mut self,
        account_id: &str,
        strategy: &str,
        legs: &[(String, Decimal)],
        committed_elsewhere: Decimal,
    ) -> Result<(), String> {
        // Unconfigured scopes stay TRANSPARENT end to end: no check, and no
        // pot created either (reserving into an unconfigured scope would
        // mint occupancy the readout must never report).
        if !self.governed(account_id, strategy) {
            return Ok(());
        }
        self.batch_fits(account_id, strategy, legs, committed_elsewhere)?;
        let pot = self.pot_mut(account_id, strategy);
        for (key, amount) in legs {
            pot.reserve(key, *amount);
        }
        Ok(())
    }

    /// The DRY-RUN twin of [`Self::reserve_batch`]: the same arithmetic,
    /// mutates NOTHING — the pre-check seam the submit pipeline reads before
    /// the per-order commit holds each leg (`virtual_batch_fits`). Splitting
    /// check from hold is what keeps the pot single-counted: the check sees
    /// the batch, only the commit ever reserves it.
    pub fn batch_fits(
        &self,
        account_id: &str,
        strategy: &str,
        legs: &[(String, Decimal)],
        committed_elsewhere: Decimal,
    ) -> Result<(), String> {
        let Some(&cap) = self
            .caps
            .get(&(account_id.to_string(), strategy.to_string()))
        else {
            return Ok(()); // unconfigured: transparent pass-through
        };
        let used = self
            .pots
            .get(&(account_id.to_string(), strategy.to_string()))
            .map(|p| p.used())
            .unwrap_or(Decimal::ZERO);
        let total: Decimal = legs.iter().map(|(_, a)| (*a).max(Decimal::ZERO)).sum();
        if used + committed_elsewhere + total > cap {
            return Err(format!(
                "virtual account cap ({used}+{committed}+{total} > {cap})",
                used = used,
                committed = committed_elsewhere
            ));
        }
        Ok(())
    }

    /// Precise partial release: drop exactly `amount` from `key`'s
    /// reservation — the fill-conversion path (a fill consumes the shares it
    /// executed, the rest keeps resting).
    pub fn release(&mut self, account_id: &str, strategy: &str, key: &str, amount: Decimal) {
        if !self.governed(account_id, strategy) {
            return;
        }
        self.pot_mut(account_id, strategy).release(key, amount);
    }

    /// Full release of whatever `key` still holds — the terminal transition
    /// path (cancelled / rejected / expired / force-closed orders).
    pub fn release_all(&mut self, account_id: &str, strategy: &str, key: &str) {
        if !self.governed(account_id, strategy) {
            return;
        }
        self.pot_mut(account_id, strategy).release_all(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::collections::HashMap;

    fn cap_for(strategy: &str, account: &str, cap: Decimal) -> VirtualAccounts {
        let mut limits = HashMap::new();
        limits.insert(
            strategy.to_string(),
            crate::service::VirtualAccountLimit {
                max_batch_notional_usd: Some(cap),
                accounts: vec![account.to_string()],
            },
        );
        VirtualAccounts::new(&limits)
    }

    #[test]
    fn unconfigured_scopes_are_transparent() {
        let mut v = cap_for("s", "acct", dec!(10));
        assert!(!v.governed("acct", "other"));
        assert!(
            v.reserve_batch("acct", "other", &[("k1".into(), dec!(999))], Decimal::ZERO)
                .is_ok()
        );
        v.release("acct", "other", "k1", dec!(999));
        v.release_all("acct", "other", "k1");
        assert_eq!(v.used("acct", "other"), Decimal::ZERO);
        assert_eq!(v.cap("acct", "other"), None);
    }

    #[test]
    fn batch_is_all_or_nothing() {
        let mut v = cap_for("s", "acct", dec!(10));
        // Leg 1 fits alone; the batch must still refuse because the SUM does
        // not fit, and NOTHING may be held afterwards.
        assert!(
            v.reserve_batch(
                "acct",
                "s",
                &[("a".into(), dec!(6)), ("b".into(), dec!(5))],
                Decimal::ZERO
            )
            .is_err()
        );
        assert_eq!(v.used("acct", "s"), Decimal::ZERO, "no leg may be held");
        assert!(
            v.reserve_batch(
                "acct",
                "s",
                &[("a".into(), dec!(6)), ("b".into(), dec!(4))],
                Decimal::ZERO
            )
            .is_ok()
        );
        assert_eq!(v.used("acct", "s"), dec!(10));
        // Occupancy converted to positions counts against the pot too — a
        // filled pair must consume quota exactly like a resting one.
        assert!(
            v.reserve_batch("acct", "s", &[("c".into(), dec!(1))], dec!(1))
                .is_err()
        );
    }

    #[test]
    fn release_is_precise_per_leg() {
        let mut v = cap_for("s", "acct", dec!(10));
        v.reserve_batch(
            "acct",
            "s",
            &[("a".into(), dec!(6)), ("b".into(), dec!(4))],
            Decimal::ZERO,
        )
        .unwrap();
        // Fill consumes 2 of leg a's 6: exactly 2 leaves the pot.
        v.release("acct", "s", "a", dec!(2));
        assert_eq!(v.used("acct", "s"), dec!(8));
        // Terminal release drops the rest of a's reservation.
        v.release_all("acct", "s", "a");
        assert_eq!(v.used("acct", "s"), dec!(4));
        // A SELL leg (never reserved) and a double release are no-ops.
        v.release_all("acct", "s", "never-reserved");
        v.release_all("acct", "s", "b");
        assert_eq!(v.used("acct", "s"), Decimal::ZERO);
    }

    #[test]
    fn pots_are_per_strategy_and_per_account() {
        let mut limits = HashMap::new();
        limits.insert(
            "s".to_string(),
            crate::service::VirtualAccountLimit {
                max_batch_notional_usd: Some(dec!(10)),
                accounts: vec!["acct".to_string()],
            },
        );
        let mut v = VirtualAccounts::new(&limits);
        v.reserve_batch("acct", "s", &[("a".into(), dec!(8))], Decimal::ZERO)
            .unwrap();
        // Another strategy on the SAME account keeps its own headroom.
        assert!(v.governed("acct", "s"));
        assert!(
            !v.governed("acct", "other"),
            "undeclared strategy is ungoverned"
        );
        // Another account under the same strategy cap is not governed either
        // (the cap names its accounts explicitly).
        assert!(!v.governed("acct2", "s"));
    }
}
