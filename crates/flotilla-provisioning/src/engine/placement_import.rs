//! Placement's import onto the engine (#3824 layer 4a, ADR 0047).
//!
//! FulfilmentKinds and PlacementPolicies are kept per home on the old store: each home's live copy
//! imports to records at that home. The 2026-10-26 rulings retire PlacementPolicy: each policy
//! becomes a FulfilmentKind of the same name, and the realisation settings only a policy carried
//! become its host's EnvironmentProvider settings.
//!
//! | Old copy | Imported |
//! |---|---|
//! | Deleted | dropped |
//! | FulfilmentKind | 1:1, its origin from its stamps; refused when its image names a CrewImageBaseline |
//! | PlacementPolicy with a kind of its name at its home | that kind stands; the policy gives its settings |
//! | PlacementPolicy with none | a kind of its name: host, pool, cost class, image and the default grants a live policy's kind takes (`FulfilmentKindSpec::from_policy`); a host-direct policy takes its platform from its host's host-direct kind, and is refused without one |
//! | A prepared placement snapshot (`placement-snapshot-*`, `-remote-placement-*`) | a Snapshot-origin kind of its name with its frozen image; its settings dropped, its host's apply |
//! | A live policy's Docker pull and memory policy, cwd, env and checkout; host-direct checkout; a cloud sandbox's size, checkout and env | its host's EnvironmentProvider, one per realisation, named `<host>.<realisation>`; a manifest's declared settings override a seeded or operator policy's; refused when two of one rank set it differently |
//! | `priority` | dropped: admission selects by grants and cost (ADR 0046) |
//! | `agent_adapters` | dropped: an image fact the host probes |
//! | `legacy_image_baseline_ref` | dropped with CrewImageBaseline |
//! | `provider_requirements` | its instance names the settings' provider; any other constraint is refused |
//!
//! The engine has no namespaces (#3212), so one name held in two namespaces at one home is refused.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_engine_store::Rehearsal;
use flotilla_resources::{
    environment_provider::{EnvironmentProviderSpec, ProviderSettings},
    is_prepared_snapshot, FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, PlacementPolicy, PlacementPolicySpec,
    Resource, ResourceObject, PLACEMENT_SNAPSHOT_KIND,
};
use flotilla_schema::Kind;

use super::{
    definition_import::{label, origin},
    import::{ImportRefusal, ImportReport},
};
use crate::stored::placement::{self as stored, DefinitionOrigin, EnvironmentProviderDefinition, FulfilmentKindDefinition};

const KINDS: &str = <FulfilmentKindDefinition as Kind>::KIND;
const PROVIDERS: &str = <EnvironmentProviderDefinition as Kind>::KIND;
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

/// One engine record the import makes: its name, home and value.
pub struct Placed<S> {
    pub name: String,
    pub home: String,
    pub record: S,
}

/// What the import makes of the copies.
#[derive(Default)]
pub struct ImportedPlacement {
    pub kinds: Vec<Placed<FulfilmentKindDefinition>>,
    pub providers: Vec<Placed<EnvironmentProviderDefinition>>,
}

/// A policy's settings for its host's provider, or why it has none to give.
fn settings(spec: &PlacementPolicySpec) -> Result<EnvironmentProviderSpec, String> {
    let requirements = spec.provider_requirements.clone().unwrap_or_default();
    if !requirements.material_modes.is_empty() || requirements.checkout_strategy.is_some() || requirements.egress_enforcement.is_some() {
        return Err("its provider requirements constrain more than the instance, which nothing carries after the move".into());
    }
    let (host_ref, settings) = match (&spec.docker_per_vessel, &spec.host_direct, &spec.cloud_sandbox_per_vessel) {
        (Some(docker), None, None) => (
            docker.host_ref.clone(),
            ProviderSettings::DockerPerVessel {
                pull_policy: docker.pull_policy,
                memory_policy: docker.memory_policy.clone(),
                default_cwd: docker.default_cwd.clone(),
                env: docker.env.clone(),
                checkout: docker.checkout.clone(),
            },
        ),
        (None, Some(host), None) => (host.host_ref.clone(), ProviderSettings::HostDirect { host_checkout: host.checkout.clone() }),
        (None, None, Some(cloud)) => (
            String::new(),
            ProviderSettings::CloudSandboxPerVessel { size: cloud.size.clone(), sandbox_checkout: cloud.checkout, env: cloud.env.clone() },
        ),
        _ => return Err("it defines no single realisation".into()),
    };
    Ok(EnvironmentProviderSpec { host_ref, instance: requirements.instance, settings })
}

/// The host a policy realises on; empty for a cloud sandbox, which is bound to none.
fn host_ref(spec: &PlacementPolicySpec) -> &str {
    match (&spec.docker_per_vessel, &spec.host_direct) {
        (Some(docker), _) => &docker.host_ref,
        (None, Some(host)) => &host.host_ref,
        (None, None) => "",
    }
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

/// Maps the copies onto engine records. Drops are counted and refusals listed in `report`.
pub fn import_placement(copies: &PlacementCopies, report: &mut ImportReport) -> ImportedPlacement {
    let mut imported = ImportedPlacement::default();
    // Every record by (name, home), with the namespaces it came from.
    let mut kinds = BTreeMap::<(String, String), (BTreeSet<String>, Result<FulfilmentKindDefinition, String>)>::new();
    // The platform grant of each home's host-direct kind for each host, which a host-direct policy
    // with no kind of its own takes.
    let mut platforms = BTreeMap::<(String, String), FulfilmentGrant>::new();
    for copy in &copies.kinds {
        if copy.object.metadata.deletion_timestamp.is_some() {
            *report.dropped.entry((KINDS, "deleted")).or_default() += 1;
            continue;
        }
        let spec = &copy.object.spec;
        if let (FulfilmentRealisation::HostDirect, Some(platform)) =
            (&spec.realisation, spec.grants.iter().find(|grant| grant.0.starts_with("platform:")))
        {
            platforms.insert((copy.home.clone(), spec.host_ref.clone()), platform.clone());
        }
        let record = origin(&copy.object.metadata).and_then(|origin| FulfilmentKindDefinition::new(origin, spec));
        let entry = kinds.entry((copy.object.metadata.name.clone(), copy.home.clone())).or_insert_with(|| (BTreeSet::new(), record));
        entry.0.insert(copy.namespace.clone());
    }
    // Each home's settings for each provider, with the policies that set them.
    let mut providers = BTreeMap::<(String, String), Vec<(String, DefinitionOrigin, EnvironmentProviderSpec)>>::new();
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
        let policy_origin = match policy_origin {
            Ok(origin) => origin,
            Err(reason) => {
                report.refused.push(refuse(reason));
                continue;
            }
        };
        // A snapshot's settings are dropped, so nothing in them can refuse it.
        let provider = match (!snapshot).then(|| settings(spec)).transpose() {
            Ok(provider) => provider,
            Err(reason) => {
                report.refused.push(refuse(reason));
                continue;
            }
        };
        let key = (name.clone(), copy.home.clone());
        if let Some((namespaces, _)) = kinds.get_mut(&key) {
            // The kind of its name, which the daemon migrated from it, stands.
            namespaces.insert(copy.namespace.clone());
        } else {
            let host = (copy.home.clone(), host_ref(spec).to_owned());
            let record = kind_of(spec, platforms.get(&host))
                .and_then(|kind| FulfilmentKindDefinition::new(policy_origin.clone(), &kind))
                .map_err(|reason| format!("its kind: {reason}"));
            kinds.insert(key, (BTreeSet::from([copy.namespace.clone()]), record));
        }
        let Some(provider) = provider else {
            *report.dropped.entry((POLICIES, "a snapshot's settings; its host's provider settings apply")).or_default() += 1;
            continue;
        };
        providers.entry((provider.record_name(), copy.home.clone())).or_default().push((name.clone(), policy_origin, provider));
    }
    // The engine has no namespaces: a name a home holds in several is refused in all of them.
    for ((name, home), (namespaces, record)) in kinds {
        let refuse = |reason: String| ImportRefusal { kind: KINDS, record: name.clone(), reason: format!("at `{home}`: {reason}") };
        if namespaces.len() > 1 {
            let held: Vec<_> = namespaces.into_iter().collect();
            report.refused.push(refuse(format!("held in namespaces `{}`, and the engine has no namespaces", held.join("`, `"))));
            continue;
        }
        match record {
            Ok(record) => imported.kinds.push(Placed { name, home, record }),
            Err(reason) => report.refused.push(refuse(reason)),
        }
    }
    for ((name, home), mut set) in providers {
        // A manifest's settings are declared; the daemon's seeded policy and an operator's are
        // defaults the declaration overrides. The record takes the declaring manifest's settings and
        // origin, so its reconciler owns it; else the first policy's by name. Neither depends on the
        // order the copies were listed in.
        let undeclared = |origin: &DefinitionOrigin| !matches!(origin, DefinitionOrigin::Manifest { .. });
        set.sort_by(|(left, left_origin, _), (right, right_origin, _)| {
            (undeclared(left_origin), left).cmp(&(undeclared(right_origin), right))
        });
        let (first, origin, spec) = &set[0];
        let rank = undeclared(origin);
        let overridden: Vec<_> = set
            .iter()
            .filter(|(_, other_origin, other)| undeclared(other_origin) != rank && other != spec)
            .map(|(policy, _, _)| policy.as_str())
            .collect();
        if !overridden.is_empty() {
            report.notes.push(format!(
                "{PROVIDERS} `{name}` at `{home}`: `{first}`'s declared settings override those of `{}`",
                overridden.join("`, `")
            ));
        }
        let differing: Vec<_> = set
            .iter()
            .filter(|(_, other_origin, other)| undeclared(other_origin) == rank && other != spec)
            .map(|(policy, _, _)| policy.as_str())
            .collect();
        if !differing.is_empty() {
            report.refused.push(ImportRefusal {
                kind: PROVIDERS,
                record: name,
                reason: format!("at `{home}`: policies `{first}` and `{}` set this provider differently", differing.join("`, `")),
            });
            continue;
        }
        if set.len() > 1 {
            let policies: Vec<_> = set.iter().map(|(policy, _, _)| policy.as_str()).collect();
            let origins =
                if set.iter().any(|(_, other, _)| other != origin) { format!("; it takes `{first}`'s origin") } else { String::new() };
            report.notes.push(format!("{PROVIDERS} `{name}` at `{home}`: set alike by policies `{}`{origins}", policies.join("`, `")));
        }
        imported.providers.push(Placed { name, home, record: EnvironmentProviderDefinition::new(origin.clone(), spec) });
    }
    imported
}

/// Maps every copy onto the engine, restores each record into a temporary store holding
/// placement's kinds, reads it back, and reports counts by kind and origin, the drops by kind and
/// reason, and each refusal.
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
            Ok(()) => *report.imported.entry((KINDS, label(&placed.record.origin))).or_default() += 1,
            Err(reason) => report.refused.push(ImportRefusal { kind: KINDS, record: placed.name.clone(), reason }),
        }
    }
    for placed in &imported.providers {
        match rehearsal.restore(&placed.home, &placed.name, 1, &placed.record) {
            Ok(()) => *report.imported.entry((PROVIDERS, label(&placed.record.origin))).or_default() += 1,
            Err(reason) => report.refused.push(ImportRefusal { kind: PROVIDERS, record: placed.name.clone(), reason }),
        }
    }
    report
}
