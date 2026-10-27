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
