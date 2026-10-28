//! Artifact on the engine: a plain record (a spec and no machine) describing a body held by a
//! blob store, homed where it was put and written by that home alone.

use std::collections::BTreeMap;

use flotilla_resources::ArtifactSpec;
use flotilla_schema::{KindRegistry, RegistryError, Stored, Timestamp};

/// One summary value: the scalars a put accepts.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum SummaryValue {
    Text(String),
    Bool(bool),
    Integer(i64),
    /// An integer above `i64::MAX`.
    Unsigned(u64),
    Float(f64),
}

impl PartialEq for SummaryValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Text(left), Self::Text(right)) => left == right,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Integer(left), Self::Integer(right)) => left == right,
            (Self::Unsigned(left), Self::Unsigned(right)) => left == right,
            (Self::Float(left), Self::Float(right)) => left.to_bits() == right.to_bits(),
            _ => false,
        }
    }
}

impl Eq for SummaryValue {}

impl SummaryValue {
    /// The summary value of `value`; refused for anything but a string, number or boolean.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, String> {
        match value {
            serde_json::Value::String(text) => Ok(Self::Text(text.clone())),
            serde_json::Value::Bool(flag) => Ok(Self::Bool(*flag)),
            serde_json::Value::Number(number) => number
                .as_i64()
                .map(Self::Integer)
                .or_else(|| number.as_u64().map(Self::Unsigned))
                .or_else(|| number.as_f64().map(Self::Float))
                .ok_or_else(|| format!("number {number} is not representable")),
            other => Err(format!("summary value {other} is not a string, number or boolean")),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Text(text) => serde_json::Value::String(text.clone()),
            Self::Bool(flag) => serde_json::Value::Bool(*flag),
            Self::Integer(number) => serde_json::Value::from(*number),
            Self::Unsigned(number) => serde_json::Value::from(*number),
            Self::Float(number) => serde_json::Value::from(*number),
        }
    }
}

/// One artifact slot's latest body: kind code `af`, named by the slot's record name (or, for an
/// image build's log, by the build and attempt).
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "Artifact", code = "af", index(name = "by_convoy", eq(convoy)))]
pub struct ArtifactRecord {
    /// The convoy the artifact belongs to; empty for one that belongs to none (a build log).
    pub convoy: String,
    pub producer: String,
    /// What the artifact is (`decision-ledger`, `brief`, `build-log`, …).
    pub artifact_kind: String,
    pub subject: String,
    /// Scalar facts about the body, read whole.
    #[store(cbor)]
    pub summary: BTreeMap<String, SummaryValue>,
    pub digest: String,
    pub size: u64,
    pub media_type: String,
    /// When the body was put; none for a brief, whose writer does not stamp it.
    pub recorded_at: Option<Timestamp>,
    pub expires_at: Timestamp,
    /// Kept past `expires_at`.
    pub pinned: bool,
}

impl ArtifactRecord {
    /// The record of `spec`, its times to the microsecond; refused for a summary value that is
    /// not a scalar.
    pub fn from_spec(spec: &ArtifactSpec) -> Result<Self, String> {
        let summary = spec
            .summary
            .iter()
            .map(|(key, value)| Ok((key.clone(), SummaryValue::from_json(value).map_err(|error| format!("summary `{key}`: {error}"))?)))
            .collect::<Result<_, String>>()?;
        Ok(Self {
            convoy: spec.convoy.clone(),
            producer: spec.producer.clone(),
            artifact_kind: spec.kind.clone(),
            subject: spec.subject.clone(),
            summary,
            digest: spec.digest.clone(),
            size: spec.size,
            media_type: spec.media_type.clone(),
            recorded_at: spec.recorded_at.as_ref().map(Timestamp::from_datetime),
            expires_at: Timestamp::from_datetime(&spec.expires_at),
            pinned: spec.pinned,
        })
    }

    /// The record as the old spec; refused when a time lies outside chrono's range.
    pub fn spec(&self) -> Result<ArtifactSpec, String> {
        let time = |at: Timestamp| at.to_datetime().ok_or_else(|| format!("time {} is out of range", at.micros()));
        Ok(ArtifactSpec {
            convoy: self.convoy.clone(),
            producer: self.producer.clone(),
            kind: self.artifact_kind.clone(),
            subject: self.subject.clone(),
            summary: self.summary.iter().map(|(key, value)| (key.clone(), value.to_json())).collect(),
            digest: self.digest.clone(),
            size: self.size,
            media_type: self.media_type.clone(),
            recorded_at: self.recorded_at.map(time).transpose()?,
            expires_at: time(self.expires_at)?,
            pinned: self.pinned,
        })
    }
}

/// Registers Artifact into `registry` under its code, `af`: a plain kind.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register::<ArtifactRecord>()
}

/// Artifact alone, for its DDL snapshot and the import rehearsal.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
    Ok(registry)
}
