//! momentum_alpha —— BlitzkriegStrategy API 1.0 参考策略（Rust 原生）。
//!
//! 职责边界（读 STRATEGY_GUIDE 的「止损不归你管」章节）：
//!   * 本策略只判断**何时进场**与**何时离场**，并给出理由字符串；
//!   * 不计算止损、不监视止损、不表达止损——生存由内核绑定并执行；
//!   * 仓位只给"建议比例"，内核裁决绝对股数。

use blitzkrieg_strategy_api::{
    dec, export_strategy, BookUpdate, Entry, FreshBook, Intents, RoundContext, SafeStrategy,
    StrategyMode,
};
use blitzkrieg_strategy_api::MarketType; // 由 strategy_api 重导出（见 §11.2 依赖方向）
use blitzkrieg_strategy_api::MarketStructure;
use rust_decimal::Decimal;

#[derive(Default)]
struct MomentumAlpha {
    /// 已确认上一轮出现过的动量标的（回合内记忆）。
    seen: Vec<String>,
    /// 本次回调里观察到的中间价（用于比较相邻两笔）。
    last_mid: Option<Decimal>,
}

impl SafeStrategy for MomentumAlpha {
    fn name(&self) -> &str {
        "momentum_alpha"
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    // ── 模式声明（API 1.0 新增；不声明则不参与兼容性校验）────────────────────
    fn declare_modes(&self) -> Vec<StrategyMode> {
        vec![StrategyMode {
            market_type: MarketType::Prediction,
            structure: Some(MarketStructure::BinaryOutcomeWheel),
            // 要求的每一项，插件都必须具备（超集判定，§7.5）
            required_capabilities: blitzkrieg_strategy_api::MarketCapabilities::WEBSOCKET_FEED
                | blitzkrieg_strategy_api::MarketCapabilities::LEVEL2_SNAPSHOT,
        }]
    }

    fn on_book(&mut self, update: &BookUpdate) {
        // 只记中间价；价格一律走 dec()（精确 decimal），绝不经 f64。
        if let Some(mid) = dec(&update.mid) {
            self.last_mid = Some(mid);
        }
    }

    fn on_eval_books(&mut self, _books: &[FreshBook]) {
        // 本策略只信 evaluate 时传入的 ctx；这里不做事。
    }

    fn evaluate(&mut self, ctx: &RoundContext) -> Intents {
        let mut out = Intents::none();
        let Some(mid) = self.last_mid else { return out };

        for m in &ctx.markets {
            // 回合剩余时间太短就放弃入场（把"该不该在这里交易"留给内核，
            // 但"还值不值得开新仓"是策略自己的判断）。
            if ctx.round.time_left_sec < 240 {
                continue;
            }
            // 动量条件：中间价在 [0.35, 0.62] 区间表明向上动量。
            let want = mid >= Decimal::new(35, 2) && mid <= Decimal::new(62, 2);
            if !want || self.seen.contains(&m.up_token) {
                continue;
            }

            self.seen.push(m.up_token.clone());
            out.entries.push(Entry {
                token: m.up_token.clone(),
                // 限价：以中间价报价，具体能否成交由内核按盘口裁决。
                price: format!("{mid}"),
                reason: "momentum_alpha: mid in momentum band, round has time left".into(),
                // 不给绝对股数 → 内核按账户净值与风控上限定寸。
                shares: None,
            });
        }
        out
    }

    fn take_breaks(&mut self) -> Vec<blitzkrieg_strategy_api::Break> {
        Vec::new()
    }

    /// 离场建议：**只表达"理由消失"**，不表达止损、不表达价格。
    /// 内核会把它与自身的出场纪律（止损/时间退出/阶梯）合并裁决。
    fn confirmed_tokens(&self) -> Vec<String> {
        self.seen.clone()
    }

    fn diagnostics(&self) -> Vec<serde_json::Value> {
        vec![serde_json::json!({
            "seen": self.seen.len(),
            "last_mid": self.last_mid.map(|d| d.to_string()),
        })]
    }
}

// 生成全部 ABI 表面：vtable、bk_strategy_create / abi_version / free_string /
// declare_modes（本策略声明非空 → 宏会导出真实载荷）。
export_strategy!(crate::MomentumAlpha);
