#![cfg(test)]

// ── #191: the limited hot reload, at the wire ───────────────────────────
//
// The issue is about what a client may change LIVE, so both tests below
// travel the road a panel/CLI does — `handle_line` with the params bytes.
// Between them they pin the three facts that make the feature safe: the
// change lands (with its audit record), the NEXT order is judged by the new
// value, and everything outside the entry limits is refused with nothing
// applied.
