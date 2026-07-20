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

    fn result_set(&self, scope: &Option<QueryScope>) -> ResultSet {
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
    sets: HashMap<Option<QueryScope>, MaterializedSet>,
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

    pub fn result_set(&mut self, scope: &Option<QueryScope>) -> ResultSet {
        if !self.sets.contains_key(scope) {
            let materialized = self.materialize(scope);
            self.sets.insert(scope.clone(), materialized);
        }
        self.sets.get(scope).expect("checkout scope inserted").result_set(scope)
    }

    /// Local checkout facts are the federation unit. Project views are
    /// derived after the receiver groups fleet rows by their Repository key.
    pub fn local_result_sets(&self) -> Vec<ResultSet> {
        let rows = self.local_by_repo.values().flat_map(|rows| rows.iter()).map(|(key, row)| (key.clone(), row.clone())).collect();
        let scope = None;
        let seq = self.sets.get(&scope).map_or(0, |set| set.seq);
        vec![MaterializedSet { seq, rows, state: ResultSetState::default() }.result_set(&scope)]
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
        Some(ResultDelta { seq, changes: QueryChanges::Checkouts { scope, changed, removed }, state })
    }

    fn materialize(&self, scope: &Option<QueryScope>) -> MaterializedSet {
        match scope {
            None => {
                let rows = self
                    .known_repositories
                    .iter()
                    .chain(self.local_by_repo.keys())
                    .chain(self.replicas_by_repo.keys())
                    .flat_map(|repo| self.rows_for_repo(repo))
                    .collect();
                MaterializedSet { seq: 0, rows, state: ResultSetState::default() }
            }
            Some(project) => {
                let Some(repositories) = self.projects.get(project) else {
                    return MaterializedSet::unavailable(project, unavailable_message(project));
                };
                let missing = repositories.iter().filter(|repo| !self.known_repositories.contains(*repo)).collect::<Vec<_>>();
                if !missing.is_empty() {
                    let missing = missing.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                    return MaterializedSet::unavailable(
                        project,
                        format!("project {}/{} references unavailable repositories: {missing}", project.namespace, project.name),
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
            HashSet::from([repo("repo-a"), repo("repo-b")]),
            HashMap::from([(project.clone(), vec![repo("repo-a"), repo("repo-b")])]),
        );
        projection.replace_local_rows(vec![row("repo-a", "local", "laptop")]);
        projection.replace_replica_rows(HashMap::from([(HostName::new("kiwi"), vec![row("repo-b", "remote", "kiwi")])]));

        let result = projection.result_set(&Some(project));
        let rows = result.rows.as_checkouts().expect("checkout rows");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.host == HostName::new("laptop")));
        assert!(rows.iter().any(|row| row.host == HostName::new("kiwi")));
    }

    #[test]
    fn fleet_export_is_one_unscoped_local_result_set() {
        let mut projection = ScopedCheckoutProjection::default();
        let repository = repo("repo-a");
        let project = QueryScope::new("flotilla", "suite");
        projection.replace_catalog(HashSet::from([repository.clone()]), HashMap::from([(project, vec![repository.clone()])]));

        let sets = projection.local_result_sets();

        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].query(), QueryId::Checkouts { scope: None });
        assert!(sets[0].rows.is_empty());
        assert!(sets[0].state.conditions.is_empty());
    }

    #[test]
    fn project_membership_change_emits_typed_addition_and_removal() {
        let mut projection = ScopedCheckoutProjection::default();
        let project = QueryScope::new("flotilla", "suite");
        projection
            .replace_catalog(HashSet::from([repo("repo-a"), repo("repo-b")]), HashMap::from([(project.clone(), vec![repo("repo-a")])]));
        projection.replace_local_rows(vec![row("repo-a", "a", "local"), row("repo-b", "b", "local")]);

        let deltas = projection
            .replace_catalog(HashSet::from([repo("repo-a"), repo("repo-b")]), HashMap::from([(project.clone(), vec![repo("repo-b")])]));
        let delta =
            deltas.into_iter().find(|delta| delta.query() == QueryId::Checkouts { scope: Some(project.clone()) }).expect("project delta");
        assert_eq!(delta.changes.as_checkouts().expect("changed checkout")[0].resource.name, "b");
        assert_eq!(delta.changes.removed_resources().expect("removed checkout")[0].name, "a");
    }
}
