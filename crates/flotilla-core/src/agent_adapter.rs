use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::arg::{flatten, Arg};
use flotilla_resources::TerminalBrief;

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{discovery::EnvironmentBag, CommandRunner},
};

pub const TRUSTED_IMPLICIT_STANCE: &str = "trusted-implicit";

pub fn crew_brief_path(role: &str) -> String {
    let file_name: String = role
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') { character } else { '_' })
        .collect();
    format!(".flotilla/briefs/{}.md", if file_name.is_empty() { "crew" } else { &file_name })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewBriefMember {
    pub role: String,
    pub state: String,
    pub is_agent: bool,
}

pub fn build_crew_brief(
    context: &flotilla_resources::TerminalCrewContext,
    vessel: &str,
    role: &str,
    prompt: Option<&str>,
    members: &[CrewBriefMember],
) -> flotilla_resources::TerminalBrief {
    let mut content = format!(
        "# Flotilla crew brief\n\nYou are `{role}` in convoy `{}`, aboard vessel `{vessel}` (`{}`).\n\n## Crew\n\n",
        context.convoy, context.vessel_ref
    );
    for member in members {
        content.push_str(&format!("- `{}`: {}\n", member.role, member.state));
    }
    content.push_str("\nRun `flotilla crew list` for current crew state.\n");
    for member in members.iter().filter(|member| member.is_agent && member.role != role) {
        content.push_str(&format!("Hand off to {} with `flotilla crew {} handoff --message '...'`.\n", member.role, member.role));
    }
    content.push_str(&format!(
        "Complete your assignment with `flotilla crew complete --message '...'`. If it cannot be completed, report the failure with `flotilla crew fail --message '...'`.\n\n## Assignment\n\n{}\n",
        prompt.unwrap_or("No additional assignment was provided.")
    ));
    flotilla_resources::TerminalBrief { path: crew_brief_path(role), content, copies: Vec::new() }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLaunchRequest {
    pub role: String,
    pub model: Option<String>,
    pub brief: TerminalBrief,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLaunchPlan {
    pub command: String,
    pub env: Vec<(String, String)>,
    pub stance: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequirement {
    pub adapter: String,
    pub model: Option<String>,
}

pub struct CapabilityTable {
    requirements: BTreeMap<String, AgentRequirement>,
}

impl CapabilityTable {
    pub fn seeded() -> Self {
        Self {
            requirements: BTreeMap::from([
                ("coding".into(), AgentRequirement { adapter: "codex".into(), model: None }),
                ("code".into(), AgentRequirement { adapter: "codex".into(), model: None }),
                ("review".into(), AgentRequirement { adapter: "claude-code".into(), model: Some("opus".into()) }),
                ("code-review".into(), AgentRequirement { adapter: "claude-code".into(), model: Some("opus".into()) }),
            ]),
        }
    }

    pub fn resolve(&self, capability: &str) -> Result<&AgentRequirement, String> {
        self.requirements.get(capability).ok_or_else(|| format!("unknown agent capability `{capability}`"))
    }
}

impl Default for CapabilityTable {
    fn default() -> Self {
        Self::seeded()
    }
}

#[async_trait]
pub trait AgentAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    async fn prepare(&self, cwd: &ExecutionEnvironmentPath, brief: &TerminalBrief) -> Result<(), String>;
    fn deliver_brief(&self, brief: &TerminalBrief) -> String {
        format!("Read your crew brief at {} and follow it.", brief.path)
    }
    fn launch(&self, request: &AgentLaunchRequest) -> Result<AgentLaunchPlan, String>;
}

struct CliAgentAdapter {
    id: &'static str,
    binary: String,
    runner: Arc<dyn CommandRunner>,
    autonomy_args: &'static [&'static str],
}

impl CliAgentAdapter {
    fn command(&self, request: &AgentLaunchRequest) -> String {
        let mut args = vec![Arg::Literal(self.binary.clone())];
        args.extend(self.autonomy_args.iter().map(|arg| Arg::Literal((*arg).into())));
        if let Some(model) = &request.model {
            args.extend([Arg::Literal("--model".into()), Arg::Literal(model.clone())]);
        }
        args.push(Arg::Quoted(self.deliver_brief(&request.brief)));
        flatten(&args, 0)
    }
}

#[async_trait]
impl AgentAdapter for CliAgentAdapter {
    fn id(&self) -> &'static str {
        self.id
    }

    async fn prepare(&self, cwd: &ExecutionEnvironmentPath, brief: &TerminalBrief) -> Result<(), String> {
        self.runner.write_file(&cwd.as_path().join(&brief.path), &brief.content).await
    }

    fn launch(&self, request: &AgentLaunchRequest) -> Result<AgentLaunchPlan, String> {
        Ok(AgentLaunchPlan { command: self.command(request), env: Vec::new(), stance: TRUSTED_IMPLICIT_STANCE.into() })
    }
}

#[derive(Default)]
pub struct AgentAdapterRegistry {
    adapters: BTreeMap<String, Arc<dyn AgentAdapter>>,
}

impl AgentAdapterRegistry {
    pub fn discover(env: &EnvironmentBag, runner: Arc<dyn CommandRunner>) -> Self {
        let mut registry = Self::default();
        if let Some(binary) = env.find_binary("claude") {
            registry.insert(Arc::new(CliAgentAdapter {
                id: "claude-code",
                binary: binary.as_path().display().to_string(),
                runner: Arc::clone(&runner),
                autonomy_args: &["--dangerously-skip-permissions"],
            }));
        }
        if let Some(binary) = env.find_binary("codex") {
            registry.insert(Arc::new(CliAgentAdapter {
                id: "codex",
                binary: binary.as_path().display().to_string(),
                runner,
                autonomy_args: &["--dangerously-bypass-approvals-and-sandbox"],
            }));
        }
        registry
    }

    pub fn insert(&mut self, adapter: Arc<dyn AgentAdapter>) {
        self.adapters.insert(adapter.id().to_string(), adapter);
    }

    pub fn get(&self, id: &str) -> Option<&Arc<dyn AgentAdapter>> {
        self.adapters.get(id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        agent_adapter::{AgentAdapterRegistry, AgentLaunchRequest, CapabilityTable},
        path_context::ExecutionEnvironmentPath,
        providers::{
            discovery::{EnvironmentAssertion, EnvironmentBag},
            testing::MockRunner,
        },
    };

    fn discovered_registry() -> AgentAdapterRegistry {
        let env = EnvironmentBag::new()
            .with(EnvironmentAssertion::binary("claude", "/tools/claude"))
            .with(EnvironmentAssertion::binary("codex", "/tools/codex"));
        AgentAdapterRegistry::discover(&env, Arc::new(MockRunner::new(vec![])))
    }

    #[test]
    fn capability_resolution_selects_harness_and_model_without_exposing_harnesses_to_templates() {
        let table = CapabilityTable::seeded();

        let coding = table.resolve("coding").expect("coding requirement");
        assert_eq!(coding.adapter, "codex");
        assert_eq!(coding.model.as_deref(), None);

        let review = table.resolve("review").expect("review requirement");
        assert_eq!(review.adapter, "claude-code");
        assert_eq!(review.model.as_deref(), Some("opus"));
        assert_eq!(table.resolve("code").expect("ADR code alias").adapter, "codex");
        assert_eq!(table.resolve("code-review").expect("example review alias").adapter, "claude-code");

        assert_eq!(table.resolve("architect").expect_err("unknown capability must fail"), "unknown agent capability `architect`");
    }

    #[tokio::test]
    async fn adapters_prepare_the_canonical_brief_and_launch_with_only_a_short_pointer() {
        let registry = discovered_registry();
        let cwd = ExecutionEnvironmentPath::new("/workspace");
        let brief = flotilla_resources::TerminalBrief {
            path: ".flotilla/briefs/coder.md".into(),
            content: "protocol preamble\n\nImplement the issue.".into(),
            copies: Vec::new(),
        };

        let codex = registry.get("codex").expect("codex adapter");
        codex.prepare(&cwd, &brief).await.expect("prepare brief");
        let plan =
            codex.launch(&AgentLaunchRequest { role: "coder".into(), model: None, brief: brief.clone() }).expect("codex launch plan");
        assert_eq!(
            plan.command,
            "/tools/codex --dangerously-bypass-approvals-and-sandbox 'Read your crew brief at .flotilla/briefs/coder.md and follow it.'"
        );
        assert!(!plan.command.contains("Implement the issue"));
        assert_eq!(plan.stance, "trusted-implicit");

        let claude = registry.get("claude-code").expect("claude adapter");
        let plan =
            claude.launch(&AgentLaunchRequest { role: "reviewer".into(), model: Some("opus".into()), brief }).expect("claude launch plan");
        assert_eq!(
            plan.command,
            "/tools/claude --dangerously-skip-permissions --model opus 'Read your crew brief at .flotilla/briefs/coder.md and follow it.'"
        );
        assert!(!plan.command.contains("Implement the issue"));
    }
}
