//! Decimal (money/price/size) wire helpers.
//!
//! Prices/sizes cross the plugin boundary as JSON numbers (Node) or strings
//! (Polymarket/Rust). Monetary arithmetic uses `rust_decimal::Decimal`; the
//! boundary decodes both forms via these serde functions. Mirrors the core's
//! `src/decimal.rs` so DTOs serialize identically on both sides.

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

pub mod opt {
    use super::*;
    pub fn deserialize<'de, D>(d: D) -> Result<Option<Decimal>, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error;
        let v = Option::<serde_json::Value>::deserialize(d)?;
        match v {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(s)) => Decimal::from_str_exact(s.trim())
                .map(Some)
                .map_err(Error::custom),
            Some(serde_json::Value::Number(n)) => Decimal::from_str_exact(&n.to_string())
                .map(Some)
                .map_err(Error::custom),
            Some(other) => Err(Error::custom(format!(
                "expected number/string, got {other}"
            ))),
        }
    }
    pub fn serialize<S>(d: &Option<Decimal>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match d {
            Some(v) => super::serialize(v, s),
            None => s.serialize_none(),
        }
    }
}
