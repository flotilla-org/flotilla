//! Current provisioning evidence for surface-agnostic convoy rows.

use flotilla_protocol::result_set::{Readiness, ReadinessBlocker, ReadinessState};
use flotilla_resources::{CheckoutPhase, ClonePhase, CrewWorkPhase, CrewWorkState, VesselPhase};

use super::*;

impl Aggregator {
    pub(super) async fn recover_clone_readiness_watch(
        &mut self,
        resolver: &dyn AggregatorWatchSource<CloneResource>,
    ) -> Result<WatchStream<CloneResource>, ResourceError> {
        loop {
            let recovered = async {
                let listed = resolver.list().await?;
                let watch = resolver.watch(WatchStart::resuming_from(&listed)).await?;
                self.readiness_clones = readiness_resources(
                    listed.items.into_iter().map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local }).collect(),
                );
                Ok(watch)
            }
            .await;
            match recovered {
                Err(ResourceError::WatchExpired { .. }) => tokio::time::sleep(Self::WATCH_RESTART_BACKOFF).await,
                result => return result,
            }
        }
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
        let vessel = self.readiness_vessels.values().find(|source| {
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
                self.readiness_checkouts
                    .values()
                    .filter(|source| {
                        source.object.metadata.namespace == convoy.namespace
                            && source.object.metadata.labels.get(CONVOY_LABEL) == Some(&convoy.name)
                            && self.read_host(&source.provenance).as_ref() == Some(host)
                    })
                    .map(|source| source.object.metadata.name.clone()),
            );
            for name in &checkout_refs {
                let source = self.readiness_checkouts.values().find(|source| {
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
                let status = source.object.status.as_ref();
                let phase = status.map(|status| status.phase).unwrap_or_default();
                if phase == CheckoutPhase::Ready {
                    continue;
                }
                if let CheckoutSpec::Worktree(spec) = &source.object.spec {
                    if let Some(clone) = self.readiness_clones.values().find(|clone| {
                        clone.object.metadata.namespace == convoy.namespace
                            && clone.object.metadata.name == spec.clone_ref
                            && self.read_host(&clone.provenance).as_ref() == Some(host)
                    }) {
                        if let Some(status) = clone.object.status.as_ref().filter(|status| status.phase != ClonePhase::Ready) {
                            let state = if status.phase == ClonePhase::Failed && status.failure_policy.is_some() {
                                ReadinessState::Failed
                            } else if status.retry.is_some() || status.phase == ClonePhase::Failed {
                                ReadinessState::Blocked
                            } else {
                                ReadinessState::Provisioning
                            };
                            result.state = result.state.max(state);
                            result.blockers.push(ReadinessBlocker {
                                resource: ResourceRef::new(
                                    api_version(CloneResource::API_PATHS),
                                    CloneResource::API_PATHS.kind,
                                    &convoy.namespace,
                                    &spec.clone_ref,
                                )
                                .on_host(host.clone()),
                                phase: format!("{:?}", status.phase),
                                reason: status.message.clone().unwrap_or_else(|| "clone is not ready".into()),
                            });
                        }
                    }
                }
                let state = if phase == CheckoutPhase::Failed {
                    ReadinessState::Failed
                } else if status.is_some_and(|status| status.clone_retry.is_some()) {
                    ReadinessState::Blocked
                } else {
                    ReadinessState::Provisioning
                };
                result.state = result.state.max(state);
                result.blockers.push(ReadinessBlocker {
                    resource: ResourceRef::new(api_version(Checkout::API_PATHS), Checkout::API_PATHS.kind, &convoy.namespace, name)
                        .on_host(host.clone()),
                    phase: format!("{phase:?}"),
                    reason: status.and_then(|status| status.message.clone()).unwrap_or_else(|| "checkout is not ready".into()),
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
