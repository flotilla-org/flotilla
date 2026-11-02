//! Forge observations on the engine: ChangeRequest and Issue, whose records live in
//! `flotilla_resources::forge_observation`, and ForgeRead, each a plain kind registered here.

use flotilla_protocol::IssueSource;
pub use flotilla_resources::forge_observation::*;
use flotilla_resources::{ForgeReadRequest, ForgeReadStatus};
use flotilla_schema::{Kind, KindRegistry, RegistryError, Stored, TableSet, Timestamp};
use serde::{Deserialize, Serialize};

/// A forge read's successful result, read whole; `value` is null when the read found nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadValue {
    pub value: serde_json::Value,
}

/// The observing home's latest result of one forge read: kind code `fr`, named by the read's
/// `forge_read_name`.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "ForgeRead", code = "fr")]
pub struct ForgeReadRecord {
    /// The read's canonical source.
    pub service: String,
    pub scope: String,
    /// The operation and its parameters, read whole.
    #[store(cbor)]
    pub request: ForgeReadRequest,
    /// The last successful result; none until a read succeeds.
    #[store(cbor)]
    pub value: Option<ReadValue>,
    pub error: Option<String>,
    pub retry_at: Option<Timestamp>,
}

impl ForgeReadRecord {
    /// The record of an old read's source, request and latest status.
    pub fn from_old(source: &IssueSource, request: &ForgeReadRequest, status: &ForgeReadStatus) -> Self {
        Self {
            service: source.service.clone(),
            scope: source.scope.clone(),
            request: request.clone(),
            value: status.value.clone().map(|value| ReadValue { value }),
            error: status.error.clone(),
            retry_at: status.retry_at.as_ref().map(Timestamp::from_datetime),
        }
    }

    pub fn source(&self) -> IssueSource {
        IssueSource { service: self.service.clone(), scope: self.scope.clone() }
    }
}

/// Registers the three kinds into `registry` under their codes, `cq`, `iu` and `fr`: plain kinds.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register_table_set(added_since_previous_generation::<ChangeRequestRecord>(
        &["head_branch", "base_branch"],
        &["closes_observed"],
    ))?;
    registry.register_table_set(added_since_previous_generation::<IssueRecord>(
        &["body", "issue_type", "parent_service", "parent_scope", "parent_number", "fields"],
        &["parent_observed", "blocked_by_observed"],
    ))?;
    registry.register::<ForgeReadRecord>()
}

/// `K`'s tables with root columns the previous generation's table lacks marked, so opening its
/// store adds them: `absent` as unobserved, `unset` flags as false. ADR 0047 N→N+1: remove the
/// marks, and the test helper `previous_generation_tables`, after the next fleet roll.
fn added_since_previous_generation<K: Kind>(absent: &[&str], unset: &[&str]) -> TableSet {
    let mut set = K::table_set();
    for (name, flag) in absent.iter().map(|name| (name, false)).chain(unset.iter().map(|name| (name, true))) {
        let column = set.tables[0].columns.iter_mut().find(|column| column.name == *name);
        let column = column.unwrap_or_else(|| panic!("{} has no root column {name}", K::KIND));
        if flag {
            column.mark_previous_generation_default(0);
        } else {
            column.mark_previous_generation_absent();
        }
        assert!(column.previous_generation_value().is_some(), "{} column {name} cannot be added to an older table", K::KIND);
    }
    set
}

/// The forge observation kinds alone, for their DDL snapshot.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
    Ok(registry)
}
