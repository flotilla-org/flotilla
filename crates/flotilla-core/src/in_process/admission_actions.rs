//! Convoy admission and project/workflow declaration commands.
//!
//! Handlers use the capability-owned port below; the composition root supplies
//! orchestration collaborators and shares their existing state.
use super::{
    create_adopted_checkout_resource, empty_repo_identity, parse_and_validate_workflow_template_yaml, parse_project_yaml,
    placement_target_host, AdoptedCheckoutRequest,
};
use crate::event_sink::EventSink;
use crate::in_process::convoy_admission::allocate_convoy_generation;
use crate::in_process::convoy_admission::convoy_record_name;
use crate::in_process::convoy_admission::normalize_convoy_start_intent;
use crate::in_process::convoy_admission::resolve_and_validate_workflow_credentials;
use crate::in_process::convoy_admission::validate_convoy_name;
use crate::in_process::convoy_admission::ConvoyAdmission;
use crate::in_process::convoy_admission::ConvoyCreateAdmission;
use crate::in_process::convoy_admission::ConvoyStartKey;
use crate::in_process::convoy_admission::ConvoyStartTask;
use crate::in_process::convoy_admission::PlacementResolution;
use crate::in_process::project_ops::is_declaration_backed_project;
use crate::in_process::project_ops::validate_project_name;
use crate::repository_inspection::RepositoryInspection;
use async_trait::async_trait;
use flotilla_protocol::Command;
use flotilla_protocol::CommandAction;
use flotilla_protocol::CommandValue;
use flotilla_protocol::DaemonEvent;
use flotilla_protocol::NodeId;
use flotilla_protocol::PlacementDecision;
use flotilla_protocol::PrincipalRef;
use flotilla_resources::normalize_project_spec;
use flotilla_resources::ConvoyRepositorySpec;
use flotilla_resources::InputMeta;
use flotilla_resources::Project;
use flotilla_resources::Repository;
use flotilla_resources::RepositoryKey;
use flotilla_resources::RepositorySpec;
use flotilla_resources::ResourceBackend;
use flotilla_resources::ResourceError;
use flotilla_resources::WorkflowTemplate;
use flotilla_resources::WorkflowTemplateSpec;
use flotilla_resources::WriterIdentity;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

#[async_trait]
pub(super) trait AdmissionActionPort: Send + Sync {
    async fn repository_transport_url(&self, namespace: &str, repository: &RepositorySpec) -> Result<String, String>;
    async fn check_local_free_space_floor(&self) -> Result<(), String>;
    async fn check_remote_placement_free_space_floor(&self, namespace: &str, placement: Option<&PlacementDecision>) -> Result<(), String>;
    fn convoy_admission(&self) -> &ConvoyAdmission;
    fn event_sink(&self) -> &Arc<dyn EventSink>;
    fn finish_context_free_command(
        &self,
        command_id: u64,
        repo_identity: flotilla_protocol::RepoIdentity,
        result: flotilla_protocol::CommandValue,
    );
    async fn inspect_adopted_checkout(
        &self,
        path: &Path,
        repository_url: Option<&str>,
        git_ref: Option<&str>,
    ) -> Result<RepositoryInspection, String>;
    fn node_id(&self) -> &NodeId;
    fn observed_checkout_reconciliation(&self) -> &Arc<Mutex<()>>;
    fn observed_resource_backend(&self) -> &ResourceBackend;
    async fn project_add(
        &self,
        target: &str,
        explicit_name: Option<&str>,
        explicit_display_name: Option<&str>,
        remote: Option<&str>,
    ) -> Result<String, String>;
    async fn project_refresh(&self, name: &str) -> Result<(usize, bool, Vec<String>, Vec<String>), String>;
    async fn project_register(&self, target: &str) -> Result<(String, usize), String>;
    async fn provisioning_namespace(&self) -> String;
    async fn resolve_convoy_placement(
        &self,
        namespace: &str,
        project_ref: Option<&str>,
        repositories: &[ConvoyRepositorySpec],
        workflow: &WorkflowTemplateSpec,
        placement_policy: Option<&str>,
        allow_unready: bool,
    ) -> Result<PlacementResolution, String>;
    async fn resolve_repository_remote(&self, remote: &str) -> Result<RepositorySpec, String>;
    fn resource_backend(&self) -> &ResourceBackend;
    async fn roll_convoy_ensure(&self, namespace: &str, name: &str) -> Result<String, String>;
    async fn snapshot_project_repositories(
        &self,
        namespace: &str,
        project_ref: &str,
        selected: Option<&[RepositoryKey]>,
    ) -> Result<Vec<ConvoyRepositorySpec>, String>;
    fn spawn_convoy_start(&self, task: ConvoyStartTask) -> bool;
    fn start_context_free_command(&self, command_id: u64, description: String) -> flotilla_protocol::RepoIdentity;
}

pub(super) struct AdmissionActions<'a> {
    pub(super) port: &'a dyn AdmissionActionPort,
}

impl AdmissionActions<'_> {
    pub(super) async fn execute_action_ensure_roll(&self, id: u64, command: &Command) -> Result<u64, String> {
        let CommandAction::ConvoyEnsureRoll { namespace, name } = &command.action else {
            return Err("ensure roll selected the wrong handler".into());
        };
        let identity = self.port.start_context_free_command(id, command.description().to_string());
        let result = match self.port.roll_convoy_ensure(namespace, name).await {
            Ok(message) => CommandValue::ResourceReconciled { resource_kind: "ConvoyEnsure".into(), name: name.clone(), message },
            Err(message) => CommandValue::Error { message },
        };
        self.port.finish_context_free_command(id, identity, result);
        Ok(id)
    }

    pub(super) async fn execute_action_convoy_start(
        &self,
        id: u64,
        command: &Command,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let dispatching_principal_ref = dispatching_principal_ref.clone();
        if let flotilla_protocol::CommandAction::ConvoyStart { intent } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let acting_namespace = self.port.provisioning_namespace().await;
            let default_namespace = intent.namespace.clone().unwrap_or_else(|| acting_namespace.clone());
            let (namespace, intent) = match normalize_convoy_start_intent(&default_namespace, intent) {
                Ok(resolved) => resolved,
                Err(message) => {
                    self.port.finish_context_free_command(id, empty_identity, flotilla_protocol::CommandValue::Error { message });
                    return Ok(id);
                }
            };
            let dispatching_principal_ref =
                dispatching_principal_ref.clone().unwrap_or_else(|| PrincipalRef::implicit_for_namespace(&acting_namespace));
            let key = ConvoyStartKey::new(namespace, &intent);
            if !self.port.convoy_admission().mark_pending(key.clone()).await {
                self.port.finish_context_free_command(
                    id,
                    empty_identity,
                    flotilla_protocol::CommandValue::Error {
                        message: format!("convoy start for project {} is already in progress", intent.project_ref),
                    },
                );
                return Ok(id);
            }
            let task = ConvoyStartTask::builder()
                .command_id(id)
                .intent(intent)
                .key(key.clone())
                .dispatching_principal_ref(dispatching_principal_ref)
                .build();
            if !self.port.spawn_convoy_start(task) {
                self.port.convoy_admission().clear_pending(&key).await;
                self.port.finish_context_free_command(
                    id,
                    empty_identity,
                    flotilla_protocol::CommandValue::Error { message: "convoy start worker is unavailable".to_string() },
                );
            }
            return Ok(id);
        }
        Err("ConvoyStart action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_convoy_create(
        &self,
        id: u64,
        command: &Command,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let dispatching_principal_ref = dispatching_principal_ref.clone();
        if let flotilla_protocol::CommandAction::ConvoyCreate {
            name,
            workflow_ref,
            inputs,
            repository_url,
            r#ref,
            project_ref,
            placement_policy,
            adopted_checkout,
        } = &command.action
        {
            let empty_identity = empty_repo_identity();
            self.port.event_sink().emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let namespace = self.port.provisioning_namespace().await;
            let role = name.clone();
            let project_identity = project_ref.as_deref();
            if let Err(message) = validate_convoy_name(&role) {
                let result = flotilla_protocol::CommandValue::Error { message };
                self.port.event_sink().emit(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.port.node_id().clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result,
                });
                return Ok(id);
            }
            if let Err(message) = self.port.check_local_free_space_floor().await {
                let result = flotilla_protocol::CommandValue::Error { message };
                self.port.event_sink().emit(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.port.node_id().clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result,
                });
                return Ok(id);
            }
            // Use the admission transaction before checking identity or writing
            // adopted checkout resources. A duplicate must have no side effects.
            let admission_guard = self.port.convoy_admission().lock().await;
            if let Err(message) = allocate_convoy_generation(self.port.resource_backend(), &namespace, project_identity, &role).await {
                let result = flotilla_protocol::CommandValue::Error { message };
                self.port.event_sink().emit(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.port.node_id().clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result,
                });
                return Ok(id);
            }
            let record_name = convoy_record_name();
            let name = &record_name;
            let mut workflow = match self
                .port
                .resource_backend()
                .clone()
                .including_replicas::<WorkflowTemplate>(&namespace)
                .get(workflow_ref)
                .await
                .map(|source| source.object)
                .map_err(|error| format!("workflow template {workflow_ref}: {error}"))
            {
                Ok(workflow) => workflow,
                Err(message) => {
                    self.port.event_sink().emit(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: self.port.node_id().clone(),
                        repo_identity: empty_identity,
                        repo: None,
                        result: flotilla_protocol::CommandValue::Error { message },
                    });
                    return Ok(id);
                }
            };
            let project_repositories = if let Some(project_ref) = project_ref {
                match self.port.snapshot_project_repositories(&namespace, project_ref, None).await {
                    Ok(repositories) => Some(repositories),
                    Err(message) => {
                        self.port.event_sink().emit(DaemonEvent::CommandFinished {
                            command_id: id,
                            node_id: self.port.node_id().clone(),
                            repo_identity: empty_identity,
                            repo: None,
                            result: flotilla_protocol::CommandValue::Error { message },
                        });
                        return Ok(id);
                    }
                }
            } else {
                None
            };
            if project_repositories.is_some() && repository_url.is_some() {
                let message = "convoy repository selection is not allowed when a project is supplied".to_string();
                self.port.event_sink().emit(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.port.node_id().clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result: flotilla_protocol::CommandValue::Error { message },
                });
                return Ok(id);
            }
            let mut direct_repository_url = repository_url.clone();
            let mut r#ref = r#ref.clone();
            let adopted_checkout = match adopted_checkout {
                Some(path) => {
                    let adopted_result = async {
                        let inspection =
                            self.port.inspect_adopted_checkout(path.as_ref(), direct_repository_url.as_deref(), r#ref.as_deref()).await?;
                        let repo_ref = inspection.spec.key();
                        let transport_url = inspection
                            .transport_url
                            .as_deref()
                            .ok_or_else(|| "an adopted checkout requires a repository transport URL".to_string())?;
                        let git_ref = r#ref.as_deref().unwrap_or(&inspection.checkout.git_ref);
                        let _reconciliation = self.port.observed_checkout_reconciliation().lock().await;
                        let (checkout_ref, inferred_repository_url, inferred_ref) = create_adopted_checkout_resource(
                            self.port.resource_backend(),
                            self.port.observed_resource_backend(),
                            AdoptedCheckoutRequest::builder()
                                .namespace(&namespace)
                                .convoy_name(name)
                                .checkout_path(&inspection.checkout.path)
                                .repository_spec(&inspection.spec)
                                .repository_url(transport_url)
                                .git_ref(git_ref)
                                .host_ref(&inspection.checkout.host_ref)
                                .build(),
                        )
                        .await?;
                        Ok::<_, String>((repo_ref, checkout_ref, inferred_repository_url, inferred_ref))
                    }
                    .await;
                    match adopted_result {
                        Ok((repo_ref, checkout_ref, inferred_repository_url, inferred_ref)) => {
                            if project_repositories.is_none() {
                                direct_repository_url.get_or_insert(inferred_repository_url);
                            }
                            r#ref.get_or_insert(inferred_ref);
                            Some((repo_ref, checkout_ref))
                        }
                        Err(message) => {
                            let result = flotilla_protocol::CommandValue::Error { message };
                            self.port.event_sink().emit(DaemonEvent::CommandFinished {
                                command_id: id,
                                node_id: self.port.node_id().clone(),
                                repo_identity: empty_identity,
                                repo: None,
                                result,
                            });
                            return Ok(id);
                        }
                    }
                }
                None => None,
            };
            let repositories = if let Some(repositories) = project_repositories {
                repositories
            } else if let Some(url) = direct_repository_url {
                let resolved = async {
                    let repository_spec = self.port.resolve_repository_remote(&url).await?;
                    let canonical_url = self.port.repository_transport_url(&namespace, &repository_spec).await?;
                    let repo_ref = repository_spec.key();
                    let repository = flotilla_resources::ensure_repository(
                        &self.port.resource_backend().clone().using::<Repository>(&namespace),
                        &repo_ref,
                        &repository_spec,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    let default_ref = repository
                        .status
                        .as_ref()
                        .and_then(|status| status.default_branch.clone())
                        .or_else(|| if adopted_checkout.is_some() { r#ref.clone() } else { None })
                        .ok_or_else(|| format!("repository {repo_ref} has no resolved default branch"))?;
                    let workspace_slug = flotilla_resources::repository_workspace_slugs([(&repo_ref, &repository_spec)])
                        .remove(&repo_ref)
                        .expect("repository slug should resolve");
                    Ok::<_, String>(vec![ConvoyRepositorySpec {
                        url: canonical_url,
                        repo_ref,
                        source_ref: default_ref.clone(),
                        target_ref: default_ref,
                        workspace_slug,
                        subpaths: Vec::new(),
                    }])
                }
                .await;
                match resolved {
                    Ok(repositories) => repositories,
                    Err(message) => {
                        self.port.event_sink().emit(DaemonEvent::CommandFinished {
                            command_id: id,
                            node_id: self.port.node_id().clone(),
                            repo_identity: empty_identity,
                            repo: None,
                            result: flotilla_protocol::CommandValue::Error { message },
                        });
                        return Ok(id);
                    }
                }
            } else {
                Vec::new()
            };
            let adopted_checkout_ref_to_cleanup = adopted_checkout.as_ref().map(|(_, checkout_ref)| checkout_ref.clone());
            let mut adopted_checkout_refs = BTreeMap::new();
            if let Some((repo_ref, checkout_ref)) = adopted_checkout {
                if !repositories.iter().any(|repository| repository.repo_ref == repo_ref) {
                    let message =
                        format!("adopted checkout repository {repo_ref} is not part of project {}", project_ref.as_deref().unwrap_or(""));
                    self.port.event_sink().emit(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: self.port.node_id().clone(),
                        repo_identity: empty_identity,
                        repo: None,
                        result: flotilla_protocol::CommandValue::Error { message },
                    });
                    return Ok(id);
                }
                adopted_checkout_refs.insert(repo_ref, checkout_ref);
            }
            let placement = match self
                .port
                .resolve_convoy_placement(
                    &namespace,
                    project_ref.as_deref(),
                    &repositories,
                    &workflow.spec,
                    placement_policy.as_deref(),
                    false,
                )
                .await
            {
                Ok(placement) => placement,
                Err(message) => {
                    self.port.event_sink().emit(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: self.port.node_id().clone(),
                        repo_identity: empty_identity,
                        repo: None,
                        result: flotilla_protocol::CommandValue::Error { message },
                    });
                    return Ok(id);
                }
            };
            let credential_result = resolve_and_validate_workflow_credentials(
                self.port.resource_backend(),
                &namespace,
                project_ref.as_deref(),
                &repositories,
                placement.selected.as_ref(),
                &mut workflow.spec,
            )
            .await;
            if let Err(message) = credential_result {
                self.port.event_sink().emit(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.port.node_id().clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result: flotilla_protocol::CommandValue::Error { message },
                });
                return Ok(id);
            }
            let placement_decision = match placement.selected.as_ref() {
                Some(selected) => match placement_target_host(self.port.resource_backend(), &namespace, selected).await {
                    Ok(target_host) => Some(PlacementDecision {
                        minimal_alternatives: Vec::new(),
                        escalation_reason: None,
                        policy_name: selected.metadata.name.clone(),
                        target_host,
                        refused_candidates: placement.refused_candidates.clone(),
                        viable_not_selected: placement.viable_not_selected.clone(),
                        allocation: placement.allocation.clone(),
                    }),
                    Err(message) => {
                        self.port.event_sink().emit(DaemonEvent::CommandFinished {
                            command_id: id,
                            node_id: self.port.node_id().clone(),
                            repo_identity: empty_identity,
                            repo: None,
                            result: flotilla_protocol::CommandValue::Error { message },
                        });
                        return Ok(id);
                    }
                },
                None => None,
            };
            if let Err(message) = self.port.check_remote_placement_free_space_floor(&namespace, placement_decision.as_ref()).await {
                self.port.event_sink().emit(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.port.node_id().clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result: flotilla_protocol::CommandValue::Error { message },
                });
                return Ok(id);
            }
            let result = self
                .port
                .convoy_admission()
                .admit_created_convoy(
                    ConvoyCreateAdmission::builder()
                        .namespace(&namespace)
                        .name(name)
                        .role(&role)
                        .workflow_ref(workflow_ref)
                        .workflow(&workflow.spec)
                        .placement(placement)
                        .maybe_placement_decision(placement_decision)
                        .inputs(inputs)
                        .repositories(repositories)
                        .maybe_source_ref(r#ref)
                        .maybe_project_ref(project_ref.clone())
                        .adopted_checkout_refs(adopted_checkout_refs)
                        .maybe_adopted_checkout_ref_to_cleanup(adopted_checkout_ref_to_cleanup)
                        .maybe_dispatching_principal_ref(dispatching_principal_ref)
                        .build(),
                    admission_guard,
                )
                .await;
            self.port.event_sink().emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("ConvoyCreate action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_workflow_template_apply(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::WorkflowTemplateApply { name, spec_yaml } = &command.action {
            let empty_identity = empty_repo_identity();
            self.port.event_sink().emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let namespace = self.port.provisioning_namespace().await;
            let templates = self.port.resource_backend().clone().using::<WorkflowTemplate>(&namespace);
            let result = match parse_and_validate_workflow_template_yaml(spec_yaml) {
                Ok(spec) => {
                    let meta = InputMeta::builder().name(name.clone()).build();
                    let outcome = match templates.get(name).await {
                        Ok(existing) => templates.update(&meta, &existing.metadata.resource_version, &spec).await.map(|_| ()),
                        Err(ResourceError::NotFound { .. }) => templates.create(&meta, &spec).await.map(|_| ()),
                        Err(err) => Err(err),
                    };
                    match outcome {
                        Ok(()) => flotilla_protocol::CommandValue::WorkflowTemplateApplied { name: name.clone() },
                        Err(err) => flotilla_protocol::CommandValue::Error { message: err.to_string() },
                    }
                }
                Err(err) => flotilla_protocol::CommandValue::Error { message: err },
            };
            self.port.event_sink().emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("WorkflowTemplateApply action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_project_add(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ProjectAdd { target, name, display_name, remote } = &command.action {
            let empty_identity = empty_repo_identity();
            self.port.event_sink().emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let result = match self.port.project_add(target, name.as_deref(), display_name.as_deref(), remote.as_deref()).await {
                Ok(name) => flotilla_protocol::CommandValue::ProjectAdded { name },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.port.event_sink().emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("ProjectAdd action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_project_apply(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ProjectApply { name, spec_yaml } = &command.action {
            let empty_identity = empty_repo_identity();
            self.port.event_sink().emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let namespace = self.port.provisioning_namespace().await;
            let projects = self.port.resource_backend().clone().definitions::<Project>(&namespace);
            let result = match validate_project_name(name).and_then(|_| parse_project_yaml(spec_yaml)) {
                Ok(spec) => match normalize_project_spec(spec) {
                    Ok(spec) => {
                        let outcome = match projects.get(name).await {
                            Ok(existing) if is_declaration_backed_project(&existing) => {
                                Err(format!("project {name} is managed by a declaration; use project refresh to update it"))
                            }
                            Ok(existing) => projects
                                .apply_as(
                                    &WriterIdentity::operator().with_source("project-apply"),
                                    &InputMeta::from(&existing.metadata),
                                    &spec,
                                )
                                .await
                                .map(|_| ())
                                .map_err(|error| error.to_string()),
                            Err(ResourceError::NotFound { .. }) => projects
                                .apply_as(
                                    &WriterIdentity::operator().with_source("project-apply"),
                                    &InputMeta::builder().name(name.clone()).build(),
                                    &spec,
                                )
                                .await
                                .map(|_| ())
                                .map_err(|error| error.to_string()),
                            Err(error) => Err(error.to_string()),
                        };
                        match outcome {
                            Ok(()) => flotilla_protocol::CommandValue::ProjectApplied { name: name.clone() },
                            Err(message) => flotilla_protocol::CommandValue::Error { message },
                        }
                    }
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                },
                Err(err) => flotilla_protocol::CommandValue::Error { message: err },
            };
            self.port.event_sink().emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("ProjectApply action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_project_register(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ProjectRegister { target } = &command.action {
            let empty_identity = empty_repo_identity();
            self.port.event_sink().emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let result = match self.port.project_register(target).await {
                Ok((name, members)) => CommandValue::ProjectRegistered { name, members },
                Err(message) => CommandValue::Error { message },
            };
            self.port.event_sink().emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("ProjectRegister action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_project_refresh(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ProjectRefresh { name } = &command.action {
            let empty_identity = empty_repo_identity();
            self.port.event_sink().emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let result = match self.port.project_refresh(name).await {
                Ok((members, converged, changes, operational_entries)) => {
                    CommandValue::ProjectRefreshed { name: name.clone(), members, converged, changes, operational_entries }
                }
                Err(message) => CommandValue::Error { message },
            };
            self.port.event_sink().emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("ProjectRefresh action selected the wrong handler".to_string())
    }
}
