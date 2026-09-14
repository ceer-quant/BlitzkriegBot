//! Extension system — the plugin layer.
//!
//! The kernel is the skeleton; extensions are the plugins. New markets,
//! strategies, UIs or data sources are installed/enabled as extensions rather
//! than by editing the kernel.
//!
//! SECURITY MODEL:
//!  - each extension runs in its own async task and holds the shared core only
//!    through the narrowed `ExtensionContext`;
//!  - an extension is NEVER given the signer, credentials, the UDS socket or
//!    direct access to internal state;
//!  - an extension panic/crash must not take down the kernel (supervised task).
//!
//! Lifecycle: discovered → installed → enabled → (running) → disabled → uninstalled.

pub mod builtins;

use crate::ipc::schema::Event;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionType {
    Market,
    Strategy,
    Ui,
    Data,
    Risk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionState {
    Discovered,
    Installed,
    Enabled,
    Disabled,
    Uninstalled,
    Failed,
}

/// The ONLY surface an extension may use to interact with the kernel. It exposes
/// awareness (which events happened) and a way to suggest intent — never keys,
/// the venue client, or the order manager.
pub trait ExtensionContext: Send + Sync {
    /// Emit a kernel event (e.g. `strategy.signal` for audit) to subscribers.
    fn emit(&self, event: Event);
    /// Read-only view of enabled strategy names (for UI extensions).
    fn strategy_names(&self) -> Vec<String>;
    /// Structured log line surfaced to the Node/UI layer.
    fn log(&self, message: &str);
}

/// The contract every extension implements.
#[async_trait_lite::async_trait]
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn extension_type(&self) -> ExtensionType;
    /// Called when the extension is enabled.
    async fn on_load(&self, ctx: &dyn ExtensionContext) -> Result<(), String>;
    /// Called when the extension is disabled/uninstalled.
    async fn on_unload(&self) -> Result<(), String>;
    /// Receive a kernel event (optional; default ignores).
    async fn on_event(&self, _event: &Event) -> Result<(), String> {
        Ok(())
    }
}

/// A registered extension plus its lifecycle state.
pub struct RegisteredExtension {
    pub extension: Box<dyn Extension>,
    pub state: ExtensionState,
    /// Config file path (`extensions/<name>/config.toml`), if any.
    pub config_path: Option<std::path::PathBuf>,
}

/// The kernel-side registry. Owns lifecycle transitions and enforces the
/// discovered→installed→enabled order.
#[derive(Default)]
pub struct ExtensionRegistry {
    entries: HashMap<String, RegisteredExtension>,
}

impl ExtensionRegistry {
    pub fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    /// Register an extension (state = Installed).
    pub fn install(&mut self, ext: Box<dyn Extension>, config_path: Option<std::path::PathBuf>) {
        let name = ext.name().to_string();
        self.entries.insert(name, RegisteredExtension { extension: ext, state: ExtensionState::Installed, config_path });
    }

    /// Enable an extension; runs `on_load`.
    pub async fn enable(&mut self, name: &str, ctx: &dyn ExtensionContext) -> Result<(), String> {
        let Some(e) = self.entries.get_mut(name) else {
            return Err(format!("extension not installed: {name}"));
        };
        match e.extension.on_load(ctx).await {
            Ok(()) => {
                e.state = ExtensionState::Enabled;
                Ok(())
            }
            Err(err) => {
                e.state = ExtensionState::Failed;
                Err(err)
            }
        }
    }

    /// Disable an extension; runs `on_unload`.
    pub async fn disable(&mut self, name: &str) -> Result<(), String> {
        let Some(e) = self.entries.get_mut(name) else {
            return Err(format!("extension not installed: {name}"));
        };
        e.extension.on_unload().await?;
        e.state = ExtensionState::Disabled;
        Ok(())
    }

    pub fn uninstall(&mut self, name: &str) -> bool {
        self.entries.remove(name).is_some()
    }

    /// Fan a kernel event to every enabled extension. Errors are isolated so one
    /// bad extension cannot break the kernel or its peers.
    pub async fn dispatch(&self, event: &Event) {
        for e in self.entries.values() {
            if e.state == ExtensionState::Enabled {
                if let Err(err) = e.extension.on_event(event).await {
                    tracing::warn!(extension = e.extension.name(), error = %err, "extension on_event failed");
                }
            }
        }
    }

    pub fn list(&self) -> Vec<(String, ExtensionType, ExtensionState)> {
        self.entries
            .values()
            .map(|e| (e.extension.name().to_string(), e.extension.extension_type(), e.state))
            .collect()
    }

    pub fn enabled_count(&self) -> usize {
        self.entries.values().filter(|e| e.state == ExtensionState::Enabled).count()
    }
}

/// Minimal async-trait shim: re-exported so extensions don't depend on the
/// `async_trait` crate directly. (A future stage may replace this with native
/// async-in-trait once the MSRV allows it.)
pub mod async_trait_lite {
    pub use async_trait::async_trait;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingCtx {
        emitted: Arc<AtomicUsize>,
    }
    impl ExtensionContext for CountingCtx {
        fn emit(&self, _e: Event) {
            self.emitted.fetch_add(1, Ordering::SeqCst);
        }
        fn strategy_names(&self) -> Vec<String> {
            vec!["spread_arb".into()]
        }
        fn log(&self, _m: &str) {}
    }

    struct DemoExt {
        loaded: Arc<AtomicUsize>,
        events: Arc<AtomicUsize>,
    }
    #[async_trait_lite::async_trait]
    impl Extension for DemoExt {
        fn name(&self) -> &str {
            "demo_market"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn extension_type(&self) -> ExtensionType {
            ExtensionType::Market
        }
        async fn on_load(&self, _ctx: &dyn ExtensionContext) -> Result<(), String> {
            self.loaded.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn on_unload(&self) -> Result<(), String> {
            Ok(())
        }
        async fn on_event(&self, _e: &Event) -> Result<(), String> {
            self.events.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn lifecycle_install_enable_dispatch_disable_uninstall() {
        let loaded = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(AtomicUsize::new(0));
        let ctx = CountingCtx { emitted: Arc::new(AtomicUsize::new(0)) };
        let mut reg = ExtensionRegistry::new();

        reg.install(
            Box::new(DemoExt { loaded: loaded.clone(), events: events.clone() }),
            None,
        );
        assert_eq!(reg.list().len(), 1);
        // Dispatch before enable → no delivery.
        reg.dispatch(&Event::Ready { version: "1".into(), mode: crate::model::Mode::Dry }).await;
        assert_eq!(events.load(Ordering::SeqCst), 0);

        reg.enable("demo_market", &ctx).await.unwrap();
        assert_eq!(loaded.load(Ordering::SeqCst), 1);
        assert_eq!(reg.enabled_count(), 1);
        reg.dispatch(&Event::Ready { version: "1".into(), mode: crate::model::Mode::Dry }).await;
        assert_eq!(events.load(Ordering::SeqCst), 1);

        reg.disable("demo_market").await.unwrap();
        reg.dispatch(&Event::Ready { version: "1".into(), mode: crate::model::Mode::Dry }).await;
        assert_eq!(events.load(Ordering::SeqCst), 1); // disabled → no delivery
        assert!(reg.uninstall("demo_market"));
        assert_eq!(reg.list().len(), 0);
    }

    #[tokio::test]
    async fn failing_load_marks_failed_not_panic() {
        struct BadExt;
        #[async_trait_lite::async_trait]
        impl Extension for BadExt {
            fn name(&self) -> &str {
                "bad"
            }
            fn version(&self) -> &str {
                "0"
            }
            fn extension_type(&self) -> ExtensionType {
                ExtensionType::Risk
            }
            async fn on_load(&self, _ctx: &dyn ExtensionContext) -> Result<(), String> {
                Err("boom".into())
            }
            async fn on_unload(&self) -> Result<(), String> {
                Ok(())
            }
        }
        let ctx = CountingCtx { emitted: Arc::new(AtomicUsize::new(0)) };
        let mut reg = ExtensionRegistry::new();
        reg.install(Box::new(BadExt), None);
        assert!(reg.enable("bad", &ctx).await.is_err());
        let list = reg.list();
        assert_eq!(list[0].2, ExtensionState::Failed);
    }
}
