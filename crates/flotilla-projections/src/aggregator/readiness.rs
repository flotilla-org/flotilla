//! Current provisioning evidence for surface-agnostic convoy rows.

use flotilla_protocol::{
    result_set::{Readiness, ReadinessBlocker, ReadinessState},
    LifecycleReading,
};
use flotilla_resources::checkout_records::{CheckoutChange, CheckoutRecord};
use flotilla_resources::{CrewWorkPhase, CrewWorkState, VesselPhase};

use super::*;
use flotilla_resources::vessel_records::engine_vessel;

impl Aggregator {
    pub(super) fn vessel_readiness(
        &self,
        convoy: &ResourceRef,
        definition: &VesselRequirement,
        work: Option<&WorkState>,
        crew: Option<&BTreeMap<String, CrewWorkState>>,
        host: &HostName,
        checkouts: &HeldCheckouts,
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
            let origin = engine_origin(&vessel.provenance);
            let engine = match engine_vessel(self.lifecycle_reads.as_ref(), &vessel.object, origin.as_deref()) {
                Ok(record) => record.map(|record| record.reading),
                Err(error) => {
                    return (
                        Readiness {
                            state: ReadinessState::Blocked,
                            blockers: vec![ReadinessBlocker {
                                resource: ResourceRef::new(
                                    api_version(Vessel::API_PATHS),
                                    Vessel::API_PATHS.kind,
                                    &convoy.namespace,
                                    &vessel.object.metadata.name,
                                )
                                .on_host(host.clone()),
                                phase: "Unknown".into(),
                                reason: error.clone(),
                            }],
                        },
                        Some(error),
                    );
                }
            };
            let status = engine.is_none().then_some(vessel.object.status.as_ref()).flatten();
            let vessel_phase = VesselPhase::from_reading(engine.as_ref(), vessel.object.status.as_ref());
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
                    reason: vessel_blocker_reason(engine.as_ref(), vessel, vessel_phase),
                });
            }
            // Lost backing is frozen. Missing child evidence cannot turn it
            // back into provisioning or replace the recovery explanation.
            if vessel_phase == VesselPhase::Lost {
                // This same Lost phase always pushed a blocker above, before any child traversal.
                let loss_reason = result.blockers.last().expect("lost vessel blocker").reason.clone();
                return (result, Some(loss_reason));
            }
            // The vessel's checkouts: those it records and adopts, and the ones the engine holds for
            // the convoy at the vessel's home, provisioning before the ready vessel records them.
            let held = match checkouts {
                Ok(held) => held,
                Err(error) => {
                    result.state = result.state.max(ReadinessState::Blocked);
                    result.blockers.push(ReadinessBlocker {
                        resource: convoy.subresource(format!("vessels/{}/checkouts", definition.name)),
                        phase: "Unknown".into(),
                        reason: error.clone(),
                    });
                    return (result, None);
                }
            };
            let vessel_home = engine_origin(&vessel.provenance);
            let at_vessel_home = |checkout: &CheckoutRecord| match &vessel_home {
                Some(home) => checkout.home == *home,
                None => !self.known_origins.contains(&flotilla_protocol::NodeId::new(checkout.home.clone())),
            };
            let mut checkout_refs =
                status.map(|status| status.checkout_refs.values().cloned().collect::<BTreeSet<_>>()).unwrap_or_default();
            checkout_refs.extend(vessel.object.spec.adopted_checkout_refs.values().cloned());
            checkout_refs
                .extend(held.values().filter(|checkout| at_vessel_home(checkout)).map(|checkout| checkout.checkout_name().to_owned()));
            for name in &checkout_refs {
                let checkout_ref = || {
                    ResourceRef::new(api_version(Checkout::API_PATHS), Checkout::API_PATHS.kind, &convoy.namespace, name)
                        .on_host(host.clone())
                };
                let Some(checkout) = held.get(name).filter(|checkout| at_vessel_home(checkout)) else {
                    result.state = result.state.max(ReadinessState::Provisioning);
                    result.blockers.push(ReadinessBlocker {
                        resource: checkout_ref(),
                        phase: "Unknown".into(),
                        reason: "waiting for checkout evidence".into(),
                    });
                    continue;
                };
                let Some((state, phase, reason)) = reading_blocker(&checkout.reading, "checkout is not ready") else {
                    continue;
                };
                if let CheckoutSpec::Worktree(spec) = &checkout.spec {
                    // The clone launch the checkout was made from, at the checkout's home (#3108).
                    let clone_ref = || {
                        ResourceRef::new(
                            api_version(CloneResource::API_PATHS),
                            CloneResource::API_PATHS.kind,
                            &convoy.namespace,
                            &spec.clone_ref,
                        )
                        .on_host(host.clone())
                    };
                    match self.lifecycle_reads.clones().and_then(|clones| clones.get(&spec.clone_ref, Some(&checkout.home))) {
                        Ok(Some(clone)) => {
                            if let Some((state, phase, reason)) = reading_blocker(&clone.reading, "clone is not ready") {
                                result.state = result.state.max(state);
                                result.blockers.push(ReadinessBlocker { resource: clone_ref(), phase, reason });
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            result.state = result.state.max(ReadinessState::Blocked);
                            result.blockers.push(ReadinessBlocker { resource: clone_ref(), phase: "Unknown".into(), reason: error });
                            continue;
                        }
                    }
                }
                result.state = result.state.max(state);
                result.blockers.push(ReadinessBlocker { resource: checkout_ref(), phase, reason });
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
/// The checkouts the engine holds for one convoy, by checkout name, or why they could not be read.
pub(super) type HeldCheckouts = Result<BTreeMap<String, CheckoutRecord>, String>;

/// Waits for the engine's next checkout commit: true when the checkouts changed (or changes were
/// missed), false once the changes end; never returns where none are followed.
pub(super) async fn next_checkout_change(changes: &mut Option<tokio::sync::broadcast::Receiver<CheckoutChange>>) -> bool {
    let Some(receiver) = changes.as_mut() else {
        return std::future::pending().await;
    };
    match receiver.recv().await {
        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
            *changes = None;
            false
        }
    }
}

pub(super) fn engine_origin(provenance: &ResourceProvenance) -> Option<String> {
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
        LifecycleReading::Failed { .. } | LifecycleReading::Lost { .. } | LifecycleReading::Released | LifecycleReading::GaveUp { .. } => {
            ReadinessState::Failed
        }
    };
    Some((state, reading.label().to_owned(), reading.reason().unwrap_or_else(|| not_ready.to_owned())))
}

pub(super) fn vessel_blocker_reason(engine: Option<&LifecycleReading>, vessel: &ReadResourceObject<Vessel>, phase: VesselPhase) -> String {
    engine
        .and_then(LifecycleReading::reason)
        .or_else(|| {
            if engine.is_none() || phase != VesselPhase::Interrupted {
                return None;
            }
            let roles = vessel.object.status.iter().flat_map(|status| status.interrupted_roles.iter()).cloned().collect::<Vec<_>>();
            (!roles.is_empty()).then(|| format!("crew roles are interrupted: {}", roles.join(", ")))
        })
        .or_else(|| engine.is_none().then(|| vessel.object.status.as_ref().and_then(|status| status.message.clone())).flatten())
        .unwrap_or_else(|| {
            if phase == VesselPhase::Lost {
                "lost, recoverable; rehydration is not available yet (#2872)".into()
            } else {
                "vessel is not ready".into()
            }
        })
}
