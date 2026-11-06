// #780 — the value-fetching macros moved out of `boot.rs` verbatim so
// `parse_args` can live in `args.rs` without dragging them along. Declared
// with `#[macro_use]` from `mod.rs`, so every child module sees them.
/// Fetch the next argv token as the value of `flag`, refusing a missing value.
macro_rules! next_value {
    ($it:expr, $flag:literal) => {
        match $it.next() {
            Some(v) => v,
            None => bad_value($flag, "", "requires a value (see --help)"),
        }
    };
}

/// Parse the next token as `T`, refusing a non-number.
macro_rules! num_value {
    ($it:expr, $flag:literal, $t:ty, $want:literal) => {{
        let raw = next_value!($it, $flag);
        match raw.trim().parse::<$t>() {
            Ok(v) => v,
            Err(_) => bad_value($flag, &raw, $want),
        }
    }};
}

/// Parse the next token as a Decimal, refusing a non-decimal.
macro_rules! dec_value {
    ($it:expr, $flag:literal, $want:literal) => {{
        let raw = next_value!($it, $flag);
        match rust_decimal::Decimal::from_str(raw.trim()) {
            Ok(v) => v,
            Err(_) => bad_value($flag, raw.trim(), $want),
        }
    }};
}

/// Parse the next token as an explicit boolean (`true|1|on`, `false|0|off`),
/// refusing anything else.
macro_rules! bool_value {
    ($it:expr, $flag:literal) => {{
        let raw = next_value!($it, $flag);
        match raw.as_str() {
            "true" | "1" | "on" => true,
            "false" | "0" | "off" => false,
            _ => bad_value($flag, &raw, "want true|false (1|on / 0|off accepted)"),
        }
    }};
}

/// Fetch the next argv token as a path-like string value, refusing a missing
/// one. The `.filter(non-empty)` arms keep their empty-is-off semantics — an
/// explicit empty string is a documented spelling for "off" on those flags —
/// but a MISSING token is a dangling flag, not an off switch.
macro_rules! path_value {
    ($it:expr, $flag:literal) => {
        next_value!($it, $flag)
    };
}
