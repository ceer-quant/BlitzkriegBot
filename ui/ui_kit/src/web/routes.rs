use super::*;

impl WebServer {
    /// Issue 718: THE transparent-read primitive of the gateway — one verbatim
    /// IPC call through the shared core client, the kernel's answer (or the
    /// transport error) back as-is. The gateway adds nothing and reads nothing
    /// back out: no filesystem access, no command construction, no parameter
    /// shaping — the spec's "WebUI 必须走 IPC" boundary is enforced by the
    /// shape of this function: there is nothing here a malicious body could
    /// steer onto a path or a subprocess. Every transparent read route
    /// (`risk.limits`, `unified.list`, `orders.list`, the #353 backtest trio,
    /// the E25 audit tail, …) is a one-line call into this. The error document
    /// shape (`{ "error": … }`, with the kernel's own message — the frontend's
    /// `rpc -32601` feature-detect rides inside) is frozen wire contract.
    pub(super) fn proxy_read(
        &self,
        verb: &str,
        params: serde_json::Value,
    ) -> (u16, &'static str, Vec<u8>) {
        let doc = match self.snapshot_src.lock() {
            Ok(mut c) => match c.call(verb, params) {
                Ok(v) => v,
                Err(e) => serde_json::json!({ "error": e.to_string() }),
            },
            Err(_) => serde_json::json!({ "error": "core client poisoned" }),
        };
        (200, "application/json", doc.to_string().into_bytes())
    }

    /// #353: the POST trio (`onchain-pull` / `run`) — the body IS the IPC
    /// params object; a body that is not valid JSON is a 400 that never
    /// reaches the kernel. A thin wrapper over [`Self::proxy_read`].
    pub(super) fn proxy_backtest_body(
        &self,
        method: &str,
        body: &str,
    ) -> (u16, &'static str, Vec<u8>) {
        match body_params(body) {
            Ok(p) => self.proxy_read(method, p),
            Err(e) => (
                400,
                "application/json",
                serde_json::json!({ "error": e }).to_string().into_bytes(),
            ),
        }
    }

    /// #353: the GET trio (`status` / `result` / `export`) — the single `?id=`
    /// query param forwarded verbatim; a missing id reaches the kernel's own
    /// validation, which names the field. A thin wrapper over
    /// [`Self::proxy_read`].
    pub(super) fn proxy_backtest_id(
        &self,
        method: &str,
        id: Option<String>,
    ) -> (u16, &'static str, Vec<u8>) {
        let params = id
            .map(|i| serde_json::json!({ "id": i }))
            .unwrap_or_else(|| serde_json::json!({}));
        self.proxy_read(method, params)
    }

    pub(super) fn handle(&self, mut stream: TcpStream) {
        // A client that connects and stalls must not wedge the accept loop.
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
        let Some(req) = read_request(&mut stream) else {
            return;
        };
        let target = req.path().to_string();

        // E6-a: auth + origin gate runs BEFORE any route does work.
        let status_gate = self.authorize(&req);
        if status_gate != 200 {
            let reason = if status_gate == 401 {
                "Unauthorized"
            } else {
                "Forbidden"
            };
            let head = format!(
                "HTTP/1.1 {status_gate} {reason}\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
            return;
        }

        // Set by a route that wants a header the plain (status, ctype, body)
        // tuple cannot carry — currently only `Retry-After` on a login lockout.
        let mut retry_after: Option<u64> = None;
        // #460: the failed-login speed bump, paid where it hurts only the
        // attacker. The old code slept INSIDE the route — but `serve` answers
        // one connection at a time, so a client hammering failed logins was
        // stalling the operator's own panel (the exact DoS the throttle exists
        // to prevent). The route reports the delay here; `handle` sleeps after
        // the failed response is already on the wire, so the accept loop is
        // never held and a successful login is never delayed.
        let mut respond_then_delay_ms: Option<u64> = None;
        let (status, ctype, body): (u16, &'static str, Vec<u8>) = match (
            req.method.as_str(),
            target.as_str(),
        ) {
            ("GET", "/api/ping") | ("HEAD", "/api/ping") => {
                // Unauthenticated by design; see `ping_doc`.
                (200, "application/json", self.ping_doc().into_bytes())
            }
            ("GET", "/api/intent-audit") => {
                // E25 (#331): the decisions page reads the arbitration audit
                // through the SAME core client the snapshot uses — a thin
                // proxy of `intent.audit.tail` (§12.3); the query filters
                // (`limit` / `strategy` / `decision`) pass through untouched.
                // Issue 718: the hand-rolled lock→call→error-wrap boilerplate
                // collapsed into [`Self::proxy_read`] (that boilerplate spelled
                // the poisoned-lock string "audit client poisoned", but the
                // client IS the snapshot's, and no test or panel code ever
                // matched on the difference).
                let mut params = serde_json::Map::new();
                if let Some(n) = req
                    .query_param("limit")
                    .and_then(|v| v.parse::<usize>().ok())
                {
                    params.insert("limit".into(), serde_json::json!(n));
                }
                if let Some(v) = req.query_param("strategy") {
                    params.insert("strategy".into(), serde_json::json!(v));
                }
                if let Some(v) = req.query_param("decision") {
                    params.insert("decision".into(), serde_json::json!(v));
                }
                self.proxy_read("intent.audit.tail", serde_json::Value::Object(params))
            }
            ("GET", "/api/backtest/onchain-list") => {
                // #353: the backtest page's dataset picker — a verbatim proxy
                // of `backtest.onchain.list` (the kernel's dataset root is
                // fixed there; no caller parameter to smuggle a path through).
                self.proxy_read("backtest.onchain.list", serde_json::json!({}))
            }
            ("GET", "/api/backtest/status") => {
                // #353: the page's ~1s poll — verbatim proxy of
                // `backtest.status`, `?id=` forwarded untouched.
                let id = req.query_param("id");
                self.proxy_backtest_id("backtest.status", id)
            }
            ("GET", "/api/backtest/result") => {
                // #353: the finished job's full report — verbatim proxy of
                // `backtest.result`.
                let id = req.query_param("id");
                self.proxy_backtest_id("backtest.result", id)
            }
            ("GET", "/api/backtest/export") => {
                // #353: the download envelope (`fileName` + `content`) — the
                // BROWSER writes the file, the gateway never does.
                let id = req.query_param("id");
                self.proxy_backtest_id("backtest.export", id)
            }
            ("POST", "/api/backtest/onchain-pull") => {
                // #353: 拉数据 — the body IS the `backtest.onchain.pull`
                // params object (wallet/window/assets), forwarded verbatim.
                self.proxy_backtest_body("backtest.onchain.pull", &req.body)
            }
            ("POST", "/api/backtest/run") => {
                // #353: 回测 — the body IS the `backtest.run` params object
                // (dataset/mode/knobs), forwarded verbatim. The kernel's typed
                // deserialization validates every field; the gateway shapes
                // nothing.
                self.proxy_backtest_body("backtest.run", &req.body)
            }
            ("POST", "/api/blueprint/compile") => {
                // #362: the blueprint editor's preview pane — the body IS the
                // `blueprint.compile` params object (`{json}`), forwarded
                // verbatim. Read-only and stateless kernel-side: a refusal
                // names the node id, and the gateway shapes nothing.
                self.proxy_backtest_body("blueprint.compile", &req.body)
            }
            ("POST", "/api/blueprint/save") => {
                // #362: save — the body IS the `blueprint.save` params object
                // (`{name, json, overwrite?}`). The kernel validates the name,
                // compiles FRESH, and writes the strategy package under its
                // own strategy root; the browser never learns a filesystem and
                // the gateway shapes nothing.
                self.proxy_backtest_body("blueprint.save", &req.body)
            }
            ("POST", "/api/blueprint/load") => {
                // #393: the editor's preload read — the body IS the
                // `blueprint.load` params object (`{name}`), forwarded
                // verbatim. Read-only kernel-side; a package that was never
                // written by blueprint.save refuses with a message the card
                // shows inline.
                self.proxy_backtest_body("blueprint.load", &req.body)
            }
            ("POST", "/api/blueprint/load-source") => {
                // 手写包的编辑入口（用户裁决：编辑功能必须能打开所有 Lua 包）：
                // the body IS the `blueprint.loadSource` params object
                // (`{name}`), forwarded verbatim. Read-only kernel-side; the
                // editor's source mode shows the entry file + manifest.
                self.proxy_backtest_body("blueprint.loadSource", &req.body)
            }
            ("POST", "/api/blueprint/save-source") => {
                // 写回镜像 — the body IS the `blueprint.saveSource` params
                // object (`{name, lua, overwrite?}`), forwarded verbatim. The
                // kernel re-seals the manifest sha256; the browser never
                // touches the filesystem (repo iron rule).
                self.proxy_backtest_body("blueprint.saveSource", &req.body)
            }
            ("GET", "/api/risk-limits") => {
                // E26 (§4.4): the settings page's risk card reads the EFFECTIVE
                // systemic limits through the SAME core client the snapshot
                // uses — a thin proxy of `risk.limits` (read-only, no params;
                // the core answers from the LIVE risk config, so a hot edit
                // made through the card below shows up on the next read).
                // Issue 718: one [`Self::proxy_read`] line where the
                // lock→call→error-wrap boilerplate used to be spelled out.
                self.proxy_read("risk.limits", serde_json::json!({}))
            }
            ("POST", "/api/risk/set-systemic") => {
                // 生效风控的可编辑写路径（用户裁决：九项系统限额必须都能改）：
                // the body IS the `risk.setSystemic` params object
                // (`{maxSingleLossUsd?, ..., reason?}`), forwarded verbatim.
                // The kernel plans first, writes later, and names each bound's
                // effect class (live / next_session) in the audited reply.
                self.proxy_backtest_body("risk.setSystemic", &req.body)
            }
            ("POST", "/api/risk/set-exit") => {
                // 退出纪律的可编辑写路径（用户裁决：Gate 4 绑定的止损/止盈/
                // 强平必须可以改）：the body IS the `risk.setExit` params
                // object (`{stopLossPct?, takeProfitPct?, forceExitSec?,
                // reason?}`), forwarded verbatim. The kernel plans first,
                // writes later, and the audited reply names each old→new.
                self.proxy_backtest_body("risk.setExit", &req.body)
            }
            ("GET", "/api/unified") => {
                // Issue #481 (G4): the confirmed cross-venue mappings — the
                // 配对视图 card's data source. Read-only proxy of
                // `unified.list`; the wire shape is the kernel's own (status /
                // venues / discrepancies as data, no venue branch).
                self.proxy_read("unified.list", serde_json::json!({}))
            }
            ("GET", "/api/unified-candidates") => {
                // ④: the current round's cross-venue pairing PROPOSALS
                // (auto-matching proposes, the operator disposes). Read-only
                // proxy of `unified.candidates`.
                self.proxy_read("unified.candidates", serde_json::json!({}))
            }
            ("POST", "/api/unified/confirm") => {
                // ④: the operator confirms one candidate pair — the body IS
                // the `unified.confirm` params object (`{title, listings,
                // actor?}`), forwarded verbatim. The ledger revalidates
                // everything (titles, assets, one-listing-one-event); a
                // refused confirm moves nothing.
                self.proxy_backtest_body("unified.confirm", &req.body)
            }
            ("POST", "/api/unified/revoke") => {
                // ④: withdraw a confirmed mapping — the body IS the
                // `unified.revoke` params object (`{eventId, actor?}`),
                // forwarded verbatim.
                self.proxy_backtest_body("unified.revoke", &req.body)
            }
            ("GET", "/api/strategy-params") => {
                // Issue #481 (G3): one strategy's declared tunables + the
                // values in force (`strategy.params.get`, read-only). The
                // strategy name rides the `?name=` query param verbatim; a
                // missing name reaches the kernel's own validation.
                let params = match req.query_param("name") {
                    Some(n) => serde_json::json!({ "name": n }),
                    None => serde_json::json!({}),
                };
                self.proxy_read("strategy.params.get", params)
            }
            ("POST", "/api/strategy-params/set") => {
                // Issue #481 (G3): the tunables' operator write path — the
                // body IS the `strategy.params.set` params object
                // (`{name, params: {knob: value, …}, reason?}`), forwarded
                // verbatim. The kernel validates every knob against the
                // strategy's OWN declared domain (plan first, write later)
                // and lands the audit rows.
                self.proxy_backtest_body("strategy.params.set", &req.body)
            }
            ("GET", "/api/round-duration") => {
                // 轮次时间周期设置卡的读路径：the body IS the
                // `round_duration.get` params object (`{}`), forwarded
                // verbatim. Read-only — the kernel answers from the live
                // config + the persisted cell (next_session), the gateway
                // shapes nothing.
                self.proxy_read("round_duration.get", serde_json::json!({}))
            }
            ("POST", "/api/round-duration/set") => {
                // 轮次时间周期设置卡的写路径（next_session）：the body IS the
                // `round_duration.set` params object (`{durationSec?, reset?,
                // reason?}`), forwarded verbatim. The kernel validates the
                // whitelist and writes the durable cell; the reply names the
                // next session's value and the running grid never moves.
                self.proxy_backtest_body("round_duration.set", &req.body)
            }
            ("GET", "/api/market-plugin") => {
                // Issue #485: the plugin page's market-plugin card — the
                // running session's active plugin, the compiled-in registry
                // and what a restart would select (`market.plugin.get`,
                // read-only). The wire shape is the kernel's own.
                self.proxy_read("market.plugin.get", serde_json::json!({}))
            }
            ("POST", "/api/market-plugin/set") => {
                // Issue #485: the market-plugin switch's write path
                // (next_session) — the body IS the `market.plugin.set`
                // params object (`{name?, reset?, reason?}`), forwarded
                // verbatim. The kernel refuses an unregistered name
                // plan-first and writes the durable cell; the running
                // session's feed/discovery/executor never move.
                self.proxy_backtest_body("market.plugin.set", &req.body)
            }
            ("GET", "/api/market-credentials") => {
                // 行情插件凭据读数：每插件所需 env 变量名 + 下一会话解析
                // 到没有（ready），值永不过线（`market.credentials.status`，
                // 只读）。The wire shape is the kernel's own.
                self.proxy_read("market.credentials.status", serde_json::json!({}))
            }
            ("POST", "/api/market-credentials/set") => {
                // 凭据写路径（next_session）—— the body IS the
                // `market.credentials.set` params object
                // (`{plugin, vars?, reset?, reason?}`), forwarded verbatim.
                // The kernel refuses an unregistered plugin / a name outside
                // the requirement table plan-first and writes the durable
                // cell; the executor reads its environment once at boot, so
                // the running session never moves.
                self.proxy_backtest_body("market.credentials.set", &req.body)
            }
            ("POST", "/api/extension/set") => {
                // Issue #485: the extension rows' 启用/停用 buttons — the
                // body IS the params object (`{name, enabled}`), forwarded
                // to the kernel's own `extension.enable` / `extension.disable`
                // (the SAME verbs the gateway's `extension <name> on|off`
                // command has always driven; the panel just never had an
                // entry point). The kernel runs on_load/on_unload and answers
                // `{name, enabled}` or names the failure.
                let parsed = body_params(&req.body);
                let enabled = parsed
                    .as_ref()
                    .ok()
                    .and_then(|p| p.get("enabled").and_then(|v| v.as_bool()));
                match (parsed, enabled) {
                    (Ok(p), Some(true)) => self.proxy_read("extension.enable", p),
                    (Ok(p), Some(false)) => self.proxy_read("extension.disable", p),
                    _ => (
                        400,
                        "application/json",
                        serde_json::json!({ "error": "body must be {\"name\": string, \"enabled\": bool}" })
                            .to_string()
                            .into_bytes(),
                    ),
                }
            }
            ("POST", "/api/system/reset-trading") => {
                // issue 458 交易环境重置卡: the body IS the
                // `system.reset_trading` params object (`{confirm, reason?,
                // accountId?}`), forwarded verbatim. issue 561: `accountId`
                // (the panel's display account) rides through untouched — the
                // reset is account-scoped at the kernel. The kernel refuses
                // LIVE mode, a wrong confirmation word, an unknown account and
                // a missing audit sink; a dry-mode pass wipes that account's
                // records and reseeds ITS ledger, leaving one manual-ops
                // audit row. Same proxy shape as round-duration: nothing is
                // reshaped here, the kernel's typed deserialization validates.
                self.proxy_backtest_body("system.reset_trading", &req.body)
            }
            ("POST", "/api/system/env-reload") => {
                // Explicit operator action: reload the subset of `.env` that
                // the running core can safely apply without replacing books.
                self.proxy_read("system.env.reload", serde_json::json!({}))
            }
            ("GET", "/api/orders") => {
                // issue 456: the order book as the kernel tracks it — the
                // 挂单 card's data source. Read-only proxy; the wire shape is
                // `orders.list`'s own (camelCase TrackedOrder rows).
                self.proxy_read("orders.list", serde_json::json!({}))
            }
            ("POST", "/api/orders/force-close") => {
                // issue 456: the 挂单 card's per-row force-close write path.
                // The body IS the `orders.force_close` params object
                // (`{orderId}`), forwarded verbatim; the kernel refuses an
                // order whose round is still live and lands the audit row.
                self.proxy_backtest_body("orders.force_close", &req.body)
            }
            ("POST", "/api/positions/force-close") => {
                // #684: the 当前持仓 card's per-row clear-out write path for an
                // EXPIRED market's position whose settlement verdict never
                // arrived. The body IS the `positions.force_close` params
                // object (`{positionId}`), forwarded verbatim; the kernel
                // refuses a live round (that is positions.exit's job) and an
                // unaudited call, and lands the manual-ops audit row itself.
                self.proxy_backtest_body("positions.force_close", &req.body)
            }
            ("GET", "/api/execution-policy") | ("GET", "/api/execution-policy/preview") => {
                // #364: the settings page's 生效风控 section. Effective view =
                // `execution_policy.get` (the kernel folds `[accounts.<id>]`
                // over `[defaults]` itself; the browser never reads the TOML),
                // preview = `execution_policy.preview` (`?accountId=`
                // forwarded untouched). Both read-only, both the SAME core
                // client the snapshot uses.
                let method = if req.path() == "/api/execution-policy/preview" {
                    "execution_policy.preview"
                } else {
                    "execution_policy.get"
                };
                let mut params = serde_json::Map::new();
                if let Some(a) = req.query_param("accountId") {
                    params.insert("accountId".into(), serde_json::json!(a));
                }
                self.proxy_read(method, serde_json::Value::Object(params))
            }
            ("GET", "/api/execution-policy/list") => {
                // #364: which accounts the book knows + whether a policy file
                // loaded — the account chips' source. Verbatim proxy.
                self.proxy_read("execution_policy.list", serde_json::json!({}))
            }
            ("GET", "/api/execution-policy/history") => {
                // #364: the audit journal for one account — the version list
                // the rollback UI counts. `?accountId=` forwarded verbatim.
                let mut params = serde_json::Map::new();
                if let Some(a) = req.query_param("accountId") {
                    params.insert("accountId".into(), serde_json::json!(a));
                }
                if let Some(n) = req
                    .query_param("limit")
                    .and_then(|v| v.parse::<usize>().ok())
                {
                    params.insert("limit".into(), serde_json::json!(n));
                }
                self.proxy_read(
                    "execution_policy.history",
                    serde_json::Value::Object(params),
                )
            }
            ("POST", "/api/execution-policy/set") | ("POST", "/api/execution-policy/reset") => {
                // #364: write paths — the body IS the IPC params object
                // (accountId + the SectionFields the kernel's typed
                // deserializer validates; the gateway shapes nothing). The
                // kernel answers only after write→reread→audit, so a 200 body
                // IS the fresh effective view the UI re-renders from.
                let method = if req.path() == "/api/execution-policy/set" {
                    "execution_policy.set"
                } else {
                    "execution_policy.reset"
                };
                self.proxy_backtest_body(method, &req.body)
            }
            ("GET", "/api/kline-history") => {
                // E29 (§12.2): the K-line chart reads bars through the SAME
                // core client — a thin proxy of `kline.history` (read-only).
                // Query params (`symbol` / `interval` / `limit`) pass through
                // untouched in wire shape; the kernel clamps limit 1..=1000
                // and answers "what happened" from its aggregator. The panel
                // has no WebSocket, so it re-polls this on its fast tick and
                // draws the growing bar as-is — §12.2's 1/s preview throttle
                // is a push-side courtesy, not a read-side contract.
                let mut params = serde_json::Map::new();
                if let Some(v) = req.query_param("symbol") {
                    params.insert("symbol".into(), serde_json::json!(v));
                }
                if let Some(v) = req.query_param("interval") {
                    params.insert("interval".into(), serde_json::json!(v));
                }
                if let Some(n) = req
                    .query_param("limit")
                    .and_then(|v| v.parse::<usize>().ok())
                {
                    params.insert("limit".into(), serde_json::json!(n));
                }
                // Issue 718: the trailing lock→call→error-wrap boilerplate is
                // [`Self::proxy_read`]; only the query parsing stays local.
                self.proxy_read("kline.history", serde_json::Value::Object(params))
            }
            ("GET", "/api/account-journal") => {
                // #511: the wallet's cash journal — a thin proxy of
                // `account.journal` (read-only). Query params (`accountId` /
                // `limit`) pass through untouched in wire shape; accountId
                // absent = the session's default account, limit clamps
                // 1..=500 kernel-side. The ledger answers from its append-only
                // journal, so the panel reads the trail the balance commits
                // against — the window (`truncated`) is disclosed, not hidden.
                let mut params = serde_json::Map::new();
                if let Some(v) = req.query_param("accountId") {
                    params.insert("accountId".into(), serde_json::json!(v));
                }
                if let Some(n) = req
                    .query_param("limit")
                    .and_then(|v| v.parse::<usize>().ok())
                {
                    params.insert("limit".into(), serde_json::json!(n));
                }
                self.proxy_read("account.journal", serde_json::Value::Object(params))
            }
            ("GET", "/api/account-venue") => {
                // #508: the account↔venue binding readout — a thin proxy of
                // `account.venue.get` (read-only). `?accountId=` narrows to
                // one account; absent = the whole book. The wire shape is the
                // kernel's own.
                let mut params = serde_json::Map::new();
                if let Some(v) = req.query_param("accountId") {
                    params.insert("accountId".into(), serde_json::json!(v));
                }
                self.proxy_read("account.venue.get", serde_json::Value::Object(params))
            }
            ("POST", "/api/account-venue/set") => {
                // #508: the binding write — the body IS the
                // `account.venue.set` params object
                // (`{accountId, venue, reason?}`), forwarded verbatim. An
                // empty `venue` string unbinds. The kernel validates the
                // name against its market-plugin registry and persists into
                // accounts.toml; the gateway shapes nothing.
                self.proxy_backtest_body("account.venue.set", &req.body)
            }
            ("POST", "/api/display-account/set") => {
                // issue 559 显示账户配置化: the DISPLAY pointer write — the
                // body IS the `display.account.set` params object
                // (`{accountId, reason?}`), forwarded verbatim (same proxy
                // discipline as account-venue/set: the gateway shapes
                // nothing). The kernel validates the plan first (账户存在 +
                // status=active), updates the runtime pointer AND persists
                // the accounts.toml top-level `display_account` key; the
                // session-default switch (`account <id>`) is untouched —
                // a PURE presentation pointer, no ledger/strategy semantics.
                // Old cores answer with the JSON-RPC unknown-method error
                // and the panel falls back to the account-binding chain.
                self.proxy_backtest_body("display.account.set", &req.body)
            }
            ("GET", "/api/arb-config") => {
                // 跨所套利创建（合同 v1）：双腿配置读 —— `arb.config.get`
                // 的薄代理（只读，无参数）。configured=false = 未配置；
                // 老内核不认识该动词时错误照包（`error` 带 rpc -32601 字样）
                // —— 面板据此 feature-detect 降级，网关不代答、不shape。
                self.proxy_read("arb.config.get", serde_json::json!({}))
            }
            ("POST", "/api/arb-config/set") => {
                // 跨所套利创建（合同 v1）：双腿配置写 —— body 即
                // `arb.config.set` 的 params 对象（`{version, legAVenue,
                // legAAccount, legBVenue, legBAccount, pairs}`），逐字转发
                // （同 account-venue/set 的纪律：网关零加工）。B≠A（venue 与
                // 账户两个维度）的校验在内核是 APPLICATION 错误，消息点名
                // 非法字段 —— 原样过线给弹窗转述。持久化（accounts.toml）
                // 在内核侧，网关零持久化职责。
                self.proxy_backtest_body("arb.config.set", &req.body)
            }
            ("POST", "/api/account-trading/set") => {
                // 多账户扇出（PR B of #518）：交易开关写 —— body 即
                // `account.trading.set` 的 params 对象
                // （`{accountId, enabled, reason?}`），逐字转发。关 = 该
                // 账户拒开仓、平仓照放（Gate 2 内核裁决）；persist 失败
                // 内核整体拒绝。网关零加工。
                self.proxy_backtest_body("account.trading.set", &req.body)
            }
            ("POST", "/api/account-freeze/set") => {
                // PR G：账户级急冻（account-scoped kill switch）—— body 即
                // `account.freeze` 的 params 对象
                // （`{accountId, frozen, reason?}`），逐字转发。冻结 = 该
                // 账户拒一切新单（手动+策略 entry）、平仓照放；会话级不
                // 持久化（重启即清，重新探测）。网关零加工。
                self.proxy_backtest_body("account.freeze", &req.body)
            }
            ("GET", "/api/trades-summary") => {
                // PR H (账户筛选修复)：钱包页的闭环统计 —— `trades.summary`
                // 的薄代理（只读）。`?accountId=` 带上时内核按行过滤重算
                // 该账户的 summary（TradeDb 持久 headline 仍是全局）；缺席
                // = 全局 headline，老面板行为逐字节不变。
                let mut params = serde_json::Map::new();
                if let Some(v) = req.query_param("accountId") {
                    params.insert("accountId".into(), serde_json::json!(v));
                }
                self.proxy_read("trades.summary", serde_json::Value::Object(params))
            }
            ("GET", "/api/strategy-stats") => {
                // PR H (账户筛选修复)：策略页的 per-NAME 计数 ——
                // `engine.stats` 的薄代理换成按账户切片的读数。
                // `?accountId=` 带上时内核答 `strategy_stats_for_account`
                // （同形状，counters 只读该账户 slice）；缺席 = 全局
                // fold（行带 `accounts` 数组）。网关零加工。
                let mut params = serde_json::Map::new();
                if let Some(v) = req.query_param("accountId") {
                    params.insert("accountId".into(), serde_json::json!(v));
                }
                self.proxy_read("engine.stats", serde_json::Value::Object(params))
            }
            ("POST", "/api/account-strategies/set") => {
                // 多账户扇出（PR B of #518）：策略分配写 —— body 即
                // `account.strategies.set` 的 params 对象
                // （`{accountId, strategies, reason?}`），逐字转发。空
                // 列表 = 显式不跑（INHERIT 已废除，issue 552 裁决：空集
                // 不跟随全局开关）；非空 = 精确钉住。被移出但持仓的
                // 策略内核以 exits-only 保活（keptExitsOnly 回执带回）。
                // 网关零加工。
                self.proxy_backtest_body("account.strategies.set", &req.body)
            }
            ("POST", "/api/account-create") => {
                // #517 (the add-account dialog): the body IS the
                // `account.create` params object
                // (`{id, name?, venue?, actor?, reason?}`), forwarded
                // verbatim. The kernel refuses duplicates / the reserved
                // `default` / an empty id, persists into accounts.toml and
                // inserts the in-memory ledger; the gateway shapes nothing.
                // Credentials do NOT ride this call — they go through
                // /api/market-credentials/set (values never echo).
                self.proxy_backtest_body("account.create", &req.body)
            }
            ("POST", "/api/account-remove") => {
                // #517: the retirement write — the body IS the
                // `account.remove` params object (`{id, actor?, reason?}`),
                // forwarded verbatim. The kernel refuses `default` and an
                // account with open positions, re-homes the session default
                // if it was the one removed, and persists the book.
                self.proxy_backtest_body("account.remove", &req.body)
            }
            ("POST", "/api/account-rounds/set") => {
                // 轮次周期集写（account.rounds.set，轮次多族 B2）—— body 即
                // params 对象（`{accountId, rounds, reason?}`），逐字转发
                // （同 account-trading/set 的代理纪律：网关零加工）。
                // `rounds` = 白名单秒数数组（空数组 = 未声明限制，许可轴空集
                // 放行）；移除有敞口的族被内核整体拒绝 —— APPLICATION 错误
                // 原文过线给卡面转述。老内核不认识该动词 → JSON-RPC
                // unknown-method 错误原样返回，面板 feature-detect 降级只读。
                self.proxy_backtest_body("account.rounds.set", &req.body)
            }
            ("POST", "/api/strategy-enable") => {
                // 账户↔venue 解耦（本轮）：策略启用写的账户级通道 —— body 即
                // `strategy.enable` 的 params 对象（`{name, enabled,
                // accountId?}`），逐字转发（同 account-create/remove 的代理
                // 纪律：网关零加工）。`accountId` 缺省/空 = 内核的 default
                // 分支（全局开关，回放基线字节不变）；非 default = 内核的
                // assignment IS enablement 分支（账户级启停，回执带
                // `accountId`/`keptExitsOnly`）。前端文本动词通道（/api/command
                // 的 `strategy <name> on|off`）保持原样 —— 旧面板路径不变。
                self.proxy_backtest_body("strategy.enable", &req.body)
            }
            ("GET", "/api/plugins") => {
                // E9-g: the registry trio the TUI Plugins page shows (strategies
                // / extensions / market plugins) in one authenticated call.
                let doc = match self.dispatcher {
                    Some(ref d) => match d.lock() {
                        Ok(mut dp) => {
                            let snap = dp.plugins_snapshot();
                            serde_json::json!({
                                "connected": snap.connected,
                                "strategies": snap.strategies,
                                "extensions": snap.extensions,
                                "marketPlugins": snap.market_plugins,
                                "marketActive": snap.market_active,
                                "lastError": snap.last_error,
                            })
                        }
                        Err(_) => serde_json::json!({"connected": false,
                            "lastError": "dispatcher poisoned"}),
                    },
                    None => {
                        // read-only mode: registry via the snapshot client
                        let mut doc = serde_json::json!({"connected": false});
                        doc["lastError"] =
                            serde_json::json!("plugin registry requires gateway (--manage)");
                        doc
                    }
                };
                (200, "application/json", doc.to_string().into_bytes())
            }
            ("GET", "/api/netcheck") => {
                // Network diagnosis (`net.check`): the paths this venue trades
                // over, walked resolver → TCP → TLS → one cheap request each.
                //
                // Answered from a cache, never probed inline: `serve()` accepts
                // in ONE thread, so a handler that dials for ten seconds freezes
                // every other request in this process — the panel's own snapshot
                // poll included. The route starts a background probe when the
                // cached report is stale and answers immediately; `probing` says
                // whether a fresh one is still coming. See [`NetCheckCache`].
                (200, "application/json", self.net_check_poll().into_bytes())
            }
            ("POST", "/api/netcheck/probe") => {
                // The 网络诊断 card's probe button. A separate route rather than
                // a `?force=1` because `handle` matches on `req.path()`, which
                // has already dropped the query string.
                //
                // It still does not dial inline (the cache rule above): the
                // timestamp is dropped so the next poll starts a probe, and the
                // caller gets the same document — `probing: true` until the
                // fresh report lands. A probe already in flight is left alone:
                // its result IS the answer to this request.
                self.net_check_invalidate();
                (200, "application/json", self.net_check_poll().into_bytes())
            }
            ("POST", "/api/version/check") => {
                // The 设置页's 检查更新 button (VERSIONING.md §6.3). Never
                // inline — the kernel dials GitHub with its own timeout — so
                // this starts the background ask and answers at once; the card
                // polls the snapshot until the kernel's own state moves. A
                // kernel-side refusal (checkEnabled=false) is instant and lands
                // in the cache's error, which the next GET rides along with.
                (
                    200,
                    "application/json",
                    self.version_check_start().into_bytes(),
                )
            }
            ("POST", "/api/version/stage") => {
                // #379: the 设置页's 暂存下载 button (§7.5). Like the check
                // route: never inline — the download has its own timeout — so
                // this starts the background ask and answers at once; the card
                // polls the snapshot's `stageState` until the phase moves. A
                // kernel-side refusal (autoUpdate=false) lands in the cache's
                // error and rides the next poll.
                (
                    200,
                    "application/json",
                    self.version_stage_start().into_bytes(),
                )
            }
            ("POST", "/api/version/configure") => {
                // The update switches (#393): {"autoUpdate": bool} and/or
                // {"checkEnabled": bool} — at least one. Inline is fine —
                // the kernel answers in milliseconds (a small file write plus
                // an audit line).
                (
                    200,
                    "application/json",
                    self.version_configure(&req.body).into_bytes(),
                )
            }
            ("GET", "/api/snapshot") => {
                let snap = self.snapshot();
                let lifecycle = self.lifecycle_view();
                (
                    200,
                    "application/json",
                    render_json_full(&snap, lifecycle.as_ref(), Some(self.security_doc()))
                        .into_bytes(),
                )
            }
            ("GET", "/api/command") | ("POST", "/api/command") => {
                let cmd = if req.method == "POST" {
                    body_to_command(&req.body)
                } else {
                    req.query_cmd().unwrap_or_default()
                };
                (200, "application/json", self.run_command(&cmd).into_bytes())
            }
            ("POST", "/api/login") => {
                // body: {"user":"…","password":"…"} → {"ok":true,"token":…,
                // "user":…} so the WebUI can store it as a session.
                //
                // #185: every failure is counted against the CLIENT ADDRESS, and a
                // client that has failed enough is refused outright. The refusal
                // happens BEFORE the credentials are examined, which is what keeps
                // the lockout from leaking whether a guessed password was right:
                // the answer is the same for every input while locked.
                let key = Self::login_client_key(&stream);
                if let Some(secs) = self.login_lockout_remaining(&key) {
                    retry_after = Some(secs);
                    eprintln!(
                        "panel: login refused — {key} is locked out for another {secs}s \
                         ({MAX_LOGIN_FAILURES} failures reached)"
                    );
                    let doc = serde_json::json!({
                        "ok": false,
                        "locked": true,
                        "retryAfterSec": secs,
                        "error": format!(
                            "登录失败次数过多，已暂时锁定，请在 {secs} 秒后重试"
                        ),
                    });
                    (
                        429,
                        "application/json; charset=utf-8",
                        serde_json::to_string(&doc).unwrap_or_default().into_bytes(),
                    )
                } else {
                    let doc = match parse_login_body(&req.body) {
                        Some((u, p)) => match self.login(&u, &p) {
                            Some(tok) => {
                                self.note_login_success(&key);
                                serde_json::json!({
                                    "ok": true, "token": tok, "user": u,
                                })
                            }
                            None => {
                                let failures = self.note_login_failure(&key);
                                // Loud, and deliberately without the submitted
                                // username or password: the log records that an
                                // attempt failed and how close the client is to
                                // the lockout, never what was tried.
                                eprintln!(
                                    "panel: failed login from {key} \
                                     ({failures}/{MAX_LOGIN_FAILURES}){}",
                                    if failures >= MAX_LOGIN_FAILURES {
                                        " — client locked out"
                                    } else {
                                        ""
                                    }
                                );
                                respond_then_delay_ms = Some(login_failure_delay_ms(failures));
                                serde_json::json!({
                                    "ok": false, "error": "用户名或密码错误",
                                })
                            }
                        },
                        None => serde_json::json!({
                            "ok": false, "error": "请求格式错误（需要 JSON {user, password}）",
                        }),
                    };
                    let status = if doc["ok"] == serde_json::json!(true) {
                        200
                    } else {
                        401
                    };
                    (
                        status,
                        "application/json; charset=utf-8",
                        serde_json::to_string(&doc).unwrap_or_default().into_bytes(),
                    )
                }
            }
            ("GET", "/api/logout") | ("POST", "/api/logout") => {
                // Session token via any authenticated channel; revoke it.
                // Harmless if absent. (#460: the query channel is gone —
                // see `session_token`.)
                let token = self
                    .token_from_cookies(&req)
                    .or_else(|| req.header("x-auth-token").map(String::from))
                    .or_else(|| req.header("authorization").and_then(bearer_or_basic))
                    .unwrap_or_default();
                self.logout(&token);
                (200, "application/json", b"{\"ok\":true}".to_vec())
            }
            ("GET", "/") | ("GET", "/panel") | ("GET", "/panel/") => {
                // /panel is the canonical entry: the E9-g Vue app when built
                // (ui/webapp/webui/dist), else the built-in HTML panel. `/`
                // redirects so human-typed origins always land in the same place.
                if target != "/panel" && target != "/panel/" {
                    let head = "HTTP/1.1 302 Found\r\nLocation: /panel\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.flush();
                    return;
                }
                match serve_vue_panel() {
                    Some((body, ctype)) => (200, ctype, body),
                    None => {
                        let snap = self.snapshot();
                        let lifecycle = self.lifecycle_view();
                        (
                            200,
                            "text/html; charset=utf-8",
                            render_html_full(
                                &snap,
                                self.dispatcher.is_some(),
                                lifecycle.as_ref(),
                                self.bind_warning().as_deref(),
                            )
                            .into_bytes(),
                        )
                    }
                }
            }
            ("GET", path) if path.starts_with("/panel/") => {
                // Vue app assets (`/panel/assets/*.js|css`, favicon, …).
                match serve_vue_panel_asset(path) {
                    Some((body, ctype)) => (200, ctype, body),
                    None => (404, "text/plain; charset=utf-8", b"not found".to_vec()),
                }
            }
            ("GET", _) => {
                let snap = self.snapshot();
                let lifecycle = self.lifecycle_view();
                (
                    200,
                    "text/html; charset=utf-8",
                    render_html_full(
                        &snap,
                        self.dispatcher.is_some(),
                        lifecycle.as_ref(),
                        self.bind_warning().as_deref(),
                    )
                    .into_bytes(),
                )
            }
            _ => (404, "text/plain; charset=utf-8", b"not found".to_vec()),
        };

        if std::env::var("UIKIT_WEB_TRACE").is_ok() {
            eprintln!(
                "ui_kit web: {} {} -> {} bytes ({status})",
                req.method,
                target,
                body.len()
            );
        }
        let reason = match status {
            200 => "OK",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            429 => "Too Many Requests",
            _ => "Not Found",
        };
        let extra = retry_after
            .map(|s| format!("Retry-After: {s}\r\n"))
            .unwrap_or_default();
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n{extra}Connection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
        let _ = stream.flush();
        // #460: the failed-login speed bump — after the response is on the
        // wire, so only the attacker's own request pays it.
        if let Some(delay) = respond_then_delay_ms {
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
    }
}
