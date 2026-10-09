//! Crew lifecycle, convoy supervision, and crew delivery commands.
//!
//! Handlers use the capability-owned port below; the composition root supplies
//! orchestration collaborators and shares their existing state.
use super::{crew_ops, resolve_local_convoy_name};
use crate::in_process::crew_ops::ConvoyResumeOutcome;
use crate::in_process::crew_ops::CrewRoutingContext;
use crate::in_process::crew_ops::CrewSupervisionRequest;
use crate::in_process::crew_ops::MessageAttribution;
use async_trait::async_trait;
use flotilla_protocol::CheckoutArchiveOutcome;
use flotilla_protocol::Command;
use flotilla_protocol::CommandAction;
use flotilla_protocol::CommandValue;
use flotilla_protocol::CrewCommandContext;
use flotilla_protocol::PrincipalRef;
use flotilla_resources::apply_status_patch_checked as apply_resource_status_patch_checked;
use flotilla_resources::external_patches as convoy_external_patches;
use flotilla_resources::Convoy as ResourceConvoy;
use flotilla_resources::ResourceBackend;
use flotilla_resources::ResourceError;
use flotilla_resources::ResourceObject;
use std::sync::Arc;

#[async_trait]
pub(super) trait CrewActionPort: Send + Sync {
    fn clock(&self) -> &Arc<dyn flotilla_resources::Clock>;
    async fn message_inbox(&self, namespace: &str) -> flotilla_resources::MessageInbox;
    async fn convoy_resume_with_sender_internal(
        &self,
        namespace: &str,
        name: &str,
        prompt: &str,
        requested_vessel: Option<&str>,
        requested_role: Option<&str>,
        attribution: MessageAttribution,
    ) -> Result<ConvoyResumeOutcome, String>;
    async fn handoff_with_carries(
        &self,
        requested: &CrewCommandContext,
        target: &str,
        message: &str,
        carries: Vec<flotilla_resources::MessageReference>,
    ) -> Result<(), String>;
    async fn abandon_convoy_internal(
        &self,
        namespace: &str,
        name: &str,
        reason: &str,
        principal_ref: Option<&PrincipalRef>,
    ) -> Result<Vec<CheckoutArchiveOutcome>, String>;
    async fn convoy_withdraw_pending_brief_internal(&self, namespace: &str, name: &str) -> Result<Option<String>, String>;
    async fn crew_complete_as_principal_internal(
        &self,
        requested: &CrewCommandContext,
        message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
        force: bool,
        principal: Option<PrincipalRef>,
    ) -> Result<flotilla_protocol::CommandValue, String>;
    async fn crew_fail_internal(
        &self,
        requested: &CrewCommandContext,
        message: String,
        force: bool,
        principal: Option<&PrincipalRef>,
    ) -> Result<(), String>;
    async fn crew_stall_internal(
        &self,
        requested: &CrewCommandContext,
        reason: flotilla_protocol::StallReason,
        proposed_disposition: Option<flotilla_protocol::StallProposedDisposition>,
        message: String,
    ) -> Result<(), String>;
    async fn crew_supervise_internal(&self, request: CrewSupervisionRequest<'_>) -> Result<(), String>;
    fn finish_context_free_command(
        &self,
        command_id: u64,
        repo_identity: flotilla_protocol::RepoIdentity,
        result: flotilla_protocol::CommandValue,
    );
    async fn link_convoy_subject(
        &self,
        namespace: &str,
        convoy_name: &str,
        reference: &str,
        relationship: Option<flotilla_protocol::Relationship>,
    ) -> Result<(), String>;
    async fn provisioning_namespace(&self) -> String;
    async fn reap_convoy_internal(&self, namespace: &str, name: &str, force: bool) -> Result<(), String>;
    async fn record_lifecycle_mutation_best_effort(
        &self,
        namespace: &str,
        name: &str,
        action: &str,
        caller: Option<&flotilla_protocol::CommandCaller>,
        missing_expected: bool,
    );
    async fn resolve_crew_routing_context(&self, requested: &CrewCommandContext) -> Result<CrewRoutingContext, String>;
    fn resource_backend(&self) -> &ResourceBackend;
    fn start_context_free_command(&self, command_id: u64, description: String) -> flotilla_protocol::RepoIdentity;
}

pub(super) struct CrewActions<'a> {
    pub(super) port: &'a dyn CrewActionPort,
}

impl CrewActions<'_> {
    pub(super) async fn execute_action_artifact_reserve_ledger_comment(&self, id: u64, command: &Command) -> Result<u64, String> {
        let CommandAction::ArtifactReserveLedgerComment { namespace, name, address } = &command.action else {
            return Err("ledger reservation selected the wrong handler".into());
        };
        let identity = self.port.start_context_free_command(id, command.description().to_string());
        let result = match flotilla_resources::reserve_ledger_comment_creation(self.port.resource_backend(), namespace, name, address).await
        {
            Ok(granted) => CommandValue::LedgerCommentCreationReserved { granted },
            Err(error) => CommandValue::Error { message: error.to_string() },
        };
        self.port.finish_context_free_command(id, identity, result);
        Ok(id)
    }

    pub(super) async fn execute_action_crew_handoff(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::CrewHandoff { context, target, message, carries } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let result = match Box::pin(self.port.handoff_with_carries(context, target, message, carries.clone())).await {
                Ok(()) => flotilla_protocol::CommandValue::Ok,
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("CrewHandoff action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_convoy_resume(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<flotilla_protocol::CommandCaller>,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        let dispatching_principal_ref = dispatching_principal_ref.clone();
        if let flotilla_protocol::CommandAction::ConvoyResume { namespace, name, prompt, vessel, role } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let namespace = namespace.clone().unwrap_or(self.port.provisioning_namespace().await);
            let result = match resolve_local_convoy_name(self.port.resource_backend(), &namespace, name).await {
                Ok(record_name) => {
                    match Box::pin(self.port.convoy_resume_with_sender_internal(
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
                            self.port
                                .record_lifecycle_mutation_best_effort(&namespace, &record_name, "convoy_resume", caller.as_ref(), false)
                                .await;
                            flotilla_protocol::CommandValue::ConvoyBriefDelivered { displaced }
                        }
                        Ok(ConvoyResumeOutcome::Queued { displaced }) => {
                            self.port
                                .record_lifecycle_mutation_best_effort(&namespace, &record_name, "convoy_resume", caller.as_ref(), false)
                                .await;
                            flotilla_protocol::CommandValue::ConvoyBriefQueued { displaced }
                        }
                        Err(message) => flotilla_protocol::CommandValue::Error { message },
                    }
                }
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("ConvoyResume action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_convoy_withdraw_pending_brief(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ConvoyWithdrawPendingBrief { namespace, name } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let namespace = namespace.clone().unwrap_or(self.port.provisioning_namespace().await);
            let result = match resolve_local_convoy_name(self.port.resource_backend(), &namespace, name).await {
                Ok(record_name) => match self.port.convoy_withdraw_pending_brief_internal(&namespace, &record_name).await {
                    Ok(withdrawn) => flotilla_protocol::CommandValue::ConvoyBriefWithdrawn { withdrawn },
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("ConvoyWithdrawPendingBrief action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_crew_complete(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<flotilla_protocol::CommandCaller>,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        let dispatching_principal_ref = dispatching_principal_ref.clone();
        if let flotilla_protocol::CommandAction::CrewComplete { context, message, disposition, decision_ledger_ref, force } =
            &command.action
        {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let routing = Box::pin(self.port.resolve_crew_routing_context(context)).await.ok();
            let result = match self
                .port
                .crew_complete_as_principal_internal(
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
                        self.port
                            .record_lifecycle_mutation_best_effort(namespace, &resolved.convoy, "crew_complete", caller.as_ref(), false)
                            .await;
                    }
                    value
                }
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("CrewComplete action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_crew_fail(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<flotilla_protocol::CommandCaller>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        if let flotilla_protocol::CommandAction::CrewFail { context, message, force } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let operator =
                caller.as_ref().filter(|caller| caller.crew.is_none() && context.crew_id.is_none()).map(|caller| &caller.principal_ref);
            let result = match self.port.crew_fail_internal(context, message.clone(), *force, operator).await {
                Ok(()) => flotilla_protocol::CommandValue::Ok,
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("CrewFail action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_crew_stall(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::CrewStall { context, reason, proposed_disposition, message } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let result = match self.port.crew_stall_internal(context, *reason, *proposed_disposition, message.clone()).await {
                Ok(()) => flotilla_protocol::CommandValue::Ok,
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("CrewStall action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_crew_supervise(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<flotilla_protocol::CommandCaller>,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        let dispatching_principal_ref = dispatching_principal_ref.clone();
        if let flotilla_protocol::CommandAction::CrewSupervise { namespace, convoy, vessel, role, operation, message, actor_crew_id } =
            &command.action
        {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let namespace = namespace.clone().unwrap_or(self.port.provisioning_namespace().await);
            let result = match resolve_local_convoy_name(self.port.resource_backend(), &namespace, convoy).await {
                Ok(name) => match self
                    .port
                    .crew_supervise_internal(
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
                        self.port
                            .record_lifecycle_mutation_best_effort(
                                &namespace,
                                &name,
                                &format!("crew_supervise_{operation:?}"),
                                caller.as_ref(),
                                false,
                            )
                            .await;
                        flotilla_protocol::CommandValue::Ok
                    }
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("CrewSupervise action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_convoy_link(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ConvoyLink { namespace, name, reference, relationship } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.port.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(self.port.resource_backend(), &namespace, name).await {
                Ok(record_name) => self.port.link_convoy_subject(&namespace, &record_name, reference, Some(*relationship)).await,
                Err(error) => Err(error),
            };
            self.port.finish_context_free_command(
                id,
                empty_identity,
                result.map_or_else(|message| flotilla_protocol::CommandValue::Error { message }, |()| flotilla_protocol::CommandValue::Ok),
            );
            return Ok(id);
        }
        Err("ConvoyLink action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_convoy_unlink(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ConvoyUnlink { namespace, name, reference } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.port.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(self.port.resource_backend(), &namespace, name).await {
                Ok(record_name) => self.port.link_convoy_subject(&namespace, &record_name, reference, None).await,
                Err(error) => Err(error),
            };
            self.port.finish_context_free_command(
                id,
                empty_identity,
                result.map_or_else(|message| flotilla_protocol::CommandValue::Error { message }, |()| flotilla_protocol::CommandValue::Ok),
            );
            return Ok(id);
        }
        Err("ConvoyUnlink action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_convoy_delete(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<flotilla_protocol::CommandCaller>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        if let flotilla_protocol::CommandAction::ConvoyDelete { namespace, name, force } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.port.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(self.port.resource_backend(), &namespace, name).await {
                Ok(record_name) => match self.port.reap_convoy_internal(&namespace, &record_name, *force).await {
                    Ok(()) => {
                        // Finalizers retain an explainable convoy after delete; a fully
                        // removed convoy has no remaining status to annotate.
                        self.port
                            .record_lifecycle_mutation_best_effort(&namespace, &record_name, "convoy_delete", caller.as_ref(), true)
                            .await;
                        flotilla_protocol::CommandValue::Ok
                    }
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("ConvoyDelete action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_convoy_abandon(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<flotilla_protocol::CommandCaller>,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        let dispatching_principal_ref = dispatching_principal_ref.clone();
        if let flotilla_protocol::CommandAction::ConvoyAbandon { namespace, name, reason } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.port.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(self.port.resource_backend(), &namespace, name).await {
                Ok(record_name) => {
                    match self.port.abandon_convoy_internal(&namespace, &record_name, reason, dispatching_principal_ref.as_ref()).await {
                        Ok(archives) => {
                            self.port
                                .record_lifecycle_mutation_best_effort(&namespace, &record_name, "convoy_abandon", caller.as_ref(), false)
                                .await;
                            flotilla_protocol::CommandValue::ConvoyAbandoned { name: name.clone(), archives }
                        }
                        Err(message) => flotilla_protocol::CommandValue::Error { message },
                    }
                }
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("ConvoyAbandon action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_convoy_work_force_complete(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ConvoyWorkForceComplete { convoy, work, message } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let namespace = self.port.provisioning_namespace().await;
            let convoys = self.port.resource_backend().clone().using::<ResourceConvoy>(&namespace);
            let record_name = match resolve_local_convoy_name(self.port.resource_backend(), &namespace, convoy).await {
                Ok(record_name) => record_name,
                Err(message) => {
                    self.port.finish_context_free_command(id, empty_identity, flotilla_protocol::CommandValue::Error { message });
                    return Ok(id);
                }
            };
            let check_work_is_completable = |current: &ResourceObject<ResourceConvoy>| match current.status.as_ref() {
                None => Err(ResourceError::other(format!("convoy {convoy} has no status"))),
                Some(status) => match status.work.get(work) {
                    None => Err(ResourceError::other(format!("convoy {convoy} does not contain work {work}"))),
                    Some(state)
                        if matches!(
                            state.phase,
                            flotilla_resources::WorkPhase::Failed
                                | flotilla_resources::WorkPhase::Cancelled
                                | flotilla_resources::WorkPhase::Abandoned
                        ) =>
                    {
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
                Ok(_) => flotilla_protocol::CommandValue::Ok,
                Err(err) => flotilla_protocol::CommandValue::Error { message: err.to_string() },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("ConvoyWorkForceComplete action selected the wrong handler".to_string())
    }
}

impl CrewActions<'_> {
    pub(super) async fn execute_action_message_fail_batch(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::MessageFailBatch { namespace, name, reason } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let result = match self.port.message_inbox(namespace).await.fail_batch(name, reason, self.port.clock().now()).await {
                Ok(()) => CommandValue::Ok,
                Err(error) => CommandValue::Error { message: error.to_string() },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("MessageFailBatch action selected the wrong handler".into())
    }
}
