#!/usr/bin/env bash
#
# .env 单源配置迁移脚本
# 将所有 .toml/.json 配置文件迁移到 .env，备份旧文件到 data/config-backup-<timestamp>/
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CONFIG_DIR="$PROJECT_ROOT/user_layer/configs"
DATA_DIR="$PROJECT_ROOT/data"
ENV_FILE="$PROJECT_ROOT/.env"
BACKUP_DIR="$DATA_DIR/config-backup-$(date +%Y%m%d-%H%M%S)"

echo "=== .env 单源配置迁移 ==="
echo "项目根目录: $PROJECT_ROOT"
echo "备份目录: $BACKUP_DIR"
echo ""

# 创建备份目录
mkdir -p "$BACKUP_DIR"

# 辅助函数：追加到 .env（避免重复）
append_env() {
    local key="$1"
    local value="$2"
    local temp_file=$(mktemp)

    # 如果 .env 不存在，创建它
    if [ ! -f "$ENV_FILE" ]; then
        touch "$ENV_FILE"
    fi

    # 如果键已存在，更新它；否则追加
    if grep -q "^${key}=" "$ENV_FILE"; then
        # 更新现有键
        sed "s/^${key}=.*/${key}=${value}/" "$ENV_FILE" > "$temp_file"
        mv "$temp_file" "$ENV_FILE"
    else
        # 追加新键
        echo "${key}=${value}" >> "$ENV_FILE"
    fi
}

# 辅助函数：解析 TOML 值（简化版，不支持嵌套数组）
parse_toml_value() {
    local file="$1"
    local key="$2"
    # 使用 grep 提取键值对，处理引号
    grep -E "^${key}\\s*=" "$file" | head -1 | sed 's/.*=\s*//' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//' | tr -d '"'
}

# 辅助函数：解析 JSON 值
parse_json_value() {
    local file="$1"
    local key="$2"
    python3 -c "import json, sys; data = json.load(open('$file')); print(data.get('$key', ''))" 2>/dev/null || echo ""
}

# 辅助函数：解析嵌套 JSON 值
parse_json_nested() {
    local file="$1"
    local key_path="$2"
    python3 -c "import json, sys; data = json.load(open('$file')); keys = '$key_path'.split('.'); val = data; [val := val[k] for k in keys if k in val]; print(val if isinstance(val, (str, int, float, bool)) else json.dumps(val))" 2>/dev/null || echo ""
}

# 迁移 accounts.toml
echo "迁移 accounts.toml..."
if [ -f "$CONFIG_DIR/accounts.toml" ]; then
    cp "$CONFIG_DIR/accounts.toml" "$BACKUP_DIR/"

    # display_account
    display_account=$(parse_toml_value "$CONFIG_DIR/accounts.toml" "display_account")
    if [ -n "$display_account" ]; then
        append_env "ACCOUNT_DISPLAY" "$display_account"
    fi

    # default 账户
    default_name=$(parse_toml_value "$CONFIG_DIR/accounts.toml" "accounts.default.name")
    default_venue=$(parse_toml_value "$CONFIG_DIR/accounts.toml" "accounts.default.venue")
    default_status=$(parse_toml_value "$CONFIG_DIR/accounts.toml" "accounts.default.status")
    default_credential=$(parse_toml_value "$CONFIG_DIR/accounts.toml" "accounts.default.credential_env")

    [ -n "$default_name" ] && append_env "ACCOUNT_DEFAULT_NAME" "$default_name"
    [ -n "$default_venue" ] && append_env "ACCOUNT_DEFAULT_VENUE" "$default_venue"
    [ -n "$default_status" ] && append_env "ACCOUNT_DEFAULT_STATUS" "$default_status"
    [ -n "$default_credential" ] && append_env "ACCOUNT_DEFAULT_CREDENTIAL_ENV" "$default_credential"

    # paper 账户
    paper_name=$(parse_toml_value "$CONFIG_DIR/accounts.toml" "accounts.paper.name")
    paper_venue=$(parse_toml_value "$CONFIG_DIR/accounts.toml" "accounts.paper.venue")
    paper_status=$(parse_toml_value "$CONFIG_DIR/accounts.toml" "accounts.paper.status")
    paper_credential=$(parse_toml_value "$CONFIG_DIR/accounts.toml" "accounts.paper.credential_env")

    [ -n "$paper_name" ] && append_env "ACCOUNT_PAPER_NAME" "$paper_name"
    [ -n "$paper_venue" ] && append_env "ACCOUNT_PAPER_VENUE" "$paper_venue"
    [ -n "$paper_status" ] && append_env "ACCOUNT_PAPER_STATUS" "$paper_status"
    [ -n "$paper_credential" ] && append_env "ACCOUNT_PAPER_CREDENTIAL_ENV" "$paper_credential"

    # kalshi 账户
    kalshi_name=$(parse_toml_value "$CONFIG_DIR/accounts.toml" "accounts.kalshi.name")
    kalshi_venue=$(parse_toml_value "$CONFIG_DIR/accounts.toml" "accounts.kalshi.venue")
    kalshi_status=$(parse_toml_value "$CONFIG_DIR/accounts.toml" "accounts.kalshi.status")
    kalshi_credential=$(parse_toml_value "$CONFIG_DIR/accounts.toml" "accounts.kalshi.credential_env")

    [ -n "$kalshi_name" ] && append_env "ACCOUNT_KALSHI_NAME" "$kalshi_name"
    [ -n "$kalshi_venue" ] && append_env "ACCOUNT_KALSHI_VENUE" "$kalshi_venue"
    [ -n "$kalshi_status" ] && append_env "ACCOUNT_KALSHI_STATUS" "$kalshi_status"
    [ -n "$kalshi_credential" ] && append_env "ACCOUNT_KALSHI_CREDENTIAL_ENV" "$kalshi_credential"

    echo "  ✓ accounts.toml 已迁移"
else
    echo "  ⚠ accounts.toml 不存在，跳过"
fi

# 迁移 default.toml
echo "迁移 default.toml..."
if [ -f "$CONFIG_DIR/default.toml" ]; then
    cp "$CONFIG_DIR/default.toml" "$BACKUP_DIR/"

    # default.toml 目前只有注释，没有实际值
    # 如果有实际值，可以在这里添加 ENGINE_* 前缀的迁移
    echo "  ✓ default.toml 已备份（无实际值需迁移）"
else
    echo "  ⚠ default.toml 不存在，跳过"
fi

# 迁移 execution_policy.toml
echo "迁移 execution_policy.toml..."
if [ -f "$CONFIG_DIR/execution_policy.toml" ]; then
    cp "$CONFIG_DIR/execution_policy.toml" "$BACKUP_DIR/"

    version=$(parse_toml_value "$CONFIG_DIR/execution_policy.toml" "version")
    [ -n "$version" ] && append_env "POLICY_VERSION" "$version"

    # defaults section
    budget_ratio=$(parse_toml_value "$CONFIG_DIR/execution_policy.toml" "defaults.budget_ratio")
    min_budget=$(parse_toml_value "$CONFIG_DIR/execution_policy.toml" "defaults.min_budget_usd")
    max_budget=$(parse_toml_value "$CONFIG_DIR/execution_policy.toml" "defaults.max_budget_usd")
    min_equity=$(parse_toml_value "$CONFIG_DIR/execution_policy.toml" "defaults.min_equity_usd")
    max_positions=$(parse_toml_value "$CONFIG_DIR/execution_policy.toml" "defaults.max_positions_per_asset")

    [ -n "$budget_ratio" ] && append_env "POLICY_DEFAULT_BUDGET_RATIO" "$budget_ratio"
    [ -n "$min_budget" ] && append_env "POLICY_DEFAULT_MIN_BUDGET_USD" "$min_budget"
    [ -n "$max_budget" ] && append_env "POLICY_DEFAULT_MAX_BUDGET_USD" "$max_budget"
    [ -n "$min_equity" ] && append_env "POLICY_DEFAULT_MIN_EQUITY_USD" "$min_equity"
    [ -n "$max_positions" ] && append_env "POLICY_DEFAULT_MAX_POSITIONS_PER_ASSET" "$max_positions"

    # rules 数组（简化处理：只迁移第一个规则）
    # 完整实现需要解析 TOML 数组，这里先跳过
    echo "  ✓ execution_policy.toml 已迁移（rules 数组暂跳过）"
else
    echo "  ⚠ execution_policy.toml 不存在，跳过"
fi

# 迁移 shadow_evolution.toml
echo "迁移 shadow_evolution.toml..."
if [ -f "$CONFIG_DIR/shadow_evolution.toml" ]; then
    cp "$CONFIG_DIR/shadow_evolution.toml" "$BACKUP_DIR/"

    enabled=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.enabled")
    eval_window=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.evaluation_window_minutes")
    min_sample=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.min_sample_count")
    min_wr=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.min_win_rate_improvement")
    min_pf=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.min_profit_factor_improvement")
    min_obs=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.min_observation_minutes")
    cooldown=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.cooldown_minutes")
    max_grad=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.max_gradient")
    variant_count=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.variant_count")
    audit_dir=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.audit_dir")
    auto_evolve=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.auto_evolve")
    cycle_min=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.evolution_cycle_minutes")
    deep_dims=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.deep_dims")
    proposal_ttl=$(parse_toml_value "$CONFIG_DIR/shadow_evolution.toml" "shadow_evolution.proposal_ttl_minutes")

    [ -n "$enabled" ] && append_env "SHADOW_ENABLED" "$enabled"
    [ -n "$eval_window" ] && append_env "SHADOW_EVALUATION_WINDOW_MINUTES" "$eval_window"
    [ -n "$min_sample" ] && append_env "SHADOW_MIN_SAMPLE_COUNT" "$min_sample"
    [ -n "$min_wr" ] && append_env "SHADOW_MIN_WIN_RATE_IMPROVEMENT" "$min_wr"
    [ -n "$min_pf" ] && append_env "SHADOW_MIN_PROFIT_FACTOR_IMPROVEMENT" "$min_pf"
    [ -n "$min_obs" ] && append_env "SHADOW_MIN_OBSERVATION_MINUTES" "$min_obs"
    [ -n "$cooldown" ] && append_env "SHADOW_COOLDOWN_MINUTES" "$cooldown"
    [ -n "$max_grad" ] && append_env "SHADOW_MAX_GRADIENT" "$max_grad"
    [ -n "$variant_count" ] && append_env "SHADOW_VARIANT_COUNT" "$variant_count"
    [ -n "$audit_dir" ] && append_env "SHADOW_AUDIT_DIR" "$audit_dir"
    [ -n "$auto_evolve" ] && append_env "SHADOW_AUTO_EVOLVE" "$auto_evolve"
    [ -n "$cycle_min" ] && append_env "SHADOW_EVOLUTION_CYCLE_MINUTES" "$cycle_min"
    [ -n "$deep_dims" ] && append_env "SHADOW_DEEP_DIMS" "$deep_dims"
    [ -n "$proposal_ttl" ] && append_env "SHADOW_PROPOSAL_TTL_MINUTES" "$proposal_ttl"

    echo "  ✓ shadow_evolution.toml 已迁移"
else
    echo "  ⚠ shadow_evolution.toml 不存在，跳过"
fi

# 迁移 data/runtime/config.json
echo "迁移 data/runtime/config.json..."
if [ -f "$DATA_DIR/runtime/config.json" ]; then
    cp "$DATA_DIR/runtime/config.json" "$BACKUP_DIR/"

    round_duration=$(parse_json_nested "$DATA_DIR/runtime/config.json" "roundDurationSec")
    round_actor=$(parse_json_nested "$DATA_DIR/runtime/config.json" "roundDurationActor")
    round_at_ms=$(parse_json_nested "$DATA_DIR/runtime/config.json" "roundDurationAtMs")

    [ -n "$round_duration" ] && append_env "RUNTIME_ROUND_DURATION_SEC" "$round_duration"
    [ -n "$round_actor" ] && append_env "RUNTIME_ROUND_DURATION_ACTOR" "$round_actor"
    [ -n "$round_at_ms" ] && append_env "RUNTIME_ROUND_DURATION_AT_MS" "$round_at_ms"

    echo "  ✓ data/runtime/config.json 已迁移"
else
    echo "  ⚠ data/runtime/config.json 不存在，跳过"
fi

# 迁移 data/strategy-state.json
echo "迁移 data/strategy-state.json..."
if [ -f "$DATA_DIR/strategy-state.json" ]; then
    cp "$DATA_DIR/strategy-state.json" "$BACKUP_DIR/"

    # enabled 数组转为逗号分隔字符串
    enabled_list=$(python3 -c "import json; data = json.load(open('$DATA_DIR/strategy-state.json')); print(','.join(data.get('enabled', [])))" 2>/dev/null || echo "")
    version=$(parse_json_value "$DATA_DIR/strategy-state.json" "version")

    [ -n "$enabled_list" ] && append_env "STRATEGY_ENABLED_LIST" "$enabled_list"
    [ -n "$version" ] && append_env "STRATEGY_STATE_VERSION" "$version"

    echo "  ✓ data/strategy-state.json 已迁移"
else
    echo "  ⚠ data/strategy-state.json 不存在，跳过"
fi

# 迁移 data/update/state.json
echo "迁移 data/update/state.json..."
if [ -f "$DATA_DIR/update/state.json" ]; then
    cp "$DATA_DIR/update/state.json" "$BACKUP_DIR/"

    auto_check=$(parse_json_nested "$DATA_DIR/update/state.json" "autoCheckIntervalSecs")
    auto_update=$(parse_json_nested "$DATA_DIR/update/state.json" "autoUpdate")
    check_enabled=$(parse_json_nested "$DATA_DIR/update/state.json" "checkEnabled")
    last_check=$(parse_json_nested "$DATA_DIR/update/state.json" "lastCheckMs")

    [ -n "$auto_check" ] && append_env "UPDATE_AUTO_CHECK_INTERVAL_SECS" "$auto_check"
    [ -n "$auto_update" ] && append_env "UPDATE_AUTO_UPDATE" "$auto_update"
    [ -n "$check_enabled" ] && append_env "UPDATE_CHECK_ENABLED" "$check_enabled"
    [ -n "$last_check" ] && append_env "UPDATE_LAST_CHECK_MS" "$last_check"

    echo "  ✓ data/update/state.json 已迁移"
else
    echo "  ⚠ data/update/state.json 不存在，跳过"
fi

# 迁移 data/evolution/state.json
echo "迁移 data/evolution/state.json..."
if [ -f "$DATA_DIR/evolution/state.json" ]; then
    cp "$DATA_DIR/evolution/state.json" "$BACKUP_DIR/"

    cycle_seq=$(parse_json_nested "$DATA_DIR/evolution/state.json" "cycleSeq")
    last_cycle=$(parse_json_nested "$DATA_DIR/evolution/state.json" "lastCycleMs")

    [ -n "$cycle_seq" ] && append_env "EVOLUTION_CYCLE_SEQ" "$cycle_seq"
    [ -n "$last_cycle" ] && append_env "EVOLUTION_LAST_CYCLE_MS" "$last_cycle"

    echo "  ✓ data/evolution/state.json 已迁移"
else
    echo "  ⚠ data/evolution/state.json 不存在，跳过"
fi

# 迁移 data/positions/positions.daily-loss.json
echo "迁移 data/positions/positions.daily-loss.json..."
if [ -f "$DATA_DIR/positions/positions.daily-loss.json" ]; then
    cp "$DATA_DIR/positions/positions.daily-loss.json" "$BACKUP_DIR/"

    # 这个文件比较复杂，包含每个账户的详细状态
    # 简化处理：只迁移顶层信息，详细状态保留在文件中
    echo "  ✓ data/positions/positions.daily-loss.json 已备份（详细状态保留原文件）"
else
    echo "  ⚠ data/positions/positions.daily-loss.json 不存在，跳过"
fi

# 删除工作区原文件
echo ""
echo "删除工作区原文件..."
rm -f "$CONFIG_DIR/accounts.toml"
rm -f "$CONFIG_DIR/default.toml"
rm -f "$CONFIG_DIR/execution_policy.toml"
rm -f "$CONFIG_DIR/shadow_evolution.toml"
rm -f "$DATA_DIR/runtime/config.json"
rm -f "$DATA_DIR/strategy-state.json"
rm -f "$DATA_DIR/update/state.json"
rm -f "$DATA_DIR/evolution/state.json"
# positions.daily-loss.json 保留（运行时状态，格式复杂）

echo "  ✓ 已删除 .toml/.json 配置文件"
echo ""
echo "=== 迁移完成 ==="
echo "备份位置: $BACKUP_DIR"
echo "新配置文件: $ENV_FILE"
echo ""
echo "⚠ 注意：polysim 账户配置需要手动恢复（添加 ACCOUNT_POLYSIM_* 键）"
