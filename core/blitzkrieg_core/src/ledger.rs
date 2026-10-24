//! Inventory ledger — local USDC reservation/release so the core never places
//! orders whose notional exceeds available collateral. Balances are periodically
//! reconciled against the venue (set_balance); P0 enforces the local gate, the
//! on-chain/REST reconciliation loop lands with P1.
//!
//! Every cash movement also lands in an append-only JOURNAL (the audit trail):
//! one [`JournalEntry`] per change, `amount` = the actual signed delta
//! (post-clamp), `balance_after` = the resulting balance. The balance itself is
//! still maintained incrementally — O(1), never a traversal — the journal is
//! the trail that makes the number explainable and boot-replayable. The
//! invariant the tests pin: `balance == Σ journal.amount` for a trail with
//! nothing evicted, every retained row chains (`row[i].balance_after` is what
//! `row[i+1]` moved from), and the LAST row's `balance_after` is always the
//! live balance. Reserve/release never touch `balance` and never journal.

use crate::model::{CoreError, CoreErrorCode, CoreResult, OrderId};
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

/// The cash-movement kinds, one per place the balance actually changes.
/// Wire/file spelling is `snake_case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalKind {
    /// Boot/seed placement (`dry_seed_balance`).
    Seed,
    /// A BUY fill's cash cost (or its rollback correction — the sign tells).
    BuyFill,
    /// A SELL fill's proceeds net of its venue fee (one cash event at the
    /// venue; the fee is inside the amount).
    SellFill,
    /// An entry-side cash fee (`charge_fee`).
    Fee,
    /// MERGE collateral confirmed on-chain ($1.00 per pair).
    Merge,
    /// A settled position's redemption payout confirmed on-chain.
    Redemption,
    /// A wallet-level venue cash row (MAKER_REBATE / REWARD, #388).
    Cashflow,
    /// The venue-reported balance realigned the local book (live reconcile).
    Realign,
    /// `system.reset_trading` re-seeded the book (the trail keeps running —
    /// append-only evidence, never a truncation).
    Reset,
}

/// One cash movement: the audit-trail row and the boot-replay unit. Money is
/// serialized through `crate::decimal` (strings on the wire/file).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    pub at_ms: i64,
    pub kind: JournalKind,
    #[serde(with = "crate::decimal")]
    pub amount: Decimal,
    #[serde(with = "crate::decimal")]
    pub balance_after: Decimal,
    /// What moved the money: order id for fills, claim id for
    /// redemption/merge, `rebate`/`reward` for cashflow, `boot`/`reset` for
    /// the seed family, `venue` for a realign. Free text — never parsed.
    #[serde(rename = "ref")]
    pub ref_: String,
}

/// In-memory journal window per account. The FILE is the durable record; this
/// cap only bounds memory — eviction never rewrites the file.
const JOURNAL_CAP: usize = 10_000; // ponytail: fixed window, not unbounded — replay reads the file, not this

#[derive(Debug, Clone, Default)]
pub struct Ledger {
    /// Last venue-reported cash balance (gross, before local reservations).
    balance: Decimal,
    /// Notional reserved per resting BUY order.
    reservations: HashMap<OrderId, Decimal>,
    /// The cash-movement audit trail, oldest → newest, capped at
    /// [`JOURNAL_CAP`]. Rows also append to [`Self::journal_path`] when the
    /// deployment persists one.
    journal: VecDeque<JournalEntry>,
    /// Where journal rows append (`data/ledger/<account>.journal.jsonl`).
    /// `None` = memory-only (backtester, in-process tests): nothing is
    /// written or replayed.
    journal_path: Option<PathBuf>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the durable journal file. Boot-replay only — a ledger that
    /// already holds rows keeps them (the replay path fills this itself).
    pub(crate) fn set_journal_path(&mut self, path: PathBuf) {
        self.journal_path = Some(path);
    }

    /// Boot replay: adopt rows loaded from the durable file and restore the
    /// opening balance from the LAST row's `balance_after` — O(1), never a
    /// summing traversal. Only the newest [`JOURNAL_CAP`] rows stay in memory.
    pub(crate) fn replay_journal(&mut self, rows: Vec<JournalEntry>) {
        self.journal.clear();
        for row in rows {
            if self.journal.len() == JOURNAL_CAP {
                self.journal.pop_front();
            }
            self.journal.push_back(row);
        }
        if let Some(last) = self.journal.back() {
            self.balance = last.balance_after;
        }
    }

    /// The ONE entry every balance change goes through. `requested` is the
    /// arithmetic delta; the recorded `amount` is the ACTUAL delta after the
    /// zero-floor clamp, so `Σ amounts == balance` stays exact even where a
    /// fill costs more than the book holds.
    fn apply(&mut self, kind: JournalKind, requested: Decimal, ref_: &str) -> Decimal {
        let new_balance = (self.balance + requested).max(Decimal::ZERO);
        let amount = new_balance - self.balance;
        self.balance = new_balance;
        if amount != Decimal::ZERO {
            let entry = JournalEntry {
                at_ms: now_ms(),
                kind,
                amount,
                balance_after: self.balance,
                ref_: ref_.to_string(),
            };
            if let Some(path) = self.journal_path.clone() {
                crate::jsonl::append(&path, &entry);
            }
            if self.journal.len() == JOURNAL_CAP {
                self.journal.pop_front();
            }
            self.journal.push_back(entry);
        }
        amount
    }

    /// Read-only audit view, oldest → newest (at most the retained window).
    pub fn journal_snapshot(&self) -> Vec<JournalEntry> {
        self.journal.iter().cloned().collect()
    }

    /// Seed/refresh the book at boot (`seed_all`). kind = `Seed`.
    pub fn seed(&mut self, balance: Decimal) {
        let target = balance.max(Decimal::ZERO);
        self.apply(JournalKind::Seed, target - self.balance, "boot");
    }

    /// Fund a runtime-minted account (`account.create` on a locally-settling
    /// core). kind = `Seed` — a boot fact at heart — but the journal ref
    /// spells the minting verb, so the trail distinguishes "boot funded this"
    /// from "the operator minted the account later and it got the same seed".
    /// A zero amount records nothing (a zero seed on a zero book is a no-op).
    pub fn fund(&mut self, amount: Decimal, ref_: &str) {
        self.apply(JournalKind::Seed, amount.max(Decimal::ZERO), ref_);
    }

    pub fn balance(&self) -> Decimal {
        self.balance
    }

    pub fn reserved(&self) -> Decimal {
        self.reservations
            .values()
            .copied()
            .fold(Decimal::ZERO, |a, b| a + b)
    }

    /// Collateral free to commit to new BUY orders.
    pub fn available(&self) -> Decimal {
        (self.balance - self.reserved()).max(Decimal::ZERO)
    }

    /// Reserve notional for a resting BUY (price * size). Idempotent per order.
    pub fn reserve(&mut self, order_id: &str, notional: Decimal) -> CoreResult<()> {
        if notional < Decimal::ZERO {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "negative notional",
            ));
        }
        if self.reservations.contains_key(order_id) {
            return Ok(());
        }
        if notional > self.available() {
            return Err(CoreError::new(
                CoreErrorCode::InsufficientFunds,
                format!(
                    "reserve {notional} exceeds available {} (balance {}, reserved {})",
                    self.available(),
                    self.balance,
                    self.reserved()
                ),
            ));
        }
        self.reservations.insert(order_id.to_string(), notional);
        Ok(())
    }

    /// Release a BUY reservation on cancel/reject/expiry.
    pub fn release(&mut self, order_id: &str) -> Decimal {
        self.reservations.remove(order_id).unwrap_or(Decimal::ZERO)
    }

    /// Realign the book to the venue's last report — kind = `Realign` when the
    /// number moved, a silent no-op when it did not (the periodic free-cash
    /// sync would otherwise flood the trail with zero rows). Negative targets
    /// clamp to zero, unchanged.
    pub fn set_balance(&mut self, balance: Decimal) {
        let target = balance.max(Decimal::ZERO);
        self.apply(JournalKind::Realign, target - self.balance, "venue");
    }

    /// Set a BUY order's outstanding commitment to exactly `remaining_notional`
    /// (= `limit_price * (size - filled_size)`, zero when nothing is left).
    ///
    /// Unlike [`Ledger::reserve`] this is a SET, not a gated add, and it is the
    /// single mechanism that keeps the reservation table equal to the order's
    /// real outstanding commitment (issue #181):
    ///  - after every applied fill (shrinks it by the filled notional, so a
    ///    price-improved fill leaves no residue behind);
    ///  - on the terminal transition (zero, driving the entry out of the table);
    ///  - at startup for orders re-adopted from the order log, where the local
    ///    table was just rebuilt from disk and must not start empty while real
    ///    orders rest at the venue.
    ///
    /// It never refuses: a refusal would silently UNDER-reserve and let the next
    /// entry over-commit, which is the failure this exists to prevent. The
    /// accounting audit reports the over-commit if the target exceeds what the
    /// local cash can cover. Returns the previous value.
    pub fn sync_buy_reservation(&mut self, order_id: &str, remaining_notional: Decimal) -> Decimal {
        let target = remaining_notional.max(Decimal::ZERO);
        let prev = self.reservations.remove(order_id).unwrap_or(Decimal::ZERO);
        if target > Decimal::ZERO {
            self.reservations.insert(order_id.to_string(), target);
        }
        prev
    }

    /// Settle a BUY fill EXACTLY: release the notional the filled shares had
    /// reserved and spend the cash they actually cost.
    ///
    /// The two are not the same number. A resting BUY reserved
    /// `limit_price * size` up front, so `filled` shares release
    /// `limit_price * filled` — while the cash that leaves is the EXECUTION price
    /// actually paid. Under the old `min(reserved, cost)` draw the difference
    /// (a price improvement of `(limit − execution) * filled`) stayed in the
    /// table forever once the order reached `Filled`: no cancel/reject path runs
    /// for a filled order, so nothing ever released it and `available()` bled
    /// down for the rest of the session (issue #181).
    ///
    /// The release is clamped to what is actually held, so a fill for an order
    /// with no local reservation (an order re-adopted from the venue, or a SELL
    /// rollback passing zero) is harmless. Returns the notional released.
    pub fn settle_buy_fill(
        &mut self,
        order_id: &str,
        cost_usd: Decimal,
        reserved_released: Decimal,
    ) -> Decimal {
        let held = self
            .reservations
            .get(order_id)
            .copied()
            .unwrap_or(Decimal::ZERO);
        let draw = reserved_released.max(Decimal::ZERO).min(held);
        if draw > Decimal::ZERO {
            let left = held - draw;
            if left > Decimal::ZERO {
                self.reservations.insert(order_id.to_string(), left);
            } else {
                self.reservations.remove(order_id);
            }
        } else if reserved_released > held {
            // Either the order was never reserved locally (a venue-only order)
            // or the reservation table drifted. Say so: the audit compares the
            // table against the OME's live BUY commitments.
            tracing::debug!(
                order = %order_id,
                requested = %reserved_released,
                held = %held,
                "buy settlement released more than was reserved"
            );
        }
        // One journal row for the whole fill: the cash that actually left, net
        // of nothing (the entry fee arrives separately through `charge_fee`).
        self.apply(JournalKind::BuyFill, -cost_usd, order_id);
        draw
    }

    /// The cash identity `available + reserved == balance` (issue #181). It
    /// fails exactly when reserved exceeds balance, i.e. when local commitments
    /// no longer fit under the last venue-reported balance — which is also what
    /// [`Ledger::unfunded_reserved`] measures.
    pub fn is_balanced(&self) -> bool {
        self.available() + self.reserved() == self.balance
    }

    /// Reserved notional the local balance does not cover (zero when balanced).
    pub fn unfunded_reserved(&self) -> Decimal {
        (self.reserved() - self.balance).max(Decimal::ZERO)
    }

    /// Every live reservation, sorted by order id — the audit's view of what the
    /// ledger thinks is outstanding.
    pub fn reservations_snapshot(&self) -> Vec<(OrderId, Decimal)> {
        let mut v: Vec<(OrderId, Decimal)> = self
            .reservations
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Settle a SELL fill: add proceeds, minus the venue trading fee charged
    /// on the way out. Without the fee the cash ledger credits the full
    /// notional and the cash view drifts above the realized-net view the trade
    /// records use. One journal row (kind = `SellFill`): the venue pays the
    /// notional and takes its fee in the same cash event, so the row's
    /// `amount` is the net.
    pub fn settle_sell_fill(&mut self, proceeds: Decimal, fee_usd: Decimal) {
        self.apply(JournalKind::SellFill, proceeds - fee_usd, "sell");
    }

    /// Charge an entry-side cash fee (buy fills pay it on top of the cost).
    /// kind = `Fee`.
    pub fn charge_fee(&mut self, fee_usd: Decimal) {
        self.apply(JournalKind::Fee, -fee_usd, "entry fee");
    }

    /// Credit collateral redeemed on-chain (issue #175): the payout of a settled
    /// position moving out of the receivable and into spendable cash. Called
    /// exactly once per claim — the settlement book's durable journal is the
    /// idempotency guard, so a replayed redemption result cannot credit twice.
    /// kind = `Redemption`, `ref` = the claim id.
    pub fn credit_redemption(&mut self, payout_usd: Decimal, claim_id: &str) {
        if payout_usd <= Decimal::ZERO {
            return;
        }
        self.apply(JournalKind::Redemption, payout_usd, claim_id);
    }

    /// Credit the collateral of MERGED share-pairs (task 2.2): one complete
    /// UP+DOWN pair of a condition merges on-chain into $1.00 of USDC — the
    /// collection channel of pair-discount arbitrage (@almach: 12,570 merges,
    /// zero sells). `pairs` counts share-pairs; each credits $1.00. Idempotence
    /// lives upstream: the merged shares leave the position book in the same
    /// transaction, so a replayed merge finds no pair to burn. kind = `Merge`,
    /// `ref` = the claim id.
    pub fn credit_merge(&mut self, pairs: Decimal, claim_id: &str) {
        if pairs <= Decimal::ZERO {
            return;
        }
        self.apply(JournalKind::Merge, pairs, claim_id);
    }

    /// #388: credit one wallet-level venue cash payment (a MAKER_REBATE or
    /// REWARD ledger row) — cash that arrived OUTSIDE any fill, redemption or
    /// merge. It rides the archive stream at the row's own timestamp and
    /// credits the active account exactly once per row. A zero/negative row
    /// is a no-op: the venue does not claw cash back through these rows, and
    /// a phantom debit would corrupt the cash identity the audit reconciles.
    /// kind = `Cashflow`, `ref` = `rebate` / `reward`.
    pub fn credit_cashflow(&mut self, amount_usd: Decimal, which: &str) {
        if amount_usd <= Decimal::ZERO {
            return;
        }
        self.apply(JournalKind::Cashflow, amount_usd, which);
    }

    /// `system.reset_trading` re-seed (or a deployer's explicit re-seed): the
    /// balance returns to the configured seed and the trail records the gap as
    /// one `Reset` row — append-only evidence, never a truncation (a Live core
    /// refuses a reset outright; a Dry/ReadOnly wipe leaves the audit trail).
    /// kind = `Reset`, `ref` = `reset`.
    pub fn reset_to_seed(&mut self, seed: Decimal) {
        let target = seed.max(Decimal::ZERO);
        self.apply(JournalKind::Reset, target - self.balance, "reset");
    }

    /// issue 561 账户维重置原语：把这本账簿「翻新」为一次刚落地的重置 ——
    /// 余额回种子、BUY 预留表清空、cash journal 收缩为恰好一行 `Reset` 行
    /// （balanceAfter = 种子）。三条理由钉死这个语义（缺一不可）：
    ///
    /// 1. boot 回放以「末行 balance_after」恢复开局余额，且任意账户有
    ///    journal 就整表跳过 seed_all —— 留下旧流水（即使末行是 Reset 行）
    ///    意味着每次重启都把 pre-reset 的全部成交流水带回内存 trail，操作
    ///    者在 account.journal 里还能读到已被重置抹掉的交易；重写成单行后
    ///    回放 == 种子，与 ResetTradingResult 自检断言逐字一致。
    /// 2. 不能截成空文件：同簿多账户部署里其他账户仍有 journal，
    ///    `any_journal_on_disk` 依旧为真，被清账户重启时既不回放也不
    ///    seed，余额落到 0。单行 Reset 行让该账户在任何 boot 组合下都
    ///    自恢复到种子。
    /// 3. Reset 行（不是无行）保留「环境被重置」这一审计事实：append-only
    ///    语义在账户维收缩为「重写为重置时刻的单行快照」，而非抹除痕迹。
    ///
    /// 返回 false 仅当磁盘重写失败（内存态照常翻新 —— 内存是权威，调用方
    /// 决定是否告警），与 `jsonl::rewrite_atomic` 的契约一致。
    pub fn reset_account_to_seed(&mut self, seed: Decimal) -> bool {
        let target = seed.max(Decimal::ZERO);
        self.reservations.clear();
        self.balance = target;
        self.journal.clear();
        let entry = JournalEntry {
            at_ms: now_ms(),
            kind: JournalKind::Reset,
            amount: target,
            balance_after: target,
            ref_: "reset".to_string(),
        };
        if let Some(path) = self.journal_path.clone() {
            match serde_json::to_vec(&entry) {
                Ok(mut bytes) => {
                    bytes.push(b'\n');
                    if !crate::jsonl::rewrite_atomic(&path, &bytes) {
                        tracing::warn!(
                            path = %path.display(),
                            "issue 561: account journal rewrite failed — \
                             the on-disk trail keeps pre-reset rows; the next \
                             boot replay would restore them"
                        );
                        self.journal.push_back(entry);
                        return false;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "issue 561: account journal row serialize failed");
                    self.journal.push_back(entry);
                    return false;
                }
            }
        }
        self.journal.push_back(entry);
        true
    }

    /// 账户退役（`account.remove`）的 journal 收尾：删掉该账户的磁盘流水
    /// 文件并清空内存 trail。为什么删文件而不是留 tombstone —— 同名 id 的
    /// 下一次 `account.create` 是一个**新身份的新账本**，不是老账户的续弦；
    /// 留着旧文件，boot 的 `attach_journals` 会把上一世的成交流水整表回放
    /// 进新账本（生产 kalshi 实锤：6 次 建户→删户 循环叠出 6 笔幽灵
    /// `seed@account.create`，Σamount 比真实余额多出整整 5000 —— 流水卡
    /// 与任何账户都对不上的根因）。内存 trail 同步清空：账户都没了，trail
    /// 也不该再占窗口。文件不存在（本就无 journal 的账户）是合法 no-op。
    /// 返回 false 仅当删除失败（调用方决定是否告警）—— 删除失败比重写失
    /// 败更糟（旧文件会复活），所以调用方在 false 时拒绝整个 remove。
    pub fn retire_journal_file(&mut self) -> bool {
        self.journal.clear();
        match self.journal_path.clone() {
            Some(path) => match std::fs::remove_file(&path) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "account remove: journal file delete failed — the \
                         stale trail would resurrect at the next boot"
                    );
                    false
                }
            },
            None => true,
        }
    }
}

/// Market-agnostic reservation lifecycle (see `ledger_api`). The core pipeline
/// can talk to this trait; prediction markets use the whole balance as the
/// single collateral asset.
impl crate::ledger_api::LedgerApi for Ledger {
    fn reserve_for_order(&mut self, id: &str, amount: Decimal) -> CoreResult<()> {
        self.reserve(id, amount)
    }
    fn release_reservation(&mut self, id: &str) -> Decimal {
        self.release(id)
    }
    fn settle_reservation(&mut self, id: &str, filled_notional: Decimal) {
        // Market-agnostic lifecycle: the caller states one number, so the
        // reserved notional and the cash cost coincide (no price improvement
        // information reaches this trait).
        self.settle_buy_fill(id, filled_notional, filled_notional);
    }
    fn available_for(&self, _asset: &crate::ledger_api::Asset) -> Decimal {
        // Single-collateral (USDC) model: the asset argument is informational.
        self.available()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn reservation_caps_overspend() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        l.reserve("o1", dec!(6)).unwrap();
        assert_eq!(l.available(), dec!(4));
        let err = l.reserve("o2", dec!(5)).unwrap_err();
        assert_eq!(err.code, CoreErrorCode::InsufficientFunds);
        // Cancel frees it, then a smaller order fits.
        assert_eq!(l.release("o1"), dec!(6));
        l.reserve("o2", dec!(5)).unwrap();
        assert_eq!(l.available(), dec!(5));
    }

    #[test]
    fn buy_fill_draws_reservation_and_cash() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        l.reserve("o1", dec!(4)).unwrap();
        l.settle_buy_fill("o1", dec!(4), dec!(4));
        assert_eq!(l.balance(), dec!(6));
        assert_eq!(l.reserved(), dec!(0));
        l.settle_sell_fill(dec!(7), Decimal::ZERO);
        assert_eq!(l.balance(), dec!(13));
    }

    #[test]
    fn fees_leave_the_cash_ledger() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        // Sell 10 notional with 1 fee: 10 + 10 − 1 = 19 cash.
        l.settle_sell_fill(dec!(10), dec!(1));
        assert_eq!(l.balance(), dec!(19));
        // Entry fee also comes out of cash.
        l.charge_fee(dec!(2));
        assert_eq!(l.balance(), dec!(17));
    }

    /// MERGE (task 2.2): one complete UP+DOWN pair merges into $1.00 of
    /// collateral — @almach's collection channel. Zero/negative pair counts are
    /// no-ops: no phantom collateral ever enters the book.
    #[test]
    fn merge_credits_one_dollar_per_pair() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        l.credit_merge(dec!(7), "cond#merge");
        assert_eq!(l.balance(), dec!(17));
        l.credit_merge(Decimal::ZERO, "cond#merge");
        l.credit_merge(dec!(-3), "cond#merge");
        assert_eq!(l.balance(), dec!(17));
    }

    /// #388: a wallet-level cash row (MAKER_REBATE / REWARD) credits the cash
    /// book exactly once per row, by the row's exact amount. Zero/negative
    /// rows are no-ops — the venue pays through these rows, never claws back,
    /// and a phantom debit would corrupt the cash identity.
    #[test]
    fn cashflow_credits_exactly_once_per_row() {
        let mut l = Ledger::new();
        l.set_balance(dec!(100000));
        l.credit_cashflow(dec!(331.2175), "rebate");
        assert_eq!(l.balance(), dec!(100331.2175), "one row, exact amount");
        l.credit_cashflow(dec!(659.995), "reward");
        assert_eq!(l.balance(), dec!(100991.2125), "the second row adds on");
        l.credit_cashflow(Decimal::ZERO, "rebate");
        l.credit_cashflow(dec!(-5), "reward");
        assert_eq!(l.balance(), dec!(100991.2125), "no phantom debit");
        assert!(l.is_balanced(), "the cash identity survives the credit");
    }

    // ── #181: reservation must reach zero on every path ──────────────────────

    /// Acceptance: a price-improved fill leaves NO residue. Resting BUY 10 @ 0.40
    /// reserves 4.00; the fill executes at 0.35, so 4.00 of reservation is
    /// released while only 3.50 of cash moves. The old `min(reserved, cost)` draw
    /// released just 3.50 and stranded 0.50 in the table — and since the order is
    /// now Filled, no cancel/reject ever released it.
    #[test]
    fn price_improved_fill_releases_the_full_reservation() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        l.reserve("o1", dec!(4)).unwrap();
        l.settle_buy_fill("o1", dec!(3.50), dec!(4));
        assert_eq!(l.reserved(), dec!(0), "no residue after a full fill");
        assert_eq!(l.balance(), dec!(6.50), "cash pays the execution price");
        assert!(l.is_balanced());
        assert_eq!(l.available(), dec!(6.50));
    }

    /// Acceptance: partial fill then cancel leaves nothing behind. Both the
    /// per-fill release (at the LIMIT price the notional was reserved at) and the
    /// cancel's `release` drive the entry to zero, and the identity holds
    /// throughout.
    #[test]
    fn partial_fill_then_cancel_leaves_no_residue() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        l.reserve("o1", dec!(4)).unwrap();
        assert!(l.is_balanced());
        // 4 of 10 shares fill at a better price.
        l.settle_buy_fill("o1", dec!(1.40), dec!(1.60));
        assert_eq!(l.reserved(), dec!(2.40), "remaining 6 shares stay reserved");
        assert!(l.is_balanced());
        // The kernel re-syncs to the true outstanding commitment...
        l.sync_buy_reservation("o1", dec!(2.40));
        assert_eq!(l.reserved(), dec!(2.40));
        // ...and the cancel retires the rest.
        l.release("o1");
        assert_eq!(l.reserved(), dec!(0));
        assert_eq!(l.balance(), dec!(8.60));
        assert!(l.is_balanced());
    }

    /// The identity is enforced by the reserve gate, not merely asserted: a
    /// commitment larger than the balance is refused rather than recorded.
    #[test]
    fn reserve_never_breaks_the_cash_identity() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        assert!(l.reserve("o1", dec!(11)).is_err());
        assert_eq!(l.reserved(), dec!(0));
        assert!(l.is_balanced());
        // A venue-reported balance drop below the outstanding commitments is the
        // one way the identity can break, and it is reported as such.
        l.reserve("o1", dec!(6)).unwrap();
        l.set_balance(dec!(5));
        assert!(!l.is_balanced());
        assert_eq!(l.unfunded_reserved(), dec!(1));
        assert_eq!(l.available(), Decimal::ZERO);
    }

    /// Sync is a set, not an add: it is idempotent, it can shrink a stale
    /// reservation, and zero removes the entry outright.
    #[test]
    fn sync_buy_reservation_sets_exactly() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        assert_eq!(l.sync_buy_reservation("o1", dec!(3)), Decimal::ZERO);
        assert_eq!(l.reserved(), dec!(3));
        assert_eq!(l.sync_buy_reservation("o1", dec!(3)), dec!(3));
        assert_eq!(l.reserved(), dec!(3), "idempotent");
        assert_eq!(l.sync_buy_reservation("o1", dec!(1)), dec!(3));
        assert_eq!(l.reserved(), dec!(1));
        assert_eq!(l.sync_buy_reservation("o1", Decimal::ZERO), dec!(1));
        assert_eq!(l.reserved(), Decimal::ZERO);
        assert!(l.reservations_snapshot().is_empty());
        // A negative target clamps to zero rather than crediting.
        l.sync_buy_reservation("o2", dec!(-5));
        assert_eq!(l.reserved(), Decimal::ZERO);
    }

    /// A fill for an order with no local reservation must not create one (the
    /// SELL rollback path passes zero, and a re-adopted venue order may have
    /// none): the release is clamped, never negative.
    #[test]
    fn settlement_never_releases_more_than_held() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        assert_eq!(l.settle_buy_fill("ghost", dec!(2), dec!(5)), Decimal::ZERO);
        assert_eq!(l.reserved(), Decimal::ZERO);
        assert_eq!(l.balance(), dec!(8));
        l.reserve("o1", dec!(4)).unwrap();
        assert_eq!(l.settle_buy_fill("o1", dec!(1), dec!(9)), dec!(4));
        assert_eq!(l.reserved(), Decimal::ZERO);
        assert!(l.is_balanced());
    }

    // ── Journal: the audit trail + the Σ invariant ──────────────────────────

    /// THE invariant, over every cash-moving path at once: with nothing
    /// evicted, `balance == Σ journal.amount`, every row chains to the next
    /// (`row[i].balance_after` is what `row[i+1]` moved from) and the last
    /// row's `balance_after` IS the live balance. Zero rows are never recorded
    /// (a no-op credit leaves no trail), reserve/release never journal.
    #[test]
    fn journal_invariant_holds_over_every_cash_path() {
        let mut l = Ledger::new();
        l.set_balance(dec!(100)); // realign from 0 → the Seed row records the seed
        // — actually seed through the seed path so the row kind is honest:
        let mut l = Ledger::new();
        l.seed(dec!(100));
        l.reserve("o1", dec!(40)).unwrap(); // no cash movement, no row
        assert_eq!(l.journal_snapshot().len(), 1, "reserve never journals");
        l.settle_buy_fill("o1", dec!(30), dec!(40)); // buy fill
        l.charge_fee(dec!(2)); // entry fee
        l.settle_sell_fill(dec!(45), dec!(3)); // sell fill net of fee
        l.credit_merge(dec!(5), "cond#merge");
        l.credit_redemption(dec!(7), "cond#1");
        l.credit_cashflow(dec!(1.5), "rebate");
        l.credit_cashflow(Decimal::ZERO, "reward"); // no row: zero is a no-op
        l.credit_cashflow(dec!(-1), "reward"); // no row: negative is a no-op
        l.set_balance(dec!(200)); // venue realign (+…)
        l.reset_to_seed(dec!(150)); // reset re-seed
        l.release("o1"); // no cash movement, no row
        // Zero-delta set_balance is a silent no-op (no flood of realign rows):
        l.set_balance(dec!(150));
        let rows = l.journal_snapshot();
        let sum = rows.iter().fold(Decimal::ZERO, |a, r| a + r.amount);
        assert_eq!(sum, l.balance(), "balance == Σ journal.amount");
        for pair in rows.windows(2) {
            assert_eq!(
                pair[0].balance_after,
                pair[1].amount + pair[0].balance_after - pair[1].amount,
                "chain sanity"
            );
            // The direct chain check: the next row's delta applied to the
            // previous row's balance_after lands exactly on its balance_after.
        }
        for w in rows.windows(2) {
            assert_eq!(
                w[0].balance_after + w[1].amount,
                w[1].balance_after,
                "row chains: prev.balance_after + amount == next.balance_after"
            );
        }
        assert_eq!(
            rows.last().map(|r| r.balance_after),
            Some(l.balance()),
            "the last row's balance_after IS the live balance"
        );
        // The kinds arrived in the order the calls made them.
        let kinds: Vec<JournalKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                JournalKind::Seed,
                JournalKind::BuyFill,
                JournalKind::Fee,
                JournalKind::SellFill,
                JournalKind::Merge,
                JournalKind::Redemption,
                JournalKind::Cashflow,
                JournalKind::Realign,
                JournalKind::Reset,
            ]
        );
        // The zero/negative rows really were skipped:
        assert_eq!(rows.len(), 9);
    }

    /// Clamped fills still satisfy the identity because the row records the
    /// ACTUAL delta (post-clamp), not the request: a fill that overdraws the
    /// book journals what really moved.
    #[test]
    fn journal_amounts_are_post_clamp_actual_deltas() {
        let mut l = Ledger::new();
        l.seed(dec!(10));
        // A fill costing 30 against a 10 balance: the book floors at zero and
        // the row says −10, not −30.
        l.settle_buy_fill("o1", dec!(30), Decimal::ZERO);
        let rows = l.journal_snapshot();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].amount, dec!(-10));
        assert_eq!(rows[1].balance_after, Decimal::ZERO);
        assert_eq!(rows[1].kind, JournalKind::BuyFill);
        assert_eq!(rows[1].ref_, "o1", "the order id is the fill's ref");
        let sum = rows.iter().fold(Decimal::ZERO, |a, r| a + r.amount);
        assert_eq!(sum, l.balance());
    }

    /// The durable file: every row appends through `jsonl::append`, and a
    /// fresh ledger replaying the file restores the last row's `balance_after`
    /// as its opening balance — O(1), never a summing traversal. (The test
    /// scratch lives in a temp dir beside the other file-backed tests.)
    #[test]
    fn journal_persists_and_replays_the_last_balance_after() {
        let dir = std::env::temp_dir().join(format!(
            "bk-ledger-journal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("default.journal.jsonl");
        let mut l = Ledger::new();
        l.set_journal_path(path.clone());
        l.seed(dec!(100));
        l.settle_buy_fill("o1", dec!(30), Decimal::ZERO);
        l.credit_redemption(dec!(7), "cond#1");
        l.charge_fee(dec!(1));
        drop(l);
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "one line per cash movement");
        let last: JournalEntry = serde_json::from_str(lines[3]).unwrap();
        assert_eq!(last.kind, JournalKind::Fee);
        assert_eq!(last.balance_after, dec!(76));

        // Replay: adopt the rows, restore the opening balance from the tail.
        let mut restored = Ledger::new();
        restored.replay_journal(crate::jsonl::load::<JournalEntry>(
            &path,
            "ledger journal: skipped unparseable lines",
        ));
        assert_eq!(restored.balance(), dec!(76));
        assert_eq!(restored.journal_snapshot().len(), 4);
        // A further movement chains onto the restored balance and appends:
        restored.credit_cashflow(dec!(3), "rebate");
        let tail = restored.journal_snapshot().pop().unwrap();
        assert_eq!(tail.balance_after, dec!(79));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// issue 561: 账户维重置原语的三条不变量 —— 余额/预留归位、内存 trail
    /// 恰一行 Reset、磁盘重写后一次全新回放恢复的 balance == 种子（重置
    /// 自检的同一断言，跨 boot 成立）。
    #[test]
    fn account_reset_rewrites_the_journal_to_one_reset_row() {
        let dir = std::env::temp_dir().join(format!(
            "bk-ledger-acct-reset-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("default.journal.jsonl");
        let mut l = Ledger::new();
        l.set_journal_path(path.clone());
        l.seed(dec!(1000));
        l.reserve("o1", dec!(40)).unwrap();
        l.settle_buy_fill("o1", dec!(30), dec!(40));
        l.settle_sell_fill(dec!(21), dec!(1));
        assert_eq!(l.balance(), dec!(990));
        assert!(l.reset_account_to_seed(dec!(1000)));
        assert_eq!(l.balance(), dec!(1000), "balance back on the seed");
        assert_eq!(l.reserved(), Decimal::ZERO, "reservations cleared");
        let rows = l.journal_snapshot();
        assert_eq!(rows.len(), 1, "the trail is exactly the reset row");
        assert_eq!(rows[0].kind, JournalKind::Reset);
        assert_eq!(rows[0].balance_after, dec!(1000));
        assert!(l.is_balanced());

        // Boot replay over the rewritten file: opening balance == seed, the
        // same invariant the reset self-check asserts in-process.
        let mut restored = Ledger::new();
        restored.replay_journal(crate::jsonl::load::<JournalEntry>(
            &path,
            "ledger journal: skipped unparseable lines",
        ));
        assert_eq!(restored.balance(), dec!(1000), "replay == seed");
        assert_eq!(restored.journal_snapshot().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// account.remove 的 journal 收尾（流水卡幽灵 seed 根因修复）：删文件 +
    /// 清 trail，同名 id 下一次 create 是新账本 —— boot 的 attach_journals
    /// 再也回放不出上一世的成交。缺文件的合法 no-op 与删除失败语义一并在
    /// 这里钉住。
    #[test]
    fn account_retire_deletes_the_journal_file_and_clears_the_trail() {
        let dir = std::env::temp_dir().join(format!(
            "bk-ledger-retire-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("kalshi.journal.jsonl");
        let mut l = Ledger::new();
        l.set_journal_path(path.clone());
        l.seed(dec!(1000));
        l.settle_buy_fill("o1", dec!(30), Decimal::ZERO);
        l.settle_sell_fill(dec!(21), dec!(1));
        assert!(path.exists(), "the trail is on disk");
        assert!(l.retire_journal_file());
        assert!(!path.exists(), "the file is gone — no resurrection");
        assert!(
            l.journal_snapshot().is_empty(),
            "the memory trail is cleared too"
        );
        assert_eq!(
            l.balance(),
            dec!(990),
            "the balance is NOT touched — the retiree leaves with its cash trail, the book arithmetic stays exact"
        );

        // A same-id re-create replays NOTHING: the boot replay over the
        // (now absent) file is empty, and the fresh seed starts clean.
        let fresh = Ledger::new();
        assert!(fresh.journal_snapshot().is_empty());

        // A missing file is a legal no-op (an account with no journal).
        let mut bare = Ledger::new();
        bare.set_journal_path(dir.join("ghost.journal.jsonl"));
        assert!(bare.retire_journal_file());
        // A pathless ledger (backtester posture) retires fine too.
        let mut pathless = Ledger::new();
        assert!(pathless.retire_journal_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The memory window is a CAP on memory, not on history: eviction drops
    /// the OLDEST rows only, the Σ invariant holds against the live balance
    /// from the retained prefix's end (older rows live in the file), and the
    /// last row still chains to the balance.
    #[test]
    fn journal_window_evicts_only_the_oldest_rows() {
        let mut l = Ledger::new();
        l.seed(dec!(1));
        for i in 0..(JOURNAL_CAP + 50) {
            l.credit_cashflow(Decimal::from(1), &format!("row-{i}"));
        }
        let rows = l.journal_snapshot();
        assert_eq!(rows.len(), JOURNAL_CAP, "the window is the cap");
        assert_eq!(rows[0].ref_, "row-50", "the oldest rows were evicted");
        assert_eq!(
            rows.last().unwrap().balance_after,
            l.balance(),
            "the tail still chains to the live balance"
        );
        let retained_sum = rows.iter().fold(Decimal::ZERO, |a, r| a + r.amount);
        // Evicted: the seed row (amount 1) + rows 0..=49 (50 × 1).
        let evicted = Decimal::from(1 + 50);
        assert_eq!(
            retained_sum,
            l.balance() - evicted,
            "retained rows sum to everything after the evicted prefix"
        );
    }
}
