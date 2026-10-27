//! Placement's import onto the engine (#3824 layer 4a, ADR 0047).
//!
//! FulfilmentKinds and PlacementPolicies are kept per home on the old store: each home's live copy
//! imports to a record at that home. The 2026-10-26 rulings retire PlacementPolicy: each policy
//! becomes a FulfilmentKind of the same name. The settings only a policy carried are code constants
//! (the 2026-10-27 ruling, [`realisation_settings`]), so none is carried; a policy whose settings
//! differ from them is named, and each Docker policy's memory share is reported for its host's
//! capacity (layer 4c).
//!
//! | Old copy | Imported |
//! |---|---|
//! | Deleted | dropped |
//! | FulfilmentKind | 1:1, its origin from its stamps as the import's change; refused when its image names a CrewImageBaseline |
//! | PlacementPolicy with a kind of its name at its home | that kind stands |
//! | PlacementPolicy with none | a kind of its name: host, pool, cost class, image and the default grants a live policy's kind takes (`FulfilmentKindSpec::from_policy`); a host-direct policy takes its platform from its host's host-direct kind, and is refused without one |
//! | A prepared placement snapshot (`placement-snapshot-*`, `-remote-placement-*`) | a kind of its name with its frozen image, written by admission |
//! | A Docker policy's checkout, cwd, env and pull policy; a host-direct policy's checkout | dropped: code constants; a value other than the constant is noted |
//! | A Docker policy's memory share | dropped, noted per host: it becomes the Host's capacity (layer 4c) |
//! | A cloud sandbox's size, checkout and env | dropped (the ruling) |
//! | `priority` | dropped: admission selects by grants and cost (ADR 0046) |
//! | `agent_adapters` | dropped: an image fact the host probes |
//! | `legacy_image_baseline_ref` | dropped with CrewImageBaseline |
//! | `provider_requirements` | its instance is the provider's identifier, which the realisation names; any other constraint is refused |
//!
//! The engine has no namespaces (#3212), so one name held in two namespaces at one home is refused.

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

/// One old copy: its namespace, the object, and the home that wrote it.
pub struct PlacementCopy<R: Resource> {
    pub namespace: String,
    pub object: ResourceObject<R>,
    pub home: String,
}

/// The old copies the import carries.
#[derive(Default)]
pub struct PlacementCopies {
    pub kinds: Vec<PlacementCopy<FulfilmentKind>>,
    pub policies: Vec<PlacementCopy<PlacementPolicy>>,
}

/// One engine record the import makes: its name, home and value, and why its copy was written,
/// the import's change for it.
pub struct Placed<S> {
    pub name: String,
    pub home: String,
    pub record: S,
    pub origin: DefinitionOrigin,
}

/// What the import makes of the copies.
#[derive(Default)]
pub struct ImportedPlacement {
    pub kinds: Vec<Placed<FulfilmentKindDefinition>>,
}

/// Refuses a policy whose provider requirements constrain more than the instance: nothing carries
/// such a constraint after the move.
fn requirements(spec: &PlacementPolicySpec) -> Result<(), String> {
    let requirements = spec.provider_requirements.clone().unwrap_or_default();
    if !requirements.material_modes.is_empty() || requirements.checkout_strategy.is_some() || requirements.egress_enforcement.is_some() {
        return Err("its provider requirements constrain more than the instance, which nothing carries after the move".into());
    }
    Ok(())
}

/// The host a policy realises on; empty for a cloud sandbox, which is bound to none.
fn host_ref(spec: &PlacementPolicySpec) -> &str {
    match (&spec.docker_per_vessel, &spec.host_direct) {
        (Some(docker), _) => &docker.host_ref,
        (None, Some(host)) => &host.host_ref,
        (None, None) => "",
    }
}

/// One kind's record at one home, with the namespaces its copies came from.
struct KindEntry {
    namespaces: BTreeSet<String>,
    record: Result<(FulfilmentKindDefinition, DefinitionOrigin), String>,
}

/// The kind a policy with none of its name becomes. `host_platform` is the platform grant of its
/// host's host-direct kind, which a host-direct policy needs.
fn kind_of(spec: &PlacementPolicySpec, host_platform: Option<&FulfilmentGrant>) -> Result<FulfilmentKindSpec, String> {
    if spec.host_direct.is_some() {
        let platform = host_platform.ok_or("a host-direct policy needs its host's platform, and its host has no host-direct kind")?;
        let platform = platform.0.strip_prefix("platform:").unwrap_or(&platform.0);
        return FulfilmentKindSpec::from_policy(spec, platform);
    }
    // Only a host-direct kind takes the host's platform; the others are Linux.
    FulfilmentKindSpec::from_policy(spec, "linux")
}

/// Maps the copies onto engine records. Drops are counted, refusals listed and the settings a
/// policy held that the constants do not noted in `report`.
pub fn import_placement(copies: &PlacementCopies, report: &mut ImportReport) -> ImportedPlacement {
    let mut imported = ImportedPlacement::default();
    // Every record by (name, home), with the namespaces it came from.
    let mut kinds = BTreeMap::<(String, String), KindEntry>::new();
    // The platform grant of each home's host-direct kind for each host, which a host-direct policy
    // with no kind of its own takes.
    let mut platforms = BTreeMap::<(String, String), FulfilmentGrant>::new();
    for copy in &copies.kinds {
        if copy.object.metadata.deletion_timestamp.is_some() {
            *report.dropped.entry((KINDS, "deleted")).or_default() += 1;
            continue;
        }
        let spec = &copy.object.spec;
        let record = origin(&copy.object.metadata).and_then(|origin| Ok((FulfilmentKindDefinition::new(spec)?, origin)));
        // Only a kind that imports lends its host's platform.
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
    // Each home's memory shares by host, with the policies that set them, for the Host capacity.
    let mut shares = BTreeMap::<(String, String), BTreeMap<String, Vec<String>>>::new();
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
        // A snapshot's settings and requirements are dropped with it, so nothing in them refuses it.
        let policy_origin = match policy_origin.and_then(|origin| if snapshot { Ok(origin) } else { requirements(spec).map(|()| origin) }) {
            Ok(origin) => origin,
            Err(reason) => {
                report.refused.push(refuse(reason));
                continue;
            }
        };
        let key = (name.clone(), copy.home.clone());
        if let Some(entry) = kinds.get_mut(&key) {
            // The kind of its name, which the daemon migrated from it, stands.
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
            // A snapshot froze its image; the settings it copied are its host's, the constants.
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
        if let Some(docker) = &spec.docker_per_vessel {
            let memory = &docker.memory_policy;
            let share = format!(
                "{}% of host memory over {} crews, {} bytes of swap each",
                memory.host_memory_percent, memory.expected_concurrent_crews, memory.swap_bytes
            );
            shares.entry((copy.home.clone(), docker.host_ref.clone())).or_default().entry(share).or_default().push(name.clone());
        }
    }
    // The memory share is the Host's capacity in layer 4c: one value per host, so a host whose
    // policies set several is named.
    for ((home, host), by_share) in shares {
        let each: Vec<String> = by_share.iter().map(|(share, policies)| format!("{share} (`{}`)", policies.join("`, `"))).collect();
        let several = if by_share.len() > 1 { "several memory shares, one to choose for its capacity" } else { "memory share" };
        report.notes.push(format!("host `{host}` at `{home}`: {several}: {}", each.join("; ")));
    }
    // The engine has no namespaces: a name a home holds in several is refused in all of them.
    for ((name, home), KindEntry { namespaces, record }) in kinds {
        let refuse = |reason: String| ImportRefusal { kind: KINDS, record: name.clone(), reason: format!("at `{home}`: {reason}") };
        if namespaces.len() > 1 {
            let held: Vec<_> = namespaces.into_iter().collect();
            report.refused.push(refuse(format!("held in namespaces `{}`, and the engine has no namespaces", held.join("`, `"))));
            continue;
        }
        match record {
            Ok((record, origin)) => imported.kinds.push(Placed { name, home, record, origin }),
            Err(reason) => report.refused.push(refuse(reason)),
        }
    }
    imported
}

/// Maps every copy onto the engine, restores each record into a temporary store holding
/// placement's kind, reads it back, and reports counts by origin, the drops by kind and reason,
/// each refusal, and the notes.
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
    // Version 1: each record starts at its home as a fresh write; its old versions are not carried.
    for placed in &imported.kinds {
        match rehearsal.restore(&placed.home, &placed.name, 1, &placed.record) {
            Ok(()) => *report.imported.entry((KINDS, label(&placed.origin))).or_default() += 1,
            Err(reason) => report.refused.push(ImportRefusal { kind: KINDS, record: placed.name.clone(), reason }),
        }
    }
    report
}
