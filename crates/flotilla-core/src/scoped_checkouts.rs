//! Warm scoped checkout projections over fleet-merged observed facts.

use std::collections::{HashMap, HashSet};

use flotilla_protocol::{
    CheckoutRow, HostName, QueryChanges, QueryScope, RepositoryKey, ResourceRef, ResultDelta, ResultSet, ResultSetCondition,
    ResultSetState, Rows,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MaterializedSet {
    seq: u64,
    rows: HashMap<ResourceRef, CheckoutRow>,
    state: ResultSetState,
}

impl MaterializedSet {
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

    fn result_set(&self, scope: &QueryScope) -> ResultSet {
        let mut rows = self.rows.values().cloned().collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            (&left.host, &left.path, &left.resource.namespace, &left.resource.name).cmp(&(
                &right.host,
                &right.path,
                &right.resource.namespace,
                &right.resource.name,
            ))
        });
        ResultSet { seq: self.seq, rows: Rows::Checkouts { scope: scope.clone(), rows }, state: self.state.clone() }
    }
}

/// All warm checkout facts and their finite referent-backed scoped views.
#[derive(Debug, Default, bon::Builder)]
#[builder(builder_type(vis = "pub(crate)"))]
pub struct ScopedCheckoutProjection {
    known_repositories: HashSet<RepositoryKey>,
    projects: HashMap<QueryScope, Vec<RepositoryKey>>,
    local_by_repo: HashMap<RepositoryKey, HashMap<ResourceRef, CheckoutRow>>,
    replicas_by_repo: HashMap<RepositoryKey, HashMap<HostName, HashMap<ResourceRef, CheckoutRow>>>,
    sets: HashMap<QueryScope, MaterializedSet>,
}

impl ScopedCheckoutProjection {
    pub fn replace_catalog(
        &mut self,
        repositories: HashSet<RepositoryKey>,
        projects: HashMap<QueryScope, Vec<RepositoryKey>>,
    ) -> Vec<ResultDelta> {
        self.known_repositories = repositories;
        self.projects = projects;
        self.recompute_all()
    }

    pub fn replace_local_rows(&mut self, rows: Vec<CheckoutRow>) -> Vec<ResultDelta> {
        self.local_by_repo = group_rows(rows);
        self.recompute_all()
    }

    pub fn replace_replica_rows(&mut self, replicas: HashMap<HostName, Vec<CheckoutRow>>) -> Vec<ResultDelta> {
        self.replicas_by_repo.clear();
        for (host, rows) in replicas {
            for (repo, rows) in group_rows(rows) {
                self.replicas_by_repo.entry(repo).or_default().insert(host.clone(), rows);
            }
        }
        self.recompute_all()
    }

    pub fn result_set(&mut self, scope: &QueryScope) -> ResultSet {
        if !self.sets.contains_key(scope) {
            let materialized = self.materialize(scope);
            self.sets.insert(scope.clone(), materialized);
        }
        self.sets.get(scope).expect("checkout scope inserted").result_set(scope)
    }

    /// Local Repository-scoped facts are the federation unit. Project views
    /// are derived after the receiver has merged every Repository scope.
    pub fn local_result_sets(&self) -> Vec<ResultSet> {
        let mut scopes = self.known_repositories.iter().cloned().collect::<Vec<_>>();
        scopes.sort();
        scopes
            .into_iter()
            .map(|repo| {
                let rows = self.local_by_repo.get(&repo).cloned().unwrap_or_default();
                let scope = QueryScope::Repository(repo);
                let seq = self.sets.get(&scope).map_or(0, |set| set.seq);
                let materialized = MaterializedSet { seq, rows, state: ResultSetState::default() };
                materialized.result_set(&scope)
            })
            .collect()
    }

    fn recompute_all(&mut self) -> Vec<ResultDelta> {
        let mut scopes = self.sets.keys().cloned().collect::<HashSet<_>>();
        scopes.extend(self.known_repositories.iter().cloned().map(QueryScope::Repository));
        scopes.extend(self.projects.keys().cloned());
        scopes.extend(self.local_by_repo.keys().cloned().map(QueryScope::Repository));
        scopes.extend(self.replicas_by_repo.keys().cloned().map(QueryScope::Repository));

        let mut scopes = scopes.into_iter().collect::<Vec<_>>();
        scopes.sort_by_key(scope_sort_key);
        scopes.into_iter().filter_map(|scope| self.recompute_scope(scope)).collect()
    }

    fn recompute_scope(&mut self, scope: QueryScope) -> Option<ResultDelta> {
        let replacement = self.materialize(&scope);
        let previous = self.sets.remove(&scope).unwrap_or_else(|| MaterializedSet::unavailable(&scope, unavailable_message(&scope)));
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
        Some(ResultDelta { seq, changes: QueryChanges::Checkouts { scope, changed, removed }, state })
    }

    fn materialize(&self, scope: &QueryScope) -> MaterializedSet {
        match scope {
            QueryScope::Repository(repo) => {
                if !self.known_repositories.contains(repo) {
                    return MaterializedSet::unavailable(scope, unavailable_message(scope));
                }
                MaterializedSet { seq: 0, rows: self.rows_for_repo(repo), state: ResultSetState::default() }
            }
            QueryScope::Project { namespace, name } => {
                let Some(repositories) = self.projects.get(scope) else {
                    return MaterializedSet::unavailable(scope, unavailable_message(scope));
                };
                let missing = repositories.iter().filter(|repo| !self.known_repositories.contains(*repo)).collect::<Vec<_>>();
                if !missing.is_empty() {
                    let missing = missing.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                    return MaterializedSet::unavailable(
                        scope,
                        format!("project {namespace}/{name} references unavailable repositories: {missing}"),
                    );
                }
                let rows = repositories.iter().flat_map(|repo| self.rows_for_repo(repo)).collect();
                MaterializedSet { seq: 0, rows, state: ResultSetState::default() }
            }
        }
    }

    fn rows_for_repo(&self, repo: &RepositoryKey) -> HashMap<ResourceRef, CheckoutRow> {
        let mut rows = self.local_by_repo.get(repo).cloned().unwrap_or_default();
        if let Some(replicas) = self.replicas_by_repo.get(repo) {
            rows.extend(replicas.values().flat_map(|rows| rows.iter()).map(|(reference, row)| (reference.clone(), row.clone())));
        }
        rows
    }
}

fn group_rows(rows: Vec<CheckoutRow>) -> HashMap<RepositoryKey, HashMap<ResourceRef, CheckoutRow>> {
    let mut grouped: HashMap<RepositoryKey, HashMap<ResourceRef, CheckoutRow>> = HashMap::new();
    for row in rows {
        grouped.entry(row.repo.clone()).or_default().insert(row.resource.clone(), row);
    }
    grouped
}

fn unavailable_message(scope: &QueryScope) -> String {
    match scope {
        QueryScope::Repository(repo) => format!("repository {repo} is unavailable"),
        QueryScope::Project { namespace, name } => format!("project {namespace}/{name} is unavailable"),
    }
}

fn scope_sort_key(scope: &QueryScope) -> String {
    match scope {
        QueryScope::Repository(repo) => format!("repository/{repo}"),
        QueryScope::Project { namespace, name } => format!("project/{namespace}/{name}"),
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{LifecycleAuthority, QueryId};

    use super::*;

    fn repo(value: &str) -> RepositoryKey {
        RepositoryKey(value.into())
    }

    fn row(repo: &str, name: &str, host: &str) -> CheckoutRow {
        CheckoutRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "Checkout", "flotilla", name).on_host(HostName::new(host)))
            .repo(self::repo(repo))
            .path(format!("/work/{name}"))
            .branch("main")
            .host(HostName::new(host))
            .authority(LifecycleAuthority::Observed)
            .build()
    }

    #[test]
    fn repository_scope_distinguishes_unavailable_from_valid_empty() {
        let mut projection = ScopedCheckoutProjection::default();
        let scope = QueryScope::Repository(repo("repo-a"));
        let missing = projection.result_set(&scope);
        assert!(matches!(missing.state.conditions.as_slice(), [ResultSetCondition::QueryScopeUnavailable { .. }]));

        let deltas = projection.replace_catalog(HashSet::from([repo("repo-a")]), HashMap::new());
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].query(), QueryId::Checkouts { scope: scope.clone() });
        assert!(deltas[0].state.as_ref().expect("condition clear").conditions.is_empty());
        let available = projection.result_set(&scope);
        assert!(available.rows.is_empty());
        assert!(available.state.conditions.is_empty());
    }

    #[test]
    fn project_scope_unions_local_and_replica_repository_rows() {
        let mut projection = ScopedCheckoutProjection::default();
        let project = QueryScope::Project { namespace: "flotilla".into(), name: "suite".into() };
        projection.replace_catalog(
            HashSet::from([repo("repo-a"), repo("repo-b")]),
            HashMap::from([(project.clone(), vec![repo("repo-a"), repo("repo-b")])]),
        );
        projection.replace_local_rows(vec![row("repo-a", "local", "laptop")]);
        projection.replace_replica_rows(HashMap::from([(HostName::new("kiwi"), vec![row("repo-b", "remote", "kiwi")])]));

        let result = projection.result_set(&project);
        let rows = result.rows.as_checkouts().expect("checkout rows");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.host == HostName::new("laptop")));
        assert!(rows.iter().any(|row| row.host == HostName::new("kiwi")));
    }

    #[test]
    fn fleet_export_includes_empty_repository_scopes_and_excludes_projects() {
        let mut projection = ScopedCheckoutProjection::default();
        let repository = repo("repo-a");
        let project = QueryScope::Project { namespace: "flotilla".into(), name: "suite".into() };
        projection.replace_catalog(HashSet::from([repository.clone()]), HashMap::from([(project, vec![repository.clone()])]));

        let sets = projection.local_result_sets();

        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].query(), QueryId::Checkouts { scope: QueryScope::Repository(repository) });
        assert!(sets[0].rows.is_empty());
        assert!(sets[0].state.conditions.is_empty());
    }

    #[test]
    fn project_membership_change_emits_typed_addition_and_removal() {
        let mut projection = ScopedCheckoutProjection::default();
        let project = QueryScope::Project { namespace: "flotilla".into(), name: "suite".into() };
        projection
            .replace_catalog(HashSet::from([repo("repo-a"), repo("repo-b")]), HashMap::from([(project.clone(), vec![repo("repo-a")])]));
        projection.replace_local_rows(vec![row("repo-a", "a", "local"), row("repo-b", "b", "local")]);

        let deltas = projection
            .replace_catalog(HashSet::from([repo("repo-a"), repo("repo-b")]), HashMap::from([(project.clone(), vec![repo("repo-b")])]));
        let delta = deltas.into_iter().find(|delta| delta.query() == QueryId::Checkouts { scope: project.clone() }).expect("project delta");
        assert_eq!(delta.changes.as_checkouts().expect("changed checkout")[0].resource.name, "b");
        assert_eq!(delta.changes.removed_resources().expect("removed checkout")[0].name, "a");
    }
}
