/**
 * The kernel's taker-fee schedule, as every fee-carrying gate must use it (#182).
 *
 * Before this module, three gates each carried their own copy of
 * `0.125 * (p*(1-p))^2`. A copy cannot notice the kernel changing its default
 * schedule, and the failure is quiet and one-directional: the arithmetic in the
 * gate keeps passing while the kernel charges something else, so a green gate
 * stops meaning what it says. Since the in-flight fee work introduces a second
 * model (`official`, `0.07 * p * (1-p)`) and changes which one is the default,
 * "the scripts and the kernel agree" has to become something the gates CHECK.
 *
 * Two things are therefore asserted, and both must hold:
 *
 *   1. the fee figures a gate computes come from the kernel's own `core.feeQuote`
 *      (`feePerShare`), never from a formula restated here — so a model switch
 *      keeps the expected numbers RIGHT instead of turning them red for the
 *      wrong reason;
 *   2. the model the kernel REPORTS is the pinned one below. This is the half
 *      that must go red when a default is changed without the gates being
 *      updated in the same change — deliberately, as the acceptance for #182.
 *
 * Keeping the pin here rather than in each script means the change lands once:
 * flipping `PINNED_DEFAULT_MODEL` (plus the kernel's three constants in
 * `core/blitzkrieg_core/src/service.rs`) is the whole migration.
 */

/** Every model this repository knows how to describe. */
export const TAKER_FEE_MODELS = Object.freeze({
  // fee_per_share = rate * (p*(1-p))^exponent. The schedule the deployment line
  // charges today; superseded by `official` in the in-flight fee work (#182).
  legacy_quadratic: Object.freeze({ rate: 0.125, exponent: 2 }),
  // Polymarket's published schedule: `0.07 * p * (1-p)`, i.e. exponent 1.
  official: Object.freeze({ rate: 0.07, exponent: 1 }),
});

/**
 * The model the kernel is REQUIRED to report. Change this in the same change
 * that changes the kernel's default — never on its own: the point of the pin is
 * that a kernel-side default flip without a gate-side decision fails the gates.
 */
export const PINNED_DEFAULT_MODEL = 'legacy_quadratic';

/** Where the kernel declares the model (for whoever has to update both sides). */
export const FEE_MODEL_SOURCE = 'core/blitzkrieg_core/src/service.rs (TAKER_FEE_MODEL / TAKER_FEE_RATE / TAKER_FEE_EXPONENT)';

const n = (v) => Number(v);

/** Human-readable one-liner for a quote, for gate logs. */
export function describeQuote(quote) {
  return `model=${quote.model} rate=${quote.rate} exponent=${quote.exponent} ` +
    `price=${quote.price} feePerShare=${quote.feePerShare} ` +
    `feePctOfPrice=${quote.feePctOfPrice} modelMatches=${quote.modelMatches}`;
}

/**
 * Check one quote from the kernel against the pin.
 *
 * Returns a list of problems (empty = the kernel is charging what this
 * repository believes it charges). The three failure modes are kept distinct on
 * purpose: a changed default, a changed parameter, and a kernel whose declared
 * model no longer reproduces the fee it computes are different incidents with
 * different fixes.
 */
export function feeModelProblems(quote) {
  const problems = [];
  const pinned = TAKER_FEE_MODELS[PINNED_DEFAULT_MODEL];
  const known = TAKER_FEE_MODELS[quote.model];

  if (quote.model !== PINNED_DEFAULT_MODEL) {
    problems.push(
      `fee model: kernel reports '${quote.model}' but the pinned default is ` +
      `'${PINNED_DEFAULT_MODEL}'. If the kernel's default changed on purpose, ` +
      `update PINNED_DEFAULT_MODEL in scripts/lib/fee-model.mjs in the SAME change (#182).`,
    );
  }
  if (known === undefined) {
    problems.push(
      `fee model: kernel reports '${quote.model}', which this repository does not ` +
      `describe. Add it to TAKER_FEE_MODELS (scripts/lib/fee-model.mjs) with the ` +
      `parameters it actually charges, and say whether it is the default (#182).`,
    );
  } else if (n(quote.rate) !== known.rate || Number(quote.exponent) !== known.exponent) {
    problems.push(
      `fee model: kernel reports ${quote.model} as rate=${quote.rate} ` +
      `exponent=${quote.exponent}, but it is pinned as rate=${known.rate} ` +
      `exponent=${known.exponent} (#182)`,
    );
  }
  if (!Number.isFinite(n(quote.rate))) {
    problems.push(`fee model: kernel reported a non-numeric rate (${quote.rate})`);
  }
  if (quote.modelMatches !== true) {
    problems.push(
      `fee model: the kernel's declared model (${quote.model} ` +
      `rate=${quote.rate} exponent=${quote.exponent}) does not reproduce the fee it ` +
      `charges at price ${quote.price} (feePerShare=${quote.feePerShare}) — the ` +
      `formula and its declaration have drifted apart (#182)`,
    );
  }
  if (quote.model === PINNED_DEFAULT_MODEL && pinned === undefined) {
    problems.push('fee model: the pinned default has no entry in TAKER_FEE_MODELS');
  }
  return problems;
}

/**
 * A memoised fee quoter bound to one core's `request` function.
 *
 * `await quoter(0.4)` returns the kernel's quote at 0.4; `sharesFee(quote, 5)`
 * turns it into the USD a fill of 5 shares pays. Deriving the expected fee this
 * way — rather than restating a formula — is what keeps a gate numerically
 * correct across a fee-model change while `feeModelProblems` independently
 * decides whether that change was allowed.
 */
export function feeQuoter(request) {
  const cache = new Map();
  return async function quote(price) {
    const key = String(price);
    if (!cache.has(key)) {
      cache.set(key, await request('core.feeQuote', { price }));
    }
    return cache.get(key);
  };
}

/**
 * USD charged on a taker fill of `shares` at `price`, from a quote of that price.
 * Maker fills are free (`makerFeePerShare` is zero by construction), and the
 * kernel charges entry and exit alike, so callers pick the role.
 */
export function feeUsdFor(quote, shares) {
  return n(quote.feePerShare) * shares;
}

/** The model name a quote reports — the field to print in a drift report. */
export const feeModelName = (quote) => `${quote.model}(rate=${quote.rate},exp=${quote.exponent})`;
