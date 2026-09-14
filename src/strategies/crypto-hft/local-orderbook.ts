/**
 * Local L2 Order Book — Maintains a local copy of the order book
 * from WebSocket incremental updates for sub-millisecond reads.
 *
 * Data flow:
 *   Poly WS 'book' event → full snapshot → replace local book
 *   Poly WS 'price_change' event → incremental update → merge into local book
 *
 * Reads are synchronous from memory (nanosecond latency).
 */

import { logger } from '../../utils/logger.js';

// ============================================================================
// TYPES
// ============================================================================

export interface L2Level {
  price: number;
  size: number;
}

export interface L2OrderBook {
  tokenId: string;
  bids: L2Level[];  // sorted descending by price
  asks: L2Level[];  // sorted ascending by price
  bestBid: number;
  bestAsk: number;
  midPrice: number;
  spread: number;
  spreadPct: number;
  bidDepth: number;  // total size on bid side
  askDepth: number;  // total size on ask side
  obi: number;       // order book imbalance
  timestamp: number;
  sequence: number;  // for detecting gaps
}

export interface LocalOrderBookManager {
  /** Apply full snapshot from WS 'book' event */
  applySnapshot(tokenId: string, bids: Array<[number, number]>, asks: Array<[number, number]>, sequence?: number): void;
  /** Apply incremental update from WS 'price_change' event */
  applyDelta(tokenId: string, bids: Array<[number, number]>, asks: Array<[number, number]>, sequence?: number): void;
  /** Get local order book (synchronous, nanosecond read) */
  getBook(tokenId: string): L2OrderBook | null;
  /** Check if book is fresh (updated within maxAgeMs) */
  isFresh(tokenId: string, maxAgeMs?: number): boolean;
  /** Get all tracked token IDs */
  getTrackedTokens(): string[];
  /** Remove a token from tracking */
  removeToken(tokenId: string): void;
  /** Clear all books */
  clear(): void;
}

// ============================================================================
// IMPLEMENTATION
// ============================================================================

const DEFAULT_MAX_AGE_MS = 3000;  // 3 seconds

export function createLocalOrderBookManager(): LocalOrderBookManager {
  const books = new Map<string, L2OrderBook>();

  function computeDerived(book: L2OrderBook): void {
    // Best bid/ask
    book.bestBid = book.bids.length > 0 ? book.bids[0].price : 0;
    book.bestAsk = book.asks.length > 0 ? book.asks[0].price : 0;

    // Mid price
    if (book.bestBid > 0 && book.bestAsk > 0) {
      book.midPrice = (book.bestBid + book.bestAsk) / 2;
    } else if (book.bestBid > 0) {
      book.midPrice = book.bestBid;
    } else {
      book.midPrice = book.bestAsk;
    }

    // Spread
    if (book.bestBid > 0 && book.bestAsk > 0) {
      book.spread = book.bestAsk - book.bestBid;
      book.spreadPct = book.midPrice > 0 ? (book.spread / book.midPrice) * 100 : 0;
    } else {
      book.spread = 0;
      book.spreadPct = 0;
    }

    // Depth
    book.bidDepth = book.bids.reduce((sum, l) => sum + l.size, 0);
    book.askDepth = book.asks.reduce((sum, l) => sum + l.size, 0);

    // OBI (Order Book Imbalance)
    const totalDepth = book.bidDepth + book.askDepth;
    book.obi = totalDepth > 0 ? (book.bidDepth - book.askDepth) / totalDepth : 0;
  }

  function applyLevels(
    existing: L2Level[],
    updates: Array<[number, number]>,
    ascending: boolean
  ): L2Level[] {
    const map = new Map(existing.map(l => [l.price, l.size]));

    for (const [price, size] of updates) {
      if (size === 0) {
        map.delete(price);  // Level removed
      } else {
        map.set(price, size);  // Level added or updated
      }
    }

    const result = Array.from(map.entries())
      .map(([price, size]) => ({ price, size }))
      .filter(l => l.size > 0);

    // Sort: bids descending, asks ascending
    result.sort((a, b) => ascending ? a.price - b.price : b.price - a.price);

    return result;
  }

  return {
    applySnapshot(tokenId, bids, asks, sequence = 0) {
      const book: L2OrderBook = {
        tokenId,
        bids: [],
        asks: [],
        bestBid: 0,
        bestAsk: 0,
        midPrice: 0,
        spread: 0,
        spreadPct: 0,
        bidDepth: 0,
        askDepth: 0,
        obi: 0,
        timestamp: Date.now(),
        sequence,
      };

      // Parse and sort bids (descending)
      book.bids = bids
        .map(([price, size]) => ({ price, size }))
        .filter(l => l.size > 0)
        .sort((a, b) => b.price - a.price);

      // Parse and sort asks (ascending)
      book.asks = asks
        .map(([price, size]) => ({ price, size }))
        .filter(l => l.size > 0)
        .sort((a, b) => a.price - b.price);

      computeDerived(book);
      books.set(tokenId, book);
    },

    applyDelta(tokenId, bids, asks, sequence = 0) {
      let book = books.get(tokenId);

      if (!book) {
        // No existing book, create from delta
        book = {
          tokenId,
          bids: [],
          asks: [],
          bestBid: 0,
          bestAsk: 0,
          midPrice: 0,
          spread: 0,
          spreadPct: 0,
          bidDepth: 0,
          askDepth: 0,
          obi: 0,
          timestamp: Date.now(),
          sequence,
        };
        books.set(tokenId, book);
      }

      // Check for sequence gap
      if (sequence > 0 && book.sequence > 0 && sequence <= book.sequence) {
        // Stale or duplicate update, skip
        return;
      }

      // Apply bid updates (descending sort)
      book.bids = applyLevels(book.bids, bids, false);

      // Apply ask updates (ascending sort)
      book.asks = applyLevels(book.asks, asks, true);

      book.timestamp = Date.now();
      book.sequence = sequence;

      computeDerived(book);
    },

    getBook(tokenId) {
      return books.get(tokenId) ?? null;
    },

    isFresh(tokenId, maxAgeMs = DEFAULT_MAX_AGE_MS) {
      const book = books.get(tokenId);
      if (!book) return false;
      return (Date.now() - book.timestamp) <= maxAgeMs;
    },

    getTrackedTokens() {
      return Array.from(books.keys());
    },

    removeToken(tokenId) {
      books.delete(tokenId);
    },

    clear() {
      books.clear();
    },
  };
}
