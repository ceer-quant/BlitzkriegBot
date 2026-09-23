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
