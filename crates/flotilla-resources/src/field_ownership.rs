use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Resource, ResourceError};

/// The closed set of roles which may author resource fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriterRole {
    Operator,
    ReconcileLoop,
    Actuator,
}

/// Identity supplied at the single resource write path.
///
/// `store_authority` is deliberately part of the identity rather than a separate
/// write argument. ADR 0025 can make it mandatory without changing call sites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WriterIdentity {
    pub role: WriterRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_authority: Option<NodeId>,
}

impl WriterIdentity {
    pub fn new(role: WriterRole) -> Self {
        Self { role, store_authority: None }
    }

    pub fn operator() -> Self {
        Self::new(WriterRole::Operator)
    }

    pub fn reconcile_loop() -> Self {
        Self::new(WriterRole::ReconcileLoop)
    }

    pub fn actuator() -> Self {
        Self::new(WriterRole::Actuator)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipEnforcement {
    Observe,
    Enforce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldOwnership {
    /// Dot-separated path rooted at `spec`.
    ///
    /// Status ownership joins this format when the status write path enrolls.
    pub field: &'static str,
    pub owner: WriterRole,
}

impl FieldOwnership {
    pub const fn new(field: &'static str, owner: WriterRole) -> Self {
        Self { field, owner }
    }
}

/// A resource enrolled in static field ownership.
///
/// Tables live beside the resource type and enumerate every spec/status leaf.
/// Nested objects may be declared as one field when they have one indivisible
/// owner. New enrollees start in `Observe` and flip this associated constant
/// only after their adversarial scenarios pass.
pub trait FieldOwnedResource: Resource {
    const FIELD_OWNERSHIP: &'static [FieldOwnership];
    const OWNERSHIP_ENFORCEMENT: OwnershipEnforcement = OwnershipEnforcement::Observe;

    fn spec_field_value(spec: &Self::Spec, field: &str) -> Result<Option<Value>, ResourceError>
    where
        Self: Sized,
    {
        serialized_spec_field_value::<Self>(spec, field)
    }

    fn spec_field_restore_value(spec: &Self::Spec, field: &str) -> Result<Value, ResourceError>
    where
        Self: Sized,
    {
        Ok(serialized_spec_field_value::<Self>(spec, field)?.unwrap_or(Value::Null))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct FieldOwnershipViolation {
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub writer: WriterIdentity,
    pub field: String,
    pub attempted_value: Value,
    pub rule: String,
    pub observed_at: DateTime<Utc>,
}

pub(crate) fn merge_owned_spec<T: FieldOwnedResource>(
    current: &T::Spec,
    requested: &T::Spec,
    writer: &WriterIdentity,
    namespace: &str,
    name: &str,
) -> Result<(T::Spec, Vec<FieldOwnershipViolation>), ResourceError> {
    let mut merged =
        serde_json::to_value(requested).map_err(|error| ResourceError::decode(format!("serialize requested spec: {error}")))?;
    let mut violations = Vec::new();
    let mut resolved_subtrees = Vec::<&str>::new();

    for ownership in T::FIELD_OWNERSHIP {
        if resolved_subtrees.iter().any(|parent| ownership.field.strip_prefix(parent).is_some_and(|suffix| suffix.starts_with('.'))) {
            continue;
        }
        let relative = ownership.field.strip_prefix("spec.").ok_or_else(|| {
            ResourceError::invalid(format!("{} ownership table field '{}' is not rooted at spec", T::API_PATHS.kind, ownership.field))
        })?;
        let current_field = T::spec_field_value(current, ownership.field)?;
        let requested_field = T::spec_field_value(requested, ownership.field)?;
        if current_field == requested_field {
            continue;
        }
        if ownership.owner == writer.role {
            resolved_subtrees.push(ownership.field);
            continue;
        }
        let attempted_value = requested_field.unwrap_or(Value::Null);
        violations.push(
            FieldOwnershipViolation::builder()
                .kind(T::API_PATHS.kind.to_string())
                .namespace(namespace.to_string())
                .name(name.to_string())
                .writer(writer.clone())
                .field(ownership.field.to_string())
                .attempted_value(attempted_value)
                .rule(format!("{} is owned by {:?}", ownership.field, ownership.owner))
                .observed_at(Utc::now())
                .build(),
        );
        set_value_at_path(&mut merged, relative, T::spec_field_restore_value(current, ownership.field)?)?;
        resolved_subtrees.push(ownership.field);
    }

    let merged = serde_json::from_value(merged).map_err(|error| ResourceError::decode(format!("decode ownership-merged spec: {error}")))?;
    Ok((merged, violations))
}

pub(crate) fn serialized_spec_field_value<T: Resource>(spec: &T::Spec, field: &str) -> Result<Option<Value>, ResourceError> {
    let relative = field
        .strip_prefix("spec.")
        .ok_or_else(|| ResourceError::invalid(format!("{} ownership field '{field}' is not rooted at spec", T::API_PATHS.kind)))?;
    let value = serde_json::to_value(spec).map_err(|error| ResourceError::decode(format!("serialize spec field: {error}")))?;
    Ok(value_at_path(&value, relative).cloned())
}

fn value_at_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |cursor, segment| cursor.get(segment))
}

fn set_value_at_path(value: &mut Value, path: &str, replacement: Value) -> Result<(), ResourceError> {
    let mut segments = path.split('.').peekable();
    let mut cursor = value;
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            let object = cursor
                .as_object_mut()
                .ok_or_else(|| ResourceError::invalid(format!("ownership field parent for '{path}' is not an object")))?;
            object.insert(segment.to_string(), replacement);
            return Ok(());
        }
        cursor = cursor.get_mut(segment).ok_or_else(|| ResourceError::invalid(format!("ownership field parent for '{path}' is absent")))?;
    }
    Err(ResourceError::invalid("ownership field path must not be empty"))
}
