//! Split out of the parent module (issue #780): pure move.

use super::*;

#[cfg(test)]
mod tests {
    use super::*;

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
}
