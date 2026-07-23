//! Warm Project-scoped projections over fleet-merged store-backed facts.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

use flotilla_protocol::{
    CheckoutRow, HostName, IndependentRow, QueryChanges, QueryScope, RepoKey, RepositoryKey, ResourceRef, ResultDelta, ResultSet,
    ResultSetCondition, ResultSetState, Rows, UNKNOWN_REPOSITORY_LABEL,
};

pub(crate) trait ScopedStoreRow: Clone + PartialEq {
    fn resource(&self) -> &ResourceRef;
    fn repository_key(&self) -> Option<&RepositoryKey>;
    fn apply_repository_label(&mut self, labels: &HashMap<RepositoryKey, String>);
    fn compare(left: &Self, right: &Self) -> Ordering;
    fn rows(scope: Option<QueryScope>, rows: Vec<Self>) -> Rows;
    fn changes(scope: Option<QueryScope>, changed: Vec<Self>, removed: Vec<ResourceRef>) -> QueryChanges;
}

impl ScopedStoreRow for CheckoutRow {
    fn resource(&self) -> &ResourceRef {
        &self.resource
    }

    fn repository_key(&self) -> Option<&RepositoryKey> {
        Some(&self.repo)
    }

    fn apply_repository_label(&mut self, labels: &HashMap<RepositoryKey, String>) {
        if let Some(label) = labels.get(&self.repo) {
            self.repo_label = label.clone();
        } else if self.repo_label == self.repo.0 {
            self.repo_label = UNKNOWN_REPOSITORY_LABEL.to_string();
        }
    }

    fn compare(left: &Self, right: &Self) -> Ordering {
        (&left.host, &left.path, &left.resource.namespace, &left.resource.name).cmp(&(
            &right.host,
            &right.path,
            &right.resource.namespace,
            &right.resource.name,
        ))
    }

    fn rows(scope: Option<QueryScope>, rows: Vec<Self>) -> Rows {
        Rows::Checkouts { scope, rows }
    }

    fn changes(scope: Option<QueryScope>, changed: Vec<Self>, removed: Vec<ResourceRef>) -> QueryChanges {
        QueryChanges::Checkouts { scope, changed, removed }
    }
}

impl ScopedStoreRow for IndependentRow {
    fn resource(&self) -> &ResourceRef {
        &self.resource
    }

    fn repository_key(&self) -> Option<&RepositoryKey> {
        self.repository_key.as_ref()
    }

    fn apply_repository_label(&mut self, labels: &HashMap<RepositoryKey, String>) {
        if let Some(repository_key) = &self.repository_key {
            let label = labels
                .get(repository_key)
                .cloned()
                .or_else(|| self.repo.as_ref().filter(|label| label.0 != repository_key.0).map(|label| label.0.clone()))
                .unwrap_or_else(|| UNKNOWN_REPOSITORY_LABEL.to_string());
            self.repo = Some(RepoKey(label));
        }
    }

    fn compare(left: &Self, right: &Self) -> Ordering {
        (&left.name, &left.host, &left.resource.namespace, &left.resource.name).cmp(&(
            &right.name,
            &right.host,
            &right.resource.namespace,
            &right.resource.name,
        ))
    }

    fn rows(scope: Option<QueryScope>, rows: Vec<Self>) -> Rows {
        Rows::Independents { scope, rows }
    }

    fn changes(scope: Option<QueryScope>, changed: Vec<Self>, removed: Vec<ResourceRef>) -> QueryChanges {
        QueryChanges::Independents { scope, changed, removed }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MaterializedSet<R> {
    seq: u64,
    rows: HashMap<ResourceRef, R>,
    state: ResultSetState,
}

impl<R: ScopedStoreRow> MaterializedSet<R> {
    fn unavailable(scope: &QueryScope, message: String) -> Self {
        Self {
            seq: 0,
            rows: HashMap::new(),
            state: ResultSetState {
                demand: None,
                conditions: vec![ResultSetCondition::QueryScopeUnavailable { scope: scope.clone(), message }],
            },
        }
    }

    fn result_set(&self, scope: &Option<QueryScope>) -> ResultSet {
        let mut rows = self.rows.values().cloned().collect::<Vec<_>>();
        rows.sort_by(R::compare);
        ResultSet { seq: self.seq, rows: R::rows(scope.clone(), rows), state: self.state.clone() }
    }
}

/// All warm facts for one store-backed query family and their finite,
/// referent-backed Project views.
#[derive(Debug, bon::Builder)]
#[builder(builder_type(vis = "pub(crate)"))]
pub(crate) struct ScopedStoreProjection<R> {
    known_repositories: HashSet<RepositoryKey>,
    repository_labels: HashMap<RepositoryKey, String>,
    projects: HashMap<QueryScope, Vec<RepositoryKey>>,
    local_by_repo: HashMap<Option<RepositoryKey>, HashMap<ResourceRef, R>>,
    replicas_by_repo: HashMap<Option<RepositoryKey>, HashMap<HostName, HashMap<ResourceRef, R>>>,
    sets: HashMap<Option<QueryScope>, MaterializedSet<R>>,
}

impl<R> Default for ScopedStoreProjection<R> {
    fn default() -> Self {
        Self {
            known_repositories: HashSet::new(),
            repository_labels: HashMap::new(),
            projects: HashMap::new(),
            local_by_repo: HashMap::new(),
            replicas_by_repo: HashMap::new(),
            sets: HashMap::new(),
        }
    }
}

impl<R: ScopedStoreRow> ScopedStoreProjection<R> {
    pub fn replace_catalog(
        &mut self,
        repositories: HashMap<RepositoryKey, String>,
        projects: HashMap<QueryScope, Vec<RepositoryKey>>,
    ) -> Vec<ResultDelta> {
        self.known_repositories = repositories.keys().cloned().collect();
        self.repository_labels = repositories;
        self.projects = projects;
        self.recompute_all()
    }

    pub fn replace_local_rows(&mut self, rows: Vec<R>) -> Vec<ResultDelta> {
        self.local_by_repo = group_rows(rows);
        self.recompute_all()
    }

    pub fn replace_replica_rows(&mut self, replicas: HashMap<HostName, Vec<R>>) -> Vec<ResultDelta> {
        self.replicas_by_repo.clear();
        for (host, rows) in replicas {
            for (repo, rows) in group_rows(rows) {
                self.replicas_by_repo.entry(repo).or_default().insert(host.clone(), rows);
            }
        }
        self.recompute_all()
    }

    pub fn result_set(&mut self, scope: &Option<QueryScope>) -> ResultSet {
        if !self.sets.contains_key(scope) {
            let materialized = self.materialize(scope);
            self.sets.insert(scope.clone(), materialized);
        }
        self.sets.get(scope).expect("scoped result set inserted").result_set(scope)
    }

    /// Local unscoped facts are the federation unit. Project views are
    /// derived by each receiving Aggregator from its local Project catalog.
    pub fn local_result_sets(&self) -> Vec<ResultSet> {
        let rows = self.local_by_repo.values().flat_map(|rows| rows.iter()).map(|(key, row)| (key.clone(), row.clone())).collect();
        let scope = None;
        let seq = self.sets.get(&scope).map_or(0, |set| set.seq);
        vec![MaterializedSet { seq, rows, state: ResultSetState::default() }.result_set(&scope)]
    }

    pub fn project_scopes(&self) -> Vec<QueryScope> {
        let mut scopes = self.projects.keys().cloned().collect::<Vec<_>>();
        scopes.sort_by(|left, right| (&left.namespace, &left.name).cmp(&(&right.namespace, &right.name)));
        scopes
    }

    fn recompute_all(&mut self) -> Vec<ResultDelta> {
        let mut scopes = self.sets.keys().cloned().collect::<HashSet<_>>();
        scopes.insert(None);
        scopes.extend(self.projects.keys().cloned().map(Some));

        let mut scopes = scopes.into_iter().collect::<Vec<_>>();
        scopes.sort_by_key(scope_sort_key);
        scopes.into_iter().filter_map(|scope| self.recompute_scope(scope)).collect()
    }

    fn recompute_scope(&mut self, scope: Option<QueryScope>) -> Option<ResultDelta> {
        let replacement = self.materialize(&scope);
        let previous = self.sets.remove(&scope).unwrap_or_else(|| match &scope {
            Some(scope) => MaterializedSet::unavailable(scope, unavailable_message(scope)),
            None => MaterializedSet { seq: 0, rows: HashMap::new(), state: ResultSetState::default() },
        });
        let changed = replacement
            .rows
            .iter()
            .filter(|(reference, row)| previous.rows.get(*reference) != Some(*row))
            .map(|(_, row)| row.clone())
            .collect::<Vec<_>>();
        let removed = previous.rows.keys().filter(|reference| !replacement.rows.contains_key(*reference)).cloned().collect::<Vec<_>>();
        let state = (previous.state != replacement.state).then(|| replacement.state.clone());
        if changed.is_empty() && removed.is_empty() && state.is_none() {
            self.sets.insert(scope, previous);
            return None;
        }

        let materialized = MaterializedSet { seq: previous.seq.saturating_add(1), ..replacement };
        let seq = materialized.seq;
        self.sets.insert(scope.clone(), materialized);
        Some(ResultDelta { seq, changes: R::changes(scope, changed, removed), state })
    }

    fn materialize(&self, scope: &Option<QueryScope>) -> MaterializedSet<R> {
        match scope {
            None => {
                let repositories = self.local_by_repo.keys().chain(self.replicas_by_repo.keys()).cloned().collect::<HashSet<_>>();
                MaterializedSet { seq: 0, rows: self.rows_for_keys(repositories), state: ResultSetState::default() }
            }
            Some(project) => {
                let Some(repositories) = self.projects.get(project) else {
                    return MaterializedSet::unavailable(project, unavailable_message(project));
                };
                let missing = repositories.iter().filter(|repo| !self.known_repositories.contains(*repo)).collect::<Vec<_>>();
                if !missing.is_empty() {
                    return MaterializedSet::unavailable(
                        project,
                        format!(
                            "project {}/{} references {} unavailable {}",
                            project.namespace,
                            project.name,
                            missing.len(),
                            if missing.len() == 1 { "repository" } else { "repositories" }
                        ),
                    );
                }
                MaterializedSet {
                    seq: 0,
                    rows: self.rows_for_keys(repositories.iter().map(|repo| Some(repo.clone()))),
                    state: ResultSetState::default(),
                }
            }
        }
    }

    fn rows_for_keys(&self, repositories: impl IntoIterator<Item = Option<RepositoryKey>>) -> HashMap<ResourceRef, R> {
        let mut rows = HashMap::new();
        for repository in repositories {
            rows.extend(
                self.local_by_repo
                    .get(&repository)
                    .into_iter()
                    .flat_map(|rows| rows.iter())
                    .map(|(key, row)| (key.clone(), self.label_row(row))),
            );
            if let Some(replicas) = self.replicas_by_repo.get(&repository) {
                rows.extend(replicas.values().flat_map(|rows| rows.iter()).map(|(key, row)| (key.clone(), self.label_row(row))));
            }
        }
        rows
    }

    fn label_row(&self, row: &R) -> R {
        let mut row = row.clone();
        row.apply_repository_label(&self.repository_labels);
        row
    }
}

pub(crate) type ScopedCheckoutProjection = ScopedStoreProjection<CheckoutRow>;
pub(crate) type ScopedIndependentProjection = ScopedStoreProjection<IndependentRow>;

fn group_rows<R: ScopedStoreRow>(rows: Vec<R>) -> HashMap<Option<RepositoryKey>, HashMap<ResourceRef, R>> {
    let mut grouped: HashMap<Option<RepositoryKey>, HashMap<ResourceRef, R>> = HashMap::new();
    for row in rows {
        grouped.entry(row.repository_key().cloned()).or_default().insert(row.resource().clone(), row);
    }
    grouped
}

fn unavailable_message(scope: &QueryScope) -> String {
    format!("project {}/{} is unavailable", scope.namespace, scope.name)
}

fn scope_sort_key(scope: &Option<QueryScope>) -> String {
    match scope {
        None => String::new(),
        Some(scope) => format!("project/{}/{}", scope.namespace, scope.name),
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{LifecycleAuthority, QueryId, SessionPhase};

    use super::*;

    fn repo(value: &str) -> RepositoryKey {
        RepositoryKey(value.into())
    }

    fn checkout(repo: &str, name: &str, host: &str) -> CheckoutRow {
        CheckoutRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "Checkout", "flotilla", name).on_host(HostName::new(host)))
            .repo(self::repo(repo))
            .repo_label(name)
            .path(format!("/work/{name}"))
            .branch("main")
            .host(HostName::new(host))
            .authority(LifecycleAuthority::Observed)
            .build()
    }

    fn independent(repo: Option<&str>, name: &str, host: &str) -> IndependentRow {
        IndependentRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "TerminalSession", "flotilla", name).on_host(HostName::new(host)))
            .name(name)
            .maybe_repository_key(repo.map(self::repo))
            .host(HostName::new(host))
            .phase(SessionPhase::Running)
            .build()
    }

    #[test]
    fn fleet_scope_is_available_when_empty() {
        let mut projection = ScopedCheckoutProjection::default();
        let available = projection.result_set(&None);
        assert!(available.rows.is_empty());
        assert!(available.state.conditions.is_empty());
    }

    #[test]
    fn project_scope_unions_local_and_replica_repository_rows() {
        let mut projection = ScopedCheckoutProjection::default();
        let project = QueryScope::new("flotilla", "suite");
        projection.replace_catalog(
            HashMap::from([(repo("repo-a"), "a".to_string()), (repo("repo-b"), "b".to_string())]),
            HashMap::from([(project.clone(), vec![repo("repo-a"), repo("repo-b")])]),
        );
        projection.replace_local_rows(vec![checkout("repo-a", "local", "laptop")]);
        projection.replace_replica_rows(HashMap::from([(HostName::new("kiwi"), vec![checkout("repo-b", "remote", "kiwi")])]));

        let result = projection.result_set(&Some(project));
        let rows = result.rows.as_checkouts().expect("checkout rows");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.host == HostName::new("laptop")));
        assert!(rows.iter().any(|row| row.host == HostName::new("kiwi")));
    }

    #[test]
    fn replica_checkout_keeps_its_forge_slug_without_a_local_repository_catalog() {
        let mut projection = ScopedCheckoutProjection::default();
        let mut row = checkout("remote-repo", "widgets", "kiwi");
        row.repo_label = "flotilla-org/widgets".to_string();
        projection.replace_replica_rows(HashMap::from([(HostName::new("kiwi"), vec![row])]));

        let rows = projection.result_set(&None).rows.as_checkouts().expect("fleet checkout rows").to_vec();

        assert!(matches!(rows.as_slice(), [row] if row.repo_label == "flotilla-org/widgets"));
    }

    #[test]
    fn replica_independent_keeps_its_forge_slug_without_a_local_repository_catalog() {
        let mut projection = ScopedIndependentProjection::default();
        let mut row = independent(Some("remote-repo"), "governor", "kiwi");
        row.repo = Some(RepoKey("flotilla-org/widgets".to_string()));
        projection.replace_replica_rows(HashMap::from([(HostName::new("kiwi"), vec![row])]));

        let rows = projection.result_set(&None).rows.as_independents().expect("fleet independent rows").to_vec();

        assert!(matches!(rows.as_slice(), [row] if row.repo.as_ref().is_some_and(|label| label.0 == "flotilla-org/widgets")));
    }

    #[test]
    fn replica_rows_never_use_the_identity_key_as_their_display_label() {
        let repository = "opaque-repository-key";
        let mut checkouts = ScopedCheckoutProjection::default();
        let mut checkout = checkout(repository, "widgets", "kiwi");
        checkout.repo_label = repository.to_string();
        checkouts.replace_replica_rows(HashMap::from([(HostName::new("kiwi"), vec![checkout])]));

        let mut independents = ScopedIndependentProjection::default();
        let mut independent = independent(Some(repository), "governor", "kiwi");
        independent.repo = Some(RepoKey(repository.to_string()));
        independents.replace_replica_rows(HashMap::from([(HostName::new("kiwi"), vec![independent])]));

        let checkout_rows = checkouts.result_set(&None).rows.as_checkouts().expect("fleet checkout rows").to_vec();
        let independent_rows = independents.result_set(&None).rows.as_independents().expect("fleet independent rows").to_vec();
        assert!(matches!(checkout_rows.as_slice(), [row] if row.repo_label == UNKNOWN_REPOSITORY_LABEL));
        assert!(matches!(
            independent_rows.as_slice(),
            [row] if row.repo.as_ref().is_some_and(|label| label.0 == UNKNOWN_REPOSITORY_LABEL)
        ));
    }

    #[test]
    fn project_independents_exclude_repositoryless_rows_but_fleet_keeps_them() {
        let mut projection = ScopedIndependentProjection::default();
        let project = QueryScope::new("flotilla", "suite");
        projection
            .replace_catalog(HashMap::from([(repo("repo-a"), "a".to_string())]), HashMap::from([(project.clone(), vec![repo("repo-a")])]));
        projection.replace_local_rows(vec![independent(Some("repo-a"), "governor", "laptop"), independent(None, "yeoman", "laptop")]);

        let project_rows = projection.result_set(&Some(project)).rows.as_independents().expect("project independent rows").to_vec();
        let fleet_rows = projection.result_set(&None).rows.as_independents().expect("fleet independent rows").to_vec();

        assert!(matches!(project_rows.as_slice(), [row] if row.name == "governor"));
        assert_eq!(fleet_rows.len(), 2);
    }

    #[test]
    fn checkout_fleet_export_is_one_unscoped_local_result_set() {
        let mut projection = ScopedCheckoutProjection::default();
        let repository = repo("repo-a");
        let project = QueryScope::new("flotilla", "suite");
        projection.replace_catalog(HashMap::from([(repository.clone(), "a".to_string())]), HashMap::from([(project, vec![repository])]));

        let sets = projection.local_result_sets();

        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].query(), QueryId::Checkouts { scope: None });
        assert!(sets[0].rows.is_empty());
        assert!(sets[0].state.conditions.is_empty());
    }

    #[test]
    fn checkout_fleet_export_preserves_the_source_qualified_label() {
        let mut projection = ScopedCheckoutProjection::default();
        let repository = repo("repo-a");
        projection.replace_catalog(HashMap::from([(repository, "flotilla-org/widgets".to_string())]), HashMap::new());
        let mut row = checkout("repo-a", "widgets", "laptop");
        row.repo_label = "github.com/flotilla-org/widgets".to_string();
        projection.replace_local_rows(vec![row]);

        let local_rows = projection.result_set(&None).rows.as_checkouts().expect("local checkout rows").to_vec();
        let exported_rows = projection.local_result_sets()[0].rows.as_checkouts().expect("exported checkout rows").to_vec();

        assert!(matches!(local_rows.as_slice(), [row] if row.repo_label == "flotilla-org/widgets"));
        assert!(matches!(exported_rows.as_slice(), [row] if row.repo_label == "github.com/flotilla-org/widgets"));
    }

    #[test]
    fn project_membership_change_emits_typed_checkout_addition_and_removal() {
        let mut projection = ScopedCheckoutProjection::default();
        let project = QueryScope::new("flotilla", "suite");
        let repositories = HashMap::from([(repo("repo-a"), "a".to_string()), (repo("repo-b"), "b".to_string())]);
        projection.replace_catalog(repositories.clone(), HashMap::from([(project.clone(), vec![repo("repo-a")])]));
        projection.replace_local_rows(vec![checkout("repo-a", "a", "local"), checkout("repo-b", "b", "local")]);

        let deltas = projection.replace_catalog(repositories, HashMap::from([(project.clone(), vec![repo("repo-b")])]));
        let delta =
            deltas.into_iter().find(|delta| delta.query() == QueryId::Checkouts { scope: Some(project.clone()) }).expect("project delta");
        assert_eq!(delta.changes.as_checkouts().expect("changed checkout")[0].resource.name, "b");
        assert_eq!(delta.changes.removed_resources().expect("removed checkout")[0].name, "a");
    }

    #[test]
    fn independent_fleet_export_is_unscoped_and_local_only() {
        let mut projection = ScopedIndependentProjection::default();
        projection.replace_catalog(HashMap::from([(repo("repo-a"), "flotilla-org/widgets".to_string())]), HashMap::new());
        let mut local = independent(Some("repo-a"), "local", "laptop");
        local.repo = Some(RepoKey("github.com/flotilla-org/widgets".to_string()));
        projection.replace_local_rows(vec![local]);
        projection.replace_replica_rows(HashMap::from([(HostName::new("kiwi"), vec![independent(Some("repo-a"), "remote", "kiwi")])]));

        let sets = projection.local_result_sets();
        let local_rows = projection.result_set(&None).rows.as_independents().expect("local independent rows").to_vec();

        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].query(), QueryId::Independents { scope: None });
        assert!(matches!(
            sets[0].rows.as_independents().expect("exported independent rows"),
            [row] if row.name == "local"
                && row.repo.as_ref().is_some_and(|label| label.0 == "github.com/flotilla-org/widgets")
        ));
        assert!(local_rows.iter().all(|row| row.repo.as_ref().is_some_and(|label| label.0 == "flotilla-org/widgets")));
    }
}
