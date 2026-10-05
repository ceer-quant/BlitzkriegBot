/**
 * 执行策略规则构建器的字段词表（Issue 364）——构建器表单与保存前的值
 * 类型折叠共用这一份，免得「字段长什么样」出现两种说法。
 */

export interface PolicyConditionField {
  value: string
  label: string
  unit: string
  numeric: boolean
}

export const CONDITION_FIELDS: readonly PolicyConditionField[] = [
  { value: 'available_balance', label: '可用余额', unit: 'USD', numeric: true },
  { value: 'total_equity', label: '总权益', unit: 'USD', numeric: true },
  { value: 'open_positions', label: '持仓数', unit: '笔', numeric: true },
  { value: 'current_price', label: '当前价', unit: '', numeric: true },
  { value: 'time_left_sec', label: '剩余秒数', unit: 's', numeric: true },
  { value: 'symbol', label: '交易标的', unit: '', numeric: false },
  { value: 'recent_pnl_1h', label: '近1小时盈亏', unit: 'USD', numeric: true },
  { value: 'consecutive_losses', label: '连亏次数', unit: '次', numeric: true },
] as const

export const CONDITION_OPS = ['<', '<=', '>', '>=', '==', '!=', 'in'] as const

export const THEN_ACTIONS = [
  { value: 'skip', label: '跳过本单（不下）' },
  { value: 'budget_ratio', label: '覆盖下注比例' },
  { value: 'min_budget_usd', label: '覆盖最小下注' },
  { value: 'max_budget_usd', label: '覆盖最大下注' },
  { value: 'cooldown_sec', label: '冷静期（秒）' },
] as const

/** 字段元数据：值类型折叠（numeric）与单位展示都从这来。 */
export function conditionFieldMeta(field: string): PolicyConditionField {
  return CONDITION_FIELDS.find((f) => f.value === field) ?? CONDITION_FIELDS[2]
}
