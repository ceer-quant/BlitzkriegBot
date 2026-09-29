//! Strategy ↔ plugin compatibility handshake (DEV_V0_3 §7.5 / §8.2).
//!
//! Directionality first, because it is the one rule here that is easy to get
//! backwards (§7.5): **a strategy may be loose, a plugin must be specific.**
//! A plugin that declares `structure: None` says "I adapt to every structure
//! under this market type" — allowed, but it is then INCOMPATIBLE with a
//! strategy that requires a concrete structure: a plugin that cannot say
//! whether it is a CLOB cannot promise CLOB semantics.
//!
//! One strategy mode matches one plugin mode iff
//!   (1) market_type is equal (a strong match, never a substring game),
//!   (2) the strategy's structure is `None` (doesn't care) or equal,
//!   (3) the plugin's capabilities are a SUPERSET of the strategy's
//!       requirements (`MarketCapabilities::satisfies`).
//! A strategy is loadable iff it is UNDECLARED (empty — participates not, the
//! 0.2 default) or at least one declared pair matches.

use blitzkrieg_market_api::{MarketMode, MarketPlugin};
use blitzkrieg_strategy_api::StrategyMode;

/// The active plugin's declaration as the handshake sees it, injected by the
/// server assembly BEFORE any strategy is loaded or enabled (§8.2: all three
/// handshake sites judge against the same snapshot).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginModes {
    pub name: String,
    pub modes: Vec<MarketMode>,
}

impl PluginModes {
    /// Snapshot one plugin's declaration. An UNDECLARED plugin (no
    /// `declare_modes` override) becomes ONE implicit market_type-only mode:
    /// it names its market class and promises neither a structure nor any
    /// capability — so a strategy requiring either is refused, which is the
    /// honest direction (§7.2: the design doc does not guess bitmaps for
    /// plugins, and neither does this fallback).
    pub fn for_plugin(p: &dyn MarketPlugin) -> Self {
        let declared = p.declare_modes();
        let modes = if declared.is_empty() {
            vec![MarketMode {
                market_type: p.market_type(),
                structure: None,
                capabilities: blitzkrieg_market_api::MarketCapabilities::NONE,
            }]
        } else {
            declared
        };
        Self {
            name: p.name().to_string(),
            modes,
        }
    }
}

/// The §8.2 diagnostic. Every refusal names BOTH sides — what the strategy
/// wants, what the active plugin offers — because "strategy load failed" is
/// not diagnosable and "market mismatch" without the two lists is a guess.
/// The gate (R-B) asserts the reason carries the strategy's market tag, the
/// plugin's market tag and the plugin NAME.
pub fn check_compatibility(
    strategy: &str,
    modes: &[StrategyMode],
    plugin: &PluginModes,
) -> Result<(), String> {
    // Undeclared strategy: participates not (§8.1 — every 0.2 library's state;
    // the handshake must be invisible to it).
    if modes.is_empty() {
        return Ok(());
    }
    if plugin.modes.is_empty() {
        // No active plugin at all (backtester, market-free build): there is
        // nothing to be incompatible WITH. Whether trading can work without a
        // market is not this gate's question.
        return Ok(());
    }
    for m in modes {
        for p in &plugin.modes {
            if pair_compatible(m, p) {
                return Ok(());
            }
        }
    }
    Err(format!(
        "incompatible modes: strategy '{strategy}' wants [{}] but active plugin '{}' offers [{}]",
        modes
            .iter()
            .map(render_strategy_mode)
            .collect::<Vec<_>>()
            .join("; "),
        plugin.name,
        plugin
            .modes
            .iter()
            .map(render_plugin_mode)
            .collect::<Vec<_>>()
            .join("; "),
    ))
}

/// One (strategy mode, plugin mode) pair under §7.5 (1)-(3).
fn pair_compatible(m: &StrategyMode, p: &MarketMode) -> bool {
    m.market_type == p.market_type
        && (m.structure.is_none() || m.structure == p.structure)
        && p.capabilities.satisfies(m.required_capabilities)
}

/// Wire tag of a serde `snake_case` enum value, via the ONE serializer — a
/// hand-rolled match here would be a second spelling of the wire that the
/// declaration codec and `market.list` already own.
fn wire_tag<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|x| x.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// `market_type[/structure](cap,cap,…)` — the want side of the diagnostic.
/// An absent structure renders bare (`futures`): the strategy does not care.
fn render_strategy_mode(m: &StrategyMode) -> String {
    let mut s = wire_tag(&m.market_type);
    if let Some(st) = &m.structure {
        s.push('/');
        s.push_str(&wire_tag(st));
    }
    push_caps(
        &mut s,
        &blitzkrieg_market_api::modes::capabilities_to_names(m.required_capabilities),
    );
    s
}

/// The offer side, same spelling. A plugin mode with `structure: None`
/// renders bare — and that is exactly the case that cannot match a strategy
/// requiring a concrete structure (the directionality rule above).
fn render_plugin_mode(p: &MarketMode) -> String {
    let mut s = wire_tag(&p.market_type);
    if let Some(st) = &p.structure {
        s.push('/');
        s.push_str(&wire_tag(st));
    }
    push_caps(
        &mut s,
        &blitzkrieg_market_api::modes::capabilities_to_names(p.capabilities),
    );
    s
}

fn push_caps(s: &mut String, caps: &[&str]) {
    if !caps.is_empty() {
        s.push('(');
        s.push_str(&caps.join(","));
        s.push(')');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitzkrieg_market_api::{MarketCapabilities, MarketStructure, MarketType};

    fn plugin(name: &str, modes: Vec<MarketMode>) -> PluginModes {
        PluginModes {
            name: name.into(),
            modes,
        }
    }

    fn plugin_mode(
        mt: MarketType,
        st: Option<MarketStructure>,
        caps: MarketCapabilities,
    ) -> MarketMode {
        MarketMode {
            market_type: mt,
            structure: st,
            capabilities: caps,
        }
    }

    fn strategy_mode(
        mt: MarketType,
        st: Option<MarketStructure>,
        caps: MarketCapabilities,
    ) -> StrategyMode {
        StrategyMode {
            market_type: mt,
            structure: st,
            required_capabilities: caps,
        }
    }

    // ── §7.5 (1)-(3), the passing half ──────────────────────────────────────

    #[test]
    fn undeclared_strategy_participates_not() {
        let p = plugin(
            "polymarket",
            vec![plugin_mode(
                MarketType::Prediction,
                Some(MarketStructure::BinaryOutcomeWheel),
                MarketCapabilities::NONE,
            )],
        );
        assert!(check_compatibility("legacy", &[], &p).is_ok());
    }

    #[test]
    fn loose_strategy_matches_concrete_plugin() {
        let p = plugin(
            "polymarket",
            vec![plugin_mode(
                MarketType::Prediction,
                Some(MarketStructure::BinaryOutcomeWheel),
                MarketCapabilities::NONE,
            )],
        );
        let modes = vec![strategy_mode(
            MarketType::Prediction,
            None,
            MarketCapabilities::NONE,
        )];
        assert!(check_compatibility("s", &modes, &p).is_ok());
    }

    #[test]
    fn equal_structure_and_superset_capabilities_match() {
        let p = plugin(
            "polymarket",
            vec![plugin_mode(
                MarketType::Prediction,
                Some(MarketStructure::BinaryOutcomeWheel),
                MarketCapabilities::WEBSOCKET_FEED | MarketCapabilities::LEVEL2_SNAPSHOT,
            )],
        );
        let modes = vec![strategy_mode(
            MarketType::Prediction,
            Some(MarketStructure::BinaryOutcomeWheel),
            MarketCapabilities::WEBSOCKET_FEED,
        )];
        assert!(check_compatibility("s", &modes, &p).is_ok());
    }

    #[test]
    fn any_compatible_pair_out_of_many_suffices() {
        let p = plugin(
            "multi",
            vec![
                plugin_mode(
                    MarketType::Futures,
                    Some(MarketStructure::CentralLimitOrderBook),
                    MarketCapabilities::NONE,
                ),
                plugin_mode(
                    MarketType::Prediction,
                    Some(MarketStructure::BinaryOutcomeWheel),
                    MarketCapabilities::NONE,
                ),
            ],
        );
        let modes = vec![
            strategy_mode(MarketType::Spot, None, MarketCapabilities::NONE),
            strategy_mode(
                MarketType::Prediction,
                Some(MarketStructure::BinaryOutcomeWheel),
                MarketCapabilities::NONE,
            ),
        ];
        assert!(check_compatibility("s", &modes, &p).is_ok());
    }

    #[test]
    fn no_active_plugin_is_not_a_refusal() {
        let p = PluginModes::default();
        let modes = vec![strategy_mode(
            MarketType::Futures,
            Some(MarketStructure::CentralLimitOrderBook),
            MarketCapabilities::NONE,
        )];
        assert!(check_compatibility("s", &modes, &p).is_ok());
    }

    // ── the refusing half — every message names BOTH sides ──────────────────

    #[test]
    fn structure_mismatch_is_refused_and_named_on_both_sides() {
        let p = plugin(
            "polymarket",
            vec![plugin_mode(
                MarketType::Prediction,
                Some(MarketStructure::BinaryOutcomeWheel),
                MarketCapabilities::NONE,
            )],
        );
        let modes = vec![strategy_mode(
            MarketType::Prediction,
            Some(MarketStructure::CentralLimitOrderBook),
            MarketCapabilities::NONE,
        )];
        let err = check_compatibility("clob_strat", &modes, &p).unwrap_err();
        assert!(err.contains("central_limit_order_book"), "{err}");
        assert!(err.contains("binary_outcome_wheel"), "{err}");
        assert!(err.contains("polymarket"), "{err}");
        assert!(
            err.starts_with("incompatible modes: strategy 'clob_strat'"),
            "{err}"
        );
    }

    #[test]
    fn missing_capability_is_refused_and_named() {
        let p = plugin(
            "polymarket",
            vec![plugin_mode(
                MarketType::Prediction,
                Some(MarketStructure::BinaryOutcomeWheel),
                MarketCapabilities::NONE,
            )],
        );
        let modes = vec![strategy_mode(
            MarketType::Prediction,
            None,
            MarketCapabilities::LEVERAGE,
        )];
        let err = check_compatibility("lev_strat", &modes, &p).unwrap_err();
        assert!(err.contains("leverage"), "{err}");
        assert!(err.contains("polymarket"), "{err}");
    }

    #[test]
    fn market_type_mismatch_is_refused_with_both_tags_and_plugin_name() {
        // The R-B shape: a strategy declaring futures against the prediction
        // plugin. The reason must carry `futures`, `prediction` AND the name.
        let p = plugin(
            "polymarket",
            vec![plugin_mode(
                MarketType::Prediction,
                Some(MarketStructure::BinaryOutcomeWheel),
                MarketCapabilities::WEBSOCKET_FEED | MarketCapabilities::LEVEL2_SNAPSHOT,
            )],
        );
        let modes = vec![strategy_mode(
            MarketType::Futures,
            None,
            MarketCapabilities::NONE,
        )];
        let err = check_compatibility("futures_strat", &modes, &p).unwrap_err();
        for want in ["futures", "prediction", "polymarket"] {
            assert!(err.contains(want), "reason lacks {want}: {err}");
        }
    }

    // ── the directionality rule (§7.5, R-C): plugin None ≠ any structure ────

    #[test]
    fn plugin_with_unspecified_structure_cannot_serve_a_concrete_structure() {
        let p = plugin(
            "adapting",
            vec![plugin_mode(
                MarketType::Futures,
                None,
                MarketCapabilities::NONE,
            )],
        );
        let modes = vec![strategy_mode(
            MarketType::Futures,
            Some(MarketStructure::CentralLimitOrderBook),
            MarketCapabilities::NONE,
        )];
        let err = check_compatibility("clob_strat", &modes, &p).unwrap_err();
        assert!(err.contains("central_limit_order_book"), "{err}");
        assert!(err.contains("adapting"), "{err}");
    }

    #[test]
    fn plugin_with_unspecified_structure_still_serves_loose_strategies() {
        let p = plugin(
            "adapting",
            vec![plugin_mode(
                MarketType::Futures,
                None,
                MarketCapabilities::NONE,
            )],
        );
        let modes = vec![strategy_mode(
            MarketType::Futures,
            None,
            MarketCapabilities::NONE,
        )];
        assert!(check_compatibility("s", &modes, &p).is_ok());
    }

    // ── the undeclared-plugin fallback ───────────────────────────────────────

    #[test]
    fn undeclared_plugin_becomes_one_market_type_only_mode() {
        struct Bare;
        impl MarketPlugin for Bare {
            fn name(&self) -> &str {
                "none"
            }
            fn market_type(&self) -> MarketType {
                MarketType::Prediction
            }
        }
        let p = PluginModes::for_plugin(&Bare);
        assert_eq!(p.name, "none");
        assert_eq!(p.modes.len(), 1);
        assert_eq!(p.modes[0].market_type, MarketType::Prediction);
        assert_eq!(p.modes[0].structure, None);
        assert_eq!(p.modes[0].capabilities, MarketCapabilities::NONE);
        // …so a strategy requiring capabilities is refused against it.
        let modes = vec![strategy_mode(
            MarketType::Prediction,
            None,
            MarketCapabilities::WEBSOCKET_FEED,
        )];
        assert!(check_compatibility("s", &modes, &p).is_err());
        // …while a loose prediction strategy still loads (0.2 zero-change).
        let loose = vec![strategy_mode(
            MarketType::Prediction,
            None,
            MarketCapabilities::NONE,
        )];
        assert!(check_compatibility("s", &loose, &p).is_ok());
    }

    #[test]
    fn declared_plugin_snapshot_is_taken_verbatim() {
        struct Declared;
        impl MarketPlugin for Declared {
            fn name(&self) -> &str {
                "decl"
            }
            fn market_type(&self) -> MarketType {
                MarketType::Spot
            }
            fn declare_modes(&self) -> Vec<MarketMode> {
                vec![plugin_mode(
                    MarketType::Spot,
                    Some(MarketStructure::CentralLimitOrderBook),
                    MarketCapabilities::POST_ONLY,
                )]
            }
        }
        let p = PluginModes::for_plugin(&Declared);
        assert_eq!(p.name, "decl");
        assert_eq!(p.modes.len(), 1);
        assert_eq!(
            p.modes[0].structure,
            Some(MarketStructure::CentralLimitOrderBook)
        );
    }
}
