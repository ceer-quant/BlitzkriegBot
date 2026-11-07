/**
 * The #962 coin — ONE implementation of the flip, shared by the Lua strategy
 * (user_layer/strategies_lua/coin_flip_exit_lab/strategy.lua, the authority)
 * and the lab script's cross-check (this file, in JS/BigInt).
 *
 * FNV-1a-64 over the UTF-8 bytes of `seed\0slot\0conditionId`, then the
 * splitmix64 finalizer, then mod 2. Lua's `>>` is a LOGICAL shift on the
 * 64-bit pattern, so the JS mirrors mask to unsigned before every shift.
 * Pure: no clock, no RNG state, no call-order dependence — the same inputs
 * produce the same side forever.
 */

const MASK64 = (1n << 64n) - 1n;
const FNV_OFFSET = 0xcbf29ce484222325n;
const FNV_PRIME = 0x100000001b3n;

/** Lua integers are signed 64-bit two's complement; keep the bit pattern. */
function asI64(h) {
  return BigInt.asIntN(64, h);
}

export function coinFlipJs(seed, slot, condition) {
  const key = `${seed}\0${slot}\0${condition}`;
  let h = FNV_OFFSET;
  for (const byte of Buffer.from(key, 'utf8')) {
    h ^= BigInt(byte);
    h = asI64(h * FNV_PRIME) & MASK64;
  }
  h ^= (h & MASK64) >> 30n;
  h = asI64(h * 0xbf58476d1ce4e5b9n) & MASK64;
  h ^= (h & MASK64) >> 27n;
  h = asI64(h * 0x94d049bb133111ebn) & MASK64;
  h ^= (h & MASK64) >> 31n;
  return asI64(h) % 2n === 0n ? 'up' : 'down';
}
