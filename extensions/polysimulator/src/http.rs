//! The plugin's ONE reqwest surface (#841).
//!
//! PolySimulator is an execution-identity venue: its parasitic feed form
//! (`feed_source = Some("polymarket")`, RFC-661 M2) means the plugin owns NO
//! market-data component — feed/discovery speak through bk_contract's traits
//! and are served by the plugin it parasitizes. The two remaining direct
//! `reqwest` construction sites (the REST client's executor transport in
//! `rest.rs` and the net-check's read-only probe in `net_check.rs`) are
//! converged HERE: one module that owns client-builder assembly (User-Agent,
//! timeout, process-proxy posture via [`blitzkrieg_bk_contract::net`]) so the
//! data plane has a single, auditable HTTP face and the "no direct external
//! market-data path" acceptance item is pinned by the deny test in
//! `tests/offline_contract.rs`.
//!
//! Types that cross to the kernel stay bk_contract types (`ClientProxy`,
//! `error_chain`) — the plugin keeps speaking the contract, no market_api
//! internals. Protocol behavior (paths, bodies, retry classification,
//! Rate-After handling) is untouched; only HOW a `reqwest::Client` comes to
//! be is centralized.

use blitzkrieg_bk_contract::net::{ClientProxy, client_proxy, error_chain};
use std::time::Duration;

/// The User-Agent every request carries. The venue's Cloudflare rule 1010s
/// non-browser UAs (curl/python/reqwest defaults) in waves; a browser-shaped
/// string is what their WAF admits. Honest and boring: it identifies nothing
/// false — it is a compatibility knob for a WAF that does not know this
/// venue's own API consumers exist.
pub(crate) const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 BlitzkriegBot/0.4.1";

/// Map the process-level proxy posture (`BK_PROXY` / env) from the contract's
/// vocabulary onto a reqwest builder. Errors carry the contract's error-chain
/// formatting so a bad proxy URL names every cause.
pub(crate) fn apply_bk_proxy(
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::ClientBuilder, String> {
    match client_proxy()? {
        ClientProxy::FromEnvironment => Ok(builder),
        ClientProxy::Disable => Ok(builder.no_proxy()),
        ClientProxy::Explicit(url) => {
            let proxy = reqwest::Proxy::all(url)
                .map_err(|e| format!("proxy URL rejected: {}", error_chain(&e)))?;
            Ok(builder.proxy(proxy))
        }
    }
}

/// Assemble a client the way BOTH of this plugin's HTTP paths are allowed to:
/// a hard per-request timeout, the WAF-compatible User-Agent, and the
/// process-proxy posture. Callers add nothing transport-related — the
/// executor path passes its own `timeout` (the `RestConfig` knob) so the
/// venue's own budget applies to order writes too; the net-check passes the
/// probe budget. `force_no_proxy` is the mock-server tests' escape hatch
/// (reqwest would otherwise route loopback traffic through an ambient system
/// proxy).
pub(crate) fn build_client(
    timeout: Duration,
    force_no_proxy: bool,
) -> Result<reqwest::Client, String> {
    let builder = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(BROWSER_UA);
    let builder = if force_no_proxy {
        builder.no_proxy()
    } else {
        apply_bk_proxy(builder)?
    };
    builder.build().map_err(|e| error_chain(&e))
}
