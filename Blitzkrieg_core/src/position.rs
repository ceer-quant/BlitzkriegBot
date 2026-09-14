//! Position manager — open positions, PnL, exit checks, cooldowns, capacity.
//!
//! Rust port of `src/strategies/crypto-hft/positions.ts`. Decision logic is
//! delegated to the pure `exit_policy` module so offline replay and both
//! runtimes share one implementation; this module owns the stateful parts:
//! the open/closed books, daily PnL, and per-asset/direction cooldowns.

use crate::exit_policy::{
    decide_exit, executable_bid, pnl_pct, update_exit_state, ExitConfig, ExitState, ExitTickInput,
};
use crate::model::{ExitReason, OrderbookSnapshot, Side, SignalDirection};

use rust_decimal::Decimal;

/// Per-unit taker fee percentage (Polymarket formula).
fn taker_fee_pct(price: Decimal) -> Decimal {
    crate::exit_policy::taker_fee_pct(price)
}

#[derive(Debug, Clone)]
pub struct PositionConfig {
    pub exit: ExitConfig,
    pub max_positions: usize,
    pub max_daily_loss_usd: Decimal,
    pub stop_loss_cooldown_sec: i64,
    pub exit_cooldown_sec: i64,
    pub asset_cooldown_sec: i64,
    pub loss_cooldown_sec: i64,
    /// Shares withheld from a sell to avoid rounding oversell.
    pub exit_share_buffer: Decimal,
}

impl Default for PositionConfig {
    fn default() -> Self {
        Self {
            exit: ExitConfig::default(),
            max_positions: 2,
            max_daily_loss_usd: Decimal::from(200),
            stop_loss_cooldown_sec: 180,
            exit_cooldown_sec: 60,
            asset_cooldown_sec: 90,
            loss_cooldown_sec: 180,
            exit_share_buffer: Decimal::new(1, 2), // 0.01
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenPosition {
    pub id: String,
    pub strategy: String,
    pub asset: String,
    pub direction: SignalDirection,
    pub token_id: String,
    pub condition_id: String,
    pub entry_price: Decimal,
    pub current_price: Decimal,
    pub prev_price: Decimal,
    pub shares: Decimal,
    pub cost_usd: Decimal,
    pub was_maker_entry: bool,
    pub entry_fee_pct: Decimal,
    pub target_exit_price: Option<Decimal>,
    pub entered_at_ms: i64,
    pub expires_at_ms: i64,
    pub state: ExitState,
}

impl OpenPosition {
    pub fn high_pnl_pct(&self) -> Decimal {
        self.state.high_pnl_pct
    }
    pub fn low_pnl_pct(&self) -> Decimal {
        self.state.low_pnl_pct
    }
}

#[derive(Debug, Clone)]
pub struct ClosedPosition {
    pub id: String,
    pub strategy: String,
    pub asset: String,
    pub direction: SignalDirection,
    pub token_id: String,
    pub condition_id: String,
    pub entry_price: Decimal,
    pub exit_price: Decimal,
    pub shares: Decimal,
    pub cost_usd: Decimal,
    pub was_maker_entry: bool,
    pub was_maker_exit: bool,
    pub entry_fee_pct: Decimal,
    pub exit_fee_pct: Decimal,
    pub pnl_usd: Decimal,
    pub pnl_pct: Decimal,
    pub net_pnl_usd: Decimal,
    pub net_pnl_pct: Decimal,
    pub high_pnl_pct: Decimal,
    pub low_pnl_pct: Decimal,
    pub hold_time_sec: i64,
    pub exit_reason: ExitReason,
    pub entered_at_ms: i64,
    pub exited_at_ms: i64,
}

pub struct OpenParams {
    pub strategy: String,
    pub asset: String,
    pub direction: SignalDirection,
    pub token_id: String,
    pub condition_id: String,
    pub entry_price: Decimal,
    pub shares: Decimal,
    pub expires_at_ms: i64,
    pub was_maker: bool,
    pub target_exit_price: Option<Decimal>,
}

#[derive(Debug, Clone)]
pub struct ExitRequest {
    pub position_id: String,
    pub reason: ExitReason,
    pub exit_price: Decimal,
    pub use_maker: bool,
}

pub struct PositionManager {
    config: PositionConfig,
    open: Vec<OpenPosition>,
    closed: Vec<ClosedPosition>,
    next_id: u64,
    daily_pnl: Decimal,
    last_stop_loss_at: i64,
    exit_cooldowns: std::collections::HashMap<String, i64>,
    asset_last_exit_at: std::collections::HashMap<String, i64>,
    asset_last_loss_at: std::collections::HashMap<String, i64>,
}

fn cooldown_key(asset: &str, direction: SignalDirection) -> String {
    format!("{asset}_{}", direction.as_str())
}

impl PositionManager {
    pub fn new(config: PositionConfig) -> Self {
        Self {
            config,
            open: Vec::new(),
            closed: Vec::new(),
            next_id: 1,
            daily_pnl: Decimal::ZERO,
            last_stop_loss_at: 0,
            exit_cooldowns: Default::default(),
            asset_last_exit_at: Default::default(),
            asset_last_loss_at: Default::default(),
        }
    }

    pub fn set_config(&mut self, config: PositionConfig) {
        self.config = config;
    }

    pub fn open_positions(&self) -> &[OpenPosition] {
        &self.open
    }
    pub fn closed_positions(&self) -> &[ClosedPosition] {
        &self.closed
    }
    pub fn daily_pnl(&self) -> Decimal {
        self.daily_pnl
    }
    pub fn reset_daily(&mut self) {
        self.daily_pnl = Decimal::ZERO;
        self.last_stop_loss_at = 0;
        self.exit_cooldowns.clear();
    }

    pub fn open(&mut self, p: OpenParams, now_ms: i64) -> OpenPosition {
        let id = format!("hft-{}", self.next_id);
        self.next_id += 1;
        let entry_fee_pct = if p.was_maker { Decimal::ZERO } else { taker_fee_pct(p.entry_price) };
        let pos = OpenPosition {
            id,
            strategy: p.strategy,
            asset: p.asset,
            direction: p.direction,
            token_id: p.token_id,
            condition_id: p.condition_id,
            entry_price: p.entry_price,
            current_price: p.entry_price,
            prev_price: p.entry_price,
            shares: p.shares,
            cost_usd: p.entry_price * p.shares,
            was_maker_entry: p.was_maker,
            entry_fee_pct,
            target_exit_price: p.target_exit_price,
            entered_at_ms: now_ms,
            expires_at_ms: p.expires_at_ms,
            state: ExitState::new(p.entry_price, now_ms),
        };
        self.open.push(pos.clone());
        pos
    }

    /// Update a position's exit state from a fresh book.
    pub fn tick(&mut self, position_id: &str, book: Option<&OrderbookSnapshot>, now_ms: i64) {
        let cfg = self.config.exit.clone();
        if let Some(pos) = self.open.iter_mut().find(|p| p.id == position_id) {
            Self::valuate_one(pos, book, now_ms, &cfg);
        }
    }

    /// Update a single position's exit state AND its `current_price` from a book.
    /// This is what makes the UI's unrealized PnL move; it must run independently
    /// of whether automated exits are enabled.
    fn valuate_one(pos: &mut OpenPosition, book: Option<&OrderbookSnapshot>, now_ms: i64, cfg: &ExitConfig) {
        update_exit_state(&mut pos.state, pos.entry_price, book, now_ms, cfg);
        let val = executable_bid(book);
        if val > Decimal::ZERO {
            if pos.current_price != val {
                pos.prev_price = pos.current_price;
                pos.current_price = val;
            }
        }
    }

    /// Re-value every open position from the latest books (no exit decisions).
    /// Safe to call even when automated exits are disabled, so the dashboard
    /// always shows live unrealized PnL and HWM.
    pub fn valuate(&mut self, books: &dyn Fn(&str) -> Option<OrderbookSnapshot>, now_ms: i64) {
        let cfg = self.config.exit.clone();
        for pos in self.open.iter_mut() {
            let book = books(&pos.token_id);
            Self::valuate_one(pos, book.as_ref(), now_ms, &cfg);
        }
    }

    /// Evaluate every open position; returns exit requests for those that hit a
    /// rule. Updates exit state from the book first so the decision and the
    /// recorded HWM share the same quote.
    pub fn check_exits(&mut self, books: &dyn Fn(&str) -> Option<OrderbookSnapshot>, now_ms: i64) -> Vec<ExitRequest> {
        let cfg = self.config.exit.clone();
        let mut out = Vec::new();
        for pos in self.open.iter_mut() {
            let book = books(&pos.token_id);
            Self::valuate_one(pos, book.as_ref(), now_ms, &cfg);
            let exit_price = executable_bid(book.as_ref());
            let exit_price = if exit_price > Decimal::ZERO {
                exit_price
            } else if pos.current_price > Decimal::ZERO {
                pos.current_price
            } else {
                continue;
            };
            let time_left_sec = (pos.expires_at_ms - now_ms) / 1000;
            let hold_sec = (now_ms - pos.entered_at_ms) / 1000;

            // Fixed-target strategies (e.g. sharp_reversal).
            if let Some(t) = pos.target_exit_price {
                if exit_price >= t && hold_sec >= self.config.exit.exit_grace_sec {
                    out.push(ExitRequest {
                        position_id: pos.id.clone(),
                        reason: ExitReason::TakeProfit,
                        exit_price: t,
                        use_maker: true,
                    });
                    continue;
                }
            }

            if let Some(d) = decide_exit(ExitTickInput {
                entry_price: pos.entry_price,
                book: book.as_ref(),
                fallback_price: Some(pos.current_price),
                time_left_sec,
                hold_sec,
                state: &pos.state,
                now_ms,
                cfg: &cfg,
            }) {
                out.push(ExitRequest {
                    position_id: pos.id.clone(),
                    reason: d.reason,
                    exit_price,
                    use_maker: d.use_maker,
                });
            }
        }
        out
    }

    /// Close a position, computing gross/net PnL and setting cooldowns.
    pub fn close(&mut self, position_id: &str, exit_price: Decimal, reason: ExitReason, was_maker: bool, now_ms: i64) -> Option<ClosedPosition> {
        let idx = self.open.iter().position(|p| p.id == position_id)?;
        let pos = self.open.remove(idx);

        let exit_fee_pct = if was_maker { Decimal::ZERO } else { taker_fee_pct(exit_price) };
        let pnl_pct_val = pnl_pct(exit_price, pos.entry_price);
        let gross = (exit_price - pos.entry_price) * pos.shares;
        let entry_fee_usd = (pos.entry_fee_pct / Decimal::ONE_HUNDRED) * pos.entry_price * pos.shares;
        let exit_fee_usd = (exit_fee_pct / Decimal::ONE_HUNDRED) * exit_price * pos.shares;
        let net = gross - entry_fee_usd - exit_fee_usd;
        let net_pct = if pos.cost_usd > Decimal::ZERO {
            (net / pos.cost_usd) * Decimal::ONE_HUNDRED
        } else {
            Decimal::ZERO
        };
        let hold_time_sec = (now_ms - pos.entered_at_ms) / 1000;

        self.daily_pnl += net;
        if reason == ExitReason::StopLoss {
            self.last_stop_loss_at = now_ms;
        }
        self.exit_cooldowns.insert(cooldown_key(&pos.asset, pos.direction), now_ms);
        self.asset_last_exit_at.insert(pos.asset.clone(), now_ms);
        if net < Decimal::ZERO {
            self.asset_last_loss_at.insert(pos.asset.clone(), now_ms);
        }

        let closed = ClosedPosition {
            id: pos.id,
            strategy: pos.strategy,
            asset: pos.asset,
            direction: pos.direction,
            token_id: pos.token_id,
            condition_id: pos.condition_id,
            entry_price: pos.entry_price,
            exit_price,
            shares: pos.shares,
            cost_usd: pos.cost_usd,
            was_maker_entry: pos.was_maker_entry,
            was_maker_exit: was_maker,
            entry_fee_pct: pos.entry_fee_pct,
            exit_fee_pct,
            pnl_usd: gross,
            pnl_pct: pnl_pct_val,
            net_pnl_usd: net,
            net_pnl_pct: net_pct,
            high_pnl_pct: pos.state.high_pnl_pct,
            low_pnl_pct: pos.state.low_pnl_pct,
            hold_time_sec,
            exit_reason: reason,
            entered_at_ms: pos.entered_at_ms,
            exited_at_ms: now_ms,
        };
        self.closed.push(closed.clone());
        if self.closed.len() > 5000 {
            let excess = self.closed.len() - 5000;
            self.closed.drain(0..excess);
        }
        Some(closed)
    }

    /// Compute the sell size for a position (shares minus rounding buffer).
    pub fn sell_shares(&self, position_id: &str) -> Option<Decimal> {
        let pos = self.open.iter().find(|p| p.id == position_id)?;
        let raw = pos.shares - self.config.exit_share_buffer;
        if raw <= Decimal::ZERO {
            return None;
        }
        // floor to 2dp
        Some((raw * Decimal::ONE_HUNDRED).floor() / Decimal::ONE_HUNDRED)
    }

    /// Adjust an open position's entry price and share count (averaging in a BUY
    /// or reducing on a partial SELL). No-op if the position is unknown.
    pub fn adjust_open(&mut self, position_id: &str, entry_price: Decimal, shares: Decimal) {
        if let Some(pos) = self.open.iter_mut().find(|p| p.id == position_id) {
            pos.entry_price = entry_price;
            pos.shares = shares;
            pos.cost_usd = entry_price * shares;
        }
    }

    /// Capacity + cooldown gate (mirrors TS `canOpen`).
    pub fn can_open(&self, asset: Option<&str>, direction: Option<SignalDirection>, now_ms: i64) -> Result<(), String> {
        if self.open.len() >= self.config.max_positions {
            return Err(format!("Max positions ({})", self.config.max_positions));
        }
        if self.daily_pnl <= -self.config.max_daily_loss_usd {
            return Err(format!("Daily loss limit (${})", self.config.max_daily_loss_usd));
        }
        if self.config.stop_loss_cooldown_sec > 0
            && self.last_stop_loss_at > 0
            && now_ms - self.last_stop_loss_at < self.config.stop_loss_cooldown_sec * 1000
        {
            let left = (self.config.stop_loss_cooldown_sec * 1000 - (now_ms - self.last_stop_loss_at)) / 1000 + 1;
            return Err(format!("SL cooldown: {left}s"));
        }
        if let Some(asset) = asset {
            if self.open.iter().any(|p| p.asset == asset) {
                return Err(format!("Already in {asset}"));
            }
        }
        if let (Some(asset), Some(direction)) = (asset, direction) {
            let key = cooldown_key(asset, direction);
            if let Some(&last) = self.exit_cooldowns.get(&key) {
                if now_ms - last < self.config.exit_cooldown_sec * 1000 {
                    let left = (self.config.exit_cooldown_sec * 1000 - (now_ms - last)) / 1000 + 1;
                    return Err(format!("Exit cooldown {asset} {}: {left}s", direction.as_str()));
                }
            }
        }
        if let Some(asset) = asset {
            if self.config.loss_cooldown_sec > 0 {
                if let Some(&last) = self.asset_last_loss_at.get(asset) {
                    if now_ms - last < self.config.loss_cooldown_sec * 1000 {
                        let left = (self.config.loss_cooldown_sec * 1000 - (now_ms - last)) / 1000 + 1;
                        return Err(format!("Loss cooldown {asset}: {left}s"));
                    }
                }
            }
            if self.config.asset_cooldown_sec > 0 {
                if let Some(&last) = self.asset_last_exit_at.get(asset) {
                    if now_ms - last < self.config.asset_cooldown_sec * 1000 {
                        let left = (self.config.asset_cooldown_sec * 1000 - (now_ms - last)) / 1000 + 1;
                        return Err(format!("Asset cooldown {asset}: {left}s"));
                    }
                }
            }
        }
        Ok(())
    }
}

// Convenience: SignalDirection string used for cooldown keys and logs.
impl SignalDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            SignalDirection::Up => "up",
            SignalDirection::Down => "down",
        }
    }
}

// Keep Side referenced (used by callers wiring exits to orders).
#[allow(dead_code)]
fn _side_used(_: Side) {}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn params(asset: &str, dir: SignalDirection, entry: Decimal) -> OpenParams {
        OpenParams {
            strategy: "spread_arb".into(),
            asset: asset.into(),
            direction: dir,
            token_id: format!("tok_{asset}"),
            condition_id: "cond".into(),
            entry_price: entry,
            shares: dec!(10),
            expires_at_ms: 900_000,
            was_maker: true,
            target_exit_price: None,
        }
    }

    #[test]
    fn open_and_close_pnl_is_net_of_fees() {
        let mut pm = PositionManager::new(PositionConfig::default());
        let p = pm.open(params("BTC", SignalDirection::Up, dec!(0.4)), 0);
        assert_eq!(p.shares, dec!(10));
        // Sell at 0.6 as taker → gross 2.0 minus taker exit fee.
        let c = pm.close(&p.id, dec!(0.6), ExitReason::TakeProfit, false, 1000).unwrap();
        assert!(c.net_pnl_usd < dec!(2.0));
        assert!(c.net_pnl_usd > dec!(1.9)); // fee is small around 0.6
        assert_eq!(pm.daily_pnl(), c.net_pnl_usd);
    }

    #[test]
    fn capacity_and_same_asset_are_blocked() {
        let mut cfg = PositionConfig::default();
        cfg.max_positions = 2;
        cfg.asset_cooldown_sec = 0;
        cfg.loss_cooldown_sec = 0;
        cfg.exit_cooldown_sec = 0;
        let mut pm = PositionManager::new(cfg);
        pm.open(params("BTC", SignalDirection::Up, dec!(0.4)), 0);
        assert!(pm.can_open(Some("BTC"), Some(SignalDirection::Up), 1000).is_err());
        assert!(pm.can_open(Some("ETH"), Some(SignalDirection::Up), 1000).is_ok());
        pm.open(params("ETH", SignalDirection::Up, dec!(0.4)), 1000);
        assert!(pm.can_open(Some("SOL"), Some(SignalDirection::Up), 1000).is_err()); // max positions
    }

    #[test]
    fn loss_sets_asset_cooldown() {
        let mut cfg = PositionConfig::default();
        cfg.asset_cooldown_sec = 90;
        cfg.loss_cooldown_sec = 180;
        let mut pm = PositionManager::new(cfg);
        let p = pm.open(params("BTC", SignalDirection::Up, dec!(0.4)), 0);
        pm.close(&p.id, dec!(0.3), ExitReason::StopLoss, false, 10_000).unwrap();
        // Immediately after a loss, re-entry is blocked by asset/loss cooldown.
        assert!(pm.can_open(Some("BTC"), Some(SignalDirection::Down), 11_000).is_err());
        // Far in the future it clears.
        assert!(pm.can_open(Some("BTC"), Some(SignalDirection::Down), 10_000 + 200_000).is_ok());
    }

    #[test]
    fn daily_loss_limit_blocks_new_positions() {
        let mut cfg = PositionConfig::default();
        cfg.max_daily_loss_usd = dec!(1);
        cfg.asset_cooldown_sec = 0;
        cfg.loss_cooldown_sec = 0;
        cfg.stop_loss_cooldown_sec = 0;
        let mut pm = PositionManager::new(cfg);
        let p = pm.open(params("BTC", SignalDirection::Up, dec!(0.4)), 0);
        pm.close(&p.id, dec!(0.05), ExitReason::StopLoss, false, 1000).unwrap();
        assert!(pm.daily_pnl() < dec!(-1));
        assert!(pm.can_open(Some("ETH"), Some(SignalDirection::Up), 2000).is_err());
    }
}
