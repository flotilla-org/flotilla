//! Query conversion, enrichment, and resource watch commands.
//!
//! Handlers use the capability-owned port below; the composition root supplies
//! orchestration collaborators and shares their existing state.
use super::{empty_repo_identity, read_projections};
use crate::config::ConfigStore;
use crate::event_sink::EventSink;
use crate::in_process::attach::ResolvedAttach;
use crate::providers::issue_tracker::IssueProvider;
use crate::resource_explain::resource_read_envelope;
use crate::resource_explain::resource_record;
use crate::resource_explain::run_resource_watch_command;
use crate::resource_explain::ResourceWatchCommandContext;
use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::commands::AttachMode;
use flotilla_protocol::AttachBinding;
use flotilla_protocol::CliListKind;
use flotilla_protocol::CliListResponse;
use flotilla_protocol::Command;
use flotilla_protocol::CommandValue;
use flotilla_protocol::ConvoyExplanation;
use flotilla_protocol::CrewCommandContext;
use flotilla_protocol::CrewListResponse;
use flotilla_protocol::DaemonEvent;
use flotilla_protocol::DispatchQueueResponse;
use flotilla_protocol::EnvironmentId;
use flotilla_protocol::FleetHealthResponse;
use flotilla_protocol::FleetListResponse;
use flotilla_protocol::FulfilmentListResponse;
use flotilla_protocol::HostListResponse;
use flotilla_protocol::HostName;
use flotilla_protocol::HostProvidersResponse;
use flotilla_protocol::HostStatusResponse;
use flotilla_protocol::NodeId;
use flotilla_protocol::ProjectListResponse;
use flotilla_protocol::RepoProvidersResponse;
use flotilla_protocol::ResourceCursor;
use flotilla_protocol::ResourceRecordType;
use flotilla_resources::current_resource_kind_position;
use flotilla_resources::get_resource_kind_including_replicas;
use flotilla_resources::list_resource_kind;
use flotilla_resources::list_resource_kind_including_replicas;
use flotilla_resources::resolve_project_issue_sources;
use flotilla_resources::Clock;
use flotilla_resources::EventRecorder;
use flotilla_resources::EventRegarding;
use flotilla_resources::IssueSourceResolution;
use flotilla_resources::IssueSourceUnavailable;
use flotilla_resources::Repository;
use flotilla_resources::RepositoryKey;
use flotilla_resources::ResourceBackend;
use flotilla_resources::ResourceError;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::warn;

#[async_trait]
pub(super) trait ProjectionsActionPort: Send + Sync {
    async fn attach_project_context(&self, selector: Option<&flotilla_protocol::RepoSelector>) -> Result<Option<String>, String>;
    async fn message_contacts_internal(&self, requested: &CrewCommandContext) -> Result<flotilla_resources::CrewAddressBook, String>;
    async fn scoped_fleet_list(
        &self,
        project: Option<&str>,
        crew_id: Option<&str>,
        convoy: Option<&str>,
    ) -> Result<FleetListResponse, String>;
    async fn resolve_repository_selector(&self, selector: &flotilla_protocol::RepoSelector) -> Result<Option<RepositoryKey>, String>;
    async fn resolve_attach_with_context(
        &self,
        reference: &str,
        host: Option<&HostName>,
        transient: bool,
        mode: AttachMode,
        project_context: Option<&str>,
    ) -> Result<ResolvedAttach, String>;
    fn node_id(&self) -> &NodeId;
    async fn list_projects_internal(&self) -> Result<ProjectListResponse, String>;
    async fn list_hosts_internal(&self) -> Result<HostListResponse, String>;
    async fn list_cli_items_internal(&self, kind: CliListKind) -> Result<CliListResponse, String>;
    async fn get_repo_providers_internal(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoProvidersResponse, String>;
    async fn get_issue_provider_for_repository(
        &self,
        selector: &flotilla_protocol::RepoSelector,
    ) -> Result<(Arc<dyn IssueProvider>, flotilla_protocol::IssueSource), String>;
    async fn get_host_status_internal(&self, environment_id: &EnvironmentId) -> Result<HostStatusResponse, String>;
    async fn get_host_providers_internal(&self, environment_id: &EnvironmentId) -> Result<HostProvidersResponse, String>;
    async fn fulfilment_list_internal(&self) -> Result<FulfilmentListResponse, String>;
    async fn fleet_health_internal(&self) -> Result<FleetHealthResponse, String>;
    async fn explain_project_internal(&self, name: &str) -> Result<serde_json::Value, String>;
    async fn explain_convoy_internal(&self, requested_namespace: Option<&str>, name: &str) -> Result<ConvoyExplanation, String>;
    async fn emit_attach_regard(&self, binding: &AttachBinding, surface_id: uuid::Uuid) -> Result<(), String>;
    async fn dispatch_queue_internal(&self, project_filter: Option<&str>) -> Result<DispatchQueueResponse, String>;
    async fn dispatch_board_internal(&self, project_filter: Option<&str>) -> Result<flotilla_protocol::DispatchBoardResponse, String>;
    async fn crew_list_internal(&self, requested: &CrewCommandContext) -> Result<CrewListResponse, String>;
    async fn crew_capabilities_internal(&self, requested: &CrewCommandContext) -> Result<String, String>;
    fn config(&self) -> &Arc<ConfigStore>;
    fn clock(&self) -> &Arc<dyn Clock>;
    fn active_commands(&self) -> &Arc<Mutex<HashMap<u64, CancellationToken>>>;
    fn event_sink(&self) -> &Arc<dyn EventSink>;
    fn observed_resource_backend(&self) -> &ResourceBackend;
    fn resource_backend(&self) -> &ResourceBackend;
}

pub(super) struct ProjectionsActions<'a> {
    pub(super) port: &'a dyn ProjectionsActionPort,
}

impl ProjectionsActions<'_> {
    pub(super) async fn execute_action_resource_watch(&self, id: u64, command: &Command, command_node_id: &NodeId) -> Result<u64, String> {
        let command_node_id = command_node_id.clone();
        if let flotilla_protocol::CommandAction::ResourceWatch { namespace, kind, name, include_replicas, replica_sources, cursor } =
            command.action.clone()
        {
            let repo_identity = empty_repo_identity();
            let description = format!("watch resource {namespace}/{kind}");
            let token = CancellationToken::new();
            {
                let mut guard = self.port.active_commands().lock().await;
                guard.insert(id, token.clone());
            }
            self.port.event_sink().emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: command_node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: None,
                description,
            });

            let (backend, kind) = match kind.strip_prefix("observed/") {
                Some(kind) => (self.port.observed_resource_backend().clone(), kind.to_string()),
                None => (self.port.resource_backend().clone(), kind),
            };
            let event_sink = self.port.event_sink().clone();
            let active_ref = Arc::clone(self.port.active_commands());
            tokio::spawn(async move {
                let result = run_resource_watch_command(
                    ResourceWatchCommandContext::builder()
                        .backend(backend)
                        .namespace(namespace)
                        .kind(kind)
                        .maybe_name(name)
                        .include_replicas(include_replicas)
                        .replica_sources(replica_sources)
                        .maybe_cursor(cursor)
                        .command_id(id)
                        .node_id(command_node_id.clone())
                        .repo_identity(repo_identity.clone())
                        .event_sink(event_sink.clone())
                        .token(token)
                        .build(),
                )
                .await;
                active_ref.lock().await.remove(&id);
                event_sink.emit(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: command_node_id,
                    repo_identity,
                    repo: None,
                    result,
                });
            });
            return Ok(id);
        }
        Err("ResourceWatch action selected the wrong handler".to_string())
    }
}

impl ProjectionsActions<'_> {
    pub(super) async fn execute_query(&self, command: Command, session_id: uuid::Uuid) -> Result<flotilla_protocol::CommandValue, String> {
        use flotilla_protocol::CommandAction;
        match &command.action {
            CommandAction::QueryResolveRepository { repo } => {
                let key = self.port.resolve_repository_selector(repo).await?;
                Ok(CommandValue::RepositoryResolved { key })
            }
            CommandAction::QueryRepoProviders { repo } => match self.port.get_repo_providers_internal(repo).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::RepoProviders(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryHostList {} => match self.port.list_hosts_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::HostList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryExplainProject { name } => match self.port.explain_project_internal(name).await {
                Ok(explanation) => Ok(CommandValue::ProjectExplanation(explanation)),
                Err(message) => Ok(CommandValue::Error { message }),
            },
            CommandAction::QueryProjectList {} => match self.port.list_projects_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::ProjectList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryCliList { kind } => match self.port.list_cli_items_internal(*kind).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::CliList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryDispatchBoard { project } => match self.port.dispatch_board_internal(project.as_deref()).await {
                Ok(board) => Ok(CommandValue::DispatchBoard(Box::new(board))),
                Err(error) => Ok(CommandValue::Error { message: error }),
            },
            CommandAction::QueryDispatchQueue { project } => match self.port.dispatch_queue_internal(project.as_deref()).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::DispatchQueue(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryHostStatus { target_environment_id } => {
                match self.port.get_host_status_internal(target_environment_id).await {
                    Ok(v) => Ok(flotilla_protocol::CommandValue::HostStatus(Box::new(v))),
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::QueryHostProviders { target_environment_id } => {
                match self.port.get_host_providers_internal(target_environment_id).await {
                    Ok(v) => Ok(flotilla_protocol::CommandValue::HostProviders(Box::new(v))),
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::QueryFleetHealth {} => match self.port.fleet_health_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::FleetHealth(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryFulfilmentList {} => match self.port.fulfilment_list_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::FulfilmentList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryFleetList { project, crew_id, convoy } => {
                match self.port.scoped_fleet_list(project.as_deref(), crew_id.as_deref(), convoy.as_deref()).await {
                    Ok(v) => Ok(flotilla_protocol::CommandValue::FleetList(Box::new(v))),
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::QueryCrewStalls { full } => {
                match read_projections::ReadProjections::crew_stalls(self.port.resource_backend(), *full, self.port.clock().now()).await {
                    Ok(value) => Ok(CommandValue::CrewStalls(Box::new(value))),
                    Err(message) => Ok(CommandValue::Error { message }),
                }
            }
            CommandAction::QueryCrewCapabilities { context } => match self.port.crew_capabilities_internal(context).await {
                Ok(card) => Ok(CommandValue::CrewCapabilities { card }),
                Err(message) => Ok(CommandValue::Error { message }),
            },
            CommandAction::QueryMessageContacts { context } => match self.port.message_contacts_internal(context).await {
                Ok(book) => Ok(CommandValue::MessageContacts {
                    text: book.render(),
                    book: serde_json::to_value(book).map_err(|error| error.to_string())?,
                }),
                Err(message) => Ok(CommandValue::Error { message }),
            },
            CommandAction::QueryCrewList { context } => match self.port.crew_list_internal(context).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::CrewList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryDaemonLogs { query } => {
                let generations = self.port.config().load_daemon_config()?.logging.generations;
                let state_dir = self.port.config().state_dir().as_path().to_path_buf();
                let query = query.clone();
                let read_result = tokio::task::spawn_blocking(move || crate::log_file::read_daemon_logs(&state_dir, generations, &query))
                    .await
                    .map_err(|error| format!("daemon log reader task failed: {error}"))?;
                match read_result {
                    Ok(lines) => Ok(flotilla_protocol::CommandValue::DaemonLogs { lines }),
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::QueryExplainConvoy { namespace, name } => {
                match self.port.explain_convoy_internal(namespace.as_deref(), name).await {
                    Ok(explanation) => Ok(CommandValue::ConvoyExplanation(Box::new(explanation))),
                    Err(message) => Ok(CommandValue::Error { message }),
                }
            }
            CommandAction::QueryResourceDigest { namespace, kind, query } => {
                let (kind, backend) = match kind.strip_prefix("observed/") {
                    Some(kind) => (kind, self.port.observed_resource_backend()),
                    None => (kind.as_str(), self.port.resource_backend()),
                };
                let result = flotilla_resources::digest_resource_kind(backend, namespace, kind, query).await;
                match result {
                    Ok(digest) => Ok(CommandValue::ResourceDigest(Box::new(digest.into()))),
                    Err(error) => Ok(CommandValue::Error { message: error.to_string() }),
                }
            }
            CommandAction::QueryResourceList { namespace, kind, include_replicas } => {
                let listed = if *include_replicas {
                    list_resource_kind_including_replicas(self.port.resource_backend(), namespace, kind).await
                } else {
                    list_resource_kind(self.port.resource_backend(), namespace, kind).await
                };
                match listed {
                    Ok(v) => {
                        let resource_version = v.value["metadata"]["resourceVersion"].as_str().unwrap_or_default().to_string();
                        let generation = v.value["metadata"]["generation"].as_str().map(ToOwned::to_owned);
                        let records = v.value["items"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .cloned()
                            .map(|object| resource_record(ResourceRecordType::Current, object, self.port.node_id()))
                            .collect();
                        Ok(CommandValue::ResourceRead(Box::new(resource_read_envelope(
                            v.kind,
                            v.plural,
                            v.namespace,
                            ResourceCursor::from_position(resource_version, generation),
                            records,
                        ))))
                    }
                    Err(error) => Ok(CommandValue::Error { message: error.to_string() }),
                }
            }
            CommandAction::QueryResourceGet { namespace, kind, name } => {
                // Take the collection cursor before reading the object. A
                // concurrent mutation can then be replayed (at worst as a
                // duplicate) instead of being hidden behind a newer cursor.
                let position = match current_resource_kind_position(self.port.resource_backend(), namespace, kind).await {
                    Ok(listed) => listed,
                    Err(error) => return Ok(CommandValue::Error { message: error.to_string() }),
                };
                let visible = match get_resource_kind_including_replicas(self.port.resource_backend(), namespace, kind, name).await {
                    Ok(object) => object,
                    Err(ResourceError::NotFound { .. }) => {
                        return Ok(CommandValue::Error { message: format!("resource {kind}/{namespace}/{name} not found") });
                    }
                    Err(error) => return Ok(CommandValue::Error { message: error.to_string() }),
                };
                let resource_version = position.resource_version;
                let generation = position.generation;
                let mut value = visible.value;
                if visible.kind == "Project" {
                    match serde_json::from_value::<flotilla_resources::ProjectSpec>(value["spec"].clone()) {
                        Ok(spec) => {
                            match resolve_project_issue_sources(
                                &self.port.resource_backend().including_replicas::<Repository>(namespace),
                                &spec,
                            )
                            .await
                            {
                                IssueSourceResolution::Available { bindings } => {
                                    value["resolvedIssueSources"] = serde_json::Value::Array(
                                        bindings
                                            .into_iter()
                                            .map(|binding| {
                                                serde_json::json!({
                                                    "service": binding.source.service,
                                                    "scope": binding.source.scope,
                                                    "alias": binding.alias,
                                                    "creatable": binding.creatable,
                                                })
                                            })
                                            .collect(),
                                    );
                                }
                                IssueSourceResolution::Unavailable(reason) => {
                                    value["resolvedIssueSources"] = serde_json::Value::Array(Vec::new());
                                    let message = match reason {
                                        IssueSourceUnavailable::RepositoryUnavailable { repository, message } => {
                                            format!("repository {repository}: {message}")
                                        }
                                        IssueSourceUnavailable::InvalidBindings { message } => message,
                                        IssueSourceUnavailable::NoIssueSource => format!("project {name} has no issue source"),
                                    };
                                    value["issueSourceResolutionError"] = serde_json::Value::String(message);
                                }
                            }
                        }
                        Err(error) => {
                            warn!(resource_kind = %visible.kind, resource = %name, %error, "failed to decode project spec for resource read");
                        }
                    }
                }
                if visible.kind != "Event" {
                    let object_name = value["metadata"]["name"].as_str().unwrap_or(name);
                    let regarding = EventRegarding {
                        api_version: value["apiVersion"].as_str().unwrap_or("flotilla.work/v1").to_string(),
                        kind: visible.kind.clone(),
                        namespace: namespace.clone(),
                        name: object_name.to_string(),
                    };
                    match EventRecorder::new(self.port.resource_backend().clone()).recent_for(&regarding, Utc::now()).await {
                        Ok(events) if !events.is_empty() => {
                            value["recentEvents"] =
                                serde_json::Value::Array(events.into_iter().filter_map(|event| serde_json::to_value(event).ok()).collect());
                        }
                        Ok(_) => {}
                        Err(error) => {
                            warn!(resource_kind = %visible.kind, resource = %name, %error, "failed to enrich resource read with recent events")
                        }
                    }
                }
                let record = resource_record(ResourceRecordType::Current, value, self.port.node_id());
                Ok(CommandValue::ResourceRead(Box::new(resource_read_envelope(
                    visible.kind,
                    visible.plural,
                    visible.namespace,
                    ResourceCursor::from_position(resource_version, generation),
                    vec![record],
                ))))
            }
            CommandAction::Attach { reference, host, mode } => {
                let project_context = self.port.attach_project_context(command.context_repo.as_ref()).await?;
                match self.port.resolve_attach_with_context(reference, host.as_ref(), false, *mode, project_context.as_deref()).await {
                    Ok(resolved) => {
                        if let Some(binding) = &resolved.binding {
                            if let Err(error) = self.port.emit_attach_regard(binding, session_id).await {
                                warn!(%error, "failed to emit attach regard");
                            }
                        }
                        Ok(flotilla_protocol::CommandValue::AttachCommandResolved { plan: resolved.plan, binding: resolved.binding })
                    }
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::AttachTransient { reference, host, mode } => {
                let project_context = self.port.attach_project_context(command.context_repo.as_ref()).await?;
                match self.port.resolve_attach_with_context(reference, host.as_ref(), true, *mode, project_context.as_deref()).await {
                    Ok(resolved) => {
                        Ok(flotilla_protocol::CommandValue::AttachCommandResolved { plan: resolved.plan, binding: resolved.binding })
                    }
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::QueryIssues { repo, params, page, count } => {
                let (provider, source) = self.port.get_issue_provider_for_repository(repo).await?;
                let page = provider.query(&source, params, *page, *count).await?;
                Ok(flotilla_protocol::CommandValue::IssuePage(page))
            }
            CommandAction::QueryIssueFetchByIds { repo, ids } => {
                let (provider, source) = self.port.get_issue_provider_for_repository(repo).await?;
                let items = provider.fetch_by_ids(&source, ids).await?;
                Ok(flotilla_protocol::CommandValue::IssuesByIds { items })
            }
            CommandAction::QueryIssueOpenInBrowser { repo, id } => {
                let (provider, source) = self.port.get_issue_provider_for_repository(repo).await?;
                provider.open_in_browser(&flotilla_protocol::IssueRef { source, id: id.clone() }).await?;
                Ok(flotilla_protocol::CommandValue::Ok)
            }
            other => Err(format!("execute_query not implemented for this command type: {:?}", std::mem::discriminant(other))),
        }
    }
}
