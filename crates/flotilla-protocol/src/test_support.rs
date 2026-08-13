//! Shared test builders for protocol types.
//!
//! Available when `cfg(test)` or the `test-support` feature is enabled.
//! All builders produce minimal structs with empty/default fields — callers
//! opt in to correlation keys and other detail via fluent methods.

use std::path::PathBuf;

use crate::{
    provider_data::{
        ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession, Issue, IssueRef, IssueSource, IssueState, SessionStatus,
    },
    qualified_path::QualifiedPath,
    HostName, HostPath,
};

/// Build a `HostPath` with a deterministic `"test-host"` hostname.
pub fn hp(path: &str) -> HostPath {
    HostPath::new(HostName::new("test-host"), PathBuf::from(path))
}

/// Build a hostname-qualified `QualifiedPath` with a deterministic `"test-host"` hostname.
pub fn qp(path: &str) -> QualifiedPath {
    QualifiedPath::from_host_name(&HostName::new("test-host"), PathBuf::from(path))
}

// ---------------------------------------------------------------------------
// TestCheckout
// ---------------------------------------------------------------------------

pub struct TestCheckout {
    branch: String,
    is_main: bool,
}

impl TestCheckout {
    pub fn new(branch: &str) -> Self {
        Self { branch: branch.to_string(), is_main: false }
    }

    pub fn at(self, _path: &str) -> Self {
        self
    }

    pub fn is_main(mut self, val: bool) -> Self {
        self.is_main = val;
        self
    }

    pub fn with_branch_key(self) -> Self {
        self
    }

    pub fn build(self) -> Checkout {
        Checkout {
            branch: self.branch,
            is_main: self.is_main,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            host_name: None,
            environment_id: None,
        }
    }
}

// ---------------------------------------------------------------------------
// TestChangeRequest
// ---------------------------------------------------------------------------

pub struct TestChangeRequest {
    title: String,
    branch: String,
}

impl TestChangeRequest {
    pub fn new(title: &str, branch: &str) -> Self {
        Self { title: title.to_string(), branch: branch.to_string() }
    }

    pub fn with_branch_key(self) -> Self {
        self
    }

    pub fn build(self) -> ChangeRequest {
        ChangeRequest {
            title: self.title,
            branch: self.branch,
            status: ChangeRequestStatus::Open,
            body: None,
            provider_name: String::new(),
            provider_display_name: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// TestSession
// ---------------------------------------------------------------------------

pub struct TestSession {
    title: String,
    status: SessionStatus,
    provider_name: String,
}

impl TestSession {
    pub fn new(title: &str) -> Self {
        Self { title: title.to_string(), status: SessionStatus::Running, provider_name: String::new() }
    }

    pub fn with_status(mut self, status: SessionStatus) -> Self {
        self.status = status;
        self
    }

    pub fn with_session_ref(mut self, provider: &str, _id: &str) -> Self {
        self.provider_name = provider.to_string();
        self
    }

    pub fn with_branch_key(self, _branch: &str) -> Self {
        self
    }

    pub fn build(self) -> CloudAgentSession {
        CloudAgentSession {
            title: self.title,
            status: self.status,
            model: None,
            updated_at: None,
            provider_name: self.provider_name,
            provider_display_name: String::new(),
            item_noun: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// TestIssue
// ---------------------------------------------------------------------------

pub struct TestIssue {
    id: String,
    title: String,
    labels: Vec<String>,
}

impl TestIssue {
    pub fn new(title: &str) -> Self {
        Self { id: "test".into(), title: title.to_string(), labels: Vec::new() }
    }

    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn with_labels(mut self, labels: Vec<String>) -> Self {
        self.labels = labels;
        self
    }

    pub fn build(self) -> Issue {
        Issue {
            reference: IssueRef { source: IssueSource { service: "test".into(), scope: "owner/repo".into() }, id: self.id },
            title: self.title,
            body: None,
            state: IssueState::Open,
            labels: self.labels,
            as_of: "2026-07-15T09:30:00Z".parse().expect("valid test issue timestamp"),
            observed_at: None,
            provider_name: String::new(),
            provider_display_name: String::new(),
        }
    }
}
