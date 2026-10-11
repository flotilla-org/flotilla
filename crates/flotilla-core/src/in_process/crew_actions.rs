//! Crew lifecycle, convoy supervision, and crew delivery commands.
//!
//! Handlers use the capability-owned port below; the composition root supplies
//! orchestration collaborators and shares their existing state.
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

use super::{crew_ops, resolve_local_convoy_name};
use crate::in_process::crew_ops::ConvoyResumeOutcome;
use crate::in_process::crew_ops::CrewSupervisionRequest;

/// Subject reference resolution requires host repository context. Lifecycle,
/// routing, message admission and audit mutations use the shared CrewService
/// directly rather than calling back through the composition root.
#[async_trait]
pub(super) trait CrewActionPort: Send + Sync {
    async fn link_convoy_subject(
        &self,
        namespace: &str,
        convoy_name: &str,
        reference: &str,
        relationship: Option<Relationship>,
    ) -> Result<(), String>;
}

pub(super) struct CrewActions<'a> {
    pub(super) port: &'a dyn CrewActionPort,
    pub(super) crew: &'a crew_ops::CrewService,
    pub(super) resource_backend: &'a ResourceBackend,
    pub(super) clock: &'a Arc<dyn Clock>,
    pub(super) events: super::action_events::ActionEvents<'a>,
}

impl CrewActions<'_> {
    pub(super) async fn execute_action_crew_promise(&self, id: u64, command: &Command) -> Result<u64, String> {
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

    pub(super) async fn execute_action_artifact_reserve_ledger_comment(&self, id: u64, command: &Command) -> Result<u64, String> {
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

    pub(super) async fn execute_action_crew_handoff(&self, id: u64, command: &Command) -> Result<u64, String> {
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

    pub(super) async fn execute_action_convoy_resume(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<CommandCaller>,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        let dispatching_principal_ref = dispatching_principal_ref.clone();
        if let CommandAction::ConvoyResume { namespace, name, prompt, vessel, role } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
            let namespace = namespace.clone().unwrap_or(self.crew.provisioning_namespace().await);
            let result = match resolve_local_convoy_name(self.resource_backend, &namespace, name).await {
                Ok(record_name) => {
                    match Box::pin(self.crew.convoy_resume_with_sender_internal(
                        &namespace,
                        &record_name,
                        prompt,
                        vessel.as_deref(),
                        role.as_deref(),
                        crew_ops::MessageAttribution::operator(dispatching_principal_ref.as_ref()),
                    ))
                    .await
                    {
                        Ok(ConvoyResumeOutcome::Delivered { displaced }) => {
                            self.crew
                                .record_lifecycle_mutation_best_effort(&namespace, &record_name, "convoy_resume", caller.as_ref(), false)
                                .await;
                            CommandValue::ConvoyBriefDelivered { displaced }
                        }
                        Ok(ConvoyResumeOutcome::Queued { displaced }) => {
                            self.crew
                                .record_lifecycle_mutation_best_effort(&namespace, &record_name, "convoy_resume", caller.as_ref(), false)
                                .await;
                            CommandValue::ConvoyBriefQueued { displaced }
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

    pub(super) async fn execute_action_convoy_withdraw_pending_brief(&self, id: u64, command: &Command) -> Result<u64, String> {
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

    pub(super) async fn execute_action_crew_complete(
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

    pub(super) async fn execute_action_crew_fail(&self, id: u64, command: &Command, caller: &Option<CommandCaller>) -> Result<u64, String> {
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

    pub(super) async fn execute_action_crew_stall(&self, id: u64, command: &Command) -> Result<u64, String> {
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

    pub(super) async fn execute_action_crew_supervise(
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
                    Ok(()) => {
                        self.crew
                            .record_lifecycle_mutation_best_effort(
                                &namespace,
                                &name,
                                &format!("crew_supervise_{operation:?}"),
                                caller.as_ref(),
                                false,
                            )
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
        Err("CrewSupervise action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_convoy_link(&self, id: u64, command: &Command) -> Result<u64, String> {
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

    pub(super) async fn execute_action_convoy_unlink(&self, id: u64, command: &Command) -> Result<u64, String> {
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

    pub(super) async fn execute_action_convoy_delete(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<CommandCaller>,
    ) -> Result<u64, String> {
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

    pub(super) async fn execute_action_convoy_abandon(
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

    pub(super) async fn execute_action_convoy_work_force_complete(&self, id: u64, command: &Command) -> Result<u64, String> {
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
}

impl CrewActions<'_> {
    pub(super) async fn execute_action_message_fail_batch(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::MessageFailBatch { namespace, name, reason } = &command.action {
            let empty_identity = self.events.start(id, command.description().to_string());
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
