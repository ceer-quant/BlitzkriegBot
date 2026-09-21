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
 * flipping `PINNED_DEFAULT_MODEL` is the whole migration, and the kernel side is
 * the `FeeSchedule` registry in `core/blitzkrieg_core/src/exit_policy.rs` (which
 * one schedule is the default is decided there, by `fee_schedule()`).
 */

/**
 * Every model this repository knows how to describe.
 *
 * `source` is REQUIRED per model and is not decoration: the whole reason #203
 * exists is that a fee parameter was in the tree without anyone being able to
 * say who published it, while it moved the strategy's expectancy across zero.
 * A model whose provenance is "somebody's default" says so.
 */
export const TAKER_FEE_MODELS = Object.freeze({
  // fee_per_share = rate * (p*(1-p))^exponent, in USD per share.
  //
  // PROVENANCE — the deployment line's curve, and NOT a published one. No
  // primary source states `0.125*(p(1-p))^2`; it is the arithmetic the kernel has
  // charged since the fee was introduced, and every trade log on disk was
  // produced under it. #203 is the finding that it is 2.3x-5.3x CHEAPER than the
  // published crypto schedule across the prices actually traded, so it is a
  // legacy discount, not "the fee". Using it as the cost basis stays safe only
  // while the divergence is named — which is what this comment does.
  legacy_quadratic: Object.freeze({
    rate: 0.125,
    exponent: 2,
    source: 'history only: unchanged since the fee was first charged; no publication states this curve (#203)',
  }),
  // PROVENANCE — primary source, quoted: Polymarket's published fee formula is
  // `fee = C x feeRate x p x (1-p)`, `C` = shares, and the per-category taker
  // `feeRate` for Crypto is `0.07` (Sports 0.05; Finance/Politics/Mentions/Tech
  // 0.04; Economics/Culture/Weather/Other/General 0.05; Geopolitics 0). The maker
  // side is charged `0`. Fees round to 5 decimal places, minimum 0.00001 USDC.
  // Read from https://docs.polymarket.com/trading/fees on 2026-09-21.
  //
  // What that source does NOT establish, and what #203 leaves open: that THIS
  // strategy actually trades a Crypto-category market (0.07 is the crypto rate;
  // another category is another number), and that the venue's live charge matches
  // the published formula. It establishes the formula and the rate, nothing more.
  official: Object.freeze({
    rate: 0.07,
    exponent: 1,
    source: 'Polymarket docs, fees: fee = C x feeRate x p x (1-p); Crypto feeRate = 0.07, maker 0 (fetched 2026-09-21)',
  }),
});

/** Every model name, in the order a report should present them. */
export const TAKER_FEE_MODEL_NAMES = Object.freeze(Object.keys(TAKER_FEE_MODELS));

/** The model a replay is asked to charge, as the kernel's `--fee-model` takes it. */
export const FEE_MODEL_FLAG = '--fee-model';

/**
 * The model the kernel is REQUIRED to report. Change this in the same change
 * that changes the kernel's default — never on its own: the point of the pin is
 * that a kernel-side default flip without a gate-side decision fails the gates.
 */
export const PINNED_DEFAULT_MODEL = 'legacy_quadratic';

/**
 * Where the kernel declares the model and charges it. ONE place since #203: the
 * `FeeSchedule` in `exit_policy.rs` is what `core.feeQuote` reports and what
 * `taker_fee_pct` charges, and its `source` field carries the provenance the
 * table above mirrors. `--fee-model` (replay-only) selects a different one.
 */
export const FEE_MODEL_SOURCE =
  'core/blitzkrieg_core/src/exit_policy.rs (FeeSchedule / taker_fee_pct; reported by service.rs fee_quote)';

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
 * Check the model table ITSELF (not a kernel quote). Returns a list of problems.
 *
 * The table is where a fee parameter is allowed to live, so it is where the
 * provenance requirement is enforced: a model with no `source` is a number
 * nobody can check, which is precisely how `0.07` arrived (#203). Every model
 * also has to price a share, so a typo'd exponent cannot sit in the table
 * unnoticed by the arithmetic that consumes it.
 */
export function feeModelTableProblems(table = TAKER_FEE_MODELS) {
  const problems = [];
  const names = Object.keys(table);
  if (names.length === 0) problems.push('fee model table: it describes no models at all');
  for (const name of names) {
    const m = table[name] ?? {};
    if (typeof m.source !== 'string' || m.source.trim().length < 12) {
      problems.push(
        `fee model table: '${name}' has no usable source. Say who published its parameters ` +
        `(a URL, a doc quote, or "no publication states this" for a legacy curve) — #203.`,
      );
    }
    if (!Number.isFinite(Number(m.rate)) || Number(m.rate) <= 0) {
      problems.push(`fee model table: '${name}' has a non-positive rate (${m.rate})`);
    }
    if (!Number.isInteger(Number(m.exponent)) || Number(m.exponent) < 1) {
      problems.push(`fee model table: '${name}' has a non-positive exponent (${m.exponent})`);
    }
    // The table's job is to predict what the kernel charges, so it has to be
    // able to: a model that cannot price a share is unusable to every gate.
    const perShare = Number(m.rate) * (0.4 * 0.6) ** Number(m.exponent);
    if (!Number.isFinite(perShare) || perShare <= 0) {
      problems.push(`fee model table: '${name}' prices no fee at p=0.40 (${perShare})`);
    }
  }
  if (table[PINNED_DEFAULT_MODEL] === undefined) {
    problems.push(
      `fee model table: the pinned default '${PINNED_DEFAULT_MODEL}' is not described here`,
    );
  }
  return problems;
}

/**
 * Taker fee per share in USD under a model, at `price` — the same expression the
 * kernel charges (`exit_policy::taker_fee_pct` / `declared_fee_per_share`), used
 * to reason about a schedule the kernel is not currently running.
 */
export function feePerShareAt(modelName, price) {
  const m = TAKER_FEE_MODELS[modelName];
  if (m === undefined) throw new Error(`unknown fee model: ${modelName}`);
  return m.rate * (price * (1 - price)) ** m.exponent;
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
