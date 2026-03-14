//! Conversion functions from core types to protocol types.
//!
//! This module is the serialization boundary between the rich in-process
//! core types and the flat, serde-friendly protocol types.

use std::{collections::HashMap, path::Path};

use flotilla_protocol::{CheckoutRef, DiscoveryEntry, HostName, ProviderError, RepoIdentity, Snapshot, WorkItem};

use crate::{
    data::{CorrelationResult, RefreshError},
    providers::{
        correlation::{CorrelatedGroup, ItemKind as CorItemKind},
        discovery::EnvironmentAssertion,
    },
    refresh::RefreshSnapshot,
};

pub fn assertion_to_discovery_entry(assertion: &EnvironmentAssertion) -> DiscoveryEntry {
    let mut detail = HashMap::new();
    let kind = match assertion {
        EnvironmentAssertion::BinaryAvailable { name, path, version } => {
            detail.insert("name".into(), name.clone());
            detail.insert("path".into(), path.display().to_string());
            if let Some(v) = version {
                detail.insert("version".into(), v.clone());
            }
            "binary_available"
        }
        EnvironmentAssertion::EnvVarSet { key, .. } => {
            detail.insert("key".into(), key.clone());
            detail.insert("value".into(), "<set>".into());
            "env_var_set"
        }
        EnvironmentAssertion::VcsCheckoutDetected { root, kind, is_main_checkout } => {
            detail.insert("root".into(), root.display().to_string());
            detail.insert("kind".into(), format!("{kind:?}"));
            detail.insert("is_main_checkout".into(), is_main_checkout.to_string());
            "vcs_checkout_detected"
        }
        EnvironmentAssertion::RemoteHost { platform, owner, repo, remote_name } => {
            detail.insert("platform".into(), format!("{platform:?}"));
            detail.insert("owner".into(), owner.clone());
            detail.insert("repo".into(), repo.clone());
            detail.insert("remote_name".into(), remote_name.clone());
            "remote_host"
        }
        EnvironmentAssertion::AuthFileExists { provider, path } => {
            detail.insert("provider".into(), provider.clone());
            detail.insert("path".into(), path.display().to_string());
            "auth_file_exists"
        }
        EnvironmentAssertion::SocketAvailable { name, path } => {
            detail.insert("name".into(), name.clone());
            detail.insert("path".into(), path.display().to_string());
            "socket_available"
        }
    };
    DiscoveryEntry { kind: kind.into(), detail }
}

pub fn correlation_result_to_work_item(item: &CorrelationResult, groups: &[CorrelatedGroup], host_name: &HostName) -> WorkItem {
    let kind = item.kind();
    let identity = item.identity();
    let host = item.host(host_name);

    let checkout = item.checkout().map(|co| CheckoutRef { key: co.key.clone(), is_main_checkout: co.is_main_checkout });

    let debug_group = item.correlation_group_idx().and_then(|idx| groups.get(idx)).map(format_debug_group).unwrap_or_default();

    WorkItem {
        kind,
        identity,
        host,
        branch: item.branch().map(|s| s.to_string()),
        description: item.description().to_string(),
        checkout,
        change_request_key: item.change_request_key().map(|s| s.to_string()),
        session_key: item.session_key().map(|s| s.to_string()),
        issue_keys: item.issue_keys().to_vec(),
        workspace_refs: item.workspace_refs().to_vec(),
        is_main_checkout: item.is_main_checkout(),
        debug_group,
        source: item.source().map(|s| s.to_string()),
        terminal_keys: item.terminal_ids().to_vec(),
    }
}

fn format_debug_group(group: &CorrelatedGroup) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("{} correlated items", group.items.len()));
    for ci in &group.items {
        let kind_label = match ci.kind {
            CorItemKind::Checkout => "Checkout",
            CorItemKind::ChangeRequest => "CR",
            CorItemKind::CloudSession => "Session",
            CorItemKind::Workspace => "Workspace",
            CorItemKind::ManagedTerminal => "Terminal",
        };
        lines.push(format!("  {}: {} [{:?}]", kind_label, ci.title, ci.source_key));
        for key in &ci.correlation_keys {
            lines.push(format!("    {key:?}"));
        }
    }
    lines
}

pub fn error_to_proto(error: &RefreshError) -> ProviderError {
    ProviderError { category: error.category.to_string(), provider: error.provider.clone(), message: error.message.clone() }
}

pub fn health_to_proto(health: &HashMap<(&'static str, String), bool>) -> HashMap<String, HashMap<String, bool>> {
    let mut nested: HashMap<String, HashMap<String, bool>> = HashMap::new();
    for ((category, provider), &healthy) in health {
        nested.entry(category.to_string()).or_default().insert(provider.clone(), healthy);
    }
    nested
}

pub fn snapshot_to_proto(repo_identity: RepoIdentity, repo: &Path, seq: u64, refresh: &RefreshSnapshot, host_name: &HostName) -> Snapshot {
    Snapshot {
        seq,
        repo_identity,
        repo: repo.to_path_buf(),
        host_name: host_name.clone(),
        work_items: refresh
            .work_items
            .iter()
            .map(|item| correlation_result_to_work_item(item, &refresh.correlation_groups, host_name))
            .collect(),
        providers: (*refresh.providers).clone(),
        provider_health: health_to_proto(&refresh.provider_health),
        errors: refresh.errors.iter().map(error_to_proto).collect(),
        issue_total: None,
        issue_has_more: false,
        issue_search_results: None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use flotilla_protocol::{HostName, HostPath, WorkItemIdentity, WorkItemKind};

    use super::*;
    use crate::{
        data::{CorrelatedAnchor, CorrelatedWorkItem, StandaloneResult},
        providers::discovery::EnvironmentAssertion,
    };

    #[test]
    fn convert_binary_available() {
        let assertion =
            EnvironmentAssertion::BinaryAvailable { name: "git".into(), path: PathBuf::from("/usr/bin/git"), version: Some("2.40".into()) };
        let entry = assertion_to_discovery_entry(&assertion);
        assert_eq!(entry.kind, "binary_available");
        assert_eq!(entry.detail["name"], "git");
        assert_eq!(entry.detail["path"], "/usr/bin/git");
        assert_eq!(entry.detail["version"], "2.40");
    }

    #[test]
    fn convert_auth_file_exists() {
        let assertion =
            EnvironmentAssertion::AuthFileExists { provider: "github".into(), path: PathBuf::from("/home/.config/gh/hosts.yml") };
        let entry = assertion_to_discovery_entry(&assertion);
        assert_eq!(entry.kind, "auth_file_exists");
        assert_eq!(entry.detail["provider"], "github");
    }

    #[test]
    fn convert_socket_available() {
        let assertion = EnvironmentAssertion::SocketAvailable { name: "shpool".into(), path: PathBuf::from("/tmp/shpool.sock") };
        let entry = assertion_to_discovery_entry(&assertion);
        assert_eq!(entry.kind, "socket_available");
        assert_eq!(entry.detail["name"], "shpool");
    }

    fn hp(path: &str) -> HostPath {
        HostPath::new(HostName::new("test-host"), PathBuf::from(path))
    }

    fn test_host() -> HostName {
        HostName::new("test-host")
    }

    #[test]
    fn convert_correlated_checkout() {
        let item = CorrelationResult::Correlated(CorrelatedWorkItem {
            anchor: CorrelatedAnchor::Checkout(CheckoutRef { key: hp("/repos/my-project/wt-1"), is_main_checkout: false }),
            branch: Some("feature-login".to_string()),
            description: "Implement login flow".to_string(),
            linked_change_request: Some("PR#55".to_string()),
            linked_session: Some("sess-abc".to_string()),
            linked_issues: vec!["GH-10".to_string(), "LIN-20".to_string()],
            workspace_refs: vec!["cmux-1".to_string()],
            correlation_group_idx: 0,
            source: None,
            terminal_ids: vec![],
        });

        let proto = correlation_result_to_work_item(&item, &[], &test_host());

        assert_eq!(proto.kind, WorkItemKind::Checkout);
        assert_eq!(proto.identity, WorkItemIdentity::Checkout(hp("/repos/my-project/wt-1")));
        // Checkout-anchored items derive host from HostPath
        assert_eq!(proto.host, test_host());
        assert_eq!(proto.branch.as_deref(), Some("feature-login"));
        assert_eq!(proto.description, "Implement login flow");

        let checkout = proto.checkout.expect("should have checkout ref");
        assert_eq!(checkout.key, hp("/repos/my-project/wt-1"));
        assert!(!checkout.is_main_checkout);

        assert_eq!(proto.change_request_key.as_deref(), Some("PR#55"));
        assert_eq!(proto.session_key.as_deref(), Some("sess-abc"));
        assert_eq!(proto.issue_keys, vec!["GH-10", "LIN-20"]);
        assert_eq!(proto.workspace_refs, vec!["cmux-1"]);
        assert!(!proto.is_main_checkout);
    }

    #[test]
    fn convert_standalone_issue() {
        let item = CorrelationResult::Standalone(StandaloneResult::Issue {
            key: "42".to_string(),
            description: "Fix the login bug".to_string(),
            source: String::new(),
        });

        let proto = correlation_result_to_work_item(&item, &[], &test_host());

        assert_eq!(proto.kind, WorkItemKind::Issue);
        assert_eq!(proto.identity, WorkItemIdentity::Issue("42".to_string()));
        assert_eq!(proto.host, test_host());
        assert_eq!(proto.description, "Fix the login bug");
        assert_eq!(proto.issue_keys, vec!["42"]);
        assert!(proto.branch.is_none());
        assert!(proto.checkout.is_none());
        assert!(proto.change_request_key.is_none());
        assert!(proto.session_key.is_none());
        assert!(proto.workspace_refs.is_empty());
        assert!(!proto.is_main_checkout);
    }

    #[test]
    fn convert_correlated_checkout_has_hostname_source() {
        let hostname = gethostname::gethostname().to_string_lossy().into_owned();
        let item = CorrelationResult::Correlated(CorrelatedWorkItem {
            anchor: CorrelatedAnchor::Checkout(CheckoutRef { key: hp("/repos/proj/wt"), is_main_checkout: false }),
            branch: Some("feat".to_string()),
            description: "Feature".to_string(),
            linked_change_request: None,
            linked_session: None,
            linked_issues: vec![],
            workspace_refs: vec![],
            correlation_group_idx: 0,
            source: Some(hostname.clone()),
            terminal_ids: vec![],
        });
        let proto = correlation_result_to_work_item(&item, &[], &test_host());
        assert_eq!(proto.source, Some(hostname));
    }

    #[test]
    fn convert_correlated_session_has_provider_source() {
        let item = CorrelationResult::Correlated(CorrelatedWorkItem {
            anchor: CorrelatedAnchor::Session("sess-1".to_string()),
            branch: None,
            description: "My session".to_string(),
            linked_change_request: None,
            linked_session: None,
            linked_issues: vec![],
            workspace_refs: vec![],
            correlation_group_idx: 0,
            source: Some("Claude".to_string()),
            terminal_ids: vec![],
        });
        let proto = correlation_result_to_work_item(&item, &[], &test_host());
        assert_eq!(proto.source, Some("Claude".to_string()));
        // Session-anchored items use the provided local host name
        assert_eq!(proto.host, test_host());
    }

    #[test]
    fn convert_standalone_issue_has_provider_source() {
        let item = CorrelationResult::Standalone(StandaloneResult::Issue {
            key: "42".to_string(),
            description: "Fix the bug".to_string(),
            source: "GitHub".to_string(),
        });
        let proto = correlation_result_to_work_item(&item, &[], &test_host());
        assert_eq!(proto.source, Some("GitHub".to_string()));
    }

    #[test]
    fn convert_standalone_remote_branch_has_git_source() {
        let item = CorrelationResult::Standalone(StandaloneResult::RemoteBranch { branch: "origin/feat".to_string() });
        let proto = correlation_result_to_work_item(&item, &[], &test_host());
        assert_eq!(proto.source, Some("git".to_string()));
    }

    #[test]
    fn convert_checkout_host_from_host_path() {
        let remote_host = HostName::new("remote-server");
        let item = CorrelationResult::Correlated(CorrelatedWorkItem {
            anchor: CorrelatedAnchor::Checkout(CheckoutRef {
                key: HostPath::new(remote_host.clone(), PathBuf::from("/repos/proj")),
                is_main_checkout: false,
            }),
            branch: Some("feat".to_string()),
            description: "Feature".to_string(),
            linked_change_request: None,
            linked_session: None,
            linked_issues: vec![],
            workspace_refs: vec![],
            terminal_ids: vec![],
            correlation_group_idx: 0,
            source: None,
        });
        let local = HostName::new("local-machine");
        let proto = correlation_result_to_work_item(&item, &[], &local);
        // Should use the checkout's HostPath host, not the local fallback
        assert_eq!(proto.host, remote_host);
    }
}
