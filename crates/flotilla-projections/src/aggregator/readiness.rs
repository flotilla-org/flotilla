//! Current provisioning evidence for surface-agnostic convoy rows.

use flotilla_protocol::{
    result_set::{Readiness, ReadinessBlocker, ReadinessState},
    LifecycleReading,
};
use flotilla_resources::{CheckoutPhase, ClonePhase, CrewWorkPhase, CrewWorkState, VesselPhase};

use super::*;
use crate::ports::ProvisioningKind;

impl Aggregator {
    pub(super) async fn recover_clone_readiness_watch(
        &mut self,
        resolver: &dyn AggregatorWatchSource<CloneResource>,
    ) -> Result<WatchStream<CloneResource>, ResourceError> {
        let (listed, watch) = recover_local_view(resolver).await?;
        self.readiness_clones = readiness_resources(
            listed.items.into_iter().map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local }).collect(),
        );
        Ok(watch)
    }

    pub(super) fn vessel_readiness(
        &self,
        convoy: &ResourceRef,
        definition: &VesselRequirement,
        work: Option<&WorkState>,
        crew: Option<&BTreeMap<String, CrewWorkState>>,
        host: &HostName,
    ) -> (Readiness, Option<String>) {
        let phase = work.map(|work| work.phase).unwrap_or(ResourceWorkPhase::Pending);
        if phase.is_terminal() {
            return (
                Readiness {
                    state: if phase == ResourceWorkPhase::Failed { ReadinessState::Failed } else { ReadinessState::Ready },
                    blockers: Vec::new(),
                },
                None,
            );
        }
        let mut result = Readiness {
            state: if matches!(phase, ResourceWorkPhase::Running | ResourceWorkPhase::Stalled) {
                ReadinessState::Ready
            } else {
                ReadinessState::Provisioning
            },
            blockers: Vec::new(),
        };
        let vessel = self.convoy_vessels(&convoy.namespace, &convoy.name).find(|source| {
            source.object.metadata.namespace == convoy.namespace
                && source.object.spec.convoy_ref == convoy.name
                && source.object.spec.vessel_name == definition.name
                && self.read_host(&source.provenance).as_ref() == Some(host)
        });
        if let Some(vessel) = vessel {
            let status = vessel.object.status.as_ref();
            let vessel_phase = status.map(|status| status.phase).unwrap_or_default();
            if vessel_phase == VesselPhase::Ready {
                result.state = ReadinessState::Ready;
            } else {
                result.state = match vessel_phase {
                    VesselPhase::Failed => ReadinessState::Failed,
                    VesselPhase::Lost => ReadinessState::Blocked,
                    _ => ReadinessState::Provisioning,
                };
                result.blockers.push(ReadinessBlocker {
                    resource: ResourceRef::new(
                        api_version(Vessel::API_PATHS),
                        Vessel::API_PATHS.kind,
                        &convoy.namespace,
                        &vessel.object.metadata.name,
                    )
                    .on_host(host.clone()),
                    phase: format!("{vessel_phase:?}"),
                    reason: status.and_then(|status| status.message.clone()).unwrap_or_else(|| {
                        if vessel_phase == VesselPhase::Lost {
                            // TODO(#2872): remove unavailable-rehydration guidance when recovery ships.
                            "lost, recoverable; rehydration is not available yet (#2872)".into()
                        } else {
                            "vessel is not ready".into()
                        }
                    }),
                });
            }
            // Lost backing is frozen. Missing child evidence cannot turn it
            // back into provisioning or replace the recovery explanation.
            if vessel_phase == VesselPhase::Lost {
                // This same Lost phase always pushed a blocker above, before any child traversal.
                let loss_reason = result.blockers.last().expect("lost vessel blocker").reason.clone();
                return (result, Some(loss_reason));
            }
            let mut checkout_refs =
                status.map(|status| status.checkout_refs.values().cloned().collect::<BTreeSet<_>>()).unwrap_or_default();
            checkout_refs.extend(vessel.object.spec.adopted_checkout_refs.values().cloned());
            // Provisioning checkouts belong to the convoy before the ready
            // vessel records their refs. Host-scoped membership avoids joining
            // a same-named replica or another convoy's checkout.
            checkout_refs.extend(
                self.convoy_checkouts(&convoy.namespace, &convoy.name)
                    .filter(|source| {
                        source.object.metadata.namespace == convoy.namespace
                            && source.object.metadata.labels.get(CONVOY_LABEL) == Some(&convoy.name)
                            && self.read_host(&source.provenance).as_ref() == Some(host)
                    })
                    .map(|source| source.object.metadata.name.clone()),
            );
            for name in &checkout_refs {
                let source = self.named_checkouts(&convoy.namespace, name).find(|source| {
                    source.object.metadata.namespace == convoy.namespace
                        && source.object.metadata.name == *name
                        && self.read_host(&source.provenance).as_ref() == Some(host)
                });
                let Some(source) = source else {
                    result.state = result.state.max(ReadinessState::Provisioning);
                    result.blockers.push(ReadinessBlocker {
                        resource: ResourceRef::new(api_version(Checkout::API_PATHS), Checkout::API_PATHS.kind, &convoy.namespace, name)
                            .on_host(host.clone()),
                        phase: "Unknown".into(),
                        reason: "waiting for checkout evidence".into(),
                    });
                    continue;
                };
                // Behind `[primitives]` the engine's reading of the checkout decides (#3629 layer 4).
                let origin = engine_origin(&source.provenance);
                let engine = self.lifecycle_reads.reading(ProvisioningKind::Checkout, name, origin.as_deref());
                let status = source.object.status.as_ref();
                let phase = status.map(|status| status.phase).unwrap_or_default();
                if engine.as_ref().map_or(phase == CheckoutPhase::Ready, |reading| matches!(reading, LifecycleReading::Ready { .. })) {
                    continue;
                }
                if let CheckoutSpec::Worktree(spec) = &source.object.spec {
                    if let Some(clone) = self.named_clones(&convoy.namespace, &spec.clone_ref).find(|clone| {
                        clone.object.metadata.namespace == convoy.namespace
                            && clone.object.metadata.name == spec.clone_ref
                            && self.read_host(&clone.provenance).as_ref() == Some(host)
                    }) {
                        let clone_ref = || {
                            ResourceRef::new(
                                api_version(CloneResource::API_PATHS),
                                CloneResource::API_PATHS.kind,
                                &convoy.namespace,
                                &spec.clone_ref,
                            )
                            .on_host(host.clone())
                        };
                        let reading = self.lifecycle_reads.reading(
                            ProvisioningKind::Clone,
                            &spec.clone_ref,
                            engine_origin(&clone.provenance).as_deref(),
                        );
                        if let Some(reading) = reading {
                            if let Some((state, phase, reason)) = reading_blocker(&reading, "clone is not ready") {
                                result.state = result.state.max(state);
                                result.blockers.push(ReadinessBlocker { resource: clone_ref(), phase, reason });
                            }
                        } else if let Some(status) = clone.object.status.as_ref().filter(|status| status.phase != ClonePhase::Ready) {
                            let state = if status.phase == ClonePhase::Failed && status.failure_policy.is_some() {
                                ReadinessState::Failed
                            } else if status.retry.is_some() || status.phase == ClonePhase::Failed {
                                ReadinessState::Blocked
                            } else {
                                ReadinessState::Provisioning
                            };
                            result.state = result.state.max(state);
                            result.blockers.push(ReadinessBlocker {
                                resource: clone_ref(),
                                phase: format!("{:?}", status.phase),
                                reason: status.message.clone().unwrap_or_else(|| "clone is not ready".into()),
                            });
                        }
                    }
                }
                let (state, phase, reason) = match engine.as_ref().and_then(|reading| reading_blocker(reading, "checkout is not ready")) {
                    Some(blocker) => blocker,
                    None => (
                        if phase == CheckoutPhase::Failed {
                            ReadinessState::Failed
                        } else if status.is_some_and(|status| status.clone_retry.is_some()) {
                            ReadinessState::Blocked
                        } else {
                            ReadinessState::Provisioning
                        },
                        format!("{phase:?}"),
                        status.and_then(|status| status.message.clone()).unwrap_or_else(|| "checkout is not ready".into()),
                    ),
                };
                result.state = result.state.max(state);
                result.blockers.push(ReadinessBlocker {
                    resource: ResourceRef::new(api_version(Checkout::API_PATHS), Checkout::API_PATHS.kind, &convoy.namespace, name)
                        .on_host(host.clone()),
                    phase,
                    reason,
                });
            }
        }
        for role in definition
            .eagerly_started_roles()
            .into_iter()
            .filter(|role| definition.crew.iter().any(|member| member.role == *role && matches!(member.source, CrewSource::Agent { .. })))
        {
            let phase = crew.and_then(|crew| crew.get(&role)).map(|crew| crew.phase).unwrap_or_default();
            if phase == CrewWorkPhase::Pending {
                result.state = result.state.max(ReadinessState::Provisioning);
                result.blockers.push(ReadinessBlocker {
                    resource: convoy.subresource(format!("vessels/{}/crew/{role}", definition.name)),
                    phase: "Pending".into(),
                    reason: format!("crew {role} has not started"),
                });
            }
        }
        if phase == ResourceWorkPhase::Pending {
            result.state = result.state.max(ReadinessState::Provisioning);
            result.blockers.push(ReadinessBlocker {
                resource: convoy.subresource(format!("vessels/{}", definition.name)),
                phase: "Pending".into(),
                reason: work.and_then(|work| work.message.clone()).unwrap_or_else(|| {
                    if definition.depends_on.is_empty() {
                        "awaiting admission".into()
                    } else {
                        format!("awaiting dependencies: {}", definition.depends_on.join(", "))
                    }
                }),
            });
        }
        (result, None)
    }
}

pub(super) fn readiness_resources<T: Resource>(items: Vec<ReadResourceObject<T>>) -> BTreeMap<RepositorySourceKey, ReadResourceObject<T>> {
    items
        .into_iter()
        .map(|source| (repository_source_key(&source.object.metadata.namespace, &source.object.metadata.name, &source.provenance), source))
        .collect()
}

pub(super) fn apply_readiness_event<T: Resource>(
    resources: &mut BTreeMap<RepositorySourceKey, ReadResourceObject<T>>,
    event: ReadWatchEvent<T>,
) {
    match event {
        ReadWatchEvent::Added(source) | ReadWatchEvent::Modified(source) => {
            resources
                .insert(repository_source_key(&source.object.metadata.namespace, &source.object.metadata.name, &source.provenance), source);
        }
        ReadWatchEvent::Deleted(source) => {
            resources.remove(&repository_source_key(&source.object.metadata.namespace, &source.object.metadata.name, &source.provenance));
        }
        ReadWatchEvent::DeletedByName { tombstone, provenance } => {
            resources.remove(&repository_source_key(&tombstone.namespace, &tombstone.name, &provenance));
        }
    }
}

/// The engine home of a resource read from `provenance`: a replica's origin root, or `None` for
/// this home.
fn engine_origin(provenance: &ResourceProvenance) -> Option<String> {
    match provenance {
        ResourceProvenance::Local => None,
        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.to_string()),
    }
}

/// The readiness an engine reading implies, as its state, phase label and reason; `None` once
/// ready.
fn reading_blocker(reading: &LifecycleReading, not_ready: &str) -> Option<(ReadinessState, String, String)> {
    let state = match reading {
        LifecycleReading::Ready { .. } => return None,
        LifecycleReading::InProgress => ReadinessState::Provisioning,
        LifecycleReading::Retrying { .. } | LifecycleReading::Exhausted { .. } => ReadinessState::Blocked,
        // Exhausted gives up on the next reconcile; until then a retry may still be under way.
        // An ended checkout or clone never comes back: the engine has no incarnations, so a vessel
        // waiting on it waits on a replacement under a new name, and readiness says so.
        LifecycleReading::Lost | LifecycleReading::Released | LifecycleReading::Gone | LifecycleReading::GaveUp { .. } => {
            ReadinessState::Failed
        }
    };
    Some((state, reading.label().to_owned(), reading.reason().unwrap_or_else(|| not_ready.to_owned())))
}
