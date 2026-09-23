//! Decimal (money/price/size) wire helpers.
//!
//! Prices/sizes cross every boundary as decimal STRINGS; the wire decodes both
//! string and number forms exactly (no f64 round-trip) via these serde
//! functions. Shared verbatim with the kernel's `decimal` module so a config or
//! knob value reads identically on both sides.

use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serializer};
use std::str::FromStr;

pub fn deserialize<'de, D>(d: D) -> Result<Decimal, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::String(s) => Decimal::from_str_exact(s.trim()).map_err(Error::custom),
        serde_json::Value::Number(n) => {
            Decimal::from_str_exact(&n.to_string()).map_err(Error::custom)
        }
        other => Err(Error::custom(format!(
            "expected number/string, got {other}"
        ))),
    }
}

pub fn serialize<S>(d: &Decimal, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    // Plain JSON number with finite precision; Node/zod sees a normal value.
    match f64::from_str(&d.to_string()) {
        Ok(f) if f.is_finite() => s.serialize_f64(f),
        _ => s.serialize_str(&d.to_string()),
    }
}
