//! Delta computation: diff two IndexMaps to produce keyed entry operations,
//! and full `ProviderData` snapshot diffing.

use std::hash::Hash;

use flotilla_protocol::{Change, EntryOp, ProviderData, ProviderError, WorkItem, WorkItemIdentity};
use indexmap::IndexMap;

/// Diff two IndexMaps, producing `(key, EntryOp)` pairs for all differences.
///
/// - Keys in `curr` but not `prev` → `Added`
/// - Keys in both with different values → `Updated`
/// - Keys in `prev` but not `curr` → `Removed`
/// - Keys in both with equal values → omitted
pub fn diff_indexmap<K, V>(prev: &IndexMap<K, V>, curr: &IndexMap<K, V>) -> Vec<(K, EntryOp<V>)>
where
    K: Clone + Eq + Hash,
    V: Clone + PartialEq,
{
    let mut changes = Vec::new();

    for (key, curr_val) in curr {
        match prev.get(key) {
            Some(prev_val) if prev_val == curr_val => {}
            Some(_) => changes.push((key.clone(), EntryOp::Updated(curr_val.clone()))),
            None => changes.push((key.clone(), EntryOp::Added(curr_val.clone()))),
        }
    }

    for key in prev.keys() {
        if !curr.contains_key(key) {
            changes.push((key.clone(), EntryOp::Removed));
        }
    }

    changes
}

/// Diff two `ProviderData` snapshots, producing a `Vec<Change>` covering all collections.
pub fn diff_provider_data(prev: &ProviderData, curr: &ProviderData) -> Vec<Change> {
    let mut changes = Vec::new();

    for (key, op) in diff_indexmap(&prev.checkouts, &curr.checkouts) {
        changes.push(Change::Checkout { key, op });
    }
    for (key, op) in diff_indexmap(&prev.change_requests, &curr.change_requests) {
        changes.push(Change::ChangeRequest { key, op });
    }
    for (key, op) in diff_indexmap(&prev.issues, &curr.issues) {
        changes.push(Change::Issue { key, op });
    }
    for (key, op) in diff_indexmap(&prev.sessions, &curr.sessions) {
        changes.push(Change::Session { key, op });
    }
    for (key, op) in diff_indexmap(&prev.workspaces, &curr.workspaces) {
        changes.push(Change::Workspace { key, op });
    }
    for (key, op) in diff_indexmap(&prev.attachable_sets, &curr.attachable_sets) {
        changes.push(Change::AttachableSet { key, op });
    }
    for (key, op) in diff_indexmap(&prev.branches, &curr.branches) {
        changes.push(Change::Branch { key, op });
    }
    for (key, op) in diff_indexmap(&prev.managed_terminals, &curr.managed_terminals) {
        changes.push(Change::ManagedTerminal { key, op });
    }

    changes
}

/// Apply an `EntryOp` to an IndexMap entry.
fn apply_op<K, V>(map: &mut IndexMap<K, V>, key: K, op: EntryOp<V>)
where
    K: Eq + Hash,
{
    match op {
        EntryOp::Added(v) | EntryOp::Updated(v) => {
            map.insert(key, v);
        }
        EntryOp::Removed => {
            map.shift_remove(&key);
        }
    }
}

/// Apply a list of `Change`s to a `ProviderData` snapshot, mutating it in place.
pub fn apply_changes(pd: &mut ProviderData, changes: Vec<Change>) {
    for change in changes {
        match change {
            Change::Checkout { key, op } => apply_op(&mut pd.checkouts, key, op),
            Change::ChangeRequest { key, op } => apply_op(&mut pd.change_requests, key, op),
            Change::Issue { key, op } => apply_op(&mut pd.issues, key, op),
            Change::Session { key, op } => apply_op(&mut pd.sessions, key, op),
            Change::Workspace { key, op } => apply_op(&mut pd.workspaces, key, op),
            Change::AttachableSet { key, op } => apply_op(&mut pd.attachable_sets, key, op),
            Change::Branch { key, op } => apply_op(&mut pd.branches, key, op),
            Change::ManagedTerminal { key, op } => apply_op(&mut pd.managed_terminals, key, op),
            // WorkItem and ProviderHealth are snapshot-level, not ProviderData-level.
            // They'll be handled at a higher layer.
            Change::WorkItem { .. } | Change::ProviderHealth { .. } | Change::ErrorsChanged(_) => {}
        }
    }
}

/// Compare two error lists — if different, return `Change::ErrorsChanged` with the new errors.
///
/// Errors lack stable identity, so this is a full replacement rather than keyed diffing.
pub fn diff_errors(prev: &[ProviderError], curr: &[ProviderError]) -> Option<Change> {
    if prev == curr {
        None
    } else {
        Some(Change::ErrorsChanged(curr.to_vec()))
    }
}

/// Diff two work item lists, producing `Change::WorkItem` entries.
///
/// Work items are keyed by `WorkItemIdentity`. The input slices are converted
/// to IndexMaps internally for O(n) diffing.
///
/// Not currently called in production — kept for the client-side materialization
/// path (delta replay on reconnect) planned in later PRs.
pub fn diff_work_items(prev: &[WorkItem], curr: &[WorkItem]) -> Vec<Change> {
    let prev_map: IndexMap<WorkItemIdentity, WorkItem> = prev.iter().map(|wi| (wi.identity.clone(), wi.clone())).collect();
    let curr_map: IndexMap<WorkItemIdentity, WorkItem> = curr.iter().map(|wi| (wi.identity.clone(), wi.clone())).collect();
    diff_indexmap(&prev_map, &curr_map).into_iter().map(|(identity, op)| Change::WorkItem { identity, op }).collect()
}

/// Apply `Change::WorkItem` operations to a work-item vector in place.
pub fn apply_work_item_changes(work_items: &mut Vec<WorkItem>, changes: &[Change]) {
    let mut by_identity: IndexMap<WorkItemIdentity, WorkItem> =
        work_items.iter().map(|item| (item.identity.clone(), item.clone())).collect();

    for change in changes {
        if let Change::WorkItem { identity, op } = change {
            match op {
                EntryOp::Added(item) | EntryOp::Updated(item) => {
                    by_identity.insert(identity.clone(), item.clone());
                }
                EntryOp::Removed => {
                    by_identity.shift_remove(identity);
                }
            }
        }
    }

    *work_items = by_identity.into_values().collect();
}

#[cfg(test)]
mod tests;
