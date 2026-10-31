//! Split out of the parent module (issue #780): pure move.

use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use AccountStatus::{Active, Frozen, ReadOnly};

    fn write_temp(text: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bk-accounts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, text).expect("write temp config");
        p
    }

    fn fresh_env_file(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bk-accounts-env-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join(".env")
    }

    fn static_lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    // ── the env grammar (issue 751) ─────────────────────────────────────────

    #[test]
    fn env_book_loads_with_defaults_and_probes_credentials() {
        let env = static_lookup(&[
            ("ACCOUNT_LIST", "default,paper"),
            ("ACCOUNT_DEFAULT_NAME", "Poly Main"),
            ("ACCOUNT_DEFAULT_VENUE", "polymarket"),
            ("ACCOUNT_PAPER_STATUS", "read_only"),
        ]);
        let book = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &env)
            .expect("a well-formed ACCOUNT_* set loads")
            .expect("the env grammar produced a book");
        assert_eq!(book.account_list().len(), 2);
        assert_eq!(book.active_id().as_str(), "default");
        let d = book.account(&AccountId::from("default")).unwrap();
        assert_eq!(d.name, "Poly Main");
        assert_eq!(d.venue, "polymarket");
        assert!(d.trading_enabled, "absent key defaults to true");
        let p = book.account(&AccountId::from("paper")).unwrap();
        assert!(matches!(p.status, AccountStatus::ReadOnly));
        // Display defaults to the first active account.
        assert_eq!(book.display_id(), "default");
    }

    #[test]
    fn env_book_refuses_a_bad_status_and_a_dangling_display() {
        let bad_status = static_lookup(&[("ACCOUNT_LIST", "a"), ("ACCOUNT_A_STATUS", "magical")]);
        let err = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &bad_status)
            .expect_err("an unknown status refuses (fail-closed)");
        assert!(err.message.contains("STATUS=`magical`"), "{}", err.message);

        let dangling = static_lookup(&[("ACCOUNT_LIST", "a"), ("ACCOUNT_DISPLAY", "ghost")]);
        let err = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &dangling)
            .expect_err("a display id naming no listed account refuses");
        assert!(err.message.contains("ACCOUNT_DISPLAY"), "{}", err.message);
    }

    #[test]
    fn env_book_refuses_duplicate_ids_and_bad_rounds() {
        let dup = static_lookup(&[("ACCOUNT_LIST", "a,a")]);
        assert!(load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &dup).is_err());

        let bad_rounds =
            static_lookup(&[("ACCOUNT_LIST", "a"), ("ACCOUNT_A_ROUNDS", "900,424242")]);
        let err = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &bad_rounds)
            .expect_err("a round duration no plugin declared refuses");
        assert!(err.message.contains("424242"), "{}", err.message);
        assert!(
            err.message.contains("ACCOUNT_A_ROUNDS"),
            "the refusal names the key: {}",
            err.message
        );
    }

    #[test]
    fn env_book_refuses_ids_that_share_the_normalized_key_space() {
        // Pre-fix ONLY the raw id was compared, so `paper-a` + `paper_a`
        // (and `a` + `A`) both loaded and shared one `ACCOUNT_PAPER_A_*` /
        // `ACCOUNT_A_*` key space: venue, strategies, trading switch and
        // credential names aliased, and deleting either swept the other's
        // config. The collision is now fail-closed and names BOTH ids.
        for (list, first, second) in [("paper-a,paper_a", "paper-a", "paper_a"), ("a,A", "a", "A")]
        {
            let pairs = [("ACCOUNT_LIST", list)];
            let env = static_lookup(&pairs);
            let err = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &env)
                .expect_err("a normalized-id collision refuses the boot");
            assert_eq!(err.code, CoreErrorCode::InvalidParams);
            assert!(err.message.contains(first), "{}", err.message);
            assert!(err.message.contains(second), "{}", err.message);
        }

        // The legacy toml face runs the same guard, so a book the migration
        // would turn into colliding env keys never boots either.
        let toml = "[[account]]\nid = \"paper-a\"\n\n[[account]]\nid = \"paper_a\"\n";
        let err = from_str(toml, 5, TEST_ALLOWED_ROUNDS)
            .expect_err("the toml face also refuses the alias");
        // Static messages only: the refusal text carries account ids, and
        // formatting it into a panic payload is a log sink to the scanning
        // gate (rust/cleartext-logging). The ids are here literally.
        assert!(
            err.message.contains("paper-a"),
            "the refusal must name paper-a"
        );
        assert!(
            err.message.contains("paper_a"),
            "the refusal must name paper_a"
        );
    }

    #[test]
    fn env_create_refuses_a_normalized_alias_and_leaves_the_file_untouched() {
        let toml = Path::new("/nonexistent/bk-none/accounts.toml");
        let dot_env = fresh_env_file("alias-create");
        env_persist_account_create(
            toml,
            &dot_env,
            "paper_a",
            None,
            Some("polymarket"),
            &CredentialKeys::default(),
        )
        .expect("the first create lands");
        let before = std::fs::read_to_string(&dot_env).unwrap();

        // Pre-fix `env_persist_account_create` compared the RAW id only, so
        // `paper-a` was accepted and its NAME/VENUE/credential writes landed
        // on the SAME `ACCOUNT_PAPER_A_*` keys `paper_a` owns. Now the create
        // refuses before any write — the file stays byte-identical.
        let err = env_persist_account_create(
            toml,
            &dot_env,
            "paper-a",
            None,
            None,
            &CredentialKeys::default(),
        )
        .expect_err("an aliased create refuses");
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
        assert!(err.message.contains("paper_a"), "{}", err.message);
        assert!(err.message.contains("paper-a"), "{}", err.message);
        assert_eq!(
            std::fs::read_to_string(&dot_env).unwrap(),
            before,
            "the refusal must not touch .env"
        );
        std::fs::remove_dir_all(dot_env.parent().unwrap()).unwrap();
    }

    #[test]
    fn non_colliding_accounts_read_write_and_delete_independently() {
        let toml = Path::new("/nonexistent/bk-none/accounts.toml");
        let dot_env = fresh_env_file("independent");
        env_persist_account_create(
            toml,
            &dot_env,
            "alpha",
            Some("Alpha"),
            Some("polymarket"),
            &CredentialKeys::default(),
        )
        .unwrap();
        env_persist_account_create(
            toml,
            &dot_env,
            "beta",
            Some("Beta"),
            Some("kalshi"),
            &CredentialKeys::default(),
        )
        .unwrap();
        // A write on one account must not reach the other's keys.
        env_persist_account_trading(toml, &dot_env, "alpha", false).unwrap();

        let lookup = env_lookup_from_sources(Some(&dot_env));
        let book = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &lookup)
            .unwrap()
            .expect("the env grammar produced a book");
        let alpha = book.account(&AccountId::from("alpha")).unwrap();
        let beta = book.account(&AccountId::from("beta")).unwrap();
        assert!(!alpha.trading_enabled);
        assert!(beta.trading_enabled, "beta keeps the default");
        assert_eq!(alpha.venue, "polymarket");
        assert_eq!(beta.venue, "kalshi");

        // Delete one: the other's row survives untouched.
        env_persist_account_remove(toml, &dot_env, "alpha").expect("remove alpha");
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(!text.contains("ACCOUNT_ALPHA_"), "{text}");
        assert!(text.contains("ACCOUNT_LIST=\"beta\""), "{text}");
        assert!(text.contains("ACCOUNT_BETA_NAME=\"Beta\""), "{text}");
        assert!(text.contains("ACCOUNT_BETA_VENUE=\"kalshi\""), "{text}");
        std::fs::remove_dir_all(dot_env.parent().unwrap()).unwrap();
    }

    #[test]
    fn env_book_refuses_an_unparsable_trading_switch() {
        // Pre-fix an invalid boolean became `None` and
        // `trading_enabled.unwrap_or(true)` then ENABLED trading — a typo
        // expanded permissions (fail OPEN). It must now refuse the boot.
        let env = static_lookup(&[
            ("ACCOUNT_LIST", "paper"),
            ("ACCOUNT_PAPER_TRADING_ENABLED", "flase"),
        ]);
        let err = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &env)
            .expect_err("an unparsable switch refuses");
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
        assert!(
            err.message.contains("ACCOUNT_PAPER_TRADING_ENABLED"),
            "the refusal names the key: {}",
            err.message
        );
        assert!(err.message.contains("flase"), "{}", err.message);
    }

    #[test]
    fn env_book_refuses_a_non_integer_round_instead_of_emptying_the_set() {
        // Pre-fix `collect::<Option<Vec<i64>>>` collapsed the WHOLE list to
        // `None` on `typo`; `validate_rounds(None)` produced `[]`, which
        // risk_admission reads as "no round limit" — one typo silently
        // REMOVED the restriction (fail OPEN). It must now refuse.
        let env = static_lookup(&[
            ("ACCOUNT_LIST", "paper"),
            ("ACCOUNT_PAPER_ROUNDS", "300,typo"),
        ]);
        let err = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &env)
            .expect_err("a non-integer round refuses");
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
        assert!(
            err.message.contains("ACCOUNT_PAPER_ROUNDS"),
            "the refusal names the key: {}",
            err.message
        );
        assert!(err.message.contains("typo"), "{}", err.message);

        // Overflow is the same class of unreadable entry, not a silent drop.
        let overflow = static_lookup(&[
            ("ACCOUNT_LIST", "paper"),
            ("ACCOUNT_PAPER_ROUNDS", "999999999999999999999"),
        ]);
        let err = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &overflow)
            .expect_err("an overflowing round refuses");
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
        assert!(
            err.message.contains("ACCOUNT_PAPER_ROUNDS"),
            "{}",
            err.message
        );
    }

    #[test]
    fn env_book_keeps_valid_switches_rounds_and_documented_defaults() {
        // Pre-fix these values already parsed; this guards the strict rewrite
        // did not over-tighten: explicit `false` stays false, a valid rounds
        // list survives verbatim, and only GENUINELY ABSENT keys get the
        // documented defaults (trading true, rounds empty = no declared
        // restriction, name = id).
        let env = static_lookup(&[
            ("ACCOUNT_LIST", "paper,spare"),
            ("ACCOUNT_PAPER_TRADING_ENABLED", "false"),
            ("ACCOUNT_PAPER_ROUNDS", "300,900"),
        ]);
        let book = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &env)
            .unwrap()
            .expect("the env grammar produced a book");
        let paper = book.account(&AccountId::from("paper")).unwrap();
        assert!(!paper.trading_enabled);
        assert_eq!(paper.rounds, vec![300, 900]);
        let spare = book.account(&AccountId::from("spare")).unwrap();
        assert!(spare.trading_enabled, "absent key defaults to true");
        assert!(spare.rounds.is_empty(), "absent rounds = no restriction");
        assert_eq!(spare.name, "spare");
    }

    #[test]
    fn env_book_validates_the_arb_legs_against_the_list() {
        let ok = static_lookup(&[
            ("ACCOUNT_LIST", "a,b"),
            ("ACCOUNT_ARB_LEG_A_VENUE", "polymarket"),
            ("ACCOUNT_ARB_LEG_A_ACCOUNT", "a"),
            ("ACCOUNT_ARB_LEG_B_VENUE", "kalshi"),
            ("ACCOUNT_ARB_LEG_B_ACCOUNT", "b"),
            ("ACCOUNT_ARB_PAIRS", "btc, eth"),
        ]);
        let book = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &ok)
            .expect("coherent legs load")
            .expect("the env grammar produced a book");
        assert_eq!(book.account_list().len(), 2);
        // The legs validate against the list AND read back through the
        // dedicated reader (the boot's arb_leg_config source).
        let cfg = read_cross_venue_arb_from_env(&ok)
            .expect("the coherent section reads")
            .expect("legs configured");
        assert_eq!(cfg.leg_a_venue, "polymarket");
        assert_eq!(cfg.pairs, vec!["BTC", "ETH"]);

        let dangling_leg = static_lookup(&[
            ("ACCOUNT_LIST", "a"),
            ("ACCOUNT_ARB_LEG_A_VENUE", "polymarket"),
            ("ACCOUNT_ARB_LEG_A_ACCOUNT", "ghost"),
            ("ACCOUNT_ARB_LEG_B_VENUE", "kalshi"),
            ("ACCOUNT_ARB_LEG_B_ACCOUNT", "a"),
            ("ACCOUNT_ARB_PAIRS", "btc"),
        ]);
        assert!(load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &dangling_leg).is_err());

        let half_leg = static_lookup(&[
            ("ACCOUNT_LIST", "a"),
            ("ACCOUNT_ARB_LEG_A_VENUE", "polymarket"),
            ("ACCOUNT_ARB_PAIRS", "btc"),
        ]);
        assert!(load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &half_leg).is_err());
    }

    #[test]
    fn single_source_env_wins_over_the_legacy_toml() {
        let dir = std::env::temp_dir().join(format!("bk-single-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let toml = dir.join("accounts.toml");
        std::fs::write(
            &toml,
            r#"
[[account]]
id = "from-file"
"#,
        )
        .unwrap();
        let dot_env = dir.join(".env");
        std::fs::write(&dot_env, "ACCOUNT_LIST=from-env\n").unwrap();
        // process env is empty for these keys (the test harness carries none).
        let book =
            load_account_ledgers_single_source(&toml, Some(&dot_env), 5, TEST_ALLOWED_ROUNDS)
                .expect("env wins");
        assert_eq!(book.account_list().len(), 1);
        assert_eq!(book.account_list()[0].id.as_str(), "from-env");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn single_source_falls_back_to_the_toml_then_the_default() {
        // No ACCOUNT_LIST anywhere: the legacy file still rules.
        let toml = write_temp(
            r#"
[[account]]
id = "legacy"
"#,
        );
        let missing_env = Path::new("/nonexistent/bk-none/.env");
        let book =
            load_account_ledgers_single_source(&toml, Some(missing_env), 5, TEST_ALLOWED_ROUNDS)
                .expect("toml fallback");
        assert_eq!(book.account_list()[0].id.as_str(), "legacy");

        // No file, no env: the single default.
        let book = load_account_ledgers_single_source(
            Path::new("/nonexistent/bk-none/accounts.toml"),
            Some(missing_env),
            5,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("zero-config default");
        assert_eq!(book.account_list().len(), 1);
        assert_eq!(book.active_id().as_str(), "default");
    }

    #[test]
    fn env_write_faces_upsert_and_refuse_the_legacy_toml() {
        // A PRIVATE toml path (the write faces only probe its existence): the
        // shared write_temp dir races with sibling tests re-creating their
        // files mid-run, which would resurrect the legacy refusal mid-test.
        let toml = fresh_env_file("write-faces").with_file_name("legacy-accounts.toml");
        std::fs::write(&toml, "[[account]]\nid = \"default\"\n").unwrap();
        let dot_env = fresh_env_file("write-faces");
        // The refusal: a legacy file alive means every runtime write refuses.
        let err = env_persist_display_account(&toml, &dot_env, "default")
            .expect_err("the legacy toml alive refuses the write (one source)");
        assert!(
            err.message.contains("migrate-config-to-env.sh"),
            "{}",
            err.message
        );
        assert!(
            !dot_env.exists(),
            "the refusal must not have created the env"
        );

        std::fs::remove_file(&toml).unwrap();
        // Display upsert.
        env_persist_display_account(&toml, &dot_env, "default").expect("display write");
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(text.contains("ACCOUNT_DISPLAY=\"default\""), "{text}");

        // Venue write, then venue REMOVE (empty = unbound, absence spells it).
        env_persist_account_venue(&toml, &dot_env, "default", "polymarket").unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(
            text.contains("ACCOUNT_DEFAULT_VENUE=\"polymarket\""),
            "{text}"
        );
        env_persist_account_venue(&toml, &dot_env, "default", "").unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(!text.contains("ACCOUNT_DEFAULT_VENUE"), "{text}");

        // Trading false is explicit; true removes the key.
        env_persist_account_trading(&toml, &dot_env, "default", false).unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(
            text.contains("ACCOUNT_DEFAULT_TRADING_ENABLED=\"false\""),
            "{text}"
        );
        env_persist_account_trading(&toml, &dot_env, "default", true).unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(!text.contains("TRADING_ENABLED"), "{text}");

        // Rounds/strategies: value write and remove-by-absence.
        env_persist_account_rounds(&toml, &dot_env, "default", &[900, 3600]).unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(
            text.contains("ACCOUNT_DEFAULT_ROUNDS=\"900,3600\""),
            "{text}"
        );
        env_persist_account_rounds(&toml, &dot_env, "default", &[]).unwrap();
        assert!(
            !std::fs::read_to_string(&dot_env)
                .unwrap()
                .contains("ROUNDS")
        );

        // Arb config upsert + clear sweep all five keys.
        let cfg = CrossVenueArbConfig {
            leg_a_venue: "polymarket".into(),
            leg_a_account: "default".into(),
            leg_b_venue: "kalshi".into(),
            leg_b_account: "k".into(),
            pairs: vec!["btc".into()],
        };
        env_persist_cross_venue_arb(&toml, &dot_env, &cfg).unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(
            text.contains("ACCOUNT_ARB_LEG_A_VENUE=\"polymarket\""),
            "{text}"
        );
        assert!(text.contains("ACCOUNT_ARB_PAIRS=\"BTC\""), "{text}");
        env_clear_cross_venue_arb(&toml, &dot_env).unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(!text.contains("ACCOUNT_ARB"), "{text}");
        std::fs::remove_dir_all(dot_env.parent().unwrap()).unwrap();
    }

    #[test]
    fn env_create_appends_to_the_list_and_remove_sweeps_the_row() {
        let toml = Path::new("/nonexistent/bk-none/accounts.toml");
        let dot_env = fresh_env_file("create-remove");
        let credentials = CredentialKeys::from_plugin_env(&[
            "POLYMARKET_PRIVATE_KEY",
            "POLYMARKET_FUNDER_ADDRESS",
        ]);
        env_persist_account_create(
            toml,
            &dot_env,
            "default",
            Some("Main"),
            Some("polymarket"),
            &credentials,
        )
        .expect("first create writes the list");
        env_persist_account_create(
            toml,
            &dot_env,
            "paper",
            None,
            None,
            &CredentialKeys::default(),
        )
        .expect("second create appends");
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(text.contains("ACCOUNT_LIST=\"default,paper\""), "{text}");
        assert!(text.contains("ACCOUNT_DEFAULT_NAME=\"Main\""), "{text}");
        assert!(
            text.contains("ACCOUNT_DEFAULT_PRIVATE_KEY_ENV=\"POLYMARKET_PRIVATE_KEY\""),
            "{text}"
        );
        assert!(
            text.contains("ACCOUNT_DEFAULT_API_KEY_ENV=\"POLYMARKET_FUNDER_ADDRESS\""),
            "{text}"
        );
        // Duplicate create refuses.
        assert!(
            env_persist_account_create(
                toml,
                &dot_env,
                "paper",
                None,
                None,
                &CredentialKeys::default(),
            )
            .is_err()
        );

        // The book the two writes produced loads and reads back.
        let lookup = env_lookup_from_sources(Some(&dot_env));
        let book = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &lookup)
            .unwrap()
            .expect("the env grammar produced a book");
        assert_eq!(book.account_list().len(), 2);

        // Remove sweeps every per-id key and re-derives the display pointer.
        env_persist_display_account(toml, &dot_env, "paper").unwrap();
        env_persist_account_remove(toml, &dot_env, "paper").expect("remove");
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(text.contains("ACCOUNT_LIST=\"default\""), "{text}");
        assert!(text.contains("ACCOUNT_DISPLAY=\"default\""), "{text}");
        assert!(!text.contains("ACCOUNT_PAPER_"), "{text}");
        // Removing the last account refuses (an empty book fails the boot).
        assert!(env_persist_account_remove(toml, &dot_env, "default").is_err());
        std::fs::remove_dir_all(dot_env.parent().unwrap()).unwrap();
    }

    #[test]
    fn env_remove_then_same_name_create_uses_the_hot_file_state() {
        let toml = Path::new("/nonexistent/bk-none/accounts.toml");
        let dot_env = fresh_env_file("remove-recreate");
        std::fs::write(
            &dot_env,
            "ACCOUNT_LIST=\"default,polysimulator\"\nACCOUNT_POLYSIMULATOR_VENUE=\"polysimulator\"\n",
        )
        .unwrap();

        env_persist_account_remove(toml, &dot_env, "polysimulator").expect("remove");

        // The launcher-loaded process environment still carries this boot
        // snapshot. A hot writer must prefer the file changed above.
        let stale_boot = |key: &str| match key {
            "ACCOUNT_LIST" => Some("default,polysimulator".to_string()),
            _ => None,
        };
        let lookup = env_lookup_for_write_with(&dot_env, stale_boot);
        assert_eq!(current_account_list(&lookup).unwrap(), vec!["default"]);
        drop(lookup);

        env_persist_account_create(
            toml,
            &dot_env,
            "polysimulator",
            None,
            Some("polysimulator"),
            &CredentialKeys::default(),
        )
        .expect("same-name re-create reads the hot file, not the boot snapshot");
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(
            text.contains("ACCOUNT_LIST=\"default,polysimulator\""),
            "{text}"
        );
        std::fs::remove_dir_all(dot_env.parent().unwrap()).unwrap();
    }

    #[test]
    fn env_suffix_matches_the_policy_spelling() {
        assert_eq!(env_suffix("default"), "DEFAULT");
        assert_eq!(env_suffix("polysimulator"), "POLYSIMULATOR");
        assert_eq!(env_suffix("my-account.1"), "MY_ACCOUNT_1");
    }

    #[test]
    fn missing_file_yields_the_single_default_account() {
        let book = load_account_ledgers(
            Path::new("/nonexistent/bk-accounts/none.toml"),
            5,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("a missing file is the zero-config deployment");
        assert_eq!(book.account_list().len(), 1);
        assert_eq!(book.active_id().as_str(), "default");
    }

    #[test]
    fn the_design_example_parses_with_its_restrictions() {
        // The §9.5 example, verbatim shape: two accounts, one read-only.
        let book = from_str(
            r#"
[[account]]
id = "default"
name = "Poly Main"
market_type = "prediction"
api_key_env = "POLY_API_KEY"
secret_env = "POLY_SECRET"
passphrase_env = "POLY_PASSPHRASE"

[[account]]
id = "paper"
name = "Paper Trading"
market_type = "prediction"
status = "read_only"
"#,
            7,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("the design example must parse");
        assert_eq!(book.account_list().len(), 2);
        assert!(matches!(
            book.account(&AccountId("paper".into())).unwrap().status,
            ReadOnly
        ));
        assert!(matches!(
            book.account(&default_account_id()).unwrap().status,
            Active
        ));
        // Only key NAMES traveled; nothing loaded (this test process has no
        // POLY_API_KEY in its environment).
        let keys = &book.account(&default_account_id()).unwrap().credential_keys;
        assert_eq!(keys.api_key_env.as_deref(), Some("POLY_API_KEY"));
        assert!(!keys.loaded);
    }

    #[test]
    fn a_missing_status_defaults_to_active_and_a_missing_name_to_the_id() {
        let book = from_str("[[account]]\nid = \"solo\"\n", 1, TEST_ALLOWED_ROUNDS)
            .expect("a minimal entry parses");
        let a = book.account(&AccountId("solo".into())).unwrap();
        assert!(matches!(a.status, Active));
        assert_eq!(a.name, "solo");
        assert!(matches!(a.market_type, MarketType::Prediction));
    }

    // ── 轮次多族 (B1d): the `rounds` family declaration ─────────────────────

    #[test]
    fn rounds_parse_deduplicate_and_keep_first_seen_order() {
        let book = from_str(
            r#"
[[account]]
id = "multi"
rounds = [3600, 900, 3600, 86_400]
"#,
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("a rounds declaration parses");
        let a = book.account(&AccountId("multi".into())).unwrap();
        assert_eq!(a.rounds, vec![3600, 900, 86_400], "dedup, order kept");
    }

    #[test]
    fn a_missing_or_empty_rounds_key_means_no_family_restriction() {
        // Missing key: the legacy shape. Explicit empty: the same verdict —
        // rounds is a permission axis; an empty declaration must never lock
        // the account out of trading.
        let book = from_str(
            "[[account]]\nid = \"a\"\nrounds = []\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("an empty rounds parses");
        assert!(
            book.account(&AccountId("a".into()))
                .unwrap()
                .rounds
                .is_empty()
        );
        let book2 = from_str("[[account]]\nid = \"b\"\n", 1, TEST_ALLOWED_ROUNDS)
            .expect("absent rounds parses");
        assert!(
            book2
                .account(&AccountId("b".into()))
                .unwrap()
                .rounds
                .is_empty()
        );
    }

    #[test]
    fn a_rounds_value_outside_the_whitelist_refuses_the_boot() {
        let err = from_str(
            "[[account]]\nid = \"x\"\nrounds = [9000]\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect_err("a typo'd duration is fail-closed");
        assert!(err.message.contains("9000"), "{}", err.message);
        assert!(
            err.message.contains("not declared by any market plugin"),
            "{}",
            err.message
        );
    }

    #[test]
    fn a_malformed_file_is_refused_not_silently_defaulted() {
        let p = write_temp("[[account]]\nid = \"x\"\nbogus_field = 1\n");
        let err =
            load_account_ledgers(&p, 1, TEST_ALLOWED_ROUNDS).expect_err("deny_unknown_fields");
        assert!(err.message.contains("does not parse"), "{}", err.message);
    }

    #[test]
    fn an_empty_book_and_duplicate_ids_are_refused() {
        // A truly EMPTY document fails at the TOML layer (missing field) —
        // refused either way; the code's own empty-book diagnostic speaks for
        // a document that PARSES but declares no account (`account = []`).
        let empty_file = from_str("", 1, TEST_ALLOWED_ROUNDS).expect_err("no accounts at all");
        assert!(
            empty_file.message.contains("does not parse")
                || empty_file.message.contains("no [[account]]"),
            "{}",
            empty_file.message
        );
        let err = from_str("account = []\n", 1, TEST_ALLOWED_ROUNDS)
            .expect_err("parses but no [[account]]");
        assert!(err.message.contains("no [[account]]"), "{}", err.message);
        let dup = from_str(
            "[[account]]\nid = \"a\"\n\n[[account]]\nid = \"a\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect_err("duplicate id");
        assert!(dup.message.contains("twice"), "{}", dup.message);
        let blank =
            from_str("[[account]]\nid = \"  \"\n", 1, TEST_ALLOWED_ROUNDS).expect_err("blank id");
        assert!(blank.message.contains("empty id"), "{}", blank.message);
    }

    #[test]
    fn frozen_and_suspended_status_spellings_parse() {
        let book = from_str(
            "[[account]]\nid = \"a\"\nstatus = \"frozen\"\n\n[[account]]\nid = \"b\"\nstatus = \"suspended\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("snake_case statuses parse");
        assert!(matches!(
            book.account(&AccountId("a".into())).unwrap().status,
            Frozen
        ));
        // A bare `suspended` string carries no operator reason: the parsed
        // record states that honestly instead of inventing one.
        let b = book.account(&AccountId("b".into())).unwrap();
        assert!(matches!(b.status, AccountStatus::Suspended { .. }));
        // The map form carries the reason through untouched.
        let with_reason = from_str(
            "[[account]]\nid = \"c\"\nstatus = { suspended = { reason = \"ops review\" } }\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("map-form suspended parses");
        assert!(matches!(
            &with_reason.account(&AccountId("c".into())).unwrap().status,
            AccountStatus::Suspended { reason } if reason == "ops review"
        ));
        // A typo'd spelling is refused, never parsed as something permissive.
        let typo = from_str(
            "[[account]]\nid = \"d\"\nstatus = \"actve\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect_err("unknown spelling");
        assert!(
            typo.message.contains("unknown account status"),
            "{}",
            typo.message
        );
    }

    #[test]
    fn the_venue_binding_parses_and_defaults_to_unbound() {
        // An explicit binding rides through as data…
        let bound = from_str(
            "[[account]]\nid = \"poly\"\nvenue = \"polymarket\"\n\n[[account]]\nid = \"kal\"\nvenue = \"kalshi\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("venue bindings parse");
        assert_eq!(
            bound.account(&AccountId("poly".into())).unwrap().venue,
            "polymarket"
        );
        assert_eq!(
            bound.account(&AccountId("kal".into())).unwrap().venue,
            "kalshi"
        );
        // …a missing field is UNBOUND (the 0.2 behaviour)…
        let unbound = from_str("[[account]]\nid = \"solo\"\n", 1, TEST_ALLOWED_ROUNDS)
            .expect("minimal entry");
        assert_eq!(
            unbound.account(&AccountId("solo".into())).unwrap().venue,
            ""
        );
        // …and the zero-config default account is unbound too.
        let default_book = AccountLedgers::single_default(1);
        assert_eq!(
            default_book.account(&default_account_id()).unwrap().venue,
            ""
        );
    }

    #[test]
    fn credential_probe_reports_loaded_only_when_every_named_key_resolves() {
        // No keys named → nothing was loaded by the kernel (the plugin's own
        // env path is outside this book) — `loaded` stays false, honestly.
        let none = read_credential_keys(&CredentialKeys::default());
        assert!(!none.loaded);
        // A name that cannot resolve in this process → not loaded. Read-only
        // env access with a name nothing sets: deterministic, no env mutation.
        let missing = read_credential_keys(&CredentialKeys {
            api_key_env: Some("BK_E28_DEFINITELY_UNSET_KEY".into()),
            ..CredentialKeys::default()
        });
        assert!(!missing.loaded);
        // The name itself is carried through untouched — names are the wire
        // content, never values.
        assert_eq!(
            missing.api_key_env.as_deref(),
            Some("BK_E28_DEFINITELY_UNSET_KEY")
        );
    }

    // ── account.venue.set persistence (write-back of the venue binding) ──────

    /// A multi-field, multi-account fixture — the file the persist must NOT
    /// shrink: every field of BOTH entries must survive a one-account venue
    /// write.
    const MULTI_FIELD_BOOK: &str = r#"
[[account]]
id = "default"
name = "Poly Main"
market_type = "prediction"
api_key_env = "POLY_API_KEY"
secret_env = "POLY_SECRET"
passphrase_env = "POLY_PASSPHRASE"
private_key_env = "POLY_PRIVATE"

[[account]]
id = "paper"
name = "Paper Trading"
market_type = "prediction"
status = "read_only"
venue = "kalshi"
"#;

    #[test]
    fn persist_account_venue_writes_back_and_every_sibling_field_survives() {
        // Mutation note: this test is the "the persist call happens" guard.
        // Delete the persist_account_venue call from the venue.set arm (or
        // make it a no-op) and the reloaded book below still parses — but the
        // `venue` assertion on the RELOADED file goes red: the in-memory
        // write alone never reaches this file.
        let dir = std::env::temp_dir().join(format!("bk-venue-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // Bind `default` to polymarket: the write lands, and a FRESH load —
        // the same call the boot makes — sees the binding plus EVERY sibling
        // field of BOTH accounts.
        persist_account_venue(&p, "default", "polymarket").expect("the write lands");
        let book =
            load_account_ledgers(&p, 3, TEST_ALLOWED_ROUNDS).expect("the written file re-parses");
        assert_eq!(
            book.account(&AccountId("default".into())).unwrap().venue,
            "polymarket",
            "the new binding is durable"
        );
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().venue,
            "kalshi",
            "a sibling account's binding is untouched"
        );
        let d = book.account(&AccountId("default".into())).unwrap();
        assert_eq!(d.name, "Poly Main");
        assert!(matches!(d.market_type, MarketType::Prediction));
        assert!(matches!(d.status, Active));
        assert_eq!(
            d.credential_keys.api_key_env.as_deref(),
            Some("POLY_API_KEY")
        );
        assert_eq!(d.credential_keys.secret_env.as_deref(), Some("POLY_SECRET"));
        assert_eq!(
            d.credential_keys.passphrase_env.as_deref(),
            Some("POLY_PASSPHRASE")
        );
        assert_eq!(
            d.credential_keys.private_key_env.as_deref(),
            Some("POLY_PRIVATE")
        );
        // The other account's status — the file's only restriction — rode
        // through: a persist must never loosen what the operator wrote.
        assert!(matches!(
            book.account(&AccountId("paper".into())).unwrap().status,
            ReadOnly
        ));

        // Re-binding the SECOND account over its existing value: same
        // guarantees on a file the writer itself produced (round two of the
        // read-modify-write cycle).
        persist_account_venue(&p, "paper", "polymarket").expect("rebind lands");
        let book = load_account_ledgers(&p, 4, TEST_ALLOWED_ROUNDS).expect("round two re-parses");
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().venue,
            "polymarket"
        );
        assert_eq!(
            book.account(&AccountId("default".into())).unwrap().venue,
            "polymarket",
            "the first write survived the second"
        );

        // Unbind: venue = "" REMOVES the key — the file returns to the
        // "unbound by absence" shape a pre-binding deployment had.
        persist_account_venue(&p, "paper", "").expect("unbind lands");
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            !text.contains("paper") || !text.split("[[account]]").nth(2).unwrap().contains("venue"),
            "the unbound entry carries no venue key: {text}"
        );
        let book =
            load_account_ledgers(&p, 5, TEST_ALLOWED_ROUNDS).expect("the unbound file re-parses");
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().venue,
            "",
            "absence loads as unbound"
        );
        assert_eq!(
            book.account(&AccountId("default".into())).unwrap().venue,
            "polymarket",
            "the other account's binding survives the unbind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_account_venue_refuses_unknown_ids_and_broken_files() {
        let dir = std::env::temp_dir().join(format!("bk-venue-persist2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // An id the file never declared: refused, and the file is untouched.
        let before = std::fs::read_to_string(&p).unwrap();
        let err = persist_account_venue(&p, "ghost", "polymarket").expect_err("unknown id");
        assert!(
            err.message.contains("no `[[account]]` with id `ghost`"),
            "{}",
            err.message
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "a refused write leaves the file byte-identical"
        );

        // A file that does not parse: refused fail-closed, never flattened.
        std::fs::write(&p, "[[account]]\nid = \"x\"\nbogus = 1\n").unwrap();
        let err = persist_account_venue(&p, "x", "polymarket").expect_err("does not parse");
        assert!(
            err.message.contains("does not parse; refusing"),
            "{}",
            err.message
        );

        // A file that cannot even be read: refused with the same posture.
        let err = persist_account_venue(&dir.join("nope.toml"), "x", "polymarket")
            .expect_err("unreadable file");
        assert!(err.message.contains("unreadable"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_account_rounds_writes_back_and_every_sibling_field_survives() {
        let dir = std::env::temp_dir().join(format!("bk-rounds-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // The write lands with every sibling field intact (the same
        // read-modify-write guarantee the venue writer pins — one regression
        // here would silently shrink the book at the next boot).
        persist_account_rounds(&p, "default", &[900, 3600]).expect("the write lands");
        let book = load_account_ledgers(&p, 3, TEST_ALLOWED_ROUNDS).expect("the file re-parses");
        assert_eq!(
            book.account(&AccountId("default".into())).unwrap().rounds,
            vec![900, 3600],
            "the declaration is what the next boot reads"
        );
        let d = book.account(&AccountId("default".into())).unwrap();
        assert_eq!(d.name, "Poly Main");
        assert_eq!(
            d.credential_keys.api_key_env.as_deref(),
            Some("POLY_API_KEY")
        );
        assert_eq!(d.credential_keys.secret_env.as_deref(), Some("POLY_SECRET"));
        assert_eq!(
            d.credential_keys.passphrase_env.as_deref(),
            Some("POLY_PASSPHRASE")
        );
        assert_eq!(
            d.credential_keys.private_key_env.as_deref(),
            Some("POLY_PRIVATE")
        );
        // The sibling account's own state (status restriction + existing
        // binding) rode through untouched.
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().venue,
            "kalshi"
        );
        assert!(matches!(
            book.account(&AccountId("paper".into())).unwrap().status,
            AccountStatus::ReadOnly
        ));

        // Round two of the read-modify-write cycle: a rewrite over a file
        // the writer itself produced keeps both accounts' declarations.
        persist_account_rounds(&p, "paper", &[300]).expect("round two lands");
        let book = load_account_ledgers(&p, 4, TEST_ALLOWED_ROUNDS).expect("round two re-parses");
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().rounds,
            vec![300]
        );
        assert_eq!(
            book.account(&AccountId("default".into())).unwrap().rounds,
            vec![900, 3600],
            "the first write survived the second"
        );

        // The EMPTY set REMOVES the key — the file returns to the
        // "unrestricted by absence" shape a pre-declaration file had; the
        // deleted key parses exactly like a missing one.
        persist_account_rounds(&p, "default", &[]).expect("the delete lands");
        let text = std::fs::read_to_string(&p).unwrap();
        let default_block = text.split("[[account]]").nth(1).unwrap();
        assert!(
            !default_block.contains("rounds"),
            "the unrestricted entry carries no rounds key: {text}"
        );
        let book = load_account_ledgers(&p, 5, TEST_ALLOWED_ROUNDS)
            .expect("the deleted-key file re-parses");
        assert!(
            book.account(&AccountId("default".into()))
                .unwrap()
                .rounds
                .is_empty(),
            "absence loads as the unrestricted set"
        );
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().rounds,
            vec![300],
            "the other account's declaration survives the delete"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_account_rounds_refuses_unknown_ids_and_broken_files() {
        let dir = std::env::temp_dir().join(format!("bk-rounds-persist2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // An id the file never declared: refused, and the file is untouched.
        let before = std::fs::read_to_string(&p).unwrap();
        let err = persist_account_rounds(&p, "ghost", &[900]).expect_err("unknown id");
        assert!(
            err.message.contains("no `[[account]]` with id `ghost`"),
            "{}",
            err.message
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "a refused write leaves the file byte-identical"
        );

        // A file that does not parse: refused fail-closed, never flattened.
        std::fs::write(&p, "[[account]]\nid = \"x\"\nbogus = 1\n").unwrap();
        let err = persist_account_rounds(&p, "x", &[900]).expect_err("does not parse");
        assert!(
            err.message.contains("does not parse; refusing"),
            "{}",
            err.message
        );

        // A file that cannot even be read: refused with the same posture.
        let err = persist_account_rounds(&dir.join("nope.toml"), "x", &[900])
            .expect_err("unreadable file");
        assert!(err.message.contains("unreadable"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── display_account（显示账户配置化）：解析 / 缺省 / fail-closed ─────────

    #[test]
    fn display_account_parses_and_defaults_to_the_first_active_entry() {
        // Explicit: the key rides through as the display id verbatim. NOTE
        // the TOML spelling: a top-level key must sit BEFORE the first
        // [[account]] (TOML binds anything after an array-of-tables header
        // to that table) — the writers re-render it at the top, a hand edit
        // below the last block is refused by the entry's own
        // `deny_unknown_fields` (fail-closed, with the field list named).
        let explicit = from_str(
            "display_account = \"b\"\n\n[[account]]\nid = \"a\"\n\n[[account]]\nid = \"b\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("an explicit display_account parses");
        assert_eq!(explicit.display_id(), "b");

        // …missing: the FIRST status=active entry — `b` here, `a`'s read_only
        // posture must not win the pointer just by file order.
        let defaulted = from_str(
            "[[account]]\nid = \"a\"\nstatus = \"read_only\"\n\n[[account]]\nid = \"b\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("a file without the key parses");
        assert_eq!(
            defaulted.display_id(),
            "b",
            "absence = the first ACTIVE account, not the first row"
        );

        // No active entry at all: fall back to the first row (the book still
        // boots — the pointer is presentation-only, so the zero-active shape
        // is not a boot refusal, it is an honest "nothing to show").
        let no_active = from_str(
            "[[account]]\nid = \"a\"\nstatus = \"read_only\"\n\n[[account]]\nid = \"b\"\nstatus = \"frozen\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("a book with no active entry parses");
        assert_eq!(no_active.display_id(), "a");

        // Zero-config deployment (file missing): the single default account
        // displays itself.
        assert_eq!(
            AccountLedgers::single_default(1).display_id(),
            default_account_id().as_str()
        );
    }

    #[test]
    fn display_account_names_an_unknown_id_refuses_the_boot() {
        // Fail-closed: a dangling display pointer refuses the parse — a
        // silent fallback to "some account" is the exact 「账户交易所 ≠
        // 行情页交易所」 accident the key exists to make explicit.
        let err = from_str(
            "display_account = \"ghost\"\n\n[[account]]\nid = \"a\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect_err("unknown display id");
        assert!(
            err.message.contains("display_account") && err.message.contains("ghost"),
            "{}",
            err.message
        );
        // An EMPTY id names no configured account either — the same
        // fail-closed refusal (the file spells "follow the default" by
        // deleting the key, never by an empty string).
        assert!(
            from_str(
                "display_account = \"\"\n\n[[account]]\nid = \"a\"\n",
                1,
                TEST_ALLOWED_ROUNDS
            )
            .is_err()
        );
    }

    #[test]
    fn persist_display_account_writes_back_top_level_and_siblings_survive() {
        let dir = std::env::temp_dir().join(format!("bk-display-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // The write lands as a TOP-LEVEL key — never inside an [[account]]
        // block — and a FRESH load (the boot's call) reads it back with every
        // sibling field of BOTH accounts intact.
        persist_display_account(&p, "paper").expect("the write lands");
        let text = std::fs::read_to_string(&p).unwrap();
        let first_row = text.split("[[account]]").nth(1).unwrap();
        assert!(
            !first_row.contains("display_account = \"paper\""),
            "the key is top-level, not inside the first [[account]] block: {text}"
        );
        let book =
            load_account_ledgers(&p, 3, TEST_ALLOWED_ROUNDS).expect("the written file re-parses");
        assert_eq!(book.display_id(), "paper", "the new pointer is durable");
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().venue,
            "kalshi",
            "a sibling account's binding survives the display write"
        );
        let d = book.account(&AccountId("default".into())).unwrap();
        assert_eq!(d.name, "Poly Main");
        assert_eq!(
            d.credential_keys.api_key_env.as_deref(),
            Some("POLY_API_KEY"),
            "every sibling field round-trips"
        );

        // Re-point at the OTHER account: the read-modify-write cycle holds
        // round two, the first write survives.
        persist_display_account(&p, "default").expect("the re-point lands");
        let book = load_account_ledgers(&p, 4, TEST_ALLOWED_ROUNDS).expect("round two re-parses");
        assert_eq!(book.display_id(), "default");

        // A file that does not parse: refused fail-closed, never flattened.
        std::fs::write(&p, "[[account]]\nid = \"x\"\nbogus = 1\n").unwrap();
        let err = persist_display_account(&p, "x").expect_err("does not parse");
        assert!(
            err.message.contains("does not parse; refusing"),
            "{}",
            err.message
        );

        // A file that cannot even be read: refused with the same posture.
        let err =
            persist_display_account(&dir.join("nope.toml"), "x").expect_err("unreadable file");
        assert!(err.message.contains("unreadable"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── account.create / account.remove 的持久化半（文件层）───────────────────

    #[test]
    fn persist_account_create_appends_a_default_row_and_siblings_survive() {
        let dir = std::env::temp_dir().join(format!("bk-acct-create-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // Create: the write lands, and a FRESH load — the boot's call — sees
        // the new account with the file's own defaults (active + prediction,
        // unbound) plus EVERY sibling field of BOTH old accounts.
        persist_account_create(&p, "poly-main", Some("Poly Main 2"), Some("polymarket"))
            .expect("the create lands");
        let book =
            load_account_ledgers(&p, 3, TEST_ALLOWED_ROUNDS).expect("the written file re-parses");
        let created = book.account(&AccountId("poly-main".into())).unwrap();
        assert!(
            matches!(created.status, Active),
            "the file default is active"
        );
        assert!(
            matches!(created.market_type, MarketType::Prediction),
            "the file default is prediction"
        );
        assert_eq!(created.venue, "polymarket");
        assert_eq!(created.name, "Poly Main 2");
        // Both old accounts rode through untouched.
        assert_eq!(
            book.account(&AccountId("default".into())).unwrap().name,
            "Poly Main"
        );
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().venue,
            "kalshi"
        );

        // A create with neither name nor venue: the row is the bare id, the
        // name falls back to the id and the binding is unbound.
        persist_account_create(&p, "bare", None, None).expect("the bare create lands");
        let book = load_account_ledgers(&p, 4, TEST_ALLOWED_ROUNDS).expect("re-parses");
        let bare = book.account(&AccountId("bare".into())).unwrap();
        assert_eq!(bare.name, "bare");
        assert_eq!(bare.venue, "");

        // A duplicate id is refused (in-memory AND at the file layer) and the
        // file is untouched.
        let before = std::fs::read_to_string(&p).unwrap();
        let err = persist_account_create(&p, "paper", None, None).expect_err("duplicate");
        assert!(
            err.message
                .contains("already declares `[[account]]` with id `paper`"),
            "{}",
            err.message
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "a refused create leaves the file byte-identical"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The trading switch and strategy ENABLEMENT set round-trip through
    /// accounts.toml: an OFF switch and a non-empty set are the only shapes
    /// that occupy file keys (an absent strategies key is the empty set —
    /// "this account runs nothing explicitly"), sibling
    /// rows survive, and unknown ids refuse with the file untouched.
    #[test]
    fn persist_account_trading_and_strategies_round_trip_by_absence_and_presence() {
        let dir = std::env::temp_dir().join(format!("bk-acct-trade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // OFF writes an explicit key; a fresh load reads it as false.
        persist_account_trading(&p, "default", false).expect("the off write lands");
        let book =
            load_account_ledgers(&p, 3, TEST_ALLOWED_ROUNDS).expect("the written file re-parses");
        assert!(
            !book
                .account(&AccountId("default".into()))
                .unwrap()
                .trading_enabled,
            "the OFF switch is durable"
        );
        // A sibling row rode through untouched (still trading, no key).
        assert!(
            book.account(&AccountId("paper".into()))
                .unwrap()
                .trading_enabled,
            "the sibling's default ON survives"
        );

        // ON removes the key — the file returns to the pre-switch shape.
        persist_account_trading(&p, "default", true).expect("the on write lands");
        let text = std::fs::read_to_string(&p).unwrap();
        let default_row = text.split("[[account]]").nth(1).unwrap();
        assert!(
            !default_row.contains("trading_enabled"),
            "ON is spelled by absence: {text}"
        );
        let book = load_account_ledgers(&p, 4, TEST_ALLOWED_ROUNDS).expect("re-parses");
        assert!(
            book.account(&AccountId("default".into()))
                .unwrap()
                .trading_enabled,
            "absence loads as ON"
        );

        // The strategy ENABLEMENT set rides verbatim; a fresh load reads it
        // whole (the file is the durable half of the 启用按钮).
        persist_account_strategies(&p, "paper", &["spread_arb".into(), "mad_dog".into()])
            .expect("the enablement write lands");
        let book = load_account_ledgers(&p, 5, TEST_ALLOWED_ROUNDS).expect("re-parses");
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().strategies,
            vec!["spread_arb".to_string(), "mad_dog".to_string()],
            "the enablement set is durable, verbatim and ordered"
        );

        // The zero-config spelling: an empty list REMOVES the key.
        persist_account_strategies(&p, "paper", &[]).expect("the inherit lands");
        let text = std::fs::read_to_string(&p).unwrap();
        let paper_row = text.split("[[account]]").nth(2).unwrap();
        assert!(
            !paper_row.contains("strategies"),
            "inherit is spelled by absence: {text}"
        );

        // Unknown ids refuse on both writers; the file is untouched.
        let before = std::fs::read_to_string(&p).unwrap();
        assert!(persist_account_trading(&p, "ghost", false).is_err());
        assert!(persist_account_strategies(&p, "ghost", &["x".into()]).is_err());
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "a refused write leaves the file byte-identical"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn persist_account_remove_drops_the_row_refuses_unknown_and_the_last_row() {
        let dir = std::env::temp_dir().join(format!("bk-acct-remove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // Remove: the row leaves, the survivor keeps every field, and the
        // fresh load (the boot's call) agrees.
        persist_account_remove(&p, "paper").expect("the remove lands");
        let book =
            load_account_ledgers(&p, 3, TEST_ALLOWED_ROUNDS).expect("the written file re-parses");
        assert!(book.account(&AccountId("paper".into())).is_none());
        let d = book.account(&AccountId("default".into())).unwrap();
        assert_eq!(d.name, "Poly Main");
        assert_eq!(
            d.credential_keys.api_key_env.as_deref(),
            Some("POLY_API_KEY"),
            "a sibling account's fields survive the removal"
        );

        // Unknown id: refused, file untouched.
        let before = std::fs::read_to_string(&p).unwrap();
        let err = persist_account_remove(&p, "ghost").expect_err("unknown id");
        assert!(
            err.message.contains("no `[[account]]` with id `ghost`"),
            "{}",
            err.message
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "a refused remove leaves the file byte-identical"
        );

        // The LAST row: refused — the empty book would fail the next boot.
        let err = persist_account_remove(&p, "default").expect_err("the last row");
        assert!(
            err.message.contains("empty") && err.message.contains("next boot"),
            "{}",
            err.message
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "the empty-book refusal leaves the file byte-identical"
        );

        // A file that does not parse: refused fail-closed, never flattened.
        std::fs::write(&p, "[[account]]\nid = \"x\"\nbogus = 1\n").unwrap();
        let err = persist_account_remove(&p, "x").expect_err("does not parse");
        assert!(
            err.message.contains("does not parse; refusing"),
            "{}",
            err.message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
