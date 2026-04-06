//! Shared test builders for protocol types.
//!
//! Available when `cfg(test)` or the `test-support` feature is enabled.
//! All builders produce minimal structs with empty/default fields — callers
//! opt in to correlation keys and other detail via fluent methods.

use std::path::PathBuf;

use crate::{
    provider_data::{ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession, CorrelationKey, Issue, SessionStatus},
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
    correlation_keys: Vec<CorrelationKey>,
}

impl TestCheckout {
    pub fn new(branch: &str) -> Self {
        Self { branch: branch.to_string(), is_main: false, correlation_keys: Vec::new() }
    }

    /// Set the checkout path. Adds a `CorrelationKey::CheckoutPath`.
    pub fn at(mut self, path: &str) -> Self {
        self.correlation_keys.push(CorrelationKey::CheckoutPath(qp(path)));
        self
    }

    pub fn is_main(mut self, val: bool) -> Self {
        self.is_main = val;
        self
    }

    /// Add a `CorrelationKey::Branch` for this checkout's branch name.
    pub fn with_branch_key(mut self) -> Self {
        self.correlation_keys.push(CorrelationKey::Branch(self.branch.clone()));
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
            correlation_keys: self.correlation_keys,
            association_keys: vec![],
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
    correlation_keys: Vec<CorrelationKey>,
}

impl TestChangeRequest {
    pub fn new(title: &str, branch: &str) -> Self {
        Self { title: title.to_string(), branch: branch.to_string(), correlation_keys: Vec::new() }
    }

    /// Add a `CorrelationKey::Branch` for this CR's branch name.
    pub fn with_branch_key(mut self) -> Self {
        self.correlation_keys.push(CorrelationKey::Branch(self.branch.clone()));
        self
    }

    pub fn build(self) -> ChangeRequest {
        ChangeRequest {
            title: self.title,
            branch: self.branch,
            status: ChangeRequestStatus::Open,
            body: None,
            correlation_keys: self.correlation_keys,
            association_keys: vec![],
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
    correlation_keys: Vec<CorrelationKey>,
}

impl TestSession {
    pub fn new(title: &str) -> Self {
        Self { title: title.to_string(), status: SessionStatus::Running, correlation_keys: Vec::new() }
    }

    pub fn with_status(mut self, status: SessionStatus) -> Self {
        self.status = status;
        self
    }

    /// Add a `CorrelationKey::SessionRef`.
    pub fn with_session_ref(mut self, provider: &str, id: &str) -> Self {
        self.correlation_keys.push(CorrelationKey::SessionRef(provider.to_string(), id.to_string()));
        self
    }

    /// Add a `CorrelationKey::Branch`.
    pub fn with_branch_key(mut self, branch: &str) -> Self {
        self.correlation_keys.push(CorrelationKey::Branch(branch.to_string()));
        self
    }

    pub fn build(self) -> CloudAgentSession {
        CloudAgentSession {
            title: self.title,
            status: self.status,
            model: None,
            updated_at: None,
            correlation_keys: self.correlation_keys,
            provider_name: String::new(),
            provider_display_name: String::new(),
            item_noun: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// TestIssue
// ---------------------------------------------------------------------------

pub struct TestIssue {
    title: String,
    labels: Vec<String>,
}

impl TestIssue {
    pub fn new(title: &str) -> Self {
        Self { title: title.to_string(), labels: Vec::new() }
    }

    pub fn with_labels(mut self, labels: Vec<String>) -> Self {
        self.labels = labels;
        self
    }

    pub fn build(self) -> Issue {
        Issue {
            title: self.title,
            labels: self.labels,
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        }
    }
}
