//! The Demand import: each open Demand a live writer raises becomes the Pending attention that
//! writer sends on the engine, keeping the demand's name as its id. A demand already resolved,
//! one whose fact another record holds, or one whose record is gone is dropped, counted by
//! reason; any shape no live writer makes is refused, named.
//!
//! | Demand name | Attention |
//! |---|---|
//! | `ensure-attention-<ensure>` | [`Cause::Hold`] of the convoy |
//! | `ensure-config-drift-<ensure>` | [`Cause::Drift`] of the ensure's standing role |
//! | `reclaim-refusal-<convoy>` | [`Cause::ReclaimRefused`] of the convoy |
//! | `credential-refresh-<vessel>-<credential>` | [`Cause::CredentialRefresh`], one per vessel |
//! | `terminal-delivery-<session>` | [`Cause::DeliveryHeld`] of the session's convoy role |
//! | `terminal-attention-<session>`, `declaration-refused-<project>` | dropped |

use std::collections::BTreeMap;

use flotilla_address::{Address, Kind};
use flotilla_charter::ops_entry::{ENSURE_CONFIG_DRIFT_REASON_ANNOTATION, ENSURE_DRIFT_ATTENTION_PREFIX};
use flotilla_protocol::ResourceRef;
use flotilla_resources::{
    delivery_hold::DELIVERY_HOLD_REASON_ANNOTATION, Convoy, ConvoyEnsure, Demand, DemandAddressee, DemandState, MessageReference,
    ResourceObject, TerminalSession, Vessel, CONVOY_LABEL, CREDENTIAL_REFRESH_REASON_ANNOTATION, CREDENTIAL_REFRESH_VESSEL_ANNOTATION,
    RECLAIM_REFUSAL_REASON_ANNOTATION, ROLE_LABEL,
};
use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::message::{Message as Machine, MessageState},
    Resource, StoredState, Version,
};

use super::{
    attention::{Attention, Cause, Read},
    convert::Conversion,
    import::{check, imported_name, ImportDrop, ImportRefusal, Imported, ImportedMessage},
};

const HOLD: &str = "ensure-attention-";
const RECLAIM: &str = "reclaim-refusal-";
const CREDENTIAL: &str = "credential-refresh-";
const DELIVERY: &str = "terminal-delivery-";
const TERMINAL_ATTENTION: &str = "terminal-attention-";
const DECLARATION_REFUSED: &str = "declaration-refused-";

/// One old Demand, as the import reads it: the record and the host it is homed at.
pub struct OldDemand<'a> {
    pub record: &'a ResourceObject<Demand>,
    pub home: String,
}

/// The records of one namespace a demand names.
#[derive(Default)]
pub struct DemandRecords<'a> {
    pub convoys: BTreeMap<String, &'a ResourceObject<Convoy>>,
    pub ensures: BTreeMap<String, &'a ResourceObject<ConvoyEnsure>>,
    pub vessels: Vec<&'a ResourceObject<Vessel>>,
    pub terminals: BTreeMap<String, &'a ResourceObject<TerminalSession>>,
}

/// An attention a demand maps to: its id, and the person and project it is addressed by.
struct Pending {
    id: String,
    attention: Attention,
    principal: String,
    project: Option<String>,
}

/// What one demand becomes before it is admitted: an attention to send, or not.
enum Mapped {
    Attention(Box<Pending>),
    Dropped(&'static str),
}

/// What a demand is before the records it names are read.
enum Class {
    Open,
    /// An open credential demand, by its convoy and vessel.
    Credential(String, String),
    Dropped(&'static str),
    Refused(String),
}

/// Imports `demands`, each home's attentions admitted in creation order from `next(home)`.
pub fn import_demands(
    demands: &[OldDemand<'_>],
    next: &dyn Fn(&str) -> u64,
    conversion: &Conversion<'_>,
    records: &DemandRecords<'_>,
) -> Vec<Result<Imported, ImportRefusal>> {
    let mut ordered: Vec<&OldDemand<'_>> = demands.iter().collect();
    ordered.sort_by_key(|old| (old.home.clone(), raised(old.record), old.record.metadata.name.clone()));
    let mut results = Vec::new();
    let mut credentials: BTreeMap<(String, String), Vec<&OldDemand<'_>>> = BTreeMap::new();
    let mut versions: BTreeMap<String, u64> = BTreeMap::new();
    let mut admit = |old: &OldDemand<'_>, mapped: Result<Mapped, String>, results: &mut Vec<_>| {
        let record = old.record.metadata.name.clone();
        let result = match mapped {
            Ok(Mapped::Dropped(reason)) => Ok(Imported::Dropped(ImportDrop { record, reason })),
            Ok(Mapped::Attention(pending)) => {
                let version = versions.entry(old.home.clone()).or_insert_with(|| next(&old.home));
                let admitted = message(old, &pending, *version, conversion);
                if admitted.is_ok() {
                    *version += 1;
                }
                admitted.map(|message| Imported::Message(Box::new(message))).map_err(|reason| ImportRefusal { record, reason })
            }
            Err(reason) => Err(ImportRefusal { record, reason }),
        };
        results.push(result);
    };
    for old in ordered {
        match classify(old.record, records) {
            Class::Open => admit(old, map(old.record, conversion, records), &mut results),
            Class::Credential(convoy, vessel) => credentials.entry((convoy, vessel)).or_default().push(old),
            Class::Dropped(reason) => admit(old, Ok(Mapped::Dropped(reason)), &mut results),
            Class::Refused(reason) => admit(old, Err(reason), &mut results),
        }
    }
    for ((convoy, vessel), group) in credentials {
        let (first, folded) = group.split_first().expect("a group has a demand");
        admit(first, credential(&convoy, &vessel, &group, conversion, records), &mut results);
        for old in folded {
            results.push(Ok(Imported::Dropped(ImportDrop {
                record: old.record.metadata.name.clone(),
                reason: "folded into its vessel's attention",
            })));
        }
    }
    results
}

fn raised(record: &ResourceObject<Demand>) -> chrono::DateTime<chrono::Utc> {
    record.status.as_ref().and_then(|status| status.raised.as_ref()).map_or(record.metadata.creation_timestamp, |raised| raised.as_of)
}

/// Settles what needs no records, refusing any shape no live writer makes. An open credential
/// demand is grouped by its convoy and vessel, to be mapped with the others of its vessel.
fn classify(record: &ResourceObject<Demand>, records: &DemandRecords<'_>) -> Class {
    let name = record.metadata.name.as_str();
    let spec = &record.spec;
    if let DemandAddressee::Pool { pool_ref } = &spec.addressee {
        return Class::Refused(format!("it is addressed to pool `{}`; no live writer raises one", pool_ref.0));
    }
    if !spec.response_options.is_empty() || spec.human_gate.is_some() {
        return Class::Refused("it is a human gate with response options; no live writer raises one".into());
    }
    if record.status.as_ref().is_some_and(|status| status.verdict.is_some()) {
        return Class::Refused("it carries a verdict; no live writer rules on one".into());
    }
    let known = [HOLD, ENSURE_DRIFT_ATTENTION_PREFIX, RECLAIM, CREDENTIAL, DELIVERY, TERMINAL_ATTENTION, DECLARATION_REFUSED];
    if !known.iter().any(|prefix| name.starts_with(prefix)) {
        return Class::Refused("no live writer raises a demand of this name".into());
    }
    if name.starts_with(TERMINAL_ATTENTION) {
        return Class::Dropped("its TerminalSession holds the attention");
    }
    if name.starts_with(DECLARATION_REFUSED) {
        return Class::Dropped("its Project's operations hold the refusal");
    }
    if matches!(record.status.as_ref().map(|status| status.state), Some(DemandState::Acknowledged | DemandState::Satisfied)) {
        return Class::Dropped("resolved");
    }
    if !name.starts_with(CREDENTIAL) {
        return Class::Open;
    }
    let Some(vessel) = record.metadata.annotations.get(CREDENTIAL_REFRESH_VESSEL_ANNOTATION) else {
        return Class::Refused("a credential demand names no vessel".into());
    };
    match convoy_target(record) {
        Err(reason) => Class::Refused(reason),
        Ok(convoy) if !records.convoys.contains_key(convoy) => Class::Dropped("its convoy is gone"),
        Ok(convoy) => Class::Credential(convoy.to_owned(), vessel.clone()),
    }
}

fn convoy_target(record: &ResourceObject<Demand>) -> Result<&str, String> {
    let target = &record.spec.originating_work_ref;
    if target.kind != "Convoy" {
        return Err(format!("it is about a {}, not a Convoy", target.kind));
    }
    Ok(target.name.as_str())
}

fn principal(record: &ResourceObject<Demand>) -> String {
    match &record.spec.addressee {
        DemandAddressee::Principal { principal_ref } => format!("principal:{}", principal_ref.name),
        DemandAddressee::Pool { .. } => unreachable!("refused when classified"),
    }
}

fn reason(record: &ResourceObject<Demand>, annotation: &str, otherwise: impl FnOnce() -> String) -> String {
    record.metadata.annotations.get(annotation).cloned().unwrap_or_else(otherwise)
}

fn convoy_read(convoy: &ResourceObject<Convoy>, conversion: &Conversion<'_>) -> Result<Read, String> {
    let reference = MessageReference::ControlRecord {
        resource: ResourceRef::new("flotilla.work/v1", "Convoy", &convoy.metadata.namespace, &convoy.metadata.name),
        revision: convoy.metadata.resource_version.clone(),
    };
    Ok(Read { address: conversion.records.address(&reference)?, revision: convoy.metadata.resource_version.clone() })
}

fn child(parent: &Address, kind: Kind, name: &str) -> Result<Address, String> {
    parent.child(kind, name).map_err(|error| format!("{parent} {kind:?} `{name}`: {error}"))
}

fn map(record: &ResourceObject<Demand>, conversion: &Conversion<'_>, records: &DemandRecords<'_>) -> Result<Mapped, String> {
    let name = record.metadata.name.as_str();
    let attention = |cause, reason, convoy: &ResourceObject<Convoy>| {
        Mapped::Attention(Box::new(Pending {
            id: name.to_owned(),
            attention: Attention { cause, reason },
            principal: principal(record),
            project: convoy.spec.project_ref.clone(),
        }))
    };
    if let Some(session) = name.strip_prefix(DELIVERY) {
        let Some(terminal) = records.terminals.get(session) else { return Ok(Mapped::Dropped("its TerminalSession is gone")) };
        let label = |key| terminal.metadata.labels.get(key).ok_or_else(|| format!("TerminalSession `{session}` has no `{key}` label"));
        let Some(convoy) = records.convoys.get(label(CONVOY_LABEL)?.as_str()) else { return Ok(Mapped::Dropped("its convoy is gone")) };
        let read = convoy_read(convoy, conversion)?;
        let role = Read { address: child(&read.address, Kind::Role, label(ROLE_LABEL)?)?, revision: read.revision };
        let reason = reason(record, DELIVERY_HOLD_REASON_ANNOTATION, || "Turn delivery is held until a person rules.".into());
        return Ok(attention(Cause::DeliveryHeld { role }, reason, convoy));
    }
    let target = convoy_target(record)?;
    let Some(convoy) = records.convoys.get(target) else { return Ok(Mapped::Dropped("its convoy is gone")) };
    let read = convoy_read(convoy, conversion)?;
    if name.starts_with(HOLD) {
        // The ensure reconciler records a hold's reason under the reclaim-refusal key.
        let reason = reason(record, RECLAIM_REFUSAL_REASON_ANNOTATION, || format!("Standing convoy `{target}` is held."));
        return Ok(attention(Cause::Hold { convoy: read }, reason, convoy));
    }
    if name.starts_with(RECLAIM) {
        let reason = reason(record, RECLAIM_REFUSAL_REASON_ANNOTATION, || format!("Reclaim of convoy `{target}` is refused."));
        return Ok(attention(Cause::ReclaimRefused { convoy: read }, reason, convoy));
    }
    let ensure_name = name.strip_prefix(ENSURE_DRIFT_ATTENTION_PREFIX).expect("classified");
    let Some(ensure) = records.ensures.get(ensure_name) else { return Ok(Mapped::Dropped("its ConvoyEnsure is gone")) };
    let project = Address::project(&ensure.spec.project_ref).map_err(|error| format!("project `{}`: {error}", ensure.spec.project_ref))?;
    let role = Read { address: child(&project, Kind::Role, &ensure.spec.role)?, revision: ensure.metadata.resource_version.clone() };
    let reason = reason(record, ENSURE_CONFIG_DRIFT_REASON_ANNOTATION, || format!("Standing role `{}` has drifted.", ensure.spec.role));
    Ok(attention(Cause::Drift { role, convoy: read }, reason, convoy))
}

fn credential(
    convoy: &str,
    vessel_name: &str,
    group: &[&OldDemand<'_>],
    conversion: &Conversion<'_>,
    records: &DemandRecords<'_>,
) -> Result<Mapped, String> {
    let convoy = records.convoys.get(convoy).expect("classified");
    let Some(vessel) =
        records.vessels.iter().find(|vessel| vessel.spec.convoy_ref == convoy.metadata.name && vessel.spec.vessel_name == vessel_name)
    else {
        return Ok(Mapped::Dropped("its vessel is gone"));
    };
    let prefix = format!("{CREDENTIAL}{}-", vessel.metadata.name);
    let mut lines: Vec<String> = group
        .iter()
        .map(|old| {
            let name = &old.record.metadata.name;
            let credential = name.strip_prefix(&prefix).unwrap_or(name);
            let reason = old.record.metadata.annotations.get(CREDENTIAL_REFRESH_REASON_ANNOTATION).map_or("refresh failed", String::as_str);
            format!("{credential}: {reason}")
        })
        .collect();
    lines.sort();
    let read = convoy_read(convoy, conversion)?;
    let vessel_read =
        Read { address: child(&read.address, Kind::Vessel, vessel_name)?, revision: vessel.metadata.resource_version.clone() };
    Ok(Mapped::Attention(Box::new(Pending {
        id: format!("{CREDENTIAL}{}", vessel.metadata.name),
        attention: Attention { cause: Cause::CredentialRefresh { vessel: vessel_read }, reason: lines.join("\n") },
        principal: principal(group[0].record),
        project: convoy.spec.project_ref.clone(),
    })))
}

fn message(old: &OldDemand<'_>, pending: &Pending, version: u64, conversion: &Conversion<'_>) -> Result<ImportedMessage, String> {
    let inbox = conversion.person(&pending.principal, pending.project.as_deref())?;
    let mut spec = pending.attention.spec(conversion.roles, inbox.clone())?;
    spec.admitted_seq = version;
    let name = imported_name(&inbox, &pending.id)?;
    let created = Timestamp::from_datetime(&raised(old.record));
    let resource: Resource<Machine> =
        Resource::stored(name, spec, StoredState::<Machine>::new(MessageState::Pending, created), Version(version));
    check(&resource)?;
    Ok(ImportedMessage { record: old.record.metadata.name.clone(), home: old.home.clone(), resource, created })
}
