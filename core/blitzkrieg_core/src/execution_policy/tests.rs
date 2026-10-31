//! Split out of the parent module (issue #780): pure move.

#[cfg(test)]
mod tests {
    use super::super::*;

    /// The classic 10%/$1/$50 section the first 43 tests were written
    /// against — now the SHIPPED FILE's defaults, not the compiled ones
    /// (compiled defaults are the neutral kernel; see `code_defaults`).
    fn classic() -> SectionDefaults {
        SectionDefaults {
            budget_ratio: dec!(0.10),
            min_budget_usd: dec!(1.00),
            max_budget_usd: dec!(50.00),
            min_equity_usd: dec!(1.00),
            max_positions_per_asset: 1,
            rules: vec![Rule {
                name: "global_backstop".to_string(),
                priority: 100,
                enabled: true,
                when: When {
                    field: ConditionField::OpenPositions,
                    op: CompareOp::Ge,
                    value: ConditionValue::Num(Decimal::from(2)),
                },
                then: Action {
                    action: Some(SkipAction::Skip),
                    budget_ratio: None,
                    min_budget_usd: None,
                    max_budget_usd: None,
                    cooldown_sec: None,
                },
                reason: Some("全局兜底：持仓数达上限".to_string()),
            }],
        }
    }

    fn classic_policy() -> Policy {
        Policy {
            defaults: classic(),
            accounts: HashMap::new(),
        }
    }

    fn input(balance: i64, open: u32) -> PolicyInput {
        PolicyInput {
            available_balance: Decimal::from(balance),
            total_equity: Decimal::from(balance),
            open_positions: open,
            current_price: dec!(0.50),
            time_left_sec: 300,
            symbol: "BTC".to_string(),
            recent_pnl_1h: Decimal::ZERO,
            consecutive_losses: 0,
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn no_rule_hit_uses_defaults() {
        let policy = classic_policy();
        let out = evaluate(&policy, "default", &input(100, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        assert_eq!(budget_usd, dec!(10.00)); // 100 × 0.10
    }

    #[test]
    fn first_hit_wins_among_holders() {
        let mut policy = classic_policy();
        policy.defaults.rules = vec![skip_rule(), ratio_rule()];
        let out = evaluate(&policy, "default", &input(100, 5)).unwrap();
        assert!(matches!(out, PolicyOutput::Skip { .. }));
    }

    #[test]
    fn lower_priority_number_beats_higher() {
        let mut policy = classic_policy();
        let mut first = ratio_rule();
        first.priority = 5;
        let mut second = skip_rule();
        second.priority = 10;
        policy.defaults.rules = vec![second, first];
        let out = evaluate(&policy, "default", &input(19, 5)).unwrap();
        let PolicyOutput::Place { ratio_used, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        assert_eq!(ratio_used, dec!(0.05));
    }

    fn ratio_rule() -> Rule {
        Rule {
            name: "small_balance_ratio".into(),
            priority: 100,
            enabled: true,
            when: When {
                field: ConditionField::AvailableBalance,
                op: CompareOp::Lt,
                value: ConditionValue::Num(Decimal::from(20)),
            },
            then: Action {
                action: None,
                budget_ratio: Some(dec!(0.05)),
                min_budget_usd: None,
                max_budget_usd: None,
                cooldown_sec: None,
            },
            reason: Some("小余额降杠杆".into()),
        }
    }

    fn skip_rule() -> Rule {
        Rule {
            name: "backstop".into(),
            priority: 100,
            enabled: true,
            when: When {
                field: ConditionField::OpenPositions,
                op: CompareOp::Ge,
                value: ConditionValue::Num(Decimal::from(2)),
            },
            then: Action {
                action: Some(SkipAction::Skip),
                budget_ratio: None,
                min_budget_usd: None,
                max_budget_usd: None,
                cooldown_sec: None,
            },
            reason: Some("全局兜底：持仓数达上限".into()),
        }
    }

    #[test]
    fn balance_below_threshold_uses_lower_ratio() {
        let mut policy = classic_policy();
        policy.defaults.rules = vec![ratio_rule()];
        let out = evaluate(&policy, "default", &input(19, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        // 19 × 0.05 = 0.95 → floored to min_budget_usd 1.00
        assert_eq!(budget_usd, dec!(1.00));
    }

    #[test]
    fn open_positions_ge_2_skips() {
        let policy = classic_policy();
        let out = evaluate(&policy, "default", &input(100, 2)).unwrap();
        let PolicyOutput::Skip { reason } = out else {
            panic!("expected Skip, got {out:?}")
        };
        assert!(reason.contains("全局兜底"));
    }

    #[test]
    fn min_budget_floor_applies() {
        let policy = classic_policy();
        let out = evaluate(&policy, "default", &input(5, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        // 5 × 0.10 = 0.50 → floor 1.00
        assert_eq!(budget_usd, dec!(1.00));
    }

    #[test]
    fn max_budget_ceiling_applies() {
        let policy = classic_policy();
        let out = evaluate(&policy, "default", &input(1_000, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        // 1000 × 0.10 = 100 → ceiling 50
        assert_eq!(budget_usd, dec!(50.00));
    }

    #[test]
    fn rule_cannot_lift_budget_above_section_ceiling() {
        // A rule with budget_ratio = 1 (allowed range) on a huge balance: the
        // SECTION ceiling (50) still binds — the clamp runs on the one budget
        // constructor no path skips.
        let mut policy = classic_policy();
        policy.defaults.rules = vec![Rule {
            name: "all_in".into(),
            priority: 1,
            enabled: true,
            when: When {
                field: ConditionField::Symbol,
                op: CompareOp::Eq,
                value: ConditionValue::Text("BTC".into()),
            },
            then: Action {
                action: None,
                budget_ratio: Some(Decimal::ONE),
                min_budget_usd: None,
                max_budget_usd: None,
                cooldown_sec: None,
            },
            reason: None,
        }];
        let out = evaluate(&policy, "default", &input(10_000, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        assert_eq!(budget_usd, dec!(50.00));
    }

    #[test]
    fn rule_max_override_cannot_widen_ceiling() {
        // then = { max_budget_usd = 1000 } on a section ceiling of 50: the
        // override can only TIGHTEN. Budget stays ≤ 50.
        let mut policy = classic_policy();
        policy.defaults.rules = vec![Rule {
            name: "widen".into(),
            priority: 1,
            enabled: true,
            when: When {
                field: ConditionField::Symbol,
                op: CompareOp::Eq,
                value: ConditionValue::Text("BTC".into()),
            },
            then: Action {
                action: None,
                budget_ratio: None,
                min_budget_usd: None,
                max_budget_usd: Some(dec!(1000)),
                cooldown_sec: None,
            },
            reason: None,
        }];
        let out = evaluate(&policy, "default", &input(10_000, 0)).unwrap();
        let PolicyOutput::Place {
            budget_usd,
            max_used,
            ..
        } = out
        else {
            panic!("expected Place, got {out:?}")
        };
        assert_eq!(budget_usd, dec!(50.00));
        assert_eq!(max_used, dec!(50.00));
    }

    #[test]
    fn rule_max_override_can_tighten_ceiling() {
        let mut policy = classic_policy();
        policy.defaults.rules = vec![Rule {
            name: "tighten".into(),
            priority: 1,
            enabled: true,
            when: When {
                field: ConditionField::Symbol,
                op: CompareOp::Eq,
                value: ConditionValue::Text("BTC".into()),
            },
            then: Action {
                action: None,
                budget_ratio: None,
                min_budget_usd: None,
                max_budget_usd: Some(dec!(2)),
                cooldown_sec: None,
            },
            reason: None,
        }];
        let out = evaluate(&policy, "default", &input(1000, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        assert_eq!(budget_usd, dec!(2.00));
    }

    #[test]
    fn min_equity_floor_skips_before_rules() {
        let mut policy = classic_policy();
        policy.defaults.rules = Vec::new();
        policy.defaults.min_equity_usd = dec!(10);
        let mut facts = input(5, 0);
        facts.total_equity = dec!(5);
        let out = evaluate(&policy, "default", &facts).unwrap();
        let PolicyOutput::Skip { reason } = out else {
            panic!("expected Skip, got {out:?}")
        };
        assert!(reason.contains("min_equity_usd"));
        // At the floor (or above) it places again.
        facts.total_equity = dec!(10);
        assert!(matches!(
            evaluate(&policy, "default", &facts).unwrap(),
            PolicyOutput::Place { .. }
        ));
    }

    #[test]
    fn account_isolation_a_vs_b() {
        let mut policy = classic_policy();
        policy.accounts.insert(
            "binance-main".to_string(),
            SectionDefaults {
                budget_ratio: dec!(0.20),
                ..code_defaults()
            },
        );
        let a = evaluate(&policy, "binance-main", &input(100, 0)).unwrap();
        let b = evaluate(&policy, "other", &input(100, 0)).unwrap();
        let PolicyOutput::Place {
            budget_usd: a_usd, ..
        } = a
        else {
            panic!("expected Place")
        };
        let PolicyOutput::Place {
            budget_usd: b_usd, ..
        } = b
        else {
            panic!("expected Place")
        };
        assert_eq!(a_usd, dec!(20.00));
        assert_eq!(b_usd, dec!(10.00));
    }

    #[test]
    fn unconfigured_account_inherits_defaults() {
        let mut policy = classic_policy();
        policy.accounts.insert(
            "binance-main".to_string(),
            SectionDefaults {
                budget_ratio: dec!(0.20),
                ..code_defaults()
            },
        );
        let out = evaluate(&policy, "totally-unconfigured", &input(100, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place")
        };
        assert_eq!(budget_usd, dec!(10.00));
    }

    #[test]
    fn missing_account_id_refuses() {
        let policy = classic_policy();
        let err = evaluate(&policy, "", &input(100, 0)).unwrap_err();
        assert!(err.reason.contains("no account_id"));
        let err = evaluate(&policy, "   ", &input(100, 0)).unwrap_err();
        assert!(err.reason.contains("no account_id"));
    }

    #[test]
    fn disabled_rule_does_not_fire() {
        let mut policy = classic_policy();
        let mut r = skip_rule();
        r.enabled = false;
        policy.defaults.rules = vec![r];
        let out = evaluate(&policy, "default", &input(100, 2)).unwrap();
        assert!(matches!(out, PolicyOutput::Place { .. }));
    }

    #[test]
    fn parse_valid_toml() {
        let text = r#"
version = 1
[defaults]
budget_ratio = 0.10
min_budget_usd = 1.00
max_budget_usd = 50.00
min_equity_usd = 1.00
max_positions_per_asset = 1

[[defaults.rules]]
name = "global_backstop"
enabled = true
priority = 100
when = "open_positions >= 2"
then = { action = "skip" }
reason = "全局兜底：持仓数达上限"

[accounts.binance-main]
budget_ratio = 0.05

[[accounts.binance-main.rules]]
name = "tiny"
enabled = true
priority = 1
when = "available_balance < 20"
then = { budget_ratio = 0.05 }
"#;
        let policy = Policy::load_with_env(text, no_env).unwrap();
        assert_eq!(policy.defaults.budget_ratio, dec!(0.10));
        assert_eq!(policy.defaults.max_budget_usd, dec!(50.00));
        assert_eq!(policy.defaults.max_positions_per_asset, 1);
        assert_eq!(policy.defaults.rules.len(), 1);
        let acct = policy.accounts.get("binance-main").unwrap();
        assert_eq!(acct.budget_ratio, dec!(0.05));
        assert_eq!(acct.rules.len(), 1);
        // The account kept the global backstop? No: its own rules REPLACE.
        assert_eq!(acct.rules[0].name, "tiny");
    }

    #[test]
    fn toml_float_arrives_as_clean_decimal() {
        // The precision rule: 0.10 must equal dec!(0.10) — not the binary
        // expansion the default serde path would carry.
        let policy =
            Policy::load_with_env("version = 1\n[defaults]\nbudget_ratio = 0.10\n", no_env)
                .unwrap();
        assert_eq!(policy.defaults.budget_ratio, dec!(0.10));
    }

    #[test]
    fn prefixed_env_keys_layer_over_toml_and_outrank_it() {
        // The issue 751 family: POLICY_* beats the TOML value; every decimal
        // knob + the position cap + RULES are settable from env alone.
        let policy = Policy::load_with_env(
            "version = 1\n[defaults]\nbudget_ratio = 0.10\nmax_budget_usd = 50\n",
            |k| match k {
                "POLICY_BUDGET_RATIO" => Some("0.02".into()),
                "POLICY_MIN_BUDGET_USD" => Some("1".into()),
                "POLICY_MAX_BUDGET_USD" => Some("30".into()),
                "POLICY_MIN_EQUITY_USD" => Some("2".into()),
                "POLICY_MAX_POSITIONS_PER_ASSET" => Some("3".into()),
                _ => None,
            },
        )
        .unwrap();
        assert_eq!(policy.defaults.budget_ratio, dec!(0.02));
        assert_eq!(policy.defaults.min_budget_usd, dec!(1));
        assert_eq!(policy.defaults.max_budget_usd, dec!(30));
        assert_eq!(policy.defaults.min_equity_usd, dec!(2));
        assert_eq!(policy.defaults.max_positions_per_asset, 3);
    }

    #[test]
    fn bare_env_names_stay_as_read_only_aliases() {
        // The pre-751 spelling still works, and a POLICY_-prefixed twin
        // outranks it (the write face only ever writes the prefixed family).
        let prefixed = Policy::load_with_env("version = 1\n[defaults]\n", |k| {
            (k == "POLICY_BUDGET_RATIO").then(|| "0.01".to_string())
        })
        .unwrap();
        assert_eq!(prefixed.defaults.budget_ratio, dec!(0.01));
        let alias = Policy::load_with_env("version = 1\n[defaults]\n", |k| {
            (k == "BUDGET_RATIO").then(|| "0.02".to_string())
        })
        .unwrap();
        assert_eq!(alias.defaults.budget_ratio, dec!(0.02));
        let both = Policy::load_with_env("version = 1\n[defaults]\n", |k| match k {
            "POLICY_BUDGET_RATIO" => Some("0.01".into()),
            "BUDGET_RATIO" => Some("0.02".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(both.defaults.budget_ratio, dec!(0.01));
    }

    #[test]
    fn env_only_account_section_forms_from_the_account_list() {
        // An ACCOUNT_LIST id carrying POLICY_<ID>_* keys but no TOML row
        // still gets a section (defaults < env) — the per-account surface
        // survives the toml's retirement.
        let policy =
            Policy::load_with_env(
                "version = 1\n[defaults]\nbudget_ratio = 0.10\n",
                |k| match k {
                    "ACCOUNT_LIST" => Some("default,hft-9".into()),
                    "POLICY_BUDGET_RATIO_HFT_9" => Some("0.07".into()),
                    _ => None,
                },
            )
            .unwrap();
        assert_eq!(policy.section_for("hft-9").budget_ratio, dec!(0.07));
        assert_eq!(policy.section_for("default").budget_ratio, dec!(0.10));
    }

    #[test]
    fn env_rules_json_parses_and_replaces_the_section() {
        let rules = r#"[{"name":"tiny","priority":1,"enabled":true,"when":"available_balance < 20","then":{"budget_ratio":0.05}}]"#;
        let policy = Policy::load_with_env("version = 1\n[accounts.acct-a]\n", |k| match k {
            "ACCOUNT_LIST" => Some("acct-a".into()),
            "POLICY_RULES_ACCT_A" => Some(rules.into()),
            _ => None,
        })
        .unwrap();
        let acct = policy.accounts.get("acct-a").unwrap();
        assert_eq!(acct.rules.len(), 1);
        assert_eq!(acct.rules[0].name, "tiny");
        // A malformed array refuses with the key named (fail-closed).
        let err = Policy::load_with_env("version = 1\n[accounts.acct-a]\n", |k| match k {
            "ACCOUNT_LIST" => Some("acct-a".into()),
            "POLICY_RULES_ACCT_A" => Some("[{not json".into()),
            _ => None,
        })
        .unwrap_err();
        assert!(err.contains("POLICY_RULES_ACCT_A"), "{err}");
    }

    #[test]
    fn missing_toml_still_layers_the_env() {
        // load() on a NotFound path is no longer a bare default: the POLICY_*
        // process env IS the whole surface once the toml retires.
        let policy = Policy::load_with_env("version = 1\n", |k| {
            (k == "POLICY_BUDGET_RATIO").then(|| "0.03".to_string())
        })
        .unwrap();
        assert_eq!(policy.defaults.budget_ratio, dec!(0.03));
    }

    #[test]
    fn invalid_version_refuses() {
        let err = Policy::load_with_env("version = 2\n", no_env).unwrap_err();
        assert!(err.contains("unsupported"), "{err}");
    }

    #[test]
    fn unknown_field_refuses() {
        let err = Policy::load_with_env("version = 1\n[defaults]\nbudget_ration = 0.1\n", no_env)
            .unwrap_err();
        assert!(err.contains("does not parse"), "{err}");
    }

    #[test]
    fn out_of_range_ratio_refuses() {
        let err = Policy::load_with_env(
            "version = 1\n[defaults]\nbudget_ratio = 1.5\nmax_budget_usd = 50\nmin_budget_usd = 1\n",
            no_env,
        )
        .unwrap_err();
        assert!(err.contains("outside (0, 1]"), "{err}");
    }

    #[test]
    fn per_account_out_of_range_refuses() {
        let err = Policy::load_with_env("version = 1\n[accounts.bad]\nbudget_ratio = 0\n", no_env)
            .unwrap_err();
        assert!(err.contains("[accounts.bad]"), "{err}");
    }

    #[test]
    fn missing_required_field_refuses() {
        // A rule without `when`.
        let err = Policy::load_with_env(
            "version = 1\n[[defaults.rules]]\nname = \"x\"\nenabled = true\nthen = { action = \"skip\" }\n",
            no_env,
        )
        .unwrap_err();
        assert!(err.contains("does not parse"), "{err}");
    }

    #[test]
    fn rule_with_two_actions_refuses() {
        let err = Policy::load_with_env(
            "version = 1\n[[defaults.rules]]\nname = \"x\"\nenabled = true\nwhen = \"symbol == \\\"BTC\\\"\"\nthen = { action = \"skip\", budget_ratio = 0.5 }\n",
            no_env,
        )
        .unwrap_err();
        assert!(err.contains("exactly one action"), "{err}");
    }

    #[test]
    fn max_below_min_refuses() {
        let err = Policy::load_with_env(
            "version = 1\n[defaults]\nbudget_ratio = 0.1\nmin_budget_usd = 10\nmax_budget_usd = 5\n",
            no_env,
        )
        .unwrap_err();
        assert!(err.contains("must not sit below the floor"), "{err}");
    }

    #[test]
    fn env_overrides_toml_globally() {
        let text = "version = 1\n[defaults]\nbudget_ratio = 0.10\n";
        let policy =
            Policy::load_with_env(text, |k| (k == "BUDGET_RATIO").then(|| "0.02".to_string()))
                .unwrap();
        assert_eq!(policy.defaults.budget_ratio, dec!(0.02));
    }

    #[test]
    fn per_account_env_overrides_per_account_toml() {
        let text = "version = 1\n[defaults]\nbudget_ratio = \"0.10\"\n\n[accounts.binance-main]\nbudget_ratio = \"0.05\"\n";
        let policy = Policy::load_with_env(text, |k| {
            (k == "BUDGET_RATIO_BINANCE_MAIN").then(|| "0.07".to_string())
        })
        .unwrap();
        assert_eq!(policy.accounts["binance-main"].budget_ratio, dec!(0.07));
        // Another account keeps its own (or the global) layer.
        assert_eq!(policy.defaults.budget_ratio, dec!(0.10));
    }

    #[test]
    fn global_env_sits_below_account_toml() {
        let text = "version = 1\n[accounts.binance-main]\nbudget_ratio = 0.05\n";
        let policy =
            Policy::load_with_env(text, |k| (k == "BUDGET_RATIO").then(|| "0.02".to_string()))
                .unwrap();
        // 全局 env (0.02) > 全局 TOML (0.10)，但账户级 TOML (0.05) 又高于全局 env。
        assert_eq!(policy.accounts["binance-main"].budget_ratio, dec!(0.05));
        assert_eq!(policy.defaults.budget_ratio, dec!(0.02));
    }

    #[test]
    fn account_env_does_not_touch_other_accounts() {
        let text =
            "version = 1\n[accounts.a]\nbudget_ratio = 0.05\n[accounts.b]\nbudget_ratio = 0.06\n";
        let policy = Policy::load_with_env(text, |k| {
            (k == "BUDGET_RATIO_A").then(|| "0.09".to_string())
        })
        .unwrap();
        assert_eq!(policy.accounts["a"].budget_ratio, dec!(0.09));
        assert_eq!(policy.accounts["b"].budget_ratio, dec!(0.06));
    }

    #[test]
    fn malformed_env_value_refuses_load() {
        let text = "version = 1\n";
        let err = Policy::load_with_env(text, |k| (k == "BUDGET_RATIO").then(|| "abc".to_string()))
            .unwrap_err();
        assert!(err.contains("BUDGET_RATIO"), "{err}");
    }

    #[test]
    fn when_string_form_parses_and_holds() {
        let w = parse_when("open_positions >= 2").unwrap();
        assert_eq!(
            w,
            When {
                field: ConditionField::OpenPositions,
                op: CompareOp::Ge,
                value: ConditionValue::Num(Decimal::from(2)),
            }
        );
        assert!(w.holds(&input(100, 2)));
        assert!(!w.holds(&input(100, 1)));
    }

    #[test]
    fn when_symbol_in_list_parses_and_holds() {
        let w = parse_when("symbol in [\"BTC\", \"ETH\"]").unwrap();
        assert!(matches!(w.value, ConditionValue::TextList(_)));
        assert!(w.holds(&input(100, 0)));
        let mut other = input(100, 0);
        other.symbol = "SOL".to_string();
        assert!(!w.holds(&other));
    }

    #[test]
    fn when_all_operators() {
        let i = input(100, 1);
        assert!(parse_when("available_balance < 101").unwrap().holds(&i));
        assert!(parse_when("available_balance <= 100").unwrap().holds(&i));
        assert!(parse_when("available_balance > 99").unwrap().holds(&i));
        assert!(parse_when("available_balance >= 100").unwrap().holds(&i));
        assert!(parse_when("available_balance == 100").unwrap().holds(&i));
        assert!(parse_when("available_balance != 99").unwrap().holds(&i));
        assert!(parse_when("symbol in [\"BTC\"]").unwrap().holds(&i));
        assert!(!parse_when("available_balance < 100").unwrap().holds(&i));
    }

    #[test]
    fn when_unknown_field_refuses() {
        assert!(parse_when("price_of_bitcoin > 2").is_err());
        assert!(parse_when("no operator here").is_err());
    }

    #[test]
    fn when_symbol_vs_number_refuses() {
        assert!(parse_when("symbol == 5").is_err());
        assert!(parse_when("available_balance < \"BTC\"").is_err());
    }

    #[test]
    fn when_recent_pnl_and_losses_holders() {
        let mut i = input(100, 0);
        i.recent_pnl_1h = dec!(-2.5);
        i.consecutive_losses = 3;
        assert!(parse_when("recent_pnl_1h < 0").unwrap().holds(&i));
        assert!(parse_when("consecutive_losses >= 3").unwrap().holds(&i));
        assert!(!parse_when("consecutive_losses > 3").unwrap().holds(&i));
    }

    #[test]
    fn env_account_suffix_mapping() {
        assert_eq!(env_account_suffix("binance-main"), "BINANCE_MAIN");
        assert_eq!(env_account_suffix("default"), "DEFAULT");
        assert_eq!(env_account_suffix("a.b:c"), "A_B_C");
    }

    #[test]
    fn cooldown_rule_reports_cooldown_not_skip() {
        let mut policy = classic_policy();
        policy.defaults.rules = vec![Rule {
            name: "cool".into(),
            priority: 1,
            enabled: true,
            when: When {
                field: ConditionField::ConsecutiveLosses,
                op: CompareOp::Ge,
                value: ConditionValue::Num(Decimal::from(3)),
            },
            then: Action {
                action: None,
                budget_ratio: None,
                min_budget_usd: None,
                max_budget_usd: None,
                cooldown_sec: Some(60),
            },
            reason: None,
        }];
        let mut i = input(100, 0);
        i.consecutive_losses = 3;
        let out = evaluate(&policy, "default", &i).unwrap();
        assert_eq!(out, PolicyOutput::Cooldown { seconds: 60 });
        // Non-positive cooldown is refused at evaluate time.
        policy.defaults.rules[0].then.cooldown_sec = Some(0);
        let err = evaluate(&policy, "default", &i).unwrap_err();
        assert!(err.reason.contains("must be > 0"));
    }

    #[test]
    fn audit_record_round_trips_through_json() {
        let rec = PolicyAuditRecord {
            ts_ms: 1,
            actor: ACTOR_IPC.to_string(),
            action: "set".to_string(),
            account_id: "default".to_string(),
            before: None,
            after: Some(serde_json::json!({"budget_ratio": "0.05"})),
            error: None,
        };
        let line = serde_json::to_string(&rec).unwrap();
        let back: PolicyAuditRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.account_id, "default");
        assert_eq!(back.action, "set");
        assert_eq!(back.actor, "ipc");
    }

    #[test]
    fn ratio_of_exactly_one_is_allowed() {
        let mut policy = classic_policy();
        policy.defaults.budget_ratio = Decimal::ONE;
        let out = evaluate(&policy, "default", &input(10, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place")
        };
        assert_eq!(budget_usd, dec!(10.00));
    }

    #[test]
    fn empty_account_section_inherits_globals() {
        // The globals come from the FILE's [defaults] here — including a
        // rule — while the compiled defaults are the neutral kernel (no
        // rules of their own), so anything the section carries came from
        // the file.
        let policy = Policy::load_with_env(
            "version = 1\n\n[defaults]\nbudget_ratio = \"0.10\"\nmin_budget_usd = \"1\"\n\n\
             [[defaults.rules]]\nname = \"file_rule\"\nenabled = true\npriority = 100\n\
             when = \"open_positions >= 2\"\nthen = { action = \"skip\" }\n\n\
             [accounts.empty-acct]\n",
            no_env,
        )
        .unwrap();
        let section = policy.section_for("empty-acct");
        assert_eq!(section.budget_ratio, dec!(0.10));
        // The section carries no `rules` of its own, so it inherits the
        // globals'. "未配置账户继承 defaults" means the WHOLE resolved
        // section, rules included.
        assert_eq!(section.rules.len(), 1, "inherited the file's rule");
        assert_eq!(section.rules[0].name, "file_rule");
    }

    #[test]
    fn section_default_for_ipc_serialization() {
        // The IPC layer stores section summaries as JSON; Decimal fields are
        // strings on that wire (the crate's decimal convention).
        let section = SectionFields {
            budget_ratio: Some(dec!(0.05)),
            ..SectionFields::default()
        };
        let v = serde_json::to_value(&section).unwrap();
        assert_eq!(v["budget_ratio"], serde_json::json!("0.05"));
        let back: SectionFields = serde_json::from_value(v).unwrap();
        assert_eq!(back.budget_ratio, Some(dec!(0.05)));
    }

    #[test]
    fn when_table_form_from_toml() {
        let text = r#"
version = 1
[[defaults.rules]]
name = "t"
enabled = true
when = { field = "open_positions", op = ">=", value = 2 }
then = { action = "skip" }
"#;
        let policy = Policy::load_with_env(text, no_env).unwrap();
        assert!(matches!(
            evaluate(&policy, "default", &input(100, 2)).unwrap(),
            PolicyOutput::Skip { .. }
        ));
    }

    #[test]
    fn mixed_field_value_pair_in_table_form_refuses() {
        let text = r#"
version = 1
[[defaults.rules]]
name = "t"
enabled = true
when = { field = "symbol", op = "==", value = 5 }
then = { action = "skip" }
"#;
        let err = Policy::load_with_env(text, no_env).unwrap_err();
        assert!(err.contains("compares against text"), "{err}");
    }

    // ── #757: a same-process reload must not be shadowed by the boot env ──

    /// Replay the IPC write face at the loader seam: sweep the section's six
    /// keys, write the `Some` values, and return the snapshot the handler
    /// records before reloading. Same primitives the handler uses
    /// (`env_file`), so this exercises the real file face.
    fn ipc_section_write(
        env_path: &Path,
        account_id: &str,
        section: Option<&SectionFields>,
    ) -> std::collections::BTreeMap<String, Option<String>> {
        let state = section_env_state(account_id, section).expect("section state");
        let keys: Vec<&str> = state.keys().map(|k| k.as_str()).collect();
        crate::env_file::remove_env_vars(env_path, &keys).expect("section sweep");
        let vars: std::collections::BTreeMap<String, String> = state
            .iter()
            .filter_map(|(k, v)| v.clone().map(|v| (k.clone(), v)))
            .collect();
        crate::env_file::upsert_env_vars(env_path, &vars).expect("section write");
        state
    }

    /// Read one key back through the launcher's read half — the file face as
    /// the next boot (and the operator) sees it.
    fn file_value(env_path: &Path, key: &str) -> Option<String> {
        std::fs::read_to_string(env_path)
            .unwrap()
            .lines()
            .filter_map(crate::env_file::parse_line)
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    #[test]
    fn set_written_value_wins_over_the_startup_env_on_reload() {
        // #757 repro: the launcher exported POLICY_MAX_BUDGET_USD_PAPER=100
        // into the process environment at boot (injected here — no global
        // state mutation). `set` to 5 writes the file and the write snapshot.
        // PRE-FIX (`load_single_source`, process env first): the reload read
        // the stale 100, so `after` and the effective policy were 100 while
        // the file said 5 — the requested risk tightening never applied.
        let dir = scratch("issue757-set");
        let env_path = dir.join(".env");
        let toml_path = dir.join("execution_policy.toml");
        std::fs::write(
            &env_path,
            "ACCOUNT_LIST=paper\nPOLICY_MAX_BUDGET_USD_PAPER=\"100\"\n",
        )
        .expect("seed deployment env");
        let startup = |key: &str| match key {
            "POLICY_MAX_BUDGET_USD_PAPER" => Some("100".to_string()),
            _ => None,
        };
        let no_writes = std::collections::BTreeMap::new();
        let before =
            Policy::load_for_reload_with_env(&toml_path, &env_path, &no_writes, startup).unwrap();
        assert_eq!(before.section_for("paper").max_budget_usd, dec!(100));

        // execution_policy.set { accountId: "paper", maxBudgetUsd: "5" }.
        let section = SectionFields {
            max_budget_usd: Some(dec!(5)),
            ..Default::default()
        };
        let written = ipc_section_write(&env_path, "paper", Some(&section));
        let after =
            Policy::load_for_reload_with_env(&toml_path, &env_path, &written, startup).unwrap();

        // What the handler returns as `after` (and what `evaluate` judges
        // under) is this reloaded section view: 5, the tightening.
        assert_eq!(after.section_for("paper").max_budget_usd, dec!(5));
        // The file holds the just-written value, not the stale 100.
        assert_eq!(
            file_value(&env_path, "POLICY_MAX_BUDGET_USD_PAPER").as_deref(),
            Some("5")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_does_not_resurrect_the_startup_env() {
        // #757: after the set the account owns the file keys, then `reset`
        // sweeps them. The process env still carries the boot export 100; the
        // snapshot's tombstone (`None`) must outrank it. PRE-FIX
        // (`load_single_source`, process env first): the reload fell back to
        // 100 instead of the documented default. (The file-only reload also
        // lands the default here, but loses env-only keys — next test.)
        let dir = scratch("issue757-reset");
        let env_path = dir.join(".env");
        let toml_path = dir.join("execution_policy.toml");
        std::fs::write(
            &env_path,
            "ACCOUNT_LIST=paper\nPOLICY_MAX_BUDGET_USD_PAPER=\"100\"\n",
        )
        .expect("seed deployment env");
        let startup = |key: &str| match key {
            "POLICY_MAX_BUDGET_USD_PAPER" => Some("100".to_string()),
            _ => None,
        };
        let section = SectionFields {
            max_budget_usd: Some(dec!(5)),
            ..Default::default()
        };
        let written = ipc_section_write(&env_path, "paper", Some(&section));
        let after_set =
            Policy::load_for_reload_with_env(&toml_path, &env_path, &written, startup).unwrap();
        assert_eq!(after_set.section_for("paper").max_budget_usd, dec!(5));

        // execution_policy.reset { accountId: "paper" }.
        let swept = ipc_section_write(&env_path, "paper", None);
        assert!(
            swept.values().all(Option::is_none),
            "reset sweeps every key"
        );
        let after_reset =
            Policy::load_for_reload_with_env(&toml_path, &env_path, &swept, startup).unwrap();

        assert_eq!(
            after_reset.section_for("paper").max_budget_usd,
            code_defaults().max_budget_usd,
            "reset inherits the documented default"
        );
        assert_ne!(
            after_reset.section_for("paper").max_budget_usd,
            dec!(100),
            "the stale deployment value must not come back"
        );
        assert_eq!(file_value(&env_path, "POLICY_MAX_BUDGET_USD_PAPER"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deployment_env_only_key_still_resolves_on_reload() {
        // Constraint: a key that only ever arrived from the deployment env
        // (never written through IPC) must keep working exactly as at boot.
        // Nothing on file and nothing in the write snapshot, so the reload
        // must still see the exported ACCOUNT_LIST + 100. PRE-FIX on the
        // file-only reload (#767): no section and the code default
        // (1_000_000_000) — this is why the process-env fallback stays.
        let dir = scratch("issue757-env-only");
        let env_path = dir.join(".env");
        let toml_path = dir.join("execution_policy.toml");
        std::fs::write(&env_path, "# no policy keys on file\n").expect("empty deployment env");
        let startup = |key: &str| match key {
            "ACCOUNT_LIST" => Some("paper".to_string()),
            "POLICY_MAX_BUDGET_USD_PAPER" => Some("100".to_string()),
            _ => None,
        };
        let no_writes = std::collections::BTreeMap::new();
        let reloaded =
            Policy::load_for_reload_with_env(&toml_path, &env_path, &no_writes, startup).unwrap();
        assert_eq!(reloaded.section_for("paper").max_budget_usd, dec!(100));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_rules_are_the_rules_the_reload_returns() {
        // #757 rules face: the boot export carries a looser backstop;
        // `set` writes the tighter rule set. PRE-FIX (`load_single_source`,
        // process env first) the reload returned the OLD rules while the
        // file said the new ones. The write snapshot must make the written
        // set the one the loader returns.
        let dir = scratch("issue757-rules");
        let env_path = dir.join(".env");
        let toml_path = dir.join("execution_policy.toml");
        let old = r#"[{"name":"old_backstop","priority":100,"enabled":true,"when":"open_positions >= 2","then":{"action":"skip"},"reason":"old"}]"#;
        std::fs::write(
            &env_path,
            format!("ACCOUNT_LIST=paper\nPOLICY_RULES_PAPER='{old}'\n"),
        )
        .expect("seed deployment env");
        let startup = |key: &str| match key {
            "POLICY_RULES_PAPER" => Some(old.to_string()),
            _ => None,
        };
        let no_writes = std::collections::BTreeMap::new();
        let before =
            Policy::load_for_reload_with_env(&toml_path, &env_path, &no_writes, startup).unwrap();
        assert_eq!(before.section_for("paper").rules[0].name, "old_backstop");

        // The tightening: skip as soon as ANY position is open.
        let tight: Vec<Rule> = serde_json::from_str(
            r#"[{"name":"tight_backstop","priority":1,"enabled":true,"when":"open_positions >= 1","then":{"action":"skip"},"reason":"tighten"}]"#,
        )
        .expect("tight rules");
        let section = SectionFields {
            rules: Some(tight),
            ..Default::default()
        };
        let written = ipc_section_write(&env_path, "paper", Some(&section));
        let after =
            Policy::load_for_reload_with_env(&toml_path, &env_path, &written, startup).unwrap();

        assert_eq!(after.section_for("paper").rules.len(), 1);
        assert_eq!(after.section_for("paper").rules[0].name, "tight_backstop");
        // The file carries the same set through the launcher's read half.
        let on_file = file_value(&env_path, "POLICY_RULES_PAPER").expect("rules on file");
        let parsed: Vec<Rule> = serde_json::from_str(&on_file).expect("file rules JSON");
        assert_eq!(parsed[0].name, "tight_backstop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── #380: the default config path resolves against the install root ────

    /// A scratch tree with the shipped config layout:
    /// `<root>/user_layer/configs/execution_policy.toml`.
    fn policy_tree(root: &Path) -> std::path::PathBuf {
        let dir = root.join("user_layer").join("configs");
        std::fs::create_dir_all(&dir).expect("create config tree");
        let file = dir.join("execution_policy.toml");
        std::fs::write(&file, "version = 1\n").expect("write policy fixture");
        file
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bk380-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("scratch root");
        dir
    }

    #[test]
    fn default_path_resolution_prefers_the_exe_root_over_the_cwd() {
        let exe_tree = scratch("exe");
        let cwd_tree = scratch("cwd");
        let exe_policy = policy_tree(&exe_tree);
        let _cwd_policy = policy_tree(&cwd_tree);
        // The exe sits one level below its root (the bundle `bin/` shape).
        let bin = exe_tree.join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        let r = resolve_default_config_path(Some(&bin), Some(&cwd_tree));
        assert_eq!(r.path, exe_policy, "the exe root's file wins");
        assert_eq!(r.source, DefaultPathSource::ExeRoot);
        // The evidence trail stops at the hit: the winning file is the last
        // probe, the cwd chain was never touched, and every probe names the
        // one config spelling.
        assert_eq!(r.searched.last(), Some(&exe_policy));
        assert!(r.searched.len() <= ancestor_chain(&bin).len());
        assert!(
            r.searched
                .iter()
                .all(|p| p.ends_with(EXECUTION_POLICY_CONFIG_PATH))
        );
        let _ = std::fs::remove_dir_all(&exe_tree);
        let _ = std::fs::remove_dir_all(&cwd_tree);
    }

    #[test]
    fn default_path_resolution_falls_back_to_the_cwd_when_no_exe_file_exists() {
        let cwd_tree = scratch("cwdonly");
        let cwd_policy = policy_tree(&cwd_tree);
        // An exe dir with NO policy file anywhere above it.
        let lonely = scratch("lonely");
        let bin = lonely.join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        let r = resolve_default_config_path(Some(&bin), Some(&cwd_tree));
        assert_eq!(r.path, cwd_policy);
        assert_eq!(r.source, DefaultPathSource::Cwd);
        // Both chains were probed before the cwd chain hit.
        assert!(r.searched.len() > ancestor_chain(&bin).len());
        assert_eq!(r.searched.last(), Some(&cwd_policy));
        let _ = std::fs::remove_dir_all(&cwd_tree);
        let _ = std::fs::remove_dir_all(&lonely);
    }

    #[test]
    fn default_path_resolution_nowhere_found_is_the_fallback_spelling() {
        let lonely = scratch("nowhere");
        let bin = lonely.join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        let r = resolve_default_config_path(Some(&bin), Some(&lonely.join("sub")));
        assert_eq!(r.source, DefaultPathSource::Fallback);
        assert_eq!(
            r.path,
            bin.join(EXECUTION_POLICY_CONFIG_PATH),
            "the fallback names the exe-root-relative spelling, whose absence the \
             loader reports as the #372 zero-config deployment"
        );
        // The WARN's evidence: every probed directory is on the list, exe
        // chain first, cwd chain second.
        assert_eq!(
            r.searched.len(),
            ancestor_chain(&bin).len() + ancestor_chain(&lonely.join("sub")).len()
        );
        assert_eq!(r.searched[0], bin.join(EXECUTION_POLICY_CONFIG_PATH));
        let _ = std::fs::remove_dir_all(&lonely);
    }

    #[test]
    fn default_path_resolution_walks_up_through_the_ancestors() {
        let tree = scratch("ancestors");
        let policy = policy_tree(&tree);
        // Three levels deep: the exe dir itself, its parent and grandparent
        // carry no policy file — the great-grandparent (the tree root) does.
        let deep = tree.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).expect("deep dir");
        let r = resolve_default_config_path(Some(&deep), None);
        assert_eq!(r.path, policy);
        assert_eq!(r.source, DefaultPathSource::ExeRoot);
        let _ = std::fs::remove_dir_all(&tree);
    }

    #[test]
    fn default_path_resolution_survives_missing_inputs() {
        // `None` inputs shorten the chain instead of erroring: a probe that
        // cannot name a directory is skipped, and with BOTH gone the fallback
        // is the bare relative spelling.
        let tree = scratch("noinput");
        let policy = policy_tree(&tree);
        let r = resolve_default_config_path(None, Some(&tree));
        assert_eq!(r.path, policy);
        assert_eq!(r.source, DefaultPathSource::Cwd);
        let r = resolve_default_config_path(None, None);
        assert_eq!(r.path, Path::new(EXECUTION_POLICY_CONFIG_PATH));
        assert_eq!(r.source, DefaultPathSource::Fallback);
        assert!(r.searched.is_empty());
        let _ = std::fs::remove_dir_all(&tree);
    }

    #[test]
    fn default_path_is_not_confused_by_a_directory_with_the_config_name() {
        // A DIRECTORY named execution_policy.toml must not count as a hit:
        // the probe is is_file(), not exists().
        let tree = scratch("dirshadow");
        let dir = tree.join("user_layer").join("configs");
        std::fs::create_dir_all(dir.join("execution_policy.toml")).expect("shadow dir");
        let r = resolve_default_config_path(Some(&tree), None);
        assert_eq!(
            r.source,
            DefaultPathSource::Fallback,
            "a directory is not a policy file"
        );
        let _ = std::fs::remove_dir_all(&tree);
    }

    #[test]
    fn explicit_policy_path_is_honoured_verbatim() {
        // The operator's spelling — relative, odd, whatever — is not
        // second-guessed: an explicit --execution-policy <path> means THAT
        // file, and #380's install-root lookup must never rewrite it.
        for spelling in [
            "user_layer/configs/execution_policy.toml",
            "/etc/blitzkrieg/policy.toml",
            "./relative/policy.toml",
        ] {
            assert_eq!(
                resolve_execution_policy_path(Some(spelling)),
                ResolvedPolicyPath::Explicit(std::path::PathBuf::from(spelling)),
                "explicit path {spelling} must pass through untouched"
            );
        }
    }

    #[test]
    fn absent_explicit_policy_path_enters_the_default_resolution() {
        // None → the #380 default resolution. This runs the REAL current_exe/
        // current_dir chain; the assertion is shape-only (the environment's
        // actual layout decides which source wins), the ORDER is pinned by
        // the injected tests above.
        match resolve_execution_policy_path(None) {
            ResolvedPolicyPath::Default(r) => {
                assert!(
                    !r.path.as_os_str().is_empty(),
                    "the default resolution always names a path to try"
                );
            }
            ResolvedPolicyPath::Explicit(_) => panic!("None must not resolve to Explicit"),
        }
    }
}
