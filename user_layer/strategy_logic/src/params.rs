//! Per-strategy evolvable parameter model (E2-c): knob value bags and the
//! per-strategy aggregate. Wire rule: decimals are JSON **strings**, never
//! floats. Structurally identical to the kernel's `shadow_evolution::knobs`
//! types — the kernel converts on its boundary; a dylib and the kernel read the
//! same JSON with these same rules.

use rust_decimal::Decimal;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::str::FromStr;

/// One knob a strategy declares evolvable: its current value and the admissible
/// domain `[min, max]` the value may never leave.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnobSpec {
    pub name: String,
    #[serde(with = "crate::decimal")]
    pub value: Decimal,
    #[serde(with = "crate::decimal")]
    pub min: Decimal,
    #[serde(with = "crate::decimal")]
    pub max: Decimal,
}

impl KnobSpec {
    pub fn new(name: impl Into<String>, value: Decimal, min: Decimal, max: Decimal) -> Self {
        Self {
            name: name.into(),
            value,
            min,
            max,
        }
    }

    /// Is this value inside the declared domain?
    pub fn contains(&self, v: Decimal) -> bool {
        v >= self.min && v <= self.max
    }

    /// A declaration whose own bounds are incoherent (`min > max`, or a domain
    /// that does not even contain the value in force) is rejected rather than
    /// silently accepted: it would make every proposal a domain violation.
    pub fn is_coherent(&self) -> bool {
        self.min <= self.max && self.contains(self.value)
    }
}

/// One strategy's evolvable parameter values: knob name → value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StrategyParams {
    values: BTreeMap<String, Decimal>,
}

impl StrategyParams {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_knobs(knobs: &[KnobSpec]) -> Self {
        let mut p = Self::new();
        for k in knobs {
            p.set(&k.name, k.value);
        }
        p
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    pub fn len(&self) -> usize {
        self.values.len()
    }
    pub fn get(&self, name: &str) -> Option<Decimal> {
        self.values.get(name).copied()
    }
    /// Set one knob.
    pub fn set(&mut self, name: &str, value: Decimal) {
        self.values.insert(name.to_string(), value);
    }
    pub fn names(&self) -> Vec<&str> {
        self.values.keys().map(|k| k.as_str()).collect()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, Decimal)> {
        self.values.iter().map(|(k, v)| (k.as_str(), *v))
    }

    /// Set one knob. Only DECLARED knobs can be set: an undeclared name is
    /// ignored, so a proposal can never smuggle in a field the strategy did not
    /// open to evolution.
    pub fn set_declared(&mut self, name: &str, value: Decimal, specs: &[KnobSpec]) -> bool {
        if specs.iter().any(|k| k.name == name) {
            self.values.insert(name.to_string(), value);
            true
        } else {
            false
        }
    }

    /// Raw multiplicative scaling of every value. Pure arithmetic: it does NOT
    /// clamp to a domain (the guard does that on the proposal path), so this is
    /// only safe where the caller keeps the factor tiny (tests, stepping).
    pub fn scaled(&self, factor: Decimal) -> Self {
        Self {
            values: self
                .values
                .iter()
                .map(|(k, v)| (k.clone(), *v * factor))
                .collect(),
        }
    }

    /// First value outside its declared domain, as `(knob, value, min, max)`.
    pub fn domain_violation(
        &self,
        specs: &[KnobSpec],
    ) -> Option<(String, Decimal, Decimal, Decimal)> {
        for k in specs {
            if let Some(v) = self.values.get(&k.name)
                && !k.contains(*v)
            {
                return Some((k.name.clone(), *v, k.min, k.max));
            }
        }
        None
    }

    /// Names in `self` that were not declared by this strategy.
    pub fn undeclared(&self, specs: &[KnobSpec]) -> Vec<String> {
        self.values
            .keys()
            .filter(|n| !specs.iter().any(|k| &k.name == *n))
            .cloned()
            .collect()
    }

    /// Clamp every declared knob into its domain (used after a raw step).
    pub fn clamp_to(&self, specs: &[KnobSpec]) -> Self {
        let mut out = self.clone();
        for k in specs {
            if let Some(v) = out.values.get(&k.name).copied() {
                let c = v.max(k.min).min(k.max);
                if c != v {
                    out.values.insert(k.name.clone(), c);
                }
            }
        }
        out
    }
}

/// The payload a strategy returns from its knob self-declaration (ABI
/// `bk_strategy_evolvable_knobs`, and the in-tree `evolvable_knobs()`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnobDeclaration {
    pub knobs: Vec<KnobSpec>,
}

impl KnobDeclaration {
    pub fn new(knobs: Vec<KnobSpec>) -> Self {
        Self { knobs }
    }

    /// Names only — for receipts and stats.
    pub fn names(&self) -> Vec<String> {
        self.knobs.iter().map(|k| k.name.clone()).collect()
    }

    /// The value bag these declarations imply.
    pub fn params(&self) -> StrategyParams {
        StrategyParams::from_knobs(&self.knobs)
    }

    /// Parse a declaration, treating anything malformed as "nothing declared"
    /// (never a panic — a bad library output must not take the kernel down).
    pub fn parse(text: &str) -> Self {
        serde_json::from_str::<KnobDeclaration>(text)
            .map(|d| KnobDeclaration {
                knobs: d.knobs.into_iter().filter(|k| k.is_coherent()).collect(),
            })
            .unwrap_or_default()
    }
}

impl Serialize for StrategyParams {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.values.len()))?;
        for (k, v) in &self.values {
            m.serialize_entry(k, &v.to_string())?;
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for StrategyParams {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = BTreeMap::<String, serde_json::Value>::deserialize(d)?;
        let mut out = Self::new();
        for (k, v) in raw {
            match v {
                serde_json::Value::String(s) => {
                    let dec = dec_from_wire(&s).map_err(D::Error::custom)?;
                    out.values.insert(k, dec);
                }
                serde_json::Value::Number(n) => {
                    let dec = dec_from_wire(&n.to_string()).map_err(D::Error::custom)?;
                    out.values.insert(k, dec);
                }
                other => {
                    return Err(D::Error::custom(format!(
                        "knob {k}: expected decimal string, got {other}"
                    )));
                }
            }
        }
        Ok(out)
    }
}

/// Decode a decimal from the wire (string or number form), exact — no f64 path.
fn dec_from_wire(s: &str) -> Result<Decimal, String> {
    Decimal::from_str(s.trim()).map_err(|e| e.to_string())
}

/// The aggregate swappable object: **per-strategy named parameter sets**.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MutableParams {
    by_strategy: BTreeMap<String, StrategyParams>,
}

impl MutableParams {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_strategy.is_empty()
    }
    pub fn len(&self) -> usize {
        self.by_strategy.len()
    }
    pub fn strategies(&self) -> Vec<&str> {
        self.by_strategy.keys().map(|k| k.as_str()).collect()
    }
    pub fn for_strategy(&self, strategy: &str) -> Option<&StrategyParams> {
        self.by_strategy.get(strategy)
    }
    /// Value of one knob of one strategy (None when either is unknown).
    pub fn get(&self, strategy: &str, knob: &str) -> Option<Decimal> {
        self.by_strategy.get(strategy).and_then(|p| p.get(knob))
    }
    pub fn set_strategy(&mut self, strategy: &str, params: StrategyParams) {
        self.by_strategy.insert(strategy.to_string(), params);
    }
    pub fn remove_strategy(&mut self, strategy: &str) -> Option<StrategyParams> {
        self.by_strategy.remove(strategy)
    }
}
