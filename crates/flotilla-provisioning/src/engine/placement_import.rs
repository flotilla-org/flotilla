//! Placement's import onto the engine: each live copy imports at its home, and each policy
//! becomes a FulfilmentKind of its name.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_engine_store::Rehearsal;
use flotilla_resources::{
    definitions::DefinitionOrigin, is_prepared_snapshot, realisation_settings, FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec,
    FulfilmentRealisation, PlacementPolicy, PlacementPolicySpec, Resource, ResourceObject, PLACEMENT_SNAPSHOT_KIND,
};
use flotilla_schema::Kind;

use super::{
    definition_import::{label, origin},
    import::{ImportRefusal, ImportReport},
};
use crate::stored::placement::{self as stored, FulfilmentKindDefinition};

const KINDS: &str = <FulfilmentKindDefinition as Kind>::KIND;
const POLICIES: &str = "PlacementPolicy";

pub struct PlacementCopy<R: Resource> {
    pub namespace: String,
    pub object: ResourceObject<R>,
    pub home: String,
}

#[derive(Default)]
pub struct PlacementCopies {
    pub kinds: Vec<PlacementCopy<FulfilmentKind>>,
    pub policies: Vec<PlacementCopy<PlacementPolicy>>,
}

pub struct Placed<S> {
    pub name: String,
    pub home: String,
    pub record: S,
    pub origin: DefinitionOrigin,
}

#[derive(Default)]
pub struct ImportedPlacement {
    pub kinds: Vec<Placed<FulfilmentKindDefinition>>,
}

/// The realisation names the provider instance; nothing carries any other constraint.
fn requirements(spec: &PlacementPolicySpec) -> Result<(), String> {
    let requirements = spec.provider_requirements.clone().unwrap_or_default();
    if !requirements.material_modes.is_empty() || requirements.checkout_strategy.is_some() || requirements.egress_enforcement.is_some() {
        return Err("its provider requirements constrain more than the instance, which nothing carries after the move".into());
    }
    Ok(())
}

fn host_ref(spec: &PlacementPolicySpec) -> &str {
    match (&spec.docker_per_vessel, &spec.host_direct) {
        (Some(docker), _) => &docker.host_ref,
        (None, Some(host)) => &host.host_ref,
        (None, None) => "",
    }
}

struct KindEntry {
    namespaces: BTreeSet<String>,
    record: Result<(FulfilmentKindDefinition, DefinitionOrigin), String>,
}

/// `host_platform` is the platform grant of the host's host-direct kind.
fn kind_of(spec: &PlacementPolicySpec, host_platform: Option<&FulfilmentGrant>) -> Result<FulfilmentKindSpec, String> {
    if spec.host_direct.is_some() {
        let platform = host_platform.ok_or("a host-direct policy needs its host's platform, and its host has no host-direct kind")?;
        let platform = platform.0.strip_prefix("platform:").unwrap_or(&platform.0);
        return FulfilmentKindSpec::from_policy(spec, platform);
    }
    FulfilmentKindSpec::from_policy(spec, "linux")
}

pub fn import_placement(copies: &PlacementCopies, report: &mut ImportReport) -> ImportedPlacement {
    let mut imported = ImportedPlacement::default();
    let mut kinds = BTreeMap::<(String, String), KindEntry>::new();
    let mut platforms = BTreeMap::<(String, String), FulfilmentGrant>::new();
    for copy in &copies.kinds {
        if copy.object.metadata.deletion_timestamp.is_some() {
            *report.dropped.entry((KINDS, "deleted")).or_default() += 1;
            continue;
        }
        let spec = &copy.object.spec;
        let record = origin(&copy.object.metadata).and_then(|origin| Ok((FulfilmentKindDefinition::new(spec)?, origin)));
        if let (Ok(_), FulfilmentRealisation::HostDirect, Some(platform)) =
            (&record, &spec.realisation, spec.grants.iter().find(|grant| grant.0.starts_with("platform:")))
        {
            platforms.insert((copy.home.clone(), spec.host_ref.clone()), platform.clone());
        }
        let entry = kinds
            .entry((copy.object.metadata.name.clone(), copy.home.clone()))
            .or_insert_with(|| KindEntry { namespaces: BTreeSet::new(), record });
        entry.namespaces.insert(copy.namespace.clone());
    }
    for copy in &copies.policies {
        let name = &copy.object.metadata.name;
        let spec = &copy.object.spec;
        let refuse =
            |reason: String| ImportRefusal { kind: POLICIES, record: name.clone(), reason: format!("at `{}`: {reason}", copy.home) };
        if copy.object.metadata.deletion_timestamp.is_some() {
            *report.dropped.entry((POLICIES, "deleted")).or_default() += 1;
            continue;
        }
        if spec.priority != 0 {
            report.notes.push(format!(
                "{POLICIES} `{name}` at `{}`: its priority {} is dropped; admission selects by grants and cost",
                copy.home, spec.priority
            ));
        }
        if spec.docker_per_vessel.as_ref().is_some_and(|docker| !docker.agent_adapters.is_empty()) {
            *report.dropped.entry((POLICIES, "agent adapters, an image fact the host probes")).or_default() += 1;
        }
        let snapshot = is_prepared_snapshot(name, &copy.object.metadata.labels, PLACEMENT_SNAPSHOT_KIND);
        let policy_origin = if snapshot { Ok(DefinitionOrigin::Snapshot) } else { origin(&copy.object.metadata) };
        let policy_origin = match policy_origin.and_then(|origin| if snapshot { Ok(origin) } else { requirements(spec).map(|()| origin) }) {
            Ok(origin) => origin,
            Err(reason) => {
                report.refused.push(refuse(reason));
                continue;
            }
        };
        let key = (name.clone(), copy.home.clone());
        if let Some(entry) = kinds.get_mut(&key) {
            // The daemon already migrated it to this kind.
            entry.namespaces.insert(copy.namespace.clone());
        } else {
            let host = (copy.home.clone(), host_ref(spec).to_owned());
            let record = kind_of(spec, platforms.get(&host))
                .and_then(|kind| FulfilmentKindDefinition::new(&kind))
                .map(|record| (record, policy_origin))
                .map_err(|reason| format!("its kind: {reason}"));
            let refused = record.is_err();
            kinds.insert(key, KindEntry { namespaces: BTreeSet::from([copy.namespace.clone()]), record });
            if refused {
                continue;
            }
        }
        if snapshot {
            // The settings a snapshot copied are its host's, which are the constants.
            *report.dropped.entry((POLICIES, "a snapshot's settings, the constants")).or_default() += 1;
            continue;
        }
        let differing = realisation_settings::differing_settings(spec);
        if !differing.is_empty() {
            report.notes.push(format!(
                "{POLICIES} `{name}` at `{}`: its `{}` differ from the code constants and are not carried",
                copy.home,
                differing.join("`, `")
            ));
        }
        if spec.cloud_sandbox_per_vessel.is_some() {
            *report.dropped.entry((POLICIES, "a cloud sandbox's settings")).or_default() += 1;
        }
        if spec.docker_per_vessel.is_some() {
            *report.dropped.entry((POLICIES, "a Docker memory share")).or_default() += 1;
        }
    }
    // The engine has no namespaces, so a name held in several is refused in all of them.
    for ((name, home), KindEntry { namespaces, record }) in kinds {
        let refuse = |reason: String| ImportRefusal { kind: KINDS, record: name.clone(), reason: format!("at `{home}`: {reason}") };
        if namespaces.len() > 1 {
            let held: Vec<_> = namespaces.into_iter().collect();
            let also = record.err().map(|reason| format!("; {reason}")).unwrap_or_default();
            report.refused.push(refuse(format!("held in namespaces `{}`, and the engine has no namespaces{also}", held.join("`, `"))));
            continue;
        }
        match record {
            Ok((record, origin)) => imported.kinds.push(Placed { name, home, record, origin }),
            Err(reason) => report.refused.push(refuse(reason)),
        }
    }
    imported
}

/// Restores every imported record into a temporary store and reads it back.
pub fn rehearse_placement(copies: &PlacementCopies) -> ImportReport {
    let mut report = ImportReport::default();
    let mut rehearsal = match stored::registry().map_err(|error| format!("registry: {error:?}")).and_then(Rehearsal::open) {
        Ok(rehearsal) => rehearsal,
        Err(reason) => {
            report.refused.push(ImportRefusal { kind: KINDS, record: "<all>".to_owned(), reason });
            return report;
        }
    };
    let imported = import_placement(copies, &mut report);
    for placed in &imported.kinds {
        match rehearsal.restore(&placed.home, &placed.name, 1, &placed.record) {
            Ok(()) => *report.imported.entry((KINDS, label(&placed.origin))).or_default() += 1,
            Err(reason) => report.refused.push(ImportRefusal { kind: KINDS, record: placed.name.clone(), reason }),
        }
    }
    report
}
