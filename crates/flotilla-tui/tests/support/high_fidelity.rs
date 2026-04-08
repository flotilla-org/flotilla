use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::{
        discovery::{
            test_support::{fake_vcs_discovery, FakeVcsState},
            EnvironmentAssertion, EnvironmentBag, Factory, HostPlatform, RepoDetector, UnmetRequirement, VcsKind,
        },
        types::Checkout,
        vcs::CheckoutManager,
        ChannelLabel, CommandOutput, CommandRunner,
    },
};
use flotilla_daemon::server::test_support::{spawn_in_memory_request_topology, InMemoryRequestTopology};
use flotilla_protocol::{test_support::TestCheckout, DaemonEvent, HostName, RepoIdentity, RepoSelector, WorkItemKind};
use flotilla_tui::{
    app::{self, App},
    theme::Theme,
    widgets::{delete_confirm::DeleteConfirmWidget, InteractiveWidget},
};
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};
use tempfile::TempDir;

const WIDTH: u16 = 120;
const HEIGHT: u16 = 30;

pub struct HighFidelityHarness {
    _tempdir: TempDir,
    _leader: Arc<InProcessDaemon>,
    _follower: Arc<InProcessDaemon>,
    _topology: InMemoryRequestTopology,
    remove_controller: Arc<SteppedCheckoutManager>,
    daemon_rx: tokio::sync::broadcast::Receiver<DaemonEvent>,
    leader_daemon_rx: tokio::sync::broadcast::Receiver<DaemonEvent>,
    follower_daemon_rx: tokio::sync::broadcast::Receiver<DaemonEvent>,
    recent_events: Vec<String>,
    recent_leader_events: Vec<String>,
    recent_follower_events: Vec<String>,
    last_dispatched_command: Option<String>,
    pub app: App,
}

impl HighFidelityHarness {
    pub async fn remote_checkout_removal() -> Result<Self, String> {
        let tempdir = tempfile::tempdir().map_err(|e| format!("create tempdir: {e}"))?;
        let leader_repo = tempdir.path().join("leader-repo");
        let follower_repo = tempdir.path().join("follower-repo");
        let leader_main = leader_repo.join("main");
        let follower_main = follower_repo.join("main");
        let follower_feature = follower_repo.join("feat-remote");
        let shared_identity = RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() };

        let leader_state = FakeVcsState::builder(&leader_repo)
            .checkout_raw(
                leader_main.clone(),
                TestCheckout::new("main").at(&leader_main.to_string_lossy()).is_main(true).with_branch_key().build(),
            )
            .build();
        let follower_state = FakeVcsState::builder(&follower_repo)
            .checkout_raw(
                follower_main.clone(),
                TestCheckout::new("main").at(&follower_main.to_string_lossy()).is_main(true).with_branch_key().build(),
            )
            .checkout_raw(
                follower_feature.clone(),
                TestCheckout::new("feat-remote").at(&follower_feature.to_string_lossy()).is_main(false).with_branch_key().build(),
            )
            .build();

        let leader_runtime = build_discovery_runtime(
            Arc::clone(&leader_state),
            Arc::new(ScriptedStatusRunner::idle()),
            Arc::new(SimpleCheckoutManager::new(Arc::clone(&leader_state))),
            shared_identity.clone(),
        );
        let remove_controller = Arc::new(SteppedCheckoutManager::new(Arc::clone(&follower_state)));
        let follower_runtime = build_discovery_runtime(
            Arc::clone(&follower_state),
            Arc::new(ScriptedStatusRunner::for_checkout_status(&follower_repo, &follower_feature, "feat-remote")),
            Arc::clone(&remove_controller) as Arc<dyn CheckoutManager>,
            shared_identity,
        );

        let leader = InProcessDaemon::new(
            vec![leader_repo.clone()],
            Arc::new(ConfigStore::with_base(tempdir.path().join("leader-config"))),
            leader_runtime,
            HostName::new("leader"),
        )
        .await;
        let follower = InProcessDaemon::new(
            vec![follower_repo.clone()],
            Arc::new(ConfigStore::with_base(tempdir.path().join("follower-config"))),
            follower_runtime,
            HostName::new("follower"),
        )
        .await;

        let topology = spawn_in_memory_request_topology(Arc::clone(&leader), Arc::clone(&follower)).await?;

        leader.refresh(&RepoSelector::Path(leader_repo)).await.map_err(|e| format!("refresh leader: {e}"))?;
        follower.refresh(&RepoSelector::Path(follower_repo.clone())).await.map_err(|e| format!("refresh follower: {e}"))?;

        let daemon: Arc<dyn DaemonHandle> = topology.client.clone();
        let daemon_rx = daemon.subscribe();
        let leader_daemon_rx = leader.subscribe();
        let follower_daemon_rx = follower.subscribe();
        let repos = daemon.list_repos().await?;
        let mut app =
            App::new(Arc::clone(&daemon), repos, Arc::new(ConfigStore::with_base(tempdir.path().join("tui-config"))), Theme::classic());
        for event in daemon.replay_since(&HashMap::new()).await? {
            app.handle_daemon_event(event);
        }

        let mut harness = Self {
            _tempdir: tempdir,
            _leader: leader,
            _follower: follower,
            _topology: topology,
            remove_controller,
            daemon_rx,
            leader_daemon_rx,
            follower_daemon_rx,
            recent_events: Vec::new(),
            recent_leader_events: Vec::new(),
            recent_follower_events: Vec::new(),
            last_dispatched_command: None,
            app,
        };
        harness
            .wait_for(Duration::from_secs(5), "host snapshots to initialize", |h| {
                h.app.model.my_host().is_some() && h.app.model.resolve_host(&HostName::new("follower")).is_ok()
            })
            .await?;
        Ok(harness)
    }

    pub async fn wait_for_remote_checkout(&mut self, branch: &str, timeout: Duration) -> Result<(), String> {
        self.wait_for(timeout, "remote checkout to appear", |h| h.has_checkout(branch)).await
    }

    pub fn select_checkout(&mut self, branch: &str) -> Result<(), String> {
        let repo_identity = self.app.model.active_repo_identity().clone();
        let Some(page) = self.app.screen.repo_pages.get_mut(&repo_identity) else {
            return Err(format!("active repo page missing for {}", repo_identity.path));
        };
        page.reconcile_if_changed();

        // Find the item matching the requested branch across all sections.
        let mut found = None;
        for (section_idx, item, item_idx) in page.table.all_items_with_indices() {
            if item.kind == WorkItemKind::Checkout && item.branch.as_deref() == Some(branch) {
                found = Some((section_idx, item_idx));
                break;
            }
        }
        let (section_idx, item_idx) = found.ok_or_else(|| format!("checkout row not found for branch {branch}"))?;
        page.table.select_by_mouse(section_idx, item_idx);
        Ok(())
    }

    pub async fn press_remove_shortcut(&mut self) -> Result<(), String> {
        self.app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        self.drive().await
    }

    pub async fn wait_for_delete_confirm_loaded(&mut self, timeout: Duration) -> Result<(), String> {
        self.wait_for(timeout, "delete confirmation to load", |h| h.delete_confirm_loaded()).await
    }

    pub async fn confirm_delete(&mut self) -> Result<(), String> {
        if let Some(widget) = self.app.screen.modal_stack.last().and_then(|widget| widget.as_any().downcast_ref::<DeleteConfirmWidget>()) {
            if widget.remote_node_id.is_none() {
                let selected_node_id = self.app.selected_work_item().map(|item| item.node_id.clone());
                return Err(format!(
                    "delete confirmation resolved no remote node; my_host={:?}, selected_node_id={selected_node_id:?}",
                    self.app.model.my_host()
                ));
            }
        }
        self.app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        self.drain_events()?;
        let mut dispatched = 0usize;
        while let Some((command, pending_ctx)) = self.app.proto_commands.take_next() {
            dispatched += 1;
            self.last_dispatched_command = Some(format!("{command:?}"));
            app::executor::dispatch(command, &mut self.app, pending_ctx).await;
            self.drain_events()?;
        }
        self.drain_events()?;

        if dispatched == 0 {
            let selected_node_id = self.app.selected_work_item().map(|item| item.node_id.clone());
            return Err(format!(
                "delete confirmation queued no command; my_host={:?}, selected_node_id={selected_node_id:?}, status={:?}",
                self.app.model.my_host(),
                self.app.model.status_message
            ));
        }

        if let Some(status) = self.app.model.status_message.clone() {
            return Err(format!("delete confirmation dispatch failed: {status}"));
        }

        Ok(())
    }

    pub async fn wait_for_remove_started(&mut self, timeout: Duration) -> Result<(), String> {
        self.wait_for(timeout, "checkout removal to start", |h| h.remove_controller.has_started()).await
    }

    pub async fn wait_for_progress_text(&mut self, text: &str, timeout: Duration) -> Result<(), String> {
        let expected = text.to_string();
        self.wait_for(timeout, "progress text to render", |h| h.render_to_string().contains(&expected)).await
    }

    pub fn release_remove(&self) {
        self.remove_controller.release_remove();
    }

    pub async fn wait_for_checkout_removed(&mut self, branch: &str, timeout: Duration) -> Result<(), String> {
        self.wait_for(timeout, "remote checkout to be removed", |h| !h.has_checkout(branch)).await
    }

    pub fn render_to_string(&mut self) -> String {
        buffer_to_string(&self.render_to_buffer())
    }

    async fn wait_for<F>(&mut self, timeout: Duration, label: &str, mut predicate: F) -> Result<(), String>
    where
        F: FnMut(&mut Self) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            self.drive().await?;
            if predicate(self) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let render = self.render_to_string();
                return Err(format!(
                    "timed out waiting for {label}\nstatus message: {:?}\nlast dispatched command: {:?}\nrecent client events:\n{}\nrecent leader events:\n{}\nrecent follower events:\n{}\n{render}",
                    self.app.model.status_message,
                    self.last_dispatched_command,
                    self.recent_events.join("\n"),
                    self.recent_leader_events.join("\n"),
                    self.recent_follower_events.join("\n")
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn drive(&mut self) -> Result<(), String> {
        self.drain_events()?;
        while let Some((command, pending_ctx)) = self.app.proto_commands.take_next() {
            self.last_dispatched_command = Some(format!("{command:?}"));
            app::executor::dispatch(command, &mut self.app, pending_ctx).await;
            self.drain_events()?;
        }
        self.drain_events()?;
        Ok(())
    }

    fn drain_events(&mut self) -> Result<(), String> {
        drain_daemon_events(&mut self.leader_daemon_rx, &mut self.recent_leader_events, "leader", |_| {})?;
        drain_daemon_events(&mut self.follower_daemon_rx, &mut self.recent_follower_events, "follower", |_| {})?;
        loop {
            match self.daemon_rx.try_recv() {
                Ok(event) => {
                    self.record_event(&event);
                    self.app.handle_daemon_event(event);
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return Ok(()),
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    return Err(format!("high-fidelity harness lagged on daemon events: skipped {skipped}"));
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return Err("daemon event stream closed".into()),
            }
        }
    }

    fn record_event(&mut self, event: &DaemonEvent) {
        let summary = match event {
            DaemonEvent::CommandStarted { command_id, node_id, description, .. } => {
                format!("started id={command_id} node_id={node_id} desc={description}")
            }
            DaemonEvent::CommandFinished { command_id, node_id, result, .. } => {
                format!("finished id={command_id} node_id={node_id} result={result:?}")
            }
            DaemonEvent::CommandStepUpdate { command_id, node_id, description, status, .. } => {
                format!("step id={command_id} node_id={node_id} desc={description} status={status:?}")
            }
            DaemonEvent::HostSnapshot(snap) => {
                format!(
                    "host_snapshot node_id={} display={} local={} status={:?}",
                    snap.node.node_id, snap.node.display_name, snap.is_local, snap.connection_status
                )
            }
            DaemonEvent::PeerStatusChanged { node_id, status } => format!("peer_status node_id={node_id} status={status:?}"),
            DaemonEvent::RepoSnapshot(snap) => format!("repo_snapshot repo={} seq={}", fmt_optional_path(snap.repo.as_deref()), snap.seq),
            DaemonEvent::RepoDelta(delta) => format!("repo_delta repo={} seq={}", fmt_optional_path(delta.repo.as_deref()), delta.seq),
            DaemonEvent::RepoTracked(info) => format!("repo_tracked {}", fmt_optional_path(info.path.as_deref())),
            DaemonEvent::RepoUntracked { path, .. } => format!("repo_untracked {}", fmt_optional_path(path.as_deref())),
            DaemonEvent::HostRemoved { environment_id, .. } => format!("host_removed {environment_id}"),
        };
        self.recent_events.push(summary);
        if self.recent_events.len() > 20 {
            let overflow = self.recent_events.len() - 20;
            self.recent_events.drain(0..overflow);
        }
    }

    fn has_checkout(&mut self, branch: &str) -> bool {
        let active_repo = self.app.model.active_repo_identity().clone();
        if let Some(page) = self.app.screen.repo_pages.get_mut(&active_repo) {
            page.reconcile_if_changed();
        }
        self.app.model.active_opt().is_some_and(|repo| repo.providers.checkouts.values().any(|checkout| checkout.branch == branch))
    }

    fn delete_confirm_loaded(&self) -> bool {
        self.app
            .screen
            .modal_stack
            .last()
            .and_then(|widget| widget.as_any().downcast_ref::<DeleteConfirmWidget>())
            .is_some_and(|widget| !widget.loading)
    }

    fn render_to_buffer(&mut self) -> Buffer {
        let backend = TestBackend::new(WIDTH, HEIGHT);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        let theme = self.app.theme.clone();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let mut ctx = flotilla_tui::widgets::RenderContext {
                    model: &self.app.model,
                    ui: &mut self.app.ui,
                    theme: &theme,
                    keymap: &self.app.keymap,
                    in_flight: &self.app.in_flight,
                };
                self.app.screen.render(frame, area, &mut ctx);
            })
            .expect("draw test frame");
        terminal.backend().buffer().clone()
    }
}

fn drain_daemon_events(
    rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    recent: &mut Vec<String>,
    source: &str,
    mut on_event: impl FnMut(DaemonEvent),
) -> Result<(), String> {
    loop {
        match rx.try_recv() {
            Ok(event) => {
                record_recent_event(recent, source, &event);
                on_event(event);
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return Ok(()),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                return Err(format!("{source} daemon event stream lagged: skipped {skipped}"));
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                return Err(format!("{source} daemon event stream closed"));
            }
        }
    }
}

fn fmt_optional_path(path: Option<&std::path::Path>) -> String {
    path.map(|p| p.display().to_string()).unwrap_or_else(|| "<none>".into())
}

fn record_recent_event(recent: &mut Vec<String>, source: &str, event: &DaemonEvent) {
    let summary = match event {
        DaemonEvent::CommandStarted { command_id, node_id, description, .. } => {
            format!("{source}: started id={command_id} node_id={node_id} desc={description}")
        }
        DaemonEvent::CommandFinished { command_id, node_id, result, .. } => {
            format!("{source}: finished id={command_id} node_id={node_id} result={result:?}")
        }
        DaemonEvent::CommandStepUpdate { command_id, node_id, description, status, .. } => {
            format!("{source}: step id={command_id} node_id={node_id} desc={description} status={status:?}")
        }
        DaemonEvent::HostSnapshot(snap) => {
            format!(
                "{source}: host_snapshot node_id={} display={} local={} status={:?}",
                snap.node.node_id, snap.node.display_name, snap.is_local, snap.connection_status
            )
        }
        DaemonEvent::PeerStatusChanged { node_id, status } => {
            format!("{source}: peer_status node_id={node_id} status={status:?}")
        }
        DaemonEvent::RepoSnapshot(snap) => {
            format!("{source}: repo_snapshot repo={} seq={}", fmt_optional_path(snap.repo.as_deref()), snap.seq)
        }
        DaemonEvent::RepoDelta(delta) => {
            format!("{source}: repo_delta repo={} seq={}", fmt_optional_path(delta.repo.as_deref()), delta.seq)
        }
        DaemonEvent::RepoTracked(info) => format!("{source}: repo_tracked {}", fmt_optional_path(info.path.as_deref())),
        DaemonEvent::RepoUntracked { path, .. } => format!("{source}: repo_untracked {}", fmt_optional_path(path.as_deref())),
        DaemonEvent::HostRemoved { environment_id, .. } => format!("{source}: host_removed {environment_id}"),
    };
    recent.push(summary);
    if recent.len() > 20 {
        let overflow = recent.len() - 20;
        recent.drain(0..overflow);
    }
}

fn build_discovery_runtime(
    state: Arc<RwLock<FakeVcsState>>,
    runner: Arc<dyn CommandRunner>,
    checkout_manager: Arc<dyn CheckoutManager>,
    repo_identity: RepoIdentity,
) -> flotilla_core::providers::discovery::DiscoveryRuntime {
    let mut runtime = fake_vcs_discovery(state);
    runtime.runner = runner;
    runtime.repo_detectors = vec![Box::new(FixedRepoIdentityDetector::new(repo_identity))];
    runtime.factories.checkout_managers = vec![Box::new(CheckoutManagerFactory(checkout_manager))];
    runtime
}

fn buffer_to_string(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut lines = Vec::new();
    for y in area.y..area.y + area.height {
        let mut line = String::new();
        for x in area.x..area.x + area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

struct FixedRepoIdentityDetector {
    repo_identity: RepoIdentity,
}

impl FixedRepoIdentityDetector {
    fn new(repo_identity: RepoIdentity) -> Self {
        Self { repo_identity }
    }
}

#[async_trait]
impl RepoDetector for FixedRepoIdentityDetector {
    async fn detect(
        &self,
        repo_root: &flotilla_core::path_context::ExecutionEnvironmentPath,
        _runner: &dyn CommandRunner,
        _env: &dyn flotilla_core::providers::discovery::EnvVars,
    ) -> Vec<EnvironmentAssertion> {
        let (owner, repo) = self.repo_identity.path.split_once('/').unwrap_or(("owner", "repo"));
        vec![
            EnvironmentAssertion::vcs_checkout(repo_root.as_path(), VcsKind::Git, true),
            EnvironmentAssertion::remote_host(HostPlatform::GitHub, owner, repo, "origin"),
        ]
    }
}

struct CheckoutManagerFactory(Arc<dyn CheckoutManager>);

#[async_trait]
impl Factory for CheckoutManagerFactory {
    type Descriptor = flotilla_core::providers::discovery::ProviderDescriptor;
    type Output = dyn CheckoutManager;

    fn descriptor(&self) -> flotilla_core::providers::discovery::ProviderDescriptor {
        flotilla_core::providers::discovery::ProviderDescriptor::labeled_simple(
            flotilla_core::providers::discovery::ProviderCategory::CheckoutManager,
            "stepped-checkouts",
            "Stepped Checkouts",
            "CO",
            "Checkouts",
            "checkout",
        )
    }

    async fn probe(
        &self,
        _env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &flotilla_core::path_context::ExecutionEnvironmentPath,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>> {
        Ok(Arc::clone(&self.0))
    }
}

struct ScriptedStatusRunner {
    repo_root: Option<PathBuf>,
    checkout_path: Option<PathBuf>,
    branch: Option<String>,
}

impl ScriptedStatusRunner {
    fn idle() -> Self {
        Self { repo_root: None, checkout_path: None, branch: None }
    }

    fn for_checkout_status(repo_root: &Path, checkout_path: &Path, branch: &str) -> Self {
        Self {
            repo_root: Some(repo_root.to_path_buf()),
            checkout_path: Some(checkout_path.to_path_buf()),
            branch: Some(branch.to_string()),
        }
    }

    fn response(&self, cmd: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
        match (cmd, args) {
            ("git", ["--version"]) => Ok("git version 2.43.0".into()),
            ("git", ["rev-parse", "--abbrev-ref", "origin/HEAD"]) => self.expect_repo(cwd).map(|_| "origin/main\n".into()),
            ("git", ["rev-parse", "--abbrev-ref", upstream]) => {
                self.expect_repo(cwd)?;
                let expected = format!("{}@{{upstream}}", self.branch.as_deref().unwrap_or_default());
                if *upstream == expected {
                    Err("fatal: no upstream configured".into())
                } else {
                    Err(format!("unexpected upstream lookup: {upstream}"))
                }
            }
            ("git", ["log", range, "--oneline"]) => {
                self.expect_repo(cwd)?;
                let expected = format!("origin/main..{}", self.branch.as_deref().unwrap_or_default());
                if *range == expected {
                    Ok(String::new())
                } else {
                    Err(format!("unexpected log range: {range}"))
                }
            }
            ("git", ["status", "--porcelain"]) => self.expect_checkout(cwd).map(|_| String::new()),
            _ => Err(format!("unexpected command: {cmd} {} in {}", args.join(" "), cwd.display())),
        }
    }

    fn expect_repo(&self, cwd: &Path) -> Result<(), String> {
        match &self.repo_root {
            Some(repo_root) if cwd == repo_root => Ok(()),
            Some(repo_root) => Err(format!("expected repo root {}, got {}", repo_root.display(), cwd.display())),
            None => Err("repo root not configured".into()),
        }
    }

    fn expect_checkout(&self, cwd: &Path) -> Result<(), String> {
        match &self.checkout_path {
            Some(checkout_path) if cwd == checkout_path => Ok(()),
            Some(checkout_path) => Err(format!("expected checkout path {}, got {}", checkout_path.display(), cwd.display())),
            None => Err("checkout path not configured".into()),
        }
    }
}

#[async_trait]
impl CommandRunner for ScriptedStatusRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        self.response(cmd, args, cwd)
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        match self.response(cmd, args, cwd) {
            Ok(stdout) => Ok(CommandOutput { stdout, stderr: String::new(), success: true }),
            Err(stderr) => Ok(CommandOutput { stdout: String::new(), stderr, success: false }),
        }
    }

    async fn exists(&self, cmd: &str, _args: &[&str]) -> bool {
        matches!(cmd, "git" | "gh")
    }
}

struct SimpleCheckoutManager {
    state: Arc<RwLock<FakeVcsState>>,
}

impl SimpleCheckoutManager {
    fn new(state: Arc<RwLock<FakeVcsState>>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl CheckoutManager for SimpleCheckoutManager {
    async fn validate_target(
        &self,
        _repo_root: &flotilla_core::path_context::ExecutionEnvironmentPath,
        _branch: &str,
        _intent: flotilla_protocol::CheckoutIntent,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn list_checkouts(
        &self,
        _repo_root: &flotilla_core::path_context::ExecutionEnvironmentPath,
    ) -> Result<Vec<(flotilla_core::path_context::ExecutionEnvironmentPath, Checkout)>, String> {
        Ok(self
            .state
            .read()
            .expect("simple checkout manager state")
            .checkouts
            .iter()
            .map(|(path, checkout)| (flotilla_core::path_context::ExecutionEnvironmentPath::new(path), checkout.clone()))
            .collect())
    }

    async fn create_checkout(
        &self,
        _repo_root: &flotilla_core::path_context::ExecutionEnvironmentPath,
        branch: &str,
        _create_branch: bool,
    ) -> Result<(flotilla_core::path_context::ExecutionEnvironmentPath, Checkout), String> {
        Err(format!("create_checkout not implemented for test branch {branch}"))
    }

    async fn remove_checkout(
        &self,
        _repo_root: &flotilla_core::path_context::ExecutionEnvironmentPath,
        branch: &str,
    ) -> Result<(), String> {
        let mut state = self.state.write().expect("simple checkout manager state");
        state.checkouts.retain(|(_, checkout)| checkout.branch != branch);
        Ok(())
    }
}

pub struct SteppedCheckoutManager {
    state: Arc<RwLock<FakeVcsState>>,
    remove_started: Arc<AtomicBool>,
    remove_started_notify: Arc<tokio::sync::Notify>,
    remove_released: Arc<AtomicBool>,
    remove_released_notify: Arc<tokio::sync::Notify>,
}

impl SteppedCheckoutManager {
    fn new(state: Arc<RwLock<FakeVcsState>>) -> Self {
        Self {
            state,
            remove_started: Arc::new(AtomicBool::new(false)),
            remove_started_notify: Arc::new(tokio::sync::Notify::new()),
            remove_released: Arc::new(AtomicBool::new(false)),
            remove_released_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn has_started(&self) -> bool {
        self.remove_started.load(Ordering::SeqCst)
    }

    fn release_remove(&self) {
        self.remove_released.store(true, Ordering::SeqCst);
        self.remove_released_notify.notify_one();
    }
}

#[async_trait]
impl CheckoutManager for SteppedCheckoutManager {
    async fn validate_target(
        &self,
        _repo_root: &flotilla_core::path_context::ExecutionEnvironmentPath,
        _branch: &str,
        _intent: flotilla_protocol::CheckoutIntent,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn list_checkouts(
        &self,
        _repo_root: &flotilla_core::path_context::ExecutionEnvironmentPath,
    ) -> Result<Vec<(flotilla_core::path_context::ExecutionEnvironmentPath, Checkout)>, String> {
        Ok(self
            .state
            .read()
            .expect("stepped checkout manager state")
            .checkouts
            .iter()
            .map(|(path, checkout)| (flotilla_core::path_context::ExecutionEnvironmentPath::new(path), checkout.clone()))
            .collect())
    }

    async fn create_checkout(
        &self,
        _repo_root: &flotilla_core::path_context::ExecutionEnvironmentPath,
        branch: &str,
        _create_branch: bool,
    ) -> Result<(flotilla_core::path_context::ExecutionEnvironmentPath, Checkout), String> {
        Err(format!("create_checkout not implemented for test branch {branch}"))
    }

    async fn remove_checkout(
        &self,
        _repo_root: &flotilla_core::path_context::ExecutionEnvironmentPath,
        branch: &str,
    ) -> Result<(), String> {
        self.remove_started.store(true, Ordering::SeqCst);
        self.remove_started_notify.notify_waiters();

        while !self.remove_released.load(Ordering::SeqCst) {
            self.remove_released_notify.notified().await;
        }

        let mut state = self.state.write().expect("stepped checkout manager state");
        state.checkouts.retain(|(_, checkout)| checkout.branch != branch);
        Ok(())
    }
}
