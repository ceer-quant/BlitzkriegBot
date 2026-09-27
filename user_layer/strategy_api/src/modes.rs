//! `StrategyMode` — what a strategy declares it can work in (DEV_V0_3 §2.5).
//!
//! One type only, by decision A.1.6: the shared declaration types
//! (`MarketType` / `MarketStructure` / `MarketCapabilities`) live in
//! `blitzkrieg_market_api`, and this module adds just the strategy-side
//! wrapper. Field meanings are IDENTICAL to the plugin-side `MarketMode` —
//! the two types say WHO is declaring, not two different shapes, and the
//! validator is one shared implementation (§7.4).

use blitzkrieg_market_api::modes::{capabilities_to_names, capability_bit};
use blitzkrieg_market_api::{MarketCapabilities, MarketStructure, MarketType};

// The capability-name table lives in `blitzkrieg_market_api::modes` — one
// definition shared by this codec, the load-time validator, E27's plugin side
// and the docs (§7.2). E24 lands it there (with the `BitOr` impl the §A.2.1
// example needs) as a disclosed accompanying edit.

/// One `StrategyMode` as the §2.3 wire object: `market_type` always present
/// (it is the only required field), `structure` only when concrete (`None` =
/// "every structure under this market type" — emitting `null` would drag a
/// second spelling of absence into the protocol), `capabilities` always
/// present (an empty array is the explicit "requires nothing").
pub fn mode_wire_json(mode: &StrategyMode) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("market_type".into(), serde_json::json!(mode.market_type));
    if let Some(structure) = mode.structure {
        obj.insert("structure".into(), serde_json::json!(structure));
    }
    obj.insert(
        "capabilities".into(),
        serde_json::json!(capabilities_to_names(mode.required_capabilities)),
    );
    serde_json::Value::Object(obj)
}

/// The full §2.3 declaration payload for `modes`. The `export_strategy!` macro
/// calls this for a non-empty declaration; an empty one is emitted as `NULL`
/// (the symbol present but declaring nothing), so this is never called empty
/// from the macro — the kernel treats `{"modes":[]}` as an ERROR, not as
/// "undeclared" (§7.4).
pub fn modes_payload_json(modes: &[StrategyMode]) -> String {
    let arr: Vec<serde_json::Value> = modes.iter().map(mode_wire_json).collect();
    serde_json::json!({ "modes": arr }).to_string()
}

// ── the §7.4 validator, strategy side ───────────────────────────────────────
//
// The design puts the shared validator in `core/market_api/src/modes.rs`
// (§7.4, E27's delivery). E24 precedes E27 in the S1 serial pair and still
// owes a working refusal path at load time ("invalid declaration → refuse
// registration", issue #330 task 2), so the strategy-side parse lives here —
// in E24's own territory — on top of the SAME frozen `ModeError` shape from
// Wave 0. E27's plugin-side `parse_modes` lands in market_api with the
// identical rules; the two calls are one contract, two spellings of "who
// declared" (the §7.4 equivalence).
//
// The wire (readable names, NOT a bitmap for capabilities) is the §2.3
// payload exactly:
//   {"modes":[{"market_type":"prediction",
//              "structure":"binary_outcome_wheel",
//              "capabilities":["websocket_feed","level2_snapshot"]}]}
//
// "Empty" is an error, never "undeclared": the ONLY spelling of "undeclared"
// is the absent symbol / NULL (§7.4's table is the normative text).

use blitzkrieg_market_api::ModeError;

/// Parse and validate a §2.3 declaration payload.
///
/// Errors are positioned (`index`) and quote the offending token (`got`) for
/// everything the payload can carry a position for: the point is that an
/// operator can find the typo, not that the load merely fails.
pub fn parse_strategy_modes(payload: &str) -> Result<Vec<StrategyMode>, ModeError> {
    let root: serde_json::Value =
        serde_json::from_str(payload).map_err(|e| ModeError::NotJson(format!("{e}")))?;
    let Some(modes) = root.get("modes").and_then(|m| m.as_array()) else {
        return Err(ModeError::NotJson(
            "payload is not an object with a `modes` array".into(),
        ));
    };
    if modes.is_empty() {
        return Err(ModeError::Empty);
    }
    let mut out = Vec::with_capacity(modes.len());
    for (index, mode) in modes.iter().enumerate() {
        let Some(mt) = mode.get("market_type").and_then(|v| v.as_str()) else {
            // Missing or non-string: the one required field. (A `null` or a
            // number is as invalid as an absent key — both are "not saying
            // which market this is".)
            return Err(ModeError::MissingMarketType { index });
        };
        let market_type: MarketType = serde_json::from_value(serde_json::Value::String(mt.into()))
            .map_err(|_| ModeError::UnknownMarketType {
                index,
                got: mt.to_string(),
            })?;

        let structure = match mode.get("structure") {
            None => None,
            Some(v) => {
                let Some(s) = v.as_str() else {
                    return Err(ModeError::UnknownStructure {
                        index,
                        got: v.to_string(),
                    });
                };
                Some(
                    serde_json::from_value::<MarketStructure>(serde_json::Value::String(s.into()))
                        .map_err(|_| ModeError::UnknownStructure {
                            index,
                            got: s.to_string(),
                        })?,
                )
            }
        };

        let mut caps = MarketCapabilities::NONE;
        match mode.get("capabilities") {
            None => {}
            Some(v) => {
                let Some(list) = v.as_array() else {
                    return Err(ModeError::UnknownCapability {
                        index,
                        got: v.to_string(),
                    });
                };
                for item in list {
                    let Some(name) = item.as_str() else {
                        return Err(ModeError::UnknownCapability {
                            index,
                            got: item.to_string(),
                        });
                    };
                    let Some(bit) = capability_bit(name) else {
                        return Err(ModeError::UnknownCapability {
                            index,
                            got: name.to_string(),
                        });
                    };
                    caps.0 |= bit.0;
                }
            }
        }

        out.push(StrategyMode {
            market_type,
            structure,
            required_capabilities: caps,
        });
    }
    Ok(out)
}

/// One market mode a strategy declares it can work in.
///
/// An empty `Vec` (the `SafeStrategy::declare_modes` default, and what a 0.2
/// library that exports no such symbol yields) means "undeclared" — not
/// "declares nothing": an undeclared strategy does not participate in the
/// compatibility handshake at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyMode {
    /// Required. Missing makes the whole mode invalid — it is NOT "any market".
    pub market_type: MarketType,
    /// Optional. `None` = adapts to every structure under this market_type.
    ///
    /// Directionality (§7.5): the strategy may be loose, the PLUGIN must be
    /// specific. A plugin declaring `structure: None` is not a match for a
    /// strategy that asks for a concrete structure.
    pub structure: Option<MarketStructure>,
    /// Optional. Empty default = requires no capability bits.
    pub required_capabilities: MarketCapabilities,
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitzkrieg_market_api::ModeError;

    // ── legal declarations (the gate's "3 pass" half) ───────────────────────

    #[test]
    fn parses_full_mode_with_capabilities() {
        let out = parse_strategy_modes(
            r#"{"modes":[{"market_type":"prediction",
                          "structure":"binary_outcome_wheel",
                          "capabilities":["websocket_feed","level2_snapshot"]}]}"#,
        )
        .expect("full mode must parse");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].market_type, MarketType::Prediction);
        assert_eq!(out[0].structure, Some(MarketStructure::BinaryOutcomeWheel));
        assert!(
            out[0]
                .required_capabilities
                .satisfies(MarketCapabilities::WEBSOCKET_FEED),
        );
        assert!(
            out[0]
                .required_capabilities
                .satisfies(MarketCapabilities::LEVEL2_SNAPSHOT),
        );
        assert!(
            !out[0]
                .required_capabilities
                .satisfies(MarketCapabilities::LEVERAGE),
        );
    }

    #[test]
    fn parses_minimal_mode_market_type_only() {
        let out = parse_strategy_modes(r#"{"modes":[{"market_type":"futures"}]}"#)
            .expect("minimal mode must parse");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].market_type, MarketType::Futures);
        assert_eq!(out[0].structure, None);
        assert_eq!(out[0].required_capabilities, MarketCapabilities::NONE);
    }

    #[test]
    fn parses_loose_mode_with_empty_capabilities_array() {
        let out = parse_strategy_modes(r#"{"modes":[{"market_type":"spot","capabilities":[]}]}"#)
            .expect("explicit empty capabilities must parse");
        assert_eq!(out[0].required_capabilities, MarketCapabilities::NONE);
    }

    // ── illegal declarations (the gate's "5 refused" half; each error must
    //    carry its position — `index`, plus `got` where a token exists) ──────

    #[test]
    fn rejects_non_json() {
        let err = parse_strategy_modes("not json at all").unwrap_err();
        assert!(matches!(err, ModeError::NotJson(_)), "got {err:?}");
    }

    #[test]
    fn rejects_empty_array() {
        let err = parse_strategy_modes(r#"{"modes":[]}"#).unwrap_err();
        assert!(matches!(err, ModeError::Empty), "got {err:?}");
        // The FAILURE MESSAGE is the gate's assertion object: `Empty` must be
        // in the text, because that is what "declared-but-empty" reads as.
        assert!(err.to_string().contains("Empty"), "{err}");
    }

    #[test]
    fn rejects_missing_market_type_with_index() {
        let err = parse_strategy_modes(r#"{"modes":[{"structure":"central_limit_order_book"}]}"#)
            .unwrap_err();
        assert!(
            matches!(err, ModeError::MissingMarketType { index: 0 }),
            "got {err:?}"
        );
        assert!(err.to_string().contains("index=0"), "{err}");
    }

    #[test]
    fn rejects_unknown_structure_with_index_and_got() {
        let err = parse_strategy_modes(
            r#"{"modes":[{"market_type":"spot"},{"market_type":"spot","structure":"not_a_structure"}]}"#,
        )
        .unwrap_err();
        assert!(
            matches!(err, ModeError::UnknownStructure { index: 1, .. }),
            "got {err:?}"
        );
        let text = err.to_string();
        assert!(text.contains("index=1"), "{text}");
        assert!(text.contains("got=\"not_a_structure\""), "{text}");
    }

    #[test]
    fn rejects_unknown_capability_with_index_and_got() {
        let err = parse_strategy_modes(
            r#"{"modes":[{"market_type":"earn","capabilities":["time_travel"]}]}"#,
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(
            matches!(err, ModeError::UnknownCapability { index: 0, .. }),
            "got {err:?}"
        );
        assert!(text.contains("index=0"), "{text}");
        assert!(text.contains("got=\"time_travel\""), "{text}");
    }

    #[test]
    fn rejects_unknown_market_type_with_index_and_got() {
        let err = parse_strategy_modes(r#"{"modes":[{"market_type":"nft_market"}]}"#).unwrap_err();
        let text = err.to_string();
        assert!(
            matches!(err, ModeError::UnknownMarketType { index: 0, .. }),
            "got {err:?}"
        );
        assert!(text.contains("index=0"), "{text}");
        assert!(text.contains("got=\"nft_market\""), "{text}");
    }

    // ── the codec round-trips through the validator ─────────────────────────

    #[test]
    fn encoded_payload_round_trips_through_the_validator() {
        let modes = vec![StrategyMode {
            market_type: MarketType::Prediction,
            structure: Some(MarketStructure::BinaryOutcomeWheel),
            required_capabilities: MarketCapabilities::WEBSOCKET_FEED
                | MarketCapabilities::LEVEL2_SNAPSHOT,
        }];
        let payload = modes_payload_json(&modes);
        let parsed = parse_strategy_modes(&payload).expect("our own encoding must validate");
        assert_eq!(parsed, modes);
    }
}
