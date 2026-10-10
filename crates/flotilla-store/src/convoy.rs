use crate::{
    controller::{
        delete_lifecycle_owned_matching, LabelMappedWatch, ReconcileOutcome as ControllerReconcileOutcome, Reconciler,
        ReplicaLabelMappedWatch, SecondaryWatch,
    },
    DefinitionResolver, PreparedSnapshotGarbageCollector, ReplicaReadResolver, TypedResolver,
};
use chrono::{DateTime, Utc};
use flotilla_protocol::{Relationship, Subject};
use flotilla_resources::convoy::reconcile::*;
use flotilla_resources::*;
use std::{collections::BTreeMap, marker::PhantomData, sync::Arc};
// Convoy secondary watches cover work/checkouts, not arbitrary PR or artifact
// records. A bounded retry also handles observation freshness without an event.
const PROMISE_OBSERVATION_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
pub struct ConvoyReconciler {
    templates: DefinitionResolver<WorkflowTemplate>,
    vessels: Option<TypedResolver<Vessel>>,
    federated_vessels: Option<ReplicaReadResolver<Vessel>>,
    terminal_sessions: Option<TypedResolver<TerminalSession>>,
    checkouts: Option<TypedResolver<Checkout>>,
    federated_checkouts: Option<ReplicaReadResolver<Checkout>>,
    artifacts: Option<ReplicaReadResolver<Artifact>>,
    change_requests: Option<ReplicaReadResolver<ChangeRequest>>,
    forges: Option<DefinitionResolver<Forge>>,
    hosts: Option<ReplicaReadResolver<Host>>,
    change_request_stale_after: std::time::Duration,
    landing_evidence_stale_after: std::time::Duration,
    clock: Arc<dyn Clock>,
    prepared_snapshot_gc: Option<PreparedSnapshotGarbageCollector>,
    teardown_runtime: Option<Arc<dyn ConvoyTeardownRuntime>>,
}

#[derive(Debug, Clone)]
pub struct ConvoyPrepared {
    template: Option<ResourceObject<WorkflowTemplate>>,
    vessels: BTreeMap<String, ResourceObject<Vessel>>,
    terminal_sessions: Vec<ResourceObject<TerminalSession>>,
    checkouts: BTreeMap<String, ResourceObject<Checkout>>,
    observed_subjects: Vec<Subject>,
    promise_patch: Option<ConvoyStatusPatch>,
    exit_disposition: Option<String>,
    settlement_evidence: Option<SettlementEvaluation>,
    settlement_attention: Option<crate::ConvoyAttention>,
    reclaim_eligible: bool,
    capacity_wait: Option<String>,
}

impl ConvoyReconciler {
    pub fn new(templates: DefinitionResolver<WorkflowTemplate>) -> Self {
        Self {
            templates,
            vessels: None,
            federated_vessels: None,
            terminal_sessions: None,
            checkouts: None,
            federated_checkouts: None,
            artifacts: None,
            change_requests: None,
            forges: None,
            hosts: None,
            change_request_stale_after: std::time::Duration::from_secs(180),
            landing_evidence_stale_after: std::time::Duration::from_secs(30),
            clock: Arc::new(SystemClock),
            prepared_snapshot_gc: None,
            teardown_runtime: None,
        }
    }

    pub fn with_artifacts(mut self, artifacts: ReplicaReadResolver<Artifact>) -> Self {
        self.artifacts = Some(artifacts);
        self
    }

    pub fn with_vessels(mut self, vessels: TypedResolver<Vessel>) -> Self {
        self.vessels = Some(vessels);
        self
    }

    pub fn with_hosts(mut self, hosts: ReplicaReadResolver<Host>) -> Self {
        self.hosts = Some(hosts);
        self
    }

    pub fn with_federated_vessels(mut self, vessels: ReplicaReadResolver<Vessel>) -> Self {
        self.federated_vessels = Some(vessels);
        self
    }

    pub fn with_terminal_sessions(mut self, terminal_sessions: TypedResolver<TerminalSession>) -> Self {
        self.terminal_sessions = Some(terminal_sessions);
        self
    }

    pub fn with_checkouts(mut self, checkouts: TypedResolver<Checkout>) -> Self {
        self.checkouts = Some(checkouts);
        self
    }

    pub fn with_federated_checkouts(mut self, checkouts: ReplicaReadResolver<Checkout>) -> Self {
        self.federated_checkouts = Some(checkouts);
        self
    }

    pub fn with_change_requests(mut self, change_requests: ReplicaReadResolver<ChangeRequest>, stale_after: std::time::Duration) -> Self {
        self.change_requests = Some(change_requests);
        self.change_request_stale_after = stale_after;
        self
    }

    pub fn with_forges(mut self, forges: DefinitionResolver<Forge>) -> Self {
        self.forges = Some(forges);
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_landing_evidence_stale_after(mut self, stale_after: std::time::Duration) -> Self {
        self.landing_evidence_stale_after = stale_after;
        self
    }

    pub fn with_prepared_snapshot_gc(mut self, collector: PreparedSnapshotGarbageCollector) -> Self {
        self.prepared_snapshot_gc = Some(collector);
        self
    }

    pub fn with_teardown_runtime(mut self, runtime: Arc<dyn ConvoyTeardownRuntime>) -> Self {
        self.teardown_runtime = Some(runtime);
        self
    }

    pub fn secondary_watches() -> Vec<Box<dyn SecondaryWatch<Primary = Convoy>>> {
        vec![
            Box::new(LabelMappedWatch::<Vessel, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<TerminalSession, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<Checkout, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
        ]
    }

    pub fn federated_secondary_watches(
        backend: &crate::ResourceBackend,
        namespace: &str,
    ) -> Vec<Box<dyn SecondaryWatch<Primary = Convoy>>> {
        vec![
            Box::new(ReplicaLabelMappedWatch::<Vessel, Convoy> {
                label_key: CONVOY_LABEL,
                resolver: backend.including_replicas::<Vessel>(namespace),
                _marker: PhantomData,
            }),
            Box::new(LabelMappedWatch::<TerminalSession, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(ReplicaLabelMappedWatch::<Checkout, Convoy> {
                label_key: CONVOY_LABEL,
                resolver: backend.including_replicas::<Checkout>(namespace),
                _marker: PhantomData,
            }),
        ]
    }
}

async fn federated_children<T: Resource + std::clone::Clone>(
    resolver: &ReplicaReadResolver<T>,
    convoy: &ResourceObject<Convoy>,
) -> Result<BTreeMap<String, ResourceObject<T>>, ResourceError> {
    Ok(select_convoy_children(convoy, &resolver.list().await?.items))
}

impl Reconciler for ConvoyReconciler {
    type Resource = Convoy;
    type Prepared = ConvoyPrepared;

    async fn prepare(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Prepared, ResourceError> {
        let capacity_wait = if obj.status.as_ref().is_none_or(|status| {
            status.provisioning.is_none_or(|state| state == flotilla_resources::convoy::ConvoyProvisioningState::NotStarted)
        }) {
            match &self.hosts {
                Some(hosts) => {
                    let available_hosts = hosts.list().await?;
                    let mut decisions =
                        obj.status.as_ref().and_then(|status| status.placement_decision.as_ref()).into_iter().collect::<Vec<_>>();
                    let vessel_pins = obj
                        .metadata
                        .annotations
                        .get(flotilla_resources::convoy::VESSEL_PLACEMENTS_ANNOTATION)
                        .and_then(|encoded| {
                            serde_json::from_str::<BTreeMap<String, flotilla_resources::convoy::VesselPlacementPin>>(encoded).ok()
                        })
                        .unwrap_or_default();
                    decisions.extend(vessel_pins.values().map(|pin| &pin.decision));
                    let mut wait = None;
                    for allocation in decisions.into_iter().filter_map(|decision| decision.allocation.as_ref()) {
                        let Some(selected) = allocation.candidates.iter().find(|candidate| candidate.kind == allocation.chosen_kind) else {
                            continue;
                        };
                        let canonical =
                            match crate::canonical_host_id(available_hosts.items.iter().map(|host| &host.object), &selected.host) {
                                Ok(canonical) => canonical,
                                Err(error) => {
                                    wait = Some(format!(
                                        "capacity for fulfilment `{}` on host `{}` is unavailable: {error}",
                                        selected.kind, selected.host
                                    ));
                                    break;
                                }
                            };
                        let host = canonical.and_then(|id| {
                            available_hosts
                                .items
                                .iter()
                                .find(|host| host.object.metadata.name == id.as_str())
                                .map(|host| host.object.clone())
                        });
                        let status = host.and_then(|host| host.status).map(|mut status| {
                            status.apply_heartbeat_readiness(self.clock.now());
                            status
                        });
                        let sleeping =
                            status.as_ref().and_then(|status| status.sleeping_until).is_some_and(|until| until > self.clock.now());
                        let slots = status
                            .as_ref()
                            .and_then(|status| status.fulfilment_facts.get(&selected.kind))
                            .and_then(|facts| facts.free_vessel_slots);
                        if status.as_ref().is_none_or(|status| !status.ready) || sleeping || slots == Some(0) {
                            wait = Some(format!(
                                "capacity for fulfilment `{}` on host `{}` is unavailable{}{}",
                                selected.kind,
                                selected.host,
                                if sleeping { " (sleeping)" } else { "" },
                                if slots == Some(0) { " (no free vessel slots)" } else { "" }
                            ));
                            break;
                        }
                    }
                    wait
                }
                None => None,
            }
        } else {
            None
        };
        let template = if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() {
            None
        } else {
            match self.templates.get(pinned_workflow_ref(obj)).await {
                Ok(template) => Some(template),
                Err(ResourceError::NotFound { .. }) => None,
                Err(err) => return Err(err),
            }
        };
        let vessels = match (&self.federated_vessels, &self.vessels) {
            (Some(vessels), _) if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() => {
                federated_children(vessels, obj).await?
            }
            (None, Some(vessels)) if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() => vessels
                .list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())]))
                .await?
                .items
                .into_iter()
                .map(|workspace| (workspace.metadata.name.clone(), workspace))
                .collect(),
            _ => BTreeMap::new(),
        };

        let checkouts = match (&self.federated_checkouts, &self.checkouts) {
            (Some(checkouts), _) if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() => {
                federated_children(checkouts, obj).await?
            }
            (None, Some(checkouts)) if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() => checkouts
                .list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())]))
                .await?
                .items
                .into_iter()
                .map(|checkout| (checkout.metadata.name.clone(), checkout))
                .collect(),
            _ => BTreeMap::new(),
        };
        let terminal_sessions = match &self.terminal_sessions {
            Some(sessions) if obj.status.as_ref().is_some_and(|status| status.phase.is_terminal()) => {
                sessions.list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())])).await?.items
            }
            _ => Vec::new(),
        };
        let forges = match &self.forges {
            Some(forges) => forges.list().await?.into_iter().map(|forge| forge.spec).collect::<Vec<_>>(),
            None => Vec::new(),
        };
        let observed_subjects = observed_change_request_subjects(obj, &checkouts, &forges).map_err(ResourceError::other)?;
        let is_landing = obj.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landing);
        let needs_pr = is_landing
            || obj.status.as_ref().is_some_and(|status| {
                flotilla_resources::convoy::promises::needs_observation(status, flotilla_resources::convoy::promises::PromiseKind::Pr)
            });
        let needs_ledger = obj.status.as_ref().is_some_and(|status| {
            flotilla_resources::convoy::promises::needs_observation(
                status,
                flotilla_resources::convoy::promises::PromiseKind::DecisionLedger,
            )
        });
        let change_requests = match &self.change_requests {
            Some(change_requests) if needs_pr => {
                let sources = change_requests.list().await?.items;
                crate::select_change_requests(sources.iter().map(|source| &source.object))
                    .into_values()
                    .map(|record| (crate::change_request_record_name(&record.spec.service, &record.spec.scope, record.spec.number), record))
                    .collect()
            }
            _ => BTreeMap::new(),
        };
        let artifacts = match &self.artifacts {
            Some(artifacts) if needs_ledger => {
                artifacts.list().await?.items.into_iter().map(|item| (item.object.metadata.name.clone(), item.object)).collect()
            }
            _ => BTreeMap::new(),
        };
        let promise_patch = obj.status.as_ref().filter(|status| !status.phase.is_terminal()).and_then(|status| {
            flotilla_resources::convoy::promises::next_observation(
                status,
                &obj.metadata.name,
                &change_requests,
                &artifacts,
                self.clock.now(),
                self.change_request_stale_after,
            )
        });
        let settlement = if is_landing {
            Some(evaluate_landing_settlement_with_disposition(
                obj,
                &vessels,
                &checkouts,
                &change_requests,
                self.change_request_stale_after,
                self.landing_evidence_stale_after,
                self.clock.now(),
            ))
        } else {
            None
        };
        let exit_disposition = settlement.as_ref().and_then(|settlement| settlement.disposition.clone());
        let settlement_attention = settlement.as_ref().and_then(|settlement| {
            settlement.evaluation.unmet.iter().find_map(|unmet| match unmet {
                UnmetSettlementExpectation::ObservedDigestMismatch { reference, claimed, observed } => Some(
                    crate::ConvoyAttention::builder()
                        .source("observed-digest".to_string())
                        .reason(format!("remote ref {reference} is at {observed}, but the approved claim names {claimed}"))
                        .raised_at(self.clock.now())
                        .build(),
                ),
                _ => None,
            })
        });
        let reclaim_eligible = if obj.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            let result = match &self.teardown_runtime {
                Some(runtime) => {
                    let checkout_list = checkouts.values().cloned().collect::<Vec<_>>();
                    runtime.verify_reclaim(obj, &checkout_list).await
                }
                None => Err("convoy reclaim verifier unavailable".to_string()),
            };
            for session in &terminal_sessions {
                let (session_disposition, reason) = if session.metadata.deletion_timestamp.is_some() {
                    ("already_deleting", "session deletion already requested")
                } else if !matches!(session.metadata.lifecycle_authority(), Ok(None | Some(LifecycleAuthority::Managed))) {
                    ("unmanaged", "session lifecycle is not managed")
                } else if let Err(reason) = &result {
                    ("retain", reason.as_str())
                } else {
                    ("request_deletion", "convoy teardown verified")
                };
                tracing::info!(
                    convoy = %obj.metadata.name,
                    session = %session.metadata.name,
                    gate_outcome = if result.is_ok() { "allowed" } else { "refused" },
                    session_disposition,
                    reason,
                    "terminal session reclaim decision"
                );
            }
            result.is_ok()
        } else {
            false
        };
        Ok(ConvoyPrepared {
            template,
            vessels,
            terminal_sessions,
            checkouts,
            observed_subjects,
            promise_patch,
            exit_disposition,
            settlement_evidence: settlement.map(|settlement| settlement.evaluation),
            settlement_attention,
            reclaim_eligible,
            capacity_wait,
        })
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        prepared: &Self::Prepared,
        now: DateTime<Utc>,
    ) -> ControllerReconcileOutcome<Self::Resource> {
        if !obj.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            let prior = obj.status.as_ref().and_then(|status| status.stalled.as_ref());
            if let Some(evidence) = &prepared.capacity_wait {
                let retry = ControllerRetry::retryable(
                    None,
                    now,
                    RetryBackoff { initial: std::time::Duration::from_secs(30), maximum: std::time::Duration::from_secs(30) },
                );
                let condition = StalledCondition {
                    leaves: Vec::new(),
                    maker: Some(LeafMaker::Controller {
                        resource_kind: "Convoy".to_string(),
                        name: None,
                        retry,
                        ceiling: RetryCeiling::default(),
                    }),
                    evidence: evidence.clone(),
                    source: StallEvidenceSource::LeafEngine,
                    cause: Some(StallCause::Capacity),
                    began_at: prior.filter(|stalled| stalled.cause == Some(StallCause::Capacity)).map_or(now, |stalled| stalled.began_at),
                    rung: StallRung::Operator,
                    supervisor: None,
                    supervision_message: None,
                    supervision_index: None,
                    supervision_exhausted: false,
                    reason: None,
                    proposed_disposition: None,
                    nudge_history: Vec::new(),
                };
                return ControllerReconcileOutcome {
                    patch: prior
                        .filter(|stalled| stalled.cause == Some(StallCause::Capacity) && stalled.evidence == *evidence)
                        .is_none()
                        .then_some(ConvoyStatusPatch::SetStalled { condition: Some(condition) }),
                    actuations: Vec::new(),
                    events: Vec::new(),
                    requeue_after: Some(std::time::Duration::from_secs(30)),
                };
            }
            if prior.is_some_and(|stalled| stalled.cause == Some(StallCause::Capacity)) {
                return ControllerReconcileOutcome {
                    patch: Some(ConvoyStatusPatch::SetStalled { condition: None }),
                    actuations: Vec::new(),
                    events: Vec::new(),
                    requeue_after: None,
                };
            }
        }
        let missing = prepared
            .observed_subjects
            .iter()
            .filter(|subject| {
                !obj.status.as_ref().is_some_and(|status| status.unlinked_subjects.contains(*subject) || status.produces(subject))
            })
            .cloned()
            .map(|subject| (subject, Relationship::Produces))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return ControllerReconcileOutcome {
                patch: Some(ConvoyStatusPatch::DiscoverSubjects { subjects: missing, source: SubjectDiscoverySource::Branch, at: now }),
                actuations: Vec::new(),
                events: Vec::new(),
                requeue_after: None,
            };
        }
        if let Some(patch) = &prepared.promise_patch {
            return ControllerReconcileOutcome {
                patch: Some(patch.clone()),
                actuations: Vec::new(),
                events: Vec::new(),
                requeue_after: None,
            };
        }
        let mut outcome = reconcile_internal(
            obj,
            prepared.template.as_ref(),
            &prepared.vessels,
            &prepared.checkouts,
            LifecycleConditions {
                exit_disposition: prepared.exit_disposition.clone(),
                reclaim_eligible: prepared.reclaim_eligible,
                settlement_evidence: prepared.settlement_evidence.clone(),
            },
            now,
        );
        // Landed convoy records are retained, so their deletion finalizer may
        // never run. Sweep sessions here too: a missing vessel must not strand
        // an orphan session after the independently verified reclaim gate.
        if prepared.reclaim_eligible && obj.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            outcome.actuations.extend(
                prepared
                    .terminal_sessions
                    .iter()
                    .filter(|session| session.metadata.deletion_timestamp.is_none())
                    .filter(|session| matches!(session.metadata.lifecycle_authority(), Ok(None | Some(LifecycleAuthority::Managed))))
                    .map(|session| Actuation::DeleteTerminalSession { name: session.metadata.name.clone() }),
            );
        }
        if outcome.patch.is_none() {
            if let Some(attention) = &prepared.settlement_attention {
                let changed = obj
                    .status
                    .as_ref()
                    .and_then(|status| status.attention.as_ref())
                    .is_none_or(|existing| existing.source != attention.source || existing.reason != attention.reason);
                if changed {
                    outcome.patch = Some(ConvoyStatusPatch::SetSettlementAttention { attention: Some(attention.clone()) });
                }
            } else if obj
                .status
                .as_ref()
                .and_then(|status| status.attention.as_ref())
                .is_some_and(|attention| attention.source == "observed-digest")
            {
                outcome.patch = Some(ConvoyStatusPatch::SetSettlementAttention { attention: None });
            }
        }
        // A refused reclaim must retry on its own schedule: the refusal is
        // often transient (integration evidence mid-refresh, a checkout
        // cascade in flight) and no watch event is guaranteed to arrive once
        // the convoy is terminal. Retry at the evidence-staleness horizon —
        // the freshness the gate is waiting on.
        let reclaim_refused = self.teardown_runtime.is_some()
            && obj.status.as_ref().is_some_and(|status| status.phase.is_terminal())
            && !prepared.reclaim_eligible;
        let provisioning_requeue = obj
            .status
            .as_ref()
            .into_iter()
            .flat_map(|status| status.work.values())
            .filter_map(|state| state.provisioning_retry.as_ref())
            .filter_map(ControllerRetry::next_attempt_at)
            .filter(|deadline| *deadline > now)
            .filter_map(|deadline| (deadline - now).to_std().ok())
            .min();
        ControllerReconcileOutcome {
            patch: outcome.patch,
            actuations: outcome.actuations,
            events: outcome.events.into_iter().map(|event| convoy_object_event(obj, event)).collect(),
            requeue_after: provisioning_requeue.or_else(|| reclaim_refused.then_some(self.landing_evidence_stale_after)).or_else(|| {
                obj.status.as_ref().filter(|status| !status.phase.is_terminal()).and_then(|status| {
                    status
                        .promises
                        .values()
                        .flat_map(BTreeMap::values)
                        .flatten()
                        .any(|p| {
                            p.state == flotilla_resources::convoy::promises::PromiseState::Submitted
                                || (p.kind == flotilla_resources::convoy::promises::PromiseKind::DecisionLedger
                                    && p.state == flotilla_resources::convoy::promises::PromiseState::Open)
                        })
                        .then_some(PROMISE_OBSERVATION_RETRY)
                })
            }),
        }
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        let selector = BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())]);

        if let Some(vessels) = &self.vessels {
            delete_lifecycle_owned_matching(vessels, &selector).await?;
        }
        if let Some(terminal_sessions) = &self.terminal_sessions {
            delete_lifecycle_owned_matching(terminal_sessions, &selector).await?;
        }
        if let Some(checkouts) = &self.checkouts {
            delete_lifecycle_owned_matching(checkouts, &selector).await?;
        }
        if let Some(checkouts) = &self.federated_checkouts {
            let remaining = federated_children(checkouts, obj)
                .await?
                .into_values()
                .filter(|checkout| checkout.metadata.lifecycle_authority() == Ok(Some(LifecycleAuthority::Managed)))
                .map(|checkout| crate::FinalizerWaitReason::CheckoutAuthority {
                    checkout: checkout.metadata.name,
                    message: checkout
                        .status
                        .as_ref()
                        .and_then(|status| status.message.clone())
                        .filter(|message| checkout.metadata.deletion_timestamp.is_some() || message.contains(" preserved: ")),
                })
                .collect::<Vec<_>>();
            if !remaining.is_empty() {
                return Err(ResourceError::FinalizerWait { reasons: remaining });
            }
        }
        if let Some(collector) = &self.prepared_snapshot_gc {
            collector.collect(Some(&obj.metadata.name)).await?;
        }
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some(flotilla_resources::convoy::CONVOY_TEARDOWN_FINALIZER)
    }

    fn finalizer_error_patch(&self, obj: &ResourceObject<Self::Resource>, error: &ResourceError) -> Option<ConvoyStatusPatch> {
        let ResourceError::FinalizerWait { .. } = error else { return None };
        let message = error.to_string();
        if obj.status.as_ref().and_then(|status| status.message.as_deref()) == Some(message.as_str()) {
            return None;
        }
        Some(ConvoyStatusPatch::SetTeardownWait { message })
    }
}
