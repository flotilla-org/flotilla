//! Standing project roles, keyed by their `ConvoyEnsure` declaration.
//!
//! Ensures are replicated definitions, so the Aggregator's replica-aware read
//! already yields the fleet view: there is no local/replica split to merge.
//! Project views filter by the declaration's own `project_ref`; they need no
//! repository catalog.

use std::collections::{HashMap, HashSet};

use flotilla_protocol::{QueryChanges, QueryScope, ResourceRef, ResultDelta, ResultSet, ResultSetState, Rows, StandingRoleRow};

#[derive(Debug, Default)]
pub(crate) struct StandingRoleProjection {
    rows: HashMap<ResourceRef, StandingRoleRow>,
    /// Materialized views and the sequence each last published.
    sets: HashMap<Option<QueryScope>, (u64, HashMap<ResourceRef, StandingRoleRow>)>,
}

impl StandingRoleProjection {
    pub fn replace_rows(&mut self, rows: Vec<StandingRoleRow>) -> Vec<ResultDelta> {
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
        let (seq, rows) = self.sets.get(scope).expect("standing role set inserted");
        let mut rows = rows.values().cloned().collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            (&left.resource.namespace, &left.project_ref, &left.role, &left.resource.host).cmp(&(
                &right.resource.namespace,
                &right.project_ref,
                &right.role,
                &right.resource.host,
            ))
        });
        ResultSet { seq: *seq, rows: Rows::StandingRoles { scope: scope.clone(), rows }, state: ResultSetState::default() }
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
        Some(ResultDelta { seq, changes: QueryChanges::StandingRoles { scope, changed, removed }, state: None })
    }

    fn materialize(&self, scope: &Option<QueryScope>) -> HashMap<ResourceRef, StandingRoleRow> {
        self.rows
            .iter()
            .filter(|(_, row)| scope.as_ref().is_none_or(|scope| role_matches_scope(row, scope)))
            .map(|(reference, row)| (reference.clone(), row.clone()))
            .collect()
    }
}

fn role_matches_scope(row: &StandingRoleRow, scope: &QueryScope) -> bool {
    row.resource.namespace == scope.namespace
        && (row.project_ref == scope.name || row.project_ref == format!("{}/{}", scope.namespace, scope.name))
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::QueryId;

    use super::*;

    fn role(project: &str, role: &str) -> StandingRoleRow {
        StandingRoleRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "ConvoyEnsure", "flotilla", format!("ensure-{project}-{role}")))
            .project_ref(project)
            .role(role)
            .build()
    }

    #[test]
    fn unscoped_view_publishes_changes_and_removals_with_contiguous_sequences() {
        let mut projection = StandingRoleProjection::default();
        let added = projection.replace_rows(vec![role("flotilla", "governor")]);
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].seq, 1);
        assert_eq!(added[0].query(), QueryId::StandingRoles { scope: None });

        assert!(projection.replace_rows(vec![role("flotilla", "governor")]).is_empty(), "unchanged rows emit nothing");

        let removed = projection.replace_rows(vec![]);
        assert_eq!(removed[0].seq, 2);
        assert_eq!(removed[0].changes.removed_resources().map(<[_]>::len), Some(1));
    }

    #[test]
    fn project_view_filters_by_declared_project() {
        let mut projection = StandingRoleProjection::default();
        let scope = Some(QueryScope::new("flotilla", "andamento"));
        assert!(projection.result_set(&scope).rows.is_empty());

        let deltas = projection.replace_rows(vec![role("flotilla", "governor"), role("andamento", "governor")]);
        let scoped = deltas.iter().find(|delta| delta.query() == QueryId::StandingRoles { scope: scope.clone() }).expect("scoped delta");
        let changed = scoped.changes.as_standing_roles().expect("standing role rows");
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].project_ref, "andamento");
        assert_eq!(projection.result_set(&None).rows.len(), 2);
    }
}
