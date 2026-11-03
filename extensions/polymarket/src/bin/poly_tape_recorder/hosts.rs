//! 网络纪律：主机白名单 + 唯一 URL 构造器（从 poly_tape_recorder.rs 原样迁出）。

/// 主机白名单（Mimosa 纪律）：录制器只许拨这三个公开数据主机。
pub(crate) const ALLOWED_HOSTS: &[&str] = &[
    "ws-subscriptions-clob.polymarket.com", // Polymarket CLOB market WS
    "gamma-api.polymarket.com",             // 轮发现
    "stream.binance.com",                   // 现货 @trade
];

pub(crate) const POLY_WS_HOST: &str = "ws-subscriptions-clob.polymarket.com";
pub(crate) const POLY_WS_MARKET_PATH: &str = "/ws/market";
pub(crate) const GAMMA_HOST: &str = "gamma-api.polymarket.com";
pub(crate) const BINANCE_WS_HOST: &str = "stream.binance.com";

/// 唯一的 URL 构造点：scheme 只许 https/wss、host 必须在白名单内、path 必须
/// 以 `/` 开头（base 可为空串）。先校验，后返回；调用方拿到的字符串才允许
/// 去拨号。
pub(crate) fn build_url(scheme: &str, host: &str, path: &str) -> Result<String, String> {
    if scheme != "https" && scheme != "wss" {
        return Err(format!("scheme 不在 https/wss 内: {scheme:?}"));
    }
    if !ALLOWED_HOSTS.contains(&host) {
        return Err(format!("host 不在白名单内: {host:?}"));
    }
    if !path.is_empty() && !path.starts_with('/') {
        return Err(format!("path 必须以 / 开头: {path:?}"));
    }
    Ok(format!("{scheme}://{host}{path}"))
}

/// 环境变量覆盖的 WS base（如 `wss://ws-subscriptions-clob.polymarket.com`）
/// → (scheme, host)。同样只接受白名单主机，不允许 path（path 由构造器拼）。
pub(crate) fn parse_ws_base(raw: &str) -> Result<(String, String), String> {
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| format!("WS base 缺 scheme: {raw:?}"))?;
    if scheme != "wss" {
        return Err(format!("WS base 必须是 wss: {raw:?}"));
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() {
        return Err(format!("WS base 无 host: {raw:?}"));
    }
    // host 带端口时剥掉再对白名单（白名单主机都不带端口）。
    let bare = host.split(':').next().unwrap_or_default();
    if rest.contains('/') {
        return Err(format!("WS base 不允许带 path: {raw:?}"));
    }
    // 复用构造器做白名单与 scheme 校验（path 为空）。
    build_url(scheme, bare, "").map(|_| (scheme.to_string(), bare.to_string()))
}
