use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_protocol::arg::{flatten, Arg};
use flotilla_resources::TerminalBrief;
use serde::Serialize;

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{discovery::EnvironmentBag, ChannelLabel, CommandRunner},
};

pub const TRUSTED_IMPLICIT_STANCE: &str = "trusted-implicit";
pub const DEFAULT_CREW_BRIEF_TEMPLATE: &str = "crew.md";
const BUILTIN_CREW_BRIEF_TEMPLATE: &str = include_str!("agent_adapter/templates/crew.md");
const BUILTIN_INTERACTIVE_SESSION_BRIEF_TEMPLATE: &str = include_str!("agent_adapter/templates/interactive-session.md");
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
        let template = template.filter(|template| !template.trim().is_empty()).unwrap_or(DEFAULT_CREW_BRIEF_TEMPLATE);
        let mut overrides = Vec::new();
        if let Some(config_dir) = &self.config_dir {
            push_template_override(&mut overrides, config_dir.join(BRIEF_TEMPLATE_DIR).join(template));
            if let Some(project_ref) = project_ref {
                push_template_override(
                    &mut overrides,
                    config_dir.join("projects").join(project_ref).join(BRIEF_TEMPLATE_DIR).join(template),
                );
            }
        }
        for repo_root in repo_roots {
            push_template_override(&mut overrides, repo_root.join(".flotilla").join(BRIEF_TEMPLATE_DIR).join(template));
        }
        CrewBriefRenderOptions { template: template.to_string(), overrides }
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
}

impl Default for CrewBriefRenderOptions {
    fn default() -> Self {
        Self { template: DEFAULT_CREW_BRIEF_TEMPLATE.to_string(), overrides: Vec::new() }
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
    let mut skip_overrides = 0;
    let mut current_template = match options.template.as_str() {
        DEFAULT_CREW_BRIEF_TEMPLATE => BUILTIN_CREW_BRIEF_TEMPLATE_NAME.to_string(),
        "interactive-session" | "interactive-session.md" => BUILTIN_INTERACTIVE_SESSION_BRIEF_TEMPLATE_NAME.to_string(),
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
    env.get_template(&current_template)
        .and_then(|template| template.render(context))
        .map_err(|err| format!("render crew brief template {}: {err}", options.template))
}

const BUILTIN_CREW_BRIEF_TEMPLATE_NAME: &str = "builtin/crew.md";
const BUILTIN_INTERACTIVE_SESSION_BRIEF_TEMPLATE_NAME: &str = "builtin/interactive-session.md";

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
) {
    content.push_str("\n\n## Work context\n\n");
    if let Some(branch) = &convoy.spec.r#ref {
        content.push_str(&format!("- Branch: `{branch}`\n"));
    }
    content.push_str("- Repositories:\n");
    for repository in convoy.spec.repositories.iter().filter(|repository| repository_refs.contains(&repository.repo_ref)) {
        content.push_str(&format!("  - `{}` — {}\n", repository.repo_ref, repository.url));
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
    async fn cleanup(&self, _cwd: &ExecutionEnvironmentPath, _brief: &TerminalBrief) -> Result<(), String> {
        Ok(())
    }
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
        self.runner.write_file(&cwd.as_path().join(&brief.path), &brief.content).await?;
        ensure_flotilla_git_exclude(&*self.runner, cwd.as_path()).await
    }

    async fn cleanup(&self, cwd: &ExecutionEnvironmentPath, brief: &TerminalBrief) -> Result<(), String> {
        remove_agent_brief(&*self.runner, cwd.as_path(), brief).await
    }

    fn launch(&self, request: &AgentLaunchRequest) -> Result<AgentLaunchPlan, String> {
        Ok(AgentLaunchPlan { command: self.command(request), env: Vec::new(), stance: TRUSTED_IMPLICIT_STANCE.into() })
    }
}

async fn ensure_flotilla_git_exclude(runner: &dyn CommandRunner, cwd: &Path) -> Result<(), String> {
    let Ok(output) = runner.run_output("git", &["rev-parse", "--git-path", "info/exclude"], cwd, &ChannelLabel::Noop).await else {
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
    let _ = runner.run("sh", &["-lc", &script], cwd, &ChannelLabel::Noop).await;
    Ok(())
}

async fn remove_agent_brief(runner: &dyn CommandRunner, cwd: &Path, brief: &TerminalBrief) -> Result<(), String> {
    let path = cwd.join(&brief.path);
    let path_str = path.to_str().ok_or_else(|| format!("brief path is not valid UTF-8: {}", path.display()))?;
    runner.run("rm", &["-f", path_str], Path::new("/"), &ChannelLabel::Noop).await?;

    for parent in [path.parent(), path.parent().and_then(Path::parent)].into_iter().flatten() {
        if parent != cwd {
            let Some(parent) = parent.to_str() else {
                continue;
            };
            let _ = runner.run("rmdir", &[parent], Path::new("/"), &ChannelLabel::Noop).await;
        }
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

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.adapters.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, process::Command as ProcessCommand, sync::Arc};

    use chrono::Utc;
    use flotilla_protocol::{IssueRef, IssueSource, IssueState};
    use flotilla_resources::{
        single_agent_contained_workflow_spec, Convoy, ConvoyIssue, ConvoyRepositorySpec, ConvoySpec, CrewSource, IssueSnapshot, ObjectMeta,
        RepositoryKey, ResourceObject, TerminalCrewContext,
    };

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
            .with(EnvironmentAssertion::binary("claude", "/tools/claude"))
            .with(EnvironmentAssertion::binary("codex", "/tools/codex"));
        AgentAdapterRegistry::discover(&env, Arc::new(MockRunner::new(vec![Ok(".git/info/exclude\n".into()), Ok(String::new())])))
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
        let brief = build_crew_brief(
            &TerminalCrewContext {
                namespace: "flotilla".to_string(),
                convoy: "fix-delivery".to_string(),
                vessel_ref: "vessel-fix-delivery-work".to_string(),
            },
            "work",
            "coder",
            prompt.as_deref().map_or(CrewAssignment::Unassigned, CrewAssignment::Prompt),
            &[CrewBriefMember { role: "coder".to_string(), state: "active".to_string(), is_agent: true }],
        );

        assert!(brief.content.contains(
            "For assignments that change a repository, delivery is part of the assignment: implement the change, push the branch, open a pull request that closes the issue (ready for review, never a draft), and shepherd the pull request until all checks pass; if it is a draft for any reason, mark it ready once checks are green. Do not merge it. Only then complete your assignment with `flotilla crew complete --message '<PR URL>'`."
        ));
        assert!(brief.content.contains("Clone scratch repositories outside the vessel checkout"));
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
    fn default_brief_template_matches_legacy_bytes() {
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

        assert_eq!(
            content,
            "# Flotilla crew brief\n\nYou are `coder` in convoy `fix-delivery`, aboard vessel `work` (`vessel-fix-delivery-work`).\n\n## Crew\n\n- `coder`: active\n- `reviewer`: latent\n- `watcher`: active\n\nRun `flotilla crew list` for current crew state.\nClone scratch repositories outside the vessel checkout (for example under a `mktemp -d` directory); embedded repositories make teardown refuse by default.\nHand off to reviewer with `flotilla crew reviewer handoff --message '...'`.\nFor assignments that change a repository, delivery is part of the assignment: implement the change, push the branch, open a pull request that closes the issue (ready for review, never a draft), and shepherd the pull request until all checks pass; if it is a draft for any reason, mark it ready once checks are green. Do not merge it. Only then complete your assignment with `flotilla crew complete --message '<PR URL>'`. For other assignments, complete with `flotilla crew complete --message '...'`. If the assignment cannot be completed, report the failure with `flotilla crew fail --message '...'`.\n\n## Assignment\n\nFix the flux capacitor.\n"
        );
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
            &CrewBriefRenderOptions { template: "interactive-session.md".to_string(), overrides: Vec::new() },
        )
        .expect("render selected template")
        .content;

        assert!(selected.contains("For interactive sessions, keep the user-facing loop tight"));
        assert!(selected.contains("## Assignment\n\nPair with the user."));
        assert!(!default.contains("For interactive sessions"));
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
            },
            spec: ConvoySpec {
                workflow_ref: "workflow".to_string(),
                dispatching_principal_ref: Default::default(),
                inputs: BTreeMap::new(),
                placement_policy: None,
                repositories: vec![ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/flotilla".to_string(),
                    repo_ref: repo_ref.clone(),
                    base_ref: "main".to_string(),
                    workspace_slug: "flotilla".to_string(),
                    subpaths: Vec::new(),
                }],
                r#ref: Some("fix/batch".to_string()),
                project_ref: None,
                adopted_checkout_refs: BTreeMap::new(),
                issues: vec![issue("809", "First issue", "First issue body."), issue("810", "Second issue", "Second issue body.")],
                instruction: None,
            },
            status: None,
        };
        let mut content = String::new();
        append_convoy_work_context(&mut content, &convoy, &[repo_ref]);

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

        let env = EnvironmentBag::new().with(EnvironmentAssertion::binary("codex", "/tools/codex"));
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
