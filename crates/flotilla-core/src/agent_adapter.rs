use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_protocol::arg::{flatten, Arg};
use flotilla_resources::{TerminalAttentionState, TerminalBrief};
use serde::Serialize;
use tokio::sync::Mutex;
use toml_edit::{value, DocumentMut, Item, Table};

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{discovery::EnvironmentBag, terminal::TerminalEnvVars, ChannelLabel, CommandRunner},
};

pub const TRUSTED_IMPLICIT_STANCE: &str = "trusted-implicit";
pub const DEFAULT_CREW_BRIEF_TEMPLATE: &str = "crew.md";
const BUILTIN_CREW_BRIEF_TEMPLATE: &str = include_str!("agent_adapter/templates/crew.md");
const BUILTIN_INTERACTIVE_SESSION_BRIEF_TEMPLATE: &str = include_str!("agent_adapter/templates/interactive-session.md");
const BUILTIN_DIFF_REVIEW_BRIEF_TEMPLATE: &str = include_str!("agent_adapter/templates/diff-review.md");
const BUILTIN_SHEPHERD_BRIEF_TEMPLATE: &str = include_str!("agent_adapter/templates/shepherd.md");
const BUILTIN_FORK_STANCE_BRIEF_LAYER: &str = include_str!("agent_adapter/templates/fork-stance.md");
const BRIEF_TEMPLATE_DIR: &str = "brief-templates";

pub fn crew_brief_path(role: &str) -> String {
    let file_name: String = role
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') { character } else { '_' })
        .collect();
    format!(".flotilla/briefs/{}.md", if file_name.is_empty() { "crew" } else { &file_name })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrewBriefMember {
    pub role: String,
    pub state: String,
    pub is_agent: bool,
}

/// What the `## Assignment` section of a crew brief should say. Distinct from
/// `Option<&str>` because "no ad-hoc prompt" splits into two very different
/// situations: the convoy carries an issue (the issue *is* the assignment) or
/// nothing was provided at all. Conflating them taught a crew to read "no
/// additional assignment" as overriding the issue contract below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrewAssignment<'a> {
    /// An explicit dispatch or handoff prompt.
    Prompt(&'a str),
    /// The convoy carries issue snapshots appended below the brief; those issues
    /// are the assignment.
    CarriedIssue,
    /// The convoy carries an explicitly bound change request in its work
    /// context; that request is the assignment.
    CarriedChangeRequest,
    /// Nothing was provided.
    Unassigned,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrewBriefTemplateResolver {
    config_dir: Option<PathBuf>,
}

impl CrewBriefTemplateResolver {
    pub fn with_config_dir(config_dir: impl Into<PathBuf>) -> Self {
        Self { config_dir: Some(config_dir.into()) }
    }

    pub fn render_options(
        &self,
        template: Option<&str>,
        project_ref: Option<&str>,
        repo_roots: impl IntoIterator<Item = PathBuf>,
    ) -> CrewBriefRenderOptions {
        self.render_options_with_fork_stance(template, project_ref, repo_roots, false)
    }

    pub fn render_options_with_fork_stance(
        &self,
        template: Option<&str>,
        project_ref: Option<&str>,
        repo_roots: impl IntoIterator<Item = PathBuf>,
        fork_stance: bool,
    ) -> CrewBriefRenderOptions {
        let template = template.filter(|template| !template.trim().is_empty()).unwrap_or(DEFAULT_CREW_BRIEF_TEMPLATE);
        let override_filename = match template {
            "interactive-session" => "interactive-session.md",
            "diff-review" => "diff-review.md",
            "shepherd" => "shepherd.md",
            other => other,
        };
        let mut overrides = Vec::new();
        if let Some(config_dir) = &self.config_dir {
            push_template_override(&mut overrides, config_dir.join(BRIEF_TEMPLATE_DIR).join(override_filename));
            if let Some(project_ref) = project_ref {
                push_template_override(
                    &mut overrides,
                    config_dir.join("projects").join(project_ref).join(BRIEF_TEMPLATE_DIR).join(override_filename),
                );
            }
        }
        for repo_root in repo_roots {
            push_template_override(&mut overrides, repo_root.join(".flotilla").join(BRIEF_TEMPLATE_DIR).join(override_filename));
        }
        CrewBriefRenderOptions { template: template.to_string(), overrides, fork_stance, has_credential_scope: false }
    }
}

fn push_template_override(overrides: &mut Vec<CrewBriefTemplateOverride>, path: PathBuf) {
    match std::fs::read_to_string(&path) {
        Ok(source) => overrides.push(CrewBriefTemplateOverride { path, source }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(path = %path.display(), err = %err, "failed to read crew brief template override"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewBriefRenderOptions {
    pub template: String,
    pub overrides: Vec<CrewBriefTemplateOverride>,
    pub fork_stance: bool,
    pub has_credential_scope: bool,
}

impl Default for CrewBriefRenderOptions {
    fn default() -> Self {
        Self { template: DEFAULT_CREW_BRIEF_TEMPLATE.to_string(), overrides: Vec::new(), fork_stance: false, has_credential_scope: false }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewBriefTemplateOverride {
    pub path: PathBuf,
    pub source: String,
}

#[derive(Debug, Serialize)]
struct CrewBriefTemplateContext<'a> {
    role: &'a str,
    convoy: &'a str,
    vessel: &'a str,
    vessel_ref: &'a str,
    assignment_text: &'a str,
    members: &'a [CrewBriefMember],
    handoff_members: Vec<&'a CrewBriefMember>,
    has_credential_scope: bool,
    has_in_crew_reviewer: bool,
}

pub fn build_crew_brief(
    context: &flotilla_resources::TerminalCrewContext,
    vessel: &str,
    role: &str,
    assignment: CrewAssignment<'_>,
    members: &[CrewBriefMember],
) -> flotilla_resources::TerminalBrief {
    build_crew_brief_with_options(context, vessel, role, assignment, members, &CrewBriefRenderOptions::default())
        .expect("built-in crew brief template should render")
}

pub fn build_crew_brief_with_options(
    context: &flotilla_resources::TerminalCrewContext,
    vessel: &str,
    role: &str,
    assignment: CrewAssignment<'_>,
    members: &[CrewBriefMember],
    options: &CrewBriefRenderOptions,
) -> Result<flotilla_resources::TerminalBrief, String> {
    let assignment_text = match assignment {
        CrewAssignment::Prompt(prompt) => prompt,
        CrewAssignment::CarriedIssue =>
            "Your assignment is the issue snapshot section below. Its body is the contract: work it to completion and deliver as described above.",
        CrewAssignment::CarriedChangeRequest =>
            "Your assignment is the bound pull request in the work context below. Work that pull request to the completion standard described above.",
        CrewAssignment::Unassigned =>
            "No assignment was provided with this dispatch. Check `## Human instruction` below if present; otherwise report via `flotilla crew fail` rather than inventing work.",
    };
    let mut content = render_crew_brief_template(options, &CrewBriefTemplateContext {
        role,
        convoy: &context.convoy,
        vessel,
        vessel_ref: &context.vessel_ref,
        assignment_text,
        members,
        handoff_members: members.iter().filter(|member| member.is_agent && member.role != role).collect(),
        has_credential_scope: options.has_credential_scope,
        has_in_crew_reviewer: members.iter().any(|member| member.is_agent && member.role == "reviewer"),
    })?;
    if !content.ends_with('\n') {
        content.push('\n');
    }
    Ok(flotilla_resources::TerminalBrief { path: crew_brief_path(role), content, copies: Vec::new() })
}

fn render_crew_brief_template(options: &CrewBriefRenderOptions, context: &CrewBriefTemplateContext<'_>) -> Result<String, String> {
    let mut env = minijinja::Environment::new();
    env.add_template(BUILTIN_CREW_BRIEF_TEMPLATE_NAME, BUILTIN_CREW_BRIEF_TEMPLATE)
        .map_err(|err| format!("load built-in crew brief template: {err}"))?;
    env.add_template(BUILTIN_INTERACTIVE_SESSION_BRIEF_TEMPLATE_NAME, BUILTIN_INTERACTIVE_SESSION_BRIEF_TEMPLATE)
        .map_err(|err| format!("load built-in interactive-session brief template: {err}"))?;
    env.add_template(BUILTIN_DIFF_REVIEW_BRIEF_TEMPLATE_NAME, BUILTIN_DIFF_REVIEW_BRIEF_TEMPLATE)
        .map_err(|err| format!("load built-in diff-review brief template: {err}"))?;
    env.add_template(BUILTIN_SHEPHERD_BRIEF_TEMPLATE_NAME, BUILTIN_SHEPHERD_BRIEF_TEMPLATE)
        .map_err(|err| format!("load built-in shepherd brief template: {err}"))?;
    let mut skip_overrides = 0;
    let mut current_template = match options.template.as_str() {
        DEFAULT_CREW_BRIEF_TEMPLATE => BUILTIN_CREW_BRIEF_TEMPLATE_NAME.to_string(),
        "interactive-session" | "interactive-session.md" => BUILTIN_INTERACTIVE_SESSION_BRIEF_TEMPLATE_NAME.to_string(),
        "diff-review" | "diff-review.md" => BUILTIN_DIFF_REVIEW_BRIEF_TEMPLATE_NAME.to_string(),
        "shepherd" | "shepherd.md" => BUILTIN_SHEPHERD_BRIEF_TEMPLATE_NAME.to_string(),
        custom if !options.overrides.is_empty() => {
            let first = &options.overrides[0];
            if is_block_only_override(&first.source) {
                BUILTIN_CREW_BRIEF_TEMPLATE_NAME.to_string()
            } else {
                skip_overrides = 1;
                let name = format!("override/base/{custom}");
                env.add_template_owned(name.clone(), first.source.clone())
                    .map_err(|err| format!("load crew brief template {}: {err}", first.path.display()))?;
                name
            }
        }
        custom => return Err(format!("unknown crew brief template `{custom}`")),
    };
    for (index, template_override) in options.overrides.iter().enumerate().skip(skip_overrides) {
        let name = format!("override/{index}/{}", options.template);
        let source = layered_override_source(&current_template, &template_override.source);
        env.add_template_owned(name.clone(), source)
            .map_err(|err| format!("load crew brief template {}: {err}", template_override.path.display()))?;
        current_template = name;
    }
    if options.fork_stance {
        let name = format!("builtin/fork-stance/{}", options.template);
        let source = format!("{{% extends \"{current_template}\" %}}\n{BUILTIN_FORK_STANCE_BRIEF_LAYER}");
        env.add_template_owned(name.clone(), source).map_err(|err| format!("load built-in fork-stance brief layer: {err}"))?;
        current_template = name;
    }
    env.get_template(&current_template)
        .and_then(|template| template.render(context))
        .map_err(|err| format!("render crew brief template {}: {err}", options.template))
}

const BUILTIN_CREW_BRIEF_TEMPLATE_NAME: &str = "builtin/crew.md";
const BUILTIN_INTERACTIVE_SESSION_BRIEF_TEMPLATE_NAME: &str = "builtin/interactive-session.md";
const BUILTIN_DIFF_REVIEW_BRIEF_TEMPLATE_NAME: &str = "builtin/diff-review.md";
const BUILTIN_SHEPHERD_BRIEF_TEMPLATE_NAME: &str = "builtin/shepherd.md";

fn layered_override_source(parent: &str, source: &str) -> String {
    if !is_block_only_override(source) {
        source.to_string()
    } else {
        format!("{{% extends \"{parent}\" %}}\n{source}")
    }
}

fn is_block_only_override(source: &str) -> bool {
    // Override files without an explicit `{% extends %}` are treated as terse
    // block fragments. A standalone template that declares its own blocks must
    // include an explicit `{% extends %}` or avoid block declarations.
    source.contains("{% block") && !source.contains("{% extends")
}

/// Appends the convoy's work context (branch, repositories, issue snapshots,
/// human instruction) to a crew brief. Every brief whose assignment is
/// [`CrewAssignment::CarriedIssue`] must pass through here — the assignment text
/// points at the issue snapshot section this writes.
pub fn append_convoy_work_context(
    content: &mut String,
    convoy: &flotilla_resources::ResourceObject<flotilla_resources::Convoy>,
    repository_refs: &[flotilla_resources::RepositoryKey],
    credential_scopes: &BTreeMap<String, BTreeSet<flotilla_resources::RepositoryKey>>,
) {
    content.push_str("\n\n## Work context\n\n");
    if let Some(branch) = &convoy.spec.r#ref {
        content.push_str(&format!("- Branch: `{branch}`\n"));
    }
    content.push_str("- Repositories:\n");
    for repository in convoy.spec.repositories.iter().filter(|repository| repository_refs.contains(&repository.repo_ref)) {
        content.push_str(&format!("  - `{}` — {} (target `{}`)\n", repository.repo_ref, repository.url, repository.target_ref));
    }
    if !credential_scopes.is_empty() {
        content.push_str("- Minted credential repository scope:\n");
        for (credential, scope) in credential_scopes {
            content.push_str(&format!("  - `{credential}`:\n"));
            for repo_ref in scope {
                let repository = convoy.spec.repositories.iter().find(|repository| &repository.repo_ref == repo_ref);
                match repository {
                    Some(repository) => content.push_str(&format!("    - `{}` — {}\n", repository.repo_ref, repository.url)),
                    None => content.push_str(&format!("    - `{repo_ref}`\n")),
                }
            }
        }
    }
    if let Some(change_request) = &convoy.spec.change_request {
        content.push_str(&format!(
            "- Bound pull request: `#{}` — {} (`{}`)\n",
            change_request.id, change_request.title, change_request.repository_ref
        ));
    }
    if !convoy.spec.issues.is_empty() {
        let header = if convoy.spec.issues.len() == 1 { "Issue snapshot" } else { "Issue snapshots" };
        content.push_str(&format!("\n## {header}\n\n"));
    }
    for issue in &convoy.spec.issues {
        content.push_str(&format!(
            "Source-qualified reference: `{}` / `{}` / `{}`\n\n",
            issue.reference.source.service, issue.reference.source.scope, issue.reference.id
        ));
        content.push_str(&format!("Snapshot as of `{}`.\n\n", issue.snapshot.as_of.to_rfc3339()));
        content.push_str(&format!("### {}\n\n", issue.snapshot.title));
        content.push_str(&format!("State: `{:?}`\n\n", issue.snapshot.state).to_lowercase());
        content.push_str(&format!("Labels: {}\n\n", issue.snapshot.labels.join(", ")));
        if let Some(body) = &issue.snapshot.body {
            content.push_str(body);
            content.push('\n');
        }
        content.push('\n');
    }
    if let Some(instruction) = &convoy.spec.instruction {
        content.push_str("\n## Human instruction\n\n");
        content.push_str(instruction);
        content.push('\n');
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLaunchRequest {
    pub role: String,
    pub model: Option<String>,
    pub brief: TerminalBrief,
    pub environment: TerminalEnvVars,
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
    credential_policy: AgentCredentialPolicy,
}

const CLAUDE_CODE_ADAPTER_ID: &str = "claude-code";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentCredentialPolicy {
    None,
    DeliveredWithAmbientFallback {
        slot: &'static str,
        scope: &'static str,
    },
    #[cfg(test)]
    AmbientOnly {
        scope: &'static str,
    },
}

pub struct CapabilityTable {
    requirements: BTreeMap<String, AgentRequirement>,
}

impl CapabilityTable {
    pub fn seeded() -> Self {
        Self {
            requirements: BTreeMap::from([
                ("coding".into(), AgentRequirement::new("codex", None)),
                ("code".into(), AgentRequirement::new("codex", None)),
                ("review".into(), AgentRequirement::new("claude-code", Some("opus".into()))),
                ("code-review".into(), AgentRequirement::new("claude-code", Some("opus".into()))),
                // The most judgment-concentrated, lowest-volume role in the
                // fleet gets the strongest planner available (ADR 0030).
                ("governor".into(), AgentRequirement::new(CLAUDE_CODE_ADAPTER_ID, Some("fable".into()))),
            ]),
        }
    }

    pub fn resolve(&self, capability: &str) -> Result<&AgentRequirement, String> {
        self.requirements.get(capability).ok_or_else(|| format!("unknown agent capability `{capability}`"))
    }

    #[cfg(test)]
    pub(crate) fn with_ambient_only_test_requirement(mut self, capability: &str) -> Self {
        self.requirements.insert(capability.to_string(), AgentRequirement {
            adapter: "ambient-only-test".to_string(),
            model: None,
            credential_policy: AgentCredentialPolicy::AmbientOnly { scope: flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE },
        });
        self
    }

    /// Resolve a workflow selector to its effective requirement.
    ///
    /// A selector carrying a dispatch-time `adapter` override wins outright —
    /// its `model` (or none) rides with it, never the table's, since a model
    /// name only makes sense against the harness it was chosen for. An
    /// adapter-less `model` override reskins the table's adapter. A selector
    /// with an explicit adapter does not need its capability in the table:
    /// the capability is then template vocabulary, and the admitted-vocabulary
    /// check that matters (adapter availability) happens at placement.
    pub fn resolve_selector(&self, selector: &flotilla_resources::Selector) -> Result<AgentRequirement, String> {
        if let Some(adapter) = &selector.adapter {
            return Ok(AgentRequirement::new(adapter, selector.model.clone()));
        }
        let seeded = self.resolve(&selector.capability)?;
        Ok(AgentRequirement {
            adapter: seeded.adapter.clone(),
            model: selector.model.clone().or_else(|| seeded.model.clone()),
            credential_policy: seeded.credential_policy,
        })
    }
}

/// Resolve the distinct agent adapters required by crew process selectors.
///
/// Admission and vessel materialization share this primitive so selector
/// overrides and capability errors cannot drift between the two paths.
pub fn required_agent_adapters<'a>(crew: impl IntoIterator<Item = &'a flotilla_resources::CrewSpec>) -> Result<BTreeSet<String>, String> {
    let capabilities = CapabilityTable::seeded();
    let mut required = BTreeSet::new();
    for process in crew {
        if let flotilla_resources::CrewSource::Agent { selector, .. } = &process.source {
            required.insert(capabilities.resolve_selector(selector)?.adapter);
        }
    }
    Ok(required)
}

impl AgentRequirement {
    fn new(adapter: impl Into<String>, model: Option<String>) -> Self {
        let adapter = adapter.into();
        let credential_policy = match adapter.as_str() {
            CLAUDE_CODE_ADAPTER_ID => AgentCredentialPolicy::DeliveredWithAmbientFallback {
                slot: "claude",
                scope: flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE,
            },
            _ => AgentCredentialPolicy::None,
        };
        Self { adapter, model, credential_policy }
    }

    /// Credential delivery slot supported by this adapter. A session must
    /// never fall through to ambient or interactive login when its adapter has
    /// a deliverable material form.
    pub fn credential_delivery_slot(&self) -> Option<&'static str> {
        match self.credential_policy {
            AgentCredentialPolicy::DeliveredWithAmbientFallback { slot, .. } => Some(slot),
            AgentCredentialPolicy::None => None,
            #[cfg(test)]
            AgentCredentialPolicy::AmbientOnly { .. } => None,
        }
    }

    /// Ambient-login scope this adapter falls back to when no credential is
    /// delivered, named as it appears under the Host `credential_expiry`
    /// capability. `None` for adapters with no ambient authentication path.
    pub fn ambient_credential_scope(&self) -> Option<&'static str> {
        match self.credential_policy {
            AgentCredentialPolicy::DeliveredWithAmbientFallback { scope, .. } => Some(scope),
            AgentCredentialPolicy::None => None,
            #[cfg(test)]
            AgentCredentialPolicy::AmbientOnly { scope } => Some(scope),
        }
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
    /// Prepare an invocation with the environment selected for this particular
    /// crew process. Credential-backed paths are deliberately resolved here,
    /// rather than from the environment in which adapter discovery happened.
    async fn prepare_with_environment(
        &self,
        cwd: &ExecutionEnvironmentPath,
        brief: &TerminalBrief,
        _environment: &TerminalEnvVars,
    ) -> Result<(), String> {
        self.prepare(cwd, brief).await
    }
    async fn cleanup(&self, _cwd: &ExecutionEnvironmentPath, _brief: &TerminalBrief) -> Result<(), String> {
        Ok(())
    }
    fn deliver_brief(&self, brief: &TerminalBrief) -> String {
        format!("Read your crew brief at {} and follow it.", brief.path)
    }
    fn classify_screen_attention(&self, _screen: &str) -> Option<TerminalAttentionState> {
        None
    }
    fn launch(&self, request: &AgentLaunchRequest) -> Result<AgentLaunchPlan, String>;
}

/// Where a managed Claude Code session's Flotilla settings overlay is written,
/// relative to the session's working directory. It lives under `.flotilla/`
/// alongside the brief so it inherits the same git exclusion and teardown.
pub const CLAUDE_MANAGED_SETTINGS_PATH: &str = ".flotilla/claude-settings.json";

struct CliAgentAdapter {
    binary: String,
    runner: Arc<dyn CommandRunner>,
    flavor: AdapterFlavor,
}

/// Per-harness behaviour. Each CLI agent gates autonomy differently and needs a
/// different thing seeded before it will run unattended, so the differences are
/// named here rather than inferred from an id string.
enum AdapterFlavor {
    /// Claude requires onboarding, workspace trust, and bypass-mode consent
    /// even when credentials and `--dangerously-skip-permissions` are already
    /// present. Managed sessions seed the first two into Claude's mutable state
    /// and carry the documented bypass-consent setting alongside Flotilla's
    /// hooks in the invocation-only `--settings` overlay.
    ClaudeCode { state_config: Option<ClaudeStateConfig>, state_lock: Arc<Mutex<()>>, contained: bool },
    /// Codex gates on a persisted per-project trust level, so the workspace has
    /// to be marked trusted in its config before launch.
    Codex { trust_config: Option<CodexTrustConfig> },
}

impl AdapterFlavor {
    fn id(&self) -> &'static str {
        match self {
            Self::ClaudeCode { .. } => CLAUDE_CODE_ADAPTER_ID,
            Self::Codex { .. } => "codex",
        }
    }

    fn autonomy_args(&self) -> &'static [&'static str] {
        match self {
            Self::ClaudeCode { .. } => &["--dangerously-skip-permissions"],
            Self::Codex { .. } => &["--dangerously-bypass-approvals-and-sandbox"],
        }
    }

    /// Files the adapter writes into the working directory and must remove on
    /// teardown, beyond the brief itself.
    fn managed_files(&self) -> &'static [&'static str] {
        match self {
            Self::ClaudeCode { .. } => &[CLAUDE_MANAGED_SETTINGS_PATH],
            Self::Codex { .. } => &[],
        }
    }
}

#[derive(Clone)]
struct CodexTrustConfig {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

#[derive(Clone)]
struct ClaudeStateConfig {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl CliAgentAdapter {
    fn command(&self, request: &AgentLaunchRequest) -> String {
        let mut args = vec![Arg::Literal(self.binary.clone())];
        args.extend(self.flavor.autonomy_args().iter().map(|arg| Arg::Literal((*arg).into())));
        if matches!(&self.flavor, AdapterFlavor::ClaudeCode { .. }) {
            args.extend([Arg::Literal("--settings".into()), Arg::Literal(CLAUDE_MANAGED_SETTINGS_PATH.into())]);
        }
        if let Some(model) = &request.model {
            args.extend([Arg::Literal("--model".into()), Arg::Quoted(model.clone())]);
        }
        args.push(Arg::Quoted(self.deliver_brief(&request.brief)));
        flatten(&args, 0)
    }

    fn claude_invocation_config_dir(&self, environment: &TerminalEnvVars) -> Option<PathBuf> {
        let AdapterFlavor::ClaudeCode { contained, .. } = &self.flavor else {
            return None;
        };
        contained.then(|| environment.iter().find(|(name, _)| name == "CLAUDE_CONFIG_DIR").map(|(_, value)| PathBuf::from(value))).flatten()
    }
}

#[async_trait]
impl AgentAdapter for CliAgentAdapter {
    fn id(&self) -> &'static str {
        self.flavor.id()
    }

    async fn prepare(&self, cwd: &ExecutionEnvironmentPath, brief: &TerminalBrief) -> Result<(), String> {
        self.prepare_with_environment(cwd, brief, &Vec::new()).await
    }

    async fn prepare_with_environment(
        &self,
        cwd: &ExecutionEnvironmentPath,
        brief: &TerminalBrief,
        environment: &TerminalEnvVars,
    ) -> Result<(), String> {
        match &self.flavor {
            AdapterFlavor::ClaudeCode { state_config, state_lock, contained } => {
                if *contained && !environment.iter().any(|(name, _)| name == "CLAUDE_CODE_OAUTH_TOKEN") {
                    return Err("contained Claude Code requires credential environment `CLAUDE_CODE_OAUTH_TOKEN`".to_string());
                }
                if *contained
                    && environment.iter().any(|(name, _)| name == "CLAUDE_CODE_OAUTH_TOKEN")
                    && !environment.iter().any(|(name, _)| name == "CLAUDE_CONFIG_DIR")
                {
                    return Err("Claude Code OAuth requires seam-resolved environment `CLAUDE_CONFIG_DIR`".to_string());
                }
                let invocation_state = if let Some(config_dir) = self.claude_invocation_config_dir(environment) {
                    let config_dir_string = config_dir.display().to_string();
                    self.runner.run("mkdir", &["-p", &config_dir_string], Path::new("/"), &ChannelLabel::Default).await?;
                    Some(ClaudeStateConfig { path: config_dir.join(".claude.json"), lock: Arc::clone(state_lock) })
                } else {
                    None
                };
                if let Some(state_config) = invocation_state.as_ref().or(state_config.as_ref()) {
                    seed_claude_headless_state(&*self.runner, cwd.as_path(), state_config).await?;
                }
                let mut settings = crate::agents::claude_code_hook_settings();
                settings["skipDangerousModePermissionPrompt"] = serde_json::Value::Bool(true);
                let settings =
                    serde_json::to_string_pretty(&settings).map_err(|error| format!("render Claude Code settings overlay: {error}"))?;
                self.runner.write_file(&cwd.as_path().join(CLAUDE_MANAGED_SETTINGS_PATH), &settings).await?;
            }
            AdapterFlavor::Codex { trust_config } => {
                let config = trust_config
                    .as_ref()
                    .ok_or_else(|| "cannot determine Codex config path because neither CODEX_HOME nor HOME was detected".to_string())?;
                seed_codex_workspace_trust(&*self.runner, cwd.as_path(), config).await?;
            }
        }
        self.runner.write_file(&cwd.as_path().join(&brief.path), &brief.content).await?;
        ensure_flotilla_git_exclude(&*self.runner, cwd.as_path()).await
    }

    async fn cleanup(&self, cwd: &ExecutionEnvironmentPath, brief: &TerminalBrief) -> Result<(), String> {
        remove_agent_files(&*self.runner, cwd.as_path(), brief, self.flavor.managed_files()).await
    }

    fn classify_screen_attention(&self, screen: &str) -> Option<TerminalAttentionState> {
        match &self.flavor {
            // Claude Code reports its own permission prompts through the hook
            // path, which is more precise than matching rendered text.
            AdapterFlavor::ClaudeCode { .. } => None,
            AdapterFlavor::Codex { .. } => codex_screen_needs_input(screen).then_some(TerminalAttentionState::NeedsInput),
        }
    }

    fn launch(&self, request: &AgentLaunchRequest) -> Result<AgentLaunchPlan, String> {
        let mut env = request.environment.clone();
        if matches!(&self.flavor, AdapterFlavor::ClaudeCode { contained: true, .. })
            && !env.iter().any(|(name, _)| name == "CLAUDE_CODE_OAUTH_TOKEN")
        {
            return Err("contained Claude Code requires credential environment `CLAUDE_CODE_OAUTH_TOKEN`".to_string());
        }
        if matches!(&self.flavor, AdapterFlavor::ClaudeCode { contained: true, .. })
            && env.iter().any(|(name, _)| name == "CLAUDE_CODE_OAUTH_TOKEN")
            && !env.iter().any(|(name, _)| name == "CLAUDE_CONFIG_DIR")
        {
            return Err("Claude Code OAuth requires seam-resolved environment `CLAUDE_CONFIG_DIR`".to_string());
        }
        if let Some(config_dir) = self.claude_invocation_config_dir(&env) {
            env.retain(|(name, _)| name != "CLAUDE_CONFIG_DIR");
            env.push(("CLAUDE_CONFIG_DIR".to_string(), config_dir.display().to_string()));
        } else if matches!(&self.flavor, AdapterFlavor::ClaudeCode { contained: false, .. })
            && env.iter().any(|(name, _)| name == "CLAUDE_CODE_OAUTH_TOKEN")
        {
            // Credential delivery supplies a private config directory for
            // contained crews. Trusted crews deliberately retain the host's
            // ambient settings, skills, and MCP configuration while the token
            // alone replaces ambient authentication.
            env.retain(|(name, _)| name != "CLAUDE_CONFIG_DIR");
        }
        Ok(AgentLaunchPlan { command: self.command(request), env, stance: TRUSTED_IMPLICIT_STANCE.into() })
    }
}

fn codex_screen_needs_input(screen: &str) -> bool {
    let approval_prompt = [
        "Do you trust the contents of this directory?",
        "Would you like to run the following command?",
        "Do you want to approve network access to ",
        "Would you like to grant these permissions?",
        "Would you like to make the following edits?",
        " needs your approval.",
    ]
    .iter()
    .any(|prompt| screen.contains(prompt));
    let user_question = screen.contains("Question ") && (screen.contains(" to submit answer") || screen.contains(" to submit all"));
    approval_prompt || user_question
}

async fn seed_codex_workspace_trust(runner: &dyn CommandRunner, cwd: &Path, config: &CodexTrustConfig) -> Result<(), String> {
    let _guard = config.lock.lock().await;
    let output = runner
        .run_output("pwd", &["-P"], cwd, &ChannelLabel::Default)
        .await
        .map_err(|error| format!("resolve canonical Codex workspace {}: {error}", cwd.display()))?;
    if !output.success {
        return Err(format!("resolve canonical Codex workspace {}: {}", cwd.display(), output.stderr.trim()));
    }
    let canonical_cwd = output.stdout.trim();
    if canonical_cwd.is_empty() {
        return Err(format!("resolve canonical Codex workspace {}: `pwd -P` returned an empty path", cwd.display()));
    }
    let source = runner.ensure_file(&config.path, "").await?;
    let mut document = source.parse::<DocumentMut>().map_err(|error| format!("parse Codex config {}: {error}", config.path.display()))?;
    let projects = document
        .as_table_mut()
        .entry("projects")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("Codex config {} has a non-table `projects` value", config.path.display()))?;
    let project = projects
        .entry(canonical_cwd)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("Codex config {} has a non-table project entry for {canonical_cwd}", config.path.display()))?;
    if project.get("trust_level").and_then(Item::as_str) == Some("trusted") {
        return Ok(());
    }
    project["trust_level"] = value("trusted");
    runner.write_file(&config.path, &document.to_string()).await
}

async fn seed_claude_headless_state(runner: &dyn CommandRunner, cwd: &Path, config: &ClaudeStateConfig) -> Result<(), String> {
    let _guard = config.lock.lock().await;
    let output = runner
        .run_output("pwd", &["-P"], cwd, &ChannelLabel::Default)
        .await
        .map_err(|error| format!("resolve canonical Claude workspace {}: {error}", cwd.display()))?;
    if !output.success {
        return Err(format!("resolve canonical Claude workspace {}: {}", cwd.display(), output.stderr.trim()));
    }
    let canonical_cwd = output.stdout.trim();
    if canonical_cwd.is_empty() {
        return Err(format!("resolve canonical Claude workspace {}: `pwd -P` returned an empty path", cwd.display()));
    }

    let source = runner.ensure_file(&config.path, "{}").await?;
    let mut state = serde_json::from_str::<serde_json::Value>(&source)
        .map_err(|error| format!("parse Claude state {}: {error}", config.path.display()))?;
    let root = state.as_object_mut().ok_or_else(|| format!("Claude state {} is not a JSON object", config.path.display()))?;
    root.insert("hasCompletedOnboarding".to_string(), serde_json::Value::Bool(true));
    let projects = root
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| format!("Claude state {} has a non-object `projects` value", config.path.display()))?;
    let project = projects
        .entry(canonical_cwd)
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| format!("Claude state {} has a non-object project entry for {canonical_cwd}", config.path.display()))?;
    project.insert("hasTrustDialogAccepted".to_string(), serde_json::Value::Bool(true));
    let rendered = serde_json::to_string_pretty(&state).map_err(|error| format!("render Claude state: {error}"))?;
    runner.write_file(&config.path, &rendered).await
}

async fn ensure_flotilla_git_exclude(runner: &dyn CommandRunner, cwd: &Path) -> Result<(), String> {
    let Ok(output) = runner.run_output("git", &["rev-parse", "--git-path", "info/exclude"], cwd, &ChannelLabel::Default).await else {
        return Ok(());
    };
    if !output.success {
        return Ok(());
    }
    let exclude_path = output.stdout.trim();
    if exclude_path.is_empty() {
        return Ok(());
    }

    let script = format!(
        "set -eu; exclude={}; mkdir -p \"$(dirname \"$exclude\")\"; touch \"$exclude\"; grep -qxF '.flotilla/' \"$exclude\" || printf '%s\\n' '.flotilla/' >> \"$exclude\"",
        flotilla_protocol::arg::shell_quote(exclude_path),
    );
    let _ = runner.run("sh", &["-lc", &script], cwd, &ChannelLabel::Default).await;
    Ok(())
}

/// Removes everything the adapter wrote into the working directory, then prunes
/// the `.flotilla` scaffolding it left behind. Directories are pruned deepest
/// first so an outer directory becomes empty in time to be removed.
async fn remove_agent_files(runner: &dyn CommandRunner, cwd: &Path, brief: &TerminalBrief, managed_files: &[&str]) -> Result<(), String> {
    let paths =
        std::iter::once(brief.path.as_str()).chain(managed_files.iter().copied()).map(|relative| cwd.join(relative)).collect::<Vec<_>>();

    for path in &paths {
        let path_str = path.to_str().ok_or_else(|| format!("agent file path is not valid UTF-8: {}", path.display()))?;
        runner.run("rm", &["-f", path_str], Path::new("/"), &ChannelLabel::Default).await?;
    }

    let mut directories = paths
        .iter()
        // Strictly below cwd: an absolute or escaping relative path must never
        // walk the prune up into directories the adapter did not create.
        .flat_map(|path| path.ancestors().skip(1).take_while(|ancestor| *ancestor != cwd && ancestor.starts_with(cwd)))
        .collect::<Vec<_>>();
    directories.sort_by_key(|directory| std::cmp::Reverse(directory.components().count()));
    directories.dedup();
    for directory in directories {
        let Some(directory) = directory.to_str() else {
            continue;
        };
        let _ = runner.run("rmdir", &[directory], Path::new("/"), &ChannelLabel::Default).await;
    }
    Ok(())
}

#[derive(Default)]
pub struct AgentAdapterRegistry {
    adapters: BTreeMap<String, Arc<dyn AgentAdapter>>,
}

impl AgentAdapterRegistry {
    pub fn discover(env: &EnvironmentBag, runner: Arc<dyn CommandRunner>) -> Self {
        let mut registry = Self::default();
        if let Some(binary) = env.find_binary("claude") {
            let state_path = env.find_env_var("CLAUDE_CONFIG_DIR").map(|config_dir| PathBuf::from(config_dir).join(".claude.json"));
            let state_lock = Arc::new(Mutex::new(()));
            registry.insert(Arc::new(CliAgentAdapter {
                binary: binary.as_path().display().to_string(),
                runner: Arc::clone(&runner),
                flavor: AdapterFlavor::ClaudeCode {
                    state_config: state_path.map(|path| ClaudeStateConfig { path, lock: Arc::clone(&state_lock) }),
                    state_lock,
                    contained: env.find_env_var("FLOTILLA_ENVIRONMENT_ID").is_some(),
                },
            }));
        }
        if let Some(binary) = env.find_binary("codex") {
            let config_path = env
                .find_env_var("CODEX_HOME")
                .map(PathBuf::from)
                .or_else(|| env.find_env_var("HOME").map(|home| PathBuf::from(home).join(".codex")))
                .map(|home| home.join("config.toml"));
            registry.insert(Arc::new(CliAgentAdapter {
                binary: binary.as_path().display().to_string(),
                runner,
                flavor: AdapterFlavor::Codex {
                    trust_config: config_path.map(|path| CodexTrustConfig { path, lock: Arc::new(Mutex::new(())) }),
                },
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

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.adapters.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        process::Command as ProcessCommand,
        sync::Arc,
    };

    use chrono::Utc;
    use flotilla_protocol::{IssueRef, IssueSource, IssueState};
    use flotilla_resources::{
        single_agent_contained_workflow_spec, Convoy, ConvoyIssue, ConvoyRepositorySpec, ConvoySpec, CrewSource, IssueSnapshot, ObjectMeta,
        RepositoryKey, ResourceObject, TerminalAttentionState, TerminalCrewContext,
    };
    use toml_edit::DocumentMut;

    use crate::{
        agent_adapter::{
            append_convoy_work_context, build_crew_brief, build_crew_brief_with_options, AgentAdapterRegistry, AgentLaunchRequest,
            CapabilityTable, CrewAssignment, CrewBriefMember, CrewBriefRenderOptions, CrewBriefTemplateOverride, CrewBriefTemplateResolver,
        },
        path_context::ExecutionEnvironmentPath,
        providers::{
            discovery::{EnvironmentAssertion, EnvironmentBag},
            testing::MockRunner,
            ProcessCommandRunner,
        },
    };

    fn discovered_registry() -> AgentAdapterRegistry {
        let env = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("HOME", "/home/test"))
            .with(EnvironmentAssertion::binary("claude", "/tools/claude"))
            .with(EnvironmentAssertion::binary("codex", "/tools/codex"));
        AgentAdapterRegistry::discover(
            &env,
            Arc::new(MockRunner::new(vec![Ok("/workspace\n".into()), Ok(".git/info/exclude\n".into()), Ok(String::new())])),
        )
    }

    #[test]
    fn default_single_agent_brief_requires_pr_delivery_before_completion() {
        let workflow = single_agent_contained_workflow_spec();
        let [vessel] = workflow.vessels.as_slice() else {
            panic!("default workflow should have one vessel");
        };
        let [coder] = vessel.crew.as_slice() else {
            panic!("default vessel should have one crew member");
        };
        let CrewSource::Agent { prompt, .. } = &coder.source else {
            panic!("default coder should be an agent");
        };
        let brief = build_crew_brief_with_options(
            &TerminalCrewContext {
                namespace: "flotilla".to_string(),
                convoy: "fix-delivery".to_string(),
                vessel_ref: "vessel-fix-delivery-work".to_string(),
            },
            "work",
            "coder",
            prompt.as_deref().map_or(CrewAssignment::Unassigned, CrewAssignment::Prompt),
            &[CrewBriefMember { role: "coder".to_string(), state: "active".to_string(), is_agent: true }],
            &CrewBriefRenderOptions { has_credential_scope: true, ..CrewBriefRenderOptions::default() },
        )
        .expect("render scoped coder brief");

        assert!(brief.content.contains("The pull-request destination is the repository URL and target ref named in `## Work context`"));
        assert!(brief.content.contains("the issue source may be a different forge"));
        assert!(brief.content.contains("Inspect the existing remotes and push to the one whose URL matches that destination"));
        assert!(brief.content.contains("never add or repoint a remote"));
        assert!(brief.content.contains("For a Forgejo destination, use the injected `FORGEJO_SERVER_URL`"));
        assert!(brief.content.contains("Do not use `gh`, a GitHub-only shepherding helper, or ambient human credentials"));
        assert!(brief.content.contains("only when it explicitly supports the destination forge"));
        assert!(brief.content.contains("Do not merge it"));
        assert!(brief.content.contains("Clone scratch repositories outside the vessel checkout"));
        assert!(brief.content.contains("park the verified commit"));
        assert!(brief.content.contains("redispatched under the owning project"));
    }

    fn brief_for(assignment: CrewAssignment<'_>) -> String {
        build_crew_brief(
            &TerminalCrewContext {
                namespace: "flotilla".to_string(),
                convoy: "fix-delivery".to_string(),
                vessel_ref: "vessel-fix-delivery-work".to_string(),
            },
            "work",
            "coder",
            assignment,
            &[CrewBriefMember { role: "coder".to_string(), state: "active".to_string(), is_agent: true }],
        )
        .content
    }

    #[test]
    fn default_brief_template_includes_decision_ledger_contract() {
        let content = build_crew_brief(
            &TerminalCrewContext {
                namespace: "flotilla".to_string(),
                convoy: "fix-delivery".to_string(),
                vessel_ref: "vessel-fix-delivery-work".to_string(),
            },
            "work",
            "coder",
            CrewAssignment::Prompt("Fix the flux capacitor."),
            &[
                CrewBriefMember { role: "coder".to_string(), state: "active".to_string(), is_agent: true },
                CrewBriefMember { role: "reviewer".to_string(), state: "latent".to_string(), is_agent: true },
                CrewBriefMember { role: "watcher".to_string(), state: "active".to_string(), is_agent: false },
            ],
        )
        .content;

        assert!(content.contains("## Decision ledger"));
        assert!(content.contains("ordered least-confident first"));
        assert!(content.contains("**Brief silence:**"));
        assert!(content.contains("**Choice:**"));
        assert!(content.contains("**Alternative:**"));
        assert!(content.contains("**If asking were free:**"));
        assert!(content.contains("No decisions beyond the brief."));
        assert!(content.contains("--decision-ledger-ref '<comment URL>'"));
        assert!(content.contains("A claim without this pointer is accepted but flagged"));
        assert!(content.contains("## Assignment\n\nFix the flux capacitor."));
    }

    #[test]
    fn duplicate_github_review_is_suppressed_only_for_in_crew_review() {
        let single = brief_for(CrewAssignment::Prompt("Fix the flux capacitor."));
        assert!(!single.contains("`in-vessel-review` label"));

        let reviewed = build_crew_brief(
            &TerminalCrewContext {
                namespace: "flotilla".to_string(),
                convoy: "fix-delivery".to_string(),
                vessel_ref: "vessel-fix-delivery-work".to_string(),
            },
            "work",
            "coder",
            CrewAssignment::Prompt("Fix the flux capacitor."),
            &[CrewBriefMember { role: "coder".to_string(), state: "active".to_string(), is_agent: true }, CrewBriefMember {
                role: "reviewer".to_string(),
                state: "latent".to_string(),
                is_agent: true,
            }],
        );
        assert!(reviewed.content.contains("apply the `in-vessel-review` label in the PR-create command itself"));
    }

    #[test]
    fn repo_level_override_can_replace_one_block() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let override_dir = repo.join(".flotilla/brief-templates");
        std::fs::create_dir_all(&override_dir).expect("override dir");
        std::fs::write(override_dir.join("crew.md"), "{% block delivery %}Complete when the local demo is ready.{% endblock %}")
            .expect("override file");
        let options = CrewBriefTemplateResolver::default().render_options(None, None, [repo]);
        let brief = build_crew_brief_with_options(
            &TerminalCrewContext { namespace: "flotilla".to_string(), convoy: "demo".to_string(), vessel_ref: "demo-work".to_string() },
            "work",
            "coder",
            CrewAssignment::Prompt("Demo the override."),
            &[CrewBriefMember { role: "coder".to_string(), state: "active".to_string(), is_agent: true }],
            &options,
        )
        .expect("render brief");

        assert!(brief.content.contains("Complete when the local demo is ready."));
        assert!(brief.content.contains("## Decision ledger"));
        assert!(brief.content.contains("## Assignment\n\nDemo the override."));
        assert!(!brief.content.contains("For assignments that change a repository"));
    }

    #[test]
    fn workflow_variant_selects_a_different_brief_template() {
        let default = brief_for(CrewAssignment::Prompt("Pair with the user."));
        let selected = build_crew_brief_with_options(
            &TerminalCrewContext {
                namespace: "flotilla".to_string(),
                convoy: "interactive".to_string(),
                vessel_ref: "interactive-work".to_string(),
            },
            "work",
            "driver",
            CrewAssignment::Prompt("Pair with the user."),
            &[CrewBriefMember { role: "driver".to_string(), state: "active".to_string(), is_agent: true }],
            &CrewBriefRenderOptions {
                template: "interactive-session.md".to_string(),
                overrides: Vec::new(),
                fork_stance: false,
                has_credential_scope: false,
            },
        )
        .expect("render selected template")
        .content;

        assert!(selected.contains("For interactive sessions, keep the user-facing loop tight"));
        assert!(selected.contains("## Assignment\n\nPair with the user."));
        assert!(!default.contains("For interactive sessions"));
    }

    #[test]
    fn diff_review_template_and_fork_layer_define_review_settlement() {
        let brief = build_crew_brief_with_options(
            &TerminalCrewContext {
                namespace: "flotilla".to_string(),
                convoy: "zellij-9".to_string(),
                vessel_ref: "zellij-9-work".to_string(),
            },
            "work",
            "reviewer",
            CrewAssignment::CarriedIssue,
            &[CrewBriefMember { role: "coder".to_string(), state: "handed-off".to_string(), is_agent: true }, CrewBriefMember {
                role: "reviewer".to_string(),
                state: "active".to_string(),
                is_agent: true,
            }],
            &CrewBriefRenderOptions {
                template: "diff-review".to_string(),
                overrides: Vec::new(),
                fork_stance: true,
                has_credential_scope: false,
            },
        )
        .expect("render fork review brief")
        .content;

        assert!(brief.contains("flotilla crew coder handoff --message"));
        assert!(brief.contains("sign off on the fork PR"));
        assert!(brief.contains("Never add or repoint a git remote"));
        assert!(brief.contains("Never open issues, pull requests, or comments against the upstream repository"));
        assert!(brief.contains("exact repository URL and target ref named in `## Work context`"));
        assert!(!brief.contains("only the fork remote (`origin`)"));
    }

    #[test]
    fn shepherd_template_processes_exactly_one_round_and_yields() {
        let brief = build_crew_brief_with_options(
            &TerminalCrewContext {
                namespace: "flotilla".to_string(),
                convoy: "adopt-1071".to_string(),
                vessel_ref: "adopt-1071-work".to_string(),
            },
            "work",
            "shepherd",
            CrewAssignment::Unassigned,
            &[CrewBriefMember { role: "shepherd".to_string(), state: "active".to_string(), is_agent: true }],
            &CrewBriefRenderOptions {
                template: "shepherd".to_string(),
                overrides: Vec::new(),
                fork_stance: false,
                has_credential_scope: true,
            },
        )
        .expect("render shepherd brief")
        .content;

        assert!(brief.contains("exactly one review and CI round"));
        assert!(brief.contains("Finish, don't redo"));
        assert!(brief.contains("`pr-shepherd` skill"));
        assert!(brief.contains("For a Forgejo destination, do not use that GitHub-only helper"));
        assert!(brief.contains("with the `pr-shepherd` skill for a GitHub destination, or through the injected Forgejo API credentials for a Forgejo destination"));
        assert!(!brief.contains("pull request using the `pr-shepherd` skill"));
        assert!(brief.contains("Future events belong to a later engagement"));
        assert!(brief.contains("claim's linked `## Decision ledger` comment"));
        assert!(brief.contains("A missing ledger is a finding, not grounds to reject or wedge the claim"));
        assert!(brief.contains("park the verified commit"));
        assert!(brief.contains("redispatched under the owning project"));
        assert!(brief.contains("flotilla crew complete --message '<PR URL>'"));
        assert!(!brief.contains("wait-for-checks"));
        assert!(!brief.contains("No assignment was provided"));
    }

    #[test]
    fn extensionless_interactive_template_uses_markdown_override_filename() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let override_dir = repo.join(".flotilla/brief-templates");
        std::fs::create_dir_all(&override_dir).expect("override dir");
        std::fs::write(
            override_dir.join("interactive-session.md"),
            "{% block delivery %}Interactive override from the conventional Markdown filename.{% endblock %}",
        )
        .expect("override file");

        let options = CrewBriefTemplateResolver::default().render_options(Some("interactive-session"), None, [repo]);
        let brief = build_crew_brief_with_options(
            &TerminalCrewContext {
                namespace: "flotilla".to_string(),
                convoy: "interactive".to_string(),
                vessel_ref: "interactive-work".to_string(),
            },
            "work",
            "driver",
            CrewAssignment::Prompt("Pair with the user."),
            &[CrewBriefMember { role: "driver".to_string(), state: "active".to_string(), is_agent: true }],
            &options,
        )
        .expect("render selected template");

        assert!(brief.content.contains("Interactive override from the conventional Markdown filename."));
        assert_eq!(options.overrides[0].path, override_dir.join("interactive-session.md"));
    }

    #[test]
    fn custom_block_only_template_inherits_the_default_brief_shape() {
        let brief = build_crew_brief_with_options(
            &TerminalCrewContext {
                namespace: "flotilla".to_string(),
                convoy: "pairing".to_string(),
                vessel_ref: "pairing-work".to_string(),
            },
            "work",
            "driver",
            CrewAssignment::Prompt("Pair with the user."),
            &[CrewBriefMember { role: "driver".to_string(), state: "active".to_string(), is_agent: true }],
            &CrewBriefRenderOptions {
                template: "pairing.md".to_string(),
                overrides: vec![CrewBriefTemplateOverride {
                    path: "pairing.md".into(),
                    source: "{% block delivery %}Pairing-specific delivery gate.{% endblock %}".to_string(),
                }],
                fork_stance: false,
                has_credential_scope: false,
            },
        )
        .expect("render custom block-only template");

        assert!(brief.content.starts_with("# Flotilla crew brief\n\nYou are `driver` in convoy `pairing`"));
        assert!(brief.content.contains("Pairing-specific delivery gate."));
        assert!(brief.content.contains("## Assignment\n\nPair with the user.\n"));
    }

    #[test]
    fn repo_override_wins_over_project_and_fleet_blocks() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = temp.path().join("config");
        let repo = temp.path().join("repo");
        for (dir, text) in [
            (config.join("brief-templates"), "Fleet delivery."),
            (config.join("projects/demo-project/brief-templates"), "Project delivery."),
            (repo.join(".flotilla/brief-templates"), "Repo delivery."),
        ] {
            std::fs::create_dir_all(&dir).expect("override dir");
            std::fs::write(dir.join("crew.md"), format!("{{% block delivery %}}{text}{{% endblock %}}")).expect("override file");
        }
        let options = CrewBriefTemplateResolver::with_config_dir(&config).render_options(None, Some("demo-project"), [repo]);
        let brief = build_crew_brief_with_options(
            &TerminalCrewContext { namespace: "flotilla".to_string(), convoy: "demo".to_string(), vessel_ref: "demo-work".to_string() },
            "work",
            "coder",
            CrewAssignment::Prompt("Check precedence."),
            &[CrewBriefMember { role: "coder".to_string(), state: "active".to_string(), is_agent: true }],
            &options,
        )
        .expect("render brief");

        assert!(brief.content.contains("Repo delivery."));
        assert!(!brief.content.contains("Project delivery."));
        assert!(!brief.content.contains("Fleet delivery."));
    }

    #[test]
    fn carried_issue_assignment_points_at_the_issue_section_instead_of_disclaiming() {
        let content = brief_for(CrewAssignment::CarriedIssue);
        assert!(content.contains("## Assignment\n\nYour assignment is the issue snapshot section below."));
        assert!(!content.contains("No assignment was provided"));
    }

    #[test]
    fn carried_change_request_assignment_points_at_the_bound_pr() {
        let content = brief_for(CrewAssignment::CarriedChangeRequest);
        assert!(content.contains("Your assignment is the bound pull request in the work context below."));
        assert!(!content.contains("No assignment was provided"));
    }

    #[test]
    fn convoy_work_context_separates_multiple_issue_snapshots() {
        let repo_ref = RepositoryKey("repo_widgets".to_string());
        let source = IssueSource { service: "https://github.com".to_string(), scope: "flotilla-org/flotilla".to_string() };
        let issue = |id: &str, title: &str, body: &str| ConvoyIssue {
            reference: IssueRef { source: source.clone(), id: id.to_string() },
            repository_ref: Some(repo_ref.clone()),
            snapshot: IssueSnapshot {
                title: title.to_string(),
                body: Some(body.to_string()),
                state: IssueState::Open,
                labels: Vec::new(),
                as_of: "2026-07-22T00:00:00Z".parse().expect("timestamp"),
            },
        };
        let convoy = ResourceObject::<Convoy> {
            metadata: ObjectMeta {
                name: "batch".to_string(),
                namespace: "flotilla".to_string(),
                resource_version: "1".to_string(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: Utc::now(),
                merge: None,
            },
            spec: ConvoySpec {
                workflow_ref: "workflow".to_string(),
                role: "work".to_string(),
                generation: 1,
                dispatching_principal_ref: Default::default(),
                inputs: BTreeMap::new(),
                placement_policy: None,
                repositories: vec![ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/flotilla".to_string(),
                    repo_ref: repo_ref.clone(),
                    source_ref: "main".to_string(),
                    target_ref: "main".to_string(),
                    workspace_slug: "flotilla".to_string(),
                    subpaths: Vec::new(),
                }],
                r#ref: Some("fix/batch".to_string()),
                project_ref: None,
                adopted_checkout_refs: BTreeMap::new(),
                issues: vec![issue("809", "First issue", "First issue body."), issue("810", "Second issue", "Second issue body.")],
                change_request: Some(flotilla_resources::BoundChangeRequest {
                    id: "1071".to_string(),
                    repository_ref: repo_ref.clone(),
                    title: "Existing pull request".to_string(),
                }),
                instruction: None,
            },
            status: None,
        };
        let mut content = String::new();
        append_convoy_work_context(
            &mut content,
            &convoy,
            std::slice::from_ref(&repo_ref),
            &BTreeMap::from([("github-app".to_string(), BTreeSet::from([repo_ref.clone()]))]),
        );

        assert!(content.contains("- `repo_widgets` — https://github.com/flotilla-org/flotilla (target `main`)"));
        assert!(content.contains("- Minted credential repository scope:"));
        assert!(content.contains("  - `github-app`:\n    - `repo_widgets` — https://github.com/flotilla-org/flotilla"));
        assert!(content.contains("- Bound pull request: `#1071` — Existing pull request (`repo_widgets`)"));
        assert!(content.contains("First issue body.\n\nSource-qualified reference: `https://github.com` / `flotilla-org/flotilla` / `810`"));
    }

    #[test]
    fn unassigned_brief_says_so_and_forbids_inventing_work() {
        let content = brief_for(CrewAssignment::Unassigned);
        assert!(content.contains("No assignment was provided with this dispatch."));
        assert!(content.contains("rather than inventing work"));
    }

    #[test]
    fn prompt_assignment_is_verbatim() {
        let content = brief_for(CrewAssignment::Prompt("Fix the flux capacitor."));
        assert!(content.contains("## Assignment\n\nFix the flux capacitor.\n"));
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

    #[test]
    fn selector_resolution_lets_dispatch_overrides_win_over_the_seeded_table() {
        let table = CapabilityTable::seeded();

        let plain = table.resolve_selector(&flotilla_resources::Selector::for_capability("code")).expect("seeded fallback");
        assert_eq!((plain.adapter.as_str(), plain.model.as_deref()), ("codex", None));

        let mut overridden = flotilla_resources::Selector::for_capability("code");
        overridden.adapter = Some("claude-code".to_string());
        overridden.model = Some("opus".to_string());
        let requirement = table.resolve_selector(&overridden).expect("adapter override");
        assert_eq!((requirement.adapter.as_str(), requirement.model.as_deref()), ("claude-code", Some("opus")));

        // An adapter override never inherits the table's model — a model name
        // only means something against the harness it was chosen for.
        let mut review = flotilla_resources::Selector::for_capability("review");
        review.adapter = Some("codex".to_string());
        let requirement = table.resolve_selector(&review).expect("adapter override without model");
        assert_eq!((requirement.adapter.as_str(), requirement.model.as_deref()), ("codex", None));

        let mut reskinned = flotilla_resources::Selector::for_capability("review");
        reskinned.model = Some("sonnet".to_string());
        let requirement = table.resolve_selector(&reskinned).expect("model-only override");
        assert_eq!((requirement.adapter.as_str(), requirement.model.as_deref()), ("claude-code", Some("sonnet")));

        // With an explicit adapter the capability is template vocabulary only;
        // without one, unknown capabilities stay loud.
        let mut novel = flotilla_resources::Selector::for_capability("architect");
        novel.adapter = Some("claude-code".to_string());
        assert!(table.resolve_selector(&novel).is_ok());
        assert_eq!(
            table.resolve_selector(&flotilla_resources::Selector::for_capability("architect")).expect_err("unknown must fail"),
            "unknown agent capability `architect`"
        );
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
        let plan = codex
            .launch(&AgentLaunchRequest { role: "coder".into(), model: None, brief: brief.clone(), environment: Vec::new() })
            .expect("codex launch plan");
        assert_eq!(
            plan.command,
            "/tools/codex --dangerously-bypass-approvals-and-sandbox 'Read your crew brief at .flotilla/briefs/coder.md and follow it.'"
        );
        assert!(!plan.command.contains("Implement the issue"));
        assert_eq!(plan.stance, "trusted-implicit");

        let claude = registry.get("claude-code").expect("claude adapter");
        let plan = claude
            .launch(&AgentLaunchRequest {
                role: "reviewer".into(),
                model: Some("opus".into()),
                brief: brief.clone(),
                environment: Vec::new(),
            })
            .expect("claude launch plan");
        assert_eq!(
            plan.command,
            "/tools/claude --dangerously-skip-permissions --settings .flotilla/claude-settings.json --model 'opus' 'Read your crew brief at .flotilla/briefs/coder.md and follow it.'"
        );
        assert!(!plan.command.contains("Implement the issue"));

        // Admission rejects metacharacter models before they reach launch;
        // the sink still shell-quotes so a hostile model can never splice
        // into the command line even if that boundary regressed.
        let plan = claude
            .launch(&AgentLaunchRequest {
                role: "reviewer".into(),
                model: Some("opus; touch /tmp/pwned".into()),
                brief,
                environment: Vec::new(),
            })
            .expect("claude launch plan");
        assert!(plan.command.contains("--model 'opus; touch /tmp/pwned'"));
    }

    #[test]
    fn launch_plan_uses_the_discovered_absolute_binary_path() {
        let env = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/x/y/claude"));
        let registry = AgentAdapterRegistry::discover(&env, Arc::new(MockRunner::new(Vec::new())));
        let brief =
            flotilla_resources::TerminalBrief { path: ".flotilla/briefs/coder.md".into(), content: String::new(), copies: Vec::new() };

        let plan = registry
            .get("claude-code")
            .expect("claude adapter")
            .launch(&AgentLaunchRequest { role: "coder".into(), model: None, brief, environment: Vec::new() })
            .expect("launch plan");

        assert!(plan.command.starts_with("/x/y/claude "));
        assert!(!plan.command.starts_with("claude "));
    }

    #[tokio::test]
    async fn claude_prepare_writes_a_settings_overlay_the_launch_command_loads() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let claude_config = temp.path().join("claude-config");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&claude_config).expect("Claude config");
        let env = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("CLAUDE_CONFIG_DIR", claude_config.display().to_string()))
            .with(EnvironmentAssertion::binary("claude", "/tools/claude"));
        let registry = AgentAdapterRegistry::discover(&env, Arc::new(ProcessCommandRunner));
        let claude = registry.get("claude-code").expect("claude adapter");
        let brief =
            flotilla_resources::TerminalBrief { path: ".flotilla/briefs/coder.md".into(), content: "brief".into(), copies: Vec::new() };

        let invocation_environment = vec![("CLAUDE_CONFIG_DIR".to_string(), claude_config.display().to_string())];
        claude
            .prepare_with_environment(&ExecutionEnvironmentPath::new(&workspace), &brief, &invocation_environment)
            .await
            .expect("prepare claude workspace");

        // The launch command names this path relatively, so it must resolve
        // against the session's working directory.
        let plan = claude
            .launch(&AgentLaunchRequest { role: "coder".into(), model: None, brief: brief.clone(), environment: invocation_environment })
            .expect("launch plan");
        assert!(plan.command.contains("--settings .flotilla/claude-settings.json"), "{}", plan.command);
        assert!(
            plan.env.iter().any(|(name, value)| name == "CLAUDE_CONFIG_DIR" && value == &claude_config.display().to_string()),
            "the adapter launch plan must preserve explicit trusted config"
        );
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(workspace.join(".flotilla/claude-settings.json")).expect("read settings"))
                .expect("parse settings");
        assert_eq!(settings["hooks"]["SessionStart"][0]["hooks"][0]["command"], "flotilla hook claude-code session-start");
        assert_eq!(settings["hooks"]["Notification"][0]["matcher"], "permission_prompt");
        assert_eq!(settings["skipDangerousModePermissionPrompt"], true);

        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(claude_config.join(".claude.json")).expect("read Claude state"))
                .expect("parse Claude state");
        let canonical_workspace = workspace.canonicalize().expect("canonical workspace").display().to_string();
        assert_eq!(state["hasCompletedOnboarding"], true);
        assert_eq!(state["projects"][canonical_workspace]["hasTrustDialogAccepted"], true);

        claude.cleanup(&ExecutionEnvironmentPath::new(&workspace), &brief).await.expect("cleanup");
        assert!(!workspace.join(".flotilla/claude-settings.json").exists(), "settings overlay should be removed");
        assert!(!workspace.join(".flotilla").exists(), "settings overlay should not keep .flotilla alive");
    }

    #[tokio::test]
    async fn contained_claude_uses_the_seam_resolved_invocation_config() {
        let workspace = ExecutionEnvironmentPath::new("/workspace");
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new()), Ok("/workspace\n".to_string()), Err("not a checkout".to_string())]));
        let env = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("FLOTILLA_ENVIRONMENT_ID", "contained-claude"))
            .with(EnvironmentAssertion::binary("claude", "/tools/claude"));
        let registry = AgentAdapterRegistry::discover(&env, runner.clone());
        let claude = registry.get("claude-code").expect("Claude adapter");
        let brief =
            flotilla_resources::TerminalBrief { path: ".flotilla/briefs/coder.md".into(), content: "brief".into(), copies: Vec::new() };
        let invocation_environment = vec![
            ("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "redacted-test-token".to_string()),
            ("CLAUDE_CONFIG_DIR".to_string(), "/home/crew/flotilla/credentials/claude-max/claude".to_string()),
        ];

        let missing = claude
            .prepare_with_environment(&workspace, &brief, &Vec::new())
            .await
            .expect_err("contained Claude must refuse a session without delivered authentication");
        assert_eq!(missing, "contained Claude Code requires credential environment `CLAUDE_CODE_OAUTH_TOKEN`");

        claude.prepare_with_environment(&workspace, &brief, &invocation_environment).await.expect("prepare contained Claude");
        let plan = claude
            .launch(&AgentLaunchRequest { role: "coder".into(), model: None, brief, environment: invocation_environment })
            .expect("contained launch plan");

        assert!(plan.env.iter().any(|(name, value)| name == "CLAUDE_CODE_OAUTH_TOKEN" && value == "redacted-test-token"));
        assert!(plan
            .env
            .iter()
            .any(|(name, value)| name == "CLAUDE_CONFIG_DIR" && value == "/home/crew/flotilla/credentials/claude-max/claude"));
        assert!(runner
            .calls()
            .iter()
            .any(|(command, args)| { command == "mkdir" && args == &["-p", "/home/crew/flotilla/credentials/claude-max/claude"] }));
        assert!(runner.calls().iter().flat_map(|(_, args)| args).all(|arg| !arg.starts_with("/run/flotilla")));
    }

    #[tokio::test]
    async fn claude_seeds_headless_state_for_a_multi_repo_workspace() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace_root = temp.path().join("multi-repo-workspace");
        let claude_config = temp.path().join("claude-config");
        std::fs::create_dir_all(workspace_root.join("repo-a")).expect("repo a");
        std::fs::create_dir_all(workspace_root.join("repo-b")).expect("repo b");
        std::fs::create_dir_all(&claude_config).expect("Claude config");
        let env = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("CLAUDE_CONFIG_DIR", claude_config.display().to_string()))
            .with(EnvironmentAssertion::binary("claude", "/tools/claude"));
        let registry = AgentAdapterRegistry::discover(&env, Arc::new(ProcessCommandRunner));
        let brief =
            flotilla_resources::TerminalBrief { path: ".flotilla/briefs/coder.md".into(), content: "brief".into(), copies: Vec::new() };

        registry
            .get("claude-code")
            .expect("claude adapter")
            .prepare(&ExecutionEnvironmentPath::new(&workspace_root), &brief)
            .await
            .expect("prepare multi-repo workspace root");

        assert_eq!(std::fs::read_to_string(workspace_root.join(".flotilla/briefs/coder.md")).expect("brief"), "brief");
        assert!(workspace_root.join(".flotilla/claude-settings.json").exists());
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(claude_config.join(".claude.json")).expect("Claude state"))
                .expect("parse Claude state");
        let canonical_workspace = workspace_root.canonicalize().expect("canonical workspace").display().to_string();
        assert_eq!(state["hasCompletedOnboarding"], true);
        assert_eq!(state["projects"][canonical_workspace]["hasTrustDialogAccepted"], true);
    }

    #[tokio::test]
    async fn host_direct_claude_does_not_mutate_global_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&home).expect("home");
        let global_state = home.join(".claude.json");
        std::fs::write(&global_state, r#"{"existing":true}"#).expect("global Claude state");
        let env = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("HOME", home.display().to_string()))
            .with(EnvironmentAssertion::binary("claude", "/tools/claude"));
        let registry = AgentAdapterRegistry::discover(&env, Arc::new(ProcessCommandRunner));
        let brief =
            flotilla_resources::TerminalBrief { path: ".flotilla/briefs/coder.md".into(), content: "brief".into(), copies: Vec::new() };

        registry
            .get("claude-code")
            .expect("Claude adapter")
            .prepare(&ExecutionEnvironmentPath::new(&workspace), &brief)
            .await
            .expect("prepare host-direct Claude");

        assert_eq!(std::fs::read_to_string(global_state).expect("global Claude state"), r#"{"existing":true}"#);
    }

    #[test]
    fn host_direct_claude_oauth_uses_delivered_auth_without_replacing_ambient_config() {
        let env = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/tools/claude"));
        let registry = AgentAdapterRegistry::discover(&env, Arc::new(MockRunner::new(Vec::new())));
        let claude = registry.get("claude-code").expect("Claude adapter");
        let brief =
            flotilla_resources::TerminalBrief { path: ".flotilla/briefs/coder.md".into(), content: "brief".into(), copies: Vec::new() };
        let plan = claude
            .launch(&AgentLaunchRequest {
                role: "coder".into(),
                model: None,
                brief,
                environment: vec![
                    ("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "token-alice".to_string()),
                    ("CLAUDE_CONFIG_DIR".to_string(), "/state/flotilla/credentials/alice/claude".to_string()),
                ],
            })
            .expect("host-direct OAuth launch");

        assert!(plan.env.iter().any(|(name, value)| name == "CLAUDE_CODE_OAUTH_TOKEN" && value == "token-alice"));
        assert!(plan.env.iter().all(|(name, _)| name != "CLAUDE_CONFIG_DIR"), "ambient Claude config must remain selected");
    }

    #[test]
    fn claude_leaves_screen_classification_to_its_hook_path() {
        let registry = discovered_registry();
        let claude = registry.get("claude-code").expect("claude adapter");

        // Codex's prompt vocabulary must not be matched against Claude screens.
        assert_eq!(claude.classify_screen_attention("Do you trust the contents of this directory?"), None);
    }

    #[tokio::test]
    async fn codex_prepare_preserves_config_while_trusting_the_canonical_workspace() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).expect("codex home");
        std::fs::write(
            codex_home.join("config.toml"),
            "# keep this comment\nmodel = \"gpt-5.6-sol\"\n\n[projects.\"/existing\"]\ntrust_level = \"trusted\"\n",
        )
        .expect("initial Codex config");
        let workspace = temp.path().join(r#"quote"and\slash"#);
        std::fs::create_dir_all(&workspace).expect("workspace");
        let env = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("CODEX_HOME", codex_home.display().to_string()))
            .with(EnvironmentAssertion::binary("codex", "/tools/codex"));
        let registry = AgentAdapterRegistry::discover(&env, Arc::new(ProcessCommandRunner));
        let codex = registry.get("codex").expect("codex adapter");
        let brief =
            flotilla_resources::TerminalBrief { path: ".flotilla/briefs/coder.md".into(), content: String::new(), copies: Vec::new() };
        codex.prepare(&ExecutionEnvironmentPath::new(&workspace), &brief).await.expect("prepare Codex workspace");

        let config = std::fs::read_to_string(codex_home.join("config.toml")).expect("Codex config");
        assert!(config.contains("# keep this comment"));
        let parsed = config.parse::<DocumentMut>().expect("parse updated Codex config");
        let canonical_workspace = workspace.canonicalize().expect("canonical workspace").display().to_string();
        assert_eq!(parsed["model"].as_str(), Some("gpt-5.6-sol"));
        assert_eq!(parsed["projects"]["/existing"]["trust_level"].as_str(), Some("trusted"));
        assert_eq!(parsed["projects"][&canonical_workspace]["trust_level"].as_str(), Some("trusted"));
    }

    #[test]
    fn codex_classifies_interactive_trust_and_approval_prompts_as_needing_input() {
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");

        for screen in [
            "Do you trust the contents of this directory?\n› 1. Yes, continue\n  2. No, quit\n\nPress enter to continue",
            "Would you like to run the following command?\n\n$ cargo test\n\n› 1. Yes, proceed\n  2. No, tell Codex what to do differently",
            "Do you want to approve network access to \"github.com\"?\n\n› 1. Approve once\n  2. Deny",
            "Would you like to grant these permissions?\n\n› 1. Yes\n  2. No",
            "Would you like to make the following edits?\n\n› 1. Yes\n  2. No",
            "github needs your approval.\n\n› 1. Approve\n  2. Deny",
            "Question 1/2 (2 unanswered)\n\nWhich environment?\n\n› 1. Staging\n  2. Production\n\nenter to submit answer",
        ] {
            assert_eq!(codex.classify_screen_attention(screen), Some(TerminalAttentionState::NeedsInput), "{screen}");
        }
    }

    #[test]
    fn codex_does_not_treat_its_normal_composer_as_an_interactive_prompt() {
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");
        let screen = "• Working (57s • esc to interrupt)\n\n› Run /review on my current changes\n\ngpt-5.6-sol high · /workspace";

        assert_eq!(codex.classify_screen_attention(screen), None);
    }

    #[tokio::test]
    async fn prepare_excludes_flotilla_brief_from_git_status() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        assert!(ProcessCommand::new("git")
            .args(["init", "-q", repo.to_str().expect("utf-8 repo path")])
            .status()
            .expect("git init")
            .success());
        assert!(ProcessCommand::new("git")
            .args(["-C", repo.to_str().expect("utf-8 repo path"), "config", "user.email", "test@example.com"])
            .status()
            .expect("git config email")
            .success());
        assert!(ProcessCommand::new("git")
            .args(["-C", repo.to_str().expect("utf-8 repo path"), "config", "user.name", "Test"])
            .status()
            .expect("git config name")
            .success());
        std::fs::write(repo.join("README.md"), "hello\n").expect("write readme");
        assert!(ProcessCommand::new("git")
            .args(["-C", repo.to_str().expect("utf-8 repo path"), "add", "README.md"])
            .status()
            .expect("git add")
            .success());
        assert!(ProcessCommand::new("git")
            .args(["-C", repo.to_str().expect("utf-8 repo path"), "commit", "-q", "-m", "init"])
            .status()
            .expect("git commit")
            .success());

        let env = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("CODEX_HOME", temp.path().join("codex-home").display().to_string()))
            .with(EnvironmentAssertion::binary("codex", "/tools/codex"));
        let registry = AgentAdapterRegistry::discover(&env, Arc::new(ProcessCommandRunner));
        let brief = flotilla_resources::TerminalBrief {
            path: ".flotilla/briefs/coder.md".into(),
            content: "secret assignment".into(),
            copies: Vec::new(),
        };
        registry
            .get("codex")
            .expect("codex adapter")
            .prepare(&ExecutionEnvironmentPath::new(repo.to_str().expect("utf-8 repo path")), &brief)
            .await
            .expect("prepare brief");

        let status = ProcessCommand::new("git")
            .args(["-C", repo.to_str().expect("utf-8 repo path"), "status", "--short"])
            .output()
            .expect("git status");
        assert!(status.status.success());
        assert_eq!(String::from_utf8(status.stdout).expect("utf-8 status"), "");
        assert_eq!(std::fs::read_to_string(repo.join(".git/info/exclude")).expect("read exclude").matches(".flotilla/").count(), 1);

        registry
            .get("codex")
            .expect("codex adapter")
            .cleanup(&ExecutionEnvironmentPath::new(repo.to_str().expect("utf-8 repo path")), &brief)
            .await
            .expect("cleanup brief");
        assert!(!repo.join(".flotilla/briefs/coder.md").exists(), "brief file should be removed");
        assert!(!repo.join(".flotilla/briefs").exists(), "empty briefs directory should be removed");
    }
}
