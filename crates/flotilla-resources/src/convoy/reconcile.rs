use std::{
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::json;

use super::{
    controller_patches, expected_change_request_leaves, expected_checkout_refs, provisioning_patches, Convoy, ConvoyPhase,
    ConvoyStatusPatch, CrewWorkPhase, CrewWorkState, VesselRequirement, WorkCompletionAuthority, WorkPhase, WorkState, WorkflowSnapshot,
};
use crate::{
    checkout::Checkout,
    controller::{
        delete_lifecycle_owned_matching, Actuation, LabelMappedWatch, ReconcileOutcome as ControllerReconcileOutcome, Reconciler,
        ReplicaLabelMappedWatch, SecondaryWatch,
    },
    labels::{LifecycleAuthority, CONVOY_LABEL, VESSEL_LABEL},
    pinned_placement_ref, pinned_workflow_ref, prepared_snapshot_pending,
    presentation::{Presentation, PresentationSpec},
    resource::ResourceObject,
    status_patch::StatusPatch,
    terminal_session::TerminalSession,
    vessel::{Vessel, VesselPhase, ACTUATOR_HOST_REF_ANNOTATION},
    workflow_template::{validate, visit_template_tokens, CrewSource, CrewSpec, ValidationError, WorkflowTemplate},
    ChangeRequest, ChangeRequestLeafSubject, InputMeta, InputValue, OwnerReference, PlacementStatus, PreparedSnapshotGarbageCollector,
    ReplicaReadResolver, Resource, ResourceError, ResourceProvenance, ThreeValue, TypedResolver,
};

#[async_trait]
pub trait ConvoyTeardownRuntime: Send + Sync {
    /// Re-verify ADR 0017 teardown eligibility at the execution edge.
    async fn verify_reclaim(&self, convoy: &ResourceObject<Convoy>, checkouts: &[ResourceObject<Checkout>]) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub patch: Option<ConvoyStatusPatch>,
    pub events: Vec<ConvoyEvent>,
}

#[derive(Debug, Clone)]
struct InternalReconcileOutcome {
    patch: Option<ConvoyStatusPatch>,
    actuations: Vec<Actuation>,
    events: Vec<ConvoyEvent>,
}

#[derive(Debug, Clone, Copy)]
struct LifecycleConditions {
    no_change_request_outstanding: bool,
    reclaim_eligible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyEvent {
    PhaseChanged { from: ConvoyPhase, to: ConvoyPhase },
    WorkPhaseChanged { work: String, from: WorkPhase, to: WorkPhase },
    TemplateNotFound { name: String },
    TemplateInvalid { name: String, errors: Vec<ValidationError> },
    WorkflowRefChanged { from: String, to: String },
    MissingInput { name: String },
}

#[derive(Clone)]
pub struct ConvoyReconciler {
    templates: TypedResolver<WorkflowTemplate>,
    vessels: Option<TypedResolver<Vessel>>,
    federated_vessels: Option<ReplicaReadResolver<Vessel>>,
    terminal_sessions: Option<TypedResolver<TerminalSession>>,
    presentations: Option<TypedResolver<Presentation>>,
    checkouts: Option<TypedResolver<Checkout>>,
    federated_checkouts: Option<ReplicaReadResolver<Checkout>>,
    change_requests: Option<ReplicaReadResolver<ChangeRequest>>,
    change_request_stale_after: std::time::Duration,
    prepared_snapshot_gc: Option<PreparedSnapshotGarbageCollector>,
    teardown_runtime: Option<Arc<dyn ConvoyTeardownRuntime>>,
}

#[derive(Debug, Clone)]
pub struct ConvoyDependencies {
    template: Option<ResourceObject<WorkflowTemplate>>,
    vessels: BTreeMap<String, ResourceObject<Vessel>>,
    presentations: BTreeMap<String, ResourceObject<Presentation>>,
    checkouts: BTreeMap<String, ResourceObject<Checkout>>,
    no_change_request_outstanding: bool,
    reclaim_eligible: bool,
}

impl ConvoyReconciler {
    pub fn new(templates: TypedResolver<WorkflowTemplate>) -> Self {
        Self {
            templates,
            vessels: None,
            federated_vessels: None,
            terminal_sessions: None,
            presentations: None,
            checkouts: None,
            federated_checkouts: None,
            change_requests: None,
            change_request_stale_after: std::time::Duration::from_secs(180),
            prepared_snapshot_gc: None,
            teardown_runtime: None,
        }
    }

    pub fn with_vessels(mut self, vessels: TypedResolver<Vessel>) -> Self {
        self.vessels = Some(vessels);
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

    pub fn with_presentations(mut self, presentations: TypedResolver<Presentation>) -> Self {
        self.presentations = Some(presentations);
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
            Box::new(LabelMappedWatch::<Presentation, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
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
            Box::new(LabelMappedWatch::<Presentation, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(ReplicaLabelMappedWatch::<Checkout, Convoy> {
                label_key: CONVOY_LABEL,
                resolver: backend.including_replicas::<Checkout>(namespace),
                _marker: PhantomData,
            }),
        ]
    }
}

async fn federated_children<T: Resource>(
    resolver: &ReplicaReadResolver<T>,
    convoy: &ResourceObject<Convoy>,
) -> Result<BTreeMap<String, ResourceObject<T>>, ResourceError> {
    let target_host_ref = convoy
        .status
        .as_ref()
        .and_then(|status| status.placement_decision.as_ref())
        .map(|decision| decision.target_host.reference.as_str());
    let mut selected = BTreeMap::<String, (u8, ResourceObject<T>)>::new();
    for source in resolver.list().await?.items {
        if source.object.metadata.labels.get(CONVOY_LABEL) != Some(&convoy.metadata.name) {
            continue;
        }
        let actuator_matches = target_host_ref
            .is_some_and(|target| source.object.metadata.annotations.get(ACTUATOR_HOST_REF_ANNOTATION).is_some_and(|host| host == target));
        let priority = if actuator_matches {
            2
        } else if matches!(source.provenance, ResourceProvenance::Local) {
            1
        } else {
            0
        };
        let name = source.object.metadata.name.clone();
        if selected.get(&name).is_none_or(|(current_priority, _)| priority > *current_priority) {
            selected.insert(name, (priority, source.object));
        }
    }
    Ok(selected.into_iter().map(|(name, (_, object))| (name, object)).collect())
}

fn landing_condition_satisfied(
    convoy: &ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    stale_after: std::time::Duration,
) -> bool {
    let Ok(leaves) = expected_change_request_leaves(convoy, checkouts) else {
        return false;
    };
    let all_change_requests_terminal = leaves.chunks_exact(2).all(|terminal_leaves| {
        terminal_leaves.iter().any(|leaf| {
            let flotilla_protocol::LeafAddress::ChangeRequest { service, scope, number } = &leaf.address else {
                return false;
            };
            let name = crate::change_request_record_name(service, scope, *number);
            let subject =
                change_requests.get(&name).map(|change_request| ChangeRequestLeafSubject { change_request, now: Utc::now(), stale_after });
            crate::evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn crate::LeafSubject), None)
                .is_ok_and(|evaluation| evaluation.result == ThreeValue::True)
        })
    });
    if !all_change_requests_terminal {
        return false;
    }

    let bound_repository = convoy.spec.change_request.as_ref().map(|bound| &bound.repository_ref);
    expected_checkout_refs(convoy).is_ok_and(|expected| {
        expected.iter().all(|name| {
            checkouts.get(name).is_some_and(|checkout| {
                let has_expected_change_request =
                    checkout.status.as_ref().and_then(|status| status.integration.change_request.as_ref()).is_some()
                        || bound_repository == Some(checkout.spec.repo_ref());
                has_expected_change_request
                    || checkout.status.as_ref().is_some_and(|status| status.integration.landed.value == crate::ConditionValue::True)
            })
        })
    })
}

impl Reconciler for ConvoyReconciler {
    type Resource = Convoy;
    type Dependencies = ConvoyDependencies;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
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
        let presentations = match &self.presentations {
            Some(presentations) if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() => presentations
                .list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())]))
                .await?
                .items
                .into_iter()
                .map(|presentation| (presentation.metadata.name.clone(), presentation))
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
        let change_requests = match &self.change_requests {
            Some(change_requests) => {
                change_requests.list().await?.items.into_iter().map(|item| (item.object.metadata.name.clone(), item.object)).collect()
            }
            None => BTreeMap::new(),
        };
        let no_change_request_outstanding = if obj.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landing) {
            landing_condition_satisfied(obj, &checkouts, &change_requests, self.change_request_stale_after)
        } else {
            false
        };
        let reclaim_eligible = if obj.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            match &self.teardown_runtime {
                Some(runtime) => {
                    let checkout_list = checkouts.values().cloned().collect::<Vec<_>>();
                    runtime.verify_reclaim(obj, &checkout_list).await.is_ok()
                }
                None => false,
            }
        } else {
            false
        };
        Ok(ConvoyDependencies { template, vessels, presentations, checkouts, no_change_request_outstanding, reclaim_eligible })
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        deps: &Self::Dependencies,
        now: DateTime<Utc>,
    ) -> ControllerReconcileOutcome<Self::Resource> {
        let outcome = reconcile_internal(
            obj,
            deps.template.as_ref(),
            &deps.vessels,
            &deps.presentations,
            &deps.checkouts,
            LifecycleConditions {
                no_change_request_outstanding: deps.no_change_request_outstanding,
                reclaim_eligible: deps.reclaim_eligible,
            },
            now,
        );
        ControllerReconcileOutcome {
            patch: outcome.patch,
            actuations: outcome.actuations,
            events: outcome.events.into_iter().map(|event| format!("{event:?}")).collect(),
            requeue_after: None,
        }
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        let selector = BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())]);
        if let Some(presentations) = &self.presentations {
            delete_lifecycle_owned_matching(presentations, &selector).await?;
        }
        if let Some(vessels) = &self.vessels {
            delete_lifecycle_owned_matching(vessels, &selector).await?;
        }
        if let Some(terminal_sessions) = &self.terminal_sessions {
            delete_lifecycle_owned_matching(terminal_sessions, &selector).await?;
        }
        if let Some(checkouts) = &self.checkouts {
            delete_lifecycle_owned_matching(checkouts, &selector).await?;
        }
        if let Some(collector) = &self.prepared_snapshot_gc {
            collector.collect(Some(&obj.metadata.name)).await?;
        }
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/convoy-teardown")
    }
}

/// Test-support reconcile entry that carries no vessel, presentation, or
/// checkout state. Settlement can therefore only be judged from the convoy
/// record itself: a convoy expecting no checkouts (no placement
/// `checkout_refs`, no adoptions) settles on claims alone, and any expected
/// checkout holds settlement here because this path has no integration
/// evidence to offer. Production callers go through `ConvoyReconciler`,
/// which supplies the federated checkout list.
pub fn reconcile(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    now: DateTime<Utc>,
) -> ReconcileOutcome {
    let no_change_request_outstanding = expected_checkout_refs(convoy).is_ok_and(|expected| expected.is_empty());
    let outcome = reconcile_internal(
        convoy,
        template,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        LifecycleConditions { no_change_request_outstanding, reclaim_eligible: false },
        now,
    );
    ReconcileOutcome { patch: outcome.patch, events: outcome.events }
}

fn reconcile_internal(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    presentations: &BTreeMap<String, ResourceObject<Presentation>>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    conditions: LifecycleConditions,
    now: DateTime<Utc>,
) -> InternalReconcileOutcome {
    if prepared_snapshot_pending(convoy) {
        return InternalReconcileOutcome { patch: None, actuations: Vec::new(), events: Vec::new() };
    }
    let status = convoy.status.clone().unwrap_or_default();

    if status.phase.is_terminal() {
        return with_cleanup(convoy, &status, vessels, presentations, checkouts, conditions.reclaim_eligible, InternalReconcileOutcome {
            patch: None,
            actuations: Vec::new(),
            events: Vec::new(),
        });
    }

    if let Some(observed) = status.observed_workflow_ref.as_ref() {
        if observed != &convoy.spec.workflow_ref {
            return with_cleanup(
                convoy,
                &status,
                vessels,
                presentations,
                checkouts,
                conditions.reclaim_eligible,
                InternalReconcileOutcome {
                    patch: Some(controller_patches::fail_init(
                        ConvoyPhase::Failed,
                        "workflow_ref changed after init; not supported".to_string(),
                        now,
                    )),
                    actuations: Vec::new(),
                    events: vec![ConvoyEvent::WorkflowRefChanged { from: observed.clone(), to: convoy.spec.workflow_ref.clone() }],
                },
            );
        }
    }

    if status.observed_workflow_ref.is_none() {
        return bootstrap_outcome(convoy, template, now);
    }

    if let Some(outcome) = backfill_crew_work_outcome(&status) {
        return with_cleanup(convoy, &status, vessels, presentations, checkouts, conditions.reclaim_eligible, outcome);
    }

    if let Some(outcome) = fail_fast_outcome(&status, now) {
        return with_cleanup(convoy, &status, vessels, presentations, checkouts, conditions.reclaim_eligible, outcome);
    }

    let provisioning = vessel_outcome(convoy, &status, vessels, now);
    if provisioning.patch.is_some() {
        return with_cleanup(convoy, &status, vessels, presentations, checkouts, conditions.reclaim_eligible, provisioning);
    }

    if let Some(outcome) = roll_up_crew_work_outcome(&status, now) {
        return with_cleanup(convoy, &status, vessels, presentations, checkouts, conditions.reclaim_eligible, InternalReconcileOutcome {
            patch: outcome.patch,
            actuations: provisioning.actuations,
            events: outcome.events,
        });
    }

    if let Some(outcome) = advance_ready_outcome(&status, now) {
        return with_cleanup(convoy, &status, vessels, presentations, checkouts, conditions.reclaim_eligible, InternalReconcileOutcome {
            patch: outcome.patch,
            actuations: provisioning.actuations,
            events: outcome.events,
        });
    }

    if let Some(outcome) = roll_up_phase_outcome(convoy, &status, checkouts, conditions.no_change_request_outstanding, now) {
        return with_cleanup(convoy, &status, vessels, presentations, checkouts, conditions.reclaim_eligible, InternalReconcileOutcome {
            patch: outcome.patch,
            actuations: provisioning.actuations,
            events: outcome.events,
        });
    }

    with_cleanup(convoy, &status, vessels, presentations, checkouts, conditions.reclaim_eligible, provisioning)
}

fn bootstrap_outcome(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    now: DateTime<Utc>,
) -> InternalReconcileOutcome {
    let Some(template) = template else {
        return InternalReconcileOutcome {
            patch: Some(controller_patches::fail_init(
                ConvoyPhase::Failed,
                format!("WorkflowTemplate '{}' not found", convoy.spec.workflow_ref),
                now,
            )),
            actuations: Vec::new(),
            events: vec![ConvoyEvent::TemplateNotFound { name: convoy.spec.workflow_ref.clone() }],
        };
    };

    if let Err(errors) = validate(&template.spec) {
        return InternalReconcileOutcome {
            patch: Some(controller_patches::fail_init(
                ConvoyPhase::Failed,
                format!("WorkflowTemplate '{}' is invalid: {errors:?}", convoy.spec.workflow_ref),
                now,
            )),
            actuations: Vec::new(),
            events: vec![ConvoyEvent::TemplateInvalid { name: template.metadata.name.clone(), errors }],
        };
    }

    for input in &template.spec.inputs {
        if !convoy.spec.inputs.contains_key(&input.name) {
            return InternalReconcileOutcome {
                patch: Some(controller_patches::fail_init(ConvoyPhase::Failed, format!("missing input '{}'", input.name), now)),
                actuations: Vec::new(),
                events: vec![ConvoyEvent::MissingInput { name: input.name.clone() }],
            };
        }
    }

    let workflow_snapshot = WorkflowSnapshot {
        vessels: template
            .spec
            .vessels
            .iter()
            .map(|vessel| VesselRequirement {
                name: vessel.name.clone(),
                stance: vessel.stance,
                depends_on: vessel.depends_on.clone(),
                repository_refs: vessel.repository_refs.clone(),
                credential_refs: vessel.credential_refs.clone(),
                credential_scopes: vessel.credential_scopes.clone(),
                crew: vessel.crew.iter().map(|member| instantiate_process(convoy, member)).collect(),
            })
            .collect(),
    };
    let work = template
        .spec
        .vessels
        .iter()
        .map(|vessel| {
            (vessel.name.clone(), WorkState {
                phase: WorkPhase::Pending,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: None,
                started_at: None,
                finished_at: None,
                message: None,
                placement: None,
            })
        })
        .collect();
    let crew_work = template
        .spec
        .vessels
        .iter()
        .map(|vessel| {
            let members = vessel
                .crew
                .iter()
                .filter(|member| matches!(member.source, CrewSource::Agent { .. }))
                .map(|member| (member.role.clone(), CrewWorkState::builder().phase(CrewWorkPhase::Pending).build()))
                .collect();
            (vessel.name.clone(), members)
        })
        .collect();

    InternalReconcileOutcome {
        patch: Some(controller_patches::bootstrap(
            workflow_snapshot,
            convoy.spec.workflow_ref.clone(),
            [(convoy.spec.workflow_ref.clone(), template.metadata.resource_version.clone())].into_iter().collect(),
            work,
            crew_work,
            ConvoyPhase::Pending,
            None,
        )),
        actuations: Vec::new(),
        events: Vec::new(),
    }
}

fn backfill_crew_work_outcome(status: &super::ConvoyStatus) -> Option<InternalReconcileOutcome> {
    let snapshot = status.workflow_snapshot.as_ref()?;
    let mut missing = BTreeMap::new();
    let mut completion_overrides = BTreeSet::new();

    for vessel in &snapshot.vessels {
        let existing = status.crew_work.get(&vessel.name);
        let work = status.work.get(&vessel.name);
        let missing_crew = vessel
            .crew
            .iter()
            .enumerate()
            .filter(|(_, member)| matches!(member.source, CrewSource::Agent { .. }))
            .filter(|(_, member)| existing.is_none_or(|crew| !crew.contains_key(&member.role)))
            .map(|(index, member)| {
                let mut state = CrewWorkState::builder().phase(CrewWorkPhase::Pending).build();
                // A latent agent has no session even once the vessel is running.
                if work.is_some_and(|work| work.phase == WorkPhase::Running) && vessel.starts_eagerly(index) {
                    state.phase = CrewWorkPhase::Working;
                    state.started_at = work.and_then(|work| work.started_at);
                }
                (member.role.clone(), state)
            })
            .collect::<BTreeMap<_, _>>();
        if !missing_crew.is_empty() {
            if work.is_some_and(|work| work.phase == WorkPhase::Complete) {
                completion_overrides.insert(vessel.name.clone());
            }
            missing.insert(vessel.name.clone(), missing_crew);
        }
    }

    (!missing.is_empty()).then(|| InternalReconcileOutcome {
        patch: Some(controller_patches::backfill_crew_work(missing, completion_overrides)),
        actuations: Vec::new(),
        events: Vec::new(),
    })
}

fn instantiate_process(convoy: &ResourceObject<Convoy>, process: &CrewSpec) -> CrewSpec {
    let mut process = process.clone();
    match &mut process.source {
        CrewSource::Agent { prompt, .. } => {
            if let Some(prompt) = prompt {
                *prompt = interpolate_template_text(convoy, prompt);
            }
        }
        CrewSource::Tool { command } => {
            *command = interpolate_template_text(convoy, command);
        }
    }
    process
}

fn interpolate_template_text(convoy: &ResourceObject<Convoy>, text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut search_from = 0;
    visit_template_tokens(text, |token| {
        output.push_str(&text[search_from..token.open]);
        match token.end {
            Some(end) => {
                if let Some(value) = interpolation_value(convoy, token.text) {
                    output.push_str(&value);
                } else {
                    output.push_str(&text[token.open..end]);
                }
                search_from = end;
            }
            None => {
                output.push_str(&text[token.open..]);
                search_from = text.len();
            }
        }
    });
    output.push_str(&text[search_from..]);
    output
}

fn interpolation_value(convoy: &ResourceObject<Convoy>, token: &str) -> Option<String> {
    let segments = token.split('.').collect::<Vec<_>>();
    match segments.as_slice() {
        ["inputs", input_name] => convoy.spec.inputs.get(*input_name).map(input_value_string),
        ["workflow", "name"] => Some(convoy.metadata.name.clone()),
        ["workflow", "namespace"] => Some(convoy.metadata.namespace.clone()),
        _ => None,
    }
}

fn input_value_string(value: &InputValue) -> String {
    match value {
        InputValue::String(value) => value.clone(),
    }
}

fn fail_fast_outcome(status: &super::ConvoyStatus, now: DateTime<Utc>) -> Option<InternalReconcileOutcome> {
    let failure_message = status
        .work
        .values()
        .filter(|state| state.phase == WorkPhase::Failed)
        .find_map(|state| state.message.clone())
        .or_else(|| status.work.values().any(|state| state.phase == WorkPhase::Failed).then(|| "work failure detected".to_string()))?;

    let cancelled_work = status
        .work
        .iter()
        .filter_map(|(name, state)| match state.phase {
            WorkPhase::Complete | WorkPhase::Failed | WorkPhase::Cancelled | WorkPhase::Abandoned => None,
            _ => Some((name.clone(), now)),
        })
        .collect::<BTreeMap<_, _>>();

    let mut events = Vec::new();
    if status.phase != ConvoyPhase::Failed {
        events.push(ConvoyEvent::PhaseChanged { from: status.phase, to: ConvoyPhase::Failed });
    }
    for work in cancelled_work.keys() {
        if let Some(state) = status.work.get(work) {
            events.push(ConvoyEvent::WorkPhaseChanged { work: work.clone(), from: state.phase, to: WorkPhase::Cancelled });
        }
    }

    Some(InternalReconcileOutcome {
        patch: Some(controller_patches::fail_convoy(cancelled_work, now, Some(failure_message))),
        actuations: Vec::new(),
        events,
    })
}

fn advance_ready_outcome(status: &super::ConvoyStatus, now: DateTime<Utc>) -> Option<ReconcileOutcome> {
    let snapshot = status.workflow_snapshot.as_ref()?;
    let ready = snapshot
        .vessels
        .iter()
        .filter_map(|vessel| {
            let state = status.work.get(&vessel.name)?;
            if state.phase != WorkPhase::Pending {
                return None;
            }
            let all_complete = vessel
                .depends_on
                .iter()
                .all(|dependency| matches!(status.work.get(dependency), Some(dep_state) if dep_state.phase == WorkPhase::Complete));
            all_complete.then(|| (vessel.name.clone(), now))
        })
        .collect::<BTreeMap<_, _>>();

    if ready.is_empty() {
        return None;
    }

    let events =
        ready.keys().cloned().map(|work| ConvoyEvent::WorkPhaseChanged { work, from: WorkPhase::Pending, to: WorkPhase::Ready }).collect();

    Some(ReconcileOutcome { patch: Some(controller_patches::advance_work_to_ready(ready)), events })
}

fn roll_up_crew_work_outcome(status: &super::ConvoyStatus, now: DateTime<Utc>) -> Option<ReconcileOutcome> {
    for (work, work_state) in &status.work {
        let Some(crew) = status.crew_work.get(work).filter(|crew| !crew.is_empty()) else {
            continue;
        };
        if work_state.completion_authority == WorkCompletionAuthority::HumanOverride {
            continue;
        }
        if let Some((role, failed)) = crew.iter().find(|(_, state)| state.phase == CrewWorkPhase::Failed) {
            if work_state.phase != WorkPhase::Failed {
                let message = failed.message.clone().unwrap_or_else(|| format!("crew member `{role}` failed"));
                return Some(ReconcileOutcome {
                    patch: Some(controller_patches::roll_up_work(work.clone(), WorkPhase::Failed, now, Some(message))),
                    events: vec![ConvoyEvent::WorkPhaseChanged { work: work.clone(), from: work_state.phase, to: WorkPhase::Failed }],
                });
            }
            continue;
        }

        let all_done = crew.values().all(|state| state.phase == CrewWorkPhase::Done);
        let next_phase = match (work_state.phase, all_done) {
            (WorkPhase::Running, true) => Some(WorkPhase::Complete),
            (WorkPhase::Complete, false) => Some(WorkPhase::Running),
            _ => None,
        };
        if let Some(next_phase) = next_phase {
            return Some(ReconcileOutcome {
                patch: Some(controller_patches::roll_up_work(work.clone(), next_phase, now, None)),
                events: vec![ConvoyEvent::WorkPhaseChanged { work: work.clone(), from: work_state.phase, to: next_phase }],
            });
        }
    }
    None
}

fn roll_up_phase_outcome(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    no_change_request_outstanding: bool,
    now: DateTime<Utc>,
) -> Option<ReconcileOutcome> {
    let all_complete = !status.work.is_empty() && status.work.values().all(|state| state.phase == WorkPhase::Complete);
    if status.phase == ConvoyPhase::Landing && all_complete && no_change_request_outstanding {
        let target_mismatches = convoy
            .spec
            .repositories
            .iter()
            .filter_map(|repository| {
                checkouts
                    .values()
                    .find(|checkout| checkout.spec.repo_ref() == &repository.repo_ref)
                    .and_then(|checkout| checkout.status.as_ref())
                    .and_then(|status| status.integration.landed_evidence.as_ref())
                    .and_then(|evidence| {
                        let observed_target_ref = evidence.target_ref.as_ref()?;
                        (observed_target_ref != &repository.target_ref).then(|| super::TargetMismatch {
                            repo_ref: repository.repo_ref.clone(),
                            change_request_id: evidence.change_request_id.clone(),
                            declared_target_ref: repository.target_ref.clone(),
                            observed_target_ref: observed_target_ref.clone(),
                        })
                    })
            })
            .collect();
        return Some(ReconcileOutcome {
            patch: Some(controller_patches::settle(target_mismatches, now)),
            events: vec![ConvoyEvent::PhaseChanged { from: ConvoyPhase::Landing, to: ConvoyPhase::Landed }],
        });
    }

    let any_interrupted = status.work.values().any(|state| state.phase == WorkPhase::Interrupted);
    if any_interrupted && status.phase != ConvoyPhase::Interrupted {
        return Some(ReconcileOutcome {
            patch: Some(controller_patches::roll_up_phase(ConvoyPhase::Interrupted, None, None)),
            events: vec![ConvoyEvent::PhaseChanged { from: status.phase, to: ConvoyPhase::Interrupted }],
        });
    }
    if !any_interrupted && status.phase == ConvoyPhase::Interrupted {
        return Some(ReconcileOutcome {
            patch: Some(controller_patches::roll_up_phase(ConvoyPhase::Active, None, None)),
            events: vec![ConvoyEvent::PhaseChanged { from: ConvoyPhase::Interrupted, to: ConvoyPhase::Active }],
        });
    }

    let any_progressed = status.work.values().any(|state| state.phase != WorkPhase::Pending);
    if any_progressed && status.phase == ConvoyPhase::Pending {
        return Some(ReconcileOutcome {
            patch: Some(controller_patches::roll_up_phase(ConvoyPhase::Active, Some(now), None)),
            events: vec![ConvoyEvent::PhaseChanged { from: ConvoyPhase::Pending, to: ConvoyPhase::Active }],
        });
    }

    None
}

fn vessel_outcome(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    now: DateTime<Utc>,
) -> InternalReconcileOutcome {
    let Some(snapshot) = status.workflow_snapshot.as_ref() else {
        return InternalReconcileOutcome { patch: None, actuations: Vec::new(), events: Vec::new() };
    };

    let mut actuations = Vec::new();
    for requirement in &snapshot.vessels {
        let Some(state) = status.work.get(&requirement.name) else {
            continue;
        };
        let vessel = vessels.get(&vessel_resource_name(&convoy.metadata.name, &requirement.name));
        match state.phase {
            WorkPhase::Ready => {
                if let Some(vessel) = vessel {
                    if vessel.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Failed) {
                        return work_failed_outcome(requirement.name.clone(), state.phase, vessel_failure_message(vessel), now, actuations);
                    }
                    if vessel.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Ready) {
                        return InternalReconcileOutcome {
                            patch: Some(provisioning_patches::work_launching(requirement.name.clone(), now, placement_status(vessel))),
                            actuations,
                            events: vec![ConvoyEvent::WorkPhaseChanged {
                                work: requirement.name.clone(),
                                from: WorkPhase::Ready,
                                to: WorkPhase::Launching,
                            }],
                        };
                    }
                } else if let Some(outcome) = create_vessel_outcome(convoy, &requirement.name, now) {
                    if outcome.patch.is_some() {
                        return outcome;
                    }
                    actuations.extend(outcome.actuations);
                }
            }
            WorkPhase::Launching => {
                if let Some(vessel) = vessel {
                    if vessel.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Failed) {
                        return work_failed_outcome(requirement.name.clone(), state.phase, vessel_failure_message(vessel), now, actuations);
                    }
                    if vessel.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Ready) {
                        return InternalReconcileOutcome {
                            patch: Some(provisioning_patches::work_running(
                                requirement.name.clone(),
                                now,
                                requirement.eagerly_started_roles(),
                            )),
                            actuations,
                            events: vec![ConvoyEvent::WorkPhaseChanged {
                                work: requirement.name.clone(),
                                from: WorkPhase::Launching,
                                to: WorkPhase::Running,
                            }],
                        };
                    }
                } else if let Some(outcome) = create_vessel_outcome(convoy, &requirement.name, now) {
                    if outcome.patch.is_some() {
                        return outcome;
                    }
                    actuations.extend(outcome.actuations);
                }
            }
            WorkPhase::Running => {
                if let Some(vessel) = vessel {
                    match vessel.status.as_ref().map(|status| status.phase) {
                        Some(VesselPhase::Failed) => {
                            return work_failed_outcome(
                                requirement.name.clone(),
                                state.phase,
                                vessel_failure_message(vessel),
                                now,
                                actuations,
                            );
                        }
                        Some(VesselPhase::Interrupted) => {
                            let vessel_status = vessel.status.as_ref().expect("interrupted vessel has status");
                            let message =
                                vessel_status.message.clone().unwrap_or_else(|| format!("vessel {} was interrupted", vessel.metadata.name));
                            return InternalReconcileOutcome {
                                patch: Some(provisioning_patches::work_interrupted(
                                    requirement.name.clone(),
                                    vessel_status.interrupted_roles.clone(),
                                    message,
                                )),
                                actuations,
                                events: vec![ConvoyEvent::WorkPhaseChanged {
                                    work: requirement.name.clone(),
                                    from: WorkPhase::Running,
                                    to: WorkPhase::Interrupted,
                                }],
                            };
                        }
                        _ => {}
                    }
                }
            }
            WorkPhase::Interrupted => {
                if let Some(vessel) = vessel {
                    match vessel.status.as_ref().map(|status| status.phase) {
                        Some(VesselPhase::Failed) => {
                            return work_failed_outcome(
                                requirement.name.clone(),
                                state.phase,
                                vessel_failure_message(vessel),
                                now,
                                actuations,
                            );
                        }
                        Some(VesselPhase::Ready) => {
                            return InternalReconcileOutcome {
                                patch: Some(provisioning_patches::work_running(
                                    requirement.name.clone(),
                                    now,
                                    requirement.eagerly_started_roles(),
                                )),
                                actuations,
                                events: vec![ConvoyEvent::WorkPhaseChanged {
                                    work: requirement.name.clone(),
                                    from: WorkPhase::Interrupted,
                                    to: WorkPhase::Running,
                                }],
                            };
                        }
                        _ => {}
                    }
                }
            }
            WorkPhase::Pending | WorkPhase::Complete | WorkPhase::Failed | WorkPhase::Cancelled | WorkPhase::Abandoned => {}
        }
    }

    InternalReconcileOutcome { patch: None, actuations, events: Vec::new() }
}

fn with_cleanup(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    presentations: &BTreeMap<String, ResourceObject<Presentation>>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    reclaim_eligible: bool,
    mut outcome: InternalReconcileOutcome,
) -> InternalReconcileOutcome {
    let (patch, actuations) = cleanup_plan(convoy, status, vessels, presentations, checkouts, reclaim_eligible, outcome.patch.as_ref());
    if outcome.patch.is_none() {
        outcome.patch = patch;
    }
    outcome.actuations.extend(actuations);
    outcome
}

fn cleanup_plan(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    presentations: &BTreeMap<String, ResourceObject<Presentation>>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    reclaim_eligible: bool,
    patch: Option<&ConvoyStatusPatch>,
) -> (Option<ConvoyStatusPatch>, Vec<Actuation>) {
    let mut predicted_status = status.clone();
    if let Some(patch) = patch {
        patch.apply(&mut predicted_status);
    }

    if !predicted_status.phase.is_terminal() {
        let mut actuations = Vec::new();
        for (work, state) in &predicted_status.work {
            let resource_name = vessel_resource_name(&convoy.metadata.name, work);
            if matches!(state.phase, WorkPhase::Ready | WorkPhase::Launching | WorkPhase::Running)
                && !presentations.contains_key(&resource_name)
            {
                actuations.push(create_presentation_actuation(convoy, work));
            }
        }
        return (None, actuations);
    }

    if !reclaim_eligible {
        return (None, Vec::new());
    }

    let mut actuations = extract_actuations(convoy);
    actuations.extend(
        presentations
            .keys()
            .cloned()
            .map(|name| Actuation::DeletePresentation { name })
            .chain(
                vessels
                    .values()
                    .filter(|vessel| vessel.metadata.deletion_timestamp.is_none())
                    .map(|vessel| Actuation::DeleteVessel { name: vessel.metadata.name.clone() }),
            )
            .chain(
                checkouts
                    .values()
                    .filter(|checkout| checkout.metadata.deletion_timestamp.is_none())
                    .filter(|checkout| {
                        !matches!(
                            checkout.metadata.lifecycle_authority(),
                            Ok(Some(LifecycleAuthority::Observed | LifecycleAuthority::Adopted))
                        )
                    })
                    .map(|checkout| Actuation::DeleteCheckout { name: checkout.metadata.name.clone() }),
            ),
    );
    actuations.sort_by_key(|actuation| match actuation {
        Actuation::DeletePresentation { name } => (0, name.clone()),
        Actuation::DeleteVessel { name } => (1, name.clone()),
        Actuation::DeleteCheckout { name } => (2, name.clone()),
        _ => (3, String::new()),
    });
    (None, actuations)
}

/// Reserved extraction stage. Reclamation is deliberately sequenced after
/// this call so recordings and logs can be exported here without reshaping
/// terminal cleanup.
fn extract_actuations(_convoy: &ResourceObject<Convoy>) -> Vec<Actuation> {
    Vec::new()
}

fn create_vessel_outcome(convoy: &ResourceObject<Convoy>, vessel: &str, _now: DateTime<Utc>) -> Option<InternalReconcileOutcome> {
    let placement_policy_ref = pinned_placement_ref(convoy)?.to_string();
    let requirement = convoy.status.as_ref()?.workflow_snapshot.as_ref()?.vessels.iter().find(|requirement| requirement.name == vessel)?;
    let repository_refs = requirement
        .repository_refs
        .clone()
        .unwrap_or_else(|| convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect());
    let adopted_checkout_refs = convoy
        .spec
        .adopted_checkout_refs
        .iter()
        .filter(|(repo_ref, _)| repository_refs.contains(repo_ref))
        .map(|(repo_ref, checkout_ref)| (repo_ref.clone(), checkout_ref.clone()))
        .collect();

    Some(InternalReconcileOutcome {
        patch: None,
        actuations: vec![Actuation::CreateVessel {
            meta: crate::InputMeta::builder()
                .name(vessel_resource_name(&convoy.metadata.name, vessel))
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), convoy.metadata.name.clone()),
                    (VESSEL_LABEL.to_string(), vessel.to_string()),
                ]))
                .owner_references(vec![OwnerReference {
                    api_version: format!("{}/{}", Convoy::API_PATHS.group, Convoy::API_PATHS.version),
                    kind: Convoy::API_PATHS.kind.to_string(),
                    name: convoy.metadata.name.clone(),
                    controller: true,
                }])
                .build(),
            spec: crate::VesselSpec {
                convoy_ref: convoy.metadata.name.clone(),
                vessel_name: vessel.to_string(),
                placement_policy_ref,
                adopted_checkout_refs,
            },
        }],
        events: Vec::new(),
    })
}

fn create_presentation_actuation(convoy: &ResourceObject<Convoy>, vessel: &str) -> Actuation {
    let presentation_name = if convoy
        .status
        .as_ref()
        .and_then(|status| status.workflow_snapshot.as_ref())
        .is_some_and(|snapshot| snapshot.vessels.len() == 1)
    {
        convoy.metadata.name.clone()
    } else {
        format!("{}:{vessel}", convoy.metadata.name)
    };

    Actuation::CreatePresentation {
        meta: InputMeta::builder()
            .name(vessel_resource_name(&convoy.metadata.name, vessel))
            .labels(BTreeMap::from([
                (CONVOY_LABEL.to_string(), convoy.metadata.name.clone()),
                (VESSEL_LABEL.to_string(), vessel.to_string()),
            ]))
            .owner_references(vec![OwnerReference {
                api_version: format!("{}/{}", Convoy::API_PATHS.group, Convoy::API_PATHS.version),
                kind: Convoy::API_PATHS.kind.to_string(),
                name: convoy.metadata.name.clone(),
                controller: true,
            }])
            .build(),
        spec: PresentationSpec {
            convoy_ref: convoy.metadata.name.clone(),
            // Stage 4a always uses the built-in default policy. Threading a policy ref through
            // ConvoySpec remains follow-up work once convoys can choose among multiple layouts.
            presentation_policy_ref: "default".to_string(),
            name: presentation_name,
            process_selector: BTreeMap::from([
                (CONVOY_LABEL.to_string(), convoy.metadata.name.clone()),
                (VESSEL_LABEL.to_string(), vessel.to_string()),
            ]),
        },
    }
}

fn work_failed_outcome(
    work: String,
    from: WorkPhase,
    message: String,
    now: DateTime<Utc>,
    actuations: Vec<Actuation>,
) -> InternalReconcileOutcome {
    InternalReconcileOutcome {
        patch: Some(ConvoyStatusPatch::MarkWorkFailed { work: work.clone(), finished_at: now, message }),
        actuations,
        events: vec![ConvoyEvent::WorkPhaseChanged { work, from, to: WorkPhase::Failed }],
    }
}

fn vessel_failure_message(vessel: &ResourceObject<Vessel>) -> String {
    vessel.status.as_ref().and_then(|status| status.message.clone()).unwrap_or_else(|| format!("vessel {} failed", vessel.metadata.name))
}

fn placement_status(workspace: &ResourceObject<Vessel>) -> PlacementStatus {
    let mut fields = BTreeMap::from([("vessel_ref".to_string(), json!(workspace.metadata.name))]);
    if let Some(status) = workspace.status.as_ref() {
        if let Some(decision) = &status.placement_decision {
            fields.insert("placement_decision".to_string(), json!(decision));
        }
        insert_optional_field(&mut fields, "environment_ref", status.environment_ref.clone());
        insert_optional_field(&mut fields, "image_ref", status.image_ref.clone());
        insert_optional_field(&mut fields, "image_digest", status.image_digest.clone());
        if !status.checkout_refs.is_empty() {
            fields.insert("checkout_refs".to_string(), json!(status.checkout_refs));
        }
        if !status.terminal_session_refs.is_empty() {
            fields.insert("terminal_session_refs".to_string(), json!(status.terminal_session_refs));
        }
        insert_optional_field(
            &mut fields,
            "placement_policy_ref",
            status.observed_policy_ref.clone().or_else(|| Some(workspace.spec.placement_policy_ref.clone())),
        );
        if let Some(requested_stance) = status.requested_stance {
            fields.insert("requested_stance".to_string(), json!(requested_stance));
        }
        if let Some(effective_stance) = status.effective_stance {
            fields.insert("effective_stance".to_string(), json!(effective_stance));
        }
    }
    PlacementStatus { fields }
}

fn insert_optional_field(fields: &mut BTreeMap<String, serde_json::Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        fields.insert(key.to_string(), json!(value));
    }
}

/// Per-vessel convoy resources (`Vessel`, `Presentation`) share the name
/// shape `<convoy>-<vessel>`. Resource kinds have separate namespaces, so the
/// shared shape causes no collision and keeps both resources discoverable
/// together by name.
fn vessel_resource_name(convoy_name: &str, vessel: &str) -> String {
    format!("{convoy_name}-{vessel}")
}
