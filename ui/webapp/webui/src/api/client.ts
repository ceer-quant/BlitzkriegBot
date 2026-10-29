/**
 * Gateway API client for the Blitzkrieg panel — facade.
 *
 * The endpoint surface lives in per-domain sibling modules (core/session,
 * snapshot wire, accounts, orders, risk, strategy, unified, market, rounds,
 * reset, netcheck, version, backtest, execution-policy, blueprint); each owns
 * its wire types next to the calls that speak them. This file exists so the
 * ~37 importers keep their `from '@/api/client'` (or relative) imports
 * untouched: every historical export is re-exported below, and `api` is the
 * same flat object surface it has always been — each domain module contributes
 * its slice and the spread below stitches them back together.
 *
 * Auth: user/password login (`POST /api/login`, set via
 * `BLITZKRIEG_PANEL_USER`/`BLITZKRIEG_PANEL_PASSWORD` on the server) issues a
 * session token kept in localStorage; every /api call carries it as
 * `X-Auth-Token`. (#460: there is no `?token=` hand-off — the query channel is
 * gone server-side because it leaks into logs/history/Referer.)
 */

import {  snapshotApi,
  type RejectionCauses,
  type StrategyStatsRow,
  type EngineStats,
  type MarketType,
  MARKET_TYPE_LABELS,
  marketTypeLabel,
  type MarketPrice,
  type BookLevel,
  type BookSide,
  type AssetBook,
  type Position,
  type TradeRow,
  type TradeSummary,
  type CumulativeTradeSummary,
  type VenueRound,
  type Round,
  type Snapshot,
  type SystemVersion,
  type StageState,
  type PluginRow,
  pluginKindLabel,
  type PluginsDoc,
  type EvolutionMetricsRow,
  type EvolutionState,
  type EvolutionDecisionReason,
  type EvolutionProposalRow,
  type EvolutionStatusRow,
  type EvolutionDoc,
} from './snapshot.ts'
import { auditApi, type CommandDoc, type GateTraceView, type DecisionView, type IntentAuditRecordView, type IntentAuditTailDoc } from './audit.ts'
import { riskApi, type LimitSource, type RiskBound, type AccountRiskLimitsView, type GlobalRiskLimitsView, type RiskExitView, type RiskLimitsDoc, type RiskSetSystemicBody, type SystemicLimitChange, type SystemicLimitUpdateDoc, type RiskSetExitBody, type ExitSetChange, type ExitSetUpdateDoc } from './risk.ts'
import { strategyApi, type TunableViewDoc, type StrategyParamsReadoutDoc, type StrategyParamsSetBody, type StrategyParamChangeDoc, type StrategyParamsSetUpdateDoc } from './strategy.ts'
import { unifiedApi, type UnifiedVenueRow, type UnifiedEventRow, type UnifiedListDoc, type UnifiedCandidateListing, type UnifiedCandidateRow, type UnifiedCandidatesDoc, type UnifiedConfirmDoc, type UnifiedRevokeDoc } from './unified.ts'
import { ordersApi, type OrderRow, type ForceCloseOrderBody, type ForceClosePositionBody, type ForceClosePositionResultDoc } from './orders.ts'
import { roundsApi, type RoundDurationOption, type RoundDurationDoc, type RoundDurationSetBody, type RoundDurationUpdateDoc } from './rounds.ts'
import { resetApi, type ResetPreSummaryDoc, type ResetTradingBody, type ResetTradingResultDoc } from './reset.ts'
import { marketApi, type KlineIntervalWire, KLINE_INTERVALS, type KlineBar, type KlineHistoryDoc, type MarketPluginReadoutDoc, type PluginCredentialRowDoc, type CredentialsStatusDoc, type CredentialsUpdateDoc } from './market.ts'
import { accountsApi, type AccountJournalRowDoc, type AccountJournalDoc, type AccountVenueRowDoc, type AccountVenueDoc, type AccountVenueSetDoc, type ArbConfigSetBody, type ArbConfigDoc, type DisplayAccountSetDoc, type AccountTradingSetDoc, type AccountStrategiesSetDoc, type AccountCreateDoc, type AccountRemoveDoc, type AccountRoundsSetResult } from './accounts.ts'
import { netcheckApi, type NetCheckItem, type NetCheckReport, type NetCheckDoc } from './netcheck.ts'
import { versionApi } from './version.ts'
import { backtestApi, type OnchainPullParams, type BacktestRunParams, type BacktestDataset, type BacktestDatasetListDoc, type BacktestJobIdsDoc, type BacktestJobStatusDoc, type BacktestLadderRow, type BacktestResultDoc, type BacktestExportDoc } from './backtest.ts'
import { executionPolicyApi, type PolicyConditionField, type PolicyOpWire, type PolicyThenAction, type PolicyWhenView, type PolicyThenView, type PolicyRuleView, type ExecutionPolicySectionDoc, type ExecutionPolicyListDoc, type ExecutionPolicyPreviewRow, type ExecutionPolicyPreviewDoc, type ExecutionPolicyAuditLine, type ExecutionPolicyHistoryDoc, type ExecutionPolicySetParams } from './execution-policy.ts'
import { blueprintApi, type BlueprintCompileDoc, type BlueprintSaveBody, type BlueprintSaveDoc, type BlueprintLoadDoc, type BlueprintLoadSourceDoc, type BlueprintSaveSourceBody, type BlueprintSaveSourceDoc } from './blueprint.ts'
import { ApiError, ping, login, logout, getToken, setToken, clearToken, hasToken, LOGIN_AT_KEY, loginAt, request } from './core.ts'
import { probeSession } from './probe.ts'

export type {
  CommandDoc,
  RejectionCauses, StrategyStatsRow, EngineStats, MarketType, MarketPrice,
  BookLevel, BookSide, AssetBook, Position, TradeRow, TradeSummary,
  CumulativeTradeSummary, VenueRound, Round, Snapshot, SystemVersion,
  StageState, PluginRow, PluginsDoc,
  EvolutionMetricsRow, EvolutionState, EvolutionDecisionReason,
  EvolutionProposalRow, EvolutionStatusRow, EvolutionDoc,
  GateTraceView, DecisionView, IntentAuditRecordView, IntentAuditTailDoc,
  LimitSource, RiskBound, AccountRiskLimitsView, GlobalRiskLimitsView,
  RiskExitView, RiskLimitsDoc, RiskSetSystemicBody, SystemicLimitChange,
  SystemicLimitUpdateDoc, RiskSetExitBody, ExitSetChange, ExitSetUpdateDoc,
  TunableViewDoc, StrategyParamsReadoutDoc, StrategyParamsSetBody,
  StrategyParamChangeDoc, StrategyParamsSetUpdateDoc,
  UnifiedVenueRow, UnifiedEventRow, UnifiedListDoc, UnifiedCandidateListing,
  UnifiedCandidateRow, UnifiedCandidatesDoc, UnifiedConfirmDoc, UnifiedRevokeDoc,
  OrderRow, ForceCloseOrderBody, ForceClosePositionBody, ForceClosePositionResultDoc,
  RoundDurationOption, RoundDurationDoc, RoundDurationSetBody, RoundDurationUpdateDoc,
  ResetPreSummaryDoc, ResetTradingBody, ResetTradingResultDoc,
  KlineIntervalWire, KlineBar, KlineHistoryDoc,
  MarketPluginReadoutDoc, PluginCredentialRowDoc, CredentialsStatusDoc, CredentialsUpdateDoc,
  AccountJournalRowDoc, AccountJournalDoc, AccountVenueRowDoc, AccountVenueDoc,
  AccountVenueSetDoc, ArbConfigSetBody, ArbConfigDoc, DisplayAccountSetDoc,
  AccountTradingSetDoc, AccountStrategiesSetDoc, AccountCreateDoc, AccountRemoveDoc,
  AccountRoundsSetResult,
  NetCheckItem, NetCheckReport, NetCheckDoc,
  OnchainPullParams, BacktestRunParams, BacktestDataset, BacktestDatasetListDoc,
  BacktestJobIdsDoc, BacktestJobStatusDoc, BacktestLadderRow, BacktestResultDoc,
  BacktestExportDoc,
  PolicyConditionField, PolicyOpWire, PolicyThenAction, PolicyWhenView,
  PolicyThenView, PolicyRuleView, ExecutionPolicySectionDoc, ExecutionPolicyListDoc,
  ExecutionPolicyPreviewRow, ExecutionPolicyPreviewDoc, ExecutionPolicyAuditLine,
  ExecutionPolicyHistoryDoc, ExecutionPolicySetParams,
  BlueprintCompileDoc, BlueprintSaveBody, BlueprintSaveDoc, BlueprintLoadDoc,
  BlueprintLoadSourceDoc, BlueprintSaveSourceBody, BlueprintSaveSourceDoc,
}

export {
  MARKET_TYPE_LABELS, marketTypeLabel, pluginKindLabel, KLINE_INTERVALS,
  ApiError, ping, login, logout, getToken, setToken, clearToken, hasToken,
  LOGIN_AT_KEY, loginAt, probeSession,
}

/** The flat endpoint surface, reassembled from the per-domain slices. */
export const api = {
  ...snapshotApi,
  ...auditApi,
  ...riskApi,
  ...strategyApi,
  ...unifiedApi,
  ...ordersApi,
  ...roundsApi,
  ...resetApi,
  ...marketApi,
  ...accountsApi,
  ...netcheckApi,
  ...versionApi,
  ...backtestApi,
  ...executionPolicyApi,
  ...blueprintApi,

  /** Dispatch a gateway command verb (`status`/`start`/`stop`/…). */
  command: (cmd: string) =>
    request<CommandDoc>('/command', { method: 'POST', body: cmd }),
}
