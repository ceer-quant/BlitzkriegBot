//! Built-in example extensions.
//!
//! These demonstrate the extension contract and keep `extension.list` non-empty
//! so lifecycle operations are exercisable without an external plugin.

use super::async_trait_lite::async_trait;
use super::{Extension, ExtensionContext, ExtensionType};

/// A market extension shell for Binance spot. It proves the extension point: it
/// can be installed/enabled/disabled and logs on load, but it performs no trading
/// (trading would require an adapter wired into the order pipeline).
#[derive(Default)]
pub struct BinanceSpotExtension;

impl BinanceSpotExtension {
    pub fn new() -> Self {
        Self
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
        ctx.log(&format!(
            "binance_spot extension loaded (strategies: {:?})",
            ctx.strategy_names()
        ));
        Ok(())
    }

    async fn on_unload(&self) -> Result<(), String> {
        Ok(())
    }
}
