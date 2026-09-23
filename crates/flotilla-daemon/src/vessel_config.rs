use std::{collections::BTreeMap, fmt};

pub mod order {
    pub const FIRST: u16 = 0;
    pub const EARLY: u16 = 500;
    pub const DEFAULT: u16 = 1000;
    pub const LATE: u16 = 1500;
}

pub mod priority {
    pub const FORCE: u16 = 50;
    pub const NORMAL: u16 = 100;
    pub const DEFAULT: u16 = 1000;
}

const CREW_GIT_IDENTITY_PROVENANCE: &str = "vessel/crew-identity";
const CREW_GIT_USER_NAME: &str = "flotilla-crew[bot]";
const CREW_GIT_USER_EMAIL: &str = "309902803+flotilla-crew[bot]@users.noreply.github.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetId {
    GitConfig,
    AgentEnvironment,
}

impl fmt::Display for TargetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitConfig => formatter.write_str("gitconfig"),
            Self::AgentEnvironment => formatter.write_str("agent-environment"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetKey {
    GitConfig(GitConfigKey),
    AgentEnvironment(AgentEnvironmentKey),
}

impl fmt::Display for TargetKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitConfig(key) => key.fmt(formatter),
            Self::AgentEnvironment(key) => key.fmt(formatter),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AgentEnvironmentKey(String);

impl AgentEnvironmentKey {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    fn is_valid(&self) -> bool {
        let mut characters = self.0.chars();
        characters.next().is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
            && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    }
}

impl fmt::Display for AgentEnvironmentKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitConfigKey {
    section: String,
    subsection: Option<String>,
    name: String,
}

impl GitConfigKey {
    pub fn new(section: impl Into<String>, name: impl Into<String>) -> Self {
        Self { section: section.into(), subsection: None, name: name.into() }
    }

    pub fn subsection(section: impl Into<String>, subsection: impl Into<String>, name: impl Into<String>) -> Self {
        Self { section: section.into(), subsection: Some(subsection.into()), name: name.into() }
    }

    fn contains_line_break(&self) -> bool {
        self.section.contains(['\r', '\n'])
            || self.subsection.as_ref().is_some_and(|subsection| subsection.contains(['\r', '\n']))
            || self.name.contains(['\r', '\n'])
    }
}

impl fmt::Display for GitConfigKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.subsection {
            Some(subsection) => write!(formatter, "{}.\"{}\".{}", self.section, subsection, self.name),
            None => write!(formatter, "{}.{}", self.section, self.name),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merge {
    Set,
    Append,
    ErrorOnDuplicate,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Provenance(String);

impl Provenance {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    fn contains_line_break(&self) -> bool {
        self.0.contains(['\r', '\n'])
    }
}

impl fmt::Display for Provenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct Fragment {
    pub target: TargetId,
    pub key: TargetKey,
    pub value: String,
    #[builder(default = order::DEFAULT)]
    pub order: u16,
    #[builder(default = priority::NORMAL)]
    pub priority: u16,
    #[builder(default = Merge::Set)]
    pub merge: Merge,
    pub provenance: Provenance,
}

pub fn agent_environment_fragment(name: impl Into<String>, value: impl Into<String>, provenance: impl Into<String>) -> Fragment {
    Fragment::builder()
        .target(TargetId::AgentEnvironment)
        .key(TargetKey::AgentEnvironment(AgentEnvironmentKey::new(name)))
        .value(value)
        .provenance(Provenance::new(provenance))
        .build()
}

pub fn crew_gitconfig_fragments() -> [Fragment; 2] {
    [
        Fragment::builder()
            .target(TargetId::GitConfig)
            .key(TargetKey::GitConfig(GitConfigKey::new("user", "name")))
            .value(CREW_GIT_USER_NAME)
            .priority(priority::DEFAULT)
            .provenance(Provenance::new(CREW_GIT_IDENTITY_PROVENANCE))
            .build(),
        Fragment::builder()
            .target(TargetId::GitConfig)
            .key(TargetKey::GitConfig(GitConfigKey::new("user", "email")))
            .value(CREW_GIT_USER_EMAIL)
            .priority(priority::DEFAULT)
            .provenance(Provenance::new(CREW_GIT_IDENTITY_PROVENANCE))
            .build(),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedFile {
    pub target: TargetId,
    pub contents: String,
    pub environment: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeError {
    InvalidProvenance { provenance: Provenance },
    InvalidKey { provenance: Provenance },
    InvalidValue { key: TargetKey, provenance: Provenance },
    TargetKeyMismatch { target: TargetId, key: TargetKey, provenance: Provenance },
    SetConflict { key: TargetKey, first: Provenance, second: Provenance },
    MergePolicyConflict { key: TargetKey, first: Provenance, second: Provenance },
    Duplicate { key: TargetKey, first: Provenance, second: Provenance },
    UnsupportedAppend { key: TargetKey, provenance: Provenance },
}

impl fmt::Display for ComposeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProvenance { provenance } => {
                write!(formatter, "fragment provenance `{provenance}` contains a line break")
            }
            Self::InvalidKey { provenance } => {
                write!(formatter, "fragment from `{provenance}` has a key containing a line break")
            }
            Self::InvalidValue { key, provenance } => {
                write!(formatter, "fragment from `{provenance}` has a value containing a line break for key `{key}`")
            }
            Self::TargetKeyMismatch { target, key, provenance } => {
                write!(formatter, "fragment from `{provenance}` uses key `{key}` with incompatible target `{target}`")
            }
            Self::SetConflict { key, first, second } => {
                write!(formatter, "equal-priority Set fragments from `{first}` and `{second}` conflict for key `{key}`")
            }
            Self::MergePolicyConflict { key, first, second } => {
                write!(formatter, "fragments from `{first}` and `{second}` declare different merge policies for key `{key}`")
            }
            Self::Duplicate { key, first, second } => {
                write!(formatter, "duplicate fragments from `{first}` and `{second}` are forbidden for key `{key}`")
            }
            Self::UnsupportedAppend { key, provenance } => {
                write!(formatter, "fragment from `{provenance}` cannot append single-valued agent environment key `{key}`")
            }
        }
    }
}

impl std::error::Error for ComposeError {}

pub fn compose(target: TargetId, fragments: impl IntoIterator<Item = Fragment>) -> Result<ComposedFile, ComposeError> {
    let mut fragments = fragments.into_iter().filter(|fragment| fragment.target == target).collect::<Vec<_>>();
    if let Some(fragment) = fragments.iter().find(|fragment| fragment.provenance.contains_line_break()) {
        return Err(ComposeError::InvalidProvenance { provenance: fragment.provenance.clone() });
    }
    fragments.sort_by(|left, right| (left.order, &left.provenance).cmp(&(right.order, &right.provenance)));

    match target {
        TargetId::GitConfig => compose_gitconfig(fragments),
        TargetId::AgentEnvironment => compose_agent_environment(fragments),
    }
}

fn compose_gitconfig(fragments: Vec<Fragment>) -> Result<ComposedFile, ComposeError> {
    for fragment in &fragments {
        match &fragment.key {
            TargetKey::GitConfig(key) => {
                if key.contains_line_break() {
                    return Err(ComposeError::InvalidKey { provenance: fragment.provenance.clone() });
                }
            }
            TargetKey::AgentEnvironment(_) => {
                return Err(ComposeError::TargetKeyMismatch {
                    target: TargetId::GitConfig,
                    key: fragment.key.clone(),
                    provenance: fragment.provenance.clone(),
                });
            }
        }
        if fragment.value.contains(['\r', '\n']) {
            return Err(ComposeError::InvalidValue { key: fragment.key.clone(), provenance: fragment.provenance.clone() });
        }
    }

    let rendered = resolve_fragments(&fragments, true)?;

    let contents = rendered.into_iter().map(render_gitconfig_entry).collect::<Vec<_>>().join("\n");
    Ok(ComposedFile { target: TargetId::GitConfig, contents, environment: Vec::new() })
}

fn compose_agent_environment(fragments: Vec<Fragment>) -> Result<ComposedFile, ComposeError> {
    for fragment in &fragments {
        match &fragment.key {
            TargetKey::AgentEnvironment(key) if key.is_valid() => {}
            TargetKey::AgentEnvironment(_) => {
                return Err(ComposeError::InvalidKey { provenance: fragment.provenance.clone() });
            }
            TargetKey::GitConfig(_) => {
                return Err(ComposeError::TargetKeyMismatch {
                    target: TargetId::AgentEnvironment,
                    key: fragment.key.clone(),
                    provenance: fragment.provenance.clone(),
                });
            }
        }
        if fragment.value.contains(['\0', '\r', '\n']) {
            return Err(ComposeError::InvalidValue { key: fragment.key.clone(), provenance: fragment.provenance.clone() });
        }
    }

    let rendered = resolve_fragments(&fragments, false)?;
    let contents = rendered
        .iter()
        .map(|entry| {
            let TargetKey::AgentEnvironment(key) = &entry.fragment.key else { unreachable!("target key validated above") };
            let comments = entry.provenances.iter().map(|provenance| format!("# fragment: {provenance}\n")).collect::<String>();
            format!("{comments}export {key}={}\n", shell_quote(&entry.fragment.value))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let environment = rendered
        .into_iter()
        .map(|entry| {
            let TargetKey::AgentEnvironment(key) = &entry.fragment.key else { unreachable!("target key validated above") };
            (key.0.clone(), entry.fragment.value.clone())
        })
        .collect();
    Ok(ComposedFile { target: TargetId::AgentEnvironment, contents, environment })
}

fn resolve_fragments(fragments: &[Fragment], append_supported: bool) -> Result<Vec<RenderedEntry<'_>>, ComposeError> {
    let minimum_priorities = fragments.iter().fold(BTreeMap::<TargetKey, u16>::new(), |mut priorities, fragment| {
        priorities
            .entry(fragment.key.clone())
            .and_modify(|priority| *priority = (*priority).min(fragment.priority))
            .or_insert(fragment.priority);
        priorities
    });
    let mut winners = BTreeMap::<TargetKey, Vec<&Fragment>>::new();
    for fragment in fragments {
        if minimum_priorities.get(&fragment.key) == Some(&fragment.priority) {
            winners.entry(fragment.key.clone()).or_default().push(fragment);
        }
    }

    let mut rendered = Vec::new();
    for fragments_for_key in winners.values() {
        let first = fragments_for_key[0];
        if let Some(other) = fragments_for_key.iter().skip(1).find(|fragment| fragment.merge != first.merge) {
            return Err(ComposeError::MergePolicyConflict {
                key: first.key.clone(),
                first: first.provenance.clone(),
                second: other.provenance.clone(),
            });
        }
        match first.merge {
            Merge::Set => {
                if let Some(other) = fragments_for_key.iter().skip(1).find(|fragment| fragment.value != first.value) {
                    return Err(ComposeError::SetConflict {
                        key: first.key.clone(),
                        first: first.provenance.clone(),
                        second: other.provenance.clone(),
                    });
                }
                rendered.push(RenderedEntry {
                    fragment: first,
                    provenances: fragments_for_key.iter().map(|fragment| &fragment.provenance).collect(),
                });
            }
            Merge::Append if append_supported => {
                rendered
                    .extend(fragments_for_key.iter().map(|fragment| RenderedEntry { fragment, provenances: vec![&fragment.provenance] }));
            }
            Merge::Append => {
                return Err(ComposeError::UnsupportedAppend { key: first.key.clone(), provenance: first.provenance.clone() });
            }
            Merge::ErrorOnDuplicate => {
                if let Some(other) = fragments_for_key.get(1) {
                    return Err(ComposeError::Duplicate {
                        key: first.key.clone(),
                        first: first.provenance.clone(),
                        second: other.provenance.clone(),
                    });
                }
                rendered.push(RenderedEntry { fragment: first, provenances: vec![&first.provenance] });
            }
        }
    }
    rendered
        .sort_by(|left, right| (left.fragment.order, &left.fragment.provenance).cmp(&(right.fragment.order, &right.fragment.provenance)));
    Ok(rendered)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct RenderedEntry<'a> {
    fragment: &'a Fragment,
    provenances: Vec<&'a Provenance>,
}

fn render_gitconfig_entry(entry: RenderedEntry<'_>) -> String {
    let TargetKey::GitConfig(key) = &entry.fragment.key else { unreachable!("target key validated before rendering") };
    let comments = entry.provenances.into_iter().map(|provenance| format!("# fragment: {provenance}\n")).collect::<String>();
    let section = match &key.subsection {
        Some(subsection) => format!("[{} \"{}\"]", key.section, escape_gitconfig_subsection(subsection)),
        None => format!("[{}]", key.section),
    };
    format!("{comments}{section}\n\t{} = {}\n", key.name, entry.fragment.value)
}

fn escape_gitconfig_subsection(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_fragment(key: GitConfigKey, value: &str, provenance: &str) -> Fragment {
        Fragment::builder()
            .target(TargetId::GitConfig)
            .key(TargetKey::GitConfig(key))
            .value(value)
            .provenance(Provenance::new(provenance))
            .build()
    }

    #[test]
    fn sugar_constants_match_the_ratified_order_and_priority_contract() {
        assert_eq!((order::FIRST, order::EARLY, order::DEFAULT, order::LATE), (0, 500, 1000, 1500));
        assert_eq!((priority::FORCE, priority::NORMAL, priority::DEFAULT), (50, 100, 1000));
    }

    #[test]
    fn crew_gitconfig_uses_the_canonical_github_app_identity() {
        let composed = compose(TargetId::GitConfig, crew_gitconfig_fragments()).expect("crew Git identity should compose");

        assert!(composed.contents.contains("[user]\n\tname = flotilla-crew[bot]"));
        assert!(composed.contents.contains("[user]\n\temail = 309902803+flotilla-crew[bot]@users.noreply.github.com"));
    }

    #[test]
    fn crew_gitconfig_identity_is_a_baseline_that_explicit_fragments_can_override() {
        let explicit = git_fragment(GitConfigKey::new("user", "email"), "developer@example.com", "vessel/explicit-identity");
        let composed = compose(TargetId::GitConfig, crew_gitconfig_fragments().into_iter().chain([explicit]))
            .expect("an explicit Git identity should override the crew baseline");

        assert!(composed.contents.contains("[user]\n\temail = developer@example.com"));
        assert!(!composed.contents.contains(CREW_GIT_USER_EMAIL));
    }

    #[test]
    fn equal_priority_different_set_values_name_the_key_and_both_contributors() {
        let key = GitConfigKey::new("user", "email");
        let error = compose(TargetId::GitConfig, [
            git_fragment(key.clone(), "first@example.com", "credential/first"),
            git_fragment(key, "second@example.com", "credential/second"),
        ])
        .expect_err("equal-priority Set values must conflict")
        .to_string();

        assert!(error.contains("user.email"), "error must name the key: {error}");
        assert!(error.contains("credential/first"), "error must name the first contributor: {error}");
        assert!(error.contains("credential/second"), "error must name the second contributor: {error}");
    }

    #[test]
    fn agent_environment_renders_provenance_and_single_valued_delivery() {
        let composed = compose(TargetId::AgentEnvironment, [agent_environment_fragment(
            "CODEX_HOME",
            "/run/flotilla/codex",
            "agent-material/codex codex-login",
        )])
        .expect("agent environment should compose");

        assert_eq!(composed.contents, "# fragment: agent-material/codex codex-login\nexport CODEX_HOME='/run/flotilla/codex'\n");
        assert_eq!(composed.environment, [("CODEX_HOME".to_string(), "/run/flotilla/codex".to_string())]);
    }

    #[test]
    fn credential_and_agent_material_conflict_names_both_contributors() {
        let error = compose(TargetId::AgentEnvironment, [
            agent_environment_fragment("CODEX_HOME", "/run/flotilla/codex", "agent-material/codex codex-login"),
            agent_environment_fragment("CODEX_HOME", "/run/flotilla/credentials/openai/codex", "credential/codex openai"),
        ])
        .expect_err("two Codex homes must conflict")
        .to_string();

        assert!(error.contains("CODEX_HOME"), "error must name the key: {error}");
        assert!(error.contains("agent-material/codex codex-login"), "error must name agent material: {error}");
        assert!(error.contains("credential/codex openai"), "error must name the credential: {error}");
    }

    #[test]
    fn agent_environment_rejects_invalid_names_and_values() {
        let invalid_name =
            compose(TargetId::AgentEnvironment, [agent_environment_fragment("CODEX-HOME", "/run/flotilla/codex", "agent-material/codex")])
                .expect_err("environment names must be shell identifiers")
                .to_string();
        assert!(invalid_name.contains("agent-material/codex"));

        let invalid_value = compose(TargetId::AgentEnvironment, [agent_environment_fragment(
            "CODEX_HOME",
            "/run/flotilla/codex\0injected",
            "agent-material/codex",
        )])
        .expect_err("environment values must not contain NUL")
        .to_string();
        assert!(invalid_value.contains("CODEX_HOME"));
        assert!(invalid_value.contains("agent-material/codex"));
    }

    #[test]
    fn agent_environment_rejects_append_for_single_valued_keys() {
        let mut fragment = agent_environment_fragment("CODEX_HOME", "/run/flotilla/codex", "agent-material/codex");
        fragment.merge = Merge::Append;

        let error =
            compose(TargetId::AgentEnvironment, [fragment]).expect_err("agent environment values must stay single-valued").to_string();

        assert!(error.contains("cannot append"));
        assert!(error.contains("CODEX_HOME"));
        assert!(error.contains("agent-material/codex"));
    }

    #[test]
    fn lower_priority_number_overrides_set_fragments() {
        let key = GitConfigKey::new("user", "email");
        let mut default = git_fragment(key.clone(), "default@example.com", "credential/default");
        default.priority = priority::DEFAULT;
        let mut forced = git_fragment(key, "forced@example.com", "credential/forced");
        forced.priority = priority::FORCE;
        let composed = compose(TargetId::GitConfig, [default, forced]).expect("explicit override should compose");

        assert!(composed.contents.contains("forced@example.com"));
        assert!(!composed.contents.contains("default@example.com"));
    }

    #[test]
    fn append_follows_order_then_provenance_stably() {
        let key = GitConfigKey::subsection("credential", "https://example.com", "helper");
        let fragment = |key, value, provenance, order| {
            Fragment::builder()
                .target(TargetId::GitConfig)
                .key(TargetKey::GitConfig(key))
                .value(value)
                .order(order)
                .merge(Merge::Append)
                .provenance(Provenance::new(provenance))
                .build()
        };
        let composed = compose(TargetId::GitConfig, [
            fragment(key.clone(), "late", "credential/alpha", order::LATE),
            fragment(key.clone(), "same-order-z", "credential/zulu", order::EARLY),
            fragment(key.clone(), "same-order-a-first", "credential/alpha", order::EARLY),
            fragment(key, "same-order-a-second", "credential/alpha", order::EARLY),
        ])
        .expect("append fragments should compose");

        let alpha_first = composed.contents.find("same-order-a-first").expect("first alpha fragment");
        let alpha_second = composed.contents.find("same-order-a-second").expect("second alpha fragment");
        let zulu = composed.contents.find("same-order-z").expect("zulu fragment");
        let late = composed.contents.find("late").expect("late fragment");
        assert!(alpha_first < alpha_second && alpha_second < zulu && zulu < late, "unexpected append order:\n{}", composed.contents);
    }

    #[test]
    fn error_on_duplicate_rejects_a_second_winning_fragment() {
        let key = GitConfigKey::new("core", "hooksPath");
        let mut first = git_fragment(key.clone(), "/first", "workspace/first");
        first.merge = Merge::ErrorOnDuplicate;
        let mut second = git_fragment(key, "/second", "workspace/second");
        second.merge = Merge::ErrorOnDuplicate;
        let error =
            compose(TargetId::GitConfig, [first, second]).expect_err("ErrorOnDuplicate must reject a second contributor").to_string();

        assert!(error.contains("core.hooksPath"));
        assert!(error.contains("workspace/first"));
        assert!(error.contains("workspace/second"));
    }

    #[test]
    fn different_merge_policies_name_the_key_and_both_contributors() {
        let key = GitConfigKey::new("user", "email");
        let first = git_fragment(key.clone(), "first@example.com", "credential/first");
        let mut second = git_fragment(key, "second@example.com", "credential/second");
        second.merge = Merge::Append;

        let error = compose(TargetId::GitConfig, [first, second]).expect_err("winning fragments must agree on merge policy").to_string();

        assert!(error.contains("user.email"));
        assert!(error.contains("credential/first"));
        assert!(error.contains("credential/second"));
    }

    #[test]
    fn provenance_with_a_line_break_is_rejected_before_rendering() {
        let error = compose(TargetId::GitConfig, [git_fragment(
            GitConfigKey::new("user", "email"),
            "crew@example.com",
            "credential/first\ninjected",
        )])
        .expect_err("provenance comments must remain one line")
        .to_string();

        assert!(error.contains("contains a line break"));
    }

    #[test]
    fn gitconfig_key_with_a_line_break_is_rejected_before_rendering() {
        let error = compose(TargetId::GitConfig, [git_fragment(
            GitConfigKey::new("user\n[injected]", "email"),
            "crew@example.com",
            "credential/first",
        )])
        .expect_err("gitconfig keys must remain one line")
        .to_string();

        assert!(error.contains("key containing a line break"));
        assert!(error.contains("credential/first"));
    }

    #[test]
    fn gitconfig_value_with_a_line_break_is_rejected_before_rendering() {
        let error = compose(TargetId::GitConfig, [git_fragment(
            GitConfigKey::new("user", "email"),
            "crew@example.com\n[injected]",
            "credential/first",
        )])
        .expect_err("gitconfig values must remain one line")
        .to_string();

        assert!(error.contains("value containing a line break"));
        assert!(error.contains("user.email"));
        assert!(error.contains("credential/first"));
    }
}
