//! Crew lifecycle, convoy supervision, and crew delivery commands.
//!
//! Handlers use the capability-owned port below; the composition root supplies
//! orchestration collaborators and shares their existing state.
use flotilla_resources::ResumeIntent;
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::Command;
use flotilla_protocol::CommandAction;
use flotilla_protocol::CommandCaller;
use flotilla_protocol::CommandValue;
use flotilla_protocol::PrincipalRef;
use flotilla_protocol::Relationship;
use flotilla_resources::external_patches as convoy_external_patches;
use flotilla_resources::Clock;
use flotilla_resources::Convoy as ResourceConvoy;
use flotilla_resources::ResourceError;
use flotilla_resources::ResourceObject;
use flotilla_resources::WorkPhase;
use flotilla_store::apply_status_patch_checked as apply_resource_status_patch_checked;
use flotilla_store::ResourceBackend;

use crate::crew_ops;
use crate::crew_ops::resolve_local_convoy_name;
use crate::crew_ops::ConvoyResumeOutcome;
use crate::crew_ops::CrewSupervisionRequest;
use flotilla_daemon_api::action_events::ActionEvents;

/// Subject reference resolution requires host repository context. Lifecycle,
/// routing, message admission and audit mutations use the shared CrewService
/// directly rather than calling back through the composition root.
#[async_trait]
pub trait CrewActionPort: Send + Sync {
    async fn link_convoy_subject(
        &self,
        namespace: &str,
        convoy_name: &str,
        reference: &str,
        relationship: Option<Relationship>,
    ) -> Result<(), String>;
}

pub struct CrewActions<'a> {
    pub port: &'a dyn CrewActionPort,
    pub crew: &'a crew_ops::CrewService,
    pub resource_backend: &'a ResourceBackend,
    pub clock: &'a Arc<dyn Clock>,
    pub events: ActionEvents<'a>,
}

impl CrewActions<'_> {
    pub async fn execute_action_crew_promise(&self, id: u64, command: &Command) -> Result<u64, String> {
        let CommandAction::CrewPromise { context, operation } = &command.action else {
            return Err("wrong promise handler".into());
        };
        let identity = self.events.start(id, command.description().to_string());
        let result = match self.crew.promise(context, operation.clone()).await {
            Ok(()) => CommandValue::Ok,
            Err(message) => CommandValue::Error { message },
        };
        self.events.finish(id, identity, result);
        Ok(id)
    }

    pub async fn execute_action_artifact_reserve_ledger_comment(&self, id: u64, command: &Command) -> Result<u64, String> {
        let CommandAction::ArtifactReserveLedgerComment { namespace, name, address } = &command.action else {
            return Err("ledger reservation selected the wrong handler".into());
        };
        let identity = self.events.start(id, command.description().to_string());
        let result = match flotilla_store::reserve_ledger_comment_creation(self.resource_backend, namespace, name, address).await {
            Ok(granted) => CommandValue::LedgerCommentCreationReserved { granted },
            Err(error) => CommandValue::Error { message: error.to_string() },
        };
        self.events.finish(id, identity, result);
        Ok(id)
    }

    pub async fn execute_action_crew_handoff(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::CrewHandoff { context, target, message, carries } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let result = match Box::pin(self.crew.handoff_with_carries(context, target, message, carries.clone())).await {
                Ok(()) => CommandValue::Ok,
                Err(message) => CommandValue::Error { message },
            };
            self.events.finish(id, empty_identity, result);
            return Ok(id);
        }
        Err("CrewHandoff action selected the wrong handler".to_string())
    }

    pub async fn execute_action_convoy_resume(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<CommandCaller>,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        let dispatching_principal_ref = dispatching_principal_ref.clone();
        if let CommandAction::ConvoyResume { namespace, name, prompt, vessel, role, reopen } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let intent = if *reopen { ResumeIntent::Reopen } else { ResumeIntent::Reminder };
            let mutation_action = if *reopen { "reopened" } else { "convoy_resume" };
            let namespace = namespace.clone().unwrap_or(self.crew.provisioning_namespace().await);
            let result = match resolve_local_convoy_name(self.resource_backend, &namespace, name).await {
                Ok(record_name) => {
                    match Box::pin(self.crew.convoy_resume_with_sender_internal(
                        &namespace,
                        &record_name,
                        prompt,
                        (vessel.as_deref(), role.as_deref()),
                        crew_ops::MessageAttribution::operator(dispatching_principal_ref.as_ref()),
                        intent,
                    ))
                    .await
                    {
                        Ok(ConvoyResumeOutcome::Informational) => {
                            CommandValue::ConvoyBriefInformational { message: "crew already claimed; delivered without reopening".into() }
                        }
                        Ok(ConvoyResumeOutcome::Delivered { displaced }) => {
                            self.crew
                                .record_lifecycle_mutation_best_effort(&namespace, &record_name, mutation_action, caller.as_ref(), false)
                                .await;
                            CommandValue::ConvoyBriefDelivered { displaced }
                        }
                        Ok(ConvoyResumeOutcome::Queued { displaced }) => {
                            self.crew
                                .record_lifecycle_mutation_best_effort(&namespace, &record_name, mutation_action, caller.as_ref(), false)
                                .await;
                            CommandValue::ConvoyBriefQueued { displaced }
                        }
                        Ok(ConvoyResumeOutcome::QueuedBehindHold { displaced, held }) => {
                            self.crew
                                .record_lifecycle_mutation_best_effort(&namespace, &record_name, mutation_action, caller.as_ref(), false)
                                .await;
                            CommandValue::ConvoyBriefHeld { displaced, held }
                        }
                        Err(message) => CommandValue::Error { message },
                    }
                }
                Err(message) => CommandValue::Error { message },
            };
            self.events.finish(id, empty_identity, result);
            return Ok(id);
        }
        Err("ConvoyResume action selected the wrong handler".to_string())
    }

    pub async fn execute_action_convoy_withdraw_pending_brief(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::ConvoyWithdrawPendingBrief { namespace, name } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let namespace = namespace.clone().unwrap_or(self.crew.provisioning_namespace().await);
            let result = match resolve_local_convoy_name(self.resource_backend, &namespace, name).await {
                Ok(record_name) => match self.crew.withdraw_pending_brief(&namespace, &record_name).await {
                    Ok(withdrawn) => CommandValue::ConvoyBriefWithdrawn { withdrawn },
                    Err(message) => CommandValue::Error { message },
                },
                Err(message) => CommandValue::Error { message },
            };
            self.events.finish(id, empty_identity, result);
            return Ok(id);
        }
        Err("ConvoyWithdrawPendingBrief action selected the wrong handler".to_string())
    }

    pub async fn execute_action_crew_complete(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<CommandCaller>,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        let dispatching_principal_ref = dispatching_principal_ref.clone();
        if let CommandAction::CrewComplete { context, message, disposition, decision_ledger_ref, force } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let routing = Box::pin(self.crew.resolve_crew_routing_context(context)).await.ok();
            let result = match self
                .crew
                .complete(
                    context,
                    message.clone(),
                    disposition.clone(),
                    decision_ledger_ref.clone(),
                    *force,
                    dispatching_principal_ref.clone(),
                )
                .await
            {
                Ok(value) => {
                    if let Some(resolved) = routing {
                        let namespace = resolved.command_context.namespace.as_deref().unwrap_or("flotilla");
                        self.crew
                            .record_lifecycle_mutation_best_effort(namespace, &resolved.convoy, "crew_complete", caller.as_ref(), false)
                            .await;
                    }
                    value
                }
                Err(message) => CommandValue::Error { message },
            };
            self.events.finish(id, empty_identity, result);
            return Ok(id);
        }
        Err("CrewComplete action selected the wrong handler".to_string())
    }

    pub async fn execute_action_crew_fail(&self, id: u64, command: &Command, caller: &Option<CommandCaller>) -> Result<u64, String> {
        let caller = caller.clone();
        if let CommandAction::CrewFail { context, message, force } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let operator =
                caller.as_ref().filter(|caller| caller.crew.is_none() && context.crew_id.is_none()).map(|caller| &caller.principal_ref);
            let result = match self.crew.fail(context, message.clone(), *force, operator).await {
                Ok(()) => CommandValue::Ok,
                Err(message) => CommandValue::Error { message },
            };
            self.events.finish(id, empty_identity, result);
            return Ok(id);
        }
        Err("CrewFail action selected the wrong handler".to_string())
    }

    pub async fn execute_action_crew_stall(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::CrewStall { context, reason, proposed_disposition, message } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let result = match self.crew.stall(context, *reason, *proposed_disposition, message.clone()).await {
                Ok(()) => CommandValue::Ok,
                Err(message) => CommandValue::Error { message },
            };
            self.events.finish(id, empty_identity, result);
            return Ok(id);
        }
        Err("CrewStall action selected the wrong handler".to_string())
    }

    pub async fn execute_action_crew_supervise(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<CommandCaller>,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        let dispatching_principal_ref = dispatching_principal_ref.clone();
        if let CommandAction::CrewSupervise { namespace, convoy, vessel, role, operation, message, actor_crew_id } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let namespace = namespace.clone().unwrap_or(self.crew.provisioning_namespace().await);
            let result = match resolve_local_convoy_name(self.resource_backend, &namespace, convoy).await {
                Ok(name) => match self
                    .crew
                    .supervise(
                        CrewSupervisionRequest::builder()
                            .namespace(&namespace)
                            .convoy_name(&name)
                            .vessel(vessel)
                            .role(role)
                            .operation(*operation)
                            .message(message)
                            .maybe_actor_crew_id(actor_crew_id.as_deref())
                            .maybe_principal(dispatching_principal_ref.as_ref())
                            .build(),
                    )
                    .await
                {
                    Ok(value) => {
                        if !matches!(value, CommandValue::CrewEscalationRefused { .. }) {
                            self.crew
                                .record_lifecycle_mutation_best_effort(
                                    &namespace,
                                    &name,
                                    &format!("crew_supervise_{operation:?}"),
                                    caller.as_ref(),
                                    false,
                                )
                                .await;
                        }
                        value
                    }
                    Err(message) => CommandValue::Error { message },
                },
                Err(message) => CommandValue::Error { message },
            };
            self.events.finish(id, empty_identity, result);
            return Ok(id);
        }
        Err("CrewSupervise action selected the wrong handler".to_string())
    }

    pub async fn execute_action_convoy_link(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::ConvoyLink { namespace, name, reference, relationship } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.crew.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(self.resource_backend, &namespace, name).await {
                Ok(record_name) => self.port.link_convoy_subject(&namespace, &record_name, reference, Some(*relationship)).await,
                Err(error) => Err(error),
            };
            self.events.finish(id, empty_identity, result.map_or_else(|message| CommandValue::Error { message }, |()| CommandValue::Ok));
            return Ok(id);
        }
        Err("ConvoyLink action selected the wrong handler".to_string())
    }

    pub async fn execute_action_convoy_unlink(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::ConvoyUnlink { namespace, name, reference } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.crew.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(self.resource_backend, &namespace, name).await {
                Ok(record_name) => self.port.link_convoy_subject(&namespace, &record_name, reference, None).await,
                Err(error) => Err(error),
            };
            self.events.finish(id, empty_identity, result.map_or_else(|message| CommandValue::Error { message }, |()| CommandValue::Ok));
            return Ok(id);
        }
        Err("ConvoyUnlink action selected the wrong handler".to_string())
    }

    pub async fn execute_action_convoy_delete(&self, id: u64, command: &Command, caller: &Option<CommandCaller>) -> Result<u64, String> {
        let caller = caller.clone();
        if let CommandAction::ConvoyDelete { namespace, name, force } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.crew.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(self.resource_backend, &namespace, name).await {
                Ok(record_name) => match self.crew.reap(&namespace, &record_name, *force).await {
                    Ok(()) => {
                        // Finalizers retain an explainable convoy after delete; a fully
                        // removed convoy has no remaining status to annotate.
                        self.crew
                            .record_lifecycle_mutation_best_effort(&namespace, &record_name, "convoy_delete", caller.as_ref(), true)
                            .await;
                        CommandValue::Ok
                    }
                    Err(message) => CommandValue::Error { message },
                },
                Err(message) => CommandValue::Error { message },
            };
            self.events.finish(id, empty_identity, result);
            return Ok(id);
        }
        Err("ConvoyDelete action selected the wrong handler".to_string())
    }

    pub async fn execute_action_convoy_abandon(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<CommandCaller>,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        let dispatching_principal_ref = dispatching_principal_ref.clone();
        if let CommandAction::ConvoyAbandon { namespace, name, reason } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.crew.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(self.resource_backend, &namespace, name).await {
                Ok(record_name) => match self.crew.abandon(&namespace, &record_name, reason, dispatching_principal_ref.as_ref()).await {
                    Ok(archives) => {
                        self.crew
                            .record_lifecycle_mutation_best_effort(&namespace, &record_name, "convoy_abandon", caller.as_ref(), false)
                            .await;
                        CommandValue::ConvoyAbandoned { name: name.clone(), archives }
                    }
                    Err(message) => CommandValue::Error { message },
                },
                Err(message) => CommandValue::Error { message },
            };
            self.events.finish(id, empty_identity, result);
            return Ok(id);
        }
        Err("ConvoyAbandon action selected the wrong handler".to_string())
    }

    pub async fn execute_action_convoy_work_force_complete(
        &self,
        id: u64,
        command: &Command,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        if let CommandAction::ConvoyWorkForceComplete { convoy, work, message } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let namespace = self.crew.provisioning_namespace().await;
            let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&namespace);
            let record_name = match resolve_local_convoy_name(self.resource_backend, &namespace, convoy).await {
                Ok(record_name) => record_name,
                Err(message) => {
                    self.events.finish(id, empty_identity, CommandValue::Error { message });
                    return Ok(id);
                }
            };
            let check_work_is_completable = |current: &ResourceObject<ResourceConvoy>| match current.status.as_ref() {
                None => Err(ResourceError::other(format!("convoy {convoy} has no status"))),
                Some(status) => match status.work.get(work) {
                    None => Err(ResourceError::other(format!("convoy {convoy} does not contain work {work}"))),
                    Some(state) if matches!(state.phase, WorkPhase::Failed | WorkPhase::Cancelled | WorkPhase::Abandoned) => {
                        Err(ResourceError::other(format!("convoy {convoy} work {work} is already terminal")))
                    }
                    Some(_) => Ok(()),
                },
            };
            // With the engine deciding, forcing work complete is an operator's
            // attributed verdict on its crews' open promises (#3574 decision 8).
            match convoys.get(&record_name).await {
                Ok(current) => {
                    if let Some(engine) = flotilla_resources::convoy::engine::deciding(&self.crew.convoy_engine, &current) {
                        let principal = dispatching_principal_ref.as_ref().map_or("unattributed", |principal| principal.name.as_str());
                        let scope = flotilla_resources::convoy::engine::VerdictScope { vessel: work.clone(), role: None };
                        let reason = message.clone().unwrap_or_else(|| "operator forced work completion".to_string());
                        if let Err(message) = engine.keep_by_verdict(&current, &scope, principal, &reason).await {
                            self.events.finish(id, empty_identity, CommandValue::Error { message });
                            return Ok(id);
                        }
                    }
                }
                Err(error) => {
                    self.events.finish(id, empty_identity, CommandValue::Error { message: error.to_string() });
                    return Ok(id);
                }
            }
            let result = match apply_resource_status_patch_checked(
                &convoys,
                &record_name,
                &convoy_external_patches::force_work_completed(work.clone(), chrono::Utc::now(), message.clone()),
                check_work_is_completable,
            )
            .await
            {
                Ok(_) => CommandValue::Ok,
                Err(err) => CommandValue::Error { message: err.to_string() },
            };
            self.events.finish(id, empty_identity, result);
            return Ok(id);
        }
        Err("ConvoyWorkForceComplete action selected the wrong handler".to_string())
    }

    /// An operator's verdict on one promise (#3587 layer 6): the engine's
    /// attributed `Keep` or `Retract`. It needs an operator principal, the
    /// `[primitives]` switch, and a convoy the engine holds.
    pub async fn execute_action_convoy_promise_verdict(
        &self,
        id: u64,
        command: &Command,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        if let CommandAction::ConvoyPromiseVerdict { convoy, promise, keep, reason } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let result = async {
                let principal = dispatching_principal_ref.as_ref().ok_or("a promise verdict requires an operator principal")?;
                if reason.trim().is_empty() {
                    return Err("a promise verdict requires a non-empty reason".to_string());
                }
                let namespace = self.crew.provisioning_namespace().await;
                let record_name = resolve_local_convoy_name(self.resource_backend, &namespace, convoy).await?;
                let current = self
                    .resource_backend
                    .clone()
                    .using::<ResourceConvoy>(&namespace)
                    .get(&record_name)
                    .await
                    .map_err(|error| error.to_string())?;
                let engine = flotilla_resources::convoy::engine::deciding(&self.crew.convoy_engine, &current)
                    .ok_or_else(|| format!("convoy `{convoy}` is not decided by the engine here; promise verdicts need `[primitives]`"))?;
                engine.verdict(&current, promise, *keep, &principal.name, reason).await
            }
            .await;
            let result = match result {
                Ok(()) => CommandValue::Ok,
                Err(message) => CommandValue::Error { message },
            };
            self.events.finish(id, empty_identity, result);
            return Ok(id);
        }
        Err("ConvoyPromiseVerdict action selected the wrong handler".to_string())
    }
}

impl CrewActions<'_> {
    pub async fn execute_action_message_deliver_batch(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::MessageDeliverBatch { namespace, name, reason } = &command.action {
            let identity = self.events.start(id, command.description().to_string());
            let result = if self.crew.message_primitives().is_some() {
                CommandValue::Error { message: "engine delivery does not hold submission batches".into() }
            } else {
                match self.crew.message_inbox(namespace).await.deliver_held_batch(name, reason, self.clock.now()).await {
                    Ok(()) => CommandValue::Ok,
                    Err(error) => CommandValue::Error { message: error.to_string() },
                }
            };
            self.events.finish(id, identity, result);
            return Ok(id);
        }
        Err("MessageDeliverBatch action selected the wrong handler".into())
    }

    pub async fn execute_action_message_fail_batch(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::MessageFailBatch { namespace, name, reason } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            if self.crew.message_primitives().is_some() {
                // The engine path submits no batches; a message it cannot deliver drops as undeliverable.
                let result =
                    CommandValue::Error { message: format!("Message `{name}` is on the engine path, which holds no submission batches") };
                self.events.finish(id, empty_identity, result);
                return Ok(id);
            }
            let result = match self.crew.message_inbox(namespace).await.fail_batch(name, reason, self.clock.now()).await {
                Ok(()) => CommandValue::Ok,
                Err(error) => CommandValue::Error { message: error.to_string() },
            };
            self.events.finish(id, empty_identity, result);
            return Ok(id);
        }
        Err("MessageFailBatch action selected the wrong handler".into())
    }
}
