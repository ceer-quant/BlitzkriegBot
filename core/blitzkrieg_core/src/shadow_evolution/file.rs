//! `user_layer/configs/shadow_evolution.toml` — the FILE as the single source
//! of truth for the two evolution switches.
//!
//! History: the switches used to be RUNTIME state persisted in
//! `data/evolution/state.json`, whose values OUTRANKED this file at boot
//! (#249). The panel then had to render the resulting disagreement
//! (`switchConflicts` on `shadow_evolution.status`) and offer a "拨回文件值"
//! button to reconcile them — a second source of truth wearing a repair kit.
//! The operator's ruling is unambiguous: the FILE decides. A runtime verb
//! (`shadow_evolution.enable` / `.disable` / `.set_auto`) now writes its new
//! value BACK into this file (the same posture as `account.venue.set` and
//! `round_duration.set`: the verb's durable side is the config file), and
//! `state.json` keeps only the pure bookkeeping (the deep-round clock) it was
//! invented for. A boot disagreement between the file and `state.json` is
//! therefore impossible by construction, so the conflict report and the panel
//! banner are gone.

use crate::model::{CoreError, CoreErrorCode};
use std::path::Path;

/// A switch's name as it appears in the TOML file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchKey {
    /// `[shadow_evolution] enabled` — the evaluator switch.
    Enabled,
    /// `[shadow_evolution] auto_evolve` — the unattended-adoption switch.
    AutoEvolve,
}

impl SwitchKey {
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::AutoEvolve => "auto_evolve",
        }
    }
}

/// Write ONE switch's new value back into the shadow-evolution config file.
///
/// Read-modify-write with `toml_edit`, so the operator's comments and layout
/// survive — this file carries a long explanatory header that a full
/// re-render would destroy, and unlike the account book there is no panel
/// surface that would ever rewrite it wholesale. A file that does not parse
/// is REFUSED, never overwritten (fail-closed: a switch flip must not be able
/// to "fix" a typo by flattening the operator's header into a shape they
/// never wrote). Atomic same-directory temp + rename, so the boot's read and
/// a concurrent reload can never see a partial document.
///
/// The switch is addressed as `[shadow_evolution] <key>` — the shipped file's
/// only section. A file without that section (or with the key commented out)
/// gets the key appended to the section (or the section appended to the
/// document): a first flip on a hand-minimal file must still persist.
pub fn persist_switch(path: &Path, key: SwitchKey, value: bool) -> Result<(), CoreError> {
    let name = key.key();
    let text = std::fs::read_to_string(path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(
                "{} unreadable, refusing the switch write: {e}",
                path.display()
            ),
        )
    })?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(
                "{} does not parse; refusing the switch write: {e}",
                path.display()
            ),
        )
    })?;
    let section = doc
        .entry("shadow_evolution")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let section = section.as_table_mut().ok_or_else(|| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(
                "{}: [shadow_evolution] is not a TOML table; refusing the switch write",
                path.display()
            ),
        )
    })?;
    section[name] = toml_edit::value(value);
    let out = doc.to_string();
    // Atomic replace: write the sibling temp first, then rename over the
    // original — a half-written file can never be what the next boot reads.
    let tmp = path.with_extension("toml.switch.tmp");
    std::fs::write(&tmp, out).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("{} switch write failed: {e}", path.display()),
        )
    })?;
    std::fs::rename(&tmp, path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("{} switch rename failed: {e}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bksevo-file-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("shadow_evolution.toml")
    }

    const SHIPPED: &str = r#"# Shadow Evolution — the header an operator reads.

[shadow_evolution]
enabled = false                      # evaluator OFF out of the box
variant_count = 3                    # shadow variants (>=2)

# E13 proposal workflow (#95). When `auto_evolve = false`, a qualifying
# variant does NOT move the live parameters.
auto_evolve = false
"#;

    #[test]
    fn a_write_round_trips_and_keeps_the_comments() {
        let p = tmp_path("roundtrip");
        std::fs::write(&p, SHIPPED).unwrap();

        persist_switch(&p, SwitchKey::Enabled, true).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("enabled = true"),
            "the new value lands: {text}"
        );
        assert!(
            text.contains("# Shadow Evolution — the header an operator reads."),
            "the header survives: {text}"
        );
        assert!(
            text.contains("variant_count = 3                    # shadow variants (>=2)"),
            "untouched keys keep their values AND their inline comments: {text}"
        );
        // `toml_edit`'s `value()` replaces the whole value span, so the FLIPPED
        // key's inline comment is spent (the key's own `# ...` tail is part of
        // the value decoration it rebuilds). That is the honest cost of the
        // write: the switch's meaning now lives in the value itself, and the
        // header/section comments — the prose an operator reads — all survive.

        // The kernel's own loader reads the written value back.
        let cfg = crate::config::FileConfig::load(Some(&p));
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        assert_eq!(cfg.shadow.enabled, Some(true));
        assert_eq!(cfg.shadow.auto_evolve, Some(false));
        assert_eq!(cfg.shadow.variant_count, Some(3));

        persist_switch(&p, SwitchKey::AutoEvolve, true).unwrap();
        let cfg = crate::config::FileConfig::load(Some(&p));
        assert_eq!(cfg.shadow.enabled, Some(true));
        assert_eq!(cfg.shadow.auto_evolve, Some(true));

        // And back off: the verb's OFF must land the same way.
        persist_switch(&p, SwitchKey::Enabled, false).unwrap();
        let cfg = crate::config::FileConfig::load(Some(&p));
        assert_eq!(cfg.shadow.enabled, Some(false));
        assert_eq!(cfg.shadow.auto_evolve, Some(true));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_missing_section_or_key_is_created_not_an_error() {
        let p = tmp_path("append");
        std::fs::write(&p, "# a hand-minimal file\n").unwrap();
        persist_switch(&p, SwitchKey::Enabled, true).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("enabled = true"),
            "the key (and its section) are appended: {text}"
        );

        // A file WITH the section but the key commented out gets the key.
        let p2 = tmp_path("append-key");
        std::fs::write(&p2, "[shadow_evolution]\n# enabled = false\n").unwrap();
        persist_switch(&p2, SwitchKey::Enabled, false).unwrap();
        let cfg = crate::config::FileConfig::load(Some(&p2));
        assert_eq!(cfg.shadow.enabled, Some(false));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        let _ = std::fs::remove_dir_all(p2.parent().unwrap());
    }

    #[test]
    fn a_file_that_does_not_parse_is_refused_never_flattened() {
        let p = tmp_path("broken");
        std::fs::write(&p, "[shadow_evolution\nenabled = not a bool").unwrap();
        let err = persist_switch(&p, SwitchKey::Enabled, true).unwrap_err();
        assert!(
            err.message.contains("does not parse"),
            "the refusal names the parse: {}",
            err.message
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "[shadow_evolution\nenabled = not a bool",
            "the operator's file is untouched"
        );
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn an_unreadable_file_is_refused() {
        let p = tmp_path("missing");
        let err = persist_switch(&p, SwitchKey::Enabled, true).unwrap_err();
        assert!(
            err.message.contains("unreadable"),
            "the refusal names the read: {}",
            err.message
        );
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_shadow_evolution_value_that_is_not_a_table_is_refused() {
        let p = tmp_path("nottable");
        std::fs::write(&p, "shadow_evolution = 3\n").unwrap();
        let err = persist_switch(&p, SwitchKey::Enabled, true).unwrap_err();
        assert!(
            err.message.contains("not a TOML table"),
            "the refusal names the shape: {}",
            err.message
        );
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
}
