//! Market plugin registry.
//!
//! Holds the compiled-in market plugins and their enable state. Deliberately
//! separate from `extension::ExtensionRegistry` (which is for audit/lifecycle
//! extensions) so a market plugin's blast radius stays confined to the market
//! seam. Cheap to clone; the shared state sits behind a `std::sync::Mutex`
//! (critical sections are short and never await).

use blitzkrieg_market_api::{MarketPlugin, PluginInfo};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RegistryInner {
    /// Registration order is preserved for stable `market.list` output.
    order: Vec<String>,
    plugins: HashMap<String, Arc<dyn MarketPlugin>>,
    enabled: HashMap<String, bool>,
    /// The plugin actually driving this process (set once at assembly).
    active: Option<String>,
}

#[derive(Clone, Default)]
pub struct MarketPluginRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

impl MarketPluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a plugin under its own name. A later registration with the same
    /// name replaces the earlier one (last-wins), keeping `order` unique.
    pub fn register(&self, plugin: Box<dyn MarketPlugin>) {
        let name = plugin.name().to_string();
        let mut g = self.inner.lock().expect("market registry poisoned");
        if !g.plugins.contains_key(&name) {
            g.order.push(name.clone());
        }
        g.plugins.insert(name.clone(), Arc::from(plugin));
        g.enabled.entry(name).or_insert(false);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn MarketPlugin>> {
        let g = self.inner.lock().expect("market registry poisoned");
        g.plugins.get(name).cloned()
    }

    pub fn set_enabled(&self, name: &str, enabled: bool) {
        let mut g = self.inner.lock().expect("market registry poisoned");
        if g.plugins.contains_key(name) {
            g.enabled.insert(name.to_string(), enabled);
        }
    }

    /// Mark which plugin is actually driving this process. Exactly one should be
    /// active; the assembly calls this once after selection.
    pub fn set_active(&self, name: &str) {
        let mut g = self.inner.lock().expect("market registry poisoned");
        if g.plugins.contains_key(name) {
            g.active = Some(name.to_string());
        }
    }

    pub fn active(&self) -> Option<String> {
        let g = self.inner.lock().expect("market registry poisoned");
        g.active.clone()
    }

    /// Names in registration order.
    pub fn names(&self) -> Vec<String> {
        let g = self.inner.lock().expect("market registry poisoned");
        g.order.clone()
    }

    /// Every registered plugin's DECLARED credential environment names
    /// (issue 648): `plugin → [env var name]`, from
    /// `MarketPlugin::required_credential_env`. A report of what the plugins
    /// CLAIM — the same posture as `allowed_round_secs()`: which env names a
    /// venue's executor reads is the plugin's own contract, never a hardcoded
    /// core table. Plugins returning `&[]` (the default) contribute nothing.
    pub fn credential_vars(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        let g = self.inner.lock().expect("market registry poisoned");
        g.order
            .iter()
            .filter_map(|name| g.plugins.get(name))
            .map(|p| {
                (p.name().to_string(), {
                    let vars = p.required_credential_env();
                    vars.iter().map(|v| v.to_string()).collect::<Vec<_>>()
                })
            })
            .filter(|(_, vars)| !vars.is_empty())
            .collect()
    }

    /// The DECLARED union of round lengths (轮次多族): every registered
    /// plugin's `info().supported_round_secs`, deduplicated and ascending.
    ///
    /// This is a report of what the plugins CLAIM, not kernel knowledge — the
    /// same posture as `names()`/`list()`: which durations can be routed is
    /// venue knowledge and lives in the plugins, never in a hardcoded core
    /// constant. `None` (undeclared) and empty declarations contribute
    /// nothing.
    ///
    /// An EMPTY return (no plugin declares anything) means the caller must
    /// refuse EVERY round duration (fail-closed): with no plugin claiming a
    /// family, accepting any value would be the kernel inventing knowledge
    /// the declaration face never made — not "allow everything".
    pub fn allowed_round_secs(&self) -> Vec<i64> {
        let g = self.inner.lock().expect("market registry poisoned");
        let mut out: Vec<i64> = g
            .order
            .iter()
            .filter_map(|name| g.plugins.get(name))
            .filter_map(|p| p.info().supported_round_secs)
            .flatten()
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The DECLARED union of duration labels (issue 648): every registered
    /// plugin's `MarketPlugin::round_labels`, keyed by seconds — `sec →
    /// label`. First declarer wins: two plugins labeling the same duration
    /// keep the first-registered one's name (the same registration-order
    /// posture as `active_plugin`'s first-registered default), and the
    /// survivor's choice is deterministic, never kernel-invented.
    ///
    /// This is a report of what the plugins CLAIM, not kernel knowledge —
    /// the kernel hardcodes no label table of its own. Plugins returning the
    /// default `&[]` contribute nothing; an EMPTY return means the readout
    /// lists durations unlabeled. The labels stay `&'static str` end to end
    /// — the plugins' declaration tables are constants, so the readout's
    /// wire type (`RoundDurationOption::label`) is untouched.
    pub fn round_labels(&self) -> std::collections::BTreeMap<i64, &'static str> {
        let g = self.inner.lock().expect("market registry poisoned");
        let mut out = std::collections::BTreeMap::new();
        for name in &g.order {
            let Some(p) = g.plugins.get(name) else {
                continue;
            };
            for (sec, label) in p.round_labels() {
                // First declarer wins: `entry` keeps the existing label when
                // the duration is already claimed.
                out.entry(*sec).or_insert(*label);
            }
        }
        out
    }

    /// Capability + state report for `market.list`.
    pub fn list(&self) -> Vec<PluginInfo> {
        let g = self.inner.lock().expect("market registry poisoned");
        g.order
            .iter()
            .filter_map(|name| g.plugins.get(name))
            .map(|p| {
                let mut info = p.info();
                info.enabled = g.enabled.get(&info.name).copied().unwrap_or(false);
                info.active = g.active.as_deref() == Some(info.name.as_str());
                info
            })
            .collect()
    }
}

#[cfg(test)]
mod round_labels_tests {
    use super::*;
    use blitzkrieg_market_api::MarketType;

    /// A stub plugin declaring an optional label table — the registry test's
    /// only lever (the registry is compiled in, so a test registers its own
    /// names; same trick as the `set_tests` stub).
    struct LabeledStub {
        name: String,
        labels: Vec<(i64, &'static str)>,
    }
    impl MarketPlugin for LabeledStub {
        fn name(&self) -> &str {
            &self.name
        }
        fn market_type(&self) -> MarketType {
            MarketType::Prediction
        }
        fn round_labels(&self) -> &[(i64, &'static str)] {
            &self.labels
        }
    }

    #[test]
    fn round_labels_is_the_union_of_the_plugins_declarations_first_declarer_wins() {
        let reg = MarketPluginRegistry::new();
        reg.register(Box::new(LabeledStub {
            name: "a".into(),
            labels: vec![(900, "15m"), (60, "1m")],
        }));
        reg.register(Box::new(LabeledStub {
            name: "b".into(),
            // Same duration, different label: registration order decides —
            // the survivor is deterministic, never kernel-chosen.
            labels: vec![(900, "quarter")],
        }));
        let labels = reg.round_labels();
        assert_eq!(
            labels.get(&900),
            Some(&"15m"),
            "the first declarer's label survives a collision"
        );
        assert_eq!(labels.get(&60), Some(&"1m"));
        assert_eq!(labels.len(), 2);
    }

    #[test]
    fn round_labels_empty_default_and_empty_registry_contribute_nothing() {
        // The trait default (`&[]`) and an empty registry both yield the
        // empty map — the readout then lists durations unlabeled, and the
        // kernel never invents a label of its own.
        let reg = MarketPluginRegistry::new();
        assert!(reg.round_labels().is_empty(), "empty registry");
        reg.register(Box::new(LabeledStub {
            name: "quiet".into(),
            labels: Vec::new(),
        }));
        assert!(reg.round_labels().is_empty(), "default declaration");
    }
}
