//! Authoritative Project repository definitions, including known-empty Projects.
//!
//! The Aggregator reads Projects through the replica-aware resolver. Scoped
//! views filter on the Project identity, independent of activity.

use std::collections::{HashMap, HashSet};

use flotilla_protocol::{ProjectRepositoriesRow, QueryChanges, QueryScope, ResourceRef, ResultDelta, ResultSet, ResultSetState, Rows};

#[derive(Debug, Default)]
pub(crate) struct ProjectRepositoryProjection {
    rows: HashMap<ResourceRef, ProjectRepositoriesRow>,
    /// Materialized views and the sequence each last published.
    sets: HashMap<Option<QueryScope>, (u64, HashMap<ResourceRef, ProjectRepositoriesRow>)>,
}

impl ProjectRepositoryProjection {
    pub fn replace_rows(&mut self, rows: Vec<ProjectRepositoriesRow>) -> Vec<ResultDelta> {
        self.rows = rows.into_iter().map(|row| (row.resource.clone(), row)).collect();
        let mut scopes = self.sets.keys().cloned().collect::<HashSet<_>>();
        scopes.insert(None);
        let mut scopes = scopes.into_iter().collect::<Vec<_>>();
        scopes.sort_by_key(|scope| scope.as_ref().map(|scope| (scope.namespace.clone(), scope.name.clone())));
        scopes.into_iter().filter_map(|scope| self.recompute(scope)).collect()
    }

    pub fn result_set(&mut self, scope: &Option<QueryScope>) -> ResultSet {
        if !self.sets.contains_key(scope) {
            let rows = self.materialize(scope);
            self.sets.insert(scope.clone(), (0, rows));
        }
        let (seq, rows) = self.sets.get(scope).expect("project repository set inserted");
        let mut rows = rows.values().cloned().collect::<Vec<_>>();
        rows.sort_by(|left, right| (&left.resource.namespace, &left.resource.name).cmp(&(&right.resource.namespace, &right.resource.name)));
        ResultSet { seq: *seq, rows: Rows::ProjectRepositories { scope: scope.clone(), rows }, state: ResultSetState::default() }
    }

    fn recompute(&mut self, scope: Option<QueryScope>) -> Option<ResultDelta> {
        let replacement = self.materialize(&scope);
        let (seq, previous) = self.sets.remove(&scope).unwrap_or_default();
        let changed = replacement
            .iter()
            .filter(|(reference, row)| previous.get(*reference) != Some(*row))
            .map(|(_, row)| row.clone())
            .collect::<Vec<_>>();
        let removed = previous.keys().filter(|reference| !replacement.contains_key(*reference)).cloned().collect::<Vec<_>>();
        if changed.is_empty() && removed.is_empty() {
            self.sets.insert(scope, (seq, previous));
            return None;
        }
        let seq = seq.saturating_add(1);
        self.sets.insert(scope.clone(), (seq, replacement));
        Some(ResultDelta { seq, changes: QueryChanges::ProjectRepositories { scope, changed, removed }, state: None })
    }

    fn materialize(&self, scope: &Option<QueryScope>) -> HashMap<ResourceRef, ProjectRepositoriesRow> {
        self.rows
            .iter()
            .filter(|(_, row)| scope.as_ref().is_none_or(|scope| project_matches_scope(row, scope)))
            .map(|(reference, row)| (reference.clone(), row.clone()))
            .collect()
    }
}

fn project_matches_scope(row: &ProjectRepositoriesRow, scope: &QueryScope) -> bool {
    row.resource.namespace == scope.namespace && row.resource.name == scope.name
}
