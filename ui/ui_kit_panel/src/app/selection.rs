//! Split out of the parent module: pure move.
//! Selection cursors and derived views: the accounts-overlay cursor,
//! plugin rows, the selected order, and the evolution proposals' actions.

use super::*;

impl App {
    /// 账户↔venue 解耦（本轮）：the account id under the overlay's cursor,
    /// read off the raw `account.list` reply (no second model). `None` when
    /// the book has not loaded or the cursor ran past it — the Enter arm logs
    /// instead of guessing.
    pub fn selected_account_id(&self) -> Option<String> {
        self.accounts_doc
            .as_ref()?
            .get("accounts")
            .and_then(|a| a.as_array())
            .and_then(|rows| rows.get(self.accounts_focus))
            .and_then(|row| row.get("id"))
            .and_then(|id| id.as_str())
            .map(|s| s.to_string())
    }
}

impl App {
    /// Rows in the Plugins tab the cursor can land on: one row per strategy and
    /// one per extension; market plugins are read-only.
    pub fn plugin_row_count(&self) -> usize {
        self.snap.strategies.len() + self.snap.extensions.len()
    }

    /// The command the cursor currently points at, if any. Toggling an ENABLED
    /// entry off needs confirmation (it can stop a strategy that is holding an
    /// open position); enabling is read-safe so it goes straight through.
    pub fn plugin_toggle_command(&self) -> Option<String> {
        let rows = self.plugin_row_count();
        if rows == 0 {
            return None;
        }
        let i = self.plugin_focus.min(rows - 1);
        if i < self.snap.strategies.len() {
            let s = &self.snap.strategies[i];
            Some(format!(
                "strategy {} {}",
                s.name,
                if s.enabled { "off" } else { "on" }
            ))
        } else {
            let e = &self.snap.extensions[i - self.snap.strategies.len()];
            let enable = e.state != "enabled";
            if enable {
                Some(format!("extension {} on", e.name))
            } else {
                Some(format!("extension {} off", e.name))
            }
        }
    }

    /// Pending proposals in display order (the cursor indexes into this list;
    /// `ui.rs` renders the same list, so cursor and screen stay in step).
    pub fn evo_pending(&self) -> Vec<&EvolutionProposalView> {
        self.snap
            .evolution_proposals
            .iter()
            .filter(|p| p.is_pending())
            .collect()
    }

    /// issue 456: the order id under the Orders-tab cursor, if the book is
    /// not empty. Cursor and `ui.rs` render the same `snap.orders` order.
    pub fn selected_order_id(&self) -> Option<String> {
        if self.snap.orders.is_empty() {
            return None;
        }
        let i = self.orders_focus.min(self.snap.orders.len() - 1);
        Some(self.snap.orders[i].order_id.clone())
    }

    /// The pending proposal under the cursor, if the list is not empty.
    fn evo_selected(&self) -> Option<&EvolutionProposalView> {
        let pending = self.evo_pending();
        if pending.is_empty() {
            return None;
        }
        let i = self.evo_focus.min(pending.len() - 1);
        Some(pending[i])
    }

    /// accept / reject / defer the proposal under the cursor. Accepting
    /// hot-swaps live strategy parameters — routed through the confirm bar
    /// (same bar as a dangerous plugin toggle); reject/defer move nothing and
    /// go straight through.
    pub(super) fn evo_decide(&mut self, decision: &str) -> Action {
        let Some(p) = self.evo_selected() else {
            self.log("no pending evolution proposal to decide (nothing waiting)".to_string());
            return Action::None;
        };
        let cmd = format!("decide {} {}", p.id, decision);
        if decision == "accept" {
            Action::ConfirmToggle(cmd)
        } else {
            self.log(format!("> {cmd}"));
            Action::RunCommand(cmd)
        }
    }

    /// Rollback the last accepted promotion for the strategy the cursor points
    /// at (undo restores the previous params live). Dangerous — confirm first.
    pub(super) fn evo_rollback(&mut self) -> Action {
        let Some(p) = self.evo_selected() else {
            self.log(
                "rollback needs a selection: point ↑/↓ at a pending proposal to name a strategy"
                    .to_string(),
            );
            return Action::None;
        };
        let strategy = p.strategy.clone();
        if strategy.is_empty() {
            self.log("that proposal carries no strategy name — cannot roll back".to_string());
            return Action::None;
        }
        Action::ConfirmToggle(format!("rollback {strategy}"))
    }

    /// Flip the auto-evolve switch from the status the last snapshot carried.
    pub(super) fn evo_toggle_auto(&mut self) -> Action {
        let on = self
            .snap
            .evolution_status
            .as_ref()
            .map(|s| s.auto_evolve)
            .unwrap_or(false);
        let cmd = format!("auto-evolve {}", if on { "off" } else { "on" });
        self.log(format!("> {cmd}"));
        Action::RunCommand(cmd)
    }

    /// #249: the engine switch — whether anything evolves at all. A different
    /// question from the auto switch (who applies what qualifies), and the one
    /// the page has to answer before "auto-evolve ON" means anything.
    pub(super) fn evo_toggle_engine(&mut self) -> Action {
        let on = self
            .snap
            .evolution_status
            .as_ref()
            .map(|s| s.enabled)
            .unwrap_or(false);
        let cmd = format!("evolve {}", if on { "off" } else { "on" });
        self.log(format!("> {cmd}"));
        Action::RunCommand(cmd)
    }

    /// True when the pending command must be confirmed before dispatch.
    pub fn toggle_needs_confirmation(&self, cmd: &str) -> bool {
        // Disabling anything swaps routing (may strand an open position);
        // accepting an evolution proposal swaps live strategy parameters;
        // rollback reverts them; force-order declares a live book entry dead
        // (issue 456). Those four are the dangerous verbs here.
        cmd.contains(" off")
            || cmd.contains("disable")
            || cmd.contains(" accept")
            || cmd.starts_with("rollback ")
            || cmd.starts_with("force-order ")
            || cmd.starts_with("force_order ")
            // 账户↔venue 解耦（本轮）：switching the session default
            // redirects every later entry decision this connection makes —
            // a write that always asks, like the other posture moves.
            || cmd.starts_with("account ")
    }
}
