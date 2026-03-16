use std::{collections::HashMap, path::PathBuf, sync::Arc};

use flotilla_core::data::{group_work_items, SectionLabels};
use flotilla_protocol::{
    CategoryLabels, ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession, CorrelationKey, Issue, ProviderData, RepoIdentity,
    RepoLabels, SessionStatus, WorkItem,
};
// Re-export shared WorkItem/RepoInfo builders — single source of truth in test_builders.
pub use flotilla_tui::app::test_builders::{checkout_item, issue_item, pr_item, repo_info, session_item};
use flotilla_tui::{
    app::{InFlightCommand, ProviderStatus, RepoViewLayout, TuiModel, UiMode, UiState},
    keymap::Keymap,
    theme::Theme,
    ui,
};
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 30;

fn repo_identity(name: &str) -> RepoIdentity {
    RepoIdentity { authority: "local".into(), path: format!("/test/{name}") }
}

pub struct TestHarness {
    pub model: TuiModel,
    pub ui: UiState,
    pub in_flight: HashMap<u64, InFlightCommand>,
    theme: Option<Theme>,
    width: u16,
    height: u16,
}

impl TestHarness {
    /// Empty state: single repo with no data (the UI requires at least one repo).
    pub fn empty() -> Self {
        let info = test_repo_info("empty");
        let model = TuiModel::from_repo_info(vec![info]);
        let ui = UiState::new(&model.repo_order);
        Self { model, ui, in_flight: HashMap::new(), theme: None, width: WIDTH, height: HEIGHT }
    }

    /// Single repo with given name, empty data.
    pub fn single_repo(name: &str) -> Self {
        let info = test_repo_info(name);
        let model = TuiModel::from_repo_info(vec![info]);
        let ui = UiState::new(&model.repo_order);
        Self { model, ui, in_flight: HashMap::new(), theme: None, width: WIDTH, height: HEIGHT }
    }

    /// Multiple repos by name, all with empty data.
    pub fn multi_repo(names: &[&str]) -> Self {
        let infos = names.iter().map(|n| test_repo_info(n)).collect();
        let model = TuiModel::from_repo_info(infos);
        let ui = UiState::new(&model.repo_order);
        Self { model, ui, in_flight: HashMap::new(), theme: None, width: WIDTH, height: HEIGHT }
    }

    /// Override the terminal height for this test.
    pub fn with_height(mut self, height: u16) -> Self {
        self.height = height;
        self
    }

    /// Override the terminal width for this test.
    pub fn with_width(mut self, width: u16) -> Self {
        self.width = width;
        self
    }

    /// Set the UI mode.
    pub fn with_mode(mut self, mode: UiMode) -> Self {
        self.ui.mode = mode;
        self
    }

    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = Some(theme);
        self
    }

    pub fn with_layout(mut self, layout: RepoViewLayout) -> Self {
        self.ui.view_layout = layout;
        self
    }

    /// Set a status message on the model.
    pub fn with_status_message(mut self, msg: &str) -> Self {
        self.model.status_message = Some(msg.to_string());
        self
    }

    /// Set provider names for a repo so the config screen can look up statuses.
    pub fn with_provider_names(mut self, repo: &str, names: Vec<(&str, &str)>) -> Self {
        let repo_identity = repo_identity(repo);
        let rm = self.model.repos.get_mut(&repo_identity).unwrap();
        for (category, name) in names {
            rm.provider_names.entry(category.to_string()).or_default().push(name.to_string());
        }
        self
    }

    /// Set a provider status for a repo.
    pub fn with_provider_status(mut self, repo: &str, category: &str, provider: &str, status: ProviderStatus) -> Self {
        let repo_identity = repo_identity(repo);
        self.model.provider_statuses.insert((repo_identity, category.to_string(), provider.to_string()), status);
        self
    }

    /// Set provider data and work items for the active (first) repo.
    pub fn with_provider_data(mut self, providers: ProviderData, items: Vec<WorkItem>) -> Self {
        let repo_identity = self.model.repo_order[0].clone();
        let rm = self.model.repos.get_mut(&repo_identity).unwrap();
        rm.providers = Arc::new(providers);

        let section_labels = SectionLabels {
            checkouts: rm.labels.checkouts.section.clone(),
            change_requests: rm.labels.change_requests.section.clone(),
            issues: rm.labels.issues.section.clone(),
            sessions: rm.labels.cloud_agents.section.clone(),
        };
        let table_view = group_work_items(&items, &rm.providers, &section_labels, &rm.path);

        let rui = self.ui.repo_ui.get_mut(&repo_identity).unwrap();
        rui.update_table_view(table_view);
        self
    }

    /// Render the UI into a string via TestBackend.
    pub fn render_to_string(&mut self) -> String {
        let buffer = self.render_to_buffer();
        buffer_to_string(&buffer)
    }

    /// Render the UI into a test buffer for symbol/style assertions.
    pub fn render_to_buffer(&mut self) -> Buffer {
        let backend = TestBackend::new(self.width, self.height);
        let mut terminal = Terminal::new(backend).expect("failed to create test terminal");
        let theme = self.theme.clone().unwrap_or_else(Theme::classic);
        let keymap = Keymap::defaults();
        terminal
            .draw(|frame| {
                ui::render(&self.model, &mut self.ui, &self.in_flight, &theme, &keymap, frame);
            })
            .expect("failed to draw test frame");
        terminal.backend().buffer().clone()
    }
}

fn test_repo_info(name: &str) -> flotilla_protocol::RepoInfo {
    repo_info(format!("/test/{name}"), name, test_labels())
}

fn test_labels() -> RepoLabels {
    RepoLabels {
        checkouts: CategoryLabels { section: "Checkouts".into(), noun: "worktree".into(), abbr: "WT".into() },
        change_requests: CategoryLabels { section: "Pull Requests".into(), noun: "PR".into(), abbr: "PR".into() },
        issues: CategoryLabels { section: "Issues".into(), noun: "issue".into(), abbr: "IS".into() },
        cloud_agents: CategoryLabels { section: "Sessions".into(), noun: "session".into(), abbr: "SS".into() },
    }
}

fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area;
    let mut lines = Vec::new();
    for y in area.y..area.y + area.height {
        let mut line = String::new();
        for x in area.x..area.x + area.width {
            let cell = &buffer[(x, y)];
            line.push_str(cell.symbol());
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

// ── Provider data builders (unique to snapshot tests) ───────────────────

pub fn make_checkout(branch: &str, path: &str, is_main: bool) -> (flotilla_protocol::HostPath, Checkout) {
    let key = flotilla_protocol::HostPath::new(flotilla_protocol::HostName::local(), PathBuf::from(path));
    let checkout = Checkout {
        branch: branch.to_string(),
        is_main,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![CorrelationKey::Branch(branch.to_string()), CorrelationKey::CheckoutPath(key.clone())],
        association_keys: vec![],
    };
    (key, checkout)
}

pub fn make_change_request(id: &str, title: &str, branch: &str) -> (String, ChangeRequest) {
    let cr = ChangeRequest {
        title: title.to_string(),
        branch: branch.to_string(),
        status: ChangeRequestStatus::Open,
        body: None,
        correlation_keys: vec![CorrelationKey::Branch(branch.to_string())],
        association_keys: vec![],
        provider_name: String::new(),
        provider_display_name: String::new(),
    };
    (id.to_string(), cr)
}

pub fn make_issue(id: &str, title: &str) -> (String, Issue) {
    let issue = Issue {
        title: title.to_string(),
        labels: vec![],
        association_keys: vec![],
        provider_name: String::new(),
        provider_display_name: String::new(),
    };
    (id.to_string(), issue)
}

pub fn make_session(id: &str, title: &str, status: SessionStatus) -> (String, CloudAgentSession) {
    let session = CloudAgentSession {
        title: title.to_string(),
        status,
        model: None,
        updated_at: None,
        correlation_keys: vec![],
        provider_name: String::new(),
        provider_display_name: String::new(),
        item_noun: String::new(),
    };
    (id.to_string(), session)
}

// ── WorkItem builders (thin wrappers over test_support where possible) ──

/// Checkout work item. Delegates to test_support::checkout_item.
pub fn make_work_item_checkout(branch: &str, path: &str) -> WorkItem {
    checkout_item(branch, path, false)
}

/// Change request work item with custom title and optional branch.
pub fn make_work_item_cr(id: &str, title: &str, branch: Option<&str>) -> WorkItem {
    let mut item = pr_item(id);
    item.description = title.to_string();
    item.branch = branch.map(|b| b.to_string());
    item
}

/// Issue work item with custom title and issue_keys set (test_support::issue_item omits keys).
pub fn make_work_item_issue(id: &str, title: &str) -> WorkItem {
    let mut item = issue_item(id);
    item.description = title.to_string();
    item.issue_keys = vec![id.to_string()];
    item
}
