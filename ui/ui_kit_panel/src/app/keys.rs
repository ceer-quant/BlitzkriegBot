//! Split out of the parent module: pure move.
//! Keyboard handling: the `on_key` dispatcher, the policy-key submode,
//! command-line tab completion, and the confirmation arming it feeds.

use super::*;

impl App {
    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        // Pending dangerous-action confirmation intercepts everything but `y`.
        if let Some(text) = self.pending_confirmation.clone() {
            self.pending_confirmation = None; // one key resolves it either way
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.log(format!("> {text} (confirmed)"));
                    // The update switch is not a gateway command; it routes to
                    // the update verbs instead of the dispatcher.
                    if let Some(on) = text.strip_prefix(UPDATE_AUTO_CONFIRM_PREFIX) {
                        return Action::UpdateConfigure(on == "on");
                    }
                    // #393: the check switch routes to its own configure arm —
                    // the kernel leaves the AUTO switch untouched (both keys
                    // optional on system.update.configure).
                    if let Some(on) = text.strip_prefix(UPDATE_CHECK_CONFIRM_PREFIX) {
                        return Action::UpdateConfigureCheck(on == "on");
                    }
                    // A confirmed policy write carries its params in the
                    // pending slot (the bar text is only the display name).
                    if let Some((action, _, _)) = self.policy_pending.take() {
                        return Action::PolicyEdit { action };
                    }
                    return Action::RunCommand(text);
                }
                _ => {
                    self.log("toggle cancelled".to_string());
                    self.policy_pending = None;
                    return Action::None;
                }
            }
        }
        // The network self-check overlay sits on top of everything: while it is
        // up only `n`/Esc (close), `r` (probe again) and `q` act. A full report
        // the operator is reading must not be switched out from under them by a
        // stray digit, and `n` is otherwise unused at this level.
        if self.net_visible {
            return match key.code {
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.net_visible = false;
                    Action::None
                }
                KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    self.net_busy = true;
                    Action::NetCheck
                }
                _ => Action::None,
            };
        }
        // 账户↔venue 解耦（本轮）：the `A` account-switch overlay takes the
        // keys while it is up — ↑/↓ move the cursor, Enter switches (through
        // the same confirm bar as the other panel writes: switching the
        // session default redirects every later entry decision this
        // connection makes), Esc/A close. `q` keeps its global quit meaning.
        if self.accounts_visible {
            return match key.code {
                KeyCode::Char('A') | KeyCode::Esc => {
                    self.accounts_visible = false;
                    Action::None
                }
                KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
                KeyCode::Up => {
                    self.accounts_focus = self.accounts_focus.saturating_sub(1);
                    Action::None
                }
                KeyCode::Down => {
                    let n = self
                        .accounts_doc
                        .as_ref()
                        .map(|d| d["accounts"].as_array().map(|a| a.len()).unwrap_or(0))
                        .unwrap_or(0);
                    if self.accounts_focus + 1 < n {
                        self.accounts_focus += 1;
                    }
                    Action::None
                }
                KeyCode::Enter => match self.selected_account_id() {
                    Some(id) => {
                        self.accounts_visible = false;
                        Action::ConfirmToggle(format!("account {id}"))
                    }
                    None => {
                        self.log("no account row selected".to_string());
                        Action::None
                    }
                },
                _ => Action::None,
            };
        }
        // Command bar owns input while focused.
        if self.input_active {
            match key.code {
                KeyCode::Enter => {
                    let cmd = self.input.trim().to_string();
                    self.input.clear();
                    self.input_active = false;
                    self.history_browse = None;
                    if cmd.is_empty() {
                        return Action::None;
                    }
                    self.hints_used[0] = true; // command bar consumed
                    self.history.push(cmd.clone());
                    self.log(format!("> {cmd}"));
                    return Action::RunCommand(cmd);
                }
                KeyCode::Esc => {
                    self.input_active = false;
                    self.input.clear();
                    self.history_browse = None;
                    return Action::None;
                }
                KeyCode::Backspace => {
                    self.input.pop();
                    self.history_browse = None;
                    return Action::None;
                }
                KeyCode::Up => {
                    // Recall: walk backward through executed commands.
                    if !self.history.is_empty() {
                        let n = self.history.len();
                        let off = self.history_browse.unwrap_or(0).min(n - 1);
                        let off = self.history_browse.replace(off).map(|_| off).unwrap_or(0);
                        let idx = n - 1 - off;
                        self.input = self.history[idx].clone();
                        let next = (off + 1).min(n);
                        self.history_browse = Some(next);
                    }
                    return Action::None;
                }
                KeyCode::Down => {
                    // Forward through history; past the end clears the line.
                    if let Some(off) = self.history_browse {
                        let n = self.history.len();
                        match off.checked_sub(1) {
                            Some(next_off) => {
                                self.history_browse = Some(next_off);
                                let idx = n - 1 - next_off;
                                self.input = self.history[idx].clone();
                            }
                            None => {
                                self.history_browse = None;
                                self.input.clear();
                            }
                        }
                    }
                    return Action::None;
                }
                KeyCode::Tab => {
                    self.input = complete(&self.input);
                    return Action::None;
                }
                KeyCode::Char(c) => {
                    self.input.push(c);
                    self.history_browse = None;
                    if c == '?' && self.input.trim() == "?" {
                        // `?` as the first thing typed opens help instead.
                        self.input.clear();
                        self.input_active = false;
                        self.help_visible = true;
                    }
                    return Action::None;
                }
                _ => return Action::None,
            }
        }

        // #364: the policy editor's sub-mode owns the keyboard while active —
        // it must sit BEFORE the global arms, or `q` would quit the panel and
        // `r` would refresh the snapshot instead of rolling the policy back.
        // Inside the mode: ←/→ account · ↑/↓ cursor · [e] edit · [a] add ·
        // [d] delete (asks) · [空格] on/off (asks) · [s] save (asks) · [r]
        // rollback (asks) · [q]/Esc RETURN out (the spec's `[q] return`).
        // Outside the mode the Settings tab's documented [a] auto-update and
        // the global [r] refresh keep their meaning — the mode is opt-in.
        if self.policy_mode {
            return match key.code {
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc
                    if self.policy_pending.is_none() =>
                {
                    self.policy_mode = false;
                    self.policy_draft = None;
                    self.policy_focus = 0;
                    Action::None
                }
                KeyCode::Left if !self.policy_busy => {
                    self.policy_account_idx = self.policy_account_idx.saturating_sub(1);
                    self.policy_focus = 0;
                    Action::PolicyRefresh
                }
                KeyCode::Right if !self.policy_busy => {
                    // The chips are `defaults` + one per list entry: the cap
                    // is the list LENGTH (index 0 is defaults, so the last
                    // valid index is len, not len-1).
                    if self.policy_account_idx < self.policy_accounts.len() {
                        self.policy_account_idx += 1;
                    }
                    self.policy_focus = 0;
                    Action::PolicyRefresh
                }
                KeyCode::Up if self.policy_draft.is_none() => {
                    self.policy_focus = self.policy_focus.saturating_sub(1);
                    Action::None
                }
                KeyCode::Down if self.policy_draft.is_none() => {
                    self.policy_focus = self.policy_focus.saturating_add(1);
                    Action::None
                }
                KeyCode::Char(c)
                    if self.policy_draft.is_none()
                        && !self.policy_busy
                        && matches!(c, 'e' | 'a' | 'd' | 's' | 'r' | ' ') =>
                {
                    self.policy_key(c)
                }
                _ => Action::None,
            };
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
            // Esc closes whichever overlay is up. The help panel has always
            // advertised "Esc to close" in its title, so this is the key that
            // makes that true as well as the one that closes the net-check.
            KeyCode::Esc if self.help_visible || self.net_visible => {
                self.help_visible = false;
                self.net_visible = false;
                Action::None
            }
            KeyCode::Char('?') => {
                self.help_visible = !self.help_visible;
                self.hints_used[2] = true;
                Action::None
            }
            // Network self-check: `n` opens the diagnosis overlay and probes on
            // the first press; afterwards it shows the report already held, and
            // `r` re-probes. The probe dials, so it never runs on the render
            // loop — the action hands it to a worker thread.
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.net_visible = true;
                self.hints_used[6] = true;
                if self.net_busy {
                    Action::None
                } else if self.net_report.is_none() {
                    self.net_busy = true;
                    Action::NetCheck
                } else {
                    Action::None
                }
            }
            KeyCode::Char(':') | KeyCode::Char('/') => {
                self.input_active = true;
                self.input.clear();
                self.history_browse = None;
                Action::None
            }
            // 账户↔venue 解耦（本轮）：`A` (shift-a) opens the account-switch
            // overlay — the multi-account twin of `n`. Lowercase `a` stays
            // taken (Evolution accept / Settings auto-update). First press
            // fetches `account.list` on a worker; a list already held renders
            // at once (the overlay's refresh key is the global `r`).
            KeyCode::Char('A') => {
                self.accounts_visible = true;
                if self.accounts_doc.is_none() {
                    Action::AccountsRefresh
                } else {
                    Action::None
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => Action::Refresh,
            KeyCode::Char('1') => {
                self.tab = Tab::Overview;
                Action::None
            }
            KeyCode::Char('2') => {
                self.tab = Tab::Positions;
                Action::None
            }
            KeyCode::Char('3') => {
                self.tab = Tab::Orders;
                Action::None
            }
            KeyCode::Char('4') => {
                self.tab = Tab::Trades;
                Action::None
            }
            KeyCode::Char('5') => {
                self.tab = Tab::Plugins;
                Action::RefreshPlugins
            }
            KeyCode::Char('6') => {
                self.tab = Tab::Evolution;
                Action::None // the 1 s poller re-fetches proposals with the snapshot
            }
            KeyCode::Char('7') => {
                self.tab = Tab::Decisions;
                Action::None
            }
            KeyCode::Char('8') => {
                self.tab = Tab::Settings;
                Action::None // the snapshot poller carries system_version
            }
            // issue 456: ↑/↓ move the Orders-tab cursor; [F] force-closes the
            // selected order — as a ConfirmToggle, so the y/n bar intercepts
            // and only a confirmed `force-order` line reaches the dispatcher.
            KeyCode::Up if self.tab == Tab::Orders => {
                self.orders_focus = self.orders_focus.saturating_sub(1);
                Action::None
            }
            KeyCode::Down if self.tab == Tab::Orders => {
                self.orders_focus = self.orders_focus.saturating_add(1);
                Action::None
            }
            KeyCode::Char('f') | KeyCode::Char('F') if self.tab == Tab::Orders => {
                match self.selected_order_id() {
                    Some(id) => Action::ConfirmToggle(format!("force-order {id}")),
                    None => {
                        self.log("no order row selected (use ↑/↓ in the Orders tab)".to_string());
                        Action::None
                    }
                }
            }
            // #364: `p` on the Settings tab enters the policy editor's
            // sub-mode (its keys and the [q] return are handled above).
            KeyCode::Char('p') if self.tab == Tab::Settings => {
                self.policy_mode = true;
                self.policy_focus = 0;
                Action::PolicyRefresh
            }
            // Settings keys (VERSIONING.md §6.2). `a` is the one switch that
            // decides "may the launcher auto-replace binaries later", so it
            // always asks — nothing flips silently.
            KeyCode::Char('c') if self.tab == Tab::Settings && !self.update_busy => {
                if self.snap.system_version.as_ref().map(|v| v.check_enabled) == Some(false) {
                    self.log(
                        "update check is disabled — press k to enable it (or the WebUI \
                         settings switch / user_layer/configs/update.toml)"
                            .to_string(),
                    );
                    Action::None
                } else {
                    self.update_busy = true;
                    Action::UpdateCheck
                }
            }
            KeyCode::Char('a') if self.tab == Tab::Settings && !self.update_busy => {
                let on = self
                    .snap
                    .system_version
                    .as_ref()
                    .map(|v| v.auto_update)
                    .unwrap_or(false);
                self.pending_confirmation = Some(format!(
                    "{UPDATE_AUTO_CONFIRM_PREFIX}{}",
                    if on { "off" } else { "on" }
                ));
                Action::None
            }
            // #393: `k` toggles the outbound-check switch. Before this, the
            // panel showed a greyed check button with NO unlock path unless
            // the operator hand-edited update.toml — the switch is the kernel's
            // system.update.configure verb, so it asks y/n like `a` does.
            KeyCode::Char('k') if self.tab == Tab::Settings && !self.update_busy => {
                let on = self
                    .snap
                    .system_version
                    .as_ref()
                    .map(|v| v.check_enabled)
                    .unwrap_or(false);
                self.pending_confirmation = Some(format!(
                    "{UPDATE_CHECK_CONFIRM_PREFIX}{}",
                    if on { "off" } else { "on" }
                ));
                Action::None
            }
            // #379: stage the newer release (download + verify, §7.5). Like
            // `c`, it is refused up front when the governing switch is off —
            // and staging additionally requires an Available verdict.
            KeyCode::Char('s') if self.tab == Tab::Settings && !self.update_busy => {
                let auto_on = self
                    .snap
                    .system_version
                    .as_ref()
                    .map(|v| v.auto_update)
                    .unwrap_or(false);
                if !auto_on {
                    self.log(
                        "staging needs auto-update ON (press a first) — the kernel downloads                          into data/update/staging/ and never installs on its own"
                            .to_string(),
                    );
                    Action::None
                } else if self
                    .snap
                    .system_version
                    .as_ref()
                    .map(|v| v.update_available)
                    != Some(Some(true))
                {
                    self.log("nothing to stage — check first; staging follows an                               'available' verdict"
                        .to_string());
                    Action::None
                } else {
                    self.update_busy = true;
                    Action::UpdateStage
                }
            }
            KeyCode::Char('i') if self.tab == Tab::Settings && !self.update_busy => {
                let auto_on = self
                    .snap
                    .system_version
                    .as_ref()
                    .map(|v| v.auto_update)
                    .unwrap_or(false);
                if !auto_on {
                    self.log(
                        "install needs auto-update ON (press a first) — the switch is OFF by \
                         default and the kernel never installs on its own"
                            .to_string(),
                    );
                    Action::None
                } else {
                    self.update_busy = true;
                    Action::UpdateInstall
                }
            }
            KeyCode::Tab => {
                let was_plugins = self.tab == Tab::Plugins;
                self.tab = self.tab.next();
                if self.tab == Tab::Plugins && !was_plugins {
                    Action::RefreshPlugins
                } else {
                    Action::None
                }
            }
            KeyCode::Up if self.tab == Tab::Plugins => {
                self.plugin_focus = self.plugin_focus.saturating_sub(1);
                Action::None
            }
            KeyCode::Down if self.tab == Tab::Plugins => {
                self.plugin_focus = self.plugin_focus.saturating_add(1);
                Action::None
            }
            KeyCode::Enter if self.tab == Tab::Plugins => match self.plugin_toggle_command() {
                Some(cmd) => Action::ConfirmToggle(cmd),
                None => {
                    self.log("no toggleable row selected (use ↑/↓ in the Plugins tab)".to_string());
                    Action::None
                }
            },
            KeyCode::Up if self.tab == Tab::Evolution => {
                self.evo_focus = self.evo_focus.saturating_sub(1);
                Action::None
            }
            KeyCode::Down if self.tab == Tab::Evolution => {
                self.evo_focus = self.evo_focus.saturating_add(1);
                Action::None
            }
            KeyCode::Char('a') if self.tab == Tab::Evolution => self.evo_decide("accept"),
            KeyCode::Char('x') if self.tab == Tab::Evolution => self.evo_decide("reject"),
            KeyCode::Char('d') if self.tab == Tab::Evolution => self.evo_decide("defer"),
            KeyCode::Char('e') if self.tab == Tab::Evolution => self.evo_toggle_auto(),
            KeyCode::Char('m') if self.tab == Tab::Evolution => self.evo_toggle_engine(),
            KeyCode::Char('u') if self.tab == Tab::Evolution => self.evo_rollback(),
            _ => Action::None,
        }
    }

    /// #364: the Settings-tab policy keys, dispatched from `on_key` only when
    /// the face owns the key (right tab, no draft open, no write in flight).
    /// Each arm mirrors one requirement: [e] edit, [a] add rule, [d] delete
    /// (asks), [↑/↓] priority, [空格] enable/disable, [s] save, [r] rollback.
    fn policy_key(&mut self, c: char) -> Action {
        let Some(account) = self.policy_account_id() else {
            self.log("policy: no account list yet (still loading)".to_string());
            return Action::None;
        };
        let rules_len = self
            .policy_section
            .as_ref()
            .and_then(|s| s.get("rules"))
            .and_then(|r| r.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let on_rule = self.policy_focus >= 5 && self.policy_focus - 5 < rules_len;
        match c {
            'e' => {
                if self.policy_focus < 4 {
                    self.log(
                        "param edit: use the WebUI for base params — here rules are first-class \
                         (the four numbers are shown for reference)"
                            .to_string(),
                    );
                } else if on_rule {
                    let idx = self.policy_focus - 5;
                    let rule = self
                        .policy_section
                        .as_ref()
                        .and_then(|s| s.get("rules"))
                        .and_then(|r| r.as_array())
                        .and_then(|a| a.get(idx))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    match PolicyDraft::from_rule(&rule, Some(idx)) {
                        Ok(d) => {
                            self.policy_draft = Some(d);
                        }
                        Err(e) => self.log(format!("policy: cannot edit that rule: {e}")),
                    }
                } else {
                    self.log("no rule row selected (↑/↓ to move onto a rule)".to_string());
                }
                Action::None
            }
            'a' => {
                self.policy_draft = Some(PolicyDraft::neutral());
                Action::None
            }
            'd' => {
                if on_rule {
                    let idx = self.policy_focus - 5;
                    let mut rules = self
                        .policy_section
                        .as_ref()
                        .and_then(|s| s.get("rules"))
                        .and_then(|r| r.as_array())
                        .cloned()
                        .unwrap_or_default();
                    rules.remove(idx);
                    self.ask_policy_edit(
                        PolicyAction::Set,
                        self.policy_set_params_with_rules(rules),
                        format!("delete rule #{}, account {account}", idx + 1),
                    )
                } else {
                    self.log("no rule row selected (↑/↓ to move onto a rule)".to_string());
                    Action::None
                }
            }
            ' ' => {
                if on_rule {
                    let idx = self.policy_focus - 5;
                    let mut rules = self
                        .policy_section
                        .as_ref()
                        .and_then(|s| s.get("rules"))
                        .and_then(|r| r.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let on = rules[idx]
                        .get("enabled")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false);
                    rules[idx]["enabled"] = serde_json::json!(!on);
                    self.ask_policy_edit(
                        PolicyAction::Set,
                        self.policy_set_params_with_rules(rules),
                        format!(
                            "{} rule #{}, account {account}",
                            if on { "disable" } else { "enable" },
                            idx + 1
                        ),
                    )
                } else {
                    self.log("no rule row selected (↑/↓ to move onto a rule)".to_string());
                    Action::None
                }
            }
            's' => {
                let params = self.policy_set_params_from_view();
                self.ask_policy_edit(
                    PolicyAction::Set,
                    params,
                    format!("save policy, account {account}"),
                )
            }
            'r' => {
                // Rollback = take the account's own most recent `set`'s
                // BEFORE-state and write it back via `set` (no new backend
                // method: the journal IS the version store).
                match self.policy_rollback_params() {
                    Ok((label, params)) => self.ask_policy_edit(PolicyAction::Set, params, label),
                    Err(e) => {
                        self.log(format!("rollback: {e}"));
                        Action::None
                    }
                }
            }
            _ => Action::None,
        }
    }

    /// The account id under the ←/→ cursor (`"defaults"` for index 0).
    /// The account id under the ←/→ cursor (`"defaults"` for index 0; the
    /// list entries start at chip index 1 = `policy_accounts[0]`).
    pub fn policy_account_id(&self) -> Option<String> {
        if self.policy_account_idx == 0 {
            return Some("defaults".to_string());
        }
        self.policy_accounts
            .get(self.policy_account_idx - 1)
            .and_then(|v| {
                v.get("accountId")
                    .and_then(|a| a.as_str())
                    .map(|s| s.to_string())
            })
    }

    /// Whether the confirmed `policy.` prefix was armed (used by `on_key`).
    pub fn policy_confirm_armed(&self) -> bool {
        self.policy_pending.is_some()
    }

    /// Arm a policy write: the confirm bar shows the label; `y` fires
    /// [`Action::PolicyEdit`] with the action (params ride along in the
    /// pending slot). Cancel clears both.
    fn ask_policy_edit(
        &mut self,
        action: PolicyAction,
        params: serde_json::Value,
        label: String,
    ) -> Action {
        let verb = match action {
            PolicyAction::Set => "set",
            PolicyAction::Reset => "reset",
        };
        self.policy_pending = Some((action, params, label.clone()));
        self.pending_confirmation = Some(format!("{POLICY_CONFIRM_PREFIX}{verb} {label}"));
        Action::None
    }

    /// The set params for "write the CURRENT effective view back": budget
    /// triple + equity + cap + rules, taken from the section the kernel last
    /// answered with (decimals cross as the strings it sent).
    fn policy_set_params_from_view(&self) -> serde_json::Value {
        let account = self.policy_account_id().unwrap_or_default();
        let s = self
            .policy_section
            .clone()
            .unwrap_or(serde_json::Value::Null);
        let mut params = serde_json::json!({ "accountId": account });
        for (wire, key) in [
            ("budgetRatio", "budget_ratio"),
            ("minBudgetUsd", "min_budget_usd"),
            ("maxBudgetUsd", "max_budget_usd"),
            ("minEquityUsd", "min_equity_usd"),
        ] {
            if let Some(v) = s.get(key).and_then(|v| v.as_str()) {
                params[wire] = serde_json::json!(v);
            }
        }
        if let Some(n) = s.get("max_positions_per_asset").and_then(|v| v.as_u64()) {
            params["maxPositionsPerAsset"] = serde_json::json!(n);
        }
        if let Some(rules) = s.get("rules").and_then(|r| r.as_array()) {
            params["rules"] = serde_json::json!(rules);
        }
        params
    }

    /// Same as above but with the rules replaced (the [d]/[空格] arms).
    fn policy_set_params_with_rules(&self, rules: Vec<serde_json::Value>) -> serde_json::Value {
        let mut params = self.policy_set_params_from_view();
        params["rules"] = serde_json::json!(rules);
        params
    }

    /// The rollback write: find the account's most recent `set` audit line
    /// and return its BEFORE-state as the params to write back. No new
    /// backend method — the journal already holds each version.
    fn policy_rollback_params(&self) -> Result<(String, serde_json::Value), String> {
        let account = self.policy_account_id().unwrap_or_default();
        let last_set = self
            .policy_history
            .iter()
            .rfind(|l| l.get("action").and_then(|a| a.as_str()) == Some("set"))
            .ok_or_else(|| format!("no prior version to roll back to for `{account}`"))?;
        let before = last_set
            .get("before")
            .cloned()
            .filter(|b| !b.is_null())
            .ok_or_else(|| "the last audit line carries no before-state".to_string())?;
        let ts = last_set.get("tsMs").and_then(|t| t.as_i64()).unwrap_or(0);
        let mut params = serde_json::json!({ "accountId": account });
        for (wire, key) in [
            ("budgetRatio", "budget_ratio"),
            ("minBudgetUsd", "min_budget_usd"),
            ("maxBudgetUsd", "max_budget_usd"),
            ("minEquityUsd", "min_equity_usd"),
        ] {
            if let Some(v) = before.get(key).and_then(|v| v.as_str()) {
                params[wire] = serde_json::json!(v);
            }
        }
        if let Some(n) = before
            .get("max_positions_per_asset")
            .and_then(|v| v.as_u64())
        {
            params["maxPositionsPerAsset"] = serde_json::json!(n);
        }
        if let Some(rules) = before.get("rules").and_then(|r| r.as_array()) {
            params["rules"] = serde_json::json!(rules);
        }
        Ok((
            format!("roll back account {account} to the version before {ts}"),
            params,
        ))
    }
}

/// The commands the bar completes against (longest-prefix, one candidate): the
/// gateway's own verbs, so a command it would reject can never be offered.
/// `start` completes to a filled-in example because its arguments are the point
/// of the line.
fn candidates() -> Vec<&'static str> {
    command_verbs()
        .map(|verb| match verb {
            "start" => "start BTC,ETH,SOL,XRP --dry-run",
            other => other,
        })
        .collect()
}

/// Tab-completion: when exactly one known command starts with the current
/// input, fill it; with several, fill their longest common prefix.
fn complete(input: &str) -> String {
    let t = input.trim_start_matches(':').trim();
    if t.is_empty() {
        return input.to_string();
    }
    let cands: Vec<&str> = candidates()
        .into_iter()
        .filter(|c| c.starts_with(t))
        .collect();
    match cands.first() {
        None => input.to_string(),
        Some(first) => {
            let shared = cands.iter().fold(first.to_string(), |acc: String, c| {
                common_prefix(&acc, c).to_string()
            });
            shared
        }
    }
}

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut i = 0;
    while i < a.len() && i < b.len() && a[i] == b[i] {
        i += 1;
    }
    std::str::from_utf8(&a[..i]).unwrap_or("")
}
