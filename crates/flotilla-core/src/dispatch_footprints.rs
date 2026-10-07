//! Conflict policy over immutable, repository-scoped observations.
use std::collections::{BTreeMap, BTreeSet};

use flotilla_protocol::{FileFootprint, FootprintObservation, HotFile, MergeOrderHint};

/// Declare a repository-relative file, directory or trailing-star pattern per bullet in ## Touches.
/// With no section, predict from paths and unique basenames mentioned in prose.
pub fn predict(body: &str, known: impl IntoIterator<Item = String>) -> FileFootprint {
    PredictionPaths::new(known).predict(body)
}

#[derive(Default)]
struct PredictionPaths {
    known: BTreeSet<String>,
    basenames: BTreeMap<String, usize>,
}
impl PredictionPaths {
    fn new(paths: impl IntoIterator<Item = String>) -> Self {
        let known = paths.into_iter().collect::<BTreeSet<_>>();
        let mut basenames = BTreeMap::new();
        for path in &known {
            *basenames.entry(path.rsplit('/').next().unwrap_or(path).to_string()).or_default() += 1;
        }
        Self { known, basenames }
    }

    fn observation(observation: &FootprintObservation) -> Self {
        Self::new(observation.history.iter().chain(observation.work.iter().map(|w| &w.footprint)).flat_map(|f| f.files.keys().cloned()))
    }

    fn predict(&self, body: &str) -> FileFootprint {
        predict_indexed(body, self.known.iter(), &self.basenames)
    }
}

fn predict_indexed<'a>(body: &str, known: impl Iterator<Item = &'a String> + Clone, basenames: &BTreeMap<String, usize>) -> FileFootprint {
    let mut touches = None;
    for line in body.lines() {
        if line.trim() == "## Touches" {
            touches = Some(Vec::new());
            continue;
        }
        if let Some(paths) = touches.as_mut() {
            if line.trim_start().starts_with("## ") {
                break;
            }
            let line = line.trim();
            let Some(path) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) else { continue };
            let path = path.trim().trim_matches('`');
            if !path.is_empty() {
                paths.push(path.to_string());
            }
        }
    }
    let mut files = BTreeMap::new();
    if let Some(paths) = touches {
        for path in paths {
            if path.starts_with('/') || path.split('/').any(|part| part == "..") {
                continue;
            }
            if path.contains('*') && (path.matches('*').count() != 1 || !path.ends_with('*')) || path.contains('?') || path.contains('[') {
                tracing::debug!(%path, "unsupported Touches glob; use a directory prefix or one trailing star");
                continue;
            }
            let prefix = path.trim_end_matches('*').trim_end_matches('/');
            let directory_prefix = format!("{prefix}/");
            let mut matched = false;
            for candidate in known.clone() {
                if candidate == &path || candidate.starts_with(&directory_prefix) || path.ends_with('*') && candidate.starts_with(prefix) {
                    files.insert(candidate.clone(), interface_path(candidate));
                    matched = true;
                }
            }
            if !matched && path.contains('*') {
                tracing::debug!(%path, "Touches glob matched no observed paths");
            }
            if !matched && !path.contains('*') {
                files.insert(path.clone(), interface_path(&path));
            }
        }
    } else {
        let tokens = body.split(|c: char| !(c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))).collect::<BTreeSet<_>>();
        for path in known {
            let basename = path.rsplit('/').next().unwrap_or(path);
            let unique = basenames.get(basename) == Some(&1);
            if tokens.contains(path.as_str()) || unique && tokens.contains(basename) {
                files.insert(path.clone(), interface_path(path));
            }
        }
    }
    FileFootprint { files }
}

pub fn interface_path(path: &str) -> bool {
    path.split('/').any(|part| matches!(part, "flotilla-protocol" | "flotilla-resources" | "protocol" | "resources"))
        || path.ends_with(".crd.yaml")
        || path.rsplit('/').next() == Some("lib.rs")
}

pub fn interface_patch(path: &str, patch: &str) -> bool {
    interface_path(path)
        || patch.lines().filter(|line| line.starts_with('+') || line.starts_with('-')).any(|line| {
            ["pub struct ", "pub enum ", "pub trait ", "pub type ", "pub fn ", "pub async fn ", "Serialize", "Deserialize", "#[serde"]
                .iter()
                .any(|needle| line.contains(needle))
        })
}

/// Integer rarity: an unseen file costs window+1, a file in every merge costs 1.
/// Interface evidence multiplies the same file weight by four; each file counts once.
pub fn overlap(left: &FileFootprint, right: &FileFootprint, history: &[FileFootprint]) -> (u64, Vec<String>) {
    let mut weight = 0u64;
    let mut files = Vec::new();
    for (path, public) in &left.files {
        let Some(other_public) = right.files.get(path) else { continue };
        let frequency = history.iter().filter(|merge| merge.files.contains_key(path)).count();
        let rarity = (history.len() as u64 + 1) / (frequency as u64 + 1);
        weight = weight.saturating_add(rarity.saturating_mul(if *public || *other_public { 4 } else { 1 }));
        files.push(path.clone());
    }
    (weight, files)
}

fn indexed_overlap(left: &FileFootprint, right: &FileFootprint, observation: &FootprintObservation) -> (u64, Vec<String>) {
    let mut weight = 0u64;
    let mut files = Vec::new();
    for (path, public) in &left.files {
        if let Some(other_public) = right.files.get(path) {
            let rarity = observation.file_rarity.get(path).copied().unwrap_or(observation.history.len() as u64 + 1);
            weight = weight.saturating_add(rarity.saturating_mul(if *public || *other_public { 4 } else { 1 }));
            files.push(path.clone());
        }
    }
    (weight, files)
}

/// Stable total target ordering gives hints without introducing hold cycles.
pub fn reports(observation: &mut FootprintObservation) {
    let mut paths = BTreeMap::<String, (usize, usize)>::new();
    for merge in &observation.history {
        for path in merge.files.keys() {
            paths.entry(path.clone()).or_default().0 += 1;
        }
    }
    for work in &observation.work {
        for path in work.footprint.files.keys() {
            paths.entry(path.clone()).or_default().1 += 1;
        }
    }
    observation.file_rarity =
        paths.iter().map(|(path, (merges, _))| (path.clone(), (observation.history.len() as u64 + 1) / (*merges as u64 + 1))).collect();
    observation.basename_counts.clear();
    for path in paths.keys() {
        *observation.basename_counts.entry(path.rsplit('/').next().unwrap_or(path).to_string()).or_default() += 1;
    }
    observation.hot_files = paths.into_iter().map(|(path, (merges, active_work))| HotFile { path, merges, active_work }).collect();
    observation.hot_files.sort_by(|a, b| b.active_work.cmp(&a.active_work).then(b.merges.cmp(&a.merges)).then(a.path.cmp(&b.path)));
    observation.work.sort_by(|a, b| a.target.cmp(&b.target));
    observation.merge_order.clear();
    for (index, before) in observation.work.iter().enumerate() {
        for after in &observation.work[index + 1..] {
            let (weight, files) = indexed_overlap(&before.footprint, &after.footprint, observation);
            if weight > 0 {
                observation.merge_order.push(MergeOrderHint { before: before.target.clone(), after: after.target.clone(), weight, files });
            }
        }
    }
}

type WorkPairs = BTreeMap<(flotilla_protocol::FootprintTarget, flotilla_protocol::FootprintTarget), (u64, Vec<String>)>;

pub struct ConflictBoard {
    repositories: BTreeMap<flotilla_protocol::IssueSource, FootprintObservation>,
    pairs: BTreeMap<flotilla_protocol::IssueSource, WorkPairs>,
}

#[derive(Debug, Clone)]
pub struct ConflictMeasurement {
    pub outcome: bool,
    pub source: flotilla_protocol::IssueSource,
    pub target: flotilla_protocol::FootprintTarget,
    pub candidate_actual: bool,
    pub target_actual: bool,
    pub revision: String,
    pub conflicts: Option<bool>,
    pub weight: u64,
    pub files: Vec<String>,
}

impl ConflictBoard {
    /// Build one pass-wide read index from already prepared source observations.
    pub fn new(boards: &[flotilla_protocol::DispatchBoardRepository]) -> Self {
        Self::cached(
            boards
                .iter()
                .filter_map(|board| board.footprints.clone().map(|observation| (canonical_source(&board.source), observation)))
                .collect(),
        )
    }

    #[cfg(test)]
    fn indexed(mut repositories: BTreeMap<flotilla_protocol::IssueSource, FootprintObservation>) -> Self {
        for observation in repositories.values_mut() {
            reports(observation);
        }
        Self::cached(repositories)
    }

    fn cached(repositories: BTreeMap<flotilla_protocol::IssueSource, FootprintObservation>) -> Self {
        let pairs = repositories
            .iter()
            .map(|(source, observation)| {
                (
                    source.clone(),
                    observation
                        .merge_order
                        .iter()
                        .map(|pair| ((pair.before.clone(), pair.after.clone()), (pair.weight, pair.files.clone())))
                        .collect(),
                )
            })
            .collect();
        Self { repositories, pairs }
    }

    /// Retain predicted footprints and observed conflict outcomes even after
    /// the work it overlapped has landed and left the in-flight set.
    pub fn outcomes(&self, serving: &str) -> Vec<ConflictMeasurement> {
        use flotilla_protocol::FootprintTarget;
        self.repositories
            .iter()
            .filter_map(|(source, observation)| {
                let work = observation
                    .work
                    .iter()
                    .find(|w| w.convoy.as_deref() == Some(serving) || w.target == FootprintTarget::Convoy { name: serving.into() })?;
                Some(ConflictMeasurement {
                    outcome: true,
                    source: source.clone(),
                    target: work.target.clone(),
                    candidate_actual: work.actual,
                    target_actual: work.actual,
                    revision: format!("outcome:{}", work.revision),
                    conflicts: work.conflicts,
                    weight: 0,
                    files: work.footprint.files.keys().cloned().collect(),
                })
            })
            .collect()
    }

    pub fn measurements(&self, issue: &flotilla_protocol::Issue, serving: Option<&str>) -> Vec<ConflictMeasurement> {
        self.measurements_in(issue, serving, None)
    }

    pub fn measurements_in(
        &self,
        issue: &flotilla_protocol::Issue,
        serving: Option<&str>,
        sources: Option<&BTreeSet<flotilla_protocol::IssueSource>>,
    ) -> Vec<ConflictMeasurement> {
        use flotilla_protocol::FootprintTarget;
        let mut measurements = Vec::new();
        let text = format!("{}\n{}", issue.title, issue.body.as_deref().unwrap_or_default());
        for (source, observation) in &self.repositories {
            if sources.is_some_and(|sources| !sources.contains(source)) {
                continue;
            }
            let actual = serving.and_then(|name| {
                observation
                    .work
                    .iter()
                    .find(|w| w.actual && (w.convoy.as_deref() == Some(name) || w.target == FootprintTarget::Convoy { name: name.into() }))
            });
            let predicted = predict_indexed(&text, observation.hot_files.iter().map(|file| &file.path), &observation.basename_counts);
            let candidate = actual.map_or(&predicted, |w| &w.footprint);
            for work in &observation.work {
                if serving.is_some_and(|name| {
                    work.convoy.as_deref() == Some(name) || work.target == FootprintTarget::Convoy { name: name.into() }
                }) {
                    continue;
                }
                let (weight, files) = if let Some(actual) = actual {
                    let key = if actual.target < work.target {
                        (actual.target.clone(), work.target.clone())
                    } else {
                        (work.target.clone(), actual.target.clone())
                    };
                    self.pairs[source].get(&key).cloned().unwrap_or_default()
                } else {
                    indexed_overlap(candidate, &work.footprint, observation)
                };
                measurements.push(ConflictMeasurement {
                    outcome: false,
                    source: source.clone(),
                    target: work.target.clone(),
                    candidate_actual: actual.is_some(),
                    target_actual: work.actual,
                    revision: format!("{}:{}", actual.map_or("prediction", |w| w.revision.as_str()), work.revision),
                    conflicts: match (actual.map(|w| w.conflicts), work.conflicts) {
                        (Some(Some(true)), _) | (_, Some(true)) => Some(true),
                        (None, state) => state,
                        (Some(Some(false)), Some(false)) => Some(false),
                        _ => None,
                    },
                    weight,
                    files,
                });
            }
        }
        measurements
    }
}

/// Only the background source refresh predicts live work and computes reports.
pub fn prepare_observation(
    scope: &flotilla_protocol::IssueSource,
    observation: &mut FootprintObservation,
    convoys: &[flotilla_resources::ResourceObject<flotilla_resources::Convoy>],
    repository_sources: &BTreeMap<flotilla_protocol::RepositoryKey, flotilla_protocol::IssueSource>,
) {
    use flotilla_protocol::{FootprintTarget, WorkFootprint};
    let live = convoys
        .iter()
        .filter(|c| !c.status.as_ref().is_some_and(|s| s.phase.is_terminal()))
        .map(|c| c.metadata.name.as_str())
        .collect::<BTreeSet<_>>();
    {
        for work in &mut observation.work {
            if matches!(work.target, FootprintTarget::PullRequest { .. }) && work.convoy.as_deref().is_some_and(|name| !live.contains(name))
            {
                work.convoy = None;
            }
        }
        observation.work.retain(|work| work.convoy.as_deref().is_none_or(|name| live.contains(name)));
    }
    let prediction_paths = PredictionPaths::observation(observation);
    for convoy in convoys {
        if convoy.status.as_ref().is_some_and(|s| s.phase.is_terminal()) {
            continue;
        }
        for repository in &convoy.spec.repositories {
            let Some(source) = repository_sources.get(&repository.repo_ref) else { continue };
            if canonical_source(source) != canonical_source(scope) {
                continue;
            }
            let target = FootprintTarget::Convoy { name: convoy.metadata.name.clone() };
            if observation.work.iter().any(|w| w.target == target || w.convoy.as_deref() == Some(convoy.metadata.name.as_str())) {
                continue;
            }
            let mut footprint = FileFootprint::default();
            for issue in &convoy.spec.issues {
                footprint.files.extend(
                    prediction_paths
                        .predict(&format!("{}\n{}", issue.snapshot.title, issue.snapshot.body.as_deref().unwrap_or_default()))
                        .files,
                );
            }
            observation.work.push(WorkFootprint {
                target,
                convoy: Some(convoy.metadata.name.clone()),
                footprint,
                actual: false,
                revision: "prediction".into(),
                conflicts: None,
            });
        }
    }
    reports(observation);
}

/// Match source identity across canonical repository URLs and issue bindings.
pub fn canonical_source(source: &flotilla_protocol::IssueSource) -> flotilla_protocol::IssueSource {
    let mut source = flotilla_resources::normalize_issue_source(source);
    if matches!(source.service.as_str(), "github" | "github.com" | "https://github.com") {
        source.service = "https://github.com".into();
        source.scope = source.scope.to_ascii_lowercase();
    }
    source
}

/// Add unpublished convoy predictions and recompute reports on a served copy.
/// The cached forge observation remains immutable and is replaced only by refresh.
/// Forge identity follows the resource binding, independently of mirror transport.
pub fn repository_sources(
    repositories: impl IntoIterator<Item = flotilla_resources::ResourceObject<flotilla_resources::Repository>>,
) -> BTreeMap<flotilla_protocol::RepositoryKey, flotilla_protocol::IssueSource> {
    repositories
        .into_iter()
        .filter_map(|repository| {
            let forge = repository.spec.forge()?;
            Some((
                flotilla_protocol::RepositoryKey(repository.metadata.name),
                canonical_source(&flotilla_protocol::IssueSource { service: forge.service_url.clone(), scope: forge.repository.clone() }),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Touches supports literal paths, directories and one trailing star. Invalid
    // paths and unsupported globs never become fictitious file evidence; a new
    // literal file is retained. Later sections and prose cannot add paths.
    #[test]
    fn touches_edges_and_ambiguous_basenames() {
        let known = vec!["src/a.rs".into(), "src/deep/b.rs".into(), "other/a.rs".into()];
        let declared = predict(
            "## Touches\n- ../outside.rs\n- /absolute.rs\n- src/**/*.rs\n- absent/*\n- src/\n- new.rs\nExplanation inside the section\n   ## Validation\n- unwanted.rs",
            known.clone(),
        );
        assert_eq!(declared.files.keys().map(String::as_str).collect::<Vec<_>>(), ["new.rs", "src/a.rs", "src/deep/b.rs"]);
        assert!(predict("a.rs", known.clone()).files.is_empty());
        assert_eq!(predict("b.rs", known.clone()).files.keys().map(String::as_str).collect::<Vec<_>>(), ["src/deep/b.rs"]);
        assert_eq!(predict("src/a.rs", known).files.keys().map(String::as_str).collect::<Vec<_>>(), ["src/a.rs"]);
        assert!(predict("## Touches", Vec::new()).files.is_empty());
    }

    // Interface classification is independent of directory depth, including
    // root protocol/resources directories and the repository's own lib.rs.
    #[test]
    fn root_and_nested_interface_paths() {
        for path in ["protocol/x.rs", "resources/x.rs", "lib.rs", "nested/lib.rs", "crates/flotilla-protocol/src/a.rs", "thing.crd.yaml"] {
            assert!(interface_path(path), "{path}");
        }
        for path in ["protocolish/x.rs", "myresources/x.rs", "lib.rs.bak", "src/private.rs"] {
            assert!(!interface_path(path), "{path}");
        }
    }

    #[hegel::test]
    fn rarity_and_interfaces_weight_each_shared_file_once(tc: hegel::TestCase) {
        let count = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(60));
        let frequent = FileFootprint { files: BTreeMap::from([("hot.rs".into(), false)]) };
        let history = vec![frequent.clone(); count];
        let rare = FileFootprint { files: BTreeMap::from([("cold.rs".into(), false)]) };
        assert_eq!(overlap(&frequent, &frequent, &history).0, 1);
        assert_eq!(overlap(&rare, &rare, &history).0, count as u64 + 1);
        let public = FileFootprint { files: BTreeMap::from([("cold.rs".into(), true)]) };
        assert_eq!(overlap(&rare, &public, &history).0, 4 * (count as u64 + 1));
        assert_eq!(overlap(&frequent, &rare, &history).0, 0);
    }
    #[test]
    fn declaration_prediction_and_actual_replacement() {
        let known = vec!["src/a.rs".into(), "src/b.rs".into()];
        let predicted = predict("## Touches\n- `src/a.rs`\n## Tests\n- src/b.rs", known.clone());
        assert_eq!(predicted.files.keys().cloned().collect::<Vec<_>>(), ["src/a.rs"]);
        assert_eq!(predict("fix a.rs", known.clone()), predicted);
        assert_eq!(predict("## Touches\n- src/*", known).files.len(), 2);
        assert!(interface_patch("src/a.rs", "+pub trait Contract {}"));
    }
    use flotilla_protocol::{FootprintTarget, Issue, IssueRef, IssueSource, IssueState, WorkFootprint};

    // #2784: mirror clone transport must not change forge-scoped predictions
    // or hot-file reports. Real resource objects carry the admitted snapshot.
    #[tokio::test]
    async fn mirrored_transport_uses_authoritative_repository_forge() {
        use flotilla_resources::{
            Convoy, ConvoyIssue, ConvoyRepositorySpec, ConvoySpec, InputMeta, IssueSnapshot, Repository, RepositorySpec, ResourceBackend,
        };
        let backend = ResourceBackend::InMemory(Default::default());
        let repository_spec = RepositorySpec::remote("https://github.com/org/repo").expect("repo");
        let key = repository_spec.key();
        let repository = backend
            .using::<Repository>("ns")
            .create(&InputMeta::builder().name(key.0.clone()).build(), &repository_spec)
            .await
            .expect("repository");
        let source = IssueSource { service: "https://github.com".into(), scope: "org/repo".into() };
        let convoy = backend
            .using::<Convoy>("ns")
            .create(
                &InputMeta::builder().name("crew".into()).build(),
                &ConvoySpec::builder()
                    .workflow_ref("test".into())
                    .repositories(vec![ConvoyRepositorySpec {
                        url: "https://forgejo.example/mirror/repo".into(),
                        repo_ref: key.clone(),
                        source_ref: "main".into(),
                        target_ref: "main".into(),
                        workspace_slug: "repo".into(),
                        subpaths: vec![],
                    }])
                    .issues(vec![ConvoyIssue {
                        reference: IssueRef { source: source.clone(), id: "1".into() },
                        repository_ref: Some(key),
                        snapshot: IssueSnapshot {
                            title: "Ticket".into(),
                            body: Some("## Touches\n- src/a.rs".into()),
                            state: IssueState::Open,
                            labels: vec![],
                            as_of: chrono::Utc::now(),
                        },
                    }])
                    .build(),
            )
            .await
            .expect("convoy");
        let mut boards = vec![flotilla_protocol::DispatchBoardRepository {
            source: source.clone(),
            issues: vec![],
            pull_requests: vec![],
            observed_at: chrono::Utc::now(),
            age_seconds: 0,
            refresh_error: None,
            footprints: Some(FootprintObservation::default()),
        }];
        let sources = repository_sources([repository]);
        prepare_observation(&source, boards[0].footprints.as_mut().expect("footprints"), std::slice::from_ref(&convoy), &sources);
        let conflicts = ConflictBoard::new(&boards);
        let outcomes = conflicts.outcomes("crew");
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].source, source);
        assert!(!outcomes[0].candidate_actual);
        assert_eq!(outcomes[0].files, ["src/a.rs"]);
        assert_eq!(boards[0].footprints.as_ref().expect("footprints").hot_files[0].active_work, 1);
    }

    // #2784: first pushed diff replaces prediction, including removal of predicted paths.
    // Measurements carry their repository source for explanations and hold identity.
    #[test]
    fn actual_diff_replaces_prediction_and_sources_remain_separate() {
        let source = IssueSource { service: "https://github.com".into(), scope: "org/repo".into() };
        let issue = Issue::builder()
            .reference(IssueRef { source: source.clone(), id: "1".into() })
            .title("Ticket".into())
            .labels(vec![])
            .body("## Touches\n- src/a.rs".into())
            .state(IssueState::Open)
            .as_of(chrono::Utc::now())
            .provider_name("fake".into())
            .provider_display_name("Fake".into())
            .build();
        let other = WorkFootprint {
            target: FootprintTarget::PullRequest { url: "https://github.com/org/repo/pull/2".into() },
            convoy: None,
            footprint: FileFootprint { files: BTreeMap::from([("src/a.rs".into(), false)]) },
            actual: true,
            revision: "other".into(),
            conflicts: Some(false),
        };
        let mut board =
            ConflictBoard::indexed(BTreeMap::from([(source.clone(), FootprintObservation { work: vec![other], ..Default::default() })]));
        let predicted = board.measurements(&issue, Some("crew"));
        assert_eq!(predicted[0].source, source);
        assert_eq!(predicted[0].weight, 1);
        assert!(!predicted[0].candidate_actual);
        board.repositories.get_mut(&source).expect("repo").work.push(WorkFootprint {
            target: FootprintTarget::Convoy { name: "crew".into() },
            convoy: Some("crew".into()),
            footprint: FileFootprint { files: BTreeMap::from([("src/b.rs".into(), false)]) },
            actual: true,
            revision: "push".into(),
            conflicts: Some(false),
        });
        let actual = board.measurements(&issue, Some("crew"));
        assert_eq!(actual.len(), 1);
        assert!(actual[0].candidate_actual);
        assert_eq!(actual[0].weight, 0);
        assert!(actual[0].files.is_empty());
        board.repositories.get_mut(&source).expect("repo").work[1].conflicts = None;
        assert_eq!(board.measurements(&issue, Some("crew"))[0].conflicts, None);
        let second = IssueSource { service: source.service.clone(), scope: "org/z-other".into() };
        board.repositories.insert(second.clone(), FootprintObservation {
            work: vec![
                WorkFootprint {
                    target: FootprintTarget::PullRequest { url: "https://github.com/org/z-other/pull/3".into() },
                    convoy: None,
                    footprint: FileFootprint { files: BTreeMap::from([("src/b.rs".into(), false)]) },
                    actual: true,
                    revision: "other".into(),
                    conflicts: Some(false),
                },
                WorkFootprint {
                    target: FootprintTarget::Convoy { name: "crew".into() },
                    convoy: Some("crew".into()),
                    footprint: FileFootprint { files: BTreeMap::from([("src/c.rs".into(), false)]) },
                    actual: true,
                    revision: "push-other".into(),
                    conflicts: Some(false),
                },
            ],
            ..Default::default()
        });
        board = ConflictBoard::indexed(board.repositories);
        let isolated = board.measurements(&issue, Some("crew"));
        assert_eq!(isolated.len(), 2);
        assert_eq!(isolated.iter().map(|m| m.source.clone()).collect::<BTreeSet<_>>(), BTreeSet::from([source.clone(), second]));
        assert!(isolated.iter().all(|m| m.weight == 0));
        board.repositories.remove(&IssueSource { service: source.service.clone(), scope: "org/z-other".into() });
        let remaining = &mut board.repositories.get_mut(&source).expect("repo").work;
        remaining.retain(|w| w.convoy.as_deref() == Some("crew"));
        remaining[0].conflicts = Some(true);
        assert!(board.measurements(&issue, Some("crew")).is_empty());
        let outcome = board.outcomes("crew");
        assert_eq!(outcome.len(), 1);
        assert!(outcome[0].outcome && outcome[0].candidate_actual);
        assert_eq!(outcome[0].conflicts, Some(true));
        assert_eq!(outcome[0].files, ["src/b.rs"]);
    }
}
