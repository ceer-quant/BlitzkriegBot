//! The storage seam (issue #1199, PR-1): the kernel's write paths persist
//! through [`StoreApi`] instead of naming `.env` files and env keys
//! directly. Today's only implementation is [`EnvStore`] (the `.env`
//! backend, behaviour byte-identical to the pre-seam writers); the DB
//! backend lands in a follow-up PR.
//!
//! Why a trait on `Core` (`Option<Box<dyn StoreApi>>`) and not an enum:
//! the implementation set grows by ADDING a backend (SQLite next), and the
//! dispatch never needs to inspect the variant — call sites stay blind to
//! the carrier, which is the whole point of the ruling.
//!
//! `None` = persistence disabled for this core (backtester, most
//! in-process tests) — the same honest refusal the writers gave when their
//! path pair was `None`, now decided once at assembly instead of per call.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rust_decimal::Decimal;

use crate::account::config::CrossVenueArbConfig;
use crate::account::{AccountStatus, CredentialKeys};
use crate::model::CoreError;

/// One domain write's field→value changes, in the wire audit's camelCase
/// spelling (`maxSingleLossUsd`, `stopLossPct`, …). The store maps them to
/// its carrier's key grammar; the service layer only ever speaks fields.
pub type FieldChanges = Vec<(String, String)>;

/// The durable face of every cockpit write (issue #1199).
///
/// Domain-grouped, carrier-neutral: no method name or type mentions `.env`,
/// keys or files. One method = one domain write; composite invariants
/// (plan-first validation, persist-before-memory ordering, audit lines)
/// stay in the service layer exactly where they were.
pub trait StoreApi: Send + Sync {
    // ── Account book ─────────────────────────────────────────────────────

    /// Whether this store accepts account-book writes at all. `false` =
    /// the core has no account book carrier (backtester, most in-process
    /// tests) — the writers' standard "persistence is disabled" refusal.
    /// The IPC strategy-enable mirror gate reads this instead of probing
    /// the config pair per call (the pre-seam `account_env_write_paths()`
    /// check, moved behind the seam).
    fn account_writes_enabled(&self) -> bool;

    /// The account↔venue binding write (`account.venue.set`); an empty
    /// `venue` unbinds. Refuses while the legacy accounts toml exists.
    fn save_account_venue(&self, account_id: &str, venue: &str) -> Result<(), CoreError>;

    /// `account.create`: append the id with its display name, optional
    /// venue binding and the plugin's credential VARIABLE NAMES (values
    /// belong to [`StoreApi::save_credentials`], never here).
    fn save_account_create(
        &self,
        account_id: &str,
        name: Option<&str>,
        venue: Option<&str>,
        credential_keys: &CredentialKeys,
    ) -> Result<(), CoreError>;

    /// `account.remove`: drop the id and sweep every row it owns. Refuses
    /// an unknown id, an emptying removal, or a legacy-toml presence; the
    /// display pointer is re-derived when it named the removed id.
    fn remove_account(&self, account_id: &str) -> Result<(), CoreError>;

    /// The trading-switch write (`account.trading.set`). `true` clears the
    /// explicit-off row (absence is the "trading" spelling).
    fn save_account_trading(&self, account_id: &str, enabled: bool) -> Result<(), CoreError>;

    /// The lifecycle-status write (`account.status.set`). `Active` clears
    /// the row (absence is "active").
    fn save_account_status(
        &self,
        account_id: &str,
        status: &AccountStatus,
    ) -> Result<(), CoreError>;

    /// The unfreeze write (`account.unfreeze`): `Active` + trading-on are
    /// both spelled by absence, so this is ONE removal — no half-state.
    fn save_account_active(&self, account_id: &str) -> Result<(), CoreError>;

    /// The display-pointer write (`display.account.set`).
    fn save_display_account(&self, account_id: &str) -> Result<(), CoreError>;

    /// The strategy-enablement write (`account.strategies.set`). An empty
    /// list clears the row (absence = unrestricted).
    fn save_account_strategies(
        &self,
        account_id: &str,
        strategies: &[String],
    ) -> Result<(), CoreError>;

    /// The round-family set write (`account.rounds.set`). An empty list
    /// clears the row (absence = no declared family set).
    fn save_account_rounds(&self, account_id: &str, rounds: &[i64]) -> Result<(), CoreError>;

    /// The venue's first reported equity (`realign_venue_cash`): the
    /// account's opening principal with source + timestamp, written exactly
    /// once (an already-recorded value is never overwritten).
    fn save_account_opening_equity(
        &self,
        account_id: &str,
        equity: Decimal,
        at_ms: i64,
    ) -> Result<(), CoreError>;

    // ── Risk: the six systemic bounds ────────────────────────────────────

    /// The systemic-limit write (`risk.setSystemic`) for one account (the
    /// process default when `None`). Returns whether the write landed —
    /// false = no carrier on this core (the caller's memory-only posture,
    /// honestly reported by its own `persisted`).
    fn save_risk_bounds(
        &self,
        account_id: Option<&str>,
        fields: &FieldChanges,
    ) -> Result<bool, CoreError>;

    // ── Exit discipline ──────────────────────────────────────────────────

    /// The exit-discipline write (`risk.setExit`) for one account. Same
    /// return contract as [`StoreApi::save_risk_bounds`].
    fn save_exit(&self, account_id: Option<&str>, fields: &FieldChanges)
    -> Result<bool, CoreError>;

    // ── Cross-venue arb legs ─────────────────────────────────────────────

    /// `arb.config.set`: the five arb-leg facts as one write.
    fn save_arb_config(&self, cfg: &CrossVenueArbConfig) -> Result<(), CoreError>;

    /// `arb.config.clear`: remove the arb-leg facts.
    fn clear_arb_config(&self) -> Result<(), CoreError>;

    // ── Execution-policy sections ────────────────────────────────────────

    /// `execution_policy.set/.reset` for one account: the section's whole
    /// footprint (field → value, `None` = swept) as one sweep-then-write.
    /// Refuses while the legacy policy toml exists (the kernel's own
    /// bootstrap template is swept instead — the one exception the old
    /// write face owned). Returns the footprint for the reload snapshot.
    fn save_policy_section(
        &self,
        account_id: &str,
        state: &BTreeMap<String, Option<String>>,
    ) -> Result<(), String>;

    // ── Per-account round duration ───────────────────────────────────────

    /// `round_duration.set`: `Some(seconds)` upserts the account's round
    /// length; `None` clears the override (reset to the default grid).
    fn save_account_round_duration(
        &self,
        account_id: &str,
        seconds: Option<i64>,
    ) -> Result<(), CoreError>;

    // ── Credentials ──────────────────────────────────────────────────────

    /// `market.credentials.set`: upsert the plugin's declared variable
    /// names → values, or remove them all (`reset`). Values never appear
    /// in logs — only the key names do.
    ///
    /// `reset_keys` is the REMOVE set: empty = upsert `vars`; non-empty =
    /// remove exactly those keys (the caller passes the plugin's declared
    /// table on reset). The two branches are mutually exclusive — the
    /// pre-seam semantics of `market.credentials.set` (an empty
    /// `reset_keys` is NOT a reset request).
    fn save_credentials(
        &self,
        vars: &BTreeMap<String, String>,
        reset_keys: &[&str],
    ) -> Result<(), CoreError>;
}

/// The `.env`-backed store (issue #1199 PR-1).
///
/// Paths are captured at assembly from [`crate::service::CoreConfig`] —
/// the config is immutable after construction, so this is the same
/// resolution every writer did per call:
///   * `accounts` — the account-write path pair (env + legacy toml),
///     `None` when the core persists no account book (backtester, tests);
///   * `env` — the cockpit/policy/round-duration/credential carrier,
///     `None` when the core has no file layer at all;
///   * `policy_legacy` — the execution-policy toml whose existence refuses
///     section writes.
///
/// Every method delegates to the writer that already owns the key grammar
/// (`account::config` / `env_file` / the cockpit and policy faces), so the
/// file layout, the atomic write primitive and every refusal are the
/// pre-seam bytes.
pub struct EnvStore {
    accounts: Option<(PathBuf, PathBuf)>,
    env: Option<PathBuf>,
    policy_legacy: Option<String>,
}

impl EnvStore {
    /// Assemble from the config's resolved paths (see the struct doc).
    pub fn new(
        accounts: Option<(PathBuf, PathBuf)>,
        env: Option<PathBuf>,
        policy_legacy: Option<String>,
    ) -> Self {
        Self {
            accounts,
            env,
            policy_legacy,
        }
    }

    /// The account-write pair, or the standard persistence-disabled
    /// refusal (the exact shape the writers gave per call pre-seam).
    fn accounts(&self, op: &str) -> Result<(PathBuf, PathBuf), CoreError> {
        self.accounts.clone().ok_or_else(|| {
            CoreError::new(
                crate::model::CoreErrorCode::InvalidParams,
                format!(
                    "{op}: no accounts config path configured — \
                     persistence is disabled for this core (backtests and \
                     in-process tests); nothing was applied"
                ),
            )
        })
    }

    fn env(&self, op: &str) -> Result<&PathBuf, CoreError> {
        self.env.as_ref().ok_or_else(|| {
            CoreError::new(
                crate::model::CoreErrorCode::InvalidParams,
                format!(
                    "{op}: no .env path configured — persistence is disabled \
                     for this core (backtests and in-process tests); nothing \
                     was applied"
                ),
            )
        })
    }
}

impl StoreApi for EnvStore {
    fn account_writes_enabled(&self) -> bool {
        self.accounts.is_some()
    }

    fn save_account_venue(&self, account_id: &str, venue: &str) -> Result<(), CoreError> {
        let (env, legacy) = self.accounts("account.venue.set")?;
        crate::account::config::env_persist_account_venue(&legacy, &env, account_id, venue)
    }

    fn save_account_create(
        &self,
        account_id: &str,
        name: Option<&str>,
        venue: Option<&str>,
        credential_keys: &CredentialKeys,
    ) -> Result<(), CoreError> {
        let (env, legacy) = self.accounts("account.create")?;
        crate::account::config::env_persist_account_create(
            &legacy,
            &env,
            account_id,
            name,
            venue,
            credential_keys,
        )
    }

    fn remove_account(&self, account_id: &str) -> Result<(), CoreError> {
        let (env, legacy) = self.accounts("account.remove")?;
        crate::account::config::env_persist_account_remove(&legacy, &env, account_id)
    }

    fn save_account_trading(&self, account_id: &str, enabled: bool) -> Result<(), CoreError> {
        let (env, legacy) = self.accounts("account.trading.set")?;
        crate::account::config::env_persist_account_trading(&legacy, &env, account_id, enabled)
    }

    fn save_account_status(
        &self,
        account_id: &str,
        status: &AccountStatus,
    ) -> Result<(), CoreError> {
        let (env, legacy) = self.accounts("account.status.set")?;
        crate::account::config::env_persist_account_status(&legacy, &env, account_id, status)
    }

    fn save_account_active(&self, account_id: &str) -> Result<(), CoreError> {
        let (env, legacy) = self.accounts("account.status.set")?;
        crate::account::config::env_persist_account_active(&legacy, &env, account_id)
    }

    fn save_display_account(&self, account_id: &str) -> Result<(), CoreError> {
        let (env, legacy) = self.accounts("display.account.set")?;
        crate::account::config::env_persist_display_account(&legacy, &env, account_id)
    }

    fn save_account_strategies(
        &self,
        account_id: &str,
        strategies: &[String],
    ) -> Result<(), CoreError> {
        let (env, legacy) = self.accounts("account.strategies.set")?;
        crate::account::config::env_persist_account_strategies(
            &legacy, &env, account_id, strategies,
        )
    }

    fn save_account_rounds(&self, account_id: &str, rounds: &[i64]) -> Result<(), CoreError> {
        let (env, legacy) = self.accounts("account.rounds.set")?;
        crate::account::config::env_persist_account_rounds(&legacy, &env, account_id, rounds)
    }

    fn save_account_opening_equity(
        &self,
        account_id: &str,
        equity: Decimal,
        at_ms: i64,
    ) -> Result<(), CoreError> {
        let (env, legacy) = self.accounts("account.opening_equity")?;
        crate::account::config::env_persist_account_opening_equity(
            &legacy, &env, account_id, equity, at_ms,
        )
    }

    fn save_risk_bounds(
        &self,
        account_id: Option<&str>,
        fields: &FieldChanges,
    ) -> Result<bool, CoreError> {
        let Some(env) = self.env.as_ref() else {
            return Ok(false);
        };
        let is_default = account_id
            .map(|id| id == crate::model::default_account_id().as_str())
            .unwrap_or(true);
        let vars: BTreeMap<String, String> = fields
            .iter()
            .filter_map(|(field, value)| {
                let key = if is_default {
                    crate::cockpit::env_key_for_field(field).map(str::to_string)
                } else {
                    crate::cockpit::env_key_for_field_account(field, account_id.unwrap_or(""))
                };
                key.map(|k| (k, value.clone()))
            })
            .collect();
        // The error is the carrier's own (the io failure text); every
        // caller wraps it with its own "persist failed" wording, byte-equal
        // to the pre-seam messages.
        crate::env_file::upsert_env_vars(env, &vars)
            .map(|_| true)
            .map_err(|e| CoreError::new(crate::model::CoreErrorCode::Internal, e.to_string()))
    }

    fn save_exit(
        &self,
        account_id: Option<&str>,
        fields: &FieldChanges,
    ) -> Result<bool, CoreError> {
        // The exit write path's own tail (trading_reset.rs apply_exit_set)
        // spells the keys through the SAME cockpit table, then upserts and
        // re-derives — the re-derive (working-copy refresh) stays in the
        // service layer; the store owns the carrier only.
        let Some(env) = self.env.as_ref() else {
            return Ok(false);
        };
        let is_default = account_id
            .map(|id| id == crate::model::default_account_id().as_str())
            .unwrap_or(true);
        let vars: BTreeMap<String, String> = fields
            .iter()
            .filter_map(|(field, value)| {
                let key = if is_default {
                    crate::cockpit::env_key_for_field(field).map(str::to_string)
                } else {
                    crate::cockpit::env_key_for_field_account(field, account_id.unwrap_or(""))
                        .or_else(|| crate::cockpit::env_key_for_field(field).map(str::to_string))
                };
                key.map(|k| (k, value.clone()))
            })
            .collect();
        crate::env_file::upsert_env_vars(env, &vars)
            .map(|_| true)
            .map_err(|e| CoreError::new(crate::model::CoreErrorCode::Internal, e.to_string()))
    }

    fn save_arb_config(&self, cfg: &CrossVenueArbConfig) -> Result<(), CoreError> {
        let (env, legacy) = self.accounts("arb.config.set")?;
        crate::account::config::env_persist_cross_venue_arb(&legacy, &env, cfg)
    }

    fn clear_arb_config(&self) -> Result<(), CoreError> {
        let (env, legacy) = self.accounts("arb.config.clear")?;
        crate::account::config::env_clear_cross_venue_arb(&legacy, &env)
    }

    fn save_policy_section(
        &self,
        account_id: &str,
        state: &BTreeMap<String, Option<String>>,
    ) -> Result<(), String> {
        let env = self.env.as_ref().ok_or_else(|| {
            "execution policy: no .env path configured — persistence is \
             disabled for this core (backtests and in-process tests); \
             nothing was applied"
                .to_string()
        })?;
        // The legacy-toml refusal + the bootstrap-template sweep, exactly as
        // the old write face ran them (policy_section_write).
        if let Some(policy_path) = self.policy_legacy.as_deref() {
            let path = std::path::Path::new(policy_path);
            if path.exists() {
                let is_kernel_template = std::fs::read_to_string(path)
                    .map(|text| text == crate::execution_policy::BOOTSTRAP_TEMPLATE)
                    .unwrap_or(false);
                if is_kernel_template {
                    if let Err(e) = std::fs::remove_file(path) {
                        // The template is inert, so a failed sweep must not
                        // block the operator's write — warn and carry on.
                        tracing::warn!(
                            path = %policy_path,
                            error = %e,
                            "could not sweep the auto-generated execution_policy.toml \
                             template — proceeding with the .env write (the file is inert)"
                        );
                    }
                } else {
                    return Err(format!(
                        "refusing the policy write: the legacy {policy_path} still exists — \
                         run scripts/migrate-config-to-env.sh first (issue 751/#967: one source; \
                         the loader reads POLICY_* env only, so this file is dead data)"
                    ));
                }
            }
        }
        // `reset` of a section that is not ON FILE refuses by name (the
        // fat-finger contract): the keys are the section's whole footprint,
        // none present = nothing to reset. The file face is the truth here —
        // a section that lives only in the process environment is
        // launcher-owned and this write face never claimed it. A reset is
        // the all-`None` footprint (the section's fields all swept).
        if state.values().all(Option::is_none) {
            let on_file: std::collections::HashMap<String, String> = std::fs::read_to_string(env)
                .unwrap_or_default()
                .lines()
                .filter_map(crate::env_file::parse_line)
                .collect();
            let present = state.keys().any(|k| on_file.contains_key(k));
            if !present {
                return Err(format!(
                    "execution policy: no section for account `{account_id}` to reset"
                ));
            }
        }
        // Sweep-then-write: remove EVERY section key, then upsert the
        // `Some` fields — absent = key gone = inherit from the globals.
        let sweep_refs: Vec<&str> = state.keys().map(|k| k.as_str()).collect();
        crate::env_file::remove_env_vars(env, &sweep_refs)
            .map_err(|e| format!("policy env sweep failed: {e}"))?;
        let vars: BTreeMap<String, String> = state
            .iter()
            .filter_map(|(k, v)| v.clone().map(|v| (k.clone(), v)))
            .collect();
        crate::env_file::upsert_env_vars(env, &vars)
            .map_err(|e| format!("policy env write failed: {e}"))?;
        Ok(())
    }

    fn save_account_round_duration(
        &self,
        account_id: &str,
        seconds: Option<i64>,
    ) -> Result<(), CoreError> {
        let env = self.env.as_ref().ok_or_else(|| {
            CoreError::new(
                crate::model::CoreErrorCode::InvalidParams,
                "round_duration.set: no env_file_path configured — persistence \
                     is disabled for this core (backtests and in-process tests)"
                    .to_string(),
            )
        })?;
        let sfx: String = account_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        let key = format!("ACCOUNT_{sfx}_ROUND_DURATION_SEC");
        match seconds {
            None => {
                crate::env_file::remove_env_vars(env, &[key.as_str()]).map_err(|e| {
                    CoreError::new(
                        crate::model::CoreErrorCode::InvalidParams,
                        format!("round_duration.set: persist reset failed: {e}"),
                    )
                })?;
            }
            Some(sec) => {
                crate::env_file::upsert_env_vars(env, &BTreeMap::from([(key, sec.to_string())]))
                    .map_err(|e| {
                        CoreError::new(
                            crate::model::CoreErrorCode::InvalidParams,
                            format!("round_duration.set: persist failed, nothing changed: {e}"),
                        )
                    })?;
            }
        }
        Ok(())
    }

    fn save_credentials(
        &self,
        vars: &BTreeMap<String, String>,
        reset_keys: &[&str],
    ) -> Result<(), CoreError> {
        let env = self.env("market.credentials.set")?;
        if !reset_keys.is_empty() {
            crate::env_file::remove_env_vars(env, reset_keys).map_err(|e| {
                CoreError::new(
                    crate::model::CoreErrorCode::InvalidParams,
                    format!("market.credentials.set: .env write failed, nothing changed: {e}"),
                )
            })?;
        } else {
            crate::env_file::upsert_env_vars(env, vars).map_err(|e| {
                CoreError::new(
                    crate::model::CoreErrorCode::InvalidParams,
                    format!("market.credentials.set: .env write failed, nothing changed: {e}"),
                )
            })?;
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The in-memory test backend (PR-1 scope: NOT a production backend —
    /// tests that want to observe writes without touching a file implement
    /// the trait over a recording map). Every method accepts and records;
    /// the refusals the EnvStore owns are not reproduced here (they are
    /// covered by the EnvStore's own tests).
    #[derive(Default)]
    struct MemStore {
        accounts_enabled: bool,
        writes: std::sync::Mutex<Vec<String>>,
    }

    impl MemStore {
        fn recording(accounts_enabled: bool) -> Self {
            Self {
                accounts_enabled,
                writes: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn log(&self, what: String) {
            self.writes.lock().unwrap().push(what);
        }

        fn writes(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl StoreApi for MemStore {
        fn account_writes_enabled(&self) -> bool {
            self.accounts_enabled
        }

        fn save_account_venue(&self, account_id: &str, venue: &str) -> Result<(), CoreError> {
            self.log(format!("venue:{account_id}={venue}"));
            Ok(())
        }

        fn save_account_create(
            &self,
            account_id: &str,
            name: Option<&str>,
            venue: Option<&str>,
            credential_keys: &CredentialKeys,
        ) -> Result<(), CoreError> {
            self.log(format!(
                "create:{account_id}/{}/{}/{}/{}",
                name.unwrap_or(""),
                venue.unwrap_or(""),
                credential_keys.api_key_env.as_deref().unwrap_or(""),
                credential_keys.secret_env.as_deref().unwrap_or("")
            ));
            Ok(())
        }

        fn remove_account(&self, account_id: &str) -> Result<(), CoreError> {
            self.log(format!("remove:{account_id}"));
            Ok(())
        }

        fn save_account_trading(&self, account_id: &str, enabled: bool) -> Result<(), CoreError> {
            self.log(format!("trading:{account_id}={enabled}"));
            Ok(())
        }

        fn save_account_status(
            &self,
            account_id: &str,
            status: &AccountStatus,
        ) -> Result<(), CoreError> {
            self.log(format!("status:{account_id}={status}"));
            Ok(())
        }

        fn save_account_active(&self, account_id: &str) -> Result<(), CoreError> {
            self.log(format!("active:{account_id}"));
            Ok(())
        }

        fn save_display_account(&self, account_id: &str) -> Result<(), CoreError> {
            self.log(format!("display:{account_id}"));
            Ok(())
        }

        fn save_account_strategies(
            &self,
            account_id: &str,
            strategies: &[String],
        ) -> Result<(), CoreError> {
            self.log(format!("strategies:{account_id}={}", strategies.join(",")));
            Ok(())
        }

        fn save_account_rounds(&self, account_id: &str, rounds: &[i64]) -> Result<(), CoreError> {
            self.log(format!(
                "rounds:{account_id}={}",
                rounds
                    .iter()
                    .map(|r| r.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            Ok(())
        }

        fn save_account_opening_equity(
            &self,
            account_id: &str,
            equity: Decimal,
            at_ms: i64,
        ) -> Result<(), CoreError> {
            self.log(format!("equity:{account_id}={equity}@{at_ms}"));
            Ok(())
        }

        fn save_risk_bounds(
            &self,
            account_id: Option<&str>,
            fields: &FieldChanges,
        ) -> Result<bool, CoreError> {
            self.log(format!(
                "risk:{}={}",
                account_id.unwrap_or("default"),
                fields
                    .iter()
                    .map(|(f, v)| format!("{f}={v}"))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            Ok(true)
        }

        fn save_exit(
            &self,
            account_id: Option<&str>,
            fields: &FieldChanges,
        ) -> Result<bool, CoreError> {
            self.log(format!(
                "exit:{}={}",
                account_id.unwrap_or("default"),
                fields
                    .iter()
                    .map(|(f, v)| format!("{f}={v}"))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            Ok(true)
        }

        fn save_arb_config(&self, cfg: &CrossVenueArbConfig) -> Result<(), CoreError> {
            self.log(format!("arb:{}/{}", cfg.leg_a_venue, cfg.leg_b_venue));
            Ok(())
        }

        fn clear_arb_config(&self) -> Result<(), CoreError> {
            self.log("arb-clear".to_string());
            Ok(())
        }

        fn save_policy_section(
            &self,
            account_id: &str,
            state: &BTreeMap<String, Option<String>>,
        ) -> Result<(), String> {
            self.log(format!(
                "policy:{account_id}:{}",
                state
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.as_deref().unwrap_or("<swept>")))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            Ok(())
        }

        fn save_account_round_duration(
            &self,
            account_id: &str,
            seconds: Option<i64>,
        ) -> Result<(), CoreError> {
            self.log(format!(
                "round-duration:{account_id}={}",
                seconds.map(|s| s.to_string()).unwrap_or("reset".into())
            ));
            Ok(())
        }

        fn save_credentials(
            &self,
            vars: &BTreeMap<String, String>,
            reset_keys: &[&str],
        ) -> Result<(), CoreError> {
            self.log(format!(
                "credentials:{:?}/{:?}",
                vars.keys().collect::<Vec<_>>(),
                reset_keys
            ));
            Ok(())
        }
    }

    #[test]
    fn mem_store_records_every_domain_write() {
        let s = MemStore::recording(true);
        assert!(s.account_writes_enabled());
        s.save_account_venue("paper", "kalshi").unwrap();
        s.save_account_trading("paper", false).unwrap();
        s.save_account_status("paper", &AccountStatus::Frozen)
            .unwrap();
        s.save_account_active("paper").unwrap();
        s.save_display_account("paper").unwrap();
        s.save_account_strategies("paper", &["spread_arb".into()])
            .unwrap();
        s.save_account_rounds("paper", &[900, 3600]).unwrap();
        s.save_account_opening_equity("paper", Decimal::new(1234, 2), 7)
            .unwrap();
        s.save_risk_bounds(
            Some("paper"),
            &vec![("maxSingleLossUsd".into(), "25".into())],
        )
        .unwrap();
        s.save_exit(Some("paper"), &vec![("stopLossPct".into(), "5".into())])
            .unwrap();
        s.save_arb_config(&CrossVenueArbConfig {
            leg_a_venue: "kalshi".into(),
            leg_a_account: "paper".into(),
            leg_b_venue: "polymarket".into(),
            leg_b_account: "paper".into(),
            pairs: vec!["BTC".into()],
        })
        .unwrap();
        s.clear_arb_config().unwrap();
        s.save_policy_section(
            "paper",
            &BTreeMap::from([("POLICY_BUDGET_RATIO_PAPER".to_string(), Some("0.1".into()))]),
        )
        .unwrap();
        s.save_account_round_duration("paper", Some(3600)).unwrap();
        s.save_account_round_duration("paper", None).unwrap();
        s.save_credentials(
            &BTreeMap::from([("KALSHI_API_KEY".to_string(), "k".into())]),
            &[],
        )
        .unwrap();
        let w = s.writes();
        assert_eq!(w.len(), 16, "one line per domain write: {w:?}");
        assert_eq!(w[0], "venue:paper=kalshi");
        assert_eq!(w[1], "trading:paper=false");
        assert_eq!(w[2], "status:paper=frozen");
        assert_eq!(w[3], "active:paper");
        assert_eq!(w[4], "display:paper");
        assert_eq!(w[5], "strategies:paper=spread_arb");
        assert_eq!(w[6], "rounds:paper=900,3600");
        assert_eq!(w[7], "equity:paper=12.34@7");
        assert_eq!(w[8], "risk:paper=maxSingleLossUsd=25");
        assert_eq!(w[9], "exit:paper=stopLossPct=5");
        assert_eq!(w[10], "arb:kalshi/polymarket");
        assert_eq!(w[11], "arb-clear");
        assert_eq!(w[12], "policy:paper:POLICY_BUDGET_RATIO_PAPER=0.1");
        assert_eq!(w[13], "round-duration:paper=3600");
        assert_eq!(w[14], "round-duration:paper=reset");
        assert_eq!(w[15], "credentials:[\"KALSHI_API_KEY\"]/[]");
    }

    #[test]
    fn mem_store_can_report_persistence_disabled() {
        let s = MemStore::recording(false);
        assert!(!s.account_writes_enabled());
    }

    /// The EnvStore's account-write refusal when the core has no account
    /// book carrier — the pre-seam "persistence is disabled" bytes.
    #[test]
    fn env_store_refuses_account_writes_without_a_book_carrier() {
        let s = EnvStore::new(None, None, None);
        assert!(!s.account_writes_enabled());
        let err = s.save_account_venue("paper", "kalshi").unwrap_err();
        assert_eq!(err.code, crate::model::CoreErrorCode::InvalidParams);
        assert!(
            err.message.contains("no accounts config path configured"),
            "{err}"
        );
    }

    /// The cockpit writes report `Ok(false)` (not an error) when the core
    /// has no env carrier — the memory-only posture the callers' `persisted`
    /// flag exists for.
    #[test]
    fn env_store_risk_and_exit_report_memory_only_without_a_carrier() {
        let s = EnvStore::new(None, None, None);
        let no_fields: FieldChanges = Vec::new();
        assert!(!s.save_risk_bounds(None, &no_fields).unwrap());
        assert!(!s.save_exit(None, &no_fields).unwrap());
    }
}
