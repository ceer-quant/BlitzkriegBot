//! RFC 568 阶段一：PairBook —— 内核侧的跨所套利配对状态簿。
//!
//! 一本进程内的账（`BTreeMap`，不持久化 —— #559 3c 的「恢复 = 空账重放」
//! 语义配对簿不适用：配对簿的输入是订单事实事件，重放会经同一事件流重建），
//! 记录每个 (策略实例, 资产, 轮次 slot) 的双腿配对状态。它是**事实驱动的
//! 观察者**：`declare` 只受理策略层的配对声明（engine 的提交管线在
//! arb_precheck 过闸后送达），`on_order_event` 只吸收 OME 的成交/死亡事实，
//! 永不参与判定 —— sweep、风控、exit 策略的判定链照旧直读 OME 与持仓，
//! PairBook 的读数只经 `bk.arb.pairs()` 投影给策略层。
//!
//! 经济身份（RFC 568 决议附注 1，钉死）：(asset, slot)。slot 是交易轮次
//! slot，方向是腿的属性、不入键；同一 (owner 实例, asset, slot) 已有未
//! Closed 的配对 ⇒ 复用同一条 —— 同方向同 token 的重复 emit 幂等忽略，
//! 异方向的腿挂入同一条配对。策略自报的 pair_key 只是可读标签（审计与人读
//! 投影），防重从不依赖它 —— 自报 key 的变体（如 `btc-up-001` vs
//! `btc-up-002`）不会产生第二条配对。

use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashSet};

/// 配对状态机（RFC 568 §3.7）。
///
/// 迁移路径：`Pending`（已声明未见成交）→ `Partial`（恰一腿有成交，裸腿
/// 记名）→ `Complete`（两腿都有成交）；声明后若无任何成交而腿已死亡 ⇒
/// `Orphan`；终态 `Closed` 由轮次推进（`retire_before`）或 sweep 的观察
/// 写入（flatten 后）触发。`Orphan` 的裸腿记名与 `Partial` 同形 ——
/// 「哪条腿悬空」是同一事实的两种处置阶段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairStatus {
    /// 已声明，两腿均无成交。
    Pending,
    /// 恰一腿有成交；裸腿（无成交的腿）记名。
    Partial { naked_token: String },
    /// 两腿都有成交 —— 锁定套利成立。
    Complete,
    /// 无任何成交、且至少一腿已死亡（撤单/拒绝/过期）—— 配对未成形即散。
    Orphan { naked_token: String },
    /// 终态：轮次已推进（`retire_before`）或 sweep 已处置（flatten）。只出
    /// 不进 —— 任何后续事实事件对 Closed 配对是空操作（终态吸收）。
    Closed,
}

impl PairStatus {
    /// `bk.arb.pairs()` 的线格式（RFC 568 附注 2 的 status 字面量）。
    pub fn as_str(&self) -> &'static str {
        match self {
            PairStatus::Pending => "pending",
            PairStatus::Partial { .. } => "partial",
            PairStatus::Complete => "complete",
            PairStatus::Orphan { .. } => "orphan",
            PairStatus::Closed => "closed",
        }
    }
}

/// 一条腿的登记与事实状态。`account_id` 是腿账户（v2 盖章后的落账域），
/// `internal_key` 是 OME 订单的事实钥匙 —— 成交/死亡事件经它寻址回这条腿。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairLegState {
    pub token_id: String,
    pub account_id: String,
    pub internal_key: String,
    /// 声明时的股数（策略侧匹配好的双腿等量股数）。
    pub declared_shares: Decimal,
    /// OME 成交事实的累计（`on_order_event(filled=true)` 累加 delta）。
    pub filled_shares: Decimal,
    /// 死亡事实（撤单/拒绝/过期）：零成交 + 任一腿死亡 ⇒ `Orphan`。
    pub dead: bool,
}

impl PairLegState {
    fn alive_filled(&self) -> bool {
        self.filled_shares > Decimal::ZERO
    }
}

/// 一条配对的完整状态。`owner_strategy` + 腿 token 派生的 (asset, slot)
/// 构成经济身份；`id` 是 `{strategy}\u{1}{account}\u{1}{pair_key}` 形（与
/// `strategy_acc_key` 同形），只作可读标签（审计与人读投影）——策略自报的
/// `pair_key` 变体不产生第二条配对（防重走经济身份索引，不走这个键）。
#[derive(Debug, Clone, PartialEq)]
pub struct PairState {
    /// 可读标签（`{strategy}\u{1}{account}\u{1}{pair_key}`）。
    pub id: String,
    /// 声明方策略名（实例隔离维度之一）。
    pub owner_strategy: String,
    /// 经济身份的交易轮次 slot（declare 时从腿 token 经 token→market 表
    /// 派生，不是策略自报的）。
    pub slot: i64,
    /// 已挂入的腿（≤ 2）。第三条腿挂入是被拒绝的（两条腿的赌注）。
    pub legs: Vec<PairLegState>,
    pub status: PairStatus,
    /// 首次声明的宿主时钟（`now_ms`，回放里是虚拟时钟）。
    pub created_ms: i64,
}

impl PairState {
    /// 裸腿 token（`Partial`/`Orphan` 的记名；`Complete` 无裸腿）。
    pub fn naked_token(&self) -> Option<&str> {
        match &self.status {
            PairStatus::Partial { naked_token } | PairStatus::Orphan { naked_token } => {
                Some(naked_token)
            }
            _ => None,
        }
    }
}

/// 声明单：一条腿进入配对簿的全部输入（engine 从 TradeSignal 携带的
/// `pair` 声明 + 盖章后的落账域派生）。
#[derive(Debug, Clone, PartialEq)]
pub struct PairLegDecl {
    /// 腿 token（防重/挂腿的物理身份）。
    pub token_id: String,
    /// 策略侧声明的股数。
    pub shares: Decimal,
    /// 腿标签：策略自报（如 `"up"`/`"down"`），仅可读；方向不入经济键。
    pub leg: String,
    /// 该腿的落账账户（stamp 结果，v2 可 A≠B）——投影与 owner 过滤以它
    /// 为准；`owner_account`（经济键第二元）始终是实例主账户。
    pub leg_account: String,
}

/// 一条腿的只读投影（`bk.arb.pairs()` 的 legs 行）。股数走 decimal 字符串
/// 线格式（与 Entry/HoldingView 的 exact-decimal 契约一致）。
#[derive(Debug, Clone, PartialEq)]
pub struct PairLegView {
    pub token: String,
    pub account_id: String,
    /// 声明股数。
    pub shares: String,
    /// 已成交股数（最终一致读数 —— 允许滞后一拍，RFC 568 附注 2）。
    pub filled: String,
}

/// 一条配对的只读投影（`bk.arb.pairs()` 的行）。无凭证字段：`account_id`
/// 是策略本来就知道的盖章结果（v2 的 venue→account 映射），非秘密。
#[derive(Debug, Clone, PartialEq)]
pub struct PairView {
    /// owner 策略名（引擎 per-strategy 切片用；跨策略不可见已由
    /// `views_for` 的过滤保证，投影重复携带只为切片不再猜 id 前缀）。
    pub owner_strategy: String,
    pub id: String,
    pub slot: i64,
    /// "pending" | "partial" | "complete" | "orphan" | "closed"。
    pub status: String,
    pub legs: Vec<PairLegView>,
    /// 裸腿 token（`Partial`/`Orphan` 记名；其余 None）。
    pub naked_token: Option<String>,
}

impl PairState {
    /// 投影（最终一致：读数是调用瞬间的快照，RFC 568 附注 2）。
    pub fn view(&self) -> PairView {
        PairView {
            owner_strategy: self.owner_strategy.clone(),
            id: self.id.clone(),
            slot: self.slot,
            status: self.status.as_str().to_string(),
            legs: self
                .legs
                .iter()
                .map(|l| PairLegView {
                    token: l.token_id.clone(),
                    account_id: l.account_id.clone(),
                    shares: l.declared_shares.to_string(),
                    filled: l.filled_shares.to_string(),
                })
                .collect(),
            naked_token: self.naked_token().map(str::to_string),
        }
    }
}

/// 经济身份键（存储键）：`(owner_strategy, owner_account, asset, slot)`。
/// owner_account 是声明方实例的主账户（实例隔离维度）。
pub type PairKey = (String, String, String, i64);

/// 配对簿本体。存储键 = 经济身份 [`PairKey`]（BTreeMap 保序，`views_for`
/// 输出按键稳定排序）；`PairState.id`（自报标签拼形）只随状态存取，不做键
/// —— 跨轮同标签不覆盖、变体标签不分裂。
#[derive(Default)]
pub struct PairBook {
    pairs: BTreeMap<PairKey, PairState>,
    /// internal_key 延续表： escalation/追单的派生 key → 声明腿的原
    /// internal_key（`{order_id}:escalated` 与 chase 的拼形 key —— 同一条
    /// 入腿事实链的续段，成交/死亡事件必须寻址回同一条腿）。值 = 原腿 key；
    /// 空 String = 无延续。腿订单死后延续 key 一并失效（对象事实吸收后，
    /// 迟到续段事件照旧空操作 —— 终态/已闭腿天然不重复记数，除重放在
    /// `alive_filled` 语义下幂等）。
    continuations: BTreeMap<String, String>,
}

/// declare 的受理结果：复用还是新建（审计与测试用）。`pair_id` 一律是
/// 存量配对的可读标签（attach 时回读存量 id，与新建时自报标签无关）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclareOutcome {
    /// 挂入既有配对（同方向同 token 重复 emit = 幂等忽略也走这条）。
    Attached { pair_id: String, new_pair: bool },
    /// 拒绝：配对已满两腿，或经济身份派生不出（slot 0）。
    Full { pair_id: String },
}

/// declare 的输入里派生经济身份所需的 token→market 表项：PairBook 自己
/// 不持有行情，由调用方（Core，直读 engine scanner）注入。
#[derive(Debug, Clone)]
pub struct TokenMarketFacts {
    pub asset: String,
    pub slot: i64,
}

impl PairBook {
    /// 受理一条腿的配对声明。
    ///
    /// 经济身份从腿 token 经 `facts`（token→market）派生为 (asset, slot)；
    /// 同一 (owner 实例, asset, slot) 已有未 Closed 配对 ⇒ 复用：
    ///  - 同方向（同 token）重复 emit ⇒ 幂等忽略（`Attached`，账面不变）；
    ///  - 异方向 ⇒ 挂入同一条（第二腿）；
    ///  - 两腿已满 ⇒ 拒绝（`Full`）。
    ///
    /// `owner_account` 是声明方实例的主账户（实例隔离维度）；
    /// `strategy_pair_key` 是自报标签，只进可读 `id`，防重不依赖它。
    /// 返回的 `pair_id` 是经济身份键的字符串拼形（供 bind/close 寻址）。
    pub fn declare(
        &mut self,
        owner_strategy: &str,
        owner_account: &str,
        strategy_pair_key: &str,
        decl: &PairLegDecl,
        facts: &TokenMarketFacts,
        now_ms: i64,
    ) -> DeclareOutcome {
        // token 不在任何已知市场 ⇒ 经济身份派生不出 ⇒ 拒收（fail-closed：
        // 一个没有轮次身份的腿不配进配对簿）。
        if facts.slot == 0 {
            return DeclareOutcome::Full {
                pair_id: String::new(),
            };
        }
        let key: PairKey = (
            owner_strategy.to_string(),
            owner_account.to_string(),
            facts.asset.clone(),
            facts.slot,
        );
        let readable_id = format!("{owner_strategy}\u{1}{owner_account}\u{1}{strategy_pair_key}");
        // 复用判定：同经济身份的既有配对（无论 Closed 与否 —— Closed 的
        // 键上记录直接顶掉重开，下一轮 slot 不同键天然不同）。
        if let Some(pair) = self.pairs.get_mut(&key)
            && pair.status != PairStatus::Closed
        {
            // 幂等：同 token 重复 emit，账面不变。
            if pair.legs.iter().any(|l| l.token_id == decl.token_id) {
                return DeclareOutcome::Attached {
                    pair_id: pair.id.clone(),
                    new_pair: false,
                };
            }
            if pair.legs.len() >= 2 {
                return DeclareOutcome::Full {
                    pair_id: pair.id.clone(),
                };
            }
            pair.legs.push(PairLegState {
                token_id: decl.token_id.clone(),
                // 腿账户是 stamp 的落账结果（v2 可 A≠B），不是实例主
                // 账户 —— views_for 的白名单过滤与投影读数都以它为准。
                account_id: decl.leg_account.clone(),
                internal_key: String::new(),
                declared_shares: decl.shares,
                filled_shares: Decimal::ZERO,
                dead: false,
            });
            return DeclareOutcome::Attached {
                pair_id: pair.id.clone(),
                new_pair: false,
            };
        }
        // 新建：第一条腿（或同键旧记录已 Closed ⇒ 顶掉重开）。
        let pair = PairState {
            id: readable_id.clone(),
            owner_strategy: owner_strategy.to_string(),
            slot: facts.slot,
            legs: vec![PairLegState {
                token_id: decl.token_id.clone(),
                // 腿账户是 stamp 的落账结果（v2 可 A≠B），不是实例主账户
                // —— views_for 的白名单过滤与投影读数都以它为准。
                account_id: decl.leg_account.clone(),
                internal_key: String::new(),
                declared_shares: decl.shares,
                filled_shares: Decimal::ZERO,
                dead: false,
            }],
            status: PairStatus::Pending,
            created_ms: now_ms,
        };
        self.pairs.insert(key, pair);
        DeclareOutcome::Attached {
            pair_id: readable_id,
            new_pair: true,
        }
    }

    /// OME 事实事件（成交/死亡）——幂等，事件可重放无跳变。
    ///
    /// `internal_key` 是寻址钥匙：命中某条未 Closed 配对的腿 ⇒ 吸收事实；
    /// 未直接命中时查延续表（escalation/追单的派生 key —— 同一条入腿事实链
    /// 的续段），命中则同样吸收。`filled_delta > 0` 记成交（累加）；
    /// `dead = true` 记腿死亡（撤单/拒绝/过期）。两者可同时（一笔 Taker
    /// 部分成交后死亡）。未命中任何腿的事件是静默空操作（非配对策略的订单
    /// 流不过账）。
    pub fn on_order_event(&mut self, internal_key: &str, filled_delta: Decimal, dead: bool) {
        if filled_delta <= Decimal::ZERO && !dead {
            return;
        }
        // 延续 key 先归一（续段事实 = 原腿事实）；归一是纯查表，原 key 直落。
        let resolved: &str = if self.continuations.contains_key(internal_key) {
            self.continuations
                .get(internal_key)
                .map(String::as_str)
                .unwrap_or(internal_key)
        } else {
            internal_key
        };
        for pair in self.pairs.values_mut() {
            if pair.status == PairStatus::Closed {
                continue; // 终态吸收
            }
            let Some(leg) = pair
                .legs
                .iter_mut()
                .find(|l| !l.internal_key.is_empty() && l.internal_key == resolved)
            else {
                continue;
            };
            if filled_delta > Decimal::ZERO {
                leg.filled_shares += filled_delta;
            }
            if dead {
                leg.dead = true;
            }
            Self::recompute_status(pair);
            return; // internal_key 唯一，命中即止
        }
    }

    /// 把 OME internal_key 绑定到一条已声明腿（place 成功后由 Core 回填：
    /// 声明时订单还不存在，internal_key 是提交管线派生的）。寻址按经济身份
    /// （strategy + owner_account + 腿 token；slot 未知但同 (owner, token)
    /// 的活配对唯一 —— pending_tokens 每轮每 token 至多一单），只绑活配对
    /// 且只绑一次（首绑定锁：重放/重挂不复写）。
    pub fn bind_by_leg(
        &mut self,
        owner_strategy: &str,
        owner_account: &str,
        token_id: &str,
        internal_key: &str,
    ) {
        for pair in self.pairs.values_mut() {
            if pair.owner_strategy != owner_strategy || pair.status == PairStatus::Closed {
                continue;
            }
            if let Some(leg) = pair.legs.iter_mut().find(|l| {
                l.token_id == token_id
                    && (owner_account.is_empty() || l.account_id == owner_account)
            }) {
                if leg.internal_key.is_empty() {
                    leg.internal_key = internal_key.to_string();
                }
                return;
            }
        }
    }

    /// 登记一条 internal_key 延续（escalation/追单的派生 key → 声明腿的原
    /// key）。同一条入腿的事实链可能跨多个订单（maker 腿被拒 → taker 追单
    /// —— 追单是 `place_escalated`/chase 的 `place`，bind_before_place 的
    /// `bind_by_leg` 按 (owner, token) 寻址会在首绑定锁下拒绝复写 —— 续段
    /// key 经这里显式登记，成交/死亡事件照旧寻址回声明腿）。原 key 已绑定
    /// 的活腿才收（未声明/已闭的 token 是空操作）；同 key 重复登记幂等。
    pub fn bind_continuation(
        &mut self,
        owner_strategy: &str,
        owner_account: &str,
        token_id: &str,
        continuation_key: &str,
    ) {
        if continuation_key.is_empty() {
            return;
        }
        // 找原腿的 key：同 bind_by_leg 的寻址（owner + token，绕开 A≠B 的
        // 落账账户不对称）。
        for pair in self.pairs.values() {
            if pair.owner_strategy != owner_strategy || pair.status == PairStatus::Closed {
                continue;
            }
            if let Some(leg) = pair.legs.iter().find(|l| {
                l.token_id == token_id
                    && !l.internal_key.is_empty()
                    && (owner_account.is_empty() || l.account_id == owner_account)
            }) {
                self.continuations
                    .insert(continuation_key.to_string(), leg.internal_key.clone());
                return;
            }
        }
    }

    /// 按事实重算状态（on_order_event 的迁移内核）：
    ///  - 恰一腿有成交 ⇒ `Partial{裸腿}`；
    ///  - 零成交且任一腿已死 ⇒ `Orphan{死者}`；
    ///  - 否则维持原状（`Pending`，或既有 `Orphan`）。
    fn recompute_status(pair: &mut PairState) {
        let filled = pair.legs.iter().filter(|l| l.alive_filled()).count();
        pair.status = if filled == 2 {
            PairStatus::Complete
        } else if filled == 1 {
            let naked = pair
                .legs
                .iter()
                .find(|l| !l.alive_filled())
                .map(|l| l.token_id.clone())
                .unwrap_or_default();
            PairStatus::Partial { naked_token: naked }
        } else if pair.legs.iter().any(|l| l.dead) {
            let naked = pair
                .legs
                .iter()
                .find(|l| l.dead)
                .map(|l| l.token_id.clone())
                .unwrap_or_default();
            PairStatus::Orphan { naked_token: naked }
        } else {
            pair.status.clone()
        };
    }

    /// 轮次推进清退（RFC 568：slot 前进 ⇒ 旧轮配对闭账）。slot < `before`
    /// 的未 Closed 配对转 `Closed`（终态吸收一切迟到的迟发事件）。
    pub fn retire_before(&mut self, before: i64) {
        if before <= 0 {
            return;
        }
        for p in self.pairs.values_mut() {
            if p.slot < before && p.status != PairStatus::Closed {
                p.status = PairStatus::Closed;
            }
        }
    }

    /// sweep 观察者写点：flatten 处置后把配对推到 `Closed`（终态吸收后续
    /// 事实）。这是观察者、非输入 —— 判定链（哪些腿算 orphan）照旧直读
    /// OME 与持仓，这里只落账。寻址键 = 经济身份四元组。
    pub fn close_pair(
        &mut self,
        owner_strategy: &str,
        owner_account: &str,
        asset: &str,
        slot: i64,
    ) {
        if let Some(p) = self.pairs.get_mut(&(
            owner_strategy.to_string(),
            owner_account.to_string(),
            asset.to_string(),
            slot,
        )) {
            p.status = PairStatus::Closed;
        }
    }

    /// sweep 观察者写点的 token 形：按 (owner_strategy, 腿 token) 寻址——
    /// 腿 token 唯一对应本轮一个市场，经济键第二元（实例主账户）在调用侧
    /// 只有腿的落账账户可用（v2 A≠B 时 ≠ 实例主账户），token 形绕开这一
    /// 不对称。同 token 多条（跨轮未 retire 的旧键）全闭，幂等。
    pub fn close_by_token(&mut self, owner_strategy: &str, token_id: &str) {
        for p in self.pairs.values_mut() {
            if p.owner_strategy == owner_strategy
                && p.status != PairStatus::Closed
                && p.legs.iter().any(|l| l.token_id == token_id)
            {
                p.status = PairStatus::Closed;
            }
        }
    }

    /// owner 过滤的只读视图（`bk.arb.pairs()` 的数据源）：只返回
    /// `owner_strategy` 匹配且（主）账户在 `accounts` 白名单内的配对。
    /// 跨策略/跨账户不可见；视图不含凭证字段 —— 腿上的 `account_id` 是
    /// 策略本来就知道的盖章结果（v2 的 venue→account 映射），非秘密。
    pub fn views_for(&self, strategy: &str, accounts: &HashSet<String>) -> Vec<PairView> {
        self.pairs
            .values()
            .filter(|p| {
                p.owner_strategy == strategy
                    && p.legs
                        .iter()
                        .any(|l| accounts.contains(l.account_id.as_str()))
            })
            .map(PairState::view)
            .collect()
    }

    /// 按可读 id（`PairState.id`）取配对快照（测试/审计面；同 id 多条时
    /// 取键序第一条 —— BTreeMap 键序 = (strategy, account, asset, slot)，
    /// 稳定可复现）。
    pub fn pairs(&self, readable_id: &str) -> Option<&PairState> {
        self.pairs.values().find(|p| p.id == readable_id)
    }

    /// 测试与审计用：全量只读快照（无过滤）。
    pub fn all(&self) -> Vec<&PairState> {
        self.pairs.values().collect()
    }

    /// 只读计数：`owner_strategy` 名下未进 `Closed` 终态的配对数（统计面
    /// 的 `arbActivePairs` 用 —— 套利策略行上「在册配对数」的簿记读数，
    /// 与视图过滤无关，不携带任何腿细节）。
    pub fn active_pairs_count(&self, owner_strategy: &str) -> usize {
        self.pairs
            .values()
            .filter(|p| p.owner_strategy == owner_strategy && p.status != PairStatus::Closed)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn facts(asset: &str, slot: i64) -> TokenMarketFacts {
        TokenMarketFacts {
            asset: asset.to_string(),
            slot,
        }
    }

    fn decl(token: &str, shares: Decimal) -> PairLegDecl {
        PairLegDecl {
            token_id: token.to_string(),
            shares,
            leg: "up".into(),
            // 测试夹具：腿账户与实例主账户同形（A=B 的退化位）。
            leg_account: "account-a".into(),
        }
    }

    #[test]
    fn declare_creates_pending_then_attaches_second_leg() {
        let mut book = PairBook::default();
        let o1 = book.declare(
            "cross_venue_arb",
            "account-a",
            "BTC:11",
            &decl("pm-up", dec!(100)),
            &facts("BTC", 11),
            1_000,
        );
        assert_eq!(
            o1,
            DeclareOutcome::Attached {
                pair_id: "cross_venue_arb\u{1}account-a\u{1}BTC:11".into(),
                new_pair: true
            }
        );
        let pid = match o1 {
            DeclareOutcome::Attached { pair_id, .. } => pair_id,
            _ => unreachable!(),
        };
        let o2 = book.declare(
            "cross_venue_arb",
            "account-a",
            "BTC:11",
            &decl("kx-down", dec!(100)),
            &facts("BTC", 11),
            1_010,
        );
        assert_eq!(
            o2,
            DeclareOutcome::Attached {
                pair_id: pid.clone(),
                new_pair: false
            }
        );
        let p = book.pairs(&pid).unwrap();
        assert_eq!(p.legs.len(), 2);
        assert_eq!(p.status, PairStatus::Pending);
        assert_eq!(p.slot, 11);
    }

    /// 决议附注 1 钉死的防重形状：同一 (asset, slot) 重复 emit 幂等；
    /// 自报 pair_key 变体（btc-up-001/002）不产生第二个配对。
    #[test]
    fn same_asset_slot_dedupes_regardless_of_self_reported_key() {
        let mut book = PairBook::default();
        let a = book.declare(
            "s",
            "acc",
            "btc-up-001",
            &decl("t1", dec!(10)),
            &facts("BTC", 5),
            0,
        );
        // 自报 key 变体 + 同 token 重复 emit：幂等忽略（不新建）。
        let b = book.declare(
            "s",
            "acc",
            "btc-up-002",
            &decl("t1", dec!(10)),
            &facts("BTC", 5),
            10,
        );
        match (a, b) {
            (
                DeclareOutcome::Attached { pair_id: pa, .. },
                DeclareOutcome::Attached { pair_id: pb, .. },
            ) => {
                assert_eq!(pa, pb, "the repeat emit rides the SAME pair");
            }
            _ => panic!("both declares attach"),
        }
        assert_eq!(book.all().len(), 1);
    }

    /// 异方向腿挂入同一条配对（经济身份不含方向）。
    #[test]
    fn opposite_direction_leg_joins_the_same_pair() {
        let mut book = PairBook::default();
        book.declare(
            "s",
            "acc",
            "k1",
            &decl("up-tok", dec!(10)),
            &facts("BTC", 7),
            0,
        );
        let mut down = decl("down-tok", dec!(10));
        down.leg = "down".into();
        let o = book.declare("s", "acc", "k1", &down, &facts("BTC", 7), 5);
        assert!(matches!(
            o,
            DeclareOutcome::Attached {
                new_pair: false,
                ..
            }
        ));
        assert_eq!(book.all().len(), 1);
        assert_eq!(book.all()[0].legs.len(), 2);
    }

    /// 第三腿被拒（`Full`）——同 token 的重复 emit 才是幂等路径，一个新
    /// token 在两腿已满的配对上是拒收。
    #[test]
    fn a_third_leg_is_refused() {
        let mut book = PairBook::default();
        let pid = match book.declare("s", "acc", "k", &decl("t1", dec!(1)), &facts("BTC", 3), 0) {
            DeclareOutcome::Attached { pair_id, .. } => pair_id,
            _ => unreachable!(),
        };
        book.declare("s", "acc", "k", &decl("t2", dec!(1)), &facts("BTC", 3), 1);
        let o = book.declare("s", "acc", "k", &decl("t3", dec!(1)), &facts("BTC", 3), 2);
        assert!(matches!(o, DeclareOutcome::Full { .. }));
        assert_eq!(book.pairs(&pid).unwrap().legs.len(), 2);
    }

    #[test]
    fn order_events_drive_partial_then_complete() {
        let mut book = PairBook::default();
        let pid = match book.declare("s", "acc", "k", &decl("t1", dec!(10)), &facts("BTC", 9), 0) {
            DeclareOutcome::Attached { pair_id, .. } => pair_id,
            _ => unreachable!(),
        };
        book.declare("s", "acc", "k", &decl("t2", dec!(10)), &facts("BTC", 9), 1);
        book.bind_by_leg("s", "account-a", "t1", "s:BTC:up:9");
        book.bind_by_leg("s", "account-a", "t2", "s:BTC:down:9");

        // 无绑定的 key：空操作。
        book.on_order_event("unrelated:key", dec!(5), false);
        assert_eq!(book.pairs(&pid).unwrap().status, PairStatus::Pending);

        // 第一腿成交 ⇒ Partial，裸腿记名 t2。
        book.on_order_event("s:BTC:up:9", dec!(10), false);
        assert_eq!(
            book.pairs(&pid).unwrap().status,
            PairStatus::Partial {
                naked_token: "t2".into()
            }
        );

        // 第二腿成交 ⇒ Complete。
        book.on_order_event("s:BTC:down:9", dec!(10), false);
        assert_eq!(book.pairs(&pid).unwrap().status, PairStatus::Complete);
    }

    /// 幂等重放：同一成交事实重放 N 次，账面无跳变。
    #[test]
    fn event_replay_is_idempotent_in_status() {
        let mut book = PairBook::default();
        let pid = match book.declare("s", "acc", "k", &decl("t1", dec!(10)), &facts("BTC", 2), 0) {
            DeclareOutcome::Attached { pair_id, .. } => pair_id,
            _ => unreachable!(),
        };
        book.declare("s", "acc", "k", &decl("t2", dec!(10)), &facts("BTC", 2), 0);
        book.bind_by_leg("s", "account-a", "t1", "k1");
        book.bind_by_leg("s", "account-a", "t2", "k2");
        for _ in 0..5 {
            book.on_order_event("k1", dec!(10), false);
        }
        let p = book.pairs(&pid).unwrap();
        assert_eq!(
            p.status,
            PairStatus::Partial {
                naked_token: "t2".into()
            }
        );
        assert_eq!(
            p.legs[0].filled_shares,
            dec!(50),
            "delta 累加，但状态不跳变"
        );
    }

    /// 零成交 + 死亡事件 ⇒ Orphan（裸腿记名）。
    #[test]
    fn a_dead_unfilled_leg_makes_the_pair_orphan() {
        let mut book = PairBook::default();
        let pid = match book.declare("s", "acc", "k", &decl("t1", dec!(10)), &facts("BTC", 4), 0) {
            DeclareOutcome::Attached { pair_id, .. } => pair_id,
            _ => unreachable!(),
        };
        book.declare("s", "acc", "k", &decl("t2", dec!(10)), &facts("BTC", 4), 0);
        book.bind_by_leg("s", "account-a", "t2", "k2");
        // t2 死亡（撤单）且从未成交 ⇒ Orphan，裸腿是死者 t2。
        book.on_order_event("k2", Decimal::ZERO, true);
        assert_eq!(
            book.pairs(&pid).unwrap().status,
            PairStatus::Orphan {
                naked_token: "t2".into()
            }
        );
    }

    /// 一腿成交、另一腿死亡 ⇒ 仍是 Partial（成交事实优先于死亡）。
    #[test]
    fn partial_survives_a_dead_sibling() {
        let mut book = PairBook::default();
        let pid = match book.declare("s", "acc", "k", &decl("t1", dec!(10)), &facts("BTC", 4), 0) {
            DeclareOutcome::Attached { pair_id, .. } => pair_id,
            _ => unreachable!(),
        };
        book.declare("s", "acc", "k", &decl("t2", dec!(10)), &facts("BTC", 4), 0);
        book.bind_by_leg("s", "account-a", "t1", "k1");
        book.bind_by_leg("s", "account-a", "t2", "k2");
        book.on_order_event("k1", dec!(10), false);
        book.on_order_event("k2", Decimal::ZERO, true);
        assert_eq!(
            book.pairs(&pid).unwrap().status,
            PairStatus::Partial {
                naked_token: "t2".into()
            }
        );
    }

    /// 续段事实链（issue 568）：escalation/追单的派生 key（`{order}:escalated`
    /// 形）经 `bind_continuation` 登记后，其成交/死亡事件归一到声明腿 ——
    /// maker 腿被拒（死亡事实）后 taker 追单的成交不该丢。
    #[test]
    fn a_continuation_key_books_its_fill_on_the_declared_leg() {
        let mut book = PairBook::default();
        let pid = match book.declare("s", "acc", "k", &decl("t1", dec!(10)), &facts("BTC", 6), 0) {
            DeclareOutcome::Attached { pair_id, .. } => pair_id,
            _ => unreachable!(),
        };
        book.bind_by_leg("s", "account-a", "t1", "k1");
        // 原腿死亡（maker 被 WouldCross 拒）。
        book.on_order_event("k1", Decimal::ZERO, true);
        // 追单续段 key 登记；其成交归一回声明腿（死亡事实不改 filled 累计 ——
        // 成交事实优先，partial_survives_a_dead_sibling 同语义）。
        book.bind_continuation("s", "account-a", "t1", "k1:escalated");
        book.on_order_event("k1:escalated", dec!(10), false);
        let p = book.pairs(&pid).unwrap();
        assert_eq!(
            p.legs[0].filled_shares,
            dec!(10),
            "the escalated fill booked"
        );
        // 单腿账（第二腿未声明）：一腿有成交 ⇒ Partial；naked 记名空（无
        // 未成交的同胞腿）。
        assert_eq!(
            p.status,
            PairStatus::Partial {
                naked_token: String::new()
            }
        );
        // 未登记的续段 key 是空操作（非配对订单流不过账）。
        book.on_order_event("unrelated:escalated", dec!(5), false);
        assert_eq!(book.pairs(&pid).unwrap().legs[0].filled_shares, dec!(10));
        // 同 key 重复登记幂等（首登记的映射保持）。
        book.bind_continuation("s", "account-a", "t1", "k1:escalated");
        book.on_order_event("k1:escalated", dec!(10), false);
        assert_eq!(book.pairs(&pid).unwrap().legs[0].filled_shares, dec!(20));
    }

    /// retire_before：slot 前进 ⇒ 旧轮配对闭账，终态吸收后续事件。
    #[test]
    fn retire_before_closes_old_round_pairs() {
        let mut book = PairBook::default();
        let pid = match book.declare("s", "acc", "k", &decl("t1", dec!(1)), &facts("BTC", 20), 0) {
            DeclareOutcome::Attached { pair_id, .. } => pair_id,
            _ => unreachable!(),
        };
        book.declare("s", "acc", "k", &decl("t2", dec!(1)), &facts("BTC", 20), 0);
        book.bind_by_leg("s", "account-a", "t1", "k1");
        book.retire_before(21);
        assert_eq!(book.pairs(&pid).unwrap().status, PairStatus::Closed);
        // 终态吸收：迟到的成交事实不改状态。
        book.on_order_event("k1", dec!(1), false);
        assert_eq!(book.pairs(&pid).unwrap().status, PairStatus::Closed);
    }

    /// views_for：owner 过滤（跨策略/跨账户不可见）。腿账户 = stamp 结果
    /// （decl 夹具固定 account-a），白名单按腿账户判。
    #[test]
    fn views_for_filters_by_owner_and_accounts() {
        let mut book = PairBook::default();
        book.declare(
            "s1",
            "acc-a",
            "k",
            &decl("t1", dec!(1)),
            &facts("BTC", 1),
            0,
        );
        book.declare(
            "s2",
            "acc-a",
            "k",
            &decl("t2", dec!(1)),
            &facts("BTC", 1),
            0,
        );

        let mine = book.views_for("s1", &["account-a".to_string()].into_iter().collect());
        assert_eq!(mine.len(), 1, "s1 sees only its own pair");
        assert_eq!(mine[0].legs[0].token, "t1");
        assert_eq!(mine[0].status, "pending");

        let none = book.views_for("s3", &["account-a".to_string()].into_iter().collect());
        assert!(none.is_empty(), "another strategy sees nothing");
    }

    /// A≠B：两腿 stamp 到不同账户 ⇒ 一条配对、两条腿各记各的账户；
    /// 白名单任一账户都看到这条（owner 过滤是「任一腿账户在名单内」）。
    #[test]
    fn split_leg_accounts_stay_one_pair_and_both_accounts_see_it() {
        let mut book = PairBook::default();
        let up = PairLegDecl {
            token_id: "t-up".into(),
            shares: dec!(1),
            leg: "up".into(),
            leg_account: "acc-a".into(),
        };
        let down = PairLegDecl {
            token_id: "t-down".into(),
            shares: dec!(1),
            leg: "down".into(),
            leg_account: "acc-b".into(),
        };
        book.declare("s", "owner", "BTC:1", &up, &facts("BTC", 1), 0);
        book.declare("s", "owner", "BTC:1", &down, &facts("BTC", 1), 0);

        for acct in ["acc-a", "acc-b"] {
            let v = book.views_for("s", &[acct.to_string()].into_iter().collect());
            assert_eq!(v.len(), 1, "{acct} sees the pair");
            assert_eq!(v[0].legs.len(), 2, "both legs on one pair");
        }
        let a_view = book.views_for("s", &["acc-a".to_string()].into_iter().collect());
        assert_eq!(a_view[0].legs[0].account_id, "acc-a");
        assert_eq!(a_view[0].legs[1].account_id, "acc-b");
    }

    /// token 派生不出经济身份（slot 0）⇒ 拒收。
    #[test]
    fn an_unmapped_token_is_refused() {
        let mut book = PairBook::default();
        let o = book.declare("s", "acc", "k", &decl("t", dec!(1)), &facts("BTC", 0), 0);
        assert!(matches!(o, DeclareOutcome::Full { .. }));
        assert!(book.all().is_empty());
    }

    /// slot 推进后同一 (asset, 新 slot) 是新配对（旧轮已 Closed ⇒ 复用判定
    /// 放行新建）。
    #[test]
    fn a_new_round_opens_a_fresh_pair() {
        let mut book = PairBook::default();
        book.declare("s", "acc", "k", &decl("t1", dec!(1)), &facts("BTC", 30), 0);
        book.retire_before(31);
        let o = book.declare("s", "acc", "k", &decl("t1", dec!(1)), &facts("BTC", 31), 5);
        match o {
            DeclareOutcome::Attached { new_pair: true, .. } => {}
            other => panic!("the new round opens a fresh pair, got {other:?}"),
        }
        assert_eq!(book.all().len(), 2);
    }
}
