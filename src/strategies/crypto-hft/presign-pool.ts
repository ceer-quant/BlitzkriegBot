/**
 * Pre-sign Pool — Pre-compute static data and cache market metadata
 * for fast order submission.
 *
 * Optimization strategy:
 * 1. Precompute domain separator and address derivation (once at startup)
 * 2. Pre-fetch and cache tickSize, negRisk, feeRate for known token IDs
 * 3. Provide fast path for order submission (skip async validation)
 */

import { logger } from '../../utils/logger.js';
import { keccak_256 } from '@noble/hashes/sha3';
import { secp256k1 } from '@noble/curves/secp256k1';

// ============================================================================
// STATIC CONSTANTS (Polymarket CLOB)
// ============================================================================

const PROTOCOL_NAME = 'Polymarket CTF Exchange';
const PROTOCOL_VERSION = '1';
const CHAIN_ID = 137; // Polygon

const CTF_EXCHANGE = '0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E';
const NEG_RISK_CTF_EXCHANGE = '0xC5d563A36AE78145C45a50134d48A1215220f80a';
const OPERATOR_ADDRESS = '0x0000000000000000000000000000000000000000';

const ORDER_TYPE_STRING = 'Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)';

// ============================================================================
// PRECOMPUTED STATIC DATA
// ============================================================================

interface PrecomputedData {
  domainSeparatorCTF: string;
  domainSeparatorNegRisk: string;
  orderTypeHash: string;
  signerAddress: string;
  makerAddress: string;
  signatureType: number;
}

let precomputed: PrecomputedData | null = null;

function encodeUint256(value: string | number | bigint): string {
  return BigInt(value).toString(16).padStart(64, '0');
}

function encodeAddress(address: string): string {
  return address.slice(2).toLowerCase().padStart(64, '0');
}

function toHex(data: Uint8Array): string {
  return Array.from(data).map(b => b.toString(16).padStart(2, '0')).join('');
}

function hashDomain(contractAddress: string): string {
  const typeHash = Buffer.from(toHex(keccak_256(
    Buffer.from('EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)'),
  )), 'hex');

  const nameHash = Buffer.from(toHex(keccak_256(Buffer.from(PROTOCOL_NAME))), 'hex');
  const versionHash = Buffer.from(toHex(keccak_256(Buffer.from(PROTOCOL_VERSION))), 'hex');
  const chainIdHex = CHAIN_ID.toString(16).padStart(64, '0');
  const contractHex = contractAddress.slice(2).toLowerCase().padStart(64, '0');

  const encoded = Buffer.concat([
    typeHash,
    nameHash,
    versionHash,
    Buffer.from(chainIdHex, 'hex'),
    Buffer.from(contractHex, 'hex'),
  ]);

  return '0x' + toHex(keccak_256(encoded));
}

function deriveAddress(privateKey: string): string {
  const keyHex = privateKey.startsWith('0x') ? privateKey.slice(2) : privateKey;
  const pubKey = secp256k1.getPublicKey(keyHex, false).slice(1);
  const hash = toHex(keccak_256(pubKey));
  return '0x' + hash.slice(-40);
}

/**
 * Initialize precomputed static data. Call once at startup.
 */
export function initPrecomputedData(
  privateKey: string,
  funderAddress?: string,
  signatureType?: number
): void {
  const signerAddress = deriveAddress(privateKey);
  const makerAddress = funderAddress || signerAddress;
  const sigType = signatureType ?? (funderAddress ? 1 : 0); // 1 = POLY_PROXY (Magic Link), 0 = EOA

  precomputed = {
    domainSeparatorCTF: hashDomain(CTF_EXCHANGE),
    domainSeparatorNegRisk: hashDomain(NEG_RISK_CTF_EXCHANGE),
    orderTypeHash: '0x' + toHex(keccak_256(Buffer.from(ORDER_TYPE_STRING))),
    signerAddress,
    makerAddress,
    signatureType: sigType,
  };

  logger.info({
    signerAddress,
    makerAddress,
    signatureType: sigType,
  }, 'Pre-sign pool initialized');
}

export function getPrecomputed(): PrecomputedData {
  if (!precomputed) throw new Error('Pre-sign pool not initialized. Call initPrecomputedData() first.');
  return precomputed;
}

// ============================================================================
// MARKET METADATA CACHE
// ============================================================================

interface MarketMetadata {
  tokenId: string;
  tickSize: number;
  negRisk: boolean;
  feeRateBps: number;
  lastFetched: number;
}

const metadataCache = new Map<string, MarketMetadata>();
const METADATA_TTL = 3600_000; // 1 hour

/**
 * Cache market metadata for fast access.
 */
export function cacheMarketMetadata(
  tokenId: string,
  tickSize: number,
  negRisk: boolean,
  feeRateBps: number
): void {
  metadataCache.set(tokenId, {
    tokenId,
    tickSize,
    negRisk,
    feeRateBps,
    lastFetched: Date.now(),
  });
}

/**
 * Get cached market metadata. Returns null if not cached.
 */
export function getCachedMetadata(tokenId: string): MarketMetadata | null {
  const meta = metadataCache.get(tokenId);
  if (!meta) return null;
  if (Date.now() - meta.lastFetched > METADATA_TTL) {
    metadataCache.delete(tokenId);
    return null;
  }
  return meta;
}

/**
 * Remove expired market metadata.
 */
export function invalidateMetadata(tokenId: string): void {
  metadataCache.delete(tokenId);
}

/**
 * Clear all cached metadata.
 */
export function clearMetadataCache(): void {
  metadataCache.clear();
}

// ============================================================================
// FAST ORDER AMOUNTS CALCULATION
// ============================================================================

/**
 * Calculate maker/taker amounts from price and size.
 * This is a pure calculation, no async operations.
 */
export function getOrderAmounts(
  price: number,
  size: number,
  side: 'buy' | 'sell'
): { makerAmount: string; takerAmount: string } {
  // Polymarket uses 6 decimal places for USDC
  const USDC_DECIMALS = 6;
  const makerAmount = Math.round(price * size * 10 ** USDC_DECIMALS);
  const takerAmount = Math.round(size * 10 ** USDC_DECIMALS);

  if (side === 'buy') {
    return {
      makerAmount: makerAmount.toString(),
      takerAmount: takerAmount.toString(),
    };
  } else {
    return {
      makerAmount: takerAmount.toString(),
      takerAmount: makerAmount.toString(),
    };
  }
}

// ============================================================================
// PRE-SIGNED ORDER BUILDER (FAST PATH)
// ============================================================================

export interface FastOrderParams {
  tokenId: string;
  price: number;
  size: number;
  side: 'buy' | 'sell';
  expiration?: number;
}

/**
 * Build a signed order using precomputed data.
 * This skips async validation calls (tickSize, negRisk, feeRate).
 * 
 * IMPORTANT: The caller must ensure:
 * 1. tokenId metadata is cached (call cacheMarketMetadata first)
 * 2. Price is valid for the token's tick size
 * 3. Order won't cross the spread (if postOnly)
 */
export function buildFastSignedOrder(
  params: FastOrderParams,
  privateKey: string,
  cachedMeta: MarketMetadata
): {
  order: Record<string, unknown>;
  owner: string;
  orderType: string;
  deferExec: boolean;
  postOnly: boolean;
} {
  const data = getPrecomputed();

  // Generate unique salt and nonce
  const salt = generateSalt();
  const nonce = generateNonce();

  // Calculate amounts
  const { makerAmount, takerAmount } = getOrderAmounts(params.price, params.size, params.side);

  // Build order struct for hashing
  const order = {
    salt,
    maker: data.makerAddress,
    signer: data.signerAddress,
    taker: OPERATOR_ADDRESS,
    tokenId: params.tokenId,
    makerAmount,
    takerAmount,
    expiration: (params.expiration || 0).toString(),
    nonce,
    feeRateBps: cachedMeta.feeRateBps.toString(),
    side: params.side === 'buy' ? '0' : '1',
    signatureType: data.signatureType,
  };

  // Get domain separator based on negRisk
  const domainSeparator = cachedMeta.negRisk
    ? data.domainSeparatorNegRisk
    : data.domainSeparatorCTF;

  // Create EIP-712 hash
  const structHash = hashOrder(order);
  const encoded = Buffer.concat([
    Buffer.from([0x19, 0x01]),
    Buffer.from(domainSeparator.slice(2), 'hex'),
    Buffer.from(structHash.slice(2), 'hex'),
  ]);
  const hash = '0x' + toHex(keccak_256(encoded));

  // Sign
  const signature = signHash(hash, privateKey);

  // Return in API format
  return {
    order: {
      salt: parseInt(salt, 10),
      maker: data.makerAddress,
      signer: data.signerAddress,
      taker: OPERATOR_ADDRESS,
      tokenId: params.tokenId,
      makerAmount,
      takerAmount,
      expiration: (params.expiration || 0).toString(),
      nonce,
      feeRateBps: cachedMeta.feeRateBps.toString(),
      side: params.side === 'buy' ? 'BUY' : 'SELL',
      signatureType: data.signatureType,
      signature,
    },
    owner: '', // Caller must set this
    orderType: 'GTC',
    deferExec: false,
    postOnly: true,
  };
}

// ============================================================================
// INTERNAL HELPERS (same as polymarket-order-signer.ts)
// ============================================================================

function hashOrder(order: Record<string, string | number>): string {
  const data = getPrecomputed();
  const typeHash = Buffer.from(data.orderTypeHash.slice(2), 'hex');

  const encoded = Buffer.concat([
    typeHash,
    Buffer.from(encodeUint256(order.salt as string), 'hex'),
    Buffer.from(encodeAddress(order.maker as string), 'hex'),
    Buffer.from(encodeAddress(order.signer as string), 'hex'),
    Buffer.from(encodeAddress(order.taker as string), 'hex'),
    Buffer.from(encodeUint256(order.tokenId as string), 'hex'),
    Buffer.from(encodeUint256(order.makerAmount as string), 'hex'),
    Buffer.from(encodeUint256(order.takerAmount as string), 'hex'),
    Buffer.from(encodeUint256(order.expiration as string), 'hex'),
    Buffer.from(encodeUint256(order.nonce as string), 'hex'),
    Buffer.from(encodeUint256(order.feeRateBps as string), 'hex'),
    Buffer.from(encodeUint256(order.side as string), 'hex'),
    Buffer.from(encodeUint256(order.signatureType as number), 'hex'),
  ]);

  return '0x' + toHex(keccak_256(encoded));
}

function signHash(hash: string, privateKey: string): string {
  const keyBytes = privateKey.startsWith('0x') ? privateKey.slice(2) : privateKey;
  const hashBytes = hexToBytes(hash.startsWith('0x') ? hash.slice(2) : hash);

  const sig = secp256k1.sign(hashBytes, keyBytes);
  const r = sig.r.toString(16).padStart(64, '0');
  const s = sig.s.toString(16).padStart(64, '0');
  const v = sig.recovery + 27;

  return '0x' + r + s + v.toString(16).padStart(2, '0');
}

function hexToBytes(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < hex.length; i += 2) {
    bytes[i / 2] = parseInt(hex.substring(i, i + 2), 16);
  }
  return bytes;
}

let nonceCounter = 0;
let lastNonceTimestamp = 0;

function generateSalt(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  const hex = Array.from(bytes).map(b => b.toString(16).padStart(2, '0')).join('');
  return parseInt(hex.slice(0, 12), 16).toString();
}

function generateNonce(): string {
  const now = Date.now();
  if (now === lastNonceTimestamp) {
    nonceCounter++;
  } else {
    nonceCounter = 0;
    lastNonceTimestamp = now;
  }
  return (now * 1000 + nonceCounter).toString();
}
