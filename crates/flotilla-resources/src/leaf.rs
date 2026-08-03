use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use facet::Peek;
#[cfg(test)]
use facet::{Def, Shape, Type, UserType};
use flotilla_protocol::{Leaf, LeafKind, LeafOperator};

use crate::{Convoy, CrewWorkPhase, CrewWorkState, ResourceObject, Vessel, WorkState};

pub const ADMITTED_LEAF_VOCABULARY: &[(&str, &str)] = &[
    ("convoy", ".status.phase"),
    ("vessel", ".status.phase"),
    ("work", ".status.phase"),
    ("work", ".latest-claim.disposition"),
    ("work", ".latest-claim.claimed-at"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreeValue {
    True,
    False,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafValue {
    Text(String),
    Timestamp(DateTime<Utc>),
}

impl std::fmt::Display for LeafValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(value) => f.write_str(value),
            Self::Timestamp(value) => write!(f, "{}", value.to_rfc3339()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafEvaluation {
    pub result: ThreeValue,
    pub value: Option<LeafValue>,
}

/// Typed seam between the expression evaluator and a stored resource shape.
///
/// A descriptor-backed implementation can replace the handwritten subjects
/// without changing admission, comparison, or subscription machinery.
pub trait LeafSubject {
    fn kind(&self) -> LeafKind;
    fn value(&self, field_path: &str) -> Option<LeafValue>;
    fn observed_at(&self) -> Option<DateTime<Utc>> {
        None
    }
}

pub fn admit_leaf(leaf: &Leaf) -> Result<(), String> {
    let kind = leaf.address.kind();
    let admitted =
        ADMITTED_LEAF_VOCABULARY.iter().any(|(candidate_kind, path)| *candidate_kind == kind.to_string() && *path == leaf.field_path);
    if !admitted {
        let vocabulary = ADMITTED_LEAF_VOCABULARY.iter().map(|(kind, path)| format!("{kind}{path}")).collect::<Vec<_>>().join(", ");
        return Err(format!("leaf path `{kind}{}` is not admitted; admitted vocabulary: {vocabulary}", leaf.field_path));
    }
    if leaf.field_path != ".latest-claim.claimed-at" && !matches!(leaf.operator, LeafOperator::Equal | LeafOperator::NotEqual) {
        return Err(format!("operator `{}` is not admitted for text leaf `{kind}{}`; use `==` or `!=`", leaf.operator, leaf.field_path));
    }
    if leaf.field_path == ".latest-claim.claimed-at" {
        leaf.literal
            .parse::<DateTime<Utc>>()
            .map_err(|error| format!("invalid timestamp literal `{}` for work.latest-claim.claimed-at: {error}", leaf.literal))?;
    }
    Ok(())
}

pub fn evaluate_leaf(
    leaf: &Leaf,
    subject: Option<&dyn LeafSubject>,
    freshness_demand: Option<DateTime<Utc>>,
) -> Result<LeafEvaluation, String> {
    admit_leaf(leaf)?;
    let Some(subject) = subject else {
        return Ok(LeafEvaluation { result: ThreeValue::Unknown, value: None });
    };
    if subject.kind() != leaf.address.kind() {
        return Err(format!("leaf addresses {} but subject is {}", leaf.address.kind(), subject.kind()));
    }
    if freshness_demand.is_some_and(|demand| subject.observed_at().is_none_or(|observed_at| observed_at < demand)) {
        return Ok(LeafEvaluation { result: ThreeValue::Unknown, value: None });
    }
    let Some(value) = subject.value(&leaf.field_path) else {
        return Ok(LeafEvaluation { result: ThreeValue::Unknown, value: None });
    };
    let literal = bind_literal(&leaf.field_path, &leaf.literal)?;
    let ordering = compare_values(&value, &literal)?;
    let fired = match leaf.operator {
        LeafOperator::Equal => ordering == Ordering::Equal,
        LeafOperator::NotEqual => ordering != Ordering::Equal,
        LeafOperator::LessThan => ordering == Ordering::Less,
        LeafOperator::LessThanOrEqual => ordering != Ordering::Greater,
        LeafOperator::GreaterThan => ordering == Ordering::Greater,
        LeafOperator::GreaterThanOrEqual => ordering != Ordering::Less,
    };
    Ok(LeafEvaluation { result: if fired { ThreeValue::True } else { ThreeValue::False }, value: Some(value) })
}

fn bind_literal(path: &str, literal: &str) -> Result<LeafValue, String> {
    if path == ".latest-claim.claimed-at" {
        return literal
            .parse::<DateTime<Utc>>()
            .map(LeafValue::Timestamp)
            .map_err(|error| format!("invalid timestamp literal `{literal}`: {error}"));
    }
    Ok(LeafValue::Text(literal.to_string()))
}

fn compare_values(left: &LeafValue, right: &LeafValue) -> Result<Ordering, String> {
    match (left, right) {
        (LeafValue::Text(left), LeafValue::Text(right)) => Ok(left.cmp(right)),
        (LeafValue::Timestamp(left), LeafValue::Timestamp(right)) => Ok(left.cmp(right)),
        _ => Err("leaf value and bound literal have different types".to_string()),
    }
}

pub struct ConvoyLeafSubject<'a>(pub &'a ResourceObject<Convoy>);

impl LeafSubject for ConvoyLeafSubject<'_> {
    fn kind(&self) -> LeafKind {
        LeafKind::Convoy
    }

    fn value(&self, field_path: &str) -> Option<LeafValue> {
        match field_path {
            ".status.phase" => self.0.status.as_ref().map(|status| LeafValue::Text(format!("{:?}", status.phase))),
            _ => None,
        }
    }
}

/// Spike-only descriptor-backed Convoy subject. The handwritten
/// [`ConvoyLeafSubject`] remains the active implementation used by core.
#[allow(dead_code)]
struct FacetConvoyLeafSubject<'a>(&'a ResourceObject<Convoy>);

impl LeafSubject for FacetConvoyLeafSubject<'_> {
    fn kind(&self) -> LeafKind {
        LeafKind::Convoy
    }

    fn value(&self, field_path: &str) -> Option<LeafValue> {
        let value = facet_value_at_path(Peek::new(self.0), field_path)?;
        let variant = value.into_enum().ok()?.active_variant().ok()?;
        Some(LeafValue::Text(variant.rename.unwrap_or(variant.name).to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
enum FacetComparableType {
    Text,
}

#[allow(dead_code)]
fn facet_value_at_path<'mem, 'facet>(mut value: Peek<'mem, 'facet>, field_path: &str) -> Option<Peek<'mem, 'facet>> {
    for segment in field_path.strip_prefix('.')?.split('.') {
        if let Ok(option) = value.into_option() {
            value = option.value()?;
        }
        let structure = value.into_struct().ok()?;
        let index = structure.ty().fields.iter().position(|field| field.rename.unwrap_or(field.name) == segment)?;
        value = structure.field(index).ok()?;
    }
    if let Ok(option) = value.into_option() {
        value = option.value()?;
    }
    Some(value)
}

#[cfg(test)]
fn facet_schema_at_path(mut shape: &'static Shape, field_path: &str) -> Option<FacetComparableType> {
    for segment in field_path.strip_prefix('.')?.split('.') {
        if let Def::Option(option) = shape.def {
            shape = option.t();
        }
        let Type::User(UserType::Struct(structure)) = shape.ty else {
            return None;
        };
        shape = structure.fields.iter().find(|field| field.rename.unwrap_or(field.name) == segment)?.shape();
    }
    if let Def::Option(option) = shape.def {
        shape = option.t();
    }
    match shape.ty {
        Type::User(UserType::Enum(enumeration)) if enumeration.variants.iter().all(|variant| variant.data.fields.is_empty()) => {
            Some(FacetComparableType::Text)
        }
        _ => None,
    }
}

pub struct VesselLeafSubject<'a>(pub &'a ResourceObject<Vessel>);

impl LeafSubject for VesselLeafSubject<'_> {
    fn kind(&self) -> LeafKind {
        LeafKind::Vessel
    }

    fn value(&self, field_path: &str) -> Option<LeafValue> {
        match field_path {
            ".status.phase" => self.0.status.as_ref().map(|status| LeafValue::Text(format!("{:?}", status.phase))),
            _ => None,
        }
    }
}

pub struct WorkLeafSubject<'a> {
    pub work: &'a WorkState,
    pub crew: Option<&'a std::collections::BTreeMap<String, CrewWorkState>>,
}

impl WorkLeafSubject<'_> {
    fn latest_claim(&self) -> Option<&CrewWorkState> {
        self.crew?
            .values()
            .filter(|state| state.phase == CrewWorkPhase::Done)
            .filter(|state| state.finished_at.is_some())
            .max_by_key(|state| state.finished_at)
    }
}

impl LeafSubject for WorkLeafSubject<'_> {
    fn kind(&self) -> LeafKind {
        LeafKind::Work
    }

    fn value(&self, field_path: &str) -> Option<LeafValue> {
        match field_path {
            ".status.phase" => Some(LeafValue::Text(format!("{:?}", self.work.phase))),
            ".latest-claim.disposition" => self.latest_claim()?.disposition.clone().map(LeafValue::Text),
            ".latest-claim.claimed-at" => self.latest_claim()?.finished_at.map(LeafValue::Timestamp),
            _ => None,
        }
    }

    fn observed_at(&self) -> Option<DateTime<Utc>> {
        self.latest_claim()?.finished_at
    }
}

#[cfg(test)]
mod tests {
    use facet::{Facet, Type, UserType};
    use flotilla_protocol::LeafAddress;

    use super::*;

    struct FakeSubject {
        observed_at: Option<DateTime<Utc>>,
    }

    impl LeafSubject for FakeSubject {
        fn kind(&self) -> LeafKind {
            LeafKind::Convoy
        }

        fn value(&self, _field_path: &str) -> Option<LeafValue> {
            Some(LeafValue::Text("Landed".to_string()))
        }

        fn observed_at(&self) -> Option<DateTime<Utc>> {
            self.observed_at
        }
    }

    fn leaf(path: &str) -> Leaf {
        Leaf {
            address: LeafAddress::Convoy { name: "demo".to_string() },
            field_path: path.to_string(),
            operator: LeafOperator::Equal,
            literal: "Landed".to_string(),
        }
    }

    #[test]
    fn absent_and_stale_subjects_are_unknown() {
        assert_eq!(evaluate_leaf(&leaf(".status.phase"), None, None).expect("evaluate").result, ThreeValue::Unknown);
        let demand = "2026-08-03T20:00:00Z".parse().expect("timestamp");
        let stale = FakeSubject { observed_at: Some("2026-08-03T19:00:00Z".parse().expect("timestamp")) };
        assert_eq!(evaluate_leaf(&leaf(".status.phase"), Some(&stale), Some(demand)).expect("evaluate").result, ThreeValue::Unknown);
    }

    #[test]
    fn unknown_path_names_the_closed_vocabulary() {
        let error = admit_leaf(&leaf(".status.nope")).expect_err("path should be rejected");
        assert!(error.contains("admitted vocabulary"));
        assert!(error.contains("convoy.status.phase"));
        assert!(error.contains("work.latest-claim.disposition"));
    }

    #[test]
    fn text_leaf_rejects_order_operators_at_admission() {
        let mut leaf = leaf(".status.phase");
        leaf.operator = LeafOperator::GreaterThan;
        let error = admit_leaf(&leaf).expect_err("text ordering should be rejected");
        assert!(error.contains("operator `>` is not admitted"));
        assert!(error.contains("use `==` or `!=`"));
    }

    #[test]
    fn facet_convoy_subject_matches_the_active_handwritten_subject() {
        let timestamp = "2026-08-03T20:00:00Z".parse().expect("timestamp");
        let object = ResourceObject {
            metadata: crate::ObjectMeta {
                name: "demo".to_string(),
                namespace: "flotilla".to_string(),
                resource_version: "1".to_string(),
                labels: Default::default(),
                annotations: Default::default(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: timestamp,
                merge: None,
            },
            spec: crate::ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
            status: Some(crate::ConvoyStatus { phase: crate::ConvoyPhase::Landing, ..Default::default() }),
        };

        let handwritten = ConvoyLeafSubject(&object);
        let descriptor_backed = FacetConvoyLeafSubject(&object);
        assert_eq!(descriptor_backed.value(".status.phase"), handwritten.value(".status.phase"));
        assert_eq!(descriptor_backed.value(".status.nope"), None);
    }

    #[test]
    fn facet_shape_validates_a_comparable_path_without_a_value() {
        let shape = <ResourceObject<Convoy> as Facet>::SHAPE;
        assert_eq!(facet_schema_at_path(shape, ".status.phase"), Some(FacetComparableType::Text));
        assert_eq!(facet_schema_at_path(shape, ".status.nope"), None);
        assert_eq!(facet_schema_at_path(shape, ".spec.workflow_ref"), None);
    }

    #[test]
    fn facet_attributes_match_serde_names_and_shapes() {
        assert_struct_field_rename::<crate::ConvoySpec>("ref", "ref");
        assert_struct_field_rename::<crate::ObjectMeta>("owner_references", "ownerReferences");
        assert_struct_field_rename::<crate::OwnerReference>("api_version", "apiVersion");
        assert_struct_field_rename::<flotilla_protocol::PlacementTargetHost>("reference", "ref");
        assert_enum_variant_rename::<flotilla_protocol::IssueState>("Open", "open");
        assert_enum_variant_rename::<crate::Stance>("WorkspaceWrite", "workspace-write");

        let spec = crate::ConvoySpec::builder().workflow_ref("workflow".to_string()).r#ref("branch".to_string()).build();
        let spec_json = serde_json::to_value(spec).expect("serialize ConvoySpec");
        assert_eq!(spec_json.get("ref"), Some(&serde_json::json!("branch")));
        let metadata = crate::ObjectMeta {
            name: "demo".to_string(),
            namespace: "flotilla".to_string(),
            resource_version: "1".to_string(),
            labels: Default::default(),
            annotations: Default::default(),
            owner_references: vec![crate::OwnerReference {
                api_version: "flotilla.work/v1".to_string(),
                kind: "Convoy".to_string(),
                name: "parent".to_string(),
                controller: true,
            }],
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: "2026-08-03T20:00:00Z".parse().expect("timestamp"),
            merge: None,
        };
        let metadata_json = serde_json::to_value(metadata).expect("serialize ObjectMeta");
        assert_eq!(metadata_json["ownerReferences"][0]["apiVersion"], serde_json::json!("flotilla.work/v1"));
        let target =
            flotilla_protocol::PlacementTargetHost::builder().reference("host-a".to_string()).display_name("Host A".to_string()).build();
        let target_json = serde_json::to_value(target).expect("serialize PlacementTargetHost");
        assert_eq!(target_json.get("ref"), Some(&serde_json::json!("host-a")));
        assert_eq!(serde_json::to_value(flotilla_protocol::IssueState::Open).expect("serialize IssueState"), serde_json::json!("open"));
        assert_eq!(serde_json::to_value(crate::Stance::WorkspaceWrite).expect("serialize Stance"), serde_json::json!("workspace-write"));

        let Type::User(UserType::Struct(placement)) = <crate::PlacementStatus as Facet>::SHAPE.ty else {
            panic!("PlacementStatus should have a struct shape");
        };
        assert!(placement.fields.iter().find(|field| field.name == "fields").expect("fields shape").is_flattened());
        assert!(<crate::InputValue as Facet>::SHAPE.is_untagged());

        let placement = crate::PlacementStatus { fields: [("host".to_string(), serde_json::json!("host-a"))].into() };
        assert_eq!(serde_json::to_value(placement).expect("serialize PlacementStatus"), serde_json::json!({ "host": "host-a" }));
        assert_eq!(
            serde_json::to_value(crate::InputValue::String("value".to_string())).expect("serialize InputValue"),
            serde_json::json!("value")
        );
    }

    fn assert_struct_field_rename<T: for<'facet> Facet<'facet>>(rust_name: &str, serialized_name: &str) {
        let Type::User(UserType::Struct(structure)) = T::SHAPE.ty else {
            panic!("expected struct shape");
        };
        let field = structure.fields.iter().find(|field| field.name == rust_name).expect("field shape");
        assert_eq!(field.rename.unwrap_or(field.name), serialized_name);
    }

    fn assert_enum_variant_rename<T: for<'facet> Facet<'facet>>(rust_name: &str, serialized_name: &str) {
        let Type::User(UserType::Enum(enumeration)) = T::SHAPE.ty else {
            panic!("expected enum shape");
        };
        let variant = enumeration.variants.iter().find(|variant| variant.name == rust_name).expect("variant shape");
        assert_eq!(variant.rename.unwrap_or(variant.name), serialized_name);
    }
}
