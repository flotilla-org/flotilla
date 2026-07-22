#![allow(dead_code)]

use std::{collections::BTreeMap, future::Future, time::Duration};

use chrono::{DateTime, Utc};
use flotilla_resources::{
    canonicalize_repo_url, ensure_repository, repo_key, Checkout, CheckoutPhase, CheckoutSpec, CheckoutStatus, Clone, ClonePhase,
    CloneSpec, CloneStatus, Convoy, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, DockerCheckoutStrategy,
    DockerEnvironmentSpec, DockerPerVesselPlacementPolicySpec, Environment, EnvironmentPhase, EnvironmentSpec, EnvironmentStatus,
    HostDirectEnvironmentSpec, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, InputMeta, PlacementPolicy,
    PlacementPolicySpec, Repository, RepositorySpec, ResourceBackend, TerminalSession, TerminalSessionPhase, TerminalSessionSpec,
    TerminalSessionStatus, Vessel, VesselRequirement, VesselSpec, WorkCompletionAuthority, WorkPhase, WorkState, WorkflowSnapshot,
};
use tokio::{
    task::JoinHandle,
    time::{sleep, Instant},
};

#[bon::builder]
pub fn controller_meta(
    name: &str,
    #[builder(default)] labels: BTreeMap<String, String>,
    #[builder(default)] annotations: BTreeMap<String, String>,
    #[builder(default)] owner_references: Vec<flotilla_resources::OwnerReference>,
    #[builder(default)] finalizers: Vec<String>,
    deletion_timestamp: Option<DateTime<Utc>>,
) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .labels(labels)
        .annotations(annotations)
        .owner_references(owner_references)
        .finalizers(finalizers)
        .maybe_deletion_timestamp(deletion_timestamp)
        .build()
}

pub fn meta(name: &str) -> InputMeta {
    controller_meta().name(name).call()
}

pub fn labeled_meta(name: &str, labels: impl IntoIterator<Item = (String, String)>) -> InputMeta {
    controller_meta().name(name).labels(labels.into_iter().collect()).call()
}

pub fn vessel_meta(name: &str, repo_url: &str) -> InputMeta {
    let canonical_repo = canonicalize_repo_url(repo_url).expect("repo URL should canonicalize");
    controller_meta().name(name).labels([("flotilla.work/repo-key".to_string(), repo_key(&canonical_repo))].into_iter().collect()).call()
}

#[bon::builder]
pub fn work_state(
    phase: WorkPhase,
    ready_at: Option<DateTime<Utc>>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    message: Option<String>,
    placement: Option<flotilla_resources::PlacementStatus>,
) -> WorkState {
    WorkState { phase, completion_authority: WorkCompletionAuthority::CrewRollup, ready_at, started_at, finished_at, message, placement }
}

pub async fn create_convoy_with_single_task(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    task: &str,
    repo_url: &str,
    git_ref: &str,
) -> flotilla_resources::ResourceObject<Convoy> {
    let canonical_repo = canonicalize_repo_url(repo_url).expect("repository URL should canonicalize");
    let repository_spec = RepositorySpec::remote(canonical_repo).expect("canonical repository identity should be valid");
    let repository_key = repository_spec.key();
    ensure_repository(&backend.clone().using::<Repository>(namespace), &repository_key, &repository_spec)
        .await
        .expect("repository create should succeed");
    let convoys = backend.clone().using::<Convoy>(namespace);
    let convoy = convoys
        .create(&meta(name), &ConvoySpec {
            workflow_ref: "wf".to_string(),
            inputs: Default::default(),
            placement_policy: None,
            repositories: vec![ConvoyRepositorySpec {
                url: repo_url.to_string(),
                repo_ref: repository_key,
                base_ref: git_ref.to_string(),
                workspace_slug: repository_spec.leaf_slug(),
                subpaths: Vec::new(),
            }],
            r#ref: Some(git_ref.to_string()),
            project_ref: None,
            adopted_checkout_refs: BTreeMap::new(),
            issue: None,
            instruction: None,
        })
        .await
        .expect("convoy create should succeed");
    convoys
        .update_status(name, &convoy.metadata.resource_version, &ConvoyStatus {
            workflow_snapshot: Some(WorkflowSnapshot {
                vessels: vec![VesselRequirement {
                    name: task.to_string(),
                    stance: Default::default(),
                    depends_on: Vec::new(),
                    repository_refs: None,
                    crew: vec![CrewSpec::builder()
                        .role("coder".to_string())
                        .source(CrewSource::Tool { command: "cargo test".to_string() })
                        .build()],
                }],
            }),
            ..Default::default()
        })
        .await
        .expect("convoy status update should succeed");
    convoys.get(name).await.expect("convoy get should succeed")
}

pub async fn create_workspace(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    convoy_ref: &str,
    task: &str,
    placement_policy_ref: &str,
    repo_url: &str,
) -> flotilla_resources::ResourceObject<Vessel> {
    let workspaces = backend.clone().using::<Vessel>(namespace);
    let mut meta = vessel_meta(name, repo_url);
    meta.labels.insert(flotilla_resources::CONVOY_LABEL.to_string(), convoy_ref.to_string());
    workspaces
        .create(&meta, &VesselSpec {
            convoy_ref: convoy_ref.to_string(),
            vessel_name: task.to_string(),
            placement_policy_ref: placement_policy_ref.to_string(),
            adopted_checkout_refs: BTreeMap::new(),
        })
        .await
        .expect("workspace create should succeed")
}

pub async fn create_policy(backend: &ResourceBackend, namespace: &str, name: &str, spec: PlacementPolicySpec) {
    backend.clone().using::<PlacementPolicy>(namespace).create(&meta(name), &spec).await.expect("policy create should succeed");
}

pub async fn create_host_direct_policy(backend: &ResourceBackend, namespace: &str, name: &str, host_ref: &str, pool: &str) {
    create_policy(
        backend,
        namespace,
        name,
        PlacementPolicySpec::builder()
            .pool(pool.to_string())
            .host_direct(HostDirectPlacementPolicySpec {
                host_ref: host_ref.to_string(),
                checkout: HostDirectPlacementPolicyCheckout::Worktree,
            })
            .build(),
    )
    .await;
}

#[derive(bon::Builder)]
pub struct DockerWorktreePolicyFixture {
    pub name: String,
    pub host_ref: String,
    pub pool: String,
    pub image: String,
    pub mount_path: String,
    pub default_cwd: Option<String>,
}

pub async fn create_docker_worktree_policy(backend: &ResourceBackend, namespace: &str, fixture: DockerWorktreePolicyFixture) {
    create_policy(
        backend,
        namespace,
        &fixture.name,
        PlacementPolicySpec::builder()
            .pool(fixture.pool)
            .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                host_ref: fixture.host_ref,
                image: fixture.image,
                agent_adapters: Default::default(),
                default_cwd: fixture.default_cwd,
                env: Default::default(),
                checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: fixture.mount_path },
            })
            .build(),
    )
    .await;
}

pub async fn create_ready_host_direct_environment(
    backend: &ResourceBackend,
    namespace: &str,
    host_ref: &str,
    repo_default_dir: &str,
) -> flotilla_resources::ResourceObject<Environment> {
    let environments = backend.clone().using::<Environment>(namespace);
    let name = format!("host-direct-{host_ref}");
    let created = environments
        .create(&meta(&name), &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: host_ref.to_string(), repo_default_dir: repo_default_dir.to_string() }),
            docker: None,
        })
        .await
        .expect("environment create should succeed");
    environments
        .update_status(&name, &created.metadata.resource_version, &EnvironmentStatus {
            phase: EnvironmentPhase::Ready,
            ready: true,
            docker_container_id: None,
            message: None,
        })
        .await
        .expect("environment status update should succeed");
    environments.get(&name).await.expect("environment get should succeed")
}

pub async fn create_ready_docker_environment(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    docker: DockerEnvironmentSpec,
) -> flotilla_resources::ResourceObject<Environment> {
    let environments = backend.clone().using::<Environment>(namespace);
    let created = environments
        .create(&meta(name), &EnvironmentSpec { host_direct: None, docker: Some(docker) })
        .await
        .expect("docker env create should succeed");
    environments
        .update_status(name, &created.metadata.resource_version, &EnvironmentStatus {
            phase: EnvironmentPhase::Ready,
            ready: true,
            docker_container_id: Some(format!("container-{name}")),
            message: None,
        })
        .await
        .expect("docker env status update should succeed");
    environments.get(name).await.expect("docker env get should succeed")
}

pub async fn create_ready_clone(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    repo_url: &str,
    env_ref: &str,
    path: &str,
) -> flotilla_resources::ResourceObject<Clone> {
    let clones = backend.clone().using::<Clone>(namespace);
    let created = clones
        .create(&meta(name), &CloneSpec {
            repo_ref: flotilla_resources::RepositoryKey(repo_key(&canonicalize_repo_url(repo_url).expect("repo URL should canonicalize"))),
            url: repo_url.to_string(),
            env_ref: env_ref.to_string(),
            path: path.to_string(),
        })
        .await
        .expect("clone create should succeed");
    clones
        .update_status(name, &created.metadata.resource_version, &CloneStatus {
            phase: ClonePhase::Ready,
            default_branch: Some("main".to_string()),
            message: None,
        })
        .await
        .expect("clone status update should succeed");
    clones.get(name).await.expect("clone get should succeed")
}

#[derive(bon::Builder)]
pub struct ReadyCheckoutFixture {
    pub name: String,
    pub env_ref: String,
    pub git_ref: String,
    pub path: String,
    pub worktree: Option<flotilla_resources::CheckoutWorktreeSpec>,
    pub fresh_clone: Option<flotilla_resources::FreshCloneCheckoutSpec>,
}

pub async fn create_ready_checkout(
    backend: &ResourceBackend,
    namespace: &str,
    fixture: ReadyCheckoutFixture,
) -> flotilla_resources::ResourceObject<Checkout> {
    let checkouts = backend.clone().using::<Checkout>(namespace);
    let spec = match (fixture.worktree, fixture.fresh_clone) {
        (Some(worktree), None) => CheckoutSpec::Worktree(worktree),
        (None, Some(fresh_clone)) => CheckoutSpec::FreshClone(fresh_clone),
        (None, None) => panic!("ready checkout fixture must provide a checkout strategy"),
        (Some(_), Some(_)) => panic!("ready checkout fixture must provide exactly one checkout strategy"),
    };
    let created = checkouts.create(&meta(&fixture.name), &spec).await.expect("checkout create should succeed");
    checkouts
        .update_status(&fixture.name, &created.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: Some(fixture.path.clone()),
            commit: Some("44982740".to_string()),
            branch_provenance: Default::default(),
            integration: Default::default(),
            message: None,
        })
        .await
        .expect("checkout status update should succeed");
    checkouts.get(&fixture.name).await.expect("checkout get should succeed")
}

#[derive(bon::Builder)]
pub struct StoppedTerminalFixture {
    pub name: String,
    pub env_ref: String,
    pub role: String,
    pub command: String,
    pub cwd: String,
    pub pool: String,
    pub message: String,
}

pub async fn create_stopped_terminal(
    backend: &ResourceBackend,
    namespace: &str,
    fixture: StoppedTerminalFixture,
) -> flotilla_resources::ResourceObject<TerminalSession> {
    let sessions = backend.clone().using::<TerminalSession>(namespace);
    let created = sessions
        .create(&meta(&fixture.name), &TerminalSessionSpec {
            env_ref: fixture.env_ref,
            role: fixture.role,
            source: flotilla_resources::TerminalSessionSource::Tool { command: fixture.command.clone() },
            cwd: fixture.cwd,
            pool: fixture.pool,
        })
        .await
        .expect("terminal create should succeed");
    sessions
        .update_status(&fixture.name, &created.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Stopped,
            session_id: Some(format!("session-{}", fixture.name)),
            pid: Some(42),
            started_at: Some(Utc::now()),
            stopped_at: Some(Utc::now()),
            inner_command_status: Some(flotilla_resources::InnerCommandStatus::Exited),
            inner_exit_code: Some(1),
            message: Some(fixture.message),
            crew: None,
            launch_command: Some(fixture.command),
            delivered_message_id: None,
        })
        .await
        .expect("terminal status update should succeed");
    sessions.get(&fixture.name).await.expect("terminal get should succeed")
}

pub struct ControllerLoopHarness {
    handles: Vec<JoinHandle<()>>,
    pub backend: ResourceBackend,
}

impl ControllerLoopHarness {
    pub fn new(backend: ResourceBackend) -> Self {
        Self { handles: Vec::new(), backend }
    }

    pub fn spawn<F>(&mut self, future: F)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handles.push(tokio::spawn(async move {
            let _ = future.await;
        }));
    }

    pub async fn wait_until<F, Fut>(&self, timeout: Duration, condition: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        wait_until(timeout, condition).await;
    }

    pub async fn shutdown(mut self) {
        for handle in self.handles.drain(..) {
            handle.abort();
            let _ = handle.await;
        }
    }
}

impl Drop for ControllerLoopHarness {
    fn drop(&mut self) {
        for handle in self.handles.drain(..) {
            handle.abort();
        }
    }
}

#[allow(dead_code)]
pub async fn wait_until<F, Fut>(timeout: Duration, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition().await {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("condition was not satisfied within {:?}", timeout);
}
