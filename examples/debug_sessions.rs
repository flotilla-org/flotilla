//! Debug example: exercises the full session + correlation pipeline.
//!
//! Run with: cargo run --example debug_sessions -- --repo-root ~/dev/reticulate

use flotilla::providers::discovery::{detect_providers, first_remote_url, extract_repo_slug};
use flotilla::providers::types::RepoCriteria;
use flotilla::data::DataStore;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let repo_root = std::env::args()
        .skip_while(|a| a != "--repo-root")
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    println!("Repo root: {}", repo_root.display());

    // Step 1: Build registry (same as app startup)
    println!("\n=== Step 1: Build ProviderRegistry ===");
    let registry = detect_providers(&repo_root);
    println!("  checkout_managers: {}", registry.checkout_managers.len());
    println!("  code_review: {}", registry.code_review.len());
    println!("  issue_trackers: {}", registry.issue_trackers.len());
    println!("  coding_agents: {}", registry.coding_agents.len());
    println!("  vcs: {}", registry.vcs.len());
    println!("  workspace_manager: {}", registry.workspace_manager.is_some());

    // Step 2: Refresh data
    println!("\n=== Step 2: DataStore::refresh() ===");
    let repo_slug = first_remote_url(&repo_root).and_then(|u| extract_repo_slug(&u));
    let criteria = RepoCriteria { repo_slug };
    println!("  repo_criteria: {:?}", criteria);
    let mut ds = DataStore::default();
    let errors = ds.refresh(&repo_root, &registry, &criteria).await;
    if !errors.is_empty() {
        println!("  ERRORS:");
        for e in &errors {
            println!("    - {e}");
        }
    }

    println!("\n  Checkouts: {}", ds.providers.checkouts.len());
    for (i, co) in ds.providers.checkouts.iter().enumerate() {
        println!("    [{i}] branch={:?} keys={:?}", co.branch, co.correlation_keys);
    }

    println!("\n  Change Requests: {}", ds.providers.change_requests.len());
    for (i, cr) in ds.providers.change_requests.iter().enumerate() {
        println!("    [{i}] title={:?} branch={:?} corr_keys={:?} assoc_keys={:?}",
            cr.title, cr.branch, cr.correlation_keys, cr.association_keys);
    }

    println!("\n  Sessions: {}", ds.providers.sessions.len());
    for (i, s) in ds.providers.sessions.iter().enumerate() {
        println!("    [{i}] title={:?} status={:?} keys={:?}",
            s.title, s.status, s.correlation_keys);
    }

    println!("\n  Workspaces: {}", ds.providers.workspaces.len());
    for (i, ws) in ds.providers.workspaces.iter().enumerate() {
        println!("    [{i}] name={:?} dirs={:?} keys={:?}",
            ws.name, ws.directories, ws.correlation_keys);
    }

    // Step 3: Show resulting table entries
    println!("\n=== Step 3: Table entries after correlate() ===");
    for (i, entry) in ds.table_entries.iter().enumerate() {
        match entry {
            flotilla::data::TableEntry::Header(h) => {
                println!("  [{i}] HEADER: {h}");
            }
            flotilla::data::TableEntry::Item(item) => {
                println!("  [{i}] {:?} desc={:?} branch={:?} wt={:?} pr={:?} ses={:?} ws={:?}",
                    item.kind, item.description, item.branch,
                    item.worktree_idx, item.pr_idx, item.session_idx, item.workspace_refs);
            }
        }
    }
}
