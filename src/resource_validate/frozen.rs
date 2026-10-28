//! Candidate-side validation of durable admission pins. Transport only supplies
//! an inventory; the checker and probe ports do not depend on a running fleet.
#[cfg(test)]
use flotilla_store_memory::InMemoryBackend;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use color_eyre::{eyre::eyre, Result};
use flotilla_environment::environment::EnvironmentProvider;
#[cfg(test)]
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;
use flotilla_resources::{
    pinned_workflow_ref, Convoy, ConvoyPhase, CredentialSpec, CredentialSpecSpec, Environment, ImageBuild, ImageBuildPhase,
    InputDefinition, K8sResourceObject, Resource, ResourceObject, SkillCatalogEntry, Vessel, WorkflowTemplateSpec,
    IMAGE_DIGESTS_CAPABILITY,
};
use serde::Serialize;
use serde_json::Value;

/// A reason is required; accidental empty/boolean annotations never waive a gate.
/// Operators set this through `resource apply` on the Convoy metadata. It records
/// their commitment to abandon and freshly admit this generation after the roll.
pub(super) const READMISSION_ANNOTATION: &str = "flotilla.work/pre-roll-re-admission";

pub(super) trait Probes {
    async fn skills(
        &self,
        selected: &BTreeMap<String, Vec<SkillCatalogEntry>>,
        credentials: &BTreeMap<String, CredentialSpecSpec>,
    ) -> Result<Vec<String>, String>;
    async fn image(&self, reference: &str) -> Result<(), String>;
}

pub(super) struct CandidateProbes<'a> {
    pub options: &'a super::ValidationOptions,
    pub inventory: &'a [Value],
    pub runner: std::sync::Arc<dyn CommandRunner>,
    pub provider: &'a dyn EnvironmentProvider,
}

impl Probes for CandidateProbes<'_> {
    /// Each crew is fetched and checked as at launch, on this host, with the
    /// operator's token files; crews of one vessel at different revisions of
    /// one source are permitted and returned as notices.
    async fn skills(
        &self,
        selected: &BTreeMap<String, Vec<SkillCatalogEntry>>,
        credentials: &BTreeMap<String, CredentialSpecSpec>,
    ) -> Result<Vec<String>, String> {
        if selected.is_empty() {
            return Ok(Vec::new());
        }
        let source = self.options.sources.as_deref().ok_or("frozen skills require --skill-sources (candidate supply directory)")?;
        // A fresh private directory per run (0700, removed on drop): a fixed name under the
        // shared temporary directory could be pre-created by another user, whose Git config
        // there would then run as the operator.
        let cache = tempfile::Builder::new()
            .prefix("flotilla-skill-probe-")
            .tempdir()
            .map_err(|error| format!("create the skill probe's object cache: {error}"))?;
        let trees = flotilla_daemon::SkillTrees::new(
            std::sync::Arc::new(flotilla_vcs::vcs::FlotillaVcs::object_cache(std::sync::Arc::clone(&self.runner))),
            cache.path().join("objects"),
        );
        flotilla_daemon::validate_frozen_vessel_skills(&trees, source, selected, credentials, &self.options.credential_tokens).await
    }

    async fn image(&self, reference: &str) -> Result<(), String> {
        // This fleet-wide probe accepts evidence from any named cache. Placement
        // separately checks the exact host/provider-instance identity.
        let digest = reference.rsplit_once('@').map_or(reference, |(_, digest)| digest);
        for host in self.inventory.iter().filter(|document| document["kind"] == "Host") {
            let inventory = host.pointer(&format!("/status/capabilities/{IMAGE_DIGESTS_CAPABILITY}"));
            if inventory
                .and_then(|value| serde_json::from_value::<flotilla_resources::LocalImageInventories>(value.clone()).ok())
                .is_some_and(|inventory| inventory.0.values().any(|held| held.contains(reference) || held.contains(digest)))
                // ADR 0047: remove one roll after B1. Old host-only evidence can
                // establish fleet-wide existence, but never a specific cache.
                || inventory.and_then(Value::as_array).is_some_and(|held| held.iter().any(|value| value == reference || value == digest))
            {
                return Ok(());
            }
        }
        // Composed images are referenced by their build's digest directly; an
        // exact registry manifest is checked below. Retired ADR 0053 stage
        // builds carry no availability evidence.
        let mut registries = BTreeSet::new();
        if reference.contains('@') && flotilla_resources::is_image_digest(digest) {
            registries.insert(reference.to_string());
        }
        let mut errors = Vec::new();
        for registry in registries {
            // A location alone is not evidence. Probe with the operator host's
            // configured Docker registry credentials, without building/pulling.
            match self.provider.local_image_cache().ok_or("validation provider has no image cache")?.registry_available(&registry).await {
                Ok(_) => return Ok(()),
                Err(error) => errors.push(format!("registry manifest {registry}: {error}")),
            }
        }
        // Exact identities use the host observations checked above, then exact
        // registry manifests. Local Docker inspection below is only a legacy
        // tag fallback: it cannot prove availability on another fleet host.
        if !errors.is_empty() {
            return Err(format!("image {reference} absent from host digest inventories; {}", errors.join("; ")));
        }
        if !flotilla_resources::is_image_digest(reference) && !reference.contains('@') {
            let cache = self.provider.local_image_cache().ok_or("validation provider has no image cache")?;
            if cache.inspect_reference(reference).await.is_ok_and(|image| image.is_some()) {
                return Ok(());
            }
            return cache
                .registry_available(reference)
                .await
                .map_err(|error| format!("image {reference} unavailable locally and in the registry: {error}"));
        }
        Err(format!("image {reference} is absent from host digest inventories and has no exact registry reference"))
    }
}

#[derive(Debug, Default, Serialize)]
pub(super) struct CategoryCount {
    checked: usize,
    unsatisfied: usize,
    waived: usize,
}

#[derive(Debug, Default, Serialize)]
pub(super) struct Report {
    pub inventory_complete: bool,
    live_convoys: usize,
    re_admission: usize,
    skills: CategoryCount,
    workflow: CategoryCount,
    images: CategoryCount,
    grants: CategoryCount,
    pub failures: Vec<String>,
    pub waivers: Vec<String>,
    /// Satisfiable but worth the operator's attention; never a failure.
    pub notices: Vec<String>,
    validated_elsewhere: Vec<String>,
}

#[derive(Clone, Copy)]
enum Category {
    Skills,
    Workflow,
    Images,
    Grants,
}

impl std::fmt::Display for Category {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Skills => "skills",
            Self::Workflow => "workflow",
            Self::Images => "images",
            Self::Grants => "grants",
        })
    }
}

impl Report {
    fn record(&mut self, category: Category, convoy: &str, reference: &str, result: Result<(), String>, waiver: Option<&str>) {
        let count = match category {
            Category::Skills => &mut self.skills,
            Category::Workflow => &mut self.workflow,
            Category::Images => &mut self.images,
            Category::Grants => &mut self.grants,
        };
        count.checked += 1;
        if let Err(error) = result {
            count.unsatisfied += 1;
            let message = format!("Convoy/{convoy} {category} {reference}: {error}");
            if let Some(reason) = waiver {
                count.waived += 1;
                self.waivers.push(format!("{message}; re-admission: {reason}"));
            } else {
                self.failures.push(message);
            }
        }
    }
}

fn decode<T: Resource>(document: &Value) -> Result<ResourceObject<T>> {
    Ok(ResourceObject::from_k8s_object(serde_json::from_value::<K8sResourceObject<T>>(document.clone())?)?)
}

// Routing/diagnostics only: typed metadata requires a namespace and has no
// default. Keep absent metadata visibly invalid rather than inventing a scope.
fn namespace(document: &Value) -> &str {
    document["metadata"]["namespace"].as_str().unwrap_or("<missing namespace>")
}

fn name(document: &Value) -> &str {
    document["metadata"]["name"].as_str().unwrap_or("<unnamed>")
}

pub(super) async fn check(inventory: &[Value], retired: &BTreeSet<(String, String)>, probes: &impl Probes) -> Result<Report> {
    let mut report = Report { inventory_complete: true, ..Report::default() };
    for document in inventory.iter().filter(|document| document["kind"] == "Convoy") {
        let convoy = decode::<Convoy>(document).map_err(|error| eyre!("Convoy/{}/{}: {error:#}", namespace(document), name(document)))?;
        if convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            continue;
        }
        report.live_convoys += 1;
        let ns = &convoy.metadata.namespace;
        let identity = format!("{ns}/{}", convoy.metadata.name);
        let waiver = convoy.metadata.annotations.get(READMISSION_ANNOTATION).map(String::as_str).filter(|reason| !reason.trim().is_empty());
        report.re_admission += usize::from(waiver.is_some());
        let credentials = inventory
            .iter()
            .filter(|document| document["kind"] == "CredentialSpec" && namespace(document) == ns)
            .map(|document| decode::<CredentialSpec>(document).map(|object| (object.metadata.name, object.spec)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let workflow_ref = pinned_workflow_ref(&convoy);
        if retired.contains(&(ns.clone(), workflow_ref.to_string())) {
            report.record(Category::Workflow, &identity, workflow_ref, Err("candidate startup will tombstone this builtin".into()), waiver);
        }
        let snapshot = convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref());
        if let Some(snapshot) = snapshot {
            let spec = WorkflowTemplateSpec::builder()
                .inputs(convoy.spec.inputs.keys().map(|name| InputDefinition { name: name.clone(), description: None }).collect())
                .vessels(snapshot.vessels.clone())
                .turn_delivery(snapshot.turn_delivery.clone())
                .stall_nudges(snapshot.stall_nudges.clone())
                .maybe_exit(snapshot.exit.clone())
                .maybe_supervision(snapshot.supervision.clone())
                .build();
            report.record(
                Category::Workflow,
                &identity,
                workflow_ref,
                flotilla_resources::validate(&spec).map_err(|errors| format!("snapshot is not executable: {errors:?}")),
                waiver,
            );
            for vessel in &snapshot.vessels {
                let granted = vessel
                    .credential_refs
                    .iter()
                    .chain(vessel.credential_scopes.keys())
                    .chain(vessel.credential_permissions.keys())
                    .collect::<BTreeSet<_>>();
                for credential in granted {
                    report.record(
                        Category::Grants,
                        &identity,
                        &format!("{}/{}", vessel.name, credential),
                        credentials
                            .contains_key(credential)
                            .then_some(())
                            .ok_or_else(|| "frozen credential grant has no declared CredentialSpec".into()),
                        waiver,
                    );
                }
                let crews = vessel
                    .crew
                    .iter()
                    .filter(|crew| !crew.skills.selected.is_empty())
                    .map(|crew| (crew.role.clone(), crew.skills.selected.clone()))
                    .collect::<BTreeMap<_, _>>();
                if !crews.is_empty() {
                    let refs = crews
                        .iter()
                        .flat_map(|(crew, selected)| {
                            selected.iter().map(move |entry| format!("{crew}/{}:{}@{}", entry.source, entry.path, entry.revision))
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let result = probes.skills(&crews, &credentials).await.map(|notices| {
                        report.notices.extend(notices.into_iter().map(|notice| format!("Convoy/{identity} {}: {notice}", vessel.name)));
                    });
                    report.record(Category::Skills, &identity, &format!("{} [{refs}]", vessel.name), result, waiver);
                }
            }
        } else if convoy.status.as_ref().is_some_and(|status| status.phase != ConvoyPhase::Pending) {
            report.record(
                Category::Workflow,
                &identity,
                workflow_ref,
                Err("live admitted convoy has no frozen workflow snapshot".into()),
                waiver,
            );
        } else {
            // Pending admissions still depend on a live definition. They have
            // no snapshot yet, so confirm the candidate can admit the workflow.
            let canonical = flotilla_resources::current_builtin_workflow_name(workflow_ref);
            let template = inventory
                .iter()
                .find(|document| document["kind"] == "WorkflowTemplate" && namespace(document) == ns && name(document) == workflow_ref)
                .or_else(|| {
                    inventory
                        .iter()
                        .find(|document| document["kind"] == "WorkflowTemplate" && namespace(document) == ns && name(document) == canonical)
                });
            let result = template.ok_or_else(|| "pending workflow definition is unavailable".to_string()).and_then(|template| {
                let spec: WorkflowTemplateSpec = serde_json::from_value(template["spec"].clone()).map_err(|error| error.to_string())?;
                flotilla_resources::validate(&spec).map_err(|errors| format!("workflow is not executable: {errors:?}"))
            });
            report.record(Category::Workflow, &identity, workflow_ref, result, waiver);
        }
        // ADR 0062: the retired `flotilla.work/frozen-image-layers` annotation
        // is ignored; vessels record their composed identity and digest.
        for vessel in inventory.iter().filter(|document| {
            document["kind"] == "Vessel" && namespace(document) == ns && document["spec"]["convoy_ref"] == convoy.metadata.name
        }) {
            let vessel = match decode::<Vessel>(vessel) {
                Ok(vessel) => vessel,
                Err(error) => {
                    // Decode failures are inventory failures, never semantic waivers.
                    report.inventory_complete = false;
                    report.failures.push(format!("Convoy/{identity} Vessel/{ns}/{}: cannot decode frozen vessel: {error:#}", name(vessel)));
                    continue;
                }
            };
            let Some(status) = &vessel.status else {
                continue;
            };
            for credential in status.held_credentials.keys() {
                report.record(
                    Category::Grants,
                    &identity,
                    &format!("{}/held/{credential}", vessel.spec.vessel_name),
                    credentials
                        .contains_key(credential)
                        .then_some(())
                        .ok_or_else(|| "held landing credential has no declared CredentialSpec".into()),
                    waiver,
                );
            }
            // Environment records are host-local. A present Environment remains
            // subject to every existing check, even if a stale placement disagrees.
            let environment_is_local = status.environment_ref.as_ref().is_some_and(|reference| {
                inventory
                    .iter()
                    .any(|document| document["kind"] == "Environment" && namespace(document) == ns && name(document) == reference)
            });
            if !environment_is_local {
                if let Some(host) = remote_placement_host(inventory, &convoy, &vessel) {
                    report.validated_elsewhere.push(format!(
                        "Convoy/{identity} images Vessel/{ns}/{}: validated on the host that holds it ({host})",
                        vessel.metadata.name
                    ));
                    continue;
                }
            }
            if let Some(environment_ref) = &status.environment_ref {
                let environment = inventory
                    .iter()
                    .find(|document| document["kind"] == "Environment" && namespace(document) == ns && name(document) == environment_ref);
                if let Some(environment) = environment {
                    let environment = decode::<Environment>(environment)?;
                    if let Some(docker) = &environment.spec.docker {
                        let build = composed_build(inventory, ns, docker.image_build_ref.as_deref())?;
                        if let ComposedBuild::Current(build_ref, build) = &build {
                            report.record(Category::Images, &identity, build_ref, check_build(probes, build).await, waiver);
                        } else if let ComposedBuild::Missing(build_ref) = &build {
                            report.record(Category::Images, &identity, build_ref, Err("frozen image build is unavailable".into()), waiver);
                        } else if let Some(local) = &status.local_image_id {
                            report.record(
                                Category::Images,
                                &identity,
                                status.registry_digest.as_deref().unwrap_or(local),
                                check_identity(probes, local, status.registry_digest.as_deref()).await,
                                waiver,
                            );
                        } else {
                            let reference = status.registry_digest.as_deref().unwrap_or(&docker.image);
                            report.record(Category::Images, &identity, reference, probes.image(reference).await, waiver);
                        }
                    }
                } else {
                    report.record(
                        Category::Images,
                        &identity,
                        environment_ref,
                        Err("vessel references a frozen Environment that is missing from the inventory".into()),
                        waiver,
                    );
                }
            // No environment_ref is distinct from an unresolved reference:
            // retained landing vessels can still carry a placed image identity.
            } else if let Some(local) = &status.local_image_id {
                report.record(
                    Category::Images,
                    &identity,
                    status.registry_digest.as_deref().unwrap_or(local),
                    check_identity(probes, local, status.registry_digest.as_deref()).await,
                    waiver,
                );
            } else if let Some(registry) = &status.registry_digest {
                report.record(Category::Images, &identity, registry, probes.image(registry).await, waiver);
            }
        }
    }
    Ok(report)
}

// Placement, not the Vessel's authoring root, identifies the execution host:
// an admitting daemon may author an intent projected into a remote actuator.
// flotilla-resources' registry::read_object_value owns the merged read-view
// invariant: it strips authored origin annotations and supplies replica provenance
// itself (the key is defined by replica::ORIGIN_ROOT_ANNOTATION in that crate).
// Missing evidence never turns a refusal into a skip.
fn remote_placement_host(inventory: &[Value], convoy: &ResourceObject<Convoy>, vessel: &ResourceObject<Vessel>) -> Option<String> {
    let host = vessel
        .status
        .as_ref()
        .and_then(|status| status.placement_decision.as_ref())
        .map(|decision| decision.target_host.reference.clone())
        .or_else(|| {
            flotilla_resources::vessel_placement_pin(convoy, &vessel.spec.vessel_name).map(|pin| pin.decision.target_host.reference)
        })
        .or_else(|| {
            convoy
                .status
                .as_ref()
                .and_then(|status| status.placement_decision.as_ref())
                .map(|decision| decision.target_host.reference.clone())
        })?;
    inventory
        .iter()
        .find(|document| document["kind"] == "Host" && namespace(document) == convoy.metadata.namespace && name(document) == host.as_str())
        .filter(|document| document["metadata"]["annotations"]["flotilla.work/origin-root"].as_str().is_some_and(|root| !root.is_empty()))
        .map(|_| host.to_string())
}

async fn check_identity(probes: &impl Probes, local: &str, registry: Option<&str>) -> Result<(), String> {
    match probes.image(local).await {
        Ok(()) => Ok(()),
        Err(local_error) => match registry {
            Some(registry) => probes.image(registry).await.map_err(|error| format!("{local_error}; {error}")),
            None => Err(local_error),
        },
    }
}

enum ComposedBuild {
    /// Not a composed Environment, or a retired ADR 0053 stage build: the
    /// Environment's placed identity is checked instead.
    None,
    /// The named build is absent from the inventory: a refusal.
    Missing(String),
    Current(String, Box<ResourceObject<ImageBuild>>),
}

/// The per-identity ImageBuild a composed Environment names.
fn composed_build(inventory: &[Value], namespace_name: &str, build_ref: Option<&str>) -> Result<ComposedBuild> {
    let Some(build_ref) = build_ref else { return Ok(ComposedBuild::None) };
    let Some(document) = inventory
        .iter()
        .find(|document| document["kind"] == "ImageBuild" && namespace(document) == namespace_name && name(document) == build_ref)
    else {
        return Ok(ComposedBuild::Missing(build_ref.to_string()));
    };
    let build = decode::<ImageBuild>(document)?;
    Ok(if build.spec.current().is_some() { ComposedBuild::Current(build_ref.to_string(), Box::new(build)) } else { ComposedBuild::None })
}

/// A Built image's digest must be available; a queued or building identity is
/// satisfiable by the candidate's build service; a failed build is not.
async fn check_build(probes: &impl Probes, build: &ResourceObject<ImageBuild>) -> Result<(), String> {
    let Some(status) = &build.status else { return Ok(()) };
    match (status.phase, &status.digest, &status.failure) {
        (ImageBuildPhase::Built, Some(digest), _) => probes.image(digest).await,
        (ImageBuildPhase::Built, None, _) => Err("built image build has no digest".into()),
        (ImageBuildPhase::Failed, _, failure) => {
            Err(format!("image build failed: {}", failure.as_ref().map_or("no reason recorded", |failure| failure.reason.as_str())))
        }
        (ImageBuildPhase::Queued | ImageBuildPhase::Building, _, _) => Ok(()),
    }
}

pub(crate) fn load_tokens(path: Option<&Path>) -> Result<BTreeMap<String, PathBuf>> {
    use std::io::Read;
    const MAX_TOKEN_MAP_BYTES: u64 = 1024 * 1024;
    let Some(path) = path else { return Ok(BTreeMap::new()) };
    let file = std::fs::File::open(path).map_err(|error| eyre!("read probe token map {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_TOKEN_MAP_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| eyre!("read probe token map {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_TOKEN_MAP_BYTES {
        return Err(eyre!("probe token map {} exceeds the 1 MiB limit", path.display()));
    }
    serde_json::from_slice(&bytes).map_err(|error| eyre!("decode probe token map {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use flotilla_resources::{
        ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, InputMeta, ResolvedSkills, VesselRequirement, WorkflowSnapshot,
    };
    use flotilla_store::ResourceBackend;

    use super::*;

    // Operator map errors name their input; bounded reads accept the limit and
    // reject larger files while retaining optional and empty-map behavior.
    #[test]
    fn token_map_errors_name_the_path_and_bound_reads() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("probe map.json");
        assert!(load_tokens(None).expect("optional").is_empty());
        assert!(load_tokens(Some(&path)).expect_err("missing map").to_string().contains(&path.display().to_string()));
        std::fs::write(&path, b"invalid JSON").expect("malformed map");
        assert!(load_tokens(Some(&path)).expect_err("decode map").to_string().contains(&path.display().to_string()));
        let mut bytes = vec![b' '; 1024 * 1024];
        bytes[..2].copy_from_slice(b"{}");
        std::fs::write(&path, &bytes).expect("boundary map");
        assert!(load_tokens(Some(&path)).expect("at limit").is_empty());
        bytes.push(b' ');
        std::fs::write(&path, &bytes).expect("oversized map");
        let error = load_tokens(Some(&path)).expect_err("over limit").to_string();
        assert!(error.contains(&path.display().to_string()) && error.contains("1 MiB"));
        std::fs::write(&path, br#"{"bot":"/private/scoped-token"}"#).expect("map");
        assert_eq!(load_tokens(Some(&path)).expect("valid map")["bot"], PathBuf::from("/private/scoped-token"));
    }

    // Stands in for Git fetch and registry/cache lookup, the two external
    // boundaries. The durable inventory comes from a real in-memory backend.
    struct Supply {
        revision: String,
        image_available: bool,
    }

    impl Probes for Supply {
        async fn skills(
            &self,
            selected: &BTreeMap<String, Vec<SkillCatalogEntry>>,
            _credentials: &BTreeMap<String, CredentialSpecSpec>,
        ) -> Result<Vec<String>, String> {
            if selected.values().flatten().all(|entry| entry.revision == self.revision) {
                Ok(Vec::new())
            } else {
                Err("pinned revision does not exist".into())
            }
        }
        async fn image(&self, _reference: &str) -> Result<(), String> {
            self.image_available.then_some(()).ok_or_else(|| "digest unavailable".into())
        }
    }

    async fn frozen_store() -> (ResourceBackend, Vec<Value>) {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let convoys = backend.clone().using::<Convoy>("fleet");
        let created = convoys
            .create(
                &InputMeta::builder().name("governor".into()).build(),
                &ConvoySpec::builder().workflow_ref("single-agent".into()).build(),
            )
            .await
            .expect("convoy");
        let selected = SkillCatalogEntry::builder()
            .source("sdlc".into())
            .repository("owner/skills".into())
            .revision("1".repeat(40))
            .name("research".into())
            .path("skills/research".into())
            .build();
        let crew = CrewSpec::builder()
            .role("governor".into())
            .source(CrewSource::Tool { command: "true".into() })
            .skills(ResolvedSkills { selected: vec![selected], provenance: vec![] })
            .build();
        let status = ConvoyStatus {
            phase: ConvoyPhase::Active,
            workflow_snapshot: Some(WorkflowSnapshot {
                cascade: None,
                exit: None,
                turn_delivery: Default::default(),
                stall_nudges: Default::default(),
                supervision: None,
                vessels: vec![VesselRequirement::builder().name("work".into()).crew(vec![crew]).build()],
            }),
            ..ConvoyStatus::default()
        };
        convoys.update_status("governor", &created.metadata.resource_version, &status).await.expect("freeze admission");
        let inventory = convoys
            .list()
            .await
            .expect("list")
            .items
            .iter()
            .map(|convoy| serde_json::to_value(convoy.to_k8s_object()).expect("document"))
            .collect();
        (backend, inventory)
    }

    // #2875: a generation change that cannot fetch a governor's old pin refuses
    // the roll, even if the new generation's own skill revision is available.
    #[tokio::test]
    async fn missing_frozen_skill_revision_refuses_and_satisfiable_store_passes() {
        let (_, inventory) = frozen_store().await;
        flotilla_store::validate_resource_document(&inventory[0]).expect("the previous decode-only gate accepts this frozen pin");
        let good = check(&inventory, &BTreeSet::new(), &Supply { revision: "1".repeat(40), image_available: true }).await.expect("check");
        assert!(good.failures.is_empty(), "{:?}", good.failures);
        assert_eq!(good.live_convoys, 1);
        assert_eq!(good.skills.checked, 1);
        let bad = check(&inventory, &BTreeSet::new(), &Supply { revision: "2".repeat(40), image_available: true }).await.expect("check");
        assert_eq!(bad.skills.unsatisfied, 1);
        assert_eq!(bad.failures.len(), 1);
        assert!(bad.failures[0].contains("Convoy/fleet/governor") && bad.failures[0].contains(&"1".repeat(40)));
    }

    // An explicit reason-bearing marker waives the frozen-reference gate only;
    // blank markers do not waive anything and the unmet references stay visible.
    #[tokio::test]
    async fn readmission_marker_waives_and_reports_each_unsatisfied_reference() {
        let (_, mut inventory) = frozen_store().await;
        let supply = Supply { revision: "2".repeat(40), image_available: false };
        inventory[0]["metadata"]["annotations"][READMISSION_ANNOTATION] = Value::String(" ".into());
        assert_eq!(check(&inventory, &BTreeSet::new(), &supply).await.expect("check").failures.len(), 1);
        inventory[0]["metadata"]["annotations"][READMISSION_ANNOTATION] = Value::String("owner will re-admit after roll".into());
        let report = check(&inventory, &BTreeSet::new(), &supply).await.expect("check");
        assert!(report.failures.is_empty());
        assert_eq!(report.skills.waived, 1);
        assert!(report.waivers[0].contains("owner will re-admit"));
    }

    // A decodable frozen snapshot cannot keep a startup-tombstoned builtin
    // executable. A reference in another namespace is independent.
    #[tokio::test]
    async fn live_snapshot_refusing_retired_builtin_is_namespace_scoped() {
        let (_, inventory) = frozen_store().await;
        let supply = Supply { revision: "1".repeat(40), image_available: true };
        let retired = BTreeSet::from([("fleet".into(), "single-agent".into())]);
        let report = check(&inventory, &retired, &supply).await.expect("check");
        assert_eq!(report.workflow.unsatisfied, 1);
        assert!(report.failures[0].contains("single-agent") && report.failures[0].contains("tombstone"));
        assert!(check(&inventory, &BTreeSet::from([("elsewhere".into(), "single-agent".into())]), &supply)
            .await
            .expect("check")
            .failures
            .is_empty());
    }

    // Pending records without snapshots resolve supported retired-name aliases
    // just as admission does. Unknown definitions remain refusals.
    #[tokio::test]
    async fn pending_workflow_uses_supported_builtin_alias() {
        let (backend, mut inventory) = frozen_store().await;
        inventory[0]["status"]["phase"] = serde_json::json!("Pending");
        inventory[0]["status"]["workflow_snapshot"] = Value::Null;
        inventory[0]["spec"]["workflow_ref"] = serde_json::json!("single-agent-contained");
        // The canonical template is built in: the merged inventory carries it without a stored copy.
        let template =
            backend.definitions::<flotilla_resources::WorkflowTemplate>("fleet").get("single-agent").await.expect("canonical template");
        inventory.push(serde_json::to_value(template.to_k8s_object()).expect("document"));
        let supply = Supply { revision: "1".repeat(40), image_available: true };
        assert!(check(&inventory, &BTreeSet::new(), &supply).await.expect("check").failures.is_empty());
        inventory[0]["spec"]["workflow_ref"] = serde_json::json!("unknown");
        assert_eq!(check(&inventory, &BTreeSet::new(), &supply).await.expect("check").workflow.unsatisfied, 1);
    }

    // A placed vessel's exact image identity and composition remain required
    // after admission. Declared grants in another namespace cannot authorize it.
    #[tokio::test]
    async fn images_and_grants_use_the_frozen_vessel_and_namespace() {
        use flotilla_resources::{CredentialSpec, CredentialSpecSpec, DockerEnvironmentSpec, EnvironmentSpec, VesselSpec, VesselStatus};
        let (backend, mut inventory) = frozen_store().await;
        inventory[0]["status"]["workflow_snapshot"]["vessels"][0]["credential_refs"] = serde_json::json!(["bot"]);
        let spec: CredentialSpecSpec = serde_json::from_value(
            serde_json::json!({"consumer":{"adapter":"gh"}, "source":{"kind":"file","path":"/test/token"},"lifecycle":"static"}),
        )
        .expect("credential");
        let credential = backend
            .clone()
            .definitions::<CredentialSpec>("fleet")
            .apply(&InputMeta::builder().name("bot".into()).build(), &spec)
            .await
            .expect("credential");
        let credential_index = inventory.len();
        inventory.push(serde_json::to_value(credential.to_k8s_object()).expect("document"));
        let docker: DockerEnvironmentSpec =
            serde_json::from_value(serde_json::json!({"host_ref":"host", "image":"old-tag"})).expect("docker");
        let environment = backend
            .clone()
            .using::<Environment>("fleet")
            .create(
                &InputMeta::builder().name("environment".into()).build(),
                &EnvironmentSpec { host_direct: None, docker: Some(docker), ..Default::default() },
            )
            .await
            .expect("environment");
        let environment_index = inventory.len();
        inventory.push(serde_json::to_value(environment.to_k8s_object()).expect("document"));
        let vessels = backend.clone().using::<Vessel>("fleet");
        let vessel = vessels
            .create(
                &InputMeta::builder().name("vessel".into()).build(),
                &VesselSpec {
                    convoy_ref: "governor".into(),
                    vessel_name: "work".into(),
                    fulfilment_kind_ref: "contained".into(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
            )
            .await
            .expect("vessel");
        let digest = format!("registry.example/crew@sha256:{}", "a".repeat(64));
        let vessel = vessels
            .update_status(
                "vessel",
                &vessel.metadata.resource_version,
                &VesselStatus {
                    environment_ref: Some("environment".into()),
                    registry_digest: Some(digest.clone()),
                    ..VesselStatus::default()
                },
            )
            .await
            .expect("placed vessel");
        let vessel_index = inventory.len();
        inventory.push(serde_json::to_value(vessel.to_k8s_object()).expect("document"));
        // A malformed retained Vessel stays actionable against its convoy and
        // cannot be waived; absent namespaces have no invented default scope.
        let mut malformed = inventory.clone();
        malformed[0]["metadata"]["annotations"][READMISSION_ANNOTATION] = Value::String("re-admit".into());
        malformed[vessel_index]["status"]["held_credentials"] = Value::String("invalid".into());
        let report = check(&malformed, &BTreeSet::new(), &Supply { revision: "1".repeat(40), image_available: true })
            .await
            .expect("actionable decode report");
        assert!(!report.inventory_complete);
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("Convoy/fleet/governor Vessel/fleet/vessel") && failure.contains("cannot decode")));
        assert!(report.waivers.is_empty());
        let mut missing_namespace = inventory[0].clone();
        missing_namespace["metadata"].as_object_mut().expect("metadata").remove("namespace");
        assert!(decode::<Convoy>(&missing_namespace).is_err(), "typed metadata requires a namespace");
        assert_eq!(namespace(&missing_namespace), "<missing namespace>");
        let supply = Supply { revision: "1".repeat(40), image_available: true };
        let report = check(&inventory, &BTreeSet::new(), &supply).await.expect("check");
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.images.checked, 1);
        assert_eq!(report.grants.checked, 1);
        let report = check(&inventory, &BTreeSet::new(), &Supply { image_available: false, ..supply }).await.expect("check");
        assert_eq!(report.images.unsatisfied, 1);
        assert!(report.failures[0].contains(&digest));
        inventory[credential_index]["metadata"]["namespace"] = Value::String("other".into());
        let report = check(&inventory, &BTreeSet::new(), &Supply { revision: "1".repeat(40), image_available: true }).await.expect("check");
        assert_eq!(report.grants.unsatisfied, 1);
        assert!(report.failures[0].contains("bot"));
        inventory[credential_index]["metadata"]["namespace"] = Value::String("fleet".into());
        // A composed Environment is checked through its per-identity build: a
        // failed build refuses with its reason, whatever the vessel recorded.
        let (name, build) = failed_build(&backend).await;
        // A missing build refuses even though the vessel recorded a digest.
        inventory[environment_index]["spec"]["docker"]["image_build_ref"] = Value::String(name.clone());
        let report = check(&inventory, &BTreeSet::new(), &Supply { revision: "1".repeat(40), image_available: true }).await.expect("check");
        assert_eq!(report.images.unsatisfied, 1);
        assert!(report.failures.iter().any(|failure| failure.contains("frozen image build is unavailable")), "{:?}", report.failures);
        inventory.push(build);
        let report = check(&inventory, &BTreeSet::new(), &Supply { revision: "1".repeat(40), image_available: true }).await.expect("check");
        assert_eq!(report.images.unsatisfied, 1);
        assert!(
            report.failures.iter().any(|failure| failure.contains("image build failed: probe display-x11 failed")),
            "{:?}",
            report.failures
        );
    }

    fn current_build_spec() -> flotilla_resources::ImageBuildSpec {
        use flotilla_resources::{ImageDelivery, ImageLayerSource, ImageLayerSpec, ImageLayerTier};
        let layers = vec![ImageLayerSpec::builder()
            .name("base".into())
            .version("1".into())
            .tier(ImageLayerTier::Base)
            .source(ImageLayerSource::Stdlib)
            .from(format!("ubuntu@sha256:{}", "1".repeat(64)))
            .fragment("FROM ${BASE}\n".into())
            .probe("true".into())
            .build()];
        flotilla_resources::ImageBuildSpec::builder()
            .identity(flotilla_resources::image_identity(&layers))
            .architecture("amd64".into())
            .layers(layers)
            .host_ref("host".into())
            .delivery(ImageDelivery::Local)
            .reservation(flotilla_resources::ImageBuildReservation { cpu: 1, disk_bytes: 1 })
            .build()
    }

    async fn failed_build(backend: &ResourceBackend) -> (String, Value) {
        use flotilla_resources::{ImageBuildFailure, ImageBuildFailureClass, ImageBuildStatus};
        let spec = current_build_spec();
        let name = spec.resource_name();
        let builds = backend.clone().using::<ImageBuild>("fleet");
        let created = builds.create(&InputMeta::builder().name(name.clone()).build(), &spec.into()).await.expect("build");
        let failed = ImageBuildStatus {
            phase: ImageBuildPhase::Failed,
            failure: Some(ImageBuildFailure { class: ImageBuildFailureClass::Recipe, reason: "probe display-x11 failed".into() }),
            ..Default::default()
        };
        let build = builds.update_status(&name, &created.metadata.resource_version, &failed).await.expect("failed build");
        (name, serde_json::to_value(build.to_k8s_object()).expect("document"))
    }

    async fn placed_image_inventory() -> (ResourceBackend, Vec<Value>) {
        use flotilla_resources::{Host, HostSpec, VesselSpec, VesselStatus};
        let (backend, mut inventory) = frozen_store().await;
        let host = backend
            .clone()
            .using::<Host>("fleet")
            .create(&InputMeta::builder().name("host-b".into()).build(), &HostSpec::default())
            .await
            .expect("host B");
        inventory.push(serde_json::to_value(host.to_k8s_object()).expect("host document"));
        let vessels = backend.clone().using::<Vessel>("fleet");
        let vessel = vessels
            .create(
                &InputMeta::builder().name("vessel".into()).build(),
                &VesselSpec {
                    convoy_ref: "governor".into(),
                    vessel_name: "work".into(),
                    fulfilment_kind_ref: "contained".into(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
            )
            .await
            .expect("vessel");
        let decision = serde_json::from_value(serde_json::json!({
            "policy_name":"contained", "target_host":{"ref":"host-b","display_name":"B"}
        }))
        .expect("placement");
        let vessel = vessels
            .update_status(
                "vessel",
                &vessel.metadata.resource_version,
                &VesselStatus {
                    placement_decision: Some(decision),
                    environment_ref: Some("environment".into()),
                    ..VesselStatus::default()
                },
            )
            .await
            .expect("placed vessel");
        inventory.push(serde_json::to_value(vessel.to_k8s_object()).expect("vessel document"));
        (backend, inventory)
    }

    // #2933: a merged convoy placed on B passes on A without B's Environment;
    // B still checks its image and refuses a genuinely missing local Environment.
    #[tokio::test]
    async fn frozen_images_follow_placement_across_two_host_inventories() {
        use flotilla_protocol::NodeId;
        use flotilla_resources::{DockerEnvironmentSpec, EnvironmentSpec, Host};
        use flotilla_store::list_resource_kind_including_replicas;
        let (backend, mut inventory) = placed_image_inventory().await;
        let supply = Supply { revision: "1".repeat(40), image_available: true };
        let replica_backend = ResourceBackend::new(InMemoryBackend::default());
        let origin = NodeId::new("root-b");
        replica_backend
            .replica_writer::<Convoy>(origin.clone(), "fleet")
            .replace(&backend.using::<Convoy>("fleet").list().await.expect("convoys"), chrono::Utc::now())
            .await
            .expect("replicate convoy to A");
        replica_backend
            .replica_writer::<Host>(origin.clone(), "fleet")
            .replace(&backend.using::<Host>("fleet").list().await.expect("hosts"), chrono::Utc::now())
            .await
            .expect("replicate host to A");
        replica_backend
            .replica_writer::<Vessel>(origin, "fleet")
            .replace(&backend.using::<Vessel>("fleet").list().await.expect("vessels"), chrono::Utc::now())
            .await
            .expect("replicate vessel to A");
        let mut host_a = Vec::new();
        for kind in ["convoys", "hosts", "vessels"] {
            let merged = list_resource_kind_including_replicas(&replica_backend, "fleet", kind).await.expect("merged inventory");
            host_a.extend(merged.value["items"].as_array().expect("items").iter().cloned());
        }
        let report = check(&host_a, &BTreeSet::new(), &supply).await.expect("A checks merged inventory");
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.images.checked, 0);
        assert_eq!(report.validated_elsewhere.len(), 1);
        assert!(report.validated_elsewhere[0].contains("validated on the host that holds it (host-b)"));
        // Delegated references must be visible in the CLI's JSON report.
        let json_report = serde_json::to_value(&report).expect("serialized report");
        assert_eq!(json_report["validated_elsewhere"][0], report.validated_elsewhere[0]);
        assert_eq!(report.skills.checked, 1);
        assert_eq!(report.workflow.checked, 1);
        // Portable references still use candidate supply and merged definitions.
        let mut invalid_portable = host_a.clone();
        invalid_portable[0]["status"]["workflow_snapshot"]["vessels"][0]["credential_refs"] = serde_json::json!(["missing"]);
        let report = check(
            &invalid_portable,
            &BTreeSet::from([("fleet".into(), "single-agent".into())]),
            &Supply { revision: "2".repeat(40), image_available: false },
        )
        .await
        .expect("portable checks on A");
        assert_eq!(report.images.unsatisfied, 0);
        assert_eq!(report.skills.unsatisfied, 1);
        assert_eq!(report.workflow.unsatisfied, 1);
        assert_eq!(report.grants.unsatisfied, 1);
        let report = check(&inventory, &BTreeSet::new(), &supply).await.expect("B checks missing Environment");
        assert_eq!(report.images.unsatisfied, 1);
        assert!(report.failures[0].contains("frozen Environment"));
        let environment = backend
            .using::<Environment>("fleet")
            .create(
                &InputMeta::builder().name("environment".into()).build(),
                &EnvironmentSpec {
                    host_direct: None,
                    docker: Some(
                        serde_json::from_value::<DockerEnvironmentSpec>(serde_json::json!({"host_ref":"host-b", "image":"frozen-tag"}))
                            .expect("docker"),
                    ),
                    ..Default::default()
                },
            )
            .await
            .expect("environment");
        inventory.push(serde_json::to_value(environment.to_k8s_object()).expect("environment document"));
        let report = check(&inventory, &BTreeSet::new(), &supply).await.expect("B checks image");
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.images.checked, 1);
        // A locally inventoried Environment is checked even with a stale remote pin.
        host_a.push(inventory.last().expect("environment document").clone());
        let report = check(&host_a, &BTreeSet::new(), &Supply { revision: "1".repeat(40), image_available: false })
            .await
            .expect("present local Environment");
        assert_eq!(report.images.unsatisfied, 1);
        assert!(report.validated_elsewhere.is_empty());
        let report =
            check(&inventory, &BTreeSet::new(), &Supply { image_available: false, ..supply }).await.expect("B refuses unavailable image");
        assert_eq!(report.images.unsatisfied, 1);
    }

    // Placement/provenance property: only a known remote placement defers
    // host-local images; local and unknown ownership still refuse missing state.
    // Generators cover all three placement sources plus no pin, local/replica/
    // absent Host records, namespace mismatch, retained images, and waivers.
    #[hegel::test]
    fn image_scope_requires_positive_remote_placement(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let pin_source = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let host_source = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let other_namespace = tc.draw(gs::booleans());
        let retained = tc.draw(gs::booleans());
        let waiver = tc.draw(gs::booleans());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let (_, mut inventory) = placed_image_inventory().await;
            let decision = inventory[2]["status"]["placement_decision"].take();
            match pin_source {
                0 => inventory[2]["status"]["placement_decision"] = decision,
                1 => {
                    inventory[0]["metadata"]["annotations"][flotilla_resources::VESSEL_PLACEMENTS_ANNOTATION] =
                        serde_json::json!(serde_json::json!({"work":{"policy_ref":"contained", "decision":decision}}).to_string())
                }
                2 => inventory[0]["status"]["placement_decision"] = decision,
                _ => {}
            }
            if host_source == 1 {
                inventory[1]["metadata"]["annotations"]["flotilla.work/origin-root"] = serde_json::json!("root-b");
            } else if host_source == 2 {
                inventory[1]["metadata"]["name"] = serde_json::json!("unrelated-host");
            }
            if other_namespace {
                inventory[1]["metadata"]["namespace"] = serde_json::json!("other");
            }
            if retained {
                inventory[2]["status"]["environment_ref"] = Value::Null;
                inventory[2]["status"]["local_image_id"] = serde_json::json!("frozen-id");
            }
            if waiver {
                inventory[0]["metadata"]["annotations"][READMISSION_ANNOTATION] = serde_json::json!("re-admit");
            }
            let report =
                check(&inventory, &BTreeSet::new(), &Supply { revision: "1".repeat(40), image_available: false }).await.expect("check");
            let remote = pin_source < 3 && host_source == 1 && !other_namespace;
            assert_eq!(report.validated_elsewhere.len(), usize::from(remote));
            assert_eq!(report.images.checked, usize::from(!remote));
            assert_eq!(report.images.unsatisfied, usize::from(!remote));
            assert_eq!(report.images.waived, usize::from(!remote && waiver));
            assert_eq!(report.failures.len(), usize::from(!remote && !waiver));
            assert_eq!(report.skills.checked, 1);
            assert_eq!(report.workflow.checked, 1);
        });
    }

    // The process-boundary stand-in accepts only Docker's exact manifest
    // inspection contract; unrelated calls fail instead of being permissive.
    struct RegistryRunner {
        reference: String,
        available: bool,
        local: Option<bool>,
        calls: std::sync::Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl CommandRunner for RegistryRunner {
        async fn run(&self, command: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            assert_eq!(command, "docker");
            assert_eq!(args, ["manifest", "inspect", &self.reference]);
            assert_eq!(cwd, Path::new("/"));
            *self.calls.lock().expect("calls") += 1;
            self.available.then(|| "manifest".into()).ok_or_else(|| "manifest unknown".into())
        }
        async fn run_output(
            &self,
            command: &str,
            args: &[&str],
            cwd: &Path,
            _label: &ChannelLabel,
        ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
            assert_eq!(command, "docker");
            assert_eq!(args, ["image", "inspect", &self.reference]);
            assert_eq!(cwd, Path::new("/"));
            let local = self.local.expect("exact digest validation must not probe local tags");
            Ok(flotilla_provider_runtime::CommandOutput {
                stdout: serde_json::json!([{"Id": format!("sha256:{}", "a".repeat(64))}]).to_string(),
                stderr: "image absent".into(),
                exit_code: Some(i32::from(!local)),
            })
        }
        async fn exists(&self, _command: &str, _args: &[&str]) -> bool {
            false
        }
    }

    // Cache OR registry suffices for the frozen identity. The local config SHA
    // and registry manifest SHA differ: a cache hit must not depend on registry
    // availability or cause a registry request. Unrelated host images never count.
    #[hegel::test]
    fn image_identity_cache_or_registry(tc: hegel::TestCase) {
        let cached = tc.draw(hegel::generators::booleans());
        let published = tc.draw(hegel::generators::booleans());
        let duplicates = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(3));
        // Both pre-roll host-only observations and new per-cache observations
        // can prove fleet-wide existence without selecting a placement cache.
        let legacy_inventory = tc.draw(hegel::generators::booleans());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let local = format!("sha256:{}", "a".repeat(64));
            let registry = format!("registry.example/crew@sha256:{}", "b".repeat(64));
            let held = if cached { local.clone() } else { format!("sha256:{}", "c".repeat(64)) };
            let held = if legacy_inventory { serde_json::json!([held]) } else { serde_json::json!({"cache-a":[held]}) };
            let inventory = vec![serde_json::json!({"kind":"Host", "status":{"capabilities":{IMAGE_DIGESTS_CAPABILITY:held}}}); duplicates];
            let options = super::super::ValidationOptions::default();
            let runner = std::sync::Arc::new(RegistryRunner {
                reference: registry.clone(),
                available: published,
                local: None,
                calls: std::sync::Mutex::new(0),
            });
            let provider = flotilla_environment::environment::command_provider(
                flotilla_environment::environment::EnvironmentInput::Docker,
                runner.clone(),
            );
            let probes = CandidateProbes { options: &options, inventory: &inventory, runner: runner.clone(), provider: provider.as_ref() };
            assert_eq!(check_identity(&probes, &local, Some(&registry)).await.is_ok(), cached || published);
            assert_eq!(*runner.calls.lock().expect("calls"), usize::from(!cached));
            if !cached && !published {
                let error = probes.image(&registry).await.expect_err("cache and registry unavailable");
                assert!(error.contains("absent from host digest inventories") && error.contains("manifest unknown"), "{error}");
            }
        });
    }

    // Mutable baseline validation reads the owning provider cache first, then
    // its registry surface only on absence. Generate both independent sources;
    // neither reading may pull or build an image.
    #[hegel::test]
    fn baseline_validation_uses_provider_cache(tc: hegel::TestCase) {
        let local = tc.draw(hegel::generators::booleans());
        let registry = tc.draw(hegel::generators::booleans());
        let runner = std::sync::Arc::new(RegistryRunner {
            reference: "crew:test".into(),
            available: registry,
            local: Some(local),
            calls: std::sync::Mutex::new(0),
        });
        let provider = flotilla_environment::environment::command_provider(
            flotilla_environment::environment::EnvironmentInput::Docker,
            runner.clone(),
        );
        let options = super::super::ValidationOptions::default();
        let probes = CandidateProbes { options: &options, inventory: &[], runner: runner.clone(), provider: provider.as_ref() };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            assert_eq!(probes.image("crew:test").await.is_ok(), local || registry);
            assert_eq!(*runner.calls.lock().expect("calls"), usize::from(!local));
        });
    }

    // ADR 0062: retired stage builds decode but carry no availability evidence,
    // and the retired frozen-image-layers annotation is ignored. A composed
    // Environment's queued build is satisfiable; its Built digest is probed.
    #[tokio::test]
    async fn retired_image_records_are_ignored_and_built_digests_are_probed() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let retired = serde_json::json!({
            "apiVersion": "flotilla.work/v1", "kind": "ImageBuild",
            "metadata": {"name": "build", "namespace": "test", "resourceVersion": "1", "labels": {}, "annotations": {},
                         "creationTimestamp": "2026-10-01T00:00:00Z"},
            "spec": {"recipe_key": "sha256:old", "host_ref": "host", "attempt": 0},
            "status": {"phase": "built", "identity": {"local_image_id": digest}, "availability": {"hosts": ["host"]}}
        });
        decode::<ImageBuild>(&retired).expect("retired build decodes");
        let inventory =
            vec![serde_json::json!({"kind":"Host", "status":{"capabilities":{IMAGE_DIGESTS_CAPABILITY:{"cache-a":[]}}}}), retired];
        let options = super::super::ValidationOptions::default();
        // Stands in for a registry process: a local ID never reaches it.
        let runner = std::sync::Arc::new(RegistryRunner {
            reference: "unused".into(),
            available: false,
            local: None,
            calls: std::sync::Mutex::new(0),
        });
        let provider = flotilla_environment::environment::command_provider(
            flotilla_environment::environment::EnvironmentInput::Docker,
            runner.clone(),
        );
        let probes = CandidateProbes { options: &options, inventory: &inventory, runner: runner.clone(), provider: provider.as_ref() };
        assert!(probes.image(&digest).await.is_err(), "retired availability is not evidence");
        assert_eq!(*runner.calls.lock().expect("calls"), 0);

        let (backend, mut inventory) = placed_image_inventory().await;
        inventory[0]["metadata"]["annotations"]["flotilla.work/frozen-image-layers"] = Value::String("{not json".into());
        let spec = current_build_spec();
        let name = spec.resource_name();
        let builds = backend.clone().using::<ImageBuild>("fleet");
        let queued = builds.create(&InputMeta::builder().name(name.clone()).build(), &spec.into()).await.expect("queued build");
        let environment = backend
            .clone()
            .using::<Environment>("fleet")
            .create(
                &InputMeta::builder().name("environment".into()).build(),
                &flotilla_resources::EnvironmentSpec {
                    host_direct: None,
                    docker: Some(
                        serde_json::from_value(serde_json::json!({"host_ref":"host-b", "image":"", "image_build_ref": name}))
                            .expect("docker"),
                    ),
                    ..Default::default()
                },
            )
            .await
            .expect("environment");
        inventory.push(serde_json::to_value(environment.to_k8s_object()).expect("environment document"));
        let build_index = inventory.len();
        inventory.push(serde_json::to_value(queued.to_k8s_object()).expect("build document"));
        let unavailable = Supply { revision: "1".repeat(40), image_available: false };
        let report = check(&inventory, &BTreeSet::new(), &unavailable).await.expect("queued");
        assert!(report.failures.is_empty(), "a queued build is satisfiable: {:?}", report.failures);
        assert_eq!(report.images.checked, 1);
        let built =
            flotilla_resources::ImageBuildStatus { phase: ImageBuildPhase::Built, digest: Some(digest.clone()), ..Default::default() };
        let built = builds.update_status(&name, &queued.metadata.resource_version, &built).await.expect("built");
        inventory[build_index] = serde_json::to_value(built.to_k8s_object()).expect("built document");
        assert_eq!(check(&inventory, &BTreeSet::new(), &unavailable).await.expect("built").images.unsatisfied, 1);
        let available = Supply { revision: "1".repeat(40), image_available: true };
        assert!(check(&inventory, &BTreeSet::new(), &available).await.expect("available").failures.is_empty());
    }

    // Generated inventory property: terminal convoys are exempt; live convoys
    // fail exactly for unsatisfied references unless explicitly re-admitted.
    // Generator crosses empty/boundary collections and mixes terminal phases,
    // valid/missing skill pins, missing grants, malformed snapshots and waivers.
    #[hegel::test]
    fn eligibility_and_category_counts(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let (_, base) = frozen_store().await;
            let mut inventory = Vec::new();
            let mut expected_failures = 0;
            let mut expected_waivers = 0;
            let mut expected_live = 0;
            for index in 0..count {
                let terminal = tc.draw(gs::booleans());
                let waiver = tc.draw(gs::booleans());
                let missing = tc.draw(gs::booleans());
                let grant = tc.draw(gs::booleans());
                let invalid = tc.draw(gs::booleans());
                let mut document = base[0].clone();
                document["metadata"]["name"] = Value::String(format!("convoy-{index}"));
                if terminal {
                    document["status"]["phase"] = Value::String("Landed".into());
                } else {
                    expected_live += 1;
                }
                if waiver {
                    document["metadata"]["annotations"][READMISSION_ANNOTATION] = Value::String("re-admit".into());
                }
                if missing {
                    document["status"]["workflow_snapshot"]["vessels"][0]["crew"][0]["skills"]["selected"][0]["revision"] =
                        Value::String("2".repeat(40));
                }
                if grant {
                    document["status"]["workflow_snapshot"]["vessels"][0]["credential_refs"] = serde_json::json!(["missing"]);
                }
                if invalid {
                    document["status"]["workflow_snapshot"]["vessels"][0]["depends_on"] = serde_json::json!(["undeclared"]);
                }
                let failures = usize::from(missing) + usize::from(grant) + usize::from(invalid);
                if !terminal {
                    if waiver {
                        expected_waivers += failures;
                    } else {
                        expected_failures += failures;
                    }
                }
                inventory.push(document);
            }
            let before = inventory.clone();
            let report =
                check(&inventory, &BTreeSet::new(), &Supply { revision: "1".repeat(40), image_available: true }).await.expect("check");
            assert_eq!(report.live_convoys, expected_live);
            assert_eq!(report.failures.len(), expected_failures);
            assert_eq!(report.waivers.len(), expected_waivers);
            assert_eq!(inventory, before, "validation cannot mutate admission pins");
        });
    }
}
