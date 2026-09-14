//! Built-in example extensions.
//!
//! These demonstrate the extension contract and keep `extension.list` non-empty
//! so lifecycle operations are exercisable without an external plugin.

use super::async_trait_lite::async_trait;
use super::{Extension, ExtensionContext, ExtensionType};
use crate::ipc::schema::Event;

/// A market extension shell for Binance spot. It proves the extension point:
/// it can be installed/enabled/disabled and receives events, but it performs no
/// trading (trading would require an adapter wired into the order pipeline).
pub struct BinanceSpotExtension {
    enabled_at: std::sync::atomic::AtomicI64,
    events_seen: std::sync::atomic::AtomicU64,
}

impl BinanceSpotExtension {
    pub fn new() -> Self {
        Self {
            enabled_at: std::sync::atomic::AtomicI64::new(0),
            events_seen: std::sync::atomic::AtomicU64::new(0),
        }
    }
    pub fn events_seen(&self) -> u64 {
        self.events_seen.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Default for BinanceSpotExtension {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Extension for BinanceSpotExtension {
    fn name(&self) -> &str {
        "binance_spot"
    }
    fn version(&self) -> &str {
        "0.1.0"
    }
    fn extension_type(&self) -> ExtensionType {
        ExtensionType::Market
    }

    async fn on_load(&self, ctx: &dyn ExtensionContext) -> Result<(), String> {
        // Record load; read the read-only strategy view the context exposes.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.enabled_at.store(now, std::sync::atomic::Ordering::SeqCst);
        ctx.log(&format!("binance_spot extension loaded (strategies: {:?})", ctx.strategy_names()));
        Ok(())
    }

    async fn on_unload(&self) -> Result<(), String> {
        Ok(())
    }

    async fn on_event(&self, _event: &Event) -> Result<(), String> {
        self.events_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}
