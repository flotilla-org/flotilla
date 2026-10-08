use std::collections::HashMap;

use flotilla_protocol::{provider_data::Issue, CommandValue, IssueSource};
use tracing::{info, warn};

use crate::{
    provider_data::ProviderData,
    providers::{registry::ProviderRegistry, types::CloudAgentSession},
};

pub(super) struct ReadOnlySessionActionService<'a> {
    issue_source: IssueSource,
    registry: &'a ProviderRegistry,
    providers_data: &'a ProviderData,
}

impl<'a> ReadOnlySessionActionService<'a> {
    pub(super) fn new(issue_source: IssueSource, registry: &'a ProviderRegistry, providers_data: &'a ProviderData) -> Self {
        Self { issue_source, registry, providers_data }
    }

    pub(super) async fn archive_session_result(&self, session_id: &str) -> CommandValue {
        if let Some(session) = self.providers_data.sessions.get(session_id) {
            info!(%session_id, "archiving session");
            if let Some(key) = session_provider_key(session, session_id) {
                if let Some((_, coding_agent)) = self.registry.cloud_agents.get(key) {
                    match coding_agent.archive_session(session_id).await {
                        Ok(()) => CommandValue::Ok,
                        Err(err) => CommandValue::Error { message: err },
                    }
                } else {
                    CommandValue::Error { message: format!("No coding agent provider: {key}") }
                }
            } else {
                CommandValue::Error { message: format!("Cannot determine provider for session {session_id}") }
            }
        } else {
            CommandValue::Error { message: format!("session not found: {session_id}") }
        }
    }

    pub(super) async fn generate_branch_name_result(&self, issue_keys: &[String]) -> CommandValue {
        let issues = self.resolve_branch_name_issues(issue_keys).await;

        let issue_id_pairs: Vec<(String, String)> = issue_keys
            .iter()
            .map(|id| {
                let provider = issues
                    .iter()
                    .find(|(issue_id, _)| issue_id == id)
                    .map(|(_, issue)| self.provider_name_for_issue(issue))
                    .unwrap_or_else(|| self.default_issue_provider_name());
                (provider, id.clone())
            })
            .collect();

        info!(requested_issue_count = issue_keys.len(), resolved_issue_count = issues.len(), "generating branch name");
        let branch_result = if let Some(ai) = self.registry.ai_utilities.preferred() {
            let context: Vec<String> = issue_keys
                .iter()
                .map(|id| {
                    issues
                        .iter()
                        .find(|(issue_id, _)| issue_id == id)
                        .map(|(_, issue)| self.format_branch_issue_context(id, issue))
                        .unwrap_or_else(|| format!("issue {id}"))
                })
                .collect();
            let prompt_text = if context.len() == 1 { context[0].clone() } else { context.join("; ") };
            Some(ai.generate_branch_name(&prompt_text).await)
        } else {
            None
        };

        match branch_result {
            Some(Ok(name)) => {
                info!(%name, "AI suggested");
                CommandValue::BranchNameGenerated { name, issue_ids: issue_id_pairs }
            }
            Some(Err(error)) => {
                warn!(%error, "using fallback branch name after AI failure");
                let fallback: Vec<String> = issue_keys.iter().map(|id| format!("issue-{id}")).collect();
                let name = fallback.join("-");
                CommandValue::BranchNameGenerated { name, issue_ids: issue_id_pairs }
            }
            None => {
                if !issues.is_empty() {
                    warn!("using fallback branch name without AI provider");
                } else {
                    warn!("using fallback branch name without resolved issue context");
                }
                let fallback: Vec<String> = issue_keys.iter().map(|id| format!("issue-{id}")).collect();
                let name = fallback.join("-");
                CommandValue::BranchNameGenerated { name, issue_ids: issue_id_pairs }
            }
        }
    }

    async fn resolve_branch_name_issues(&self, issue_keys: &[String]) -> Vec<(String, Issue)> {
        let mut resolved = HashMap::new();
        if !issue_keys.is_empty() {
            if let Some(tracker) = self.registry.issue_provider_for(&self.issue_source) {
                match tracker.fetch_by_ids(&self.issue_source, issue_keys).await {
                    Ok(fetched) => {
                        for issue in fetched {
                            resolved.insert(issue.reference.id.clone(), issue);
                        }
                    }
                    Err(error) => {
                        warn!(%error, missing_issue_count = issue_keys.len(), "failed to fetch missing issues for branch naming");
                    }
                }
            }
        }

        issue_keys.iter().filter_map(|key| resolved.get(key.as_str()).cloned().map(|issue| (key.clone(), issue))).collect()
    }

    fn format_branch_issue_context(&self, id: &str, issue: &Issue) -> String {
        if issue.labels.is_empty() {
            format!("{} #{}", issue.title, id)
        } else {
            format!("{} #{} [{}]", issue.title, id, issue.labels.join(", "))
        }
    }

    fn default_issue_provider_name(&self) -> String {
        self.registry.issue_trackers.preferred_name().map(|name| name.to_string()).unwrap_or_else(|| "issues".to_string())
    }

    fn provider_name_for_issue(&self, issue: &Issue) -> String {
        if issue.provider_name.is_empty() {
            self.default_issue_provider_name()
        } else {
            issue.provider_name.clone()
        }
    }
}

fn session_provider_key<'a>(session: &'a CloudAgentSession, _session_id: &str) -> Option<&'a str> {
    (!session.provider_name.is_empty()).then_some(session.provider_name.as_str())
}
