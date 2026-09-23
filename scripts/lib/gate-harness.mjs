/**
 * The one result collector for the gates in `scripts/`.
 *
 * Twenty gates used to carry their own four-line `check()`: a counter, a
 * `console.log`, and a format. The formats had drifted — `  ok   ` here, `  ok  `
 * there, `PASS` with no indent in three, `  FAIL <name> <detail>` in nine and
 * `FAIL <name> — <detail>` in the rest — so the same event read differently
 * depending on which gate printed it, and a fix to the wording landed in one
 * gate at a time.
 *
 * This is the single place a check line is printed. The format is the one the
 * majority already used, and it is deliberately the ALIGNED one: `ok  ` plus a
 * space is the same width as `FAIL`, so the names line up under each other.
 *
 *     ok   the ladder produced a capacity at the requested impact budget
 *     FAIL the ladder produced a capacity at the requested impact budget — null
 *
 * `detail` is printed only when there is one, on success as well as failure: a
 * gate that explains what it measured is the point of the line.
 *
 * What this does NOT own, on purpose: each gate's final `RESULT:` sentence and
 * its exit code. Those are gate-specific ("21/21 claims", "dry and live ledgers
 * are bit-identical", `process.exit(2)` for a usage error) and are the part an
 * operator reads, so they stay where the meaning is.
 *
 * The returned `check` is a plain closure, not a method: seven gates hand it to
 * `lib/core-provenance.mjs` as a callback (`checkCoreProvenance(BIN, check)`),
 * which is the contract that helper documents.
 */
export function createChecks() {
  const results = [];

  const check = (name, ok, detail = '') => {
    results.push({ name, ok: Boolean(ok), detail });
    console.log(`  ${ok ? 'ok  ' : 'FAIL'} ${name}${detail ? ` — ${detail}` : ''}`);
  };

  return {
    /** Assert one thing. Passable as a callback; safe to destructure. */
    check,
    /** Every result in order, `{ name, ok, detail }` — for gates that summarize the list. */
    results,
    /** How many failed. A getter, so it is live as the gate runs. */
    get failures() {
      return results.filter((r) => !r.ok).length;
    },
  };
}
