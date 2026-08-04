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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetId {
    GitConfig,
}

impl fmt::Display for TargetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitConfig => formatter.write_str("gitconfig"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetKey {
    GitConfig(GitConfigKey),
}

impl fmt::Display for TargetKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitConfig(key) => key.fmt(formatter),
        }
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

impl Fragment {
    pub fn new(target: TargetId, key: TargetKey, value: impl Into<String>, provenance: Provenance) -> Self {
        Self { target, key, value: value.into(), order: order::DEFAULT, priority: priority::NORMAL, merge: Merge::Set, provenance }
    }

    pub fn with_order(mut self, order: u16) -> Self {
        self.order = order;
        self
    }

    pub fn with_priority(mut self, priority: u16) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_merge(mut self, merge: Merge) -> Self {
        self.merge = merge;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedFile {
    pub target: TargetId,
    pub contents: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeError {
    TargetKeyMismatch { target: TargetId, key: TargetKey, provenance: Provenance },
    SetConflict { key: TargetKey, first: Provenance, second: Provenance },
    MergePolicyConflict { key: TargetKey, first: Provenance, second: Provenance },
    Duplicate { key: TargetKey, first: Provenance, second: Provenance },
}

impl fmt::Display for ComposeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
        }
    }
}

impl std::error::Error for ComposeError {}

pub fn compose(target: TargetId, fragments: impl IntoIterator<Item = Fragment>) -> Result<ComposedFile, ComposeError> {
    let mut fragments = fragments.into_iter().filter(|fragment| fragment.target == target).collect::<Vec<_>>();
    fragments.sort_by(|left, right| (left.order, &left.provenance).cmp(&(right.order, &right.provenance)));

    match target {
        TargetId::GitConfig => compose_gitconfig(fragments),
    }
}

fn compose_gitconfig(fragments: Vec<Fragment>) -> Result<ComposedFile, ComposeError> {
    for fragment in &fragments {
        if !matches!(fragment.key, TargetKey::GitConfig(_)) {
            return Err(ComposeError::TargetKeyMismatch {
                target: fragment.target,
                key: fragment.key.clone(),
                provenance: fragment.provenance.clone(),
            });
        }
    }

    let minimum_priorities = fragments.iter().fold(BTreeMap::<TargetKey, u16>::new(), |mut priorities, fragment| {
        priorities
            .entry(fragment.key.clone())
            .and_modify(|priority| *priority = (*priority).min(fragment.priority))
            .or_insert(fragment.priority);
        priorities
    });
    let mut winners = BTreeMap::<TargetKey, Vec<&Fragment>>::new();
    for fragment in &fragments {
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
                rendered.push(RenderedGitConfigEntry {
                    fragment: first,
                    provenances: fragments_for_key.iter().map(|fragment| &fragment.provenance).collect(),
                });
            }
            Merge::Append => rendered.extend(
                fragments_for_key.iter().map(|fragment| RenderedGitConfigEntry { fragment, provenances: vec![&fragment.provenance] }),
            ),
            Merge::ErrorOnDuplicate => {
                if let Some(other) = fragments_for_key.get(1) {
                    return Err(ComposeError::Duplicate {
                        key: first.key.clone(),
                        first: first.provenance.clone(),
                        second: other.provenance.clone(),
                    });
                }
                rendered.push(RenderedGitConfigEntry { fragment: first, provenances: vec![&first.provenance] });
            }
        }
    }
    rendered
        .sort_by(|left, right| (left.fragment.order, &left.fragment.provenance).cmp(&(right.fragment.order, &right.fragment.provenance)));

    let contents = rendered.into_iter().map(render_gitconfig_entry).collect::<Vec<_>>().join("\n");
    Ok(ComposedFile { target: TargetId::GitConfig, contents })
}

struct RenderedGitConfigEntry<'a> {
    fragment: &'a Fragment,
    provenances: Vec<&'a Provenance>,
}

fn render_gitconfig_entry(entry: RenderedGitConfigEntry<'_>) -> String {
    let TargetKey::GitConfig(key) = &entry.fragment.key;
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
        Fragment::new(TargetId::GitConfig, TargetKey::GitConfig(key), value, Provenance::new(provenance))
    }

    #[test]
    fn sugar_constants_match_the_ratified_order_and_priority_contract() {
        assert_eq!((order::FIRST, order::EARLY, order::DEFAULT, order::LATE), (0, 500, 1000, 1500));
        assert_eq!((priority::FORCE, priority::NORMAL, priority::DEFAULT), (50, 100, 1000));
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
    fn lower_priority_number_overrides_set_fragments() {
        let key = GitConfigKey::new("user", "email");
        let composed = compose(TargetId::GitConfig, [
            git_fragment(key.clone(), "default@example.com", "credential/default").with_priority(priority::DEFAULT),
            git_fragment(key, "forced@example.com", "credential/forced").with_priority(priority::FORCE),
        ])
        .expect("explicit override should compose");

        assert!(composed.contents.contains("forced@example.com"));
        assert!(!composed.contents.contains("default@example.com"));
    }

    #[test]
    fn append_follows_order_then_provenance_stably() {
        let key = GitConfigKey::subsection("credential", "https://example.com", "helper");
        let composed = compose(TargetId::GitConfig, [
            git_fragment(key.clone(), "late", "credential/alpha").with_order(order::LATE).with_merge(Merge::Append),
            git_fragment(key.clone(), "same-order-z", "credential/zulu").with_order(order::EARLY).with_merge(Merge::Append),
            git_fragment(key.clone(), "same-order-a-first", "credential/alpha").with_order(order::EARLY).with_merge(Merge::Append),
            git_fragment(key, "same-order-a-second", "credential/alpha").with_order(order::EARLY).with_merge(Merge::Append),
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
        let error = compose(TargetId::GitConfig, [
            git_fragment(key.clone(), "/first", "workspace/first").with_merge(Merge::ErrorOnDuplicate),
            git_fragment(key, "/second", "workspace/second").with_merge(Merge::ErrorOnDuplicate),
        ])
        .expect_err("ErrorOnDuplicate must reject a second contributor")
        .to_string();

        assert!(error.contains("core.hooksPath"));
        assert!(error.contains("workspace/first"));
        assert!(error.contains("workspace/second"));
    }
}
