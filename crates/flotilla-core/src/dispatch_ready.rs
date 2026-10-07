use std::collections::{HashMap, HashSet};

use flotilla_protocol::{DispatchQueueRow, DispatchReadyKey, QueryChanges, QueryScope, ResultDelta, ResultSet, ResultSetState, Rows};

type ReadyRows = HashMap<DispatchReadyKey, DispatchQueueRow>;
type ReadySet = (u64, ReadyRows, ResultSetState);

#[derive(Debug, Default)]
pub(crate) struct DispatchReadyProjection {
    rows: ReadyRows,
    errors: Vec<(QueryScope, String)>,
    /// Materialized views and the sequence each last published.
    sets: HashMap<Option<QueryScope>, ReadySet>,
}

impl DispatchReadyProjection {
    pub fn replace_rows(&mut self, rows: Vec<DispatchQueueRow>, errors: Vec<(QueryScope, String)>) -> Vec<ResultDelta> {
        self.errors = errors;
        self.rows = rows.into_iter().map(|row| (row.key(), row)).collect();
        let mut scopes = self.sets.keys().cloned().collect::<HashSet<_>>();
        scopes.insert(None);
        let mut scopes = scopes.into_iter().collect::<Vec<_>>();
        scopes.sort_by_key(|scope| scope.as_ref().map(|scope| (scope.namespace.clone(), scope.name.clone())));
        scopes.into_iter().filter_map(|scope| self.recompute(scope)).collect()
    }

    pub fn result_set(&mut self, scope: &Option<QueryScope>) -> ResultSet {
        if !self.sets.contains_key(scope) {
            let rows = self.materialize(scope);
            self.sets.insert(scope.clone(), (0, rows, self.materialize_state(scope)));
        }
        let (seq, rows, state) = self.sets.get(scope).expect("dispatch ready set inserted");
        let mut rows = rows.values().cloned().collect::<Vec<_>>();
        rows.sort_by(flotilla_protocol::compare_dispatch_rows);
        ResultSet { seq: *seq, rows: Rows::DispatchReady { scope: scope.clone(), rows }, state: state.clone() }
    }

    fn recompute(&mut self, scope: Option<QueryScope>) -> Option<ResultDelta> {
        let replacement = self.materialize(&scope);
        let (seq, previous, previous_state) = self.sets.remove(&scope).unwrap_or_default();
        let state = self.materialize_state(&scope);
        let changed = replacement
            .iter()
            .filter(|(reference, row)| previous.get(*reference) != Some(*row))
            .map(|(_, row)| row.clone())
            .collect::<Vec<_>>();
        let removed = previous.keys().filter(|reference| !replacement.contains_key(*reference)).cloned().collect::<Vec<_>>();
        if changed.is_empty() && removed.is_empty() && state == previous_state {
            self.sets.insert(scope, (seq, previous, previous_state));
            return None;
        }
        let seq = seq.saturating_add(1);
        self.sets.insert(scope.clone(), (seq, replacement, state.clone()));
        Some(ResultDelta {
            seq,
            changes: QueryChanges::DispatchReady { scope, changed, removed },
            state: (state != previous_state).then_some(state),
        })
    }

    fn materialize_state(&self, scope: &Option<QueryScope>) -> ResultSetState {
        let conditions = self
            .errors
            .iter()
            .filter(|(project, _)| scope.as_ref().is_none_or(|scope| scope == project))
            .map(|(scope, message)| flotilla_protocol::ResultSetCondition::QueryScopeUnavailable {
                scope: scope.clone(),
                message: message.clone(),
            })
            .collect();
        ResultSetState { conditions, ..Default::default() }
    }

    fn materialize(&self, scope: &Option<QueryScope>) -> HashMap<DispatchReadyKey, DispatchQueueRow> {
        self.rows
            .iter()
            .filter(|(_, row)| scope.as_ref().is_none_or(|scope| project_matches_scope(row, scope)))
            .map(|(reference, row)| (reference.clone(), row.clone()))
            .collect()
    }
}

fn project_matches_scope(row: &DispatchQueueRow, scope: &QueryScope) -> bool {
    row.namespace == scope.namespace && row.project == scope.name
}

#[cfg(test)]
mod tests {
    use super::*;

    // A Project-qualified identity prevents the same issue in two Projects
    // colliding. Replacement is idempotent, and removals name only their Project.
    #[hegel::test]
    fn scoped_snapshots_and_deltas_preserve_identity(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let include_a = tc.draw(gs::booleans());
        let include_b = tc.draw(gs::booleans());
        let unavailable = tc.draw(gs::booleans());
        let row = |project: &str| {
            DispatchQueueRow::builder()
                .namespace("flotilla".into())
                .project(project.into())
                .issue(flotilla_protocol::IssueRef {
                    source: flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "org/repo".into() },
                    id: "2".into(),
                })
                .title("Work".into())
                .ready_observed_at("2026-10-06T12:00:00Z".parse().expect("time"))
                .age_seconds(0)
                .attention(false)
                .provenance("daemon".into())
                .build()
        };
        let mut projection = DispatchReadyProjection::default();
        let scope = Some(QueryScope::new("flotilla", "a"));
        projection.result_set(&scope);
        let rows =
            [(include_a, row("a")), (include_b, row("b"))].into_iter().filter_map(|(keep, row)| keep.then_some(row)).collect::<Vec<_>>();
        let errors = if unavailable { vec![(QueryScope::new("flotilla", "b"), "tracker unavailable".into())] } else { vec![] };
        projection.replace_rows(rows.clone(), errors.clone());
        let fleet = projection.result_set(&None);
        assert_eq!(fleet.rows.len(), usize::from(include_a) + usize::from(include_b));
        assert_eq!(fleet.state.conditions.len(), usize::from(unavailable));
        let scoped = projection.result_set(&scope);
        assert_eq!(scoped.rows.len(), usize::from(include_a));
        assert!(scoped.state.conditions.is_empty());
        assert!(projection.replace_rows(rows, errors).is_empty(), "identical input must not emit a delta");
        let deltas = projection.replace_rows(if include_b { vec![row("b")] } else { vec![] }, vec![]);
        for delta in deltas {
            let QueryChanges::DispatchReady { removed, .. } = delta.changes else { panic!("ready delta") };
            assert!(removed.iter().all(|key| key.project == "a"));
        }
        assert_eq!(projection.result_set(&None).rows.len(), usize::from(include_b));
        assert_eq!(projection.result_set(&scope).rows.len(), 0);
    }
}
