#![cfg(test)]

use super::*;
use crate::risk::RiskConfig;
use rust_decimal_macros::dec;

/// Records every `set_tokens` call — the mock the multi-plugin routing
/// test observes through.
struct RecordingControl(std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>);

impl blitzkrieg_market_api::SubscriptionControl for RecordingControl {
    fn set_tokens(&self, tokens: Vec<blitzkrieg_market_api::TokenId>) {
        self.0.lock().unwrap().push(tokens);
    }
}

#[tokio::test]
async fn subscription_controls_route_per_plugin() {
    // Multi-plugin data plane: two installed controls, two subscribe calls —
    // each control must see ONLY its own plugin's token set (replacement
    // semantics are per-venue; a Polymarket rollover must not wipe a
    // Kalshi subscription).
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        ..Default::default()
    });
    let poly_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let kalshi_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    c.set_subscription(
        "polymarket",
        std::sync::Arc::new(RecordingControl(poly_log.clone())),
    );
    c.set_subscription(
        "kalshi",
        std::sync::Arc::new(RecordingControl(kalshi_log.clone())),
    );

    c.subscribe_feed_tokens("polymarket", vec!["poly-up".into(), "poly-down".into()])
        .await;
    c.subscribe_feed_tokens("kalshi", vec!["kalshi-up".into()])
        .await;

    assert_eq!(
        *poly_log.lock().unwrap(),
        vec![vec!["poly-up".to_string(), "poly-down".to_string()]],
        "the polymarket control receives exactly the polymarket tokens"
    );
    assert_eq!(
        *kalshi_log.lock().unwrap(),
        vec![vec!["kalshi-up".to_string()]],
        "the kalshi control receives exactly the kalshi tokens"
    );

    // A same-token replacement only touches the named control.
    c.subscribe_feed_tokens("kalshi", vec!["kalshi-next".into()])
        .await;
    assert_eq!(
        poly_log.lock().unwrap().len(),
        1,
        "the sibling venue's subscription set is untouched"
    );
    assert_eq!(
        *kalshi_log.lock().unwrap(),
        vec![
            vec!["kalshi-up".to_string()],
            vec!["kalshi-next".to_string()]
        ],
        "the named control keeps its replacement semantics"
    );

    // An unknown plugin name is an inert no-op (no panic, no cross-talk).
    c.subscribe_feed_tokens("predictfun", vec!["x".into()])
        .await;
    assert_eq!(poly_log.lock().unwrap().len(), 1);
    assert_eq!(kalshi_log.lock().unwrap().len(), 2);
}
