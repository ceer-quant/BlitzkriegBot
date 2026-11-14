//! Core assembly: the constructors, boot-time account/policy loading, the
//! execution-policy surface (IPC + reload) and the event sink. Pure code
//! motion from `service.rs` (#780) — no logic edits.

use super::*;
use crate::account::AccountLedgers;
use std::collections::HashMap;

impl Core {
    /// Open the per-account risk/exit store (issue #1189) and overlay its
    /// rows onto the working copies. The env-derived rows loaded at boot stay
    /// in place; the store is the stronger source, so its rows win. A missing
    /// file is an empty store (the env fallback stays). Idempotent: a second
    /// open replaces the first store and re-reads it.
    ///
    /// issue #1199 (the readout asymmetry, half 1): the DEFAULT account's DB
    /// rows are mirrored into the global faces (`systemic.account` /
    /// `positions.exit`) exactly like the write path's mirror
    /// (`install_account_risk_row` / `install_account_exit_row`). The
    /// readout's account section and the no-own-row fallback resolve against
    /// the global face, so without this mirror a DB-sourced default row was
    /// invisible to `risk.limits` while the engine already judged by it — the
    /// "write DB, read env" divergence in its second spelling.
    pub fn open_account_risk_db(
        &mut self,
        path: impl Into<std::path::PathBuf>,
    ) -> Result<(), String> {
        // 装配已经打开过库（生产路径，`config.db_path`）→ 不再二次打开：
        // 同一个句柄已经在位，重复 open 只会制造第二个连接。
        if self.account_risk_db.is_some() {
            return Ok(());
        }
        let db = crate::risk::account_store::AccountRiskDb::open(path)
            .map_err(|e| format!("account risk store open failed: {e}"))?;
        self.install_risk_db(db)
    }

    /// 把一个已打开的 `AccountRiskDb` 安装进核心：风控行、退出纪律、账户
    /// 设置（策略集合 / 轮次锚点）与进化开关句柄，全部来自同一连接。
    /// 生产装配（`with_account_book`）与测试入口（`open_account_risk_db`）
    /// 共用这一段——overlay 语义只有一份。
    fn install_risk_db(
        &mut self,
        db: crate::risk::account_store::AccountRiskDb,
    ) -> Result<(), String> {
        // issue #1199 P1-1: a risk read failure is never silent-grown into an
        // empty map (the boot chain would read that as "use the env global"
        // and keep trading with widened limits). Fail-closed: refuse boot.
        let risks = db.load_risk()?;
        let exits = db.load_exits(&self.config.positions.exit)?;
        let default_id = crate::model::default_account_id();
        if let Some(row) = risks.get(default_id.as_str()) {
            self.risk.config_mut().systemic.account = row.clone();
        }
        if let Some(exit) = exits.get(default_id.as_str()) {
            self.config.positions.exit = exit.clone();
        }
        self.risk.config_mut().systemic.accounts.extend(risks);
        self.config.positions.account_exit.extend(exits);
        self.positions.set_config(self.config.positions.clone());
        // 操作者裁决 2026-11-14：轮次锚点的家是数据库，不是 `.env`。库里有行
        // 就以行为准（`.env` 的 `ACCOUNT_*_ROUND_DURATION_SEC` 只是旧部署的
        // 兼容回退）；写入面见 `round_duration.set`。这一层不读 env：env 已由
        // `with_account_book` 先铺好，行覆盖它。
        self.overlay_account_settings_from_db(&db)?;
        // 进化引擎的两个开关也是「库里的行才是真相」：把句柄交给引擎，
        // 运行时翻转直接落 `kernel_meta`，不再写 `.env`。
        self.shadow_evolution.attach_settings_db(db.clone());
        self.account_risk_db = Some(db);
        Ok(())
    }

    /// Overlay the `account_settings` rows onto the in-memory book:
    /// per-account round anchors (and, for the default account, the process
    /// default grid) plus the strategy enablement sets.
    ///
    /// Fail-closed on a value that would change the trading grid into
    /// something no plugin declared or the account did not subscribe to:
    /// the same refusal the env grammar applies, so a stale row can never
    /// arm a grid the operator did not choose. A merely unparseable row is a
    /// warn-and-skip (the env value stands) — one bad cell must not brick a
    /// boot, and it cannot widen anything.
    fn overlay_account_settings_from_db(
        &mut self,
        db: &crate::risk::account_store::AccountRiskDb,
    ) -> Result<(), String> {
        use crate::risk::account_store::ROUND_DURATION_SETTING;
        let settings = db
            .load_settings()
            .map_err(|e| format!("account_settings read failed: {e}"))?;
        let default_id = crate::model::default_account_id();
        for (account_id, keys) in &settings {
            let Some(raw) = keys.get(ROUND_DURATION_SETTING) else {
                continue;
            };
            let sec: i64 = match raw.trim().parse() {
                Ok(sec) => sec,
                Err(_) => {
                    tracing::warn!(
                        account = %account_id,
                        value = %raw,
                        "account_settings round anchor is not an integer — the boot-time \
                         env value stands for this account"
                    );
                    continue;
                }
            };
            if !self.config.allowed_round_secs.contains(&sec) {
                return Err(format!(
                    "refusing to boot: account `{account_id}`'s stored round anchor {sec}s is \
                     not declared by any market plugin (declared union: {:?}) — fix the \
                     account_settings row or add the duration to a plugin",
                    self.config.allowed_round_secs
                ));
            }
            let subscribed = self
                .accounts
                .account(&blitzkrieg_market_api::AccountId::from(account_id.as_str()))
                .map(|a| a.rounds.clone())
                .unwrap_or_default();
            if !subscribed.is_empty() && !subscribed.contains(&sec) {
                return Err(format!(
                    "refusing to boot: account `{account_id}`'s stored round anchor {sec}s is \
                     not in its subscribed round families ({subscribed:?}) — fix the \
                     account_settings row or the account's ROUNDS"
                ));
            }
            if account_id == default_id.as_str() {
                // 显式 `--round-sec` / `BK_ROUND_SEC` 仍然压过持久格（文档合同），
                // 这时只保留 per-account 行、不动进程默认网格。
                if self.config.round_duration_source == "cli_or_env" {
                    tracing::warn!(
                        stored = sec,
                        effective = self.config.default_round_duration_sec,
                        "account_settings holds a default-account round anchor, but the grid                          is locked by an explicit CLI/env flag — the flag stands"
                    );
                } else {
                    self.config.default_round_duration_sec = sec;
                    // 读数的来源如实改名：这一格来自库里的行，不是出厂默认。
                    self.config.round_duration_source = "account_settings";
                }
            }
            self.config
                .account_round_durations
                .insert(account_id.clone(), sec);
        }
        // 策略集合：行存在即为真相（显式空数组 = 本账户什么都不跑）。
        for (account_id, keys) in &settings {
            let Some(raw) = keys.get(crate::risk::account_store::STRATEGIES_SETTING) else {
                continue;
            };
            match serde_json::from_str::<Vec<String>>(raw) {
                Ok(set) => {
                    let id = blitzkrieg_market_api::AccountId::from(account_id.as_str());
                    if let Err(e) = self.accounts.set_strategies(&id, set, crate::db::now_ms()) {
                        tracing::warn!(
                            account = %account_id,
                            error = %e.message,
                            "account_settings strategies row could not be applied — \
                             the boot-time env spelling stands for this account"
                        );
                    }
                }
                Err(e) => tracing::warn!(
                    account = %account_id,
                    error = %e,
                    "account_settings strategies row is not a JSON array — the boot-time \
                     env spelling stands for this account"
                ),
            }
        }
        Ok(())
    }

    /// True when a durable per-account store is open. `false` = env fallback.
    pub fn account_risk_db_open(&self) -> bool {
        self.account_risk_db.is_some()
    }

    /// The open durable store handle (`None` = no store on this core). The
    /// session boot uses it for the credential-row injection.
    pub fn account_risk_db_handle(
        &self,
    ) -> Option<crate::risk::account_store::AccountRiskDb> {
        self.account_risk_db.clone()
    }

    /// issue #1199 R2: whether the OPEN database carries no data at all —
    /// zero `account_risk` rows AND no `import_v1` marker. Only meaningful
    /// with a store open (an env-fallback core answers `false`: there is no
    /// database to be empty). A read error answers `false` as well — the
    /// probe must never turn a diagnostic into a boot failure, and the WARN
    /// it feeds is a diagnostic.
    pub fn account_risk_db_is_empty(&self) -> bool {
        self.account_risk_db
            .as_ref()
            .and_then(|db| db.db().is_empty().ok())
            .unwrap_or(false)
    }

    /// issue #1199 (the env watcher asymmetry): whether the env-file watcher
    /// may re-derive the cockpit working copies from `.env`. The DB is the
    /// cockpit carrier when one is open — a file re-derive would clobber the
    /// DB-sourced working copies with the file's stale `BK_*` / `BK_EXIT_*`
    /// keys on ANY unrelated `.env` write (an account toggle, a credential
    /// save). The re-derive is the env-carrier backend's freshness mechanism
    /// only; with the DB open the panel write path (the store seam) is the
    /// way in, and env is read-only at runtime (issue #1199 ruling).
    pub(crate) fn cockpit_env_rederive_allowed(&self) -> bool {
        self.account_risk_db.is_none()
    }

    /// issue #1189: `budget_ratio = 1` lets the execution policy size an order
    /// at 100% of the account's available balance, while
    /// `BK_MAX_ORDER_NOTIONAL_PCT < 100` refuses any single order above that
    /// share of equity. The two are individually legal and jointly a guaranteed
    /// per-order refusal at the exact size the policy asked for, so the save
    /// path names the conflict instead of letting the operator discover it as
    /// a rejected order.
    pub(crate) fn warn_if_budget_ratio_conflicts(&self, account_id: &str) {
        let Some(policy) = self.execution_policy.as_ref() else {
            return;
        };
        let ratio = policy.section_for(account_id).budget_ratio;
        let pct = self.risk.config().max_order_notional_pct;
        if ratio == Decimal::ONE && pct < Decimal::from(100) {
            tracing::warn!(
                target: "risk",
                account = %account_id,
                budget_ratio = %ratio,
                max_order_notional_pct = %pct,
                "budget_ratio=1.0 conflicts with max_order_notional_pct<100: the policy will size an \
                 order at 100% of available balance, then the per-order equity cap will refuse it. \
                 Raise BK_MAX_ORDER_NOTIONAL_PCT to 100 or lower the account's budget_ratio."
            );
        }
    }

    pub fn new(config: CoreConfig) -> Self {
        // E28 (§9.5) + issue 751 (.env single source) + issue 923 (the
        // migration window closed): the account book loads through the
        // ACCOUNT_* env grammar only — process env first, then the `.env`
        // file — with NO legacy toml fallback. No ACCOUNT_* keys anywhere is
        // the zero-config deployment: one `default` account. A legacy
        // accounts.toml still on disk is never read for configuration; it is
        // only a residue whose existence the loader warns at. A malformed
        // source refuses the boot (fail-closed: a book that is not what the
        // operator wrote must not trade). The credential probe runs here too,
        // so `account.list` answers `credentialsLoaded` from boot-time facts.
        // Tests (and embedders) inject a book directly via
        // [`Core::with_account_book`] instead of touching the cwd.
        let accounts = crate::account::config::load_account_ledgers_single_source(
            std::path::Path::new(crate::account::config::ACCOUNTS_CONFIG_PATH),
            config.env_file_path.as_deref().map(std::path::Path::new),
            now_ms(),
            &config.allowed_round_secs,
        )
        .unwrap_or_else(|e| {
            panic!(
                "refusing to boot: {} (fail-closed: fix the account source and restart)",
                e.message
            )
        });
        Self::with_account_book(config, accounts)
    }

    /// The constructor with the account book SUPPLIED (E28): a multi-account
    /// book for tests without touching the operator's `.env` account rows the
    /// process cwd decides. Production always enters through [`Core::new`],
    /// which fails closed on a malformed source; this variant trusts the
    /// caller's book.
    pub fn with_account_book(
        mut config: CoreConfig,
        mut accounts: crate::account::AccountLedgers,
    ) -> Self {
        // Issue 1140: per-account round duration. Parse ACCOUNT_<ID>_ROUND_DURATION_SEC
        // for each configured account and validate against ACCOUNT_<ID>_ROUNDS
        // (subscription allow-list). Fail-closed: if an account's anchor duration
        // is not in its allow-list, refuse the boot.
        let env_file_path = config.env_file_path.clone();
        // issue #1199 第六轮（操作者裁决 2026-11-14）：`.env` 对内核是
        // **只读**的——唯一的持久化后端是 SQLite（`config.db_path`，session
        // 在生产里设为 `db::default_db_path()`）。打开失败 fail-closed：
        // 拒绝启动，绝不静默降级成无存储继续跑。`None` = 回测/多数进程内
        // 测试：每个写面如实拒绝，绝不写 `.env`。
        let sqlite = config.db_path.as_ref().map(|path| {
            crate::store::SqliteStore::open(path).unwrap_or_else(|e| {
                panic!(
                    "refusing to boot: SQLite store open failed ({e}) — fix the \
                     database path and restart; a kernel without its durable \
                     store must not trade"
                )
            })
        });
        let store: Option<Box<dyn crate::store::StoreApi>> = sqlite
            .as_ref()
            .map(|s| Box::new(s.clone()) as Box<dyn crate::store::StoreApi>);
        // 账户簿的家（同裁决）：`accounts` 表非空 → 整簿从行重建，SQL 是
        // 权威（`.env` 的 ACCOUNT_* 只是迁移前旧部署的兼容回退——读允许，
        // 写路径已整体废除）。空表 → 沿用 env 簿（首次升级后的状态），此后
        // 任何账户写面落库即切换。库里带的轮次锚点同时压过 env 键：
        // `sql_round_anchors` 告诉下面的 env 锚点循环跳过这些账户（否则一条
        // 陈旧的 env 键会把「库里权威」炸回「fail-closed 拒启」）。
        let sql_round_anchors: std::collections::HashSet<String> = if let Some(s) = &sqlite {
            let rows = s.risk_db().load_accounts().unwrap_or_else(|e| {
                panic!(
                    "refusing to boot: accounts table read failed ({e}) — the \
                     durable book must be readable or the kernel must not trade"
                )
            });
            let settings = s
                .risk_db()
                .load_settings()
                .unwrap_or_else(|e| panic!("refusing to boot: account_settings read failed ({e})"));
            let anchors: std::collections::HashSet<String> = settings
                .into_iter()
                .filter(|(_, keys)| {
                    keys.contains_key(crate::risk::account_store::ROUND_DURATION_SETTING)
                })
                .map(|(id, _)| id)
                .collect();
            if !rows.is_empty() {
                accounts = Self::ledger_book_from_rows(&rows, s.risk_db(), now_ms());
            } else {
                // 首次升级 boot：`accounts` 表空、簿来自只读 `.env`（兼容
                // 回退）。物化：整簿写进 SQL —— 从这次 boot 起 `accounts`
                // 表就是簿的家，运行时每个账户写面（venue/rounds/status/
                // remove…）都有行可写。只读 env、写 SQL，同一裁决的两半。
                // Fail-closed：物化写失败 = 库写不进 = 拒绝启动（与 store
                // open 失败同一姿态：没有持久家的内核不交易）。
                for acc in accounts.account_list() {
                    use crate::account::AccountStatus;
                    let status = match &acc.status {
                        AccountStatus::Active => "active",
                        AccountStatus::ReadOnly => "read_only",
                        AccountStatus::Frozen => "frozen",
                        AccountStatus::Suspended { .. } => "suspended",
                    };
                    if !matches!(
                        acc.market_type,
                        blitzkrieg_market_api::MarketType::Prediction
                    ) {
                        panic!(
                            "refusing to boot: env account `{}` carries a non-prediction \
                             market type — the env book can only spell prediction accounts \
                             (fail-closed: the durable book must parse or the kernel must \
                             not trade)",
                            acc.id
                        );
                    }
                    let rounds_json = serde_json::to_string(&acc.rounds)
                        .unwrap_or_else(|e| {
                            panic!(
                                "refusing to boot: env account `{}` rounds do not render \
                                 as JSON ({e})",
                                acc.id
                            )
                        });
                    let row = crate::risk::account_store::AccountRow {
                        id: acc.id.as_str().to_string(),
                        name: acc.name.clone(),
                        market_type: "prediction".to_string(),
                        status: status.to_string(),
                        venue: acc.venue.clone(),
                        trading_enabled: acc.trading_enabled,
                        rounds_json,
                        opening_equity: acc.opening_equity.map(|d| d.to_string()),
                        updated_at_ms: if acc.updated_at_ms > 0 {
                            acc.updated_at_ms
                        } else {
                            now_ms()
                        },
                    };
                    s.risk_db().save_account_row(&row).unwrap_or_else(|e| {
                        panic!(
                            "refusing to boot: env book materialization failed for `{}` \
                             ({e}) — the durable book must be writable or the kernel \
                             must not trade",
                            acc.id
                        )
                    });
                    let ck = &acc.credential_keys;
                    if ck.api_key_env.is_some()
                        || ck.secret_env.is_some()
                        || ck.passphrase_env.is_some()
                        || ck.private_key_env.is_some()
                    {
                        s.risk_db()
                            .save_credential_names(acc.id.as_str(), ck)
                            .unwrap_or_else(|e| {
                                panic!(
                                    "refusing to boot: credential names materialization \
                                     failed for `{}` ({e})",
                                    acc.id
                                )
                            });
                    }
                    tracing::info!(
                        account = %acc.id,
                        "env book materialized into SQL — the accounts table is the \
                         book's durable home from this boot on (env stays read-only)"
                    );
                }
            }
            anchors
        } else {
            Default::default()
        };
        let env_lookup = crate::account::config::env_lookup_from_sources(
            env_file_path.as_deref().map(std::path::Path::new),
        );
        // issue #1189: per-account risk/exit rows. The env face is the
        // fallback when the SQLite store is not open; the per-account keys
        // (`BK_RISK_*_<ACCOUNT>` / `BK_EXIT_*_<ACCOUNT>`) overlay the global
        // config already resolved by the boot chain. A row that exists wins;
        // an account without one keeps the global values (the historical
        // single-account behaviour).
        let account_ids: Vec<String> = accounts
            .account_list()
            .iter()
            .map(|a| a.id.to_string())
            .collect();
        config
            .risk
            .systemic
            .overlay_accounts_from_lookup(&account_ids, &env_lookup);
        config.positions.account_exit = crate::config::account_exit_overrides(
            &config.positions.exit,
            &account_ids,
            &env_lookup,
        );
        for acc in accounts.account_list() {
            let id = acc.id.as_str();
            // env_suffix: uppercase alphanumeric, fold everything else to '_'
            let sfx: String = id
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
            // 库里有这个账户的轮次锚点行 → 行是权威，env 键（读允许）不再
            // 参与校验：一条陈旧的 env 值不得否决库里的网格。
            if !sql_round_anchors.contains(id)
                && let Some(sec_str) = env_lookup(&key)
            {
                if let Ok(sec) = sec_str.trim().parse::<i64>() {
                    config.account_round_durations.insert(id.to_string(), sec);
                    // Validation: the anchor must be in the account's ROUNDS allow-list.
                    if !acc.rounds.is_empty() && !acc.rounds.contains(&sec) {
                        // Note: the message names the env suffix and the value
                        // only — the allow-list body itself is not echoed back
                        // (CodeQL cleartext-logging: no account payload lands
                        // in the log).
                        panic!(
                            "refusing to boot: ACCOUNT_{sfx}_ROUND_DURATION_SEC={sec}s \
                             is not in the account's ACCOUNT_{sfx}_ROUNDS allow-list \
                             (fail-closed: fix the value or add it to ROUNDS)"
                        );
                    }
                } else {
                    panic!(
                        "refusing to boot: ACCOUNT_{sfx}_ROUND_DURATION_SEC=`{sec_str}` is not an i64 \
                         (fail-closed: fix .env and restart)"
                    );
                }
            }
            // Issue #1189 (PR-D): per-account share band and market plugin.
            // ACCOUNT_<SFX>_MIN_SHARES / ACCOUNT_<SFX>_MAX_SHARES resolve the
            // account's band (`CoreConfig::share_band_for` prefers the row,
            // falls back to the global min_shares/max_shares);
            // ACCOUNT_<SFX>_MARKET_PLUGIN names the account's market plugin
            // (`CoreConfig::market_plugin_for` prefers the row, falls back to
            // the caller-supplied fallback). Fail-closed, same posture as the
            // round anchor above: a non-decimal value refuses the boot, and a
            // resolved band whose min exceeds its max refuses the boot.
            let mut account_min_shares: Option<Decimal> = None;
            let mut account_max_shares: Option<Decimal> = None;
            let min_shares_key = format!("ACCOUNT_{sfx}_MIN_SHARES");
            if let Some(raw) = env_lookup(&min_shares_key) {
                match raw.trim().parse::<Decimal>() {
                    Ok(v) => account_min_shares = Some(v),
                    Err(_) => panic!(
                        "refusing to boot: {min_shares_key}=`{raw}` is not a decimal \
                         (fail-closed: fix .env and restart)"
                    ),
                }
            }
            let max_shares_key = format!("ACCOUNT_{sfx}_MAX_SHARES");
            if let Some(raw) = env_lookup(&max_shares_key) {
                match raw.trim().parse::<Decimal>() {
                    Ok(v) => account_max_shares = Some(v),
                    Err(_) => panic!(
                        "refusing to boot: {max_shares_key}=`{raw}` is not a decimal \
                         (fail-closed: fix .env and restart)"
                    ),
                }
            }
            if account_min_shares.is_some() || account_max_shares.is_some() {
                let band_min = account_min_shares.unwrap_or(config.min_shares);
                let band_max = account_max_shares.unwrap_or(config.max_shares);
                if band_min > band_max {
                    panic!(
                        "refusing to boot: ACCOUNT_{sfx} share band [{band_min}, {band_max}] is \
                         incompatible (min > max) (fail-closed: fix the cells and restart)"
                    );
                }
            }
            if let Some(v) = account_min_shares {
                config.account_min_shares.insert(id.to_string(), v);
            }
            if let Some(v) = account_max_shares {
                config.account_max_shares.insert(id.to_string(), v);
            }
            let market_plugin_key = format!("ACCOUNT_{sfx}_MARKET_PLUGIN");
            if let Some(raw) = env_lookup(&market_plugin_key) {
                let plugin = raw.trim().to_string();
                if !plugin.is_empty() {
                    config.account_market_plugins.insert(id.to_string(), plugin);
                }
            }
        }
        drop(env_lookup);
        // #1170 (方案 A): per-strategy round-family bindings. The
        // STRATEGY_<NAME>_ROUND_DURATION_SEC grammar (issue 1140's ACCOUNT_
        // grammar applied to strategies) binds a package to ONE round family:
        // the env cell's middle section is the package name through
        // `crate::engine::strategy_env_fold` (uppercase alphanumeric, the rest
        // folded to `_` — the SAME fold registration stamps with, so a reload
        // or scope rebuild re-finds its binding without another push). Sources
        // are the SAME two the account parser reads — process env first, then
        // the `.env` file, process beating file per key (12-factor). A
        // non-i64 value refuses the boot (fail-closed, the account anchor's
        // posture); a value outside the plugins' declared round union refuses
        // while a union exists.
        {
            const PREFIX: &str = "STRATEGY_";
            const SUFFIX: &str = "_ROUND_DURATION_SEC";
            let mid_of = |key: &str| -> Option<String> {
                if key.len() > PREFIX.len() + SUFFIX.len()
                    && key.starts_with(PREFIX)
                    && key.ends_with(SUFFIX)
                {
                    Some(key[PREFIX.len()..key.len() - SUFFIX.len()].to_string())
                } else {
                    None
                }
            };
            let mut cells: Vec<(String, String)> = std::env::vars()
                .filter_map(|(k, v)| mid_of(&k).map(|mid| (mid, v)))
                .collect();
            if let Some(env_path) = config.env_file_path.as_deref()
                && let Ok(text) = std::fs::read_to_string(env_path)
            {
                for line in text.lines().filter_map(crate::env_file::parse_line) {
                    if let Some(mid) = mid_of(&line.0) {
                        // Process env beats the file line for the SAME
                        // key — the rule `env_lookup_from_sources` runs.
                        cells.retain(|(existing, _)| *existing != mid);
                        cells.push((mid, line.1));
                    }
                }
            }
            let mut families = std::collections::HashMap::new();
            for (mid, sec_str) in cells {
                let sec = match sec_str.trim().parse::<i64>() {
                    Ok(s) => s,
                    Err(_) => panic!(
                        "refusing to boot: STRATEGY_{mid}_ROUND_DURATION_SEC=`{sec_str}` \
                         is not an i64 (fail-closed: fix the cell and restart)"
                    ),
                };
                if !config.allowed_round_secs.is_empty()
                    && !config.allowed_round_secs.contains(&sec)
                {
                    panic!(
                        "refusing to boot: STRATEGY_{mid}_ROUND_DURATION_SEC={sec}s is not a \
                         declared round family (declared: {:?}; fail-closed: fix the value)",
                        config.allowed_round_secs
                    );
                }
                families.insert(mid, sec);
            }
            config.strategy_round_families = families;
        }
        // Honor the deprecated `round_duration_sec` field for backward
        // compatibility with existing test constructors. If it differs from
        // the Default value (900), propagate it to `default_round_duration_sec`
        // so the runtime code path picks it up.
        if config.round_duration_sec != 900 {
            config.default_round_duration_sec = config.round_duration_sec;
        }
        // Whether the boot below replayed durable cash journals (the server's
        // post-construction seed reads it — a replayed balance must not be
        // re-seeded over).
        let mut boot_replayed_journal = false;
        // #363: the execution policy loads fail-closed at boot. A missing
        // file is the zero-config deployment (code defaults, no account
        // sections); a file that exists but does not parse, misses a field
        // or carries an out-of-range value REFUSES the boot — and leaves one
        // `load-refused` audit line saying so (谁改的/何时/为何, the audit's
        // own first question). A config-path of `None` (backtester, most
        // tests) loads nothing: every default then equals the historical
        // hardcoded behaviour bit for bit.
        //
        // #380: the DEFAULT path in [`CoreConfig::default`] is a SPELLING,
        // not a location — the binary entry (`main.rs`) resolves it against
        // the install root before constructing the config, and logs the
        // resolution (found / neutral-kernel WARN) there. The library
        // honours whatever path it is handed, verbatim, so an embedder (and
        // every in-process test) keeps exactly the semantics it asked for.
        // 操作者裁决 2026-11-14：策略段的 SQL 行是 boot 的最上层——先取行，
        // 装载后叠加（file/env 是旧部署兼容，行是内核自己的新写入）。
        let sql_policy_settings = sqlite.as_ref().map(|s| {
            s.risk_db()
                .load_settings()
                .unwrap_or_else(|e| {
                    panic!("refusing to boot: account_settings read failed ({e})")
                })
        });
        let execution_policy = {
            let policy = Self::load_execution_policy_at_boot(&config);
            match (policy, sql_policy_settings.as_ref()) {
                (Some(mut p), Some(settings)) => {
                    if let Err(reason) = p.overlay_sql_settings(settings) {
                        panic!(
                            "refusing to boot: the SQLite policy sections do not validate \
                             ({reason}) — fix the rows and restart (fail-closed)"
                        );
                    }
                    Some(p)
                }
                (other, _) => other,
            }
        };
        // #363: the per-asset cap precedence — CLI flag > policy section >
        // compiled 1. The CLI already writes total `max_positions` straight
        // into `config.positions`; this mirrors that for per-asset, reading
        // the value back out of the LOADED policy so the file and the flag
        // cannot disagree silently. A config-path of `None` (or a missing
        // file, which loads the code defaults) leaves the compiled 1 — the
        // historical one-per-asset gate, byte for byte.
        let mut positions = config.positions.clone();
        if let Some(policy) = &execution_policy {
            positions.max_positions_per_asset = policy.defaults.max_positions_per_asset;
        }
        if let Some(v) = config.max_positions_per_asset {
            positions.max_positions_per_asset = v;
        }
        let trade_db = config
            .trade_log_path
            .as_ref()
            .map(crate::trade_db::TradeDb::new);
        let order_db = config
            .order_log_path
            .as_ref()
            .map(crate::order_db::OrderDb::new);
        let position_db = config
            .position_log_path
            .as_ref()
            .map(crate::position_db::PositionDb::new);
        // Both side-log paths are derived from the order log's directory when not
        // set explicitly: they belong to the same durable state, and a deployment
        // that persists orders wants its idempotency table and audit trail on
        // disk too. `order_log_path: None` (the backtester, most tests) means no
        // persistence for any of them.
        let side_log_path = |explicit: &Option<String>, name: &str| -> Option<String> {
            if let Some(p) = explicit {
                return Some(p.clone());
            }
            let dir = std::path::Path::new(config.order_log_path.as_deref()?).parent()?;
            Some(dir.join(name).to_string_lossy().to_string())
        };
        let applied_log =
            side_log_path(&config.applied_log_path, "applied-fills.jsonl").map(AppliedFillLog::new);
        // ④ 统一映射账本：有路径就加载持久态（候选→人工确认的映射跨重启
        // 存活），无路径（回测/进程内测试）就纯内存。自动配对只提议，
        // confirm 才写账本 —— ledger 的设计合同，接线必须遵守。
        let unified_ledger = match config.unified_ledger_path.as_deref() {
            Some(p) => blitzkrieg_market_api::unified::MappingLedger::load(std::path::Path::new(p)),
            None => blitzkrieg_market_api::unified::MappingLedger::new(),
        };
        let audit_log = side_log_path(&config.audit_log_path, "reconcile.jsonl");
        let settlement_log = side_log_path(&config.settlement_log_path, "settlements.jsonl");
        // #463: the close-credential journal rides the same derivation — a
        // deployment that persists the order log persists the credentials that
        // let a FAILED unwind one of its closes across a restart.
        let close_credential_log =
            side_log_path(&None, "close-credentials.jsonl").map(std::path::PathBuf::from);
        let settlement = SettlementBook::new(settlement_log.as_deref().map(std::path::Path::new));
        let shadow_cfg = {
            let mut c = ShadowEvolutionConfig {
                enabled: config.shadow_evolution_enabled,
                ..Default::default()
            };
            if let Some(t) = &config.shadow_evolution_tuning {
                if let Some(v) = t.min_sample_count {
                    c.min_sample_count = v;
                }
                if let Some(v) = t.min_win_rate_improvement {
                    c.min_win_rate_improvement = v;
                }
                if let Some(v) = t.min_profit_factor_improvement {
                    c.min_profit_factor_improvement = v;
                }
                if let Some(v) = t.min_observation_secs {
                    c.min_observation_secs = v;
                }
                if let Some(v) = t.cooldown_secs {
                    c.cooldown_secs = v;
                }
                if let Some(v) = t.variant_count {
                    c.variant_count = v;
                }
                if let Some(v) = t.evaluation_window_secs {
                    c.evaluation_window_secs = v;
                }
                if let Some(v) = t.max_gradient {
                    // Belt and braces: the caller clamps too, but this is the one
                    // place the lock could be widened, so it re-checks.
                    c.max_gradient = v.min(crate::config::MAX_GRADIENT_CEILING);
                }
                if let Some(v) = &t.audit_dir {
                    c.audit_dir = v.clone();
                }
                if let Some(v) = t.auto_evolve {
                    c.auto_evolve = v;
                }
                if let Some(v) = t.evolution_cycle_secs {
                    c.evolution_cycle_secs = v;
                }
                if let Some(v) = t.proposal_ttl_secs {
                    c.proposal_ttl_secs = v;
                }
                if let Some(v) = t.deep_dims {
                    c.deep_dims = v.max(1);
                }
            }
            // D-2: variant exits must replay the SAME policy the live position
            // manager runs, or the counterfactual is judged against an exit
            // mechanism the live path never uses.
            c.exit_cfg = config.positions.exit.clone();
            c
        };
        let breaker = LossBreakers::new(config.max_consecutive_losses, config.breaker_cooldown_sec);
        let positions = PositionManager::new(positions);
        // Optional market-data archive (P-1.3). A failure to open only disables
        // recording — trading must never be blocked by an archive path problem.
        let event_archive = config.event_archive_path.as_ref().and_then(|p| {
            let cap_bytes = config.event_archive_max_mb.saturating_mul(1024 * 1024);
            let rotate_bytes = config.event_archive_rotate_mb.saturating_mul(1024 * 1024);
            let min_free_bytes =
                config.event_archive_min_free_mb.saturating_mul(1024 * 1024);
            match crate::data_source::EventArchive::open_full(
                std::path::Path::new(p),
                cap_bytes,
                rotate_bytes,
                min_free_bytes,
            ) {
                Ok(a) => {
                    tracing::info!(
                        path = %p,
                        cap_mb = config.event_archive_max_mb,
                        rotate_mb = config.event_archive_rotate_mb,
                        min_free_mb = config.event_archive_min_free_mb,
                        "recording market-data archive"
                    );
                    Some(a)
                }
                Err(e) => {
                    tracing::warn!(path = %p, error = %e, "cannot open event archive; recording disabled");
                    None
                }
            }
        });
        // Durable cash journals (issue #941: data/ledger/<account>.journal.
        // live.jsonl / .journal.dry.jsonl): replay BEFORE seeding, THIS MODE'S
        // file only — the two modes are absolutely independent books, so a
        // dry boot never sees live's rows and vice versa. A deployment with a
        // journal on disk restores the opening balance from its last row and
        // skips the boot seed — re-seeding over a replay would reset the
        // balance to the seed and vaporize the盈亏 history across every
        // restart. A fresh side (no journal file for this mode — first boot
        // after the split) takes the seed_all path, bit for bit: lived
        // history is never carried across the dry/live boundary (operator
        // ruling: dry = seeded simulation cash, live = venue cash). The
        // pre-replay retirement folds the pre-#941 single-file era away
        // (DELETED outright — operator second-pass ruling: the pre-split era
        // was one book, its rows belong to no side and are never replayed). A
        // malformed journal line is skipped with one warn
        // (the jsonl::load contract — the trailing balance comes from the
        // last GOOD row); the file itself is append-only evidence, never
        // rewritten by this path.
        if let Some(dir) = config.ledger_journal_dir.as_deref() {
            let dir = std::path::Path::new(dir);
            let trading = config.mode.may_trade();
            if !trading {
                AccountLedgers::archive_legacy_journals(dir);
            }
            let replayed = accounts.any_journal_on_disk(trading, dir);
            // issue #941 (migration D's hard half): the first post-split boot
            // of the DRY (locally-settling) side drops the daily-loss ledger's
            // persisted day states to an empty envelope. The old file's
            // openingEquity numbers were measured against the OTHER book (the
            // $71.59 → $3.58 cap pollution observed in production); kept, they
            // would survive the upgrade because `reconcile_same_day_base`
            // re-anchors only TIGHTER — a bigger seed never overwrites a
            // smaller polluted base. Live side does not take this branch: its
            // basis is the venue's report and refreshes every sweep.
            if !trading
                && !replayed
                && let Some(daily_path) = config.positions.daily_pnl_path.as_deref()
            {
                DailyLossStore::reset_file_to_empty(daily_path);
                tracing::info!(
                    path = %daily_path,
                    "issue 941 migration: daily-loss day states reset for the dry side's first post-split boot"
                );
            }
            accounts.attach_journals(trading, dir);
            // E28 (§9.5): a locally-settling mode has no venue to reconcile
            // against; seed the local cash so the reserve/overspend gate is
            // meaningful for embedders that don't seed it. Uses the predicate,
            // not `== Dry`: the equality form is invisible to the compiler and
            // left ReadOnly funding at zero. EVERY account is seeded (§9.3:
            // the books are separate wallets, not one wallet with aliases);
            // with one account this is exactly the single `seed` 0.2 ran.
            // Skipped when a journal replayed: the balance is a boot-replay
            // fact now, not the seed. (issue #941: a mode's first boot after
            // the split starts from the seed — the other mode's journal is a
            // different book and never replays here.)
            if config.mode.settles_locally() && !replayed {
                accounts.seed_all(config.dry_seed_balance);
            }
            boot_replayed_journal = replayed;
        } else if config.mode.settles_locally() {
            // E28 (§9.5): a locally-settling mode has no venue to reconcile
            // against; seed the local cash so the reserve/overspend gate is
            // meaningful for embedders that don't seed it. Uses the predicate,
            // not `== Dry`: the equality form is invisible to the compiler and
            // left ReadOnly funding at zero. EVERY account is seeded (§9.3:
            // the books are separate wallets, not one wallet with aliases);
            // with one account this is exactly the single `seed` 0.2 ran.
            accounts.seed_all(config.dry_seed_balance);
        }
        let mut intent_audit = crate::arbitration::AuditSink::new(accounts.active_id().as_str());
        intent_audit.enabled = config.intent_audit_enabled;
        // 跨所套利弹窗 v2: the leg configuration reads from the SAME source
        // the book came from (issue 751: ACCOUNT_* env — process env, then
        // the `.env` file; issue 923 closed the migration window — the toml
        // fallback is gone with the account one). The parse layer validates
        // coherence (load_account_ledgers_from_env refuses an incoherent
        // section), so a read here after a successful book load cannot fail
        // semantically — "unconfigured" is the honest answer when no keys
        // are present.
        let arb_leg_config = crate::account::config::read_cross_venue_arb_from_env(
            &crate::account::config::env_lookup_from_sources(
                config.env_file_path.as_deref().map(std::path::Path::new),
            ),
        )
        .map_err(|e| {
            tracing::warn!(
                error = %e.message,
                "cross_venue_arb env section invalid at boot — running unconfigured"
            );
        })
        .ok()
        .flatten()
        .inspect(|cfg| {
            tracing::info!(
                leg_a = %format!("{}/{}", cfg.leg_a_venue, cfg.leg_a_account),
                leg_b = %format!("{}/{}", cfg.leg_b_venue, cfg.leg_b_account),
                pairs = ?cfg.pairs,
                "cross-venue arb legs configured (.env ACCOUNT_ARB_* rows)"
            );
        });
        // E-VA: the virtual pots are built from the config's caps BEFORE the
        // config moves into the struct (the caps are copied, so a later
        // runtime config mutation cannot move a quota mid-session).
        let virtual_accounts =
            crate::risk::virtual_account::VirtualAccounts::new(&config.virtual_limits);
        // The shadow manager reads the switch write-back path off the config
        // before the config moves into the struct (same ordering concern as
        // above). Issue 751: the flips land in `.env`, not the retired
        // `shadow_evolution.toml`.
        let shadow_env_path = config.env_file_path.clone();
        // #861: the runtime risk state (hot entry caps, systemic bounds, kill
        // switch, freeze book). Loaded BEFORE the config moves into the struct
        // so the restored caps land in the gate the boot builds. `None` (the
        // backtester, most in-process tests) keeps every knob session-scoped:
        // nothing is written, nothing is restored, and no boot freeze applies.
        // A file that exists but does not parse is fail-CLOSED — nothing is
        // restored, and `risk_state_corrupt` forces the every-account corrupt
        // freeze (the loud error is the contract, persist.rs). A CLEAN parse
        // freezes nobody (#928 — the #861 restart default freeze is retired).
        let risk_state_path = config.risk_state_path.clone().map(std::path::PathBuf::from);
        let mut risk_state_corrupt = false;
        let mut risk_state_restorable = false;
        let risk_state = risk_state_path.map(|path| {
            use crate::service::risk_admission::{
                RiskStateLoad, apply_restored_limits, load_risk_state_file,
            };
            match load_risk_state_file(&path) {
                RiskStateLoad::Missing => RiskStateCell {
                    path,
                    file: Default::default(),
                },
                RiskStateLoad::Corrupt(reason) => {
                    tracing::error!(
                        path = %path.display(),
                        reason = %reason,
                        "risk-state file UNREADABLE — NOT restoring any risk posture; \
                         every account will boot FROZEN (fail-closed, issue #861). \
                         Repair or delete the file, then resume trading explicitly."
                    );
                    risk_state_corrupt = true;
                    RiskStateCell {
                        path,
                        file: Default::default(),
                    }
                }
                RiskStateLoad::Ok(file) => {
                    apply_restored_limits(&mut config, &file, &path);
                    risk_state_restorable = true;
                    RiskStateCell { path, file }
                }
            }
        });
        let mut core = Self {
            risk: RiskGate::new(config.risk.clone()),
            account_risk_db: None,
            store,
            config,
            ome: Ome::new(),
            inflight_takers: Vec::new(),
            accounts,
            breaker,
            intent_audit,
            positions,
            books: HashMap::new(),
            exit_reason_ledger: ExitReasonLedger {
                exit_reasons: HashMap::new(),
                exit_reason_order: VecDeque::new(),
                exit_reasons_evicted: 0,
                exit_reasons_swept: 0,
            },
            strategy_exits: Vec::new(),
            withheld_exit_logged: std::collections::HashSet::new(),
            entry_reject_logged: std::collections::HashSet::new(),
            engine: None,
            strategy_declarations: HashMap::new(),
            plugin_modes: Vec::new(),
            feeds: std::collections::BTreeMap::new(),
            extensions: {
                use crate::extension::Extension;
                let mut reg = crate::extension::ExtensionRegistry::new();
                // Built-in example extension (installed, not enabled by default).
                // Its config file is read and checked against the registered
                // extension (KI-11): the file is no longer inert, and a config
                // that has drifted from the code is reported at startup.
                let ext = crate::extension::builtins::BinanceSpotExtension::new();
                let path =
                    crate::extension::config_path_for(crate::extension::EXTENSIONS_DIR, ext.name());
                let cfg = crate::config::ExtensionConfig::load(&path);
                for w in &cfg.warnings {
                    tracing::warn!(extension = ext.name(), "extension config: {w}");
                }
                for k in &cfg.unknown_keys {
                    tracing::warn!(
                        extension = ext.name(),
                        key = k,
                        "extension config key not understood (ignored)"
                    );
                }
                for d in &cfg.declared_only {
                    tracing::info!(
                        extension = ext.name(),
                        key = %d.key,
                        reason = d.reason,
                        "extension config declares a setting the kernel does not act on"
                    );
                }
                let kind = format!("{:?}", ext.extension_type()).to_lowercase();
                for problem in cfg.check_against(ext.name(), ext.version(), &kind) {
                    tracing::warn!(extension = ext.name(), "extension config: {problem}");
                }
                reg.install(Box::new(ext), cfg.path);
                reg
            },
            shadow_evolution: ShadowEvolution::new_with_env_path(shadow_cfg, &[], shadow_env_path),
            trade_db,
            order_db,
            position_db,
            stats: CoreStats::default(),
            // 探针从零开始：没有 tick/spot/NTP 历史（与 stats 同一生命期，
            // 会话作用域）。
            probe_health: ProbeHealth {
                probe_ticks: crate::probe::TickHealth::default(),
                probe_spot: crate::probe::SpotWatch::default(),
                probe_ntp: crate::probe::NtpWatch::default(),
                probe_last_window_ticks: 0,
                probe_last_zero: Vec::new(),
                probe_last_max_silence_ms: 0,
                probe_last_spot_stale_ms: None,
                stats_probe_stream_breaks: 0,
                stats_probe_spot_stales: 0,
            },
            archive_verdicts: HashSet::new(),
            // The session clock for the accounting identity (issue #200). Read
            // once, at construction: the ledger below is seeded at the same
            // instant, so "cash since the seed" and "trades since startedAtMs"
            // describe the same window.
            started_at_ms: now_ms(),
            event_archive,
            strategy_accounting: HashMap::new(),
            virtual_accounts,
            place_cooldowns: HashMap::new(),
            escalation_skips: HashSet::new(),
            pending_venue_cancels: Vec::new(),
            freeze_book: FreezeBook::default(),
            account_gates: HashMap::new(),
            last_error: None,
            recon_watermark_ms: 0,
            boot_replayed_journal,
            last_self_check: None,
            applied_log,
            unified_ledger,
            unified_candidates: Vec::new(),
            audit: AuditRuntime {
                audit_log_path: audit_log,
                ..Default::default()
            },
            settlement,
            receivable_warned_age_ms: 0,
            archive_recording_last: true,
            ghost_cash_alerted: std::collections::HashMap::new(),
            venue_truth_last: std::collections::HashMap::new(),
            venue_fact_cursor: std::collections::HashMap::new(),
            close_credentials: std::collections::VecDeque::new(),
            close_credential_log,
            next_id: 1,
            tx: None,
            kline_preview_last_ms: std::collections::HashMap::new(),
            // #363: the boot-loaded policy (None = no config file found).
            execution_policy,
            // #757: nothing written through IPC yet — every key resolves from
            // the file / deployment env exactly as at boot.
            execution_policy_env_writes: std::collections::BTreeMap::new(),
            policy_cooldowns: HashMap::new(),
            // Replay fast path: nothing dirty, no deadlines known yet — the
            // first tick always runs the full sweeps (MAX stamps force them).
            fast_path: ReplayFastPath {
                fast_dirty_tokens: HashSet::new(),
                fast_eval_wake: true,
                fast_next_timing_ms: 0,
                fast_orders_dirty: true,
                fast_positions_dirty: true,
                fast_next_escalation_ms: 0,
                fast_next_expiry_ms: 0,
                fast_last_eval_ms: 0,
                // Issue #390 (nofast): 0 = due on the first tick.
                fast_next_exit_deadline_ms: 0,
            },
            pair_chase_attempts: HashMap::new(),
            #[cfg(test)]
            test_duration_subscriptions: None,
            #[cfg(test)]
            test_escalation_snapshot_fill: None,
            // 跨所套利弹窗 v2: the boot-loaded leg configuration (memory
            // mirror of the ACCOUNT_ARB_* env rows — .env is the
            // durable truth, this cell feeds the engine hot). None =
            // unconfigured: the legacy unified-confirm chain stays the only
            // pairing source and the account-stamp is inert.
            arb_leg_config,
            // RFC 568 阶段一：配对簿随进程启动为空账（不持久化 —— 事实事件
            // 重放经同一事件流重建，#559 3c 的口径见 pair 模块注释）。
            pair_book: crate::pair::PairBook::default(),
            // #861: the boot-loaded risk state (None = no risk-state path —
            // the backtester/most-test shape: session-scoped, as always).
            risk_state,
        };
        // 操作者裁决 2026-11-14（#1199 第六轮）：库在装配里打开即安装——
        // 风控行、退出纪律、账户设置与进化开关句柄全部来自同一连接。
        // 失败 fail-closed：拒绝启动。
        if let Some(s) = &sqlite
            && let Err(e) = core.install_risk_db(s.risk_db().clone())
        {
            panic!("refusing to boot: {e}");
        }
        // The kill switch and the freeze book land AFTER the struct exists
        // (they run the freeze paths, which emit and touch the book). The
        // entry caps were restored into `config` above, so the gate the boot
        // builds already carries them. A clean boot freezes nobody (#928 —
        // the #861 restart default freeze is retired); the corrupt-boot
        // fail-closed freeze is the only blanket left.
        core.restore_risk_state_at_boot(risk_state_restorable);
        if risk_state_corrupt {
            core.freeze_all_accounts_corrupt_boot();
        }
        // #1057: 映射每个配置账户的耐久姿态（ReadOnly / Suspended / Frozen /
        // 交易开关关）为 AccountGate，供面板与 stats 读出一套闸门模型；
        // 执法在每条下单路径由 account_gate_permits 完成（posture_permits
        // 已退役，#1062）。
        reconcile_config_gates(&mut core, now_ms());
        // File-first boot derivation (operator ruling 2026-11-08): the
        // cockpit working copies (exit discipline + systemic limits) are
        // derived from the `.env` FILE directly — the launcher's process-env
        // snapshot is no longer consulted for these keys, so a file edit made
        // after the launcher started is what a (re)booted core runs. A read
        // failure warns and keeps the snapshot-resolved values (last known
        // good).
        if let Err(e) = core.refresh_cockpit_from_env_file() {
            tracing::warn!(
                "cockpit boot derivation from the env file failed ({e}); the                  launcher-snapshot values stand for this boot"
            );
        }
        core
    }

/// 从 `accounts` 表的行重建账户簿（SQL 权威，操作者裁决 2026-11-14）。
/// Fail-closed：任何一个格子的形状不对都拒绝启动——「写进去的和读出来的
/// 不是同一个事实」的簿不允许交易。策略集合不在行里（它们住在
/// `account_settings`，由 `install_risk_db` 的 overlay 负责）。
fn ledger_book_from_rows(
    rows: &[crate::risk::account_store::AccountRow],
    db: &crate::risk::account_store::AccountRiskDb,
    now_ms: i64,
) -> crate::account::AccountLedgers {
    use crate::account::{Account, AccountLedgers, AccountStatus};
    let mut accounts = Vec::with_capacity(rows.len());
    for row in rows {
        // `defaults` 是策略段选择器的保留拼写（env_state.rs：它永远不是
        // 真实账户 id）——`save_policy_section` 为它 ensure 了一个行，
        // 这里不许把它物化成账户。
        if row.id.eq_ignore_ascii_case("defaults") {
            continue;
        }
        if row.market_type != "prediction" {
            panic!(
                "refusing to boot: accounts row `{}` has unknown market_type `{}` \
                 (fail-closed: the durable book must parse or the kernel must not trade)",
                row.id, row.market_type
            );
        }
        let status = match row.status.as_str() {
            "active" => AccountStatus::Active,
            "read_only" => AccountStatus::ReadOnly,
            "frozen" => AccountStatus::Frozen,
            "suspended" => AccountStatus::Suspended {
                reason: "suspended (reason not stated in the store row)".into(),
            },
            other => panic!(
                "refusing to boot: accounts row `{}` has unknown status `{other}`",
                row.id
            ),
        };
        let rounds: Vec<i64> = serde_json::from_str(&row.rounds_json).unwrap_or_else(|e| {
            panic!(
                "refusing to boot: accounts row `{}` rounds_json malformed ({e})",
                row.id
            )
        });
        let opening_equity = row.opening_equity.as_deref().map(|s| {
            use std::str::FromStr as _;
            rust_decimal::Decimal::from_str(s).unwrap_or_else(|_| {
                panic!(
                    "refusing to boot: accounts row `{}` opening_equity `{s}` is not a decimal",
                    row.id
                )
            })
        });
        let credential_keys = db
            .load_account_credential_keys(&row.id)
            .unwrap_or_else(|e| {
                panic!(
                    "refusing to boot: account_credentials read for `{}` failed ({e})",
                    row.id
                )
            })
            .map(|names| crate::account::CredentialKeys {
                api_key_env: names.api_key_env,
                secret_env: names.secret_env,
                passphrase_env: names.passphrase_env,
                private_key_env: names.private_key_env,
                loaded: true,
            })
            .unwrap_or_default();
        accounts.push(Account {
            id: crate::model::AccountId::from(row.id.as_str()),
            name: row.name.clone(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status,
            credential_keys,
            venue: row.venue.clone(),
            trading_enabled: row.trading_enabled,
            strategies: Vec::new(),
            rounds,
            opening_equity,
            updated_at_ms: row.updated_at_ms,
        });
    }
    if accounts.is_empty() {
        return AccountLedgers::single_default(now_ms);
    }
    // active：配置里有 `default` 就是它，否则第一行（与 env 装载器同一规则）。
    let default_id = crate::model::default_account_id();
    let active = if accounts.iter().any(|a| a.id == default_id) {
        default_id
    } else {
        accounts[0].id.clone()
    };
    // display：`kernel_meta.display_account` 行（`save_display_account` 的
    // 家）；没有行或缺指 → 与 active 同格（env 装载器的派生规则）。
    let display = db
        .load_kernel_meta(crate::db::META_DISPLAY_ACCOUNT)
        .ok()
        .flatten()
        .map(|d| crate::model::AccountId::from(d.as_str()))
        .filter(|d| accounts.iter().any(|a| &a.id == d))
        .unwrap_or_else(|| active.clone());
    AccountLedgers::new_with_display(accounts, active, display)
}

/// #363: the boot-time policy load. Malformed input is audited
/// (`load-refused`) and logged as a warning — the boot continues with the
    /// neutral (no-policy) kernel instead of dying. This mirrors the cockpit
    /// families' posture (warn + keep last-known-good): a bad POLICY_* key in
    /// `.env` must not take the whole kernel down, and the operator can fix
    /// the file and hot-reload without a restart.
    fn load_execution_policy_at_boot(
        config: &CoreConfig,
    ) -> Option<crate::execution_policy::Policy> {
        let path = config
            .execution_policy_path
            .as_deref()
            .map(std::path::Path::new)?;
        // Single-source read (issue 751): the launcher folds .env into the
        // process environment, but the file face is read too so a same-checkout
        // tool spawn (no launcher) still sees the operator's POLICY_* keys.
        let env_file = config.env_file_path.as_deref().map(std::path::Path::new);
        match crate::execution_policy::Policy::load_single_source(path, env_file) {
            Ok(policy) => Some(policy),
            Err(reason) => {
                crate::execution_policy::append_audit_to(
                    &Self::execution_policy_audit_file(config),
                    &crate::execution_policy::PolicyAuditRecord {
                        ts_ms: now_ms(),
                        actor: crate::execution_policy::ACTOR_BOOT.to_string(),
                        action: "load-refused".to_string(),
                        account_id: "*".to_string(),
                        before: None,
                        after: None,
                        error: Some(reason.clone()),
                    },
                );
                tracing::warn!(
                    "execution_policy boot load refused ({reason}); continuing with the \
                     neutral kernel — fix .env and hot-reload, no restart needed"
                );
                None
            }
        }
    }

    // ── #363: execution policy surface (IPC + reload) ──────────────────────

    /// One section as the wire reports it: the EFFECTIVE view (own overrides
    /// folded over the globals), decimals as strings so the shortest-decimal
    /// rule survives the JSON hop.
    pub fn execution_policy_section_view(
        &self,
        account_id: &str,
    ) -> Option<crate::schema::ExecutionPolicySectionView> {
        let policy = self.execution_policy.as_ref()?;
        let s = policy.section_for(account_id);
        Some(crate::schema::ExecutionPolicySectionView {
            budget_ratio: Some(s.budget_ratio.to_string()),
            min_budget_usd: Some(s.min_budget_usd.to_string()),
            max_budget_usd: Some(s.max_budget_usd.to_string()),
            min_equity_usd: Some(s.min_equity_usd.to_string()),
            max_positions_per_asset: s.max_positions_per_asset,
            rules: Some(
                s.rules
                    .iter()
                    .map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null))
                    .collect(),
            ),
        })
    }

    /// The account ids the loaded policy names with a section of their own.
    pub fn execution_policy_account_ids(&self) -> Vec<String> {
        match &self.execution_policy {
            Some(p) => {
                let mut ids: Vec<String> = p.accounts.keys().cloned().collect();
                ids.sort();
                ids
            }
            None => Vec::new(),
        }
    }

    /// The journal file this config's execution-policy audit lines land in
    /// (#386): the config's private override, or the shipped
    /// `data/audit/execution_policy.jsonl` spelling when unset.
    fn execution_policy_audit_file(config: &CoreConfig) -> std::path::PathBuf {
        config
            .execution_policy_audit_path
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(crate::execution_policy::EXECUTION_POLICY_AUDIT_PATH)
            })
    }

    /// Whether the policy is loaded at all (the zero-config deployment has
    /// none — the read arms report that instead of inventing a section).
    pub fn execution_policy_loaded(&self) -> bool {
        self.execution_policy.is_some()
    }

    /// Append one audit line to THIS core's journal (#386). The wire set/
    /// reset arms and the reload refusal all land through here, so a test
    /// core with a private journal never touches the checkout's data dir.
    pub fn execution_policy_append_audit(
        &self,
        record: &crate::execution_policy::PolicyAuditRecord,
    ) -> bool {
        crate::execution_policy::append_audit_to(
            &Self::execution_policy_audit_file(&self.config),
            record,
        )
    }

    /// The audit journal for the history arm — read from THIS core's own
    /// journal file (#386), not a process-wide fixed path.
    pub fn execution_policy_audit(&self) -> Vec<crate::execution_policy::PolicyAuditRecord> {
        crate::execution_policy::load_audit_from(&Self::execution_policy_audit_file(&self.config))
    }

    /// #364: replay the CURRENT section over the recent closed trades of one
    /// account — the read-only fact behind the UI's "these rules would skip
    /// N of the last M entries". Every historical close supplies the facts a
    /// live entry would carry (balance is the LIVE ledger, not a per-trade
    /// snapshot: a ledger is a book, not a tape — disclosed in the row, not
    /// pretended into history), and `evaluate` — the ONE verdict function —
    /// judges it. The preview can never disagree with the kernel because it
    /// does not re-implement the kernel.
    pub fn execution_policy_preview(
        &self,
        account_id: &str,
        limit: usize,
    ) -> crate::schema::ExecutionPolicyPreviewResult {
        use crate::execution_policy::PolicyOutput;
        let now = now_ms();
        // Closed trades of THIS account, oldest first, capped at `limit` —
        // the same rows `trades.history` reports, judged instead of shown.
        let mut closed: Vec<&crate::position::ClosedPosition> = self
            .positions
            .closed_positions()
            .iter()
            .filter(|c| c.account_id.as_str() == account_id)
            .collect();
        if closed.len() > limit {
            closed.drain(0..closed.len() - limit);
        }
        // The live book facts (the parts of PolicyInput that are not
        // per-trade): balance/equity from the account's own ledger, the
        // trailing-hour PnL, the open count, the strategy streaks.
        let balance = self
            .accounts
            .ledger_of(&blitzkrieg_market_api::AccountId::from(account_id))
            .map(|l| l.balance())
            .unwrap_or_default();
        let open_for = self
            .positions
            .open_positions()
            .iter()
            // The preview replays the entry gates: the count must match the
            // entry path's LIVE-only rule (expired rounds are settlement-dead).
            .filter(|p| p.account_id.as_str() == account_id && p.expires_at_ms > now)
            .count() as u32;
        let mut considered = 0usize;
        let mut skipped = 0usize;
        let mut budgets: Vec<Decimal> = Vec::new();
        let mut rows = Vec::with_capacity(closed.len());
        for c in &closed {
            considered += 1;
            let facts = crate::execution_policy::PolicyInput {
                available_balance: balance,
                total_equity: balance,
                open_positions: open_for,
                current_price: c.entry_price,
                // The close is long over; the round it belonged to has
                // resolved. Zero is the honest "no clock running" fact.
                time_left_sec: 0,
                symbol: c.asset.clone(),
                recent_pnl_1h: self.positions.recent_pnl_1h_for(account_id, now),
                consecutive_losses: self.breaker.consecutive_losses(&c.strategy),
            };
            let (verdict, detail) = match self
                .execution_policy
                .as_ref()
                .map(|p| crate::execution_policy::evaluate(p, account_id, &facts))
            {
                None => ("refused".to_string(), Some("no policy loaded".to_string())),
                Some(Err(refusal)) => ("refused".to_string(), Some(refusal.reason)),
                Some(Ok(PolicyOutput::Place { budget_usd, .. })) => {
                    budgets.push(budget_usd);
                    ("place".to_string(), Some(budget_usd.to_string()))
                }
                Some(Ok(PolicyOutput::Skip { reason })) => {
                    skipped += 1;
                    ("skip".to_string(), Some(reason))
                }
                Some(Ok(PolicyOutput::Cooldown { seconds })) => {
                    skipped += 1;
                    ("cooldown".to_string(), Some(format!("{seconds}s")))
                }
            };
            let symbol = facts.symbol.clone();
            rows.push(crate::schema::ExecutionPolicyPreviewRow {
                ts_ms: c.exited_at_ms,
                symbol,
                balance,
                price: c.entry_price,
                verdict,
                detail,
            });
        }
        // `in` conditions on symbols judge the asset the trade named; nothing
        // special-cased here — `evaluate` already handles every field.
        let avg_budget_usd = if budgets.is_empty() {
            None
        } else {
            let sum = budgets.iter().copied().fold(Decimal::ZERO, |a, b| a + b);
            let n = Decimal::from(budgets.len() as u64);
            // Shortest-decimal discipline applies to the WIRE (the `to_string`
            // below is what crosses IPC), not to the Decimal itself — no
            // `normalize()` here, it would shave 10.00 down to "10" and break
            // the money-string convention every other field keeps.
            Some((sum / n).to_string())
        };
        crate::schema::ExecutionPolicyPreviewResult {
            considered,
            skipped,
            avg_budget_usd,
            rows,
        }
    }

    /// #757: remember the keys the IPC section write just made authoritative
    /// (`Some` = the value written, `None` = the key was swept). The write
    /// face calls this BEFORE [`Self::reload_execution_policy`] so the reload
    /// resolves those keys from memory rather than the stale startup export
    /// of the process environment.
    pub fn execution_policy_record_env_write(
        &mut self,
        written: std::collections::BTreeMap<String, Option<String>>,
    ) {
        self.execution_policy_env_writes.extend(written);
    }

    /// Replace the in-memory policy wholesale after a file write (set/reset
    /// arms). Re-derives the per-asset cap the way boot did — CLI flag wins
    /// over the file — and lands one `load-refused` audit line + keeps the
    /// OLD policy when the new file does not parse: fail-closed means a
    /// broken write never trades unguarded, it keeps the last good guard.
    pub fn reload_execution_policy(&mut self) -> Result<(), String> {
        let path = self
            .config
            .execution_policy_path
            .clone()
            .ok_or_else(|| "execution policy disabled (no config path)".to_string())?;
        // Single-source read (issue 751): the write face lands POLICY_* keys
        // in the .env the launcher owns — a same-process reload must see them
        // before the next restart folds the file into the process env. The
        // in-memory write snapshot (#757) rides between the file and the
        // process env so a stale boot export can neither shadow a write nor
        // resurface after a reset.
        let env_file = self
            .config
            .env_file_path
            .as_deref()
            .map(std::path::Path::new)
            .ok_or_else(|| "env_file_path not set".to_string())?;
        // SQL 行先取（本会话的写入已即时落库）；读取失败 fail-closed 保留
        // 旧策略（一次坏读不许让守卫消失）。
        let sql_settings = match self.account_risk_db.as_ref() {
            Some(db) => Some(
                db.load_settings()
                    .map_err(|e| format!("the SQLite policy sections are unreadable ({e})"))?,
            ),
            None => None,
        };
        let reload = match sql_settings.as_ref() {
            Some(sql) => crate::execution_policy::Policy::load_for_reload_with_sql(
                std::path::Path::new(&path),
                env_file,
                &self.execution_policy_env_writes,
                sql,
                |key| std::env::var(key).ok().filter(|v| !v.trim().is_empty()),
            ),
            None => crate::execution_policy::Policy::load_for_reload(
                std::path::Path::new(&path),
                env_file,
                &self.execution_policy_env_writes,
            ),
        };
        match reload {
            Ok(policy) => {
                let mut positions = self.config.positions.clone();
                positions.max_positions_per_asset = policy.defaults.max_positions_per_asset;
                if let Some(v) = self.config.max_positions_per_asset {
                    positions.max_positions_per_asset = v;
                }
                self.positions.set_config(positions);
                self.execution_policy = Some(policy);
                Ok(())
            }
            Err(reason) => {
                self.execution_policy_append_audit(&crate::execution_policy::PolicyAuditRecord {
                    ts_ms: now_ms(),
                    actor: crate::execution_policy::ACTOR_IPC.to_string(),
                    action: "load-refused".to_string(),
                    account_id: "*".to_string(),
                    before: None,
                    after: None,
                    error: Some(reason.clone()),
                });
                Err(reason)
            }
        }
    }

    /// Reload account configuration. 操作者裁决 2026-11-14：库里的
    /// `accounts` 表非空 → SQL 是权威（与 boot 同一规则，行从
    /// [`Self::ledger_book_from_rows`] 重建）；空表 → `.env` 的 ACCOUNT_*
    /// 兼容回退（只读）。Fails closed if the source is malformed, keeping
    /// the current configuration.
    pub fn reload_accounts(&mut self) -> Result<(), String> {
        let now_ms = blitzkrieg_market_api::net::now_ms();
        let allowed = &self.config.allowed_round_secs;
        let sql_rows = match self.account_risk_db.as_ref() {
            Some(db) => Some(db.load_accounts().map_err(|e| {
                format!("the durable account book is unreadable ({e}) — refusing the reload")
            })?),
            None => None,
        };
        let accounts = match &sql_rows {
            Some(rows) if !rows.is_empty() => {
                Self::ledger_book_from_rows(rows, self.account_risk_db.as_ref().expect("checked above"), now_ms)
            }
            _ => {
                let env_file = self
                    .config
                    .env_file_path
                    .as_deref()
                    .map(std::path::Path::new)
                    .ok_or_else(|| "env_file_path not set".to_string())?;
                // Env-only (issue 923): no legacy toml fallback — a .env without
                // ACCOUNT_LIST reloads into the zero-config single default.
                crate::account::config::load_account_ledgers_for_reload(
                    env_file, now_ms, allowed,
                )
                .map_err(|e| e.message)?
            }
        };
        self.accounts.reload_config(accounts)
    }

    /// Reload shadow evolution switches. 操作者裁决 2026-11-14（#1230 起）：
    /// 开关的家是 `kernel_meta`（`.env` 的 SHADOW_* 只是旧部署回退，只读）。
    /// 库里有行读行；没有行才解析 env。
    pub fn reload_shadow(&mut self) -> Result<(), String> {
        let (enabled, auto_evolve) = if let Some(db) = self.account_risk_db.as_ref() {
            let enabled = db
                .load_kernel_meta(crate::risk::account_store::SHADOW_ENABLED_META)
                .map_err(|e| format!("kernel_meta read failed: {e}"))?;
            let auto = db
                .load_kernel_meta(crate::risk::account_store::SHADOW_AUTO_EVOLVE_META)
                .map_err(|e| format!("kernel_meta read failed: {e}"))?;
            if enabled.is_some() || auto.is_some() {
                let parse = |name: &str, v: Option<String>| match v.as_deref() {
                    Some("true") => Ok(true),
                    Some("false") => Ok(false),
                    Some(other) => Err(format!("{name} must be true/false, got: {other}")),
                    None => Ok(false),
                };
                (
                    parse("shadow_enabled", enabled)?,
                    parse("shadow_auto_evolve", auto)?,
                )
            } else {
                Self::shadow_switches_from_env(self.config.env_file_path.as_deref())?
            }
        } else {
            Self::shadow_switches_from_env(self.config.env_file_path.as_deref())?
        };

        if enabled {
            self.shadow_evolution
                .enable(blitzkrieg_market_api::net::now_ms() as i64);
        } else {
            self.shadow_evolution.disable();
        }
        self.shadow_evolution.set_auto_evolve(auto_evolve);
        Ok(())
    }

    /// The legacy env spelling of the two switches (read-only fallback).
    fn shadow_switches_from_env(
        env_path: Option<&str>,
    ) -> Result<(bool, bool), String> {
        let path = env_path
            .map(std::path::Path::new)
            .ok_or_else(|| "env_file_path not set".to_string())?;
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read .env: {e}"))?;
        let mut enabled = false;
        let mut auto_evolve = false;
        for line in content.lines() {
            if let Some((key, value)) = crate::env_file::parse_line(line) {
                match key.as_str() {
                    "SHADOW_ENABLED" => {
                        enabled = match value.as_str() {
                            "true" => true,
                            "false" => false,
                            other => {
                                return Err(format!(
                                    "SHADOW_ENABLED must be true/false, got: {other}"
                                ))
                            }
                        };
                    }
                    "SHADOW_AUTO_EVOLVE" => {
                        auto_evolve = match value.as_str() {
                            "true" => true,
                            "false" => false,
                            other => {
                                return Err(format!(
                                    "SHADOW_AUTO_EVOLVE must be true/false, got: {other}"
                                ))
                            }
                        };
                    }
                    _ => {}
                }
            }
        }
        Ok((enabled, auto_evolve))
    }

    pub fn set_event_sink(&mut self, tx: mpsc::UnboundedSender<Event>) {
        self.tx = Some(tx);
    }
}
