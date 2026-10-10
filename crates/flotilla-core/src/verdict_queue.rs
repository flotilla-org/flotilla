//! Human verdict queue maintained from changed Convoy watch records. No store reads.
use std::collections::{HashMap, HashSet};

use flotilla_protocol::{QueryChanges, QueryScope, ResourceRef, ResultDelta, ResultSet, ResultSetState, Rows, VerdictQueueRow};

#[derive(Debug, Default)]
pub(crate) struct VerdictQueueProjection {
    rows: HashMap<ResourceRef, VerdictQueueRow>,
    /// Materialized views and the sequence each last published.
    sets: HashMap<Option<QueryScope>, (u64, HashMap<ResourceRef, VerdictQueueRow>)>,
}

impl VerdictQueueProjection {
    pub fn update_convoy(&mut self, convoy: &ResourceRef, rows: Vec<VerdictQueueRow>) -> Vec<ResultDelta> {
        self.rows.retain(|_, row| &row.convoy != convoy);
        self.rows.extend(rows.into_iter().map(|row| (row.resource.clone(), row)));
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
        let (seq, rows) = self.sets.get(scope).expect("verdict queue set inserted");
        let mut rows = rows.values().cloned().collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            (&left.submitted_at, &left.resource.namespace, &left.resource.name, &left.resource.host).cmp(&(
                &right.submitted_at,
                &right.resource.namespace,
                &right.resource.name,
                &right.resource.host,
            ))
        });
        ResultSet { seq: *seq, rows: Rows::VerdictQueue { scope: scope.clone(), rows }, state: ResultSetState::default() }
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
        Some(ResultDelta { seq, changes: QueryChanges::VerdictQueue { scope, changed, removed }, state: None })
    }

    fn materialize(&self, scope: &Option<QueryScope>) -> HashMap<ResourceRef, VerdictQueueRow> {
        self.rows
            .iter()
            .filter(|(_, row)| scope.as_ref().is_none_or(|scope| submission_matches_scope(row, scope)))
            .map(|(reference, row)| (reference.clone(), row.clone()))
            .collect()
    }
}

fn submission_matches_scope(row: &VerdictQueueRow, scope: &QueryScope) -> bool {
    row.resource.namespace == scope.namespace
        && (row.project_ref.as_deref() == Some(scope.name.as_str())
            || row.project_ref.as_deref() == Some(format!("{}/{}", scope.namespace, scope.name).as_str()))
}
