//! Placement's import onto the engine: each live copy imports at its home, and each policy
//! becomes a FulfilmentKind of its name.
//!
//! | Old copy | Imported |
//! |---|---|
//! | FulfilmentKind | the kind, written as its stamps' writer: admission's snapshot, seeding, or the definition's origin |
//! | PlacementPolicy with a kind of its name at its home | dropped: the kind is the daemon's migration of it |
//! | PlacementPolicy without one | the kind its topology describes; its settings are the code constants |
//! | Deleted | dropped |
//! | One name in several namespaces at one home | refused: the engine has no namespaces |

pub mod retired;

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_engine_store::ImportTarget;
use flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL;
use flotilla_resources::{
    definition_provenance::SEEDING_MANAGER, definitions::DefinitionOrigin, is_prepared_snapshot, FulfilmentGrant, FulfilmentKind,
    FulfilmentKindSpec, FulfilmentRealisation, ResourceObject, MANAGED_BY_LABEL, PLACEMENT_SNAPSHOT_KIND, PREPARED_SNAPSHOT_LABEL,
};
use flotilla_schema::{Kind, Timestamp};

use self::retired::{PolicySpec, RetiredPolicy};
use super::{
    definition_import::label,
    import::{ImportRefusal, ImportReport},
    kinds,
};
use crate::stored::placement::{self as stored, FulfilmentKindDefinition};

const KINDS: &str = <FulfilmentKindDefinition as Kind>::KIND;
const POLICIES: &str = retired::KIND;

pub struct PlacementCopy<O> {
    pub namespace: String,
    pub object: O,
    pub home: String,
}

#[derive(Default)]
pub struct PlacementCopies {
    pub kinds: Vec<PlacementCopy<ResourceObject<FulfilmentKind>>>,
    pub policies: Vec<PlacementCopy<RetiredPolicy>>,
}

/// A kind to write at its home, with the stamps its change is made from.
pub struct Placed {
    pub name: String,
    pub home: String,
    pub record: FulfilmentKindDefinition,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub created: DateTime<Utc>,
    /// What the report counts it under: its writer.
    pub writer: &'static str,
}

#[derive(Default)]
pub struct ImportedPlacement {
    pub kinds: Vec<Placed>,
}

/// The realisation names the provider instance; nothing carries any other constraint.
fn requirements(spec: &PolicySpec) -> Result<(), String> {
    let requirements = spec.provider_requirements.clone().unwrap_or_default();
    if !requirements.material_modes.is_empty() || requirements.checkout_strategy.is_some() || requirements.egress_enforcement.is_some() {
        return Err("its provider requirements constrain more than the instance, which nothing carries after the move".into());
    }
    Ok(())
}

fn host_ref(spec: &PolicySpec) -> &str {
    match (&spec.docker_per_vessel, &spec.host_direct) {
        (Some(docker), _) => &docker.host_ref,
        (None, Some(host)) => &host.host_ref,
        (None, None) => "",
    }
}

/// What a copy's stamps say wrote it, as the engine's kinds read it back, with a snapshot named
/// only by its name stamped as one.
fn stamped(
    name: &str,
    labels: &BTreeMap<String, String>,
    annotations: &BTreeMap<String, String>,
) -> Result<(BTreeMap<String, String>, &'static str), String> {
    let mut labels = labels.clone();
    if is_prepared_snapshot(name, &labels, PLACEMENT_SNAPSHOT_KIND) {
        labels.insert(PREPARED_SNAPSHOT_LABEL.to_owned(), PLACEMENT_SNAPSHOT_KIND.to_owned());
        return Ok((labels, label(&DefinitionOrigin::Snapshot)));
    }
    if labels.get(MANAGED_BY_LABEL).is_some_and(|manager| manager == SEEDING_MANAGER) {
        return Ok((labels, "seeding"));
    }
    let writer = label(&flotilla_resources::definitions::origin(&labels, annotations)?);
    Ok((labels, writer))
}

struct KindEntry {
    namespaces: BTreeSet<String>,
    record: Result<Placed, String>,
}

/// `host_platform` is the platform grant of the host's host-direct kind. A snapshot whose host
/// has none anywhere takes no platform grant: nothing selects a snapshot by its grants.
fn kind_of(spec: &PolicySpec, host_platform: Option<&FulfilmentGrant>, snapshot: bool) -> Result<FulfilmentKindSpec, String> {
    if spec.host_direct.is_none() {
        return spec.kind("linux");
    }
    match host_platform {
        Some(platform) => spec.kind(platform.0.strip_prefix("platform:").unwrap_or(&platform.0)),
        None if snapshot => {
            let mut kind = spec.kind("unknown")?;
            kind.grants.retain(|grant| !grant.0.starts_with("platform:"));
            Ok(kind)
        }
        None => Err("a host-direct policy needs its host's platform, and its host has no host-direct kind".into()),
    }
}

/// The kinds `copies` import to, every home's; what is dropped is counted, and what is refused
/// named, in `report`.
pub fn import_placement(copies: &PlacementCopies, report: &mut ImportReport) -> ImportedPlacement {
    import_placement_at(copies, &|_| true, report)
}

/// [`import_placement`] for the homes `imports` names. Every home's kinds are read, as a snapshot
/// takes its platform from its host's kind wherever that is homed.
fn import_placement_at(copies: &PlacementCopies, imports: &dyn Fn(&str) -> bool, report: &mut ImportReport) -> ImportedPlacement {
    let mut ignored = ImportReport::default();
    let mut imported = ImportedPlacement::default();
    let mut kinds = BTreeMap::<(String, String), KindEntry>::new();
    let mut platforms = BTreeMap::<(String, String), FulfilmentGrant>::new();
    let mut unpinned = Vec::new();
    for copy in &copies.kinds {
        let report = if imports(&copy.home) { &mut *report } else { &mut ignored };
        let metadata = &copy.object.metadata;
        if metadata.deletion_timestamp.is_some() {
            *report.dropped.entry((KINDS, "deleted")).or_default() += 1;
            continue;
        }
        let spec = &copy.object.spec;
        if let Some(instance) = metadata.labels.get(ENVIRONMENT_PROVIDER_INSTANCE_LABEL) {
            unpinned.push((KINDS, metadata.name.clone(), copy.home.clone(), instance.clone()));
        }
        let record = stamped(&metadata.name, &metadata.labels, &metadata.annotations).and_then(|(labels, writer)| {
            Ok(Placed {
                name: metadata.name.clone(),
                home: copy.home.clone(),
                record: FulfilmentKindDefinition::new(spec)?,
                labels,
                annotations: metadata.annotations.clone(),
                created: metadata.creation_timestamp,
                writer,
            })
        });
        if let (Ok(_), FulfilmentRealisation::HostDirect, Some(platform)) =
            (&record, &spec.realisation, spec.grants.iter().find(|grant| grant.0.starts_with("platform:")))
        {
            platforms.insert((copy.home.clone(), spec.host_ref.clone()), platform.clone());
        }
        let entry =
            kinds.entry((metadata.name.clone(), copy.home.clone())).or_insert_with(|| KindEntry { namespaces: BTreeSet::new(), record });
        entry.namespaces.insert(copy.namespace.clone());
    }
    for copy in &copies.policies {
        let report = if imports(&copy.home) { &mut *report } else { &mut ignored };
        let metadata = &copy.object.metadata;
        let name = &metadata.name;
        let spec = &copy.object.spec;
        let refuse =
            |reason: String| ImportRefusal { kind: POLICIES, record: name.clone(), reason: format!("at `{}`: {reason}", copy.home) };
        if metadata.deletion_timestamp.is_some() {
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
        let snapshot = is_prepared_snapshot(name, &metadata.labels, PLACEMENT_SNAPSHOT_KIND);
        let stamps = stamped(name, &metadata.labels, &metadata.annotations).and_then(|stamps| {
            if snapshot {
                Ok(stamps)
            } else {
                requirements(spec).map(|()| stamps)
            }
        });
        let (labels, writer) = match stamps {
            Ok(stamps) => stamps,
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
            if let Some(instance) = spec.provider_requirements.as_ref().and_then(|requirements| requirements.instance.clone()) {
                unpinned.push((POLICIES, name.clone(), copy.home.clone(), instance));
            }
            // A snapshot is often frozen at the admitting home for a host homed elsewhere.
            let host = host_ref(spec);
            let platform = platforms
                .get(&(copy.home.clone(), host.to_owned()))
                .or_else(|| platforms.iter().find(|((_, held), _)| held == host).map(|(_, platform)| platform));
            let record = kind_of(spec, platform, snapshot)
                .and_then(|kind| FulfilmentKindDefinition::new(&kind))
                .map(|record| Placed {
                    name: name.clone(),
                    home: copy.home.clone(),
                    record,
                    labels,
                    annotations: metadata.annotations.clone(),
                    created: metadata.creation_timestamp,
                    writer,
                })
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
        let differing = spec.differing_settings();
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
    for (kind, name, home, instance) in unpinned.into_iter().filter(|(_, _, home, _)| imports(home)) {
        report.notes.push(format!(
            "{kind} `{name}` at `{home}`: its provider instance `{instance}` is not carried; admission chooses among its host's providers"
        ));
    }
    // The engine has no namespaces, so a name held in several is refused in all of them.
    for ((name, home), KindEntry { namespaces, record }) in kinds.into_iter().filter(|((_, home), _)| imports(home)) {
        let refuse = |reason: String| ImportRefusal { kind: KINDS, record: name.clone(), reason: format!("at `{home}`: {reason}") };
        if namespaces.len() > 1 {
            let held: Vec<_> = namespaces.into_iter().collect();
            let also = record.err().map(|reason| format!("; {reason}")).unwrap_or_default();
            report.refused.push(refuse(format!("held in namespaces `{}`, and the engine has no namespaces{also}", held.join("`, `"))));
            continue;
        }
        match record {
            Ok(placed) => imported.kinds.push(placed),
            Err(reason) => report.refused.push(refuse(reason)),
        }
    }
    imported
}

/// Maps `copies` and writes every kind homed where `target` imports, each through the change the
/// engine's kinds read its stamps back from, at its copy's creation time.
pub fn write_placement(copies: &PlacementCopies, target: &mut impl ImportTarget, report: &mut ImportReport) {
    let placed = import_placement_at(copies, &|home| target.imports(home), report).kinds;
    for placed in placed {
        let written = kinds::change_of(&placed.labels, &placed.annotations, &placed.home, Timestamp::from_datetime(&placed.created))
            .and_then(|change| target.write(&placed.home, &placed.name, &change, &placed.record));
        match written {
            Ok(()) => *report.imported.entry((KINDS, placed.writer)).or_default() += 1,
            Err(reason) => report.refused.push(ImportRefusal { kind: KINDS, record: placed.name, reason }),
        }
    }
}

/// [`write_placement`] into a temporary store of its own.
pub fn rehearse_placement(copies: &PlacementCopies) -> ImportReport {
    let mut report = ImportReport::default();
    match stored::registry().map_err(|error| format!("registry: {error:?}")).and_then(flotilla_engine_store::Rehearsal::open) {
        Ok(mut rehearsal) => write_placement(copies, &mut rehearsal, &mut report),
        Err(reason) => report.refused.push(ImportRefusal { kind: KINDS, record: "<all>".to_owned(), reason }),
    }
    report
}
