use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_protocol::arg::{flatten, Arg};
use flotilla_resources::{account_access::AccessName, Convoy, FulfilmentGrant, ResourceObject, TerminalAttentionState, TerminalBrief};
use serde::Serialize;
use tokio::sync::Mutex;
use toml_edit::{value, DocumentMut, Item, Table};

use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::execution_context::AccountState;
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;
use flotilla_terminal::TerminalEnvVars;

mod fleet_canary;

/// Shared delimiter for the appended card and oversized-brief fallback.
pub const CAPABILITIES_HEADING: &str = "\n\n## Your capabilities";
pub const TRUSTED_IMPLICIT_STANCE: &str = "trusted-implicit";
pub const DEFAULT_CREW_BRIEF_TEMPLATE: &str = "crew.md";
const BUILTIN_CREW_BRIEF_TEMPLATE: &str = include_str!("adapter/templates/crew.md");
const BUILTIN_INTERACTIVE_SESSION_BRIEF_TEMPLATE: &str = include_str!("adapter/templates/interactive-session.md");
const BUILTIN_DIFF_REVIEW_BRIEF_TEMPLATE: &str = include_str!("adapter/templates/diff-review.md");
const BUILTIN_SHEPHERD_BRIEF_TEMPLATE: &str = include_str!("adapter/templates/shepherd.md");
const BUILTIN_FORK_STANCE_BRIEF_LAYER: &str = include_str!("adapter/templates/fork-stance.md");
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
        CrewBriefRenderOptions { template: template.to_string(), overrides, fork_stance, ..CrewBriefRenderOptions::default() }
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
    pub declared_template: Option<String>,
    pub charter_prose: String,
    pub charter_commit: Option<String>,
    pub template: String,
    pub overrides: Vec<CrewBriefTemplateOverride>,
    pub fork_stance: bool,
    pub has_credential_scope: bool,
    pub is_standing: bool,
}

impl CrewBriefRenderOptions {
    pub fn apply_cascade(&mut self, cascade: Option<&flotilla_resources::ResolvedCascade>, role: &str) {
        let Some(cascade) = cascade else { return };
        self.declared_template = cascade.roles.get(role).and_then(|definition| definition.brief_template.clone());
        self.charter_prose = cascade
            .charter
            .get("*")
            .into_iter()
            .chain((role != "*").then(|| cascade.charter.get(role)).flatten())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n");
        self.charter_commit = cascade.charter_commit.clone();
        // Declared templates are authoritative over checkout/config overrides.
        if self.declared_template.is_some() {
            self.overrides.clear();
        }
    }

    /// The pinned exit declaration is the same signal used by convoy exit
    /// instantiation: an absent declaration keeps the convoy standing.
    fn for_convoy(mut self, convoy: &ResourceObject<Convoy>) -> Self {
        self.is_standing = convoy.metadata.annotations.contains_key(flotilla_resources::ENSURED_FROM_ANNOTATION)
            || convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).is_some_and(|snapshot| snapshot.exit.is_none());
        self
    }
}

impl Default for CrewBriefRenderOptions {
    fn default() -> Self {
        Self {
            declared_template: None,
            charter_prose: String::new(),
            charter_commit: None,
            template: DEFAULT_CREW_BRIEF_TEMPLATE.to_string(),
            overrides: Vec::new(),
            fork_stance: false,
            has_credential_scope: false,
            is_standing: false,
        }
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
    charter_prose: &'a str,
    charter_commit: Option<&'a str>,
    members: &'a [CrewBriefMember],
    handoff_members: Vec<&'a CrewBriefMember>,
    has_credential_scope: bool,
    has_in_crew_reviewer: bool,
    is_standing: bool,
}

#[cfg(test)]
fn build_crew_brief(
    context: &flotilla_resources::TerminalCrewContext,
    vessel: &str,
    role: &str,
    assignment: CrewAssignment<'_>,
    members: &[CrewBriefMember],
) -> flotilla_resources::TerminalBrief {
    build_crew_brief_with_options(context, vessel, role, assignment, members, &CrewBriefRenderOptions::default())
        .expect("built-in crew brief template should render")
}

fn build_crew_brief_with_options(
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
        CrewAssignment::Unassigned if options.is_standing =>
            "No task was provided for this turn. Check `## Human instruction` below if present; otherwise yield at the turn boundary and wait for work.",
        CrewAssignment::Unassigned =>
            "No assignment was provided with this dispatch. Check `## Human instruction` below if present; otherwise run `flotilla crew stall --reason decision --message 'No assignment was provided'` rather than inventing work.",
    };
    let mut content = render_crew_brief_template(
        options,
        &CrewBriefTemplateContext {
            role,
            convoy: &context.convoy,
            vessel,
            vessel_ref: &context.vessel_ref,
            assignment_text,
            charter_prose: &options.charter_prose,
            charter_commit: options.charter_commit.as_deref(),
            members,
            handoff_members: members.iter().filter(|member| member.is_agent && member.role != role).collect(),
            has_credential_scope: options.has_credential_scope,
            has_in_crew_reviewer: members.iter().any(|member| member.is_agent && member.role == "reviewer"),
            is_standing: options.is_standing,
        },
    )?;
    if !options.charter_prose.is_empty() || options.charter_commit.is_some() {
        content.push_str("\n\n## Project charter\n\n");
        if let Some(commit) = &options.charter_commit {
            content.push_str(&format!("Charter revision: `{commit}`\n\n"));
        }
        content.push_str(&options.charter_prose);
    }
    if !content.ends_with('\n') {
        content.push('\n');
    }
    Ok(flotilla_resources::TerminalBrief { artifact_digest: None, path: crew_brief_path(role), content, copies: Vec::new() })
}

/// Production entry point: deriving the standing variant from the convoy here
/// keeps call sites from accidentally rendering a dispatched brief for one.
pub fn build_convoy_crew_brief_with_options(
    convoy: &ResourceObject<Convoy>,
    context: &flotilla_resources::TerminalCrewContext,
    vessel: &str,
    role: &str,
    assignment: CrewAssignment<'_>,
    members: &[CrewBriefMember],
    options: &CrewBriefRenderOptions,
) -> Result<flotilla_resources::TerminalBrief, String> {
    build_crew_brief_with_options(context, vessel, role, assignment, members, &options.clone().for_convoy(convoy))
}

fn render_crew_brief_template(options: &CrewBriefRenderOptions, context: &CrewBriefTemplateContext<'_>) -> Result<String, String> {
    let mut env = minijinja::Environment::new();
    // Demote only the document title; preserve the fenced artifact heading verbatim.
    env.add_global(
        "decision_ledger_guidance",
        include_str!("../../../docs/agents/decision-ledger.md").replacen("# Decision ledger\n", "### Ledger template and guidance\n", 1),
    );
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
    if let Some(source) = &options.declared_template {
        let name = "declared/role.md";
        env.add_template_owned(name.to_string(), layered_override_source(&current_template, source))
            .map_err(|err| format!("load declared role brief template: {err}"))?;
        current_template = name.to_string();
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
    if let Some(project_ref) = &convoy.spec.project_ref {
        content.push_str(&format!("- Island Project: `{project_ref}` — resolve current repositories and roles with `flotilla crew list` on each orientation sweep.\n"));
    }
    // Project-scoped governors orient against live membership. Delivery crews
    // retain admitted repository URLs and target refs as their delivery contract.
    if convoy.spec.role != "governor" || convoy.spec.project_ref.is_none() {
        content.push_str("- Repositories:\n");
        for repository in convoy.spec.repositories.iter().filter(|repository| repository_refs.contains(&repository.repo_ref)) {
            content.push_str(&format!("  - `{}` — {} (target `{}`)\n", repository.repo_ref, repository.url, repository.target_ref));
        }
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
    /// Grants of the selected fulfilment kind; standalone launches may omit it.
    pub fulfilment_grants: Option<BTreeSet<FulfilmentGrant>>,
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
    /// The account access the item declares in its registration (#3907),
    /// resolved through grants into a per-requester binding.
    account_access: Option<AccessName>,
    /// Authenticates only through an ambient host login. No seeded adapter
    /// uses this yet; admission is forward-provisioned for one.
    ambient_scope: Option<&'static str>,
}

const CLAUDE_CODE_ADAPTER_ID: &str = "claude-code";

/// The requester's bound Claude home and its token file. A Claude harness without both is
/// refused: nothing falls through to `~/.claude` or an ambient token (#3907 contract 8).
fn bound_claude_home(environment: &TerminalEnvVars) -> Result<(&str, &str), String> {
    let lookup = |key: &str| environment.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str());
    match (lookup("CLAUDE_CONFIG_DIR"), lookup(crate::account_access::CLAUDE_OAUTH_TOKEN_FILE_ENV)) {
        (Some(home), Some(token_file)) if Path::new(home).is_absolute() && Path::new(token_file).is_absolute() => Ok((home, token_file)),
        _ => Err(
            "Claude Code requires the requester's bound account home: no account binding delivered `CLAUDE_CONFIG_DIR` and its token file"
                .to_string(),
        ),
    }
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

    /// Add or replace the requirement a capability resolves to.
    pub fn with_requirement(mut self, capability: &str, requirement: AgentRequirement) -> Self {
        self.requirements.insert(capability.to_string(), requirement);
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
            account_access: seeded.account_access.clone(),
            ambient_scope: seeded.ambient_scope,
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
        let account_access = crate::account_access::declared_account_access(&adapter);
        Self { adapter, model, account_access, ambient_scope: None }
    }

    /// A requirement for an adapter that has no deliverable credential form and
    /// authenticates only through the named ambient host login scope.
    pub fn ambient_only(adapter: impl Into<String>, model: Option<String>, scope: &'static str) -> Self {
        Self { adapter: adapter.into(), model, account_access: None, ambient_scope: Some(scope) }
    }

    /// The account access this item declares. A session for it starts only
    /// in the explicit home its binding delivers; it never falls through to
    /// ambient or interactive login.
    pub fn account_access(&self) -> Option<&AccessName> {
        self.account_access.as_ref()
    }

    /// Ambient-login scope this adapter authenticates through, named as it
    /// appears under the Host `credential_expiry` capability. `None` for
    /// adapters with no ambient authentication path.
    pub fn ambient_credential_scope(&self) -> Option<&'static str> {
        self.ambient_scope
    }
}

impl Default for CapabilityTable {
    fn default() -> Self {
        Self::seeded()
    }
}

/// Minimum CLI version required by the launch flags, available without local discovery.
pub fn minimum_harness_version(adapter: &str) -> Option<&'static str> {
    match adapter {
        // --no-daemon is part of every managed Codex invocation.
        "codex" => Some("0.160.0"),
        _ => None,
    }
}

#[async_trait]
pub trait AgentAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    /// Return a fatal launch diagnostic; ambiguous exits remain resumable.
    fn classify_exit_failure(&self, _exit_code: i32, _screen: &str) -> Option<String> {
        None
    }
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
    /// Managed crew startup uses this non-null VCS path to guarantee runtime
    /// file exclusion. The terminal controller also uses it for brief copies.
    async fn prepare_with_vcs(
        &self,
        cwd: &ExecutionEnvironmentPath,
        brief: &TerminalBrief,
        environment: &TerminalEnvVars,
        _vcs: &dyn flotilla_vcs::vcs::Vcs,
    ) -> Result<(), String> {
        self.prepare_with_environment(cwd, brief, environment).await
    }
    async fn cleanup(&self, _cwd: &ExecutionEnvironmentPath, _brief: &TerminalBrief) -> Result<(), String> {
        Ok(())
    }
    fn deliver_brief(&self, brief: &TerminalBrief) -> String {
        // Linux limits an individual exec argument to 128 KiB, including its NUL.
        // Both Claude and Codex receive the prompt as one argument. Leave room
        // for UTF-8, quoting and harness additions within a conservative 64 KiB.
        let prompt = format!("{}\n\nThis is also at `{}`; re-read that file after a context compaction.", brief.content, brief.path);
        if flotilla_protocol::arg::shell_quote(&prompt).len() <= 64 * 1024 {
            prompt
        } else {
            let card = brief
                .content
                .rsplit_once(CAPABILITIES_HEADING)
                .map(|(_, card)| format!("{CAPABILITIES_HEADING}{card}"))
                .unwrap_or_default();
            let fallback =
                format!("Read your crew brief at {} and follow it. Re-read that file after a context compaction.{}", brief.path, card);
            if flotilla_protocol::arg::shell_quote(&fallback).len() <= 64 * 1024 {
                fallback
            } else {
                format!("Read your crew brief at {} and follow it. Re-read that file after a context compaction. Run `flotilla crew capabilities` for your live capabilities card.", brief.path)
            }
        }
    }
    fn classify_screen_attention(&self, _screen: &str) -> Option<TerminalAttentionState> {
        None
    }
    /// Why a screen that reads `Unobservable` shows an agent that is idle but
    /// cannot take a turn, so attention can name it (#3460). Never changes the
    /// attention state: delivery still refuses.
    fn classify_screen_obstruction(&self, _screen: &str) -> Option<flotilla_resources::ScreenObstruction> {
        None
    }
    /// Optional meaningful-output fingerprint, excluding harness animation.
    fn screen_output_digest(&self, _screen: &str) -> Option<String> {
        None
    }
    fn classify_screen_failure(&self, _screen: &str) -> Option<&'static str> {
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
    ClaudeCode { state_config: Option<ClaudeStateConfig>, state_lock: Arc<Mutex<()>> },
    /// Codex gates on a persisted per-project trust level, so the workspace has
    /// to be marked trusted in its config before launch.
    Codex { trust_config: Option<CodexTrustConfig>, trust_lock: Arc<Mutex<()>> },
}

/// Names the flotilla environment a session runs in. Docker vessels and
/// agentless hosts with daemon reach applied carry it; a host-direct session on
/// the daemon's own host does not.
pub const FLOTILLA_ENVIRONMENT_ID_ENV: &str = "FLOTILLA_ENVIRONMENT_ID";

/// Containment is a property of the session, not of the host it was
/// discovered on (#3655): an agentless host's discovery bag is its login
/// environment, which never names a flotilla environment, while its sessions
/// do. A contained session gets a flotilla-owned harness home; the host user's
/// own harness configuration is never selected or written.
fn session_is_contained(environment: &TerminalEnvVars) -> bool {
    environment.iter().any(|(name, _)| name == FLOTILLA_ENVIRONMENT_ID_ENV)
}

impl AdapterFlavor {
    fn id(&self) -> &'static str {
        match self {
            Self::ClaudeCode { .. } => CLAUDE_CODE_ADAPTER_ID,
            Self::Codex { .. } => "codex",
        }
    }

    fn autonomy_args(&self, grants: Option<&BTreeSet<FulfilmentGrant>>) -> &'static [&'static str] {
        let may_run_unattended = grants.is_none_or(|grants| {
            grants.contains(&FulfilmentGrant::host_account_reach()) || grants.contains(&FulfilmentGrant::network("scoped".to_string()))
        });
        if !may_run_unattended {
            return &[];
        }
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
        args.extend(self.flavor.autonomy_args(request.fulfilment_grants.as_ref()).iter().map(|arg| Arg::Literal((*arg).into())));
        if matches!(&self.flavor, AdapterFlavor::ClaudeCode { .. }) {
            args.extend([Arg::Literal("--settings".into()), Arg::Literal(CLAUDE_MANAGED_SETTINGS_PATH.into())]);
        }
        if matches!(&self.flavor, AdapterFlavor::Codex { .. }) {
            args.push(Arg::Literal("--no-daemon".into()));
            for override_value in crate::codex_hook_overrides() {
                args.extend([Arg::Literal("-c".into()), Arg::Quoted(override_value)]);
            }
        }
        if let Some(model) = &request.model {
            args.extend([Arg::Literal("--model".into()), Arg::Quoted(model.clone())]);
        }
        args.push(Arg::Quoted(self.deliver_brief(&request.brief)));
        flatten(&args, 0)
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
        self.prepare_impl(cwd, brief, environment, None).await
    }

    async fn prepare_with_vcs(
        &self,
        cwd: &ExecutionEnvironmentPath,
        brief: &TerminalBrief,
        environment: &TerminalEnvVars,
        vcs: &dyn flotilla_vcs::vcs::Vcs,
    ) -> Result<(), String> {
        self.prepare_impl(cwd, brief, environment, Some(vcs)).await
    }
    async fn cleanup(&self, cwd: &ExecutionEnvironmentPath, brief: &TerminalBrief) -> Result<(), String> {
        remove_agent_files(&*self.runner, cwd.as_path(), brief, self.flavor.managed_files()).await
    }

    fn classify_screen_attention(&self, screen: &str) -> Option<TerminalAttentionState> {
        match &self.flavor {
            // Claude Code reports its own permission prompts through the hook
            // path, which is more precise than matching rendered text.
            AdapterFlavor::ClaudeCode { .. } => None,
            AdapterFlavor::Codex { .. } => Some(if codex_screen_needs_input(screen) {
                TerminalAttentionState::NeedsInput
            } else if screen.lines().any(codex_working_line) {
                TerminalAttentionState::Working
            } else if codex_composer_visible(screen) {
                TerminalAttentionState::Idle
            } else {
                TerminalAttentionState::Unobservable
            }),
        }
    }

    fn classify_screen_obstruction(&self, screen: &str) -> Option<flotilla_resources::ScreenObstruction> {
        match &self.flavor {
            AdapterFlavor::ClaudeCode { .. } => None,
            AdapterFlavor::Codex { .. } => {
                if codex_hook_review(screen) {
                    return Some(flotilla_resources::ScreenObstruction::HookReview);
                }
                (self.classify_screen_attention(screen) == Some(TerminalAttentionState::Unobservable)).then(|| codex_obstruction(screen))?
            }
        }
    }

    fn screen_output_digest(&self, screen: &str) -> Option<String> {
        use sha2::{Digest, Sha256};
        match self.flavor {
            AdapterFlavor::ClaudeCode { .. } => None,
            AdapterFlavor::Codex { .. } => {
                let output = screen.lines().filter(|line| !codex_working_line(line)).collect::<Vec<_>>().join("\n");
                Some(format!("{:x}", Sha256::digest(output.as_bytes())))
            }
        }
    }

    fn classify_exit_failure(&self, exit_code: i32, screen: &str) -> Option<String> {
        if exit_code == 0 {
            return None;
        }
        match self.flavor {
            AdapterFlavor::ClaudeCode { .. } if exit_code == 1 => match screen.trim() {
                "Not logged in · Please run /login" => {
                    Some("Claude authentication refused at launch: not logged in; check the selected credential".into())
                }
                "Login expired · Please run /login" => {
                    Some("Claude authentication refused at launch: login expired; check the selected credential".into())
                }
                _ => None,
            },
            AdapterFlavor::Codex { .. } if exit_code == 2 => {
                // Only a standalone observed CLI usage diagnostic is conclusive.
                // Scrollback, user prompts, and quoted diagnostics are ambiguous.
                let diagnostic = screen.trim();
                (diagnostic == "error: unexpected argument '--no-daemon' found")
                    .then(|| format!("Codex launch usage error (exit 2): {diagnostic}"))
            }
            AdapterFlavor::Codex { .. } if exit_code == 1 => codex_auth_failure(screen)
                .map(|reason| format!("Codex authentication refused at launch: {reason}; check the selected credential")),
            _ => None,
        }
    }

    fn classify_screen_failure(&self, screen: &str) -> Option<&'static str> {
        match &self.flavor {
            AdapterFlavor::ClaudeCode { .. } => None,
            AdapterFlavor::Codex { .. } => codex_auth_failure(screen),
        }
    }

    fn launch(&self, request: &AgentLaunchRequest) -> Result<AgentLaunchPlan, String> {
        let mut env = request.environment.clone();
        let claude = matches!(&self.flavor, AdapterFlavor::ClaudeCode { .. });
        if claude {
            env.retain(|(name, _)| name != "CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION");
            env.push(("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION".to_string(), "false".to_string()));
        }
        let mut command = self.command(request);
        if claude {
            // #3907: the harness home is always the requester's bound home, and
            // the plan's token file is its only token source.
            let token_file = bound_claude_home(&env)?.1.to_string();
            // Other token sources would compete with the plan's own (#3882 "Unknown precedence
            // stays unknown"), so a bound launch clears the family's declared secrets.
            env.retain(|(name, _)| !CLAUDE_CODE_ACCOUNT_STATE.secrets.contains(&name.as_str()));
            command = format!("{} {command}", crate::account_access::claude_token_prefix(&token_file));
        }
        Ok(AgentLaunchPlan { command, env, stance: TRUSTED_IMPLICIT_STANCE.into() })
    }
}

impl CliAgentAdapter {
    async fn prepare_impl(
        &self,
        cwd: &ExecutionEnvironmentPath,
        brief: &TerminalBrief,
        environment: &TerminalEnvVars,
        vcs: Option<&dyn flotilla_vcs::vcs::Vcs>,
    ) -> Result<(), String> {
        // Install the exclusion before writing any runtime files or launching an agent.
        if let Some(vcs) = vcs {
            let runtime_paths = std::iter::once(brief.path.as_str()).chain(self.flavor.managed_files().iter().copied()).collect::<Vec<_>>();
            ensure_flotilla_git_exclude(&*self.runner, vcs, cwd.as_path(), &runtime_paths).await?;
        }
        match &self.flavor {
            AdapterFlavor::ClaudeCode { state_config, state_lock } => {
                let contained = session_is_contained(environment);
                let config_dir = PathBuf::from(bound_claude_home(environment)?.0);
                let config_dir_string = config_dir.display().to_string();
                self.runner.run("mkdir", &["-p", &config_dir_string], Path::new("/"), &ChannelLabel::Default).await?;
                let invocation_state = Some(ClaudeStateConfig { path: config_dir.join(".claude.json"), lock: Arc::clone(state_lock) });
                // A contained session never falls back to the discovered
                // (host user's) state file: it is seeded only in its own home.
                let host_state = state_config.as_ref().filter(|_| !contained);
                if let Some(state_config) = invocation_state.as_ref().or(host_state) {
                    seed_claude_headless_state(&*self.runner, cwd.as_path(), state_config).await?;
                }
                // The runtime supplies a hook URL and token only to host-direct sessions
                // while flotillad has a loopback listener; vessel sessions keep commands.
                let mut settings = crate::claude_code_hook_settings_over(crate::ClaudeCodeHookTransport::from_environment(environment));
                settings["skipDangerousModePermissionPrompt"] = serde_json::Value::Bool(true);
                settings["promptSuggestionEnabled"] = serde_json::Value::Bool(false);
                if contained {
                    settings["attribution"] = serde_json::json!({ "commit": "", "pr": "", "sessionUrl": false });
                    settings["includeCoAuthoredBy"] = serde_json::Value::Bool(false);
                }
                let settings =
                    serde_json::to_string_pretty(&settings).map_err(|error| format!("render Claude Code settings overlay: {error}"))?;
                self.runner.write_file(&cwd.as_path().join(CLAUDE_MANAGED_SETTINGS_PATH), &settings).await?;
            }
            AdapterFlavor::Codex { trust_config, trust_lock } => {
                let invocation_config = environment
                    .iter()
                    .find(|(name, _)| name == "CODEX_HOME")
                    .map(|(_, home)| CodexTrustConfig { path: PathBuf::from(home).join("config.toml"), lock: Arc::clone(trust_lock) });
                let config = invocation_config
                    .as_ref()
                    .or(trust_config.as_ref())
                    .ok_or_else(|| "cannot determine Codex config path because neither CODEX_HOME nor HOME was detected".to_string())?;
                seed_codex_workspace_trust(&*self.runner, cwd.as_path(), config, session_is_contained(environment)).await?;
            }
        }
        self.runner.write_file(&cwd.as_path().join(&brief.path), &brief.content).await?;
        Ok(())
    }
}

fn codex_working_line(line: &str) -> bool {
    line.trim_start().trim_start_matches('•').trim_start().starts_with("Working (") && line.contains("esc to interrupt")
}

// Codex rust-v0.160.0: codex-rs/tui/src/chatwidget.rs, PLACEHOLDER and
// SIDE_PLACEHOLDER (selected by chatwidget/side.rs). The cleat capture path
// supplies plain text, not cells or the dim attribute used for placeholders.
const CODEX_COMPOSER_PLACEHOLDERS: &[&str] = &["Ask Codex to do anything", "Ask a follow-up question"];

fn codex_composer_visible(screen: &str) -> bool {
    let lines = screen.lines().map(str::trim).collect::<Vec<_>>();
    if lines.iter().any(|line| line.starts_with("Select ") || line.starts_with("Choose ")) {
        return false;
    }
    // Submitted prompts remain visible above the composer. An attach can
    // resize the VT and expose more of that history; inspect the last row.
    let mut rows = lines.iter().enumerate().rev().filter_map(|(index, line)| line.strip_prefix('›').map(|text| (index, text.trim())));
    let Some((index, text)) = rows.next() else { return false };
    if text.starts_with(|character: char| character.is_ascii_digit()) {
        return false;
    }
    let trailing = &lines[index + 1..];
    // The model footer (`model · /path …` or `model · ~/path …`) delimits the
    // composer. Rows below it are chrome: wrapped status items, the
    // `? for shortcuts` hint, and right-aligned warnings. Only rows between the
    // composer row and the footer can hold wrapped or multiline draft text.
    let footer = trailing
        .iter()
        .rposition(|line| !line.is_empty() && !codex_footer_chrome(line))
        .filter(|&index| trailing[index].contains(" · /") || trailing[index].contains(" · ~"));
    if trailing[..footer.unwrap_or(trailing.len())].iter().any(|line| !line.is_empty() && !codex_background_terminal_footer(line)) {
        return false;
    }
    // A footer identifies a live blank composer, but never makes draft text
    // idle. Exact known placeholders also work on captures cropped above it.
    let idle = (text.is_empty() && footer.is_some()) || CODEX_COMPOSER_PLACEHOLDERS.contains(&text);
    if !idle && !text.is_empty() {
        tracing::debug!(codex_version = "rust-v0.160.0", "unrecognised Codex composer text; delivery remains blocked");
    }
    idle
}

/// Names why an `Unobservable` Codex screen cannot take a turn (#3460). A
/// scrolled-back transcript ("↓ Back to bottom") comes first: its composer
/// may not be the last prompt row. Otherwise the live composer's draft text,
/// except a selection row, is named with whether it looks like a stray
/// terminal reply.
///
/// Precondition: the caller has already classified `screen` as
/// `Unobservable`. The "Back to bottom" and last-`›`-row checks would match
/// transcript text on screens the attention classifier reads otherwise.
fn codex_obstruction(screen: &str) -> Option<flotilla_resources::ScreenObstruction> {
    let lines = screen.lines().map(str::trim).collect::<Vec<_>>();
    if lines.iter().any(|line| line.contains("Back to bottom")) {
        return Some(flotilla_resources::ScreenObstruction::ScrolledBack);
    }
    if lines.iter().any(|line| line.starts_with("Select ") || line.starts_with("Choose ")) {
        return None;
    }
    let text = lines.iter().rev().find_map(|line| line.strip_prefix('›')).map(str::trim)?;
    // A selection row is a digit run then ". " (`› 1. Yes`), not a draft that starts with a number.
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if text.is_empty() || (digits > 0 && text[digits..].starts_with(". ")) || CODEX_COMPOSER_PLACEHOLDERS.contains(&text) {
        return None;
    }
    Some(flotilla_resources::ScreenObstruction::ComposerText {
        reply: flotilla_resources::looks_like_terminal_reply(text).then(|| text.to_string()),
        chars: text.chars().count(),
    })
}

fn codex_footer_chrome(line: &str) -> bool {
    // rust-v0.160.0 rows beneath the model footer: the shortcuts hint, which may
    // share its row with a right-aligned warning, and wrapped warning rows.
    // #2927 captured scrollback footer: keep the complete hint so a draft
    // beginning with "New activity" cannot be mistaken for footer chrome.
    line.starts_with("? for shortcuts") || line.starts_with('⚠') || line == "New activity · enter/esc latest · ? shortcuts"
}

fn codex_background_terminal_footer(line: &str) -> bool {
    // rust-v0.160.0: bottom_pane/unified_exec_footer.rs. Background sessions
    // render above the model footer and do not make the foreground turn busy.
    let Some((count, status)) = line.split_once(' ') else { return false };
    !count.is_empty()
        && count.chars().all(|character| character.is_ascii_digit())
        && (status.starts_with("background terminal running · /ps") || status.starts_with("background terminals running · /ps"))
}

// Codex rust-v0.160.0 tui/src/startup_hooks_review.rs and
// tui/src/bottom_pane/hooks_browser_view.rs. Match visible headings with
// interactive footer chrome so quoted hook output does not become a prompt.
fn codex_hook_review(screen: &str) -> bool {
    let lines = screen.lines().map(str::trim).collect::<Vec<_>>();
    (lines.contains(&"Hooks need review") && screen.contains("Review hooks") && screen.contains("Trust all and continue"))
        || (lines.iter().any(|line| *line == "Hooks" || line.ends_with(" hooks"))
            && lines.iter().any(|line| line.contains("enter review · esc close") || line.ends_with("esc back")))
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
    codex_hook_review(screen) || approval_prompt || user_question
}

fn codex_auth_failure(screen: &str) -> Option<&'static str> {
    match screen.trim() {
        "codex_apps: HTTP 401 TOKEN_EXPIRED"
        | "codex_apps failed: HTTP 401 token_expired\nYour access token could not be refreshed. Please log out and sign in again." => {
            Some("token_expired")
        }
        "Your access token could not be refreshed. Please log out and sign in again." => Some("access token could not be refreshed"),
        _ => None,
    }
}

async fn seed_codex_workspace_trust(
    runner: &dyn CommandRunner,
    cwd: &Path,
    config: &CodexTrustConfig,
    contained: bool,
) -> Result<(), String> {
    let _guard = config.lock.lock().await;
    let output = runner
        .run_output("pwd", &["-P"], cwd, &ChannelLabel::Default)
        .await
        .map_err(|error| format!("resolve canonical Codex workspace {}: {error}", cwd.display()))?;
    if !output.success() {
        return Err(format!("resolve canonical Codex workspace {}: {}", cwd.display(), output.stderr.trim()));
    }
    let canonical_cwd = output.stdout.trim();
    if canonical_cwd.is_empty() {
        return Err(format!("resolve canonical Codex workspace {}: `pwd -P` returned an empty path", cwd.display()));
    }
    let source = runner.ensure_file(&config.path, "").await?;
    let mut document = source.parse::<DocumentMut>().map_err(|error| format!("parse Codex config {}: {error}", config.path.display()))?;
    if contained {
        // Invocation overrides are the single owner of crew hooks. Remove the
        // previous generation's home definitions so Codex does not discover
        // and execute both layers (#3823). Preserve unrelated user hooks.
        // Empty groups intentionally retain matcher indices for user trust;
        // features.hooks stays enabled for hooks owned by other integrations.
        crate::update_codex_hooks(&mut document, false, &config.path)?;
    }
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
    project["trust_level"] = value("trusted");
    let updated = document.to_string();
    if updated == source {
        return Ok(());
    }
    runner.write_file(&config.path, &updated).await
}

async fn seed_claude_headless_state(runner: &dyn CommandRunner, cwd: &Path, config: &ClaudeStateConfig) -> Result<(), String> {
    let _guard = config.lock.lock().await;
    let output = runner
        .run_output("pwd", &["-P"], cwd, &ChannelLabel::Default)
        .await
        .map_err(|error| format!("resolve canonical Claude workspace {}: {error}", cwd.display()))?;
    if !output.success() {
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

/// Exclude `.flotilla/` and prove that every runtime file the adapter writes is
/// ignored. The proof checks those files, not the directory: a repository may
/// track its own `.flotilla/` declarations (Git never reports a directory that
/// holds tracked files as ignored), while the runtime files must stay untracked.
async fn ensure_flotilla_git_exclude(
    runner: &dyn CommandRunner,
    vcs: &dyn flotilla_vcs::vcs::Vcs,
    cwd: &Path,
    runtime_paths: &[&str],
) -> Result<(), String> {
    // A plain directory (a multi-repository workspace root) has nothing that
    // can be committed; each repository the brief is copied into is prepared
    // separately and proves its own exclusion.
    if !vcs.inside_work_tree().await? {
        return Ok(());
    }
    let exclude_path = vcs
        .exclude_file_path()
        .await?
        .ok_or_else(|| "cannot guarantee .flotilla/ runtime-file exclusion: checkout has no exclude path".to_string())?;

    let script = format!(
        "set -eu; exclude={}; mkdir -p \"$(dirname \"$exclude\")\"; touch \"$exclude\"; [ \"$(tail -n 1 \"$exclude\")\" = '.flotilla/' ] || {{ [ -z \"$(tail -c 1 \"$exclude\")\" ] || printf '\\n' >> \"$exclude\"; printf '%s\\n' '.flotilla/' >> \"$exclude\"; }}",
        flotilla_protocol::arg::shell_quote(&exclude_path.to_string_lossy()),
    );
    runner
        .run("sh", &["-c", &script], cwd, &ChannelLabel::Default)
        .await
        .map_err(|error| format!("cannot install .flotilla/ runtime-file exclusion: {error}"))?;
    for runtime_path in runtime_paths {
        if !vcs.path_is_ignored(Path::new(runtime_path)).await? {
            return Err(format!(
                "cannot guarantee .flotilla/ runtime-file exclusion: checkout ignore rules override it for {runtime_path}"
            ));
        }
    }
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

/// Contributes an adapter beyond the built-in harnesses when an environment's
/// discovery bag selects it. The composition root installs these; discovery
/// probes them in every environment beside the built-in detection.
pub trait AgentAdapterFactory: Send + Sync {
    fn probe(&self, env: &EnvironmentBag, runner: Arc<dyn CommandRunner>) -> Option<Arc<dyn AgentAdapter>>;

    /// The variables the contributed harness keeps its account state in.
    fn account_state(&self) -> Option<AccountState> {
        None
    }
}

/// Claude Code's login and configuration home, and the variables that
/// authenticate it directly.
pub const CLAUDE_CODE_ACCOUNT_STATE: AccountState = AccountState::new("claude-code")
    .homes(&["CLAUDE_CONFIG_DIR"])
    .secrets(&["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"])
    .observation_defaults(&[("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")]);

/// Codex's login and configuration home, and the variables that authenticate
/// it directly.
pub const CODEX_ACCOUNT_STATE: AccountState =
    AccountState::new("codex").homes(&["CODEX_HOME"]).secrets(&["OPENAI_API_KEY", "CODEX_API_KEY"]);

#[derive(Clone, Default)]
pub struct AgentAdapterRegistry {
    adapters: BTreeMap<String, Arc<dyn AgentAdapter>>,
}

impl AgentAdapterRegistry {
    /// The account state of the harnesses built-in detection can register.
    pub fn built_in_account_state() -> &'static [AccountState] {
        &[CLAUDE_CODE_ACCOUNT_STATE, CODEX_ACCOUNT_STATE]
    }

    /// Built-in detection plus every installed factory that accepts the bag.
    /// An installed adapter never replaces a built-in with the same id.
    pub fn discover_with(env: &EnvironmentBag, runner: Arc<dyn CommandRunner>, factories: &[Arc<dyn AgentAdapterFactory>]) -> Self {
        let mut registry = Self::discover(env, Arc::clone(&runner));
        for factory in factories {
            if let Some(adapter) = factory.probe(env, Arc::clone(&runner)) {
                registry.adapters.entry(adapter.id().to_string()).or_insert(adapter);
            }
        }
        registry
    }

    pub fn discover(env: &EnvironmentBag, runner: Arc<dyn CommandRunner>) -> Self {
        let mut registry = Self::default();
        if env.find_env_var("FLOTILLA_FLEET_CANARY") == Some("1") {
            registry.insert(Arc::new(fleet_canary::FleetCanaryAdapter));
        }
        if let Some(binary) = env.find_binary("claude") {
            let state_path = env.find_env_var("CLAUDE_CONFIG_DIR").map(|config_dir| PathBuf::from(config_dir).join(".claude.json"));
            let state_lock = Arc::new(Mutex::new(()));
            registry.insert(Arc::new(CliAgentAdapter {
                binary: binary.as_path().display().to_string(),
                runner: Arc::clone(&runner),
                flavor: AdapterFlavor::ClaudeCode {
                    state_config: state_path.map(|path| ClaudeStateConfig { path, lock: Arc::clone(&state_lock) }),
                    state_lock,
                },
            }));
        }
        if let Some(binary) = env.find_binary("codex") {
            let config_path = env
                .find_env_var("CODEX_HOME")
                .map(PathBuf::from)
                .or_else(|| env.find_env_var("HOME").map(|home| PathBuf::from(home).join(".codex")))
                .map(|home| home.join("config.toml"));
            let trust_lock = Arc::new(Mutex::new(()));
            registry.insert(Arc::new(CliAgentAdapter {
                binary: binary.as_path().display().to_string(),
                runner,
                flavor: AdapterFlavor::Codex {
                    trust_config: config_path.map(|path| CodexTrustConfig { path, lock: Arc::clone(&trust_lock) }),
                    trust_lock,
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
        single_agent_workflow_spec, ClaimExit, Convoy, ConvoyIssue, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, CrewSource,
        ExitDeclaration, IssueSnapshot, ObjectMeta, RepositoryKey, ResourceObject, TerminalAttentionState, TerminalCrewContext,
        WorkflowSnapshot,
    };
    use toml_edit::DocumentMut;

    use crate::adapter::append_convoy_work_context;
    use crate::adapter::build_convoy_crew_brief_with_options;
    use crate::adapter::build_crew_brief;
    use crate::adapter::build_crew_brief_with_options;
    use crate::adapter::AgentAdapterRegistry;
    use crate::adapter::AgentLaunchRequest;
    use crate::adapter::AgentRequirement;
    use crate::adapter::CapabilityTable;
    use crate::adapter::CrewAssignment;
    use crate::adapter::CrewBriefMember;
    use crate::adapter::CrewBriefRenderOptions;
    use crate::adapter::CrewBriefTemplateOverride;
    use crate::adapter::CrewBriefTemplateResolver;
    use crate::adapter::CAPABILITIES_HEADING;
    use crate::adapter::CLAUDE_MANAGED_SETTINGS_PATH;
    use crate::testing::TestConfig;
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_discovery_api::Factory;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_provider_runtime::ProcessCommandRunner;
    use flotilla_replay_testkit::testing::MockRunner;
    use flotilla_vcs::factory::GitVcsFactory;

    const SETTLEMENT_YIELD: &str = "If checks are still pending when you reach settlement, report the PR state and yield at the turn boundary; flotilla wakes you when checks settle, and you then take the clean final snapshot.";

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

    /// The session variables a Claude binding's delivery sets for a requester home.
    fn bound_claude(home: &str) -> Vec<(String, String)> {
        vec![
            ("CLAUDE_CONFIG_DIR".to_string(), home.to_string()),
            (crate::account_access::CLAUDE_OAUTH_TOKEN_FILE_ENV.to_string(), format!("{home}/.flotilla-oauth-token")),
        ]
    }

    #[test]
    fn harness_bound_credential_adapter_ids_name_registered_adapters() {
        let registry = discovered_registry();
        for consumer in [
            flotilla_resources::CredentialConsumer::Claude,
            flotilla_resources::CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
            flotilla_resources::CredentialConsumer::Codex,
        ] {
            let id = consumer.harness_adapter().expect("harness-bound consumer");
            assert!(registry.get(id).is_some(), "`{id}` is not a registered agent adapter");
        }
    }

    #[hegel::test]
    fn codex_launch_contract_classifies_only_usage_exits(tc: hegel::TestCase) {
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex");
        assert_eq!(super::minimum_harness_version(codex.id()), Some("0.160.0"));
        let code = tc.draw(hegel::generators::integers::<i32>().min_value(0).max_value(255));
        let screen = if tc.draw(hegel::generators::booleans()) { "error: unexpected argument '--no-daemon' found" } else { "" };
        let failure = codex.classify_exit_failure(code, screen);
        assert_eq!(failure.is_some(), code == 2 && !screen.is_empty());
        if let Some(message) = failure {
            assert!(message.contains("exit 2"));
            assert!(screen.is_empty() || message.contains(screen));
        }
        assert!(registry.get("claude-code").expect("claude").classify_exit_failure(code, screen).is_none());
    }

    // Existing observed standalone authentication diagnostics are conclusive
    // only on nonzero exit. User/quoted/stale/ambiguous output stays resumable.
    #[hegel::test]
    fn launch_refusals_require_standalone_observed_diagnostics(tc: hegel::TestCase) {
        let registry = discovered_registry();
        let adapter_index = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(1));
        let (id, diagnostic) = [
            ("claude-code", "Not logged in · Please run /login"),
            ("codex", "Your access token could not be refreshed. Please log out and sign in again."),
        ][adapter_index];
        let adapter = registry.get(id).expect("adapter");
        // Generate all relevant provenance shapes, including empty, quoted,
        // fenced, composer, earlier screen content, and a later live prompt.
        let shape = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(6));
        let screen = match shape {
            0 => diagnostic.into(),
            1 => format!("> {diagnostic}"),
            2 => format!("```\n{diagnostic}\n```"),
            3 => format!("› {diagnostic}"),
            4 => format!("User asked about this error\n{diagnostic}"),
            5 => format!("{diagnostic}\n❯ Ready"),
            _ => String::new(),
        };
        let code = tc.draw(hegel::generators::integers::<i32>().min_value(0).max_value(1));
        let result = adapter.classify_exit_failure(code, &screen);
        assert_eq!(result.is_some(), shape == 0 && code != 0);
        if let Some(message) = result {
            assert!(message.len() < 160);
            assert!(!message.contains("fake-secret"));
        }
        assert!(adapter.classify_exit_failure(1, "Login expired for fake-secret · Please run /login").is_none());
        assert!(adapter.classify_exit_failure(1, "error: unknown model fake-secret").is_none());
        assert!(adapter.classify_exit_failure(130, diagnostic).is_none(), "an interrupted process plus stale auth text is ambiguous");
    }

    #[test]
    fn default_single_agent_brief_requires_pr_delivery_before_completion() {
        let workflow = single_agent_workflow_spec();
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

        // Glue: every dispatched brief carries the canonical guidance without a project checkout.
        // The renderer may demote the document title, but must preserve the artifact heading.
        assert!(include_str!("../../../docs/agents/decision-ledger.md").starts_with("# Decision ledger\n"));
        assert!(content.contains("```markdown\n## Decision ledger\n"));
        let guidance = include_str!("../../../docs/agents/decision-ledger.md").replacen(
            "# Decision ledger\n",
            "### Ledger template and guidance\n",
            1,
        );
        assert!(content.contains(guidance.trim()));
        assert_eq!(content.matches("**Brief silence:**").count(), 1);
        assert!(!content.contains("\n# Decision ledger\n"));
        assert!(!content.contains("\n\n\nFor repository assignments"));
        assert!(!content.contains("See `docs/agents/decision-ledger.md`"));
        assert!(content.contains("## Decision ledger"));
        assert!(content.contains("ordered least-confident first"));
        assert!(content.contains("**Brief silence:**"));
        assert!(content.contains("**Choice:**"));
        assert!(content.contains("**Alternative:**"));
        assert!(content.contains("**If asking were free:**"));
        assert!(content.contains("The trailing `### Friction` section is optional"));
        assert!(content.contains("flotilla artifact put --kind decision-ledger <path>"));
        // Settlement admits the ledger only after a clean final shepherd snapshot.
        // Select the brief section before the canonical fenced example, which repeats its heading.
        let settlement = content.split_once("\n## Decision ledger\n").expect("ledger section").1;
        // #2690: settlement gives pending checks a yield path before the clean snapshot gate.
        // Glue: fixed brief wording is checked through the real template renderer.
        assert!(settlement.contains(SETTLEMENT_YIELD));
        let clean = settlement.find("Only after the final shepherd snapshot is clean").expect("clean snapshot gate");
        let put = settlement.find("flotilla artifact put --kind decision-ledger <path>").expect("artifact put");
        let complete = settlement.find("Your part ends when every promise is kept or retracted").expect("promise settlement");
        assert!(clean < put && put < complete);
        assert!(settlement.contains("checks settled green, every review item answered, and no conflict"));
        assert!(settlement.contains("You may draft the ledger file while working"));
        assert!(settlement.contains("The ledger artifact keeps its promise"));
        assert!(settlement.contains("take a new clean final snapshot before resubmitting the ledger"));
        assert_eq!(content.matches("follows the settlement order in `## Decision ledger`").count(), 1);
        assert!(content.contains("Follow the settlement order in `## Decision ledger`: submit your PR"));
        assert!(content.contains("`crew complete` is advisory"));
        assert!(content.contains("Background delegates and sub-agents must never run those verbs"));
        assert!(content.contains("Crews never enact failure"));
        assert!(content.contains("--propose <resume|reduce-scope|fail>"));
        assert!(!content.contains("crew fail"));
        assert!(content.contains("## Assignment\n\nFix the flux capacitor."));
        assert!(content.contains("Write the pull request body with the `pr` skill, inside the repository's pull request template, keeping the line that closes the issue."));
        assert!(content.contains("Every item in every review is in scope"));
        assert!(content.contains("Reply to each finding with a fix and commit, concrete reasoning, or a filed follow-up issue number"));
        assert!(content.contains("While checks are pending, report the PR URL and current state, then yield at the turn boundary."));
        assert!(content.contains("Rebase only when the forge reports a conflict with the target branch"));
        assert!(content.contains("merely behind the target branch is ready as it is"));
        assert!(!content.contains("three rebase attempts"));
        insta::assert_snapshot!("dispatched_crew_brief", content);
    }

    // #1986: the governor brief identifies its island through a live resource reference.
    #[test]
    fn governor_work_context_references_live_island() {
        let (mut convoy, repository_key) = convoy_brief_fixture();
        convoy.spec.role = "governor".into();
        convoy.spec.project_ref = Some("porthole".into());
        let mut content = String::new();
        append_convoy_work_context(&mut content, &convoy, &[repository_key], &BTreeMap::new());
        assert!(content.contains("Island Project: `porthole`"));
        assert!(content.contains("resolve current repositories and roles with `flotilla crew list` on each orientation sweep"));
        assert!(!content.contains("- Repositories:"));
    }

    #[test]
    fn standing_brief_keeps_the_convoy_active_across_tasks() {
        let (mut convoy, _) = convoy_brief_fixture();
        convoy.metadata.annotations.insert(flotilla_resources::ENSURED_FROM_ANNOTATION.to_string(), "ensure".to_string());
        let content = build_convoy_crew_brief_with_options(
            &convoy,
            &TerminalCrewContext {
                namespace: "flotilla".to_string(),
                convoy: "standing-ops".to_string(),
                vessel_ref: "standing-ops-work".to_string(),
            },
            "work",
            "steward",
            CrewAssignment::Unassigned,
            &[CrewBriefMember { role: "steward".to_string(), state: "active".to_string(), is_agent: true }],
            &CrewBriefRenderOptions { has_credential_scope: true, ..CrewBriefRenderOptions::default() },
        )
        .expect("render standing brief")
        .content;

        assert!(!content.contains("flotilla crew complete"));
        assert!(!content.contains("flotilla crew fail"));
        assert!(!content.contains("flotilla crew stall"));
        assert!(content.contains("Write the pull request body with the `pr` skill, inside the repository's pull request template, keeping the line that closes the issue."));
        assert!(content.contains("Open a pull request that closes the issue"));
        assert!(content.contains("ADR carry:"));
        assert!(content.contains("yield at the turn boundary"));
        assert!(content.contains("Rebase only when the forge reports a conflict with the target branch"));
        assert!(!content.contains("three rebase attempts"));
        assert!(content.contains("Governors begin every orientation sweep with this command"));
        assert!(content.contains("The Project is the current island charter"));
        insta::assert_snapshot!("standing_crew_brief", content);
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
            &[
                CrewBriefMember { role: "coder".to_string(), state: "active".to_string(), is_agent: true },
                CrewBriefMember { role: "reviewer".to_string(), state: "latent".to_string(), is_agent: true },
            ],
        );
        assert!(reviewed.content.contains("apply the `in-vessel-review` label in the PR-create command itself"));
    }

    #[test]
    fn default_brief_requires_authenticated_github_access() {
        let brief = brief_for(CrewAssignment::Prompt("Fix the flux capacitor."));

        assert!(brief.contains("use only the injected `gh` token-file wrapper with `$GITHUB_TOKEN_FILE` or an injected `GH_TOKEN`"));
        assert!(brief.contains("Never use unauthenticated or anonymous GitHub API requests"));
        assert!(brief.contains("`$GITHUB_TOKEN_FILE` names a nonempty file or `GH_TOKEN` is set, and that `gh auth status` succeeds"));
        assert!(brief.contains("wait for the injected credential to refresh and retry once"));
        assert!(brief.contains("flotilla crew stall --reason infra"));
    }

    #[test]
    fn every_task_brief_variant_directs_blocked_crew_to_stall() {
        for template in ["interactive-session.md", "diff-review.md", "shepherd.md"] {
            let content = build_crew_brief_with_options(
                &TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "fix-delivery".into(),
                    vessel_ref: "fix-delivery-work".into(),
                },
                "work",
                "coder",
                CrewAssignment::Prompt("Complete the assignment."),
                &[CrewBriefMember { role: "coder".into(), state: "active".into(), is_agent: true }],
                &CrewBriefRenderOptions { template: template.into(), ..CrewBriefRenderOptions::default() },
            )
            .expect("render variant")
            .content;
            assert!(content.contains("--propose <resume|reduce-scope|fail>"), "{template}");
            assert!(!content.contains("crew fail"), "{template}");
            // #3703: each task variant must teach settlement even with no admitted promises.
            assert!(!content.contains("flotilla crew complete"), "{template}");
            assert!(content.contains("flotilla crew promise --promise"), "{template}");
            assert!(content.contains("every promise is kept or retracted"), "{template}");
        }
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
                is_standing: false,
                declared_template: None,
                charter_prose: String::new(),
                charter_commit: None,
            },
        )
        .expect("render selected template")
        .content;

        assert!(selected.contains("For interactive sessions, keep the user-facing loop tight"));
        assert!(selected.contains("Never use unauthenticated or anonymous GitHub API requests"));
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
            &[
                CrewBriefMember { role: "coder".to_string(), state: "handed-off".to_string(), is_agent: true },
                CrewBriefMember { role: "reviewer".to_string(), state: "active".to_string(), is_agent: true },
            ],
            &CrewBriefRenderOptions {
                template: "diff-review".to_string(),
                overrides: Vec::new(),
                fork_stance: true,
                has_credential_scope: false,
                is_standing: false,
                declared_template: None,
                charter_prose: String::new(),
                charter_commit: None,
            },
        )
        .expect("render fork review brief")
        .content;

        assert!(brief.contains("flotilla crew coder handoff --message"));
        assert!(brief.contains("sign off on the fork PR"));
        assert!(brief.contains("Never use unauthenticated or anonymous GitHub API requests"));
        assert!(brief.contains("Never add or repoint a git remote"));
        assert!(brief.contains("Never open issues, pull requests, or comments against the upstream repository"));
        assert!(brief.contains("exact repository URL and target ref named in `## Work context`"));
        assert!(!brief.contains("only the fork remote (`origin`)"));
    }

    #[test]
    fn shepherd_template_handles_every_review_item_through_merge_readiness() {
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
                is_standing: false,
                declared_template: None,
                charter_prose: String::new(),
                charter_commit: None,
            },
        )
        .expect("render shepherd brief")
        .content;

        assert!(brief.contains("every item in every review"));
        assert!(brief.contains("Reply to each finding with a fix and commit, concrete reasoning, or a filed follow-up issue number"));
        assert!(brief.contains("Rebase only when the forge reports a conflict with the target branch"));
        assert!(brief.contains("While checks are pending, report the PR URL and current state, then yield at the turn boundary."));
        assert!(!brief.contains("three rebase attempts"));
        assert!(brief.contains("If CI remains red for reasons outside this pull request"));
        assert!(brief.contains("`pr-shepherd` skill"));
        assert!(brief.contains("For a Forgejo destination, do not use that GitHub-only helper"));
        assert!(brief.contains("with the `pr-shepherd` skill for a GitHub destination, or through the injected Forgejo API credentials for a Forgejo destination"));
        assert!(!brief.contains("pull request using the `pr-shepherd` skill"));
        assert!(!brief.contains("Future events belong to a later engagement"));
        assert!(brief.contains("the crew's decision-ledger artifact"));
        // #2690: count both occurrences: inherited settlement and the shepherd delivery block's readiness gate.
        assert_eq!(brief.matches(SETTLEMENT_YIELD).count(), 2);
        // Select the inherited brief section before the fenced ledger example, which repeats its heading.
        let settlement = brief.split_once("\n## Decision ledger\n").expect("ledger section").1;
        assert!(settlement.contains(SETTLEMENT_YIELD));
        assert!(brief.contains("Submit your own decision ledger after the clean final snapshot"));
        assert!(brief.contains("park the verified commit"));
        assert!(brief.contains("redispatched under the owning project"));
        assert!(brief.contains("flotilla crew submit --kind pr --reference '<PR URL>'"));
        assert!(brief.contains("Never use unauthenticated or anonymous GitHub API requests"));
        assert!(!brief.contains("wait-for-checks"));
        assert!(!brief.contains("No assignment was provided"));
    }

    #[test]
    fn delivery_briefs_report_pending_checks_and_yield_for_platform_wake_up() {
        for template in ["crew.md", "shepherd", "diff-review"] {
            let options = CrewBriefRenderOptions { template: template.into(), ..CrewBriefRenderOptions::default() };
            let brief = build_crew_brief_with_options(
                &TerminalCrewContext { namespace: "flotilla".into(), convoy: "review".into(), vessel_ref: "review-work".into() },
                "work",
                "coder",
                CrewAssignment::Prompt("Review the PR."),
                &[CrewBriefMember { role: "coder".into(), state: "active".into(), is_agent: true }],
                &options,
            )
            .expect("render review brief")
            .content;

            // #2596: every delivery brief uses the platform's pending-CI yield and wake contract.
            assert!(
                brief.contains("While checks are pending, report the PR URL and current state, then yield at the turn boundary."),
                "{template}"
            );
            assert!(
                brief.contains(
                    "Flotilla delivers a new turn when checks at head settle (pass or fail), or actionable review feedback arrives."
                ),
                "{template}"
            );
            assert!(!brief.contains("#2300"), "{template}");
            assert!(!brief.contains("wait about one minute"), "{template}");
            assert!(brief.contains("every review"), "{template}");
            assert!(brief.contains("Rebase only when the forge reports a conflict with the target branch"), "{template}");
        }
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
                is_standing: false,
                declared_template: None,
                charter_prose: String::new(),
                charter_commit: None,
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

    fn convoy_brief_fixture() -> (ResourceObject<Convoy>, RepositoryKey) {
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
                continuation: None,
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
                subjects: Vec::new(),
                issues: vec![issue("809", "First issue", "First issue body."), issue("810", "Second issue", "Second issue body.")],
                change_request: Some(flotilla_resources::BoundChangeRequest {
                    id: "1071".to_string(),
                    repository_ref: repo_ref.clone(),
                    title: "Existing pull request".to_string(),
                }),
                instruction: None,
                promises: Vec::new(),
            },
            status: None,
        };
        (convoy, repo_ref)
    }

    #[test]
    fn convoy_work_context_separates_multiple_issue_snapshots() {
        let (convoy, repo_ref) = convoy_brief_fixture();
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
    fn standing_brief_selection_follows_the_pinned_exit_or_ensure() {
        let (convoy, _) = convoy_brief_fixture();
        let options = CrewBriefRenderOptions::default();
        assert!(!options.clone().for_convoy(&convoy).is_standing);
        let mut declared_exit = convoy.clone();
        declared_exit.status = Some(ConvoyStatus {
            workflow_snapshot: Some(WorkflowSnapshot {
                cascade: None,
                stall_nudges: Default::default(),
                supervision: None,
                exit: Some(ExitDeclaration::Claim(ClaimExit)),
                turn_delivery: Default::default(),
                vessels: Vec::new(),
            }),
            ..ConvoyStatus::default()
        });
        assert!(!options.clone().for_convoy(&declared_exit).is_standing);
        declared_exit.status.as_mut().expect("status").workflow_snapshot.as_mut().expect("snapshot").exit = None;
        assert!(options.clone().for_convoy(&declared_exit).is_standing);
        declared_exit.metadata.annotations.insert(flotilla_resources::ENSURED_FROM_ANNOTATION.to_string(), "ensure".to_string());
        declared_exit.status = None;
        assert!(options.for_convoy(&declared_exit).is_standing);
    }

    #[test]
    fn unassigned_brief_says_so_and_forbids_inventing_work() {
        let content = brief_for(CrewAssignment::Unassigned);
        assert!(content.contains("No assignment was provided with this dispatch."));
        assert!(content.contains("rather than inventing work"));
        assert!(content.contains("flotilla crew stall --reason decision"));
        assert!(!content.contains("crew fail"));
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
    fn ambient_only_requirements_resolve_to_their_scope_without_a_delivery_slot() {
        let scope = flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE;
        let table = CapabilityTable::seeded()
            .with_requirement("ambient", AgentRequirement::ambient_only("ambient-harness", Some("m1".into()), scope))
            .with_requirement("coding", AgentRequirement::ambient_only("replacement", None, scope));

        let ambient = table.resolve("ambient").expect("added capability");
        assert_eq!((ambient.adapter.as_str(), ambient.model.as_deref()), ("ambient-harness", Some("m1")));
        assert_eq!(ambient.account_access(), None);
        assert_eq!(ambient.ambient_credential_scope(), Some(scope));
        assert_eq!(table.resolve("coding").expect("replaced capability").adapter, "replacement");

        // A model-only override keeps the table's credential policy.
        let mut reskinned = flotilla_resources::Selector::for_capability("ambient");
        reskinned.model = Some("m2".to_string());
        let requirement = table.resolve_selector(&reskinned).expect("model override");
        assert_eq!((requirement.model.as_deref(), requirement.account_access()), (Some("m2"), None));
        assert_eq!(requirement.ambient_credential_scope(), Some(scope));
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

    // Both harnesses preserve every byte of a bounded brief and keep oversized
    // prompts below the single exec-argument limit, including shell quoting.
    #[hegel::test]
    fn brief_delivery_obeys_quoted_argument_bound(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Cross empty, exact threshold and oversized input with multibyte and
        // shell metacharacters; the full file remains the canonical fallback.
        let length = tc.draw(gs::integers::<usize>().min_value(0).max_value(70_000));
        let unit = ["a", "'", "🛥"][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
        let brief = flotilla_resources::TerminalBrief {
            path: ".flotilla/briefs/coder.md".into(),
            content: unit.repeat(length),
            artifact_digest: None,
            copies: vec![],
        };
        for adapter_id in ["codex", "claude-code"] {
            let registry = discovered_registry();
            let adapter = registry.get(adapter_id).expect("adapter");
            let prompt = adapter.deliver_brief(&brief);
            assert!(flotilla_protocol::arg::shell_quote(&prompt).len() <= 64 * 1024);
            assert!(prompt.contains(&brief.path));
            assert!(prompt.contains("context compaction"));
            let inline = format!("{}\n\nThis is also at `{}`; re-read that file after a context compaction.", brief.content, brief.path);
            if flotilla_protocol::arg::shell_quote(&inline).len() <= 64 * 1024 {
                assert_eq!(prompt, inline);
            } else {
                assert!(prompt.starts_with("Read your crew brief at"));
            }
        }
    }

    // Pin the inclusive byte boundary, plus card preservation on file fallback.
    #[test]
    fn brief_delivery_includes_exact_bound_and_preserves_fallback_card() {
        let registry = discovered_registry();
        let adapter = registry.get("codex").expect("codex");
        let mut brief =
            flotilla_resources::TerminalBrief { path: "brief.md".into(), content: String::new(), artifact_digest: None, copies: vec![] };
        let overhead = flotilla_protocol::arg::shell_quote(&adapter.deliver_brief(&brief)).len();
        brief.content = "x".repeat(64 * 1024 - overhead);
        assert!(adapter.deliver_brief(&brief).starts_with(&brief.content));
        brief.content.push('x');
        assert!(adapter.deliver_brief(&brief).starts_with("Read your crew brief at"));
        // Quoted headings in the assignment must not replace the final appended card.
        brief.content.push_str(&format!("{CAPABILITIES_HEADING}\nQuoted assignment card."));
        brief.content.push_str(&format!("{CAPABILITIES_HEADING}\nYou can write issues."));
        let prompt = adapter.deliver_brief(&brief);
        assert!(prompt.contains("You can write issues."));
        assert!(!prompt.contains("Quoted assignment card."));
        assert!(flotilla_protocol::arg::shell_quote(&prompt).len() < 64 * 1024);
    }

    #[tokio::test]
    async fn adapters_prepare_the_canonical_brief_and_launch_with_inline_content() {
        let registry = discovered_registry();
        let cwd = ExecutionEnvironmentPath::new("/workspace");
        let brief = flotilla_resources::TerminalBrief {
            artifact_digest: None,
            path: ".flotilla/briefs/coder.md".into(),
            content: "protocol preamble\n\nImplement the issue.".into(),
            copies: Vec::new(),
        };

        let codex = registry.get("codex").expect("codex adapter");
        codex.prepare(&cwd, &brief).await.expect("prepare brief");
        let plan = codex
            .launch(&AgentLaunchRequest {
                fulfilment_grants: None,
                role: "coder".into(),
                model: None,
                brief: brief.clone(),
                environment: Vec::new(),
            })
            .expect("codex launch plan");
        assert!(plan.command.starts_with("/tools/codex --dangerously-bypass-approvals-and-sandbox --no-daemon"));
        for override_value in crate::codex_hook_overrides() {
            assert!(plan.command.contains(&format!("-c '{override_value}'")));
        }
        assert!(plan.command.contains("Implement the issue"));
        assert_eq!(plan.stance, "trusted-implicit");
        let restricted = codex
            .launch(&AgentLaunchRequest {
                fulfilment_grants: Some(BTreeSet::from([flotilla_resources::FulfilmentGrant::platform("linux".to_string())])),
                role: "coder".into(),
                model: None,
                brief: brief.clone(),
                environment: Vec::new(),
            })
            .expect("restricted launch plan");
        assert!(!restricted.command.contains("--dangerously-bypass-approvals-and-sandbox"));

        let claude = registry.get("claude-code").expect("claude adapter");
        let plan = claude
            .launch(&AgentLaunchRequest {
                fulfilment_grants: None,
                role: "reviewer".into(),
                model: Some("opus".into()),
                brief: brief.clone(),
                environment: bound_claude("/homes/reviewer/claude"),
            })
            .expect("claude launch plan");
        assert_eq!(
            plan.command,
            "CLAUDE_CODE_OAUTH_TOKEN=\"$(cat -- '/homes/reviewer/claude/.flotilla-oauth-token')\" /tools/claude --dangerously-skip-permissions --settings .flotilla/claude-settings.json --model 'opus' 'protocol preamble\n\nImplement the issue.\n\nThis is also at `.flotilla/briefs/coder.md`; re-read that file after a context compaction.'"
        );
        assert!(plan.command.contains("Implement the issue"));

        // Admission rejects metacharacter models before they reach launch;
        // the sink still shell-quotes so a hostile model can never splice
        // into the command line even if that boundary regressed.
        let plan = claude
            .launch(&AgentLaunchRequest {
                fulfilment_grants: None,
                role: "reviewer".into(),
                model: Some("opus; touch /tmp/pwned".into()),
                brief,
                environment: bound_claude("/homes/reviewer/claude"),
            })
            .expect("claude launch plan");
        assert!(plan.command.contains("--model 'opus; touch /tmp/pwned'"));
    }

    #[test]
    fn launch_plan_uses_the_discovered_absolute_binary_path() {
        let env = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/x/y/claude"));
        let registry = AgentAdapterRegistry::discover(&env, Arc::new(MockRunner::new(Vec::new())));
        let brief = flotilla_resources::TerminalBrief {
            artifact_digest: None,
            path: ".flotilla/briefs/coder.md".into(),
            content: String::new(),
            copies: Vec::new(),
        };

        let plan = registry
            .get("claude-code")
            .expect("claude adapter")
            .launch(&AgentLaunchRequest {
                fulfilment_grants: None,
                role: "coder".into(),
                model: None,
                brief,
                environment: bound_claude("/homes/coder/claude"),
            })
            .expect("launch plan");

        assert!(plan.command.contains("\" /x/y/claude "), "{}", plan.command);
        assert!(!plan.command.contains(" claude "));
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
        let brief = flotilla_resources::TerminalBrief {
            artifact_digest: None,
            path: ".flotilla/briefs/coder.md".into(),
            content: "brief".into(),
            copies: Vec::new(),
        };

        let invocation_environment = vec![
            ("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION".to_string(), "true".to_string()),
            ("FLOTILLA_ENVIRONMENT_ID".to_string(), "contained-claude".to_string()),
        ]
        .into_iter()
        .chain(bound_claude(&claude_config.display().to_string()))
        .collect::<Vec<_>>();
        claude
            .prepare_with_environment(&ExecutionEnvironmentPath::new(&workspace), &brief, &invocation_environment)
            .await
            .expect("prepare claude workspace");

        // The launch command names this path relatively, so it must resolve
        // against the session's working directory.
        let plan = claude
            .launch(&AgentLaunchRequest {
                fulfilment_grants: None,
                role: "coder".into(),
                model: None,
                brief: brief.clone(),
                environment: invocation_environment,
            })
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
        // Glue: crew settings and launch environment both disable prompt suggestions,
        // including an explicitly supplied conflicting environment value.
        assert_eq!(settings["promptSuggestionEnabled"], false);
        assert_eq!(
            plan.env.iter().filter(|(name, _)| name == "CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION").collect::<Vec<_>>(),
            vec![&("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION".to_string(), "false".to_string())]
        );
        assert_eq!(settings["skipDangerousModePermissionPrompt"], true);
        assert_eq!(settings["attribution"], serde_json::json!({ "commit": "", "pr": "", "sessionUrl": false }));
        assert_eq!(settings["includeCoAuthoredBy"], false);

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

    // A launch environment carrying flotillad's hook URL and token (host-direct sessions
    // while a loopback listener is up) gets HTTP hooks; the token reaches Claude Code's
    // environment for header interpolation. Without them, as in a vessel, hooks stay
    // commands.
    #[tokio::test]
    async fn claude_prepare_installs_http_hooks_only_when_the_launch_carries_the_hook_endpoint() {
        let temp = tempfile::tempdir().expect("tempdir");
        let env = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/tools/claude"));
        let registry = AgentAdapterRegistry::discover(&env, Arc::new(ProcessCommandRunner));
        let claude = registry.get("claude-code").expect("claude adapter");
        let brief = flotilla_resources::TerminalBrief {
            artifact_digest: None,
            path: ".flotilla/briefs/coder.md".into(),
            content: "brief".into(),
            copies: Vec::new(),
        };
        let hook_environment = vec![
            (crate::CLAUDE_CODE_HOOK_URL_ENV.to_string(), "http://127.0.0.1:7878/v1/hooks/claude-code".to_string()),
            (crate::CLAUDE_CODE_HOOK_TOKEN_ENV.to_string(), "hook-token".to_string()),
        ];
        for (name, environment, http) in [("host-direct", hook_environment.clone(), true), ("vessel", Vec::new(), false)] {
            let home = temp.path().join(format!("{name}-home"));
            let environment = environment.into_iter().chain(bound_claude(&home.display().to_string())).collect::<Vec<_>>();
            let workspace = temp.path().join(name);
            std::fs::create_dir_all(&workspace).expect("workspace");
            claude.prepare_with_environment(&ExecutionEnvironmentPath::new(&workspace), &brief, &environment).await.expect("prepare");
            let settings: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(workspace.join(CLAUDE_MANAGED_SETTINGS_PATH)).expect("settings overlay"))
                    .expect("parse settings");
            let stop = &settings["hooks"]["Stop"][0]["hooks"][0];
            if http {
                assert_eq!(stop["type"], "http", "{name}");
                assert_eq!(stop["url"], "http://127.0.0.1:7878/v1/hooks/claude-code/stop");
                assert_eq!(stop["headers"]["Authorization"], "Bearer $FLOTILLA_HOOK_TOKEN");
            } else {
                assert_eq!(stop["command"], "flotilla hook claude-code stop", "{name}");
            }
            assert_eq!(settings["hooks"]["SessionStart"][0]["hooks"][0]["command"], "flotilla hook claude-code session-start", "{name}");
            let plan = claude
                .launch(&AgentLaunchRequest {
                    fulfilment_grants: None,
                    role: "coder".into(),
                    model: None,
                    brief: brief.clone(),
                    environment: environment.clone(),
                })
                .expect("launch plan");
            assert_eq!(
                plan.env.iter().any(|(key, value)| key == crate::CLAUDE_CODE_HOOK_TOKEN_ENV && value == "hook-token"),
                http,
                "{name}"
            );
        }
    }

    // #3907 contract 8: a Claude session starts only in the requester's bound
    // home, contained or not; the plan's token file is its only token source.
    #[tokio::test]
    async fn claude_uses_only_the_bound_requester_home() {
        let workspace = ExecutionEnvironmentPath::new("/workspace");
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new()), Ok("/workspace\n".to_string()), Err("not a checkout".to_string())]));
        let env = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/tools/claude"));
        let registry = AgentAdapterRegistry::discover(&env, runner.clone());
        let claude = registry.get("claude-code").expect("Claude adapter");
        let brief = flotilla_resources::TerminalBrief {
            artifact_digest: None,
            path: ".flotilla/briefs/coder.md".into(),
            content: "brief".into(),
            copies: Vec::new(),
        };
        let home = "/home/crew/flotilla/agent-homes/requesters/c1/coder/claude";
        let contained = ("FLOTILLA_ENVIRONMENT_ID".to_string(), "contained-claude".to_string());
        let ambient = [
            ("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "ambient-profile-token".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "ambient-key".to_string()),
        ];
        let unbound = std::iter::once(contained.clone()).chain(ambient.clone()).collect::<Vec<_>>();
        let refusal =
            "Claude Code requires the requester's bound account home: no account binding delivered `CLAUDE_CONFIG_DIR` and its token file";
        let missing = claude.prepare_with_environment(&workspace, &brief, &unbound).await.expect_err("an unbound session is refused");
        assert_eq!(missing, refusal);
        let launch = |environment: Vec<(String, String)>| {
            claude.launch(&AgentLaunchRequest {
                fulfilment_grants: None,
                role: "coder".into(),
                model: None,
                brief: brief.clone(),
                environment,
            })
        };
        // Mutation check: without the explicit home the session would fall through to `~/.claude`
        // and the ambient token; the launch refuses instead.
        assert_eq!(launch(unbound.clone()).expect_err("unbound launch"), refusal);

        let bound = unbound.into_iter().chain(bound_claude(home)).collect::<Vec<_>>();
        claude.prepare_with_environment(&workspace, &brief, &bound).await.expect("prepare bound Claude");
        let plan = launch(bound).expect("bound launch plan");
        let value = |key: &str| plan.env.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str());
        assert_eq!(value("CLAUDE_CONFIG_DIR"), Some(home));
        assert_eq!((value("CLAUDE_CODE_OAUTH_TOKEN"), value("ANTHROPIC_API_KEY")), (None, None));
        assert!(plan.command.starts_with(&format!("CLAUDE_CODE_OAUTH_TOKEN=\"$(cat -- '{home}/.flotilla-oauth-token')\" /tools/claude ")));
        assert!(runner.calls().iter().any(|(command, args)| command == "mkdir" && args == &["-p", home]));
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
        let brief = flotilla_resources::TerminalBrief {
            artifact_digest: None,
            path: ".flotilla/briefs/coder.md".into(),
            content: "brief".into(),
            copies: Vec::new(),
        };

        registry
            .get("claude-code")
            .expect("claude adapter")
            .prepare_with_environment(
                &ExecutionEnvironmentPath::new(&workspace_root),
                &brief,
                &bound_claude(&claude_config.display().to_string()),
            )
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
        let brief = flotilla_resources::TerminalBrief {
            artifact_digest: None,
            path: ".flotilla/briefs/coder.md".into(),
            content: "brief".into(),
            copies: Vec::new(),
        };

        let bound_home = temp.path().join("requester-home");
        registry
            .get("claude-code")
            .expect("Claude adapter")
            .prepare_with_environment(&ExecutionEnvironmentPath::new(&workspace), &brief, &bound_claude(&bound_home.display().to_string()))
            .await
            .expect("prepare host-direct Claude");

        assert_eq!(std::fs::read_to_string(global_state).expect("global Claude state"), r#"{"existing":true}"#);
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(workspace.join(CLAUDE_MANAGED_SETTINGS_PATH)).expect("settings overlay"))
                .expect("parse settings overlay");
        assert!(settings.get("attribution").is_none());
        assert!(settings.get("includeCoAuthoredBy").is_none());
    }

    // #3655: containment is a property of the session. A session that names a
    // flotilla environment seeds onboarding (Claude) and workspace trust in the
    // harness home its environment selects, and never writes the host user's
    // `~/.claude.json` or `~/.codex/config.toml`, whatever the discovery bag
    // says. Generator: both harnesses; a host bag with and without explicit
    // config homes; a bag that does or does not itself claim containment (an
    // agentless login environment never does); repeated preparation.
    #[hegel::test]
    fn a_contained_session_prepares_only_its_own_harness_home(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let codex = tc.draw(gs::booleans());
        let explicit_host_homes = tc.draw(gs::booleans());
        let bag_claims_containment = tc.draw(gs::booleans());
        let preparations = tc.draw(gs::integers::<usize>().min_value(1).max_value(2));
        let temp = tempfile::tempdir().expect("tempdir");
        let host_home = temp.path().join("host-home");
        let workspace = temp.path().join("checkout");
        let flotilla_home = temp.path().join("flotilla-config/agent-homes/host-direct-b3");
        std::fs::create_dir_all(host_home.join(".claude")).expect("host Claude dir");
        std::fs::create_dir_all(host_home.join(".codex")).expect("host Codex dir");
        std::fs::create_dir_all(&workspace).expect("checkout");
        let host_files = [host_home.join(".claude.json"), host_home.join(".claude/.claude.json"), host_home.join(".codex/config.toml")];
        for file in &host_files {
            std::fs::write(file, "host-owned").expect("host harness file");
        }
        let mut bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("HOME", host_home.display().to_string()))
            .with(EnvironmentAssertion::binary("claude", "/tools/claude"))
            .with(EnvironmentAssertion::binary("codex", "/tools/codex"));
        if explicit_host_homes {
            bag = bag
                .with(EnvironmentAssertion::env_var("CLAUDE_CONFIG_DIR", host_home.join(".claude").display().to_string()))
                .with(EnvironmentAssertion::env_var("CODEX_HOME", host_home.join(".codex").display().to_string()));
        }
        if bag_claims_containment {
            bag = bag.with(EnvironmentAssertion::env_var("FLOTILLA_ENVIRONMENT_ID", "host-direct-b3"));
        }
        let (adapter_id, home) = if codex { ("codex", flotilla_home.join("codex")) } else { ("claude-code", flotilla_home.join("claude")) };
        std::fs::create_dir_all(&home).expect("flotilla home");
        let mut session = vec![(super::FLOTILLA_ENVIRONMENT_ID_ENV.to_string(), "host-direct-b3".to_string())];
        if codex {
            session.push(("CODEX_HOME".to_string(), home.display().to_string()));
        } else {
            session.extend(bound_claude(&home.display().to_string()));
        }
        let brief = flotilla_resources::TerminalBrief {
            artifact_digest: None,
            path: ".flotilla/briefs/coder.md".into(),
            content: "brief".into(),
            copies: Vec::new(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let registry = AgentAdapterRegistry::discover(&bag, Arc::new(ProcessCommandRunner));
        let adapter = registry.get(adapter_id).expect("adapter");
        for _ in 0..preparations {
            runtime
                .block_on(adapter.prepare_with_environment(&ExecutionEnvironmentPath::new(&workspace), &brief, &session))
                .expect("prepare contained session");
        }
        let plan = adapter
            .launch(&AgentLaunchRequest { fulfilment_grants: None, role: "coder".into(), model: None, brief, environment: session })
            .expect("launch contained session");

        for file in &host_files {
            assert_eq!(std::fs::read_to_string(file).expect("host harness file"), "host-owned", "{} was written", file.display());
        }
        let canonical = workspace.canonicalize().expect("canonical checkout").display().to_string();
        let selected = |name: &str| plan.env.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
        if codex {
            let config = std::fs::read_to_string(home.join("config.toml")).expect("flotilla Codex config");
            let config = config.parse::<DocumentMut>().expect("parse Codex config");
            assert_eq!(config["projects"][&canonical]["trust_level"].as_str(), Some("trusted"));
            assert_eq!(selected("CODEX_HOME"), Some(home.display().to_string()));
        } else {
            let state: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).expect("flotilla Claude state")).expect("state");
            assert_eq!(state["hasCompletedOnboarding"], true);
            assert_eq!(state["projects"][&canonical]["hasTrustDialogAccepted"], true);
            assert_eq!(selected("CLAUDE_CONFIG_DIR"), Some(home.display().to_string()));
            let settings: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(workspace.join(CLAUDE_MANAGED_SETTINGS_PATH)).expect("settings overlay"))
                    .expect("parse settings overlay");
            assert_eq!(settings["attribution"], serde_json::json!({ "commit": "", "pr": "", "sessionUrl": false }));
        }
    }

    // #3907 contract 8: a host-direct session no longer keeps the host user's
    // ambient Claude config; it starts in its bound home like any other.
    #[test]
    fn host_direct_claude_launches_in_its_bound_home() {
        let env = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/tools/claude"));
        let registry = AgentAdapterRegistry::discover(&env, Arc::new(MockRunner::new(Vec::new())));
        let claude = registry.get("claude-code").expect("Claude adapter");
        let brief = flotilla_resources::TerminalBrief {
            artifact_digest: None,
            path: ".flotilla/briefs/coder.md".into(),
            content: "brief".into(),
            copies: Vec::new(),
        };
        let home = "/state/flotilla/agent-homes/requesters/c1/coder/claude";
        let plan = claude
            .launch(&AgentLaunchRequest {
                fulfilment_grants: None,
                role: "coder".into(),
                model: None,
                brief,
                environment: bound_claude(home),
            })
            .expect("host-direct launch");

        assert!(plan.env.iter().any(|(name, value)| name == "CLAUDE_CONFIG_DIR" && value == home));
        assert!(plan.env.iter().all(|(name, _)| name != "CLAUDE_CODE_OAUTH_TOKEN"));
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
        let brief = flotilla_resources::TerminalBrief {
            artifact_digest: None,
            path: ".flotilla/briefs/coder.md".into(),
            content: String::new(),
            copies: Vec::new(),
        };
        codex.prepare(&ExecutionEnvironmentPath::new(&workspace), &brief).await.expect("prepare Codex workspace");

        let config = std::fs::read_to_string(codex_home.join("config.toml")).expect("Codex config");
        assert!(config.contains("# keep this comment"));
        let parsed = config.parse::<DocumentMut>().expect("parse updated Codex config");
        let canonical_workspace = workspace.canonicalize().expect("canonical workspace").display().to_string();
        assert_eq!(parsed["model"].as_str(), Some("gpt-5.6-sol"));
        assert!(parsed.get("commit_attribution").is_none(), "host-direct Codex config should retain operator attribution");
        assert!(parsed.get("notify").is_none(), "host-direct Codex should use an invocation-only notify override");
        assert_eq!(parsed["projects"]["/existing"]["trust_level"].as_str(), Some("trusted"));
        assert_eq!(parsed["projects"][&canonical_workspace]["trust_level"].as_str(), Some("trusted"));
    }

    #[tokio::test]
    async fn codex_prepares_trust_in_each_invocation_home() {
        // Issue #2672: preparation must trust the workspace in the same private
        // home launch uses, without writing trust into another crew's config.
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path().join("codex");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("CODEX_HOME", base.display().to_string()))
            .with(EnvironmentAssertion::env_var("FLOTILLA_ENVIRONMENT_ID", "shared"))
            .with(EnvironmentAssertion::binary("codex", "/tools/codex"));
        let registry = AgentAdapterRegistry::discover(&bag, Arc::new(ProcessCommandRunner));
        let adapter = registry.get("codex").expect("adapter");
        let brief = flotilla_resources::TerminalBrief {
            artifact_digest: None,
            path: ".flotilla/briefs/coder.md".into(),
            content: String::new(),
            copies: Vec::new(),
        };
        let canonical = workspace.canonicalize().expect("canonical workspace").display().to_string();
        for role in ["coder", "reviewer"] {
            let home = base.join("crews").join(role);
            adapter
                .prepare_with_environment(
                    &ExecutionEnvironmentPath::new(&workspace),
                    &brief,
                    &vec![("CODEX_HOME".into(), home.display().to_string())],
                )
                .await
                .expect("prepare crew");
            let config = std::fs::read_to_string(home.join("config.toml")).expect("private config").parse::<DocumentMut>().expect("config");
            assert_eq!(config["projects"][&canonical]["trust_level"].as_str(), Some("trusted"));
        }
        assert!(!base.join("config.toml").exists(), "trust is prepared in each invocation home");
    }

    // #2634: every Codex launch carries the hook, regardless of confinement,
    // model selection, credential home, or permission grants. No prepare is needed.
    #[hegel::test]
    fn codex_launch_always_installs_native_hooks(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Both confinement/grant states, absent/present model, and missing/different homes.
        let contained = tc.draw(gs::booleans());
        let restricted = tc.draw(gs::booleans());
        let model = tc.draw(gs::booleans()).then(|| "test-model".to_string());
        let home = tc.draw(gs::booleans());
        let mut env = EnvironmentBag::new().with(EnvironmentAssertion::binary("codex", "/tools/codex"));
        if contained {
            env = env.with(EnvironmentAssertion::env_var("FLOTILLA_ENVIRONMENT_ID", "contained"));
        }
        if home {
            env = env.with(EnvironmentAssertion::env_var("CODEX_HOME", "/different-home"));
        }
        // Subprocess boundary: command rendering must not execute a process.
        let registry = AgentAdapterRegistry::discover(&env, Arc::new(MockRunner::new(Vec::new())));
        let plan = registry
            .get("codex")
            .expect("adapter")
            .launch(&AgentLaunchRequest {
                fulfilment_grants: restricted.then(BTreeSet::new),
                role: "coder".into(),
                model,
                brief: flotilla_resources::TerminalBrief {
                    path: "brief.md".into(),
                    content: String::new(),
                    artifact_digest: None,
                    copies: Vec::new(),
                },
                environment: vec![("CODEX_HOME".into(), "/launch-home".into())],
            })
            .expect("launch");
        assert_eq!(plan.command.matches("-c 'notify=[]'").count(), 1);
        assert!(plan.command.contains("flotilla hook codex stop"));
        assert!(plan.command.contains("--no-daemon"));
        assert_eq!(super::minimum_harness_version("codex"), Some("0.160.0"));
    }

    #[tokio::test]
    async fn contained_codex_uses_only_invocation_hooks_for_an_already_trusted_workspace() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex-home");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&codex_home).expect("codex home");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let canonical_workspace = workspace.canonicalize().expect("canonical workspace").display().to_string();
        std::fs::write(codex_home.join("config.toml"), format!("[projects.{canonical_workspace:?}]\ntrust_level = \"trusted\"\n"))
            .expect("initial Codex config");
        let env = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("CODEX_HOME", codex_home.display().to_string()))
            .with(EnvironmentAssertion::binary("codex", "/tools/codex"));
        let registry = AgentAdapterRegistry::discover(&env, Arc::new(ProcessCommandRunner));
        let brief = flotilla_resources::TerminalBrief {
            artifact_digest: None,
            path: ".flotilla/briefs/coder.md".into(),
            content: String::new(),
            copies: Vec::new(),
        };

        let codex = registry.get("codex").expect("codex adapter");
        // A Docker vessel's session names its environment; its home is the image's CODEX_HOME.
        let session = vec![("FLOTILLA_ENVIRONMENT_ID".to_string(), "contained-codex".to_string())];
        codex.prepare_with_environment(&ExecutionEnvironmentPath::new(&workspace), &brief, &session).await.expect("prepare");

        let plan = codex
            .launch(&AgentLaunchRequest { fulfilment_grants: None, role: "coder".into(), model: None, brief, environment: session })
            .expect("contained Codex launch");
        assert!(plan.command.contains("--no-daemon"));
        // #3051: invocation hooks remain wired if the home config is later rewritten.
        assert!(plan.command.contains("flotilla hook codex stop"));

        let config = std::fs::read_to_string(codex_home.join("config.toml")).expect("Codex config");
        let parsed = config.parse::<DocumentMut>().expect("parse updated Codex config");
        assert!(parsed.get("commit_attribution").is_none());
        assert!(parsed.get("notify").is_none());
        assert!(parsed.get("hooks").is_none());
        assert_eq!(parsed["projects"][&canonical_workspace]["trust_level"].as_str(), Some("trusted"));
    }

    // #3823: the combined staged home and invocation must discover each crew
    // hook once, with the trust record reported by real Codex 0.160.0.
    // Configuration glue: exhaust fresh and previous-generation homes, including
    // an unrelated hook, rather than generating subprocess or concurrency inputs.
    #[tokio::test]
    async fn contained_codex_combined_layers_define_each_trusted_hook_once() {
        let recorded: serde_json::Value = serde_json::from_str(include_str!("fixtures/codex-3823/hooks-list.json")).unwrap();
        let recorded_hooks = recorded["result"]["data"][0]["hooks"].as_array().unwrap();
        assert_eq!(recorded_hooks.len(), 7);
        for previous_generation in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(workspace.join(".flotilla/briefs")).unwrap();
            let config_path = home.join("config.toml");
            let mut initial = "[hooks]\nStop = [{hooks=[{type=\"command\",command=\"custom\"}]}]\n".parse::<DocumentMut>().unwrap();
            if previous_generation {
                crate::update_codex_hooks(&mut initial, true, &config_path).unwrap();
            }
            std::fs::write(&config_path, initial.to_string()).unwrap();
            let env = EnvironmentBag::new()
                .with(EnvironmentAssertion::env_var("CODEX_HOME", home.display().to_string()))
                .with(EnvironmentAssertion::binary("codex", "/tools/codex"));
            let registry = AgentAdapterRegistry::discover(&env, Arc::new(ProcessCommandRunner));
            let adapter = registry.get("codex").unwrap();
            let session = vec![("FLOTILLA_ENVIRONMENT_ID".into(), "contained-codex".into())];
            let brief = flotilla_resources::TerminalBrief {
                path: ".flotilla/briefs/coder.md".into(),
                content: String::new(),
                artifact_digest: None,
                copies: vec![],
            };
            adapter.prepare_with_environment(&ExecutionEnvironmentPath::new(&workspace), &brief, &session).await.unwrap();
            let plan = adapter
                .launch(&AgentLaunchRequest { fulfilment_grants: None, role: "coder".into(), model: None, brief, environment: session })
                .unwrap();
            let config = std::fs::read_to_string(&config_path).unwrap();
            assert!(config.contains("custom"), "preserve the user's hook");
            // State keys and hashes come from discovery of the actual staged
            // launch, not an overrides-only fixture. Windows changes only paths.
            for hook in recorded_hooks {
                assert_eq!(hook["trustStatus"], "trusted");
                assert_eq!(hook["enabled"], true);
                let command = hook["command"].as_str().unwrap();
                assert!(!config.contains(command), "home duplicates {command}");
                assert_eq!(plan.command.matches(command).count(), 1, "invocation defines {command} once");
                let key = hook["key"].as_str().unwrap();
                let key = if cfg!(windows) {
                    key.replace("/<session-flags>/config.toml", r"C:\<session-flags>\config.toml")
                } else {
                    key.to_string()
                };
                let state_override = crate::codex_hook_overrides()
                    .into_iter()
                    .find(|entry| entry.starts_with("hooks.state="))
                    .expect("invocation hook trust override");
                assert!(plan.command.contains(&state_override), "launch includes trust records");
                let state = state_override.parse::<DocumentMut>().unwrap();
                assert_eq!(state["hooks"]["state"][&key]["trusted_hash"].as_str(), hook["currentHash"].as_str());
                assert_eq!(state["hooks"]["state"][&key]["enabled"].as_bool(), Some(true));
            }
        }
    }

    // Issue #3724: hook trust dialogs and each review screen require operator input.
    // Recorded dialog plus upstream 0.160.0 rendered browser snapshots cover both levels.
    #[test]
    fn codex_hook_review_needs_input() {
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");
        for screen in [
            include_str!("fixtures/codex-3724/trust-dialog.txt"),
            include_str!("fixtures/codex-3724/review-events.txt"),
            include_str!("fixtures/codex-3724/review-handler.txt"),
            include_str!("fixtures/codex-3724/review-untrusted.txt"),
            include_str!("fixtures/codex-3724/review-modified.txt"),
            include_str!("fixtures/codex-3724/review-managed.txt"),
        ] {
            assert_eq!(codex.classify_screen_attention(screen), Some(TerminalAttentionState::NeedsInput), "{screen}");
            assert_eq!(codex.classify_screen_obstruction(screen), Some(flotilla_resources::ScreenObstruction::HookReview), "{screen}");
        }
        // Empty/ordinary transcript text must not look like an interactive dialog.
        for screen in ["", "Hooks need review", "Hooks\nExample hook output", "Modified since last trusted"] {
            assert_ne!(codex.classify_screen_attention(screen), Some(TerminalAttentionState::NeedsInput), "{screen}");
            assert_eq!(codex.classify_screen_obstruction(screen), None, "{screen}");
        }
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

        assert_eq!(codex.classify_screen_attention(screen), Some(TerminalAttentionState::Working));
    }

    // #2599: previous submitted prompts remain on screen, especially after a
    // principal attach resizes the terminal. Only the bottom composer is live.
    #[hegel::test]
    fn codex_idle_composer_survives_visible_prompt_history(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Generate empty through eight submitted prompts, including a numeric
        // submitted prompt. Only the final live composer determines readiness.
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(8));
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");
        let mut screen = (0..count).map(|n| format!("› {n} submitted prompt\nresponse\n")).collect::<String>();
        // Background terminals are independent of the foreground Codex turn.
        let background_count = tc.draw(gs::integers::<usize>().min_value(0).max_value(8));
        let plural = if background_count == 1 { "" } else { "s" };
        screen.push_str(&format!(
            "Worked for 10m 43s • 3:56 PM\n\n› Ask Codex to do anything\n\n  \
             {background_count} background terminal{plural} running · /ps\n\ngpt-6.1-sol · /workspace"
        ));
        assert_eq!(codex.classify_screen_attention(&screen), Some(TerminalAttentionState::Idle));
    }

    // #2648: every empty-composer placeholder shipped by rust-v0.160.0 must
    // remain idle, including a cropped capture without the status footer.
    // Live r531 screen (2026-10-05): the shortcuts hint follows the model footer.
    #[test]
    fn codex_idle_composer_with_shortcuts_hint_is_idle() {
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");
        let footer = "\n\n  GPT-6.1-Sol default · /workspace · Read the crew brief\n  ? for shortcuts\n";
        let live = format!("• Implemented in PR #2670.\n\n  Worked for 39m 18s • 1:29 PM\n\n\n› Ask Codex to do anything{footer}");
        assert_eq!(codex.classify_screen_attention(&live), Some(TerminalAttentionState::Idle), "{live}");
        let blank = format!("› {footer}");
        assert_eq!(codex.classify_screen_attention(&blank), Some(TerminalAttentionState::Idle), "{blank}");
        let draft = format!("› fix the flaky test{footer}");
        assert_ne!(codex.classify_screen_attention(&draft), Some(TerminalAttentionState::Idle), "{draft}");
        // Live host-direct screens (kiwi): a wide status footer, and the hint
        // sharing its row with a right-aligned warning.
        let host_footer = "\n\n  GPT-6.1-Sol high · ~/dev/flotilla-repos/convoy-9f0a/issue-13-snapshot-arena · Read and follow crew brief · kiwi · Ready · Context 80% left · weekly 88% left · 421K used\n  ? for shortcuts                                                       ⚠ 1 warning · f2 to view";
        let host_idle = format!("› Ask Codex to do anything{host_footer}");
        assert_eq!(codex.classify_screen_attention(&host_idle), Some(TerminalAttentionState::Idle), "{host_idle}");
        let wrapped = "› Ask Codex to do anything\n\n  GPT-6.1-Sol high · ~/dev/flotilla-repos/convoy-5015/166-hover-card-detach · Read the coder brief · kiwi · Ready · Context 69% left · weekly…\n                                                                 ⚠ 1 warning · f2 to view";
        assert_eq!(codex.classify_screen_attention(wrapped), Some(TerminalAttentionState::Idle), "{wrapped}");
        let host_draft = format!("› i think{host_footer}");
        assert_ne!(codex.classify_screen_attention(&host_draft), Some(TerminalAttentionState::Idle), "{host_draft}");
    }

    // #3460: an idle Codex that cannot take a turn stays Unobservable, so
    // delivery still refuses, but the screen names why: a stray terminal
    // reply or a person's draft in the composer, or a scrolled-back view.
    // Idle, working and selection screens name nothing.
    #[test]
    fn codex_names_why_an_idle_screen_cannot_take_a_turn() {
        use flotilla_resources::ScreenObstruction;
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");
        let stray = include_str!("fixtures/codex-3460/stray-cell-size-reply.txt");
        assert_eq!(codex.classify_screen_attention(stray), Some(TerminalAttentionState::Unobservable));
        assert_eq!(
            codex.classify_screen_obstruction(stray),
            Some(ScreenObstruction::ComposerText { reply: Some("6;24;9t".into()), chars: 7 })
        );
        // A person's draft is counted, never copied, including one that starts
        // with a number like a selection row does.
        for (draft, chars) in [("please also check the docs", 26), ("3 tests fail. please fix", 24), ("12.5% slower", 12)] {
            let screen = stray.replace("› 6;24;9t", &format!("› {draft}"));
            assert_eq!(codex.classify_screen_obstruction(&screen), Some(ScreenObstruction::ComposerText { reply: None, chars }), "{draft}");
        }
        let hidden = include_str!("fixtures/codex-3460/scrolled-back-composer-hidden.txt");
        assert_eq!(codex.classify_screen_attention(hidden), Some(TerminalAttentionState::Unobservable));
        assert_eq!(codex.classify_screen_obstruction(hidden), Some(ScreenObstruction::ScrolledBack));
        for screen in [
            stray.replace("› 6;24;9t", "› Ask Codex to do anything"),
            include_str!("fixtures/codex-2927/scrolled-back.txt").to_string(),
            include_str!("fixtures/codex-2927/working-after-release.txt").to_string(),
            "Select reasoning effort\n› 1. High\n  2. Low".to_string(),
            "Do you trust the contents of this directory?\n› 1. Yes, continue\n  2. No, quit\n\nPress enter to continue".to_string(),
        ] {
            assert_eq!(codex.classify_screen_obstruction(&screen), None, "{screen}");
        }
        let claude = registry.get("claude-code").expect("claude adapter");
        assert_eq!(claude.classify_screen_obstruction(stray), None);
    }

    // #2927: scrolling an attached transcript must not hide the idle composer
    // or block delivery. These screens were captured from the real cleat cast.
    #[test]
    fn codex_captured_scrollback_remains_deliverable() {
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");
        assert_eq!(
            codex.classify_screen_attention(include_str!("fixtures/codex-2927/scrolled-back.txt")),
            Some(TerminalAttentionState::Idle)
        );
        let draft = include_str!("fixtures/codex-2927/scrolled-back.txt").replace("› Ask Codex to do anything", "› operator draft");
        assert_eq!(codex.classify_screen_attention(&draft), Some(TerminalAttentionState::Unobservable));
        assert_eq!(
            codex.classify_screen_attention(include_str!("fixtures/codex-2927/working-after-release.txt")),
            Some(TerminalAttentionState::Working)
        );
    }

    #[test]
    fn codex_pinned_placeholders_are_idle() {
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");
        // Independent source oracle: chatwidget.rs PLACEHOLDER and SIDE_PLACEHOLDER.
        for placeholder in ["Ask Codex to do anything", "Ask a follow-up question"] {
            for footer in ["", "\n\ngpt-6.1-sol · /workspace", "\n\ngpt-6.1-sol · ~/workspace"] {
                let screen = format!("› {placeholder}{footer}");
                assert_eq!(codex.classify_screen_attention(&screen), Some(TerminalAttentionState::Idle), "{screen}");
            }
        }
    }

    // A genuinely blank composer is idle only when the footer identifies it;
    // a cropped blank prompt row alone cannot prove a live composer exists.
    #[test]
    fn codex_blank_composer_requires_footer() {
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");
        assert_eq!(codex.classify_screen_attention("›   \n\ngpt-6.1-sol · /workspace"), Some(TerminalAttentionState::Idle));
        assert_eq!(codex.classify_screen_attention("›   "), Some(TerminalAttentionState::Unobservable));
    }

    // #2648: unknown composer text must block delivery, even with a footer or
    // when it begins with words from a placeholder. History must not mask it.
    #[hegel::test]
    fn codex_drafts_are_not_idle(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Generate zero through eight duplicate history rows and extensions of
        // 1..128 characters. Each run covers every named draft and footer shape.
        let drafts = [
            "please fix this",
            "1 typed draft",
            "Ask Codex",
            "Ask Codex to do something",
            "Ask Codex to do anything after checking with me",
            "Ask a follow-up question about my draft",
            "Run /review on my current changes",
            "Run /review after I finish typing",
            "unrecognised future placeholder",
            "Ask Codex to do anything\n  but wait for me",
            "\n  a draft with a blank first line",
            "Ask Codex to do anything\n  foo · /bar\n  still typing",
            "\n  foo · ~/bar\n  still typing",
        ];
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(8));
        let suffix = "x".repeat(tc.draw(gs::integers::<usize>().min_value(1).max_value(128)));
        let extended = format!("Ask Codex to do anything{suffix}");
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");
        for draft in drafts.into_iter().chain(std::iter::once(extended.as_str())) {
            for footer in ["", "\n\ngpt-6.1-sol · /workspace", "\n\ngpt-6.1-sol · ~/workspace"] {
                let history = "› Ask Codex to do anything\nprevious response\n".repeat(count);
                let screen = format!("{history}› {draft}{footer}");
                assert_eq!(codex.classify_screen_attention(&screen), Some(TerminalAttentionState::Unobservable), "{screen}");
            }
        }
    }

    // #2560: elapsed-time spinner redraws are not output progress, but actual
    // screen content changes are. Cover varied turn ages with explicit generation.
    #[hegel::test]
    fn codex_output_digest_ignores_spinner_age(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let age = tc.draw(gs::integers::<u64>());
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");
        let first = "tool output\n• Working (1s • esc to interrupt)\n› Ask Codex";
        let redraw = format!("tool output\n• Working ({age}s • esc to interrupt)\n› Ask Codex");
        assert_eq!(codex.screen_output_digest(first), codex.screen_output_digest(&redraw));
        assert_ne!(codex.screen_output_digest(first), codex.screen_output_digest(&format!("new tool output\n{redraw}")));
    }

    // A Codex selection row is not the composer; tool output mentioning the
    // interrupt shortcut must not override a real idle composer.
    #[test]
    fn codex_selection_rows_are_not_turn_boundaries() {
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");
        for screen in ["Select a model\n› gpt-6.1-sol (current)\n  gpt-6-astra", "Select reasoning effort\n› 1. High\n  2. Low"] {
            assert_eq!(codex.classify_screen_attention(screen), Some(TerminalAttentionState::Unobservable));
        }
        assert_eq!(
            codex.classify_screen_attention("tool output: esc to interrupt\n\n› Ask Codex to do anything\n\ngpt-6.1-sol · /workspace"),
            Some(TerminalAttentionState::Idle)
        );
    }

    // #2560: interrupt returns to the composer without emitting a notify hook.
    #[test]
    fn codex_interrupted_composer_is_a_turn_boundary() {
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");
        assert_eq!(
            codex.classify_screen_attention(
                "■ Conversation interrupted

› Ask Codex to do anything

gpt-6.1-sol · /workspace"
            ),
            Some(TerminalAttentionState::Idle)
        );
        assert_eq!(
            codex.classify_screen_attention(
                "• Working (2h • esc to interrupt)

› Ask Codex to do anything"
            ),
            Some(TerminalAttentionState::Working)
        );
    }

    #[test]
    fn codex_classifies_expired_and_unrefreshable_tokens_as_fatal() {
        let registry = discovered_registry();
        let codex = registry.get("codex").expect("codex adapter");

        assert_eq!(codex.classify_screen_failure("codex_apps: HTTP 401 TOKEN_EXPIRED"), Some("token_expired"));
        assert_eq!(
            codex.classify_screen_failure("Your access token could not be refreshed. Please log out and sign in again."),
            Some("access token could not be refreshed")
        );
        assert_eq!(codex.classify_screen_failure("› Ask Codex to do anything"), None);
    }

    // Real Git must ignore runtime files even with an unterminated existing
    // exclude file or a later negation. Higher-priority .gitignore overrides
    // must refuse launch rather than silently expose runtime files.
    #[tokio::test]
    async fn runtime_exclusion_handles_existing_patterns_and_refuses_overrides() {
        use flotilla_vcs::providers::git_worktree::GitWorktreeStrategy;
        use flotilla_vcs::vcs::FlotillaVcs;
        use flotilla_vcs::vcs::GitCheckoutStrategy;
        const RUNTIME_PATHS: &[&str] = &[".flotilla/briefs/coder.md", CLAUDE_MANAGED_SETTINGS_PATH];
        for (existing, ignore, overridden, tracks_declarations) in [
            ("previous-pattern", None, false, false),
            (".flotilla/\n!.flotilla/", None, false, false),
            ("", Some("!.flotilla/\n"), true, false),
            ("", Some("!.flotilla/agent-exits/*\n!.flotilla/briefs/\n"), false, false),
            // A repository may track its own `.flotilla/` declarations (flotilla
            // itself does); Git then never reports the directory as ignored.
            ("", None, false, true),
        ] {
            let repo = tempfile::tempdir().expect("checkout");
            assert!(ProcessCommand::new("git").args(["init", "-q"]).current_dir(repo.path()).status().expect("git init").success());
            if tracks_declarations {
                std::fs::create_dir_all(repo.path().join(".flotilla")).expect("declaration directory");
                std::fs::write(repo.path().join(".flotilla/environment.yaml"), "kind: Environment\n").expect("declaration");
                assert!(ProcessCommand::new("git")
                    .args(["-c", "user.email=t@example.com", "-c", "user.name=t", "add", ".flotilla/environment.yaml"])
                    .current_dir(repo.path())
                    .status()
                    .expect("track declaration")
                    .success());
                assert!(ProcessCommand::new("git")
                    .args(["-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-qm", "declarations"])
                    .current_dir(repo.path())
                    .status()
                    .expect("commit declaration")
                    .success());
            }
            std::fs::write(repo.path().join(".git/info/exclude"), existing).expect("existing exclusions");
            if let Some(ignore) = ignore {
                std::fs::write(repo.path().join(".gitignore"), ignore).expect("higher-priority rules");
            }
            let runner = Arc::new(ProcessCommandRunner);
            let cwd = ExecutionEnvironmentPath::new(repo.path());
            let vcs = FlotillaVcs::new(
                cwd,
                runner.clone(),
                GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(flotilla_config::default_checkout_path(), runner.clone()))),
            );
            let (result, concurrent) = tokio::join!(
                super::ensure_flotilla_git_exclude(&*runner, &vcs, repo.path(), RUNTIME_PATHS),
                super::ensure_flotilla_git_exclude(&*runner, &vcs, repo.path(), RUNTIME_PATHS),
            );
            if overridden {
                assert!(result.is_err(), "refuse launch when Git does not ignore runtime files");
                assert!(concurrent.is_err(), "concurrent preparation also refuses the override");
                continue;
            }
            result.expect("install effective exclusion");
            concurrent.expect("concurrent role installs effective exclusion");
            std::fs::create_dir_all(repo.path().join(".flotilla/agent-exits")).expect("receipt directory");
            std::fs::write(repo.path().join(crate::process::exit_receipt("crew")), "0\n").expect("receipt");
            std::fs::create_dir_all(repo.path().join(".flotilla/briefs")).expect("brief directory");
            std::fs::write(repo.path().join(".flotilla/briefs/coder.md"), "brief").expect("runtime brief");
            assert!(ProcessCommand::new("git").args(["add", "-A"]).current_dir(repo.path()).status().expect("add all").success());
            let added = ProcessCommand::new("git")
                .args(["diff", "--cached", "--name-only", "--", ".flotilla"])
                .current_dir(repo.path())
                .output()
                .expect("index");
            assert!(added.status.success());
            assert!(added.stdout.is_empty(), "runtime files must stay out of the index");
            if tracks_declarations {
                let tracked =
                    ProcessCommand::new("git").args(["ls-files", ".flotilla"]).current_dir(repo.path()).output().expect("tracked");
                assert_eq!(String::from_utf8_lossy(&tracked.stdout).trim(), ".flotilla/environment.yaml", "declarations stay tracked");
            }
        }
    }

    // A standing convoy's multi-repository workspace root is a plain directory:
    // nothing there can be committed, so preparation proceeds without an
    // exclusion while each repository it holds still proves its own.
    #[tokio::test]
    async fn runtime_exclusion_skips_a_plain_workspace_directory() {
        use flotilla_vcs::providers::git_worktree::GitWorktreeStrategy;
        use flotilla_vcs::vcs::FlotillaVcs;
        use flotilla_vcs::vcs::GitCheckoutStrategy;
        let workspace = tempfile::tempdir().expect("workspace root");
        let runner = Arc::new(ProcessCommandRunner);
        let vcs = FlotillaVcs::new(
            ExecutionEnvironmentPath::new(workspace.path()),
            runner.clone(),
            GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(flotilla_config::default_checkout_path(), runner.clone()))),
        );
        super::ensure_flotilla_git_exclude(&*runner, &vcs, workspace.path(), &[".flotilla/briefs/governor.md"])
            .await
            .expect("plain workspace root needs no exclusion");
        assert!(!workspace.path().join(".git").exists(), "no repository is created");
    }

    // Exclusion discovery and write failures are errors, never permission to
    // expose runtime files to git add -A. This is the lowest falsifying seam.
    #[tokio::test]
    async fn runtime_exclusion_refuses_discovery_and_write_failures() {
        use flotilla_vcs::providers::git_worktree::GitWorktreeStrategy;
        use flotilla_vcs::vcs::FlotillaVcs;
        use flotilla_vcs::vcs::GitCheckoutStrategy;
        for responses in [
            vec![Ok("true\n".to_string()), Err("exclude discovery unavailable".to_string())],
            vec![Ok("true\n".to_string()), Ok(String::new())],
            vec![Ok("true\n".to_string()), Ok(".git/info/exclude\n".to_string()), Err("exclude is read-only".to_string())],
            // Work-tree inspection failure refuses too; it never means "no checkout".
            vec![Err("work tree inspection unavailable".to_string())],
            // Inside a bare repository or a `.git` directory, Git answers `false`.
            vec![Ok("false\n".to_string())],
        ] {
            let runner = Arc::new(MockRunner::new(responses));
            let cwd = ExecutionEnvironmentPath::new("/checkout");
            let vcs = FlotillaVcs::new(
                cwd.clone(),
                runner.clone(),
                GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(flotilla_config::default_checkout_path(), runner.clone()))),
            );
            assert!(super::ensure_flotilla_git_exclude(&*runner, &vcs, cwd.as_path(), &[".flotilla/briefs/coder.md"]).await.is_err());
            assert_eq!(runner.remaining(), 0);
        }
    }

    // Failure to discover or install the exclusion refuses preparation before
    // any runtime file is written. This example covers the two subprocess edges.
    #[tokio::test]
    async fn preparation_refuses_unavailable_runtime_file_exclusion() {
        use flotilla_vcs::providers::git_worktree::GitWorktreeStrategy;
        use flotilla_vcs::vcs::FlotillaVcs;
        use flotilla_vcs::vcs::GitCheckoutStrategy;
        for responses in [
            vec![Ok("true\n".to_string()), Err("exclude discovery unavailable".to_string())],
            vec![Ok("true\n".to_string()), Ok(String::new())],
            vec![Ok("true\n".to_string()), Ok(".git/info/exclude\n".to_string()), Err("exclude is read-only".to_string())],
            // Work-tree inspection failure refuses too; it never means "no checkout".
            vec![Err("work tree inspection unavailable".to_string())],
            // Inside a bare repository or a `.git` directory, Git answers `false`.
            vec![Ok("false\n".to_string())],
        ] {
            let runner = Arc::new(MockRunner::new(responses));
            let env = EnvironmentBag::new()
                .with(EnvironmentAssertion::env_var("CODEX_HOME", "/codex"))
                .with(EnvironmentAssertion::binary("codex", "/tools/codex"));
            let registry = AgentAdapterRegistry::discover(&env, runner.clone());
            let cwd = ExecutionEnvironmentPath::new("/checkout");
            let vcs = FlotillaVcs::new(
                cwd.clone(),
                runner.clone(),
                GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(flotilla_config::default_checkout_path(), runner.clone()))),
            );
            let brief = flotilla_resources::TerminalBrief {
                path: ".flotilla/briefs/coder.md".into(),
                content: "assignment".into(),
                artifact_digest: None,
                copies: Vec::new(),
            };
            assert!(registry.get("codex").expect("adapter").prepare_with_vcs(&cwd, &brief, &Vec::new(), &vcs).await.is_err());
            assert_eq!(runner.remaining(), 0);
            assert!(
                runner.calls().iter().all(|(cmd, args)| cmd == "git" || (cmd == "sh" && args.iter().any(|arg| arg.contains("exclude=")))),
                "no runtime files written after exclusion failure"
            );
        }
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
            .with(EnvironmentAssertion::binary("codex", "/tools/codex"))
            .with(EnvironmentAssertion::binary("git", "/usr/bin/git"));
        let registry = AgentAdapterRegistry::discover(&env, Arc::new(ProcessCommandRunner));
        let vcs = GitVcsFactory
            .probe(&env, &TestConfig::new(temp.path()), &ExecutionEnvironmentPath::new(&repo), Arc::new(ProcessCommandRunner))
            .await
            .expect("discovered VCS");
        let brief = flotilla_resources::TerminalBrief {
            artifact_digest: None,
            path: ".flotilla/briefs/coder.md".into(),
            content: "secret assignment".into(),
            copies: Vec::new(),
        };
        registry
            .get("codex")
            .expect("codex adapter")
            .prepare_with_vcs(&ExecutionEnvironmentPath::new(repo.to_str().expect("utf-8 repo path")), &brief, &Vec::new(), vcs.as_ref())
            .await
            .expect("prepare brief");

        // Adopted linked worktrees have a .git file, not a directory. Multiple
        // crew roles must share exclusion safely and git add -A must omit receipts.
        let adopted = temp.path().join("adopted checkout");
        assert!(ProcessCommand::new("git")
            .args(["-C", repo.to_str().expect("repo"), "worktree", "add", "-q", "-b", "adopted", adopted.to_str().expect("adopted")])
            .status()
            .expect("adopted worktree")
            .success());
        let adopted_path = ExecutionEnvironmentPath::new(&adopted);
        let adopted_vcs = GitVcsFactory
            .probe(&env, &TestConfig::new(temp.path()), &adopted_path, Arc::new(ProcessCommandRunner))
            .await
            .expect("adopted vcs");
        for role in ["coder", "reviewer"] {
            let role_brief = flotilla_resources::TerminalBrief { path: format!(".flotilla/briefs/{role}.md"), ..brief.clone() };
            registry
                .get("codex")
                .expect("adapter")
                .prepare_with_vcs(&adopted_path, &role_brief, &Vec::new(), adopted_vcs.as_ref())
                .await
                .expect("adopted crew preparation");
            let receipt = crate::process::exit_receipt(role);
            std::fs::create_dir_all(adopted.join(".flotilla/agent-exits")).expect("receipt dir");
            std::fs::write(adopted.join(receipt), "0\n").expect("receipt");
        }
        assert!(ProcessCommand::new("git").args(["-C", adopted.to_str().expect("path"), "add", "-A"]).status().expect("add all").success());
        let adopted_status =
            ProcessCommand::new("git").args(["-C", adopted.to_str().expect("path"), "status", "--short"]).output().expect("adopted status");
        assert!(adopted_status.status.success());
        assert!(adopted_status.stdout.is_empty(), "runtime receipts and briefs cannot enter a commit");

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
