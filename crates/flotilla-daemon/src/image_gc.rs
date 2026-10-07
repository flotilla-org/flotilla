//! Conservative image reachability and current-plus-previous retention.
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

use chrono::{DateTime, Utc};
use flotilla_resources::{
    ConditionValue, Convoy, CrewImageBaseline, Environment, FleetDesignation, Host, HostCondition, ImageBuild, ImageBuildPhase,
    ImageGcMode, ImageLayer, ResourceBackend, ResourceObject, Vessel, FLEET_DESIGNATION_NAME,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::image_distribution::{ImageDistributionIo, ImageDistributor};

pub(crate) const HEALTH_CAPABILITY: &str = "image_gc";
// Four missed default 30-second heartbeats. Custom heartbeat intervals must
// remain below this limit when apply-mode collection is enabled.
const MAX_HOST_HEARTBEAT_AGE_SECONDS: i64 = 120;
const MAX_REMOVALS_PER_STORE_PER_RUN: usize = 16;
const COLLECTION_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Default, Serialize)]
pub(crate) struct CollectionReport {
    pub dry_run: bool,
    pub budget_exhausted: bool,
    pub observed_at: Option<DateTime<Utc>>,
    pub local_candidates: BTreeSet<String>,
    pub registry_candidates: BTreeSet<String>,
    pub deleted_local: BTreeSet<String>,
    pub deleted_registry: BTreeSet<String>,
    pub reclaimed_local_bytes: u64,
    pub reclaimed_registry_bytes: Option<u64>,
    pub failures: Vec<String>,
}

fn candidates_after<'a>(candidates: &'a BTreeSet<String>, cursor: Option<&str>) -> Vec<&'a String> {
    candidates
        .iter()
        .filter(|id| cursor.is_none_or(|cursor| id.as_str() > cursor))
        .chain(candidates.iter().filter(|id| cursor.is_some_and(|cursor| id.as_str() <= cursor)))
        .collect()
}

fn execution_tag(build: &ResourceObject<ImageBuild>) -> String {
    format!("flotilla-build:{:x}", Sha256::digest(format!("{}\0{}", build.metadata.name, build.spec.recipe_key)))
}

fn parent_tag(image_id: &str) -> String {
    format!("flotilla-parent:{:x}", Sha256::digest(image_id))
}

/// Family identity excludes revisions, recipe hashes and realised parent IDs:
/// those are versions of the same composition. Ordered layer names plus target
/// architecture distinguish compositions, including capability combinations.
fn family(
    name: &str,
    builds: &BTreeMap<String, ResourceObject<ImageBuild>>,
    visiting: &mut BTreeSet<String>,
) -> Result<Vec<String>, String> {
    if !visiting.insert(name.into()) {
        return Err("image build parent cycle".into());
    }
    let build = builds.get(name).ok_or_else(|| format!("missing image build dependency {name}"))?;
    let mut chain = match &build.spec.parent_build_ref {
        Some(parent) => family(parent, builds, visiting)?,
        None => vec![build.spec.inputs.architecture.clone()],
    };
    chain.push(build.spec.layer.name.clone());
    visiting.remove(name);
    Ok(chain)
}

/// Extract immutable identities and build references even from frozen snapshots
/// and legacy placement annotations. Keep literal arbitrary images untouched.
pub(crate) fn pins(value: &serde_json::Value, protected: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::String(value) => {
            protected.insert(value.clone());
            if let Some((_, digest)) = value.rsplit_once('@') {
                protected.insert(digest.into());
            }
            // Placement snapshots are JSON encoded inside annotations.
            if let Ok(encoded) = serde_json::from_str::<serde_json::Value>(value) {
                if encoded.is_object() || encoded.is_array() {
                    pins(&encoded, protected);
                }
            }
        }
        serde_json::Value::Array(values) => values.iter().for_each(|value| pins(value, protected)),
        serde_json::Value::Object(values) => values.values().for_each(|value| pins(value, protected)),
        _ => {}
    }
}

pub(crate) fn retention(
    builds: &BTreeMap<String, ResourceObject<ImageBuild>>,
    live_layers: &BTreeSet<String>,
    mut protected: BTreeSet<String>,
    cutoff: DateTime<Utc>,
) -> Result<BTreeSet<String>, String> {
    let mut versions: BTreeMap<Vec<String>, Vec<&ResourceObject<ImageBuild>>> = BTreeMap::new();
    for (name, build) in builds {
        let chain = family(name, builds, &mut BTreeSet::new())?;
        let Some(status) = &build.status else {
            protected.insert(name.clone());
            continue;
        };
        if matches!(status.phase, ImageBuildPhase::Queued | ImageBuildPhase::Building)
            || (status.phase == ImageBuildPhase::Built && status.finished_at.is_none_or(|at| at > cutoff))
        {
            protected.insert(name.clone());
        }
        if status.phase == ImageBuildPhase::Built
            && !status.availability.retired
            && chain.iter().skip(1).all(|layer| live_layers.contains(layer))
        {
            versions.entry(chain).or_default().push(build);
        }
    }
    for group in versions.values_mut() {
        group.sort_by_key(|build| (std::cmp::Reverse(build.status.as_ref().and_then(|status| status.finished_at)), &build.metadata.name));
        let mut digests = BTreeSet::new();
        for build in group.iter() {
            let id = build.status.as_ref().and_then(|status| status.identity.as_ref()).ok_or("completed build lacks identity")?;
            if digests.len() < 2 || digests.contains(&id.local_image_id) {
                digests.insert(id.local_image_id.clone());
                protected.insert(build.metadata.name.clone());
            }
        }
    }
    // Every retained stage retains its complete parent chain. Directly pinned
    // image identities also root the build graph, including registry identities.
    loop {
        let before = protected.len();
        for (name, build) in builds {
            let identity = build.status.as_ref().and_then(|status| status.identity.as_ref());
            let registry = build.status.as_ref().and_then(|status| status.availability.registry_ref.as_ref());
            // Generation-1 frozen baselines may retain a literal build handle.
            // Resolve only handles derived from immutable evidence; never infer
            // a candidate's identity from an arbitrary mutable tag.
            let handle_pin = protected.contains(&execution_tag(build))
                || identity.is_some_and(|id| {
                    protected.contains(&parent_tag(&id.local_image_id))
                        || registry.is_some_and(|reference| {
                            reference.rsplit_once('@').is_some_and(|(repository, _)| {
                                protected.contains(&format!("{repository}:flotilla-{}", id.local_image_id.trim_start_matches("sha256:")))
                            })
                        })
                });
            if handle_pin
                || protected.contains(name)
                || identity.is_some_and(|id| {
                    protected.contains(&id.local_image_id) || id.registry_digest.as_ref().is_some_and(|id| protected.contains(id))
                })
                || registry
                    .is_some_and(|id| protected.contains(id) || id.rsplit_once('@').is_some_and(|(_, digest)| protected.contains(digest)))
            {
                protected.insert(name.clone());
                if let Some(id) = identity {
                    protected.insert(id.local_image_id.clone());
                    if let Some(digest) = &id.registry_digest {
                        protected.insert(digest.clone());
                    }
                }
                if let Some(id) = registry {
                    protected.insert(id.clone());
                }
                protected.insert(build.spec.inputs.parent_digest.clone());
                if let Some(parent) = &build.spec.parent_build_ref {
                    protected.insert(parent.clone());
                }
            }
        }
        if protected.len() == before {
            break;
        }
    }
    Ok(protected)
}

impl<I: ImageDistributionIo + 'static> ImageDistributor<I> {
    async fn collection_snapshot(&self, grace: u64) -> Result<(BTreeMap<String, ResourceObject<ImageBuild>>, BTreeSet<String>), String> {
        let started = Instant::now();
        if self
            .backend
            .diagnostics()
            .await
            .map_err(|error| error.to_string())?
            .is_some_and(|diagnostics| !diagnostics.decode_quarantines.is_empty() || !diagnostics.event_decode_quarantines.is_empty())
        {
            return Err("image collection cannot prove reachability while records are quarantined".into());
        }
        let builds = flotilla_resources::list_image_builds(&self.backend, &self.namespace).await.map_err(|error| error.to_string())?;
        let layers = self.backend.definitions::<ImageLayer>(&self.namespace).list().await.map_err(|error| error.to_string())?;
        let layers =
            layers.into_iter().filter(|layer| layer.metadata.deletion_timestamp.is_none()).map(|layer| layer.metadata.name).collect();
        let mut protected = BTreeSet::new();
        // Every stored convoy snapshot stays pinned until that convoy is removed,
        // even if landed. Removing runtime children cannot remove frozen testimony.
        macro_rules! runtime_pins {
            ($kind:ty) => {
                for source in
                    self.backend.including_replicas::<$kind>(&self.namespace).list().await.map_err(|error| error.to_string())?.items
                {
                    pins(&serde_json::to_value(source.object).map_err(|error| error.to_string())?, &mut protected);
                }
            };
        }
        runtime_pins!(Convoy);
        runtime_pins!(Environment);
        runtime_pins!(Vessel);
        for baseline in self.backend.definitions::<CrewImageBaseline>(&self.namespace).list().await.map_err(|error| error.to_string())? {
            pins(&serde_json::to_value(baseline).map_err(|error| error.to_string())?, &mut protected);
        }
        let seconds = i64::try_from(grace).map_err(|_| "image GC grace too large")?;
        let cutoff = Utc::now().checked_sub_signed(chrono::Duration::seconds(seconds)).ok_or("image GC grace out of range")?;
        let protected = retention(&builds, &layers, protected, cutoff)?;
        tracing::debug!(builds = builds.len(), pins = protected.len(), elapsed = ?started.elapsed(), "image GC reachability snapshot");
        Ok((builds, protected))
    }

    /// Called by the host's scheduled observation loop. One designated host may
    /// remove registry manifests; every Docker host collects its own local IDs.
    pub(crate) async fn collect_if_due(&self) -> Result<(), String> {
        let fleet = match self.backend.definitions::<FleetDesignation>(&self.namespace).get(FLEET_DESIGNATION_NAME).await {
            Ok(fleet) => fleet,
            Err(flotilla_resources::ResourceError::NotFound { .. }) => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        let Some(policy) = fleet.spec.image_gc else {
            return Ok(());
        };
        let mut next = self.next_collection.lock().await;
        if next.is_some_and(|at| at > Instant::now()) {
            return Ok(());
        }
        // Delivery and publication share the gate; collection takes it only
        // when idle so a long transfer never stalls the scheduler. Finished
        // publications must first have their digest evidence drained by refresh.
        let Ok(_gate) = self.collection_gate.try_write() else {
            *next = Some(Instant::now() + COLLECTION_RETRY_DELAY);
            return Ok(());
        };
        if !self.publications.lock().await.is_empty() || self.jobs.lock().await.values().any(|job| !job.is_finished()) {
            *next = Some(Instant::now() + COLLECTION_RETRY_DELAY);
            return Ok(());
        }
        *next = Some(Instant::now() + std::time::Duration::from_secs(policy.interval_seconds));
        let mut report = CollectionReport {
            dry_run: policy.mode == ImageGcMode::DryRun,
            observed_at: Some(Utc::now()),
            deleted_registry: self.collection_deleted_registry.lock().await.clone(),
            ..Default::default()
        };
        let result = async {
            let grace_seconds = i64::try_from(policy.grace_seconds).map_err(|_| "image GC grace too large")?;
            let (builds, protected) = self.collection_snapshot(policy.grace_seconds).await?;
            let held = self.io.inventory().await?;
            if let Ok(host) = self.backend.using::<Host>(&self.namespace).get(&self.host).await {
                if let Some(deleted) = host
                    .status
                    .as_ref()
                    .and_then(|status| status.capabilities.get(HEALTH_CAPABILITY))
                    .and_then(|report| report.get("deleted_registry"))
                {
                    match serde_json::from_value::<BTreeSet<String>>(deleted.clone()) {
                        Ok(deleted) => report.deleted_registry.extend(deleted),
                        Err(error) => tracing::warn!(%error, "ignoring malformed image GC registry tombstones"),
                    }
                }
            }
            // Bound tombstones to existing execution evidence. A removed build
            // cannot generate another delete candidate, so its tombstone is free.
            let published: BTreeSet<_> =
                builds.values().filter_map(|build| build.status.as_ref()?.availability.registry_ref.as_ref()).collect();
            report.deleted_registry.retain(|reference| published.contains(reference));
            for build in builds.values() {
                let Some(status) = build.status.as_ref().filter(|status| status.phase == ImageBuildPhase::Built) else {
                    continue;
                };
                let id = &status.identity.as_ref().ok_or("built image has no identity")?.local_image_id;
                if protected.contains(id) {
                    continue;
                }
                if held.contains(id) {
                    report.local_candidates.insert(id.clone());
                }
                if policy.registry_host.as_deref() == Some(&self.host) {
                    if let Some(reference) = &status.availability.registry_ref {
                        if !report.deleted_registry.contains(reference)
                            && !protected.contains(reference)
                            && !reference.rsplit_once('@').is_some_and(|(_, digest)| protected.contains(digest))
                        {
                            report.registry_candidates.insert(reference.clone());
                        }
                    }
                }
            }
            // A disconnected/stale fleet cannot prove that no unseen admission
            // has frozen an old digest. Refuse mutation, rather than trusting a
            // stale inventory. Single-host installations need no registry/peers.
            for source in self.backend.including_replicas::<Host>(&self.namespace).list().await.map_err(|error| error.to_string())?.items {
                if source.object.status.as_ref().is_some_and(|status| {
                    status.conditions.iter().any(|condition| {
                        condition.value == ConditionValue::False
                            && (condition.condition_type.starts_with("ResourceReplication")
                                || condition.condition_type.starts_with("ResourceStore/DecodeQuarantine"))
                    })
                }) {
                    return Err(format!("collection waits for healthy Host {} resource replication", source.object.metadata.name));
                }
                if source.object.metadata.deletion_timestamp.is_none()
                    && source
                        .object
                        .status
                        .as_ref()
                        .and_then(|status| status.heartbeat_at)
                        .is_none_or(|at| Utc::now().signed_duration_since(at).num_seconds() > MAX_HOST_HEARTBEAT_AGE_SECONDS)
                {
                    return Err(format!("collection waits for fresh Host {} heartbeat", source.object.metadata.name));
                }
            }
            if report.dry_run {
                return Ok::<(), String>(());
            }
            let free_before = self.io.disk_free().await?;
            // Retire the actuator's reusable availability before touching Docker.
            // New demands create successors instead of joining collected evidence.
            let objects = self.backend.using::<ImageBuild>(&self.namespace);
            let mut retirement_failures = BTreeSet::new();
            for build in builds.values().filter(|build| build.spec.host_ref == self.host) {
                if let Some(mut status) = build.status.clone().filter(|status| status.phase == ImageBuildPhase::Built) {
                    let retire = !protected.contains(&build.metadata.name);
                    if status.availability.retired != retire {
                        status.availability.retired = retire;
                        status.availability.retired_at = retire.then(Utc::now);
                        if let Err(error) = objects.update_status(&build.metadata.name, &build.metadata.resource_version, &status).await {
                            // This build stays quarantined. Continue independent
                            // work and retry promptly from fresh evidence.
                            retirement_failures.insert(build.metadata.name.clone());
                            report.failures.push(format!("{} retirement: {error}", build.metadata.name));
                        }
                    }
                }
            }
            // A retirement quarantine lets admissions already in flight publish
            // their frozen pins. Remote removal waits for the actuator's own
            // retirement evidence; no receiver invents retirement on its behalf.
            let quarantined = |id: &str| {
                let mut matches = builds
                    .values()
                    .filter(|build| {
                        build
                            .status
                            .as_ref()
                            .and_then(|status| status.identity.as_ref())
                            .is_some_and(|identity| identity.local_image_id == id)
                    })
                    .peekable();
                // Candidates come from builds, but explicitly refuse an empty
                // match instead of relying on vacuous all().
                matches.peek().is_some()
                    && matches.all(|build| {
                        !retirement_failures.contains(&build.metadata.name)
                            && build.status.as_ref().is_some_and(|status| {
                                status.availability.retired
                                    && status
                                        .availability
                                        .retired_at
                                        .is_some_and(|at| Utc::now().signed_duration_since(at).num_seconds() >= grace_seconds)
                            })
                    })
            };
            let mut cursors = self.collection_cursors.lock().await;
            let mut local_attempts = 0;
            for id in candidates_after(&report.local_candidates, cursors.0.as_deref()) {
                if !quarantined(id) {
                    continue;
                }
                if local_attempts == MAX_REMOVALS_PER_STORE_PER_RUN {
                    report.budget_exhausted = true;
                    break;
                }
                local_attempts += 1;
                cursors.0 = Some(id.clone());
                // Re-read immediately before each destructive operation. A newly
                // frozen convoy wins over this run's earlier candidate set.
                if self.collection_snapshot(policy.grace_seconds).await?.1.contains(id) {
                    continue;
                }
                let mut tags = BTreeSet::new();
                for build in builds.values() {
                    if build
                        .status
                        .as_ref()
                        .and_then(|status| status.identity.as_ref())
                        .is_some_and(|identity| &identity.local_image_id == id)
                    {
                        tags.insert(execution_tag(build));
                    }
                }
                tags.insert(parent_tag(id));
                if let Some(cache) = &fleet.spec.image_cache {
                    tags.insert(format!("{}:flotilla-{}", cache.repository, id.trim_start_matches("sha256:")));
                }
                match self.io.remove_local(id, &tags).await {
                    Ok(()) => {
                        report.deleted_local.insert(id.clone());
                    }
                    Err(reason) => report.failures.push(format!("{id}: {reason}")),
                }
            }
            if let (Some(cache), Some(credential)) = (&fleet.spec.image_cache, &policy.registry_credential) {
                let mut registry_attempts = 0;
                for reference in candidates_after(&report.registry_candidates, cursors.1.as_deref()) {
                    if !builds
                        .values()
                        .filter(|build| {
                            build.status.as_ref().and_then(|status| status.availability.registry_ref.as_ref()) == Some(reference)
                        })
                        .all(|build| {
                            build
                                .status
                                .as_ref()
                                .and_then(|status| status.identity.as_ref())
                                .is_some_and(|identity| quarantined(&identity.local_image_id))
                        })
                    {
                        continue;
                    }
                    if registry_attempts == MAX_REMOVALS_PER_STORE_PER_RUN {
                        report.budget_exhausted = true;
                        break;
                    }
                    registry_attempts += 1;
                    cursors.1 = Some(reference.clone());
                    let protected = self.collection_snapshot(policy.grace_seconds).await?.1;
                    if protected.contains(reference) || reference.rsplit_once('@').is_some_and(|(_, digest)| protected.contains(digest)) {
                        continue;
                    }
                    match self.io.remove_registry(cache, credential, reference).await {
                        Ok(()) => {
                            report.deleted_registry.insert(reference.clone());
                        }
                        Err(reason) => report.failures.push(format!("{reference}: {reason}")),
                    }
                }
            }
            report.reclaimed_local_bytes = self.io.disk_free().await?.saturating_sub(free_before);
            Ok(())
        }
        .await;
        if let Err(reason) = &result {
            report.failures.push(reason.clone());
        }
        // Keep evidence until a subsequent health write can persist it. A
        // publication failure must never replace the collection's own outcome.
        *self.collection_deleted_registry.lock().await = report.deleted_registry.clone();
        let publication = publish_report(&self.backend, &self.namespace, &self.host, &report).await;
        if let Err(error) = &publication {
            tracing::warn!(%error, "image GC health publication failed; deletion evidence retained for retry");
        }
        if publication.is_err() || !report.failures.is_empty() || report.budget_exhausted {
            *next = Some(Instant::now() + COLLECTION_RETRY_DELAY);
        }
        result
    }
}

async fn publish_report(backend: &ResourceBackend, namespace: &str, host: &str, report: &CollectionReport) -> Result<(), String> {
    let hosts = backend.using::<Host>(namespace);
    for _ in 0..3 {
        let object = hosts.get(host).await.map_err(|error| error.to_string())?;
        let mut status = object.status.clone().unwrap_or_default();
        status.capabilities.insert(HEALTH_CAPABILITY.into(), serde_json::to_value(report).map_err(|error| error.to_string())?);
        status.conditions.retain(|condition| condition.condition_type != "ImageGarbageCollection");
        status.conditions.push(HostCondition::builder()
            .condition_type("ImageGarbageCollection")
            .value(if report.failures.is_empty() { ConditionValue::True } else { ConditionValue::False })
            .reason(if report.dry_run { "DryRun" } else { "Collection" })
            .message(format!("{} local candidates, {} registry candidates; deleted {} local / {} registry; observed reclaimed local space {} bytes; registry bytes unknown; budget exhausted: {}; failures: {}", report.local_candidates.len(), report.registry_candidates.len(), report.deleted_local.len(), report.deleted_registry.len(), report.reclaimed_local_bytes, report.budget_exhausted, report.failures.join("; ")))
            .observed_at(Utc::now())
            .blocks_readiness(false)
            .build());
        match hosts.update_status(host, &object.metadata.resource_version, &status).await {
            Ok(_) => return Ok(()),
            Err(flotilla_resources::ResourceError::Conflict { .. }) => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("image GC health publication conflicted after three attempts".into())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use flotilla_resources::{
        FleetDesignationSpec, FrozenImageLayer, HostSpec, HostStatus, ImageBuildReason, ImageBuildReservation, ImageBuildSpec,
        ImageBuildStatus, ImageCacheBinding, ImageGcPolicy, ImageInputStability, ImageLayerParent, ImageLayerSpec, ImageLayerStage,
        InMemoryBackend, InputMeta, PlacedImageIdentity, Project, ProjectSpec, ResolvedImageInputs,
    };

    use super::*;

    fn digest(index: usize) -> String {
        format!("sha256:{index:064x}")
    }
    fn layer() -> ImageLayerSpec {
        ImageLayerSpec::builder()
            .stage(ImageLayerStage::Base)
            .parent(ImageLayerParent::Image(digest(999)))
            .repository("https://example.test/repo".into())
            .revision("a".repeat(40))
            .fragment("Dockerfile".into())
            .build()
    }
    async fn version(backend: &ResourceBackend, index: usize, image: usize) -> ResourceObject<ImageBuild> {
        let inputs = ResolvedImageInputs::builder()
            .parent_digest(digest(999))
            .content_hashes(vec![digest(index)])
            .architecture("amd64".into())
            .stability(ImageInputStability::Pinned)
            .build();
        let spec = ImageBuildSpec::builder()
            .recipe_key(inputs.recipe_key().expect("key"))
            .inputs(inputs)
            .layer(FrozenImageLayer { name: "base".into(), spec: layer() })
            .host_ref("host".into())
            .reservation(ImageBuildReservation { cpu: 1, disk_bytes: 1024 })
            .attempt(0)
            .reason(ImageBuildReason { description: "test".into(), old_inputs: Default::default(), new_inputs: Default::default() })
            .build();
        let builds = backend.using::<ImageBuild>("test");
        let object = builds.create(&InputMeta::builder().name(format!("build-{index}")).build(), &spec).await.expect("build");
        let status = ImageBuildStatus {
            phase: ImageBuildPhase::Built,
            finished_at: Some(Utc::now() - chrono::Duration::days(2) + chrono::Duration::seconds(index as i64)),
            identity: Some(PlacedImageIdentity { local_image_id: digest(image), registry_digest: None }),
            availability: flotilla_resources::ImageAvailability {
                registry_ref: Some(format!("registry.test/images@{}", digest(image + 100))),
                ..Default::default()
            },
            ..Default::default()
        };
        builds.update_status(&object.metadata.name, &object.metadata.resource_version, &status).await.expect("built");
        builds.get(&object.metadata.name).await.expect("read")
    }

    // #2733: collection preserves current and previous DISTINCT digests, and
    // every frozen pin. Generate zero through eight versions, duplicate output
    // digests, and arbitrary old pins; permutation must not change retention.
    #[hegel::test]
    fn retains_rollback_and_frozen_versions(tc: hegel::TestCase) {
        let count = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(8));
        let duplicate = tc.draw(hegel::generators::booleans());
        let pin = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(8));
        tokio::runtime::Runtime::new().expect("runtime").block_on(async {
            let backend = ResourceBackend::InMemory(InMemoryBackend::default());
            let mut builds = BTreeMap::new();
            let mut images = Vec::new();
            for index in 0..count {
                let image = if duplicate { index / 2 } else { index };
                images.push(digest(image));
                let build = version(&backend, index, image).await;
                builds.insert(build.metadata.name.clone(), build);
            }
            let protected = retention(
                &builds,
                &BTreeSet::from(["base".into()]),
                BTreeSet::from([digest(pin)]),
                Utc::now() - chrono::Duration::hours(1),
            )
            .expect("retention");
            let distinct = images.iter().rev().collect::<BTreeSet<_>>();
            for image in distinct.iter().rev().take(2) {
                assert!(protected.contains(*image), "rollback {image}");
            }
            assert!(protected.contains(&digest(pin)), "frozen convoy pin");
            for image in &images {
                if image != &digest(pin) && !distinct.iter().rev().take(2).any(|keep| *keep == image) {
                    assert!(!protected.contains(image), "obsolete digest is reclaimable");
                }
            }
        });
    }

    // Fake only at the Docker/registry process boundary. Resource collaborators
    // below use the production typed API backed by the real in-memory backend.
    #[derive(Default)]
    struct Docker {
        images: Mutex<BTreeSet<String>>,
        removed_registry: Mutex<BTreeSet<String>>,
        fail: bool,
        blocked_images: Mutex<BTreeSet<String>>,
        blocked_registry: Mutex<BTreeSet<String>>,
        conflict_retirement: Mutex<Option<ResourceBackend>>,
        delete_host_after_registry: Mutex<Option<ResourceBackend>>,
    }
    #[async_trait]
    impl ImageDistributionIo for Docker {
        async fn inventory(&self) -> Result<BTreeSet<String>, String> {
            let backend = self.conflict_retirement.lock().expect("conflict hook").take();
            if let Some(backend) = backend {
                let builds = backend.using::<ImageBuild>("test");
                let build = builds.get("build-1").await.expect("build");
                let mut status = build.status.expect("status");
                status.availability.failure = Some("concurrent inventory observation".into());
                builds.update_status("build-1", &build.metadata.resource_version, &status).await.expect("concurrent update");
            }
            Ok(self.images.lock().expect("images").clone())
        }
        async fn inspect(&self, id: &str) -> Result<PlacedImageIdentity, String> {
            if self.images.lock().expect("images").contains(id) {
                Ok(PlacedImageIdentity { local_image_id: id.into(), registry_digest: None })
            } else {
                Err("missing image".into())
            }
        }
        async fn push(&self, _: &ImageCacheBinding, _: &str) -> Result<String, String> {
            Err("unused".into())
        }
        async fn pull(&self, _: &ImageCacheBinding, _: &str) -> Result<(), String> {
            Err("unused".into())
        }
        async fn transfer(&self, _: &str, _: &str) -> Result<(), String> {
            Err("unused".into())
        }
        async fn remove_local(&self, id: &str, _: &BTreeSet<String>) -> Result<(), String> {
            if self.fail || self.blocked_images.lock().expect("blocked images").contains(id) {
                return Err("container still references image".into());
            }
            self.images.lock().expect("images").remove(id);
            Ok(())
        }
        async fn remove_registry(&self, _: &ImageCacheBinding, credential: &str, reference: &str) -> Result<(), String> {
            assert_eq!(credential, "delete-only");
            if self.blocked_registry.lock().expect("blocked registry").contains(reference) {
                return Err("registry temporarily unavailable".into());
            }
            assert!(self.removed_registry.lock().expect("registry").insert(reference.into()), "do not re-delete a tombstoned manifest");
            let backend = self.delete_host_after_registry.lock().expect("report hook").take();
            if let Some(backend) = backend {
                backend.using::<Host>("test").delete("host").await.expect("Host disappears after deletion");
            }
            Ok(())
        }
        async fn disk_free(&self) -> Result<u64, String> {
            Ok(10000 - 100 * self.images.lock().expect("images").len() as u64)
        }
    }

    async fn setup(mode: ImageGcMode, registry: bool, stale: bool, fail: bool) -> ImageDistributor<Docker> {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        backend
            .definitions::<Project>("test")
            .apply(&InputMeta::builder().name("fleet".into()).build(), &ProjectSpec::builder().display_name("Fleet".into()).build())
            .await
            .expect("project");
        backend
            .definitions::<FleetDesignation>("test")
            .apply(&InputMeta::builder().name("fleet".into()).build(), &FleetDesignationSpec {
                project: "fleet".into(),
                image_cache: registry.then(|| ImageCacheBinding {
                    repository: "registry.test/images".into(),
                    pull_credential: "pull".into(),
                    push_credential: "push".into(),
                }),
                image_gc: Some(ImageGcPolicy {
                    mode,
                    interval_seconds: 60,
                    grace_seconds: 3600,
                    registry_host: registry.then(|| "host".into()),
                    registry_credential: registry.then(|| "delete-only".into()),
                }),
            })
            .await
            .expect("fleet");
        backend.definitions::<ImageLayer>("test").apply(&InputMeta::builder().name("base".into()).build(), &layer()).await.expect("layer");
        let hosts = backend.using::<Host>("test");
        let host = hosts.create(&InputMeta::builder().name("host".into()).build(), &HostSpec::default()).await.expect("host");
        hosts
            .update_status("host", &host.metadata.resource_version, &HostStatus {
                heartbeat_at: (!stale).then(Utc::now),
                ..Default::default()
            })
            .await
            .expect("heartbeat");
        for index in 0..4 {
            version(&backend, index, index).await;
        }
        // A frozen legacy placement annotation independently pins the oldest
        // digest; it must survive even when runtime Environments are absent.
        backend
            .using::<Convoy>("test")
            .create(
                &InputMeta::builder()
                    .name("frozen".into())
                    .annotations(BTreeMap::from([(
                        "flotilla.work/placement-snapshot".into(),
                        serde_json::json!({"local_image_id":digest(0)}).to_string(),
                    )]))
                    .build(),
                &flotilla_resources::ConvoySpec::builder().workflow_ref("workflow".into()).build(),
            )
            .await
            .expect("frozen convoy");
        ImageDistributor::builder()
            .io(Arc::new(Docker { images: Mutex::new((0..4).map(digest).collect()), fail, ..Default::default() }))
            .backend(backend)
            .namespace("test".into())
            .host("host".into())
            .build()
    }

    // Preview reports the same fleet refusals as apply without retiring or
    // deleting anything, for both missing heartbeats and unhealthy replication.
    #[tokio::test]
    async fn dry_run_reports_apply_refusals_without_mutation() {
        for stale in [true, false] {
            let collector = setup(ImageGcMode::DryRun, false, stale, false).await;
            let hosts = collector.backend.using::<Host>("test");
            if !stale {
                let host = hosts.get("host").await.expect("host");
                let mut status = host.status.expect("status");
                status.conditions.push(
                    HostCondition::builder()
                        .condition_type("ResourceReplicationStreams")
                        .value(ConditionValue::False)
                        .reason("Disconnected")
                        .message("peer unavailable")
                        .observed_at(Utc::now())
                        .blocks_readiness(false)
                        .build(),
                );
                hosts.update_status("host", &host.metadata.resource_version, &status).await.expect("replication failure");
            }
            collector.collect_if_due().await.expect_err("preview surfaces apply refusal");
            let report = hosts.get("host").await.expect("host").status.expect("status").capabilities[HEALTH_CAPABILITY].clone();
            assert_eq!(report["local_candidates"], serde_json::json!([digest(1)]));
            assert!(!report["failures"].as_array().expect("failures").is_empty());
            assert_eq!(collector.io.inventory().await.expect("held").len(), 4);
            assert!(collector
                .backend
                .using::<ImageBuild>("test")
                .list()
                .await
                .expect("builds")
                .items
                .iter()
                .all(|build| { !build.status.as_ref().expect("status").availability.retired }));
        }
    }

    // A backlog makes bounded progress and schedules a prompt continuation;
    // frozen, current and previous images survive every batch.
    #[tokio::test]
    async fn collection_bounds_backlog_and_continues_without_losing_roots() {
        for registry in [false, true] {
            let collector = setup(ImageGcMode::Apply, registry, false, false).await;
            for index in 4..=21 {
                version(&collector.backend, index, index).await;
                collector.io.images.lock().expect("images").insert(digest(index));
            }
            collector.collect_if_due().await.expect("retire backlog");
            let builds = collector.backend.using::<ImageBuild>("test");
            for build in builds.list().await.expect("builds").items {
                let mut status = build.status.expect("status");
                if status.availability.retired {
                    status.availability.retired_at = Some(Utc::now() - chrono::Duration::hours(2));
                    builds
                        .update_status(&build.metadata.name, &build.metadata.resource_version, &status)
                        .await
                        .expect("quarantine elapsed");
                }
            }
            *collector.next_collection.lock().await = None;
            collector.collect_if_due().await.expect("first bounded batch");
            assert_eq!(collector.io.inventory().await.expect("held").len(), 22 - 16);
            assert_eq!(collector.io.removed_registry.lock().expect("registry").len(), if registry { 16 } else { 0 });
            let host = collector.backend.using::<Host>("test").get("host").await.expect("host");
            assert_eq!(host.status.expect("status").capabilities[HEALTH_CAPABILITY]["budget_exhausted"], true);
            *collector.next_collection.lock().await = None;
            collector.collect_if_due().await.expect("remaining batch");
            assert_eq!(collector.io.inventory().await.expect("roots"), BTreeSet::from([digest(0), digest(20), digest(21)]));
            assert_eq!(collector.io.removed_registry.lock().expect("registry").len(), if registry { 19 } else { 0 });
        }
    }

    // The per-run budget must advance past unsuccessful removals so blocked
    // images cannot starve later, independently collectable images forever.
    #[tokio::test]
    async fn collection_batches_advance_past_failed_removals() {
        for registry in [false, true] {
            let collector = setup(ImageGcMode::Apply, registry, false, false).await;
            for index in 4..=21 {
                version(&collector.backend, index, index).await;
                collector.io.images.lock().expect("images").insert(digest(index));
            }
            collector.collect_if_due().await.expect("retire backlog");
            let builds = collector.backend.using::<ImageBuild>("test");
            for build in builds.list().await.expect("builds").items {
                let mut status = build.status.expect("status");
                if status.availability.retired {
                    status.availability.retired_at = Some(Utc::now() - chrono::Duration::hours(2));
                    builds
                        .update_status(&build.metadata.name, &build.metadata.resource_version, &status)
                        .await
                        .expect("quarantine elapsed");
                }
            }
            *collector.io.blocked_images.lock().expect("blocked") = (1..=16).map(digest).collect();
            *collector.io.blocked_registry.lock().expect("blocked registry") =
                (101..=116).map(|index| format!("registry.test/images@{}", digest(index))).collect();
            *collector.next_collection.lock().await = None;
            collector.collect_if_due().await.expect("failed first batch");
            assert_eq!(collector.io.inventory().await.expect("held").len(), 22);
            *collector.next_collection.lock().await = None;
            collector.collect_if_due().await.expect("advance to healthy candidates");
            let held = collector.io.inventory().await.expect("held");
            assert_eq!(held.len(), 19);
            assert!((17..=19).all(|index| !held.contains(&digest(index))));
            assert!([0, 20, 21].iter().all(|index| held.contains(&digest(*index))));
            assert_eq!(collector.io.removed_registry.lock().expect("registry").len(), if registry { 3 } else { 0 });
        }
    }

    // HTTP boundary stand-in: concurrent status writers cause one or every
    // CAS attempt to conflict. Each retry must read a fresh resource version.
    #[cfg(not(feature = "skip-no-sandbox-tests"))]
    #[tokio::test]
    async fn report_retries_conflicts_and_bounds_exhaustion() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use axum::{
            http::StatusCode,
            routing::{get, put},
            Json, Router,
        };
        use flotilla_resources::HttpBackend;

        for conflicts in [1, 3] {
            let collector = setup(ImageGcMode::DryRun, false, false, false).await;
            let host = collector.backend.using::<Host>("test").get("host").await.expect("host");
            let object = serde_json::to_value(host.to_k8s_object()).expect("wire host");
            let reads = Arc::new(AtomicUsize::new(0));
            let writes = Arc::new(AtomicUsize::new(0));
            let read_count = Arc::clone(&reads);
            let get_object = object.clone();
            let write_count = Arc::clone(&writes);
            let app = Router::new()
                .route(
                    "/apis/flotilla.work/v1/namespaces/test/hosts/host",
                    get(move || {
                        let mut object = get_object.clone();
                        let version = read_count.fetch_add(1, Ordering::SeqCst) + 1;
                        object["metadata"]["resourceVersion"] = serde_json::json!(version.to_string());
                        async move { Json(object) }
                    }),
                )
                .route(
                    "/apis/flotilla.work/v1/namespaces/test/hosts/host/status",
                    put(move |Json(patch): Json<serde_json::Value>| {
                        let attempt = write_count.fetch_add(1, Ordering::SeqCst) + 1;
                        let mut object = object.clone();
                        async move {
                            assert_eq!(patch["metadata"]["resourceVersion"], attempt.to_string());
                            assert_eq!(patch["status"]["capabilities"][HEALTH_CAPABILITY]["dry_run"], true);
                            if attempt <= conflicts {
                                (StatusCode::CONFLICT, Json(serde_json::json!({"message": "concurrent heartbeat"})))
                            } else {
                                object["status"] = patch["status"].clone();
                                (StatusCode::OK, Json(object))
                            }
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("listener");
            let address = listener.local_addr().expect("address");
            let server = tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
            let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), format!("http://{address}")));
            let result = publish_report(&backend, "test", "host", &CollectionReport { dry_run: true, ..Default::default() }).await;
            assert_eq!(result.is_err(), conflicts == 3);
            assert_eq!(reads.load(Ordering::SeqCst), if conflicts == 3 { 3 } else { 2 });
            assert_eq!(writes.load(Ordering::SeqCst), reads.load(Ordering::SeqCst));
            server.abort();
        }
    }

    // Stored report corruption and references without build evidence cannot
    // grow health status or prevent the next conservative dry-run.
    #[tokio::test]
    async fn registry_tombstones_recover_corruption_and_drop_orphans() {
        let retained = format!("registry.test/images@{}", digest(101));
        for deleted in [serde_json::json!([retained, "registry.test/images@orphan"]), serde_json::json!({"bad": "shape"})] {
            let collector = setup(ImageGcMode::DryRun, true, false, false).await;
            let hosts = collector.backend.using::<Host>("test");
            let host = hosts.get("host").await.expect("host");
            let mut status = host.status.expect("status");
            status.capabilities.insert(HEALTH_CAPABILITY.into(), serde_json::json!({"deleted_registry": deleted}));
            hosts.update_status("host", &host.metadata.resource_version, &status).await.expect("stored report");
            collector.collect_if_due().await.expect("report recovery");
            let report = hosts.get("host").await.expect("host").status.expect("status").capabilities[HEALTH_CAPABILITY].clone();
            assert_eq!(report["deleted_registry"], if deleted.is_array() { serde_json::json!([retained]) } else { serde_json::json!([]) });
        }
    }

    // A stale retirement CAS affects one build and retries promptly; it must
    // not prevent the rest of the run from reporting its outcome.
    #[tokio::test]
    async fn retirement_conflict_is_reported_and_retried() {
        let collector = setup(ImageGcMode::Apply, false, false, false).await;
        *collector.io.conflict_retirement.lock().expect("conflict hook") = Some(collector.backend.clone());
        collector.collect_if_due().await.expect("one conflict is advisory");
        let hosts = collector.backend.using::<Host>("test");
        let status = hosts.get("host").await.expect("host").status.expect("status");
        assert!(!status.capabilities[HEALTH_CAPABILITY]["failures"].as_array().expect("failures").is_empty());
        assert!(collector.next_collection.lock().await.expect("retry") <= Instant::now() + COLLECTION_RETRY_DELAY);
        *collector.next_collection.lock().await = None;
        collector.collect_if_due().await.expect("retry");
        assert!(
            collector.backend.using::<ImageBuild>("test").get("build-1").await.expect("build").status.expect("status").availability.retired
        );
    }

    // Successful deletion survives a lost Host health write. Restoring the Host
    // must republish evidence rather than delete the same manifest again.
    #[tokio::test]
    async fn registry_deletion_survives_health_publication_failure() {
        let collector = setup(ImageGcMode::Apply, true, false, false).await;
        collector.collect_if_due().await.expect("retire");
        let builds = collector.backend.using::<ImageBuild>("test");
        let build = builds.get("build-1").await.expect("build");
        let mut status = build.status.expect("status");
        status.availability.retired_at = Some(Utc::now() - chrono::Duration::hours(2));
        builds.update_status("build-1", &build.metadata.resource_version, &status).await.expect("quarantine elapsed");
        *collector.io.delete_host_after_registry.lock().expect("report hook") = Some(collector.backend.clone());
        *collector.next_collection.lock().await = None;
        collector.collect_if_due().await.expect("deletion outcome survives report failure");
        let hosts = collector.backend.using::<Host>("test");
        let host = hosts.create(&InputMeta::builder().name("host".into()).build(), &HostSpec::default()).await.expect("restore host");
        hosts
            .update_status("host", &host.metadata.resource_version, &HostStatus { heartbeat_at: Some(Utc::now()), ..Default::default() })
            .await
            .expect("heartbeat");
        *collector.next_collection.lock().await = None;
        collector.collect_if_due().await.expect("republish deletion evidence");
        assert_eq!(collector.io.removed_registry.lock().expect("registry").len(), 1);
        let report = hosts.get("host").await.expect("host").status.expect("status").capabilities[HEALTH_CAPABILITY].clone();
        assert_eq!(report["deleted_registry"], serde_json::json!([format!("registry.test/images@{}", digest(101))]));
    }

    // Active image IO and undrained publication evidence defer retirement,
    // while the next idle run can collect normally.
    #[tokio::test]
    async fn collection_waits_for_delivery_and_publication_evidence() {
        for publication in [false, true] {
            let collector = setup(ImageGcMode::Apply, false, false, false).await;
            let gate = if publication { None } else { Some(collector.collection_gate.read().await) };
            if publication {
                let job = tokio::spawn(async { Ok("registry.test/images@digest".into()) });
                while !job.is_finished() {
                    tokio::task::yield_now().await;
                }
                collector.publications.lock().await.insert("build-1".into(), job);
            }
            collector.collect_if_due().await.expect("busy run deferred");
            let builds = collector.backend.using::<ImageBuild>("test");
            assert!(!builds.get("build-1").await.expect("old build").status.expect("status").availability.retired);
            assert_eq!(collector.io.inventory().await.expect("held").len(), 4);
            drop(gate);
            collector.publications.lock().await.clear();
            *collector.next_collection.lock().await = None;
            collector.collect_if_due().await.expect("idle run");
            assert!(builds.get("build-1").await.expect("old build").status.expect("status").availability.retired);
        }
    }

    // #2733: dry-run has no side effects; apply retains rollback and frozen pins
    // on both stores, removes only obsolete identities, and reports measured space.
    #[tokio::test]
    async fn scheduled_collection_preserves_rollback_and_frozen_convoy() {
        for registry in [false, true] {
            let collector = setup(ImageGcMode::DryRun, registry, false, false).await;
            collector.collect_if_due().await.expect("dry run");
            assert_eq!(collector.io.inventory().await.expect("held").len(), 4);
            assert!(collector.io.removed_registry.lock().expect("registry").is_empty());
            let hosts = collector.backend.using::<Host>("test");
            let report = hosts.get("host").await.expect("host").status.expect("status").capabilities[HEALTH_CAPABILITY].clone();
            assert_eq!(report["local_candidates"], serde_json::json!([digest(1)]));
            let collector = setup(ImageGcMode::Apply, registry, false, false).await;
            collector.collect_if_due().await.expect("retire");
            assert_eq!(collector.io.inventory().await.expect("quarantine").len(), 4);
            let builds = collector.backend.using::<ImageBuild>("test");
            let old = builds.get("build-1").await.expect("retired build");
            let mut status = old.status.expect("retired status");
            assert!(status.availability.retired);
            status.availability.retired_at = Some(Utc::now() - chrono::Duration::hours(2));
            builds.update_status("build-1", &old.metadata.resource_version, &status).await.expect("quarantine elapsed");
            *collector.next_collection.lock().await = None;
            collector.collect_if_due().await.expect("apply");
            assert_eq!(collector.io.inventory().await.expect("held"), BTreeSet::from([digest(0), digest(2), digest(3)]));
            let previous = collector.backend.using::<ImageBuild>("test").get("build-2").await.expect("previous");
            assert_eq!(collector.ensure(&previous).await.expect("rollback").local_image_id, digest(2));
            assert_eq!(collector.io.removed_registry.lock().expect("registry").len(), usize::from(registry));
            let host = collector.backend.using::<Host>("test").get("host").await.expect("health");
            assert_eq!(host.status.expect("status").capabilities[HEALTH_CAPABILITY]["reclaimed_local_bytes"], 100);
            *collector.next_collection.lock().await = None;
            collector.collect_if_due().await.expect("idempotent collection");
            assert_eq!(collector.io.inventory().await.expect("retained again").len(), 3);
        }
    }

    // Collection must refuse stale-fleet mutation and surface process failures;
    // it never treats a failed removal as reclaimed space.
    #[tokio::test]
    async fn collection_failure_is_advisory_and_does_not_claim_reclaimed_space() {
        for (stale, fail) in [(true, false), (false, true)] {
            let collector = setup(ImageGcMode::Apply, false, stale, fail).await;
            if fail {
                let builds = collector.backend.using::<ImageBuild>("test");
                let old = builds.get("build-1").await.expect("old");
                let mut status = old.status.expect("status");
                status.availability.retired = true;
                status.availability.retired_at = Some(Utc::now() - chrono::Duration::hours(2));
                builds.update_status("build-1", &old.metadata.resource_version, &status).await.expect("retired");
            }
            let result = collector.collect_if_due().await;
            assert_eq!(result.is_err(), stale);
            assert_eq!(collector.io.inventory().await.expect("held").len(), 4);
            let host = collector.backend.using::<Host>("test").get("host").await.expect("health");
            let status = host.status.expect("status");
            assert_eq!(status.capabilities[HEALTH_CAPABILITY]["reclaimed_local_bytes"], 0);
            assert!(status.conditions.iter().any(|condition| condition.value == ConditionValue::False && !condition.blocks_readiness));
        }
    }
    // #2733: an admitted frozen reference cancels retirement during quarantine,
    // before an immutable image is removed.
    #[tokio::test]
    async fn frozen_pin_cancels_retirement_during_quarantine() {
        let collector = setup(ImageGcMode::Apply, true, false, false).await;
        collector.collect_if_due().await.expect("retirement");
        let candidate = collector.backend.using::<ImageBuild>("test").get("build-1").await.expect("candidate");
        let handle = format!("flotilla-build:{:x}", Sha256::digest(format!("{}\0{}", candidate.metadata.name, candidate.spec.recipe_key)));
        collector
            .backend
            .using::<Convoy>("test")
            .create(
                &InputMeta::builder()
                    .name("arrived".into())
                    .annotations(BTreeMap::from([(
                        "flotilla.work/placement-snapshot".into(),
                        serde_json::json!({"image": handle}).to_string(),
                    )]))
                    .build(),
                &flotilla_resources::ConvoySpec::builder().workflow_ref("workflow".into()).build(),
            )
            .await
            .expect("new frozen pin");
        *collector.next_collection.lock().await = None;
        collector.collect_if_due().await.expect("cancel retirement");
        let build = collector.backend.using::<ImageBuild>("test").get("build-1").await.expect("pinned build");
        assert!(!build.status.as_ref().expect("status").availability.retired);
        assert_eq!(collector.ensure(&build).await.expect("frozen provisioning").local_image_id, digest(1));
        assert_eq!(collector.io.inventory().await.expect("held").len(), 4);
        assert!(collector.io.removed_registry.lock().expect("registry").is_empty());
    }

    // Reachability protects ancestor stages, not just the final image. A broken
    // dependency graph refuses collection rather than losing unknown ancestors.
    #[tokio::test]
    async fn retention_closes_parent_graph_and_refuses_missing_dependencies() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let parent = version(&backend, 0, 0).await;
        let mut child = version(&backend, 1, 1).await;
        child.spec.parent_build_ref = Some(parent.metadata.name.clone());
        child.spec.layer.name = "capability".into();
        let mut builds = BTreeMap::from([(parent.metadata.name.clone(), parent), (child.metadata.name.clone(), child)]);
        let retained = retention(&builds, &BTreeSet::new(), BTreeSet::from(["build-1".into()]), Utc::now()).expect("graph");
        assert!(retained.contains(&digest(0)));
        assert!(retained.contains(&digest(1)));
        builds.remove("build-0");
        assert!(retention(&builds, &BTreeSet::new(), BTreeSet::new(), Utc::now())
            .expect_err("missing ancestor")
            .contains("missing image build dependency"));
        builds.get_mut("build-1").expect("child").spec.parent_build_ref = Some("build-1".into());
        assert!(retention(&builds, &BTreeSet::new(), BTreeSet::new(), Utc::now()).expect_err("cycle").contains("cycle"));
    }
}
