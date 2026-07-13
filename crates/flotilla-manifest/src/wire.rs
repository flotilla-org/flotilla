//! Mirror of andamento's metadata-patch wire types.
//!
//! These types replicate `andamento-shared`'s serde exactly (verified by the
//! fixture round-trip tests in this module — fixtures are generated from
//! andamento's real serde, see `fixtures/README.md`). They are a deliberate
//! v0 stopgap: at the Leg-1 manifest extraction this module is deleted and
//! replaced by a dependency on the shared `flotilla-org/manifest` crate.
//! Never hand-tune a serde attribute here to make a test pass — regenerate
//! the fixtures against andamento and match what it actually speaks.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A metadata fact value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum MetadataValue {
    Text(String),
    Bool(bool),
    Integer(i64),
    StringList(Vec<String>),
    GroupPath(Vec<MetadataPathSegmentValue>),
}

impl MetadataValue {
    pub fn text(value: impl Into<String>) -> Self {
        MetadataValue::Text(value.into())
    }
}

/// A segment value inside a `MetadataValue::GroupPath` — like a
/// [`GroupSegment`] but restricted to non-path values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataPathSegmentValue {
    pub key: String,
    pub value: MetadataPathValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl MetadataPathSegmentValue {
    pub fn text(key: impl Into<String>, value: impl Into<String>) -> Self {
        MetadataPathSegmentValue { key: key.into(), value: MetadataPathValue::Text(value.into()), label: None }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// Labels are presentation, not identity — mirror andamento's equality.
impl PartialEq for MetadataPathSegmentValue {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.value == other.value
    }
}

impl Eq for MetadataPathSegmentValue {}

impl std::hash::Hash for MetadataPathSegmentValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
        self.value.hash(state);
    }
}

impl PartialOrd for MetadataPathSegmentValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MetadataPathSegmentValue {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.cmp(&other.key).then_with(|| self.value.cmp(&other.value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum MetadataPathValue {
    Text(String),
    Bool(bool),
    Integer(i64),
    StringList(Vec<String>),
}

/// A hierarchical grouping path — the catalog's project → convoy → vessel
/// spine is expressed as one of these per group target.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GroupPath(pub Vec<GroupSegment>);

impl GroupPath {
    /// This path as a scope *value* (for `tab.scope` facts). Nested
    /// group-path values are unrepresentable in path segments and are
    /// skipped; the catalog only builds paths from text segments.
    pub fn to_scope_value(&self) -> MetadataValue {
        let segments = self
            .0
            .iter()
            .filter_map(|segment| {
                let value = match &segment.value {
                    MetadataValue::Text(value) => MetadataPathValue::Text(value.clone()),
                    MetadataValue::Bool(value) => MetadataPathValue::Bool(*value),
                    MetadataValue::Integer(value) => MetadataPathValue::Integer(*value),
                    MetadataValue::StringList(values) => MetadataPathValue::StringList(values.clone()),
                    MetadataValue::GroupPath(_) => return None,
                };
                Some(MetadataPathSegmentValue { key: segment.key.clone(), value, label: segment.label.clone() })
            })
            .collect();
        MetadataValue::GroupPath(segments)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupSegment {
    pub key: String,
    pub value: MetadataValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl GroupSegment {
    pub fn text(key: impl Into<String>, value: impl Into<String>) -> Self {
        GroupSegment { key: key.into(), value: MetadataValue::text(value), label: None }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// Labels are presentation, not identity — mirror andamento's equality.
impl PartialEq for GroupSegment {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.value == other.value
    }
}

impl Eq for GroupSegment {}

impl std::hash::Hash for GroupSegment {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
        self.value.hash(state);
    }
}

impl PartialOrd for GroupSegment {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GroupSegment {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.cmp(&other.key).then_with(|| self.value.cmp(&other.value))
    }
}

/// A key=value identity node in the PM's metadata graph. Facts published
/// against an identity resolve transitively onto every pane carrying it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MetadataIdentity {
    pub key: String,
    pub value: MetadataValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "kebab-case")]
pub enum PaneTarget {
    Terminal(u32),
    Plugin(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum MetadataTarget {
    Root,
    Pane(PaneTarget),
    Tab(u64),
    Group(GroupPath),
    Identity(MetadataIdentity),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataValueUpdate {
    pub value: MetadataValue,
    pub ttl_ms: Option<u64>,
    pub precedence: Option<i64>,
    pub ordinal: Option<i64>,
}

impl MetadataValueUpdate {
    pub fn new(value: MetadataValue, ttl_ms: Option<u64>) -> Self {
        MetadataValueUpdate { value, ttl_ms, precedence: None, ordinal: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct MetadataPatch {
    pub target: MetadataTarget,
    pub source_id: String,
    #[serde(default)]
    #[builder(default)]
    pub set: BTreeMap<String, MetadataValueUpdate>,
    #[serde(default)]
    #[builder(default)]
    pub unset: Vec<String>,
}

impl MetadataPatch {
    /// The compact payload written to the apply-patch pipe — the
    /// patch wrapped in andamento's externally-visible message envelope.
    pub fn to_pipe_payload(&self) -> String {
        serde_json::to_string(&WireMessage::MetadataPatch(self.clone())).expect("metadata patch serializes")
    }
}

/// Andamento's `ExternalMessage` envelope, mirrored only for the variant
/// flotilla emits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum WireMessage {
    MetadataPatch(MetadataPatch),
}

/// An identity the PM reports as present on at least one pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedMetadataIdentity {
    pub identity: MetadataIdentity,
    pub target_count: usize,
    pub nearest_distance: usize,
}

/// Parse the observed-identities pipe output. When several plugin instances
/// answer, the CLI concatenates one JSON array per responder, whitespace
/// separated.
pub fn parse_observed_identities(output: &str) -> Result<Vec<ObservedMetadataIdentity>, String> {
    let mut identities = Vec::new();
    for batch in serde_json::Deserializer::from_str(output).into_iter::<Vec<ObservedMetadataIdentity>>() {
        identities.extend(batch.map_err(|error| format!("observed identities parse: {error}"))?);
    }
    Ok(identities)
}

#[cfg(test)]
mod tests;
