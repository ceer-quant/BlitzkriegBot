/**
 * Trade Database — Saves detailed trade records to JSON file
 */

import { appendFileSync, readFileSync, writeFileSync, existsSync, mkdirSync } from 'fs';
import { join, dirname } from 'path';
import type { ClosedPosition } from './types.js';

const DATA_DIR = join(process.cwd(), 'data', 'trades');
const TRADES_FILE = join(DATA_DIR, 'trades.jsonl');
const SUMMARY_FILE = join(DATA_DIR, 'summary.json');

export interface TradeRecord {
  id: string;
  strategy: string;
  asset: string;
  direction: string;
  tokenId: string;
  conditionId: string;
  entryPrice: number;
  exitPrice: number;
  shares: number;
  costUsd: number;
  grossPnlUsd: number;
  entryFeePct: number;
  exitFeePct: number;
  feesUsd: number;
  netPnlUsd: number;
  netPnlPct: number;
  holdTimeSec: number;
  entryTime: number;
  exitTime: number;
  exitReason: string;
  wasMakerEntry: boolean;
  wasMakerExit: boolean;
  /** Price at entry moment */
  marketPriceAtEntry: number;
  /** Price at exit moment */
  marketPriceAtExit: number;
  /** Peak unrealized PnL % during the hold */
  highPnlPct: number;
  /** Trough unrealized PnL % during the hold */
  lowPnlPct: number;
  /** Active risk-control toggles at trade time (for A/B attribution) */
  settings?: {
    stopPct?: number;
    tightStop?: boolean;
    crashFilter?: boolean;
    crashMinHighAgeSec?: number;
    crashMaxSpotMovePct?: number;
    slippageGuard?: boolean;
    slippageGuardMaxPct?: number;
    propTrail?: boolean;
    propTrailPct?: number;
    trailingMinHighPct?: number;
    minTrailPct?: number;
    exitGraceSec?: number;
    entryFactor?: number;
  };
}

export interface TradeSummary {
  totalTrades: number;
  wins: number;
  losses: number;
  winRate: number;
  totalGrossPnl: number;
  totalFees: number;
  totalNetPnl: number;
  avgHoldTimeSec: number;
  bestTradePnl: number;
  worstTradePnl: number;
  lastUpdated: number;
}

function ensureDir() {
  if (!existsSync(DATA_DIR)) {
    mkdirSync(DATA_DIR, { recursive: true });
  }
}

export function saveTrade(
  pos: ClosedPosition,
  marketPriceAtEntry: number,
  marketPriceAtExit: number,
  settings?: TradeRecord['settings']
) {
  ensureDir();

  const record: TradeRecord = {
    id: pos.id,
    strategy: pos.strategy,
    asset: pos.asset,
    direction: pos.direction,
    tokenId: pos.tokenId,
    conditionId: pos.conditionId,
    entryPrice: pos.entryPrice,
    exitPrice: pos.exitPrice,
    shares: pos.shares,
    costUsd: pos.costUsd,
    grossPnlUsd: pos.netPnlUsd + (pos.entryFeePct / 100 * pos.entryPrice * pos.shares) + (pos.exitFeePct / 100 * pos.exitPrice * pos.shares),
    entryFeePct: pos.entryFeePct,
    exitFeePct: pos.exitFeePct,
    feesUsd: (pos.entryFeePct / 100 * pos.entryPrice * pos.shares) + (pos.exitFeePct / 100 * pos.exitPrice * pos.shares),
    netPnlUsd: pos.netPnlUsd,
    netPnlPct: pos.netPnlPct,
    holdTimeSec: pos.holdTimeSec,
    entryTime: pos.enteredAt,
    exitTime: pos.exitedAt,
    exitReason: pos.exitReason,
    wasMakerEntry: pos.wasMakerEntry,
    wasMakerExit: pos.wasMakerExit,
    marketPriceAtEntry,
    marketPriceAtExit,
    highPnlPct: pos.highPnlPct,
    lowPnlPct: pos.lowPnlPct,
    ...(settings ? { settings } : {}),
  };

  // Append to JSONL file
  appendFileSync(TRADES_FILE, JSON.stringify(record) + '\n');

  // Update summary
  updateSummary(record);
}

function updateSummary(record: TradeRecord) {
  let summary: TradeSummary;
  
  if (existsSync(SUMMARY_FILE)) {
    try {
      summary = JSON.parse(readFileSync(SUMMARY_FILE, 'utf-8'));
    } catch {
      summary = createEmptySummary();
    }
  } else {
    summary = createEmptySummary();
  }

  summary.totalTrades++;
  if (record.netPnlUsd >= 0) summary.wins++;
  else summary.losses++;
  summary.winRate = summary.totalTrades > 0 ? (summary.wins / summary.totalTrades) * 100 : 0;
  summary.totalGrossPnl += record.grossPnlUsd;
  summary.totalFees += record.feesUsd;
  summary.totalNetPnl += record.netPnlUsd;
  summary.avgHoldTimeSec = ((summary.avgHoldTimeSec * (summary.totalTrades - 1)) + record.holdTimeSec) / summary.totalTrades;
  summary.bestTradePnl = Math.max(summary.bestTradePnl, record.netPnlUsd);
  summary.worstTradePnl = Math.min(summary.worstTradePnl, record.netPnlUsd);
  summary.lastUpdated = Date.now();

  writeFileSync(SUMMARY_FILE, JSON.stringify(summary, null, 2));
}

function createEmptySummary(): TradeSummary {
  return {
    totalTrades: 0,
    wins: 0,
    losses: 0,
    winRate: 0,
    totalGrossPnl: 0,
    totalFees: 0,
    totalNetPnl: 0,
    avgHoldTimeSec: 0,
    bestTradePnl: 0,
    worstTradePnl: 0,
    lastUpdated: Date.now(),
  };
}

export function getSummary(): TradeSummary | null {
  if (!existsSync(SUMMARY_FILE)) return null;
  try {
    return JSON.parse(readFileSync(SUMMARY_FILE, 'utf-8'));
  } catch {
    return null;
  }
}

export function getRecentTrades(limit = 20): TradeRecord[] {
  if (!existsSync(TRADES_FILE)) return [];
  try {
    const lines = readFileSync(TRADES_FILE, 'utf-8').split('\n').filter(Boolean);
    return lines.slice(-limit).map(line => JSON.parse(line));
  } catch {
    return [];
  }
}

/** Return every persisted trade (no limit). */
export function getAllTrades(): TradeRecord[] {
  if (!existsSync(TRADES_FILE)) return [];
  try {
    const lines = readFileSync(TRADES_FILE, 'utf-8').split('\n').filter(Boolean);
    return lines.map(line => JSON.parse(line));
  } catch {
    return [];
  }
}
