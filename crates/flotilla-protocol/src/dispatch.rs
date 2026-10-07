//! Shared ordering contract for CLI reads and subscribed ready result sets.
use std::cmp::{Ordering, Reverse};

use serde::{Deserialize, Serialize};

use crate::{DispatchQueueRow, IssueRef};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassOfService {
    Expedite,
    #[default]
    Standard,
    Background,
}

/// Finite numeric mission value with a total order and canonical zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct MissionValue(f64);
impl TryFrom<f64> for MissionValue {
    type Error = String;
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if !value.is_finite() {
            return Err("mission Value must be finite".into());
        }
        Ok(Self(if value == 0.0 { 0.0 } else { value }))
    }
}
impl From<MissionValue> for f64 {
    fn from(value: MissionValue) -> Self {
        value.0
    }
}
impl Eq for MissionValue {}
impl Ord for MissionValue {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}
impl PartialOrd for MissionValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MissionAttributes {
    pub value: MissionValue,
    pub class_of_service: ClassOfService,
    /// None means unlimited; zero explicitly pauses mission admission.
    pub crew_limit: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionFields {
    pub value: Option<MissionValue>,
    pub class_of_service: Option<ClassOfService>,
    pub crew_limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(default)]
pub struct DispatchScore {
    pub mission: String,
    pub mission_issue: Option<IssueRef>,
    pub attributes: MissionAttributes,
    pub membership: String,
    pub attribute_sources: std::collections::BTreeMap<String, String>,
    pub unblock_count: usize,
    pub conflict_penalty: u64,
    /// Admission inputs, not a work-queue priority term.
    pub project_share: u32,
    pub project_active_crews: usize,
    pub mission_active_crews: usize,
}

impl Default for DispatchScore {
    fn default() -> Self {
        Self {
            mission: "routine".into(),
            mission_issue: None,
            attributes: MissionAttributes::default(),
            membership: "routine".into(),
            attribute_sources: Default::default(),
            unblock_count: 0,
            conflict_penalty: 0,
            project_share: 1,
            project_active_crews: 0,
            mission_active_crews: 0,
        }
    }
}

/// Lexicographic ruling order. Fair share gates admission separately (#2785).
/// Stable identity breaks exact ties without relying on hash iteration order.
pub fn compare_dispatch_rows(left: &DispatchQueueRow, right: &DispatchQueueRow) -> Ordering {
    let terms = |row: &DispatchQueueRow| {
        let score = row.score.as_ref();
        (
            score.map_or(ClassOfService::Standard, |s| s.attributes.class_of_service),
            Reverse(score.map_or(MissionValue::default(), |s| s.attributes.value)),
            Reverse(score.map_or(0, |s| s.unblock_count)),
            row.ready_observed_at,
            score.map_or(0, |s| s.conflict_penalty),
        )
    };
    terms(left).cmp(&terms(right)).then_with(|| left.key().cmp(&right.key()))
}

#[cfg(test)]
mod tests {

    // Stored scores tolerate additions at the container level with neutral
    // ranking/admission defaults, while existing partial authored values survive.
    #[test]
    fn partial_stored_scores_decode_with_neutral_defaults() {
        let score: super::DispatchScore =
            serde_json::from_str(r#"{"mission":"stability","attributes":{"value":5},"unblock_count":7}"#).expect("previous stored score");
        assert_eq!(score.mission, "stability");
        assert_eq!(f64::from(score.attributes.value), 5.0);
        assert_eq!(score.unblock_count, 7);
        assert_eq!(score.attributes.class_of_service, super::ClassOfService::Standard);
        assert_eq!(score.project_share, 1);
        assert_eq!(score.conflict_penalty, 0);
        assert_eq!(score.project_active_crews, 0);
        let empty: super::DispatchScore = serde_json::from_str("{}").expect("empty stored score");
        assert_eq!(empty.mission, "routine");
        assert_eq!(empty.project_share, 1);
    }
    use super::*;

    // Serialization preserves decimal priority values and rejects nonfinite ones.
    #[test]
    fn mission_values_are_finite_and_zero_is_canonical() {
        for value in [-1.5, 0.0, -0.0, 3.25, f64::MAX] {
            let value = MissionValue::try_from(value).expect("finite");
            assert_eq!(serde_json::from_str::<MissionValue>(&serde_json::to_string(&value).expect("encode")).expect("decode"), value);
        }
        assert_eq!(MissionValue::try_from(-0.0).expect("zero").cmp(&MissionValue::default()), Ordering::Equal);
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(MissionValue::try_from(value).is_err());
        }
    }
}
