//! Shared ordering contract for CLI reads and subscribed ready result sets.
use std::cmp::{Ordering, Reverse};

use serde::{Deserialize, Serialize};

use crate::{DispatchQueueRow, IssueRef};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(flotilla_schema::Describe)]
pub enum Priority {
    Urgent,
    High,
    #[default]
    Medium,
    Low,
}

/// Finite nonnegative mission weight with a total order and canonical zero.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
#[derive(flotilla_schema::Describe)]
pub struct MissionWeight(f64);
impl TryFrom<f64> for MissionWeight {
    type Error = String;
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if !value.is_finite() || value < 0.0 {
            return Err("mission Weight must be finite and nonnegative".into());
        }
        Ok(Self(if value == 0.0 { 0.0 } else { value }))
    }
}
impl From<MissionWeight> for f64 {
    fn from(value: MissionWeight) -> Self {
        value.0
    }
}
impl Eq for MissionWeight {}
impl Ord for MissionWeight {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}
impl PartialOrd for MissionWeight {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Default for MissionWeight {
    fn default() -> Self {
        Self(1.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "MissionAttributesRecord")]
#[derive(flotilla_schema::Describe)]
pub struct MissionAttributes {
    pub priority: Priority,
    pub weight: MissionWeight,
}

// N→N+1 shim for #3313. Remove after the generation deploying Priority/Weight.
// Accept and discard retired stored attributes; writers emit only the new shape.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MissionAttributesRecord {
    priority: Priority,
    weight: MissionWeight,
    #[serde(rename = "value")]
    _value: Option<serde::de::IgnoredAny>,
    #[serde(rename = "class_of_service")]
    _class_of_service: Option<serde::de::IgnoredAny>,
    #[serde(rename = "crew_limit")]
    _crew_limit: Option<serde::de::IgnoredAny>,
}
impl From<MissionAttributesRecord> for MissionAttributes {
    fn from(record: MissionAttributesRecord) -> Self {
        let MissionAttributesRecord { priority, weight, _value: _, _class_of_service: _, _crew_limit: _ } = record;
        Self { priority, weight }
    }
}

/// Tracker observations also live in ForgeRead.status.value. Keep their stored
/// decoder strict, with the same one-generation retirement as Project attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "MissionFieldsRecord")]
pub struct MissionFields {
    pub priority: Option<Priority>,
    pub weight: Option<MissionWeight>,
}

// Remove in N+2, one roll after #3313's Priority/Weight observation generation.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MissionFieldsRecord {
    priority: Option<Priority>,
    weight: Option<MissionWeight>,
    #[serde(rename = "value")]
    _value: Option<serde::de::IgnoredAny>,
    #[serde(rename = "class_of_service")]
    _class_of_service: Option<serde::de::IgnoredAny>,
    #[serde(rename = "crew_limit")]
    _crew_limit: Option<serde::de::IgnoredAny>,
}
impl From<MissionFieldsRecord> for MissionFields {
    fn from(record: MissionFieldsRecord) -> Self {
        Self { priority: record.priority, weight: record.weight }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder, flotilla_schema::Describe)]
pub struct DispatchScore {
    pub mission: String,
    pub mission_issue: Option<IssueRef>,
    pub attributes: MissionAttributes,
    pub membership: String,
    #[shape(child)]
    pub attribute_sources: std::collections::BTreeMap<String, String>,
    pub unblock_count: usize,
    pub conflict_penalty: u64,
    /// Project fair-share ranking inputs.
    pub project_share: u32,
    pub project_active_crews: usize,
    pub mission_active_crews: usize,
}

/// Compare prospective crews per positive share without overflowing or
/// rounding distinct finite weights to the same infinite ratio.
fn compare_share(a_live: usize, a_weight: f64, b_live: usize, b_weight: f64) -> Ordering {
    fn parts(weight: f64) -> (u128, i32) {
        let bits = weight.to_bits();
        let exponent = ((bits >> 52) & 0x7ff) as i32;
        let fraction = u128::from(bits & ((1 << 52) - 1));
        if exponent == 0 {
            (fraction, -1074)
        } else {
            ((1 << 52) | fraction, exponent - 1075)
        }
    }
    let (a_mantissa, a_exponent) = parts(a_weight);
    let (b_mantissa, b_exponent) = parts(b_weight);
    // Products use at most 118 bits (64-bit crew count plus 53-bit mantissa).
    let a = (a_live as u128 + 1) * b_mantissa;
    let b = (b_live as u128 + 1) * a_mantissa;
    let a_bits = 128 - a.leading_zeros();
    let b_bits = 128 - b.leading_zeros();
    (a_bits as i32 + b_exponent).cmp(&(b_bits as i32 + a_exponent)).then_with(|| {
        let width = a_bits.max(b_bits);
        (a << (width - a_bits)).cmp(&(b << (width - b_bits)))
    })
}

/// Deterministic live-crew stride: the next crew goes to the smallest
/// prospective crews/share ratio. Empty missions never enter the ready set.
pub fn compare_dispatch_rows(left: &DispatchQueueRow, right: &DispatchQueueRow) -> Ordering {
    let a = left.score.as_ref();
    let b = right.score.as_ref();
    let aw = a.map_or(1.0, |s| f64::from(s.attributes.weight));
    let bw = b.map_or(1.0, |s| f64::from(s.attributes.weight));
    a.map_or(Priority::Medium, |s| s.attributes.priority)
        .cmp(&b.map_or(Priority::Medium, |s| s.attributes.priority))
        // Zero-weight fallback is tier-wide, even across project boundaries.
        .then_with(|| (aw == 0.0).cmp(&(bw == 0.0)))
        .then_with(|| {
            compare_share(
                a.map_or(0, |s| s.project_active_crews),
                a.map_or(1.0, |s| f64::from(s.project_share.max(1))),
                b.map_or(0, |s| s.project_active_crews),
                b.map_or(1.0, |s| f64::from(s.project_share.max(1))),
            )
        })
        .then_with(|| {
            compare_share(
                a.map_or(0, |s| s.mission_active_crews),
                if aw == 0.0 { 1.0 } else { aw },
                b.map_or(0, |s| s.mission_active_crews),
                if bw == 0.0 { 1.0 } else { bw },
            )
        })
        .then_with(|| Reverse(a.map_or(0, |s| s.unblock_count)).cmp(&Reverse(b.map_or(0, |s| s.unblock_count))))
        .then_with(|| left.ready_observed_at.cmp(&right.ready_observed_at))
        .then_with(|| a.map_or(0, |s| s.conflict_penalty).cmp(&b.map_or(0, |s| s.conflict_penalty)))
        .then_with(|| left.key().cmp(&right.key()))
}

#[cfg(test)]
mod tests {
    use super::*;
    // Exhaustive bounded property: counts 0..16 and weights 1..16 cross zero,
    // powers of two and exact ties. Integer cross-products are the oracle.
    // Protocol has no Hegel dependency; exhaustive enumeration covers this domain.
    #[test]
    fn share_comparison_is_rational() {
        for a in 0..16 {
            for b in 0..16 {
                for aw in 1..16_u32 {
                    for bw in 1..16_u32 {
                        assert_eq!(
                            compare_share(a, f64::from(aw), b, f64::from(bw)),
                            ((a as u128 + 1) * u128::from(bw)).cmp(&((b as u128 + 1) * u128::from(aw)))
                        );
                    }
                }
            }
        }
    }

    // Boundary weights and maximal crew counts remain ordered without overflow.
    #[test]
    fn share_comparison_extremes() {
        let tiny = f64::from_bits(1);
        assert_eq!(compare_share(1, tiny, 0, tiny), Ordering::Greater);
        assert_eq!(compare_share(usize::MAX, f64::MAX, usize::MAX, tiny), Ordering::Less);
        assert_eq!(compare_share(usize::MAX, 1.0, usize::MAX, 1.0), Ordering::Equal);
        assert_eq!(compare_share(0, f64::MIN_POSITIVE, 1, f64::MIN_POSITIVE * 2.0), Ordering::Equal);
    }

    // The previous generation stored normalized mission fields inside generic
    // ForgeRead values. Retired fields decode but never appear in new writes.
    #[test]
    fn stored_mission_field_observations_drop_retired_attributes() {
        let old = serde_json::json!({"value":3, "class_of_service":"expedite", "crew_limit":0});
        let fields: MissionFields = serde_json::from_value(old).expect("old observation");
        assert_eq!(fields, MissionFields::default());
        assert_eq!(serde_json::to_value(fields).expect("new observation"), serde_json::json!({"priority":null, "weight":null}));
        assert!(serde_json::from_value::<MissionFields>(serde_json::json!({"unknown":1})).is_err());
    }

    // #3313: finite nonnegative weights round trip; retired attributes decode
    // without being written back, and genuinely unknown fields stay errors.
    #[test]
    fn weights_and_stored_shim() {
        for value in [0.0, -0.0, f64::MIN_POSITIVE, 0.25, 1.0, f64::MAX] {
            let weight = MissionWeight::try_from(value).expect("weight");
            assert_eq!(serde_json::from_value::<MissionWeight>(serde_json::to_value(weight).expect("encode")).expect("decode"), weight);
        }
        for value in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(MissionWeight::try_from(value).is_err());
        }
        let old = serde_json::json!({"value": 10, "class_of_service": "expedite", "crew_limit": 0});
        let decoded: MissionAttributes = serde_json::from_value(old).expect("previous generation");
        assert_eq!(decoded, MissionAttributes::default());
        assert_eq!(serde_json::to_value(decoded).expect("new shape"), serde_json::json!({"priority":"medium", "weight":1.0}));
        assert!(serde_json::from_value::<MissionAttributes>(serde_json::json!({"unknown":1})).is_err());
    }
}
