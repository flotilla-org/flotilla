use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{status_patch::NoStatusPatch, ApiPaths, InputMeta, ReplicationClass, Resource, ResourceError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Forge;

impl Resource for Forge {
    type Spec = ForgeSpec;
    type Status = ();
    type StatusPatch = NoStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "forges", kind: "Forge" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Definitions;

    fn validate_spec(meta: &InputMeta, spec: &Self::Spec) -> Result<(), ResourceError> {
        if meta.name != spec.forge_id || spec.forge_id.trim().is_empty() {
            return Err(ResourceError::invalid("Forge resource name must equal a non-empty forge_id"));
        }
        if !spec.forge_id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            || !spec.forge_id.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric)
            || !spec.forge_id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(ResourceError::invalid("Forge forge_id must be a lowercase DNS label"));
        }
        if spec.hosts.is_empty() || spec.hosts.iter().any(|host| host.trim().is_empty() || host.contains('/')) {
            return Err(ResourceError::invalid("Forge hosts must be non-empty hostnames or SSH aliases"));
        }
        let Some(front) = spec.https_url.strip_prefix("https://") else {
            return Err(ResourceError::invalid("Forge https_url must use HTTPS"));
        };
        if front.split('/').next().is_none_or(str::is_empty) || front.contains(['?', '#']) {
            return Err(ResourceError::invalid("Forge https_url must name a host and optional path prefix"));
        }
        if spec.git_ssh_host.trim().is_empty() || spec.git_ssh_host.contains('/') || spec.git_ssh_host.contains(char::is_whitespace) {
            return Err(ResourceError::invalid("Forge git_ssh_host must be a hostname or SSH alias"));
        }
        Ok(())
    }
}

/// A fleet-wide identity and transport declaration for one forge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ForgeSpec {
    pub forge_id: String,
    pub kind: ForgeKind,
    /// DNS names and SSH configuration aliases that identify this forge.
    pub hosts: BTreeSet<String>,
    pub https_url: String,
    pub git_ssh_host: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeKind {
    Github,
    Forgejo,
}

impl ForgeSpec {
    /// Whether an issue service URL names this installation, including a
    /// declared host alias and the installation's path prefix.
    pub fn owns_issue_service(&self, service_url: &str) -> bool {
        let Some(front) = service_url.strip_prefix("https://") else { return false };
        let (host, path) = front.split_once('/').unwrap_or((front, ""));
        let forge_path = self
            .https_url
            .strip_prefix("https://")
            .and_then(|front| front.split_once('/'))
            .map_or("", |(_, path)| path)
            .trim_end_matches('/');
        self.matches_host(host) && path.trim_end_matches('/') == forge_path
    }

    pub fn matches_host(&self, host: &str) -> bool {
        self.hosts.iter().any(|alias| alias.eq_ignore_ascii_case(host))
            || self
                .https_url
                .split_once("://")
                .and_then(|(_, rest)| rest.split('/').next())
                .is_some_and(|front| front.eq_ignore_ascii_case(host))
            || self.git_ssh_host.eq_ignore_ascii_case(host)
    }

    pub fn repository_path(&self, remote: &str) -> Result<Option<(String, String)>, String> {
        let canonical = crate::canonicalize_repo_url(remote)?;
        let (_, rest) = canonical.split_once("://").expect("canonical URL has scheme");
        let (host, path) = rest.split_once('/').expect("canonical URL has path");
        if !self.matches_host(host) {
            return Ok(None);
        }
        let prefix =
            self.https_url.strip_prefix("https://").and_then(|front| front.split_once('/')).map(|(_, prefix)| prefix.trim_matches('/'));
        let path = match prefix.filter(|prefix| !prefix.is_empty()) {
            Some(prefix) => match path.strip_prefix(prefix).and_then(|rest| rest.strip_prefix('/')) {
                Some(path) => path,
                None => return Ok(None),
            },
            None => path,
        };
        let (owner, repo_name) = path.rsplit_once('/').ok_or_else(|| format!("forge repository URL requires owner and name: {remote}"))?;
        if owner.is_empty() || repo_name.is_empty() {
            return Err(format!("forge repository URL requires owner and name: {remote}"));
        }
        Ok(Some((owner.to_string(), repo_name.to_string())))
    }
}
