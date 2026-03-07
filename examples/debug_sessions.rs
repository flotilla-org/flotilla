//! Debug example: exercises the full session + correlation pipeline.
//!
//! Run with: cargo run --example debug_sessions -- --repo-root ~/dev/reticulate

use flotilla_core::convert::correlation_result_to_work_item;
use flotilla_core::data;
use flotilla_core::providers::discovery::detect_providers;
use flotilla_core::providers::types::RepoCriteria;
use flotilla_core::refresh::RepoRefreshHandle;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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
    let (registry, repo_slug) = detect_providers(&repo_root).await;
    println!("  checkout_managers: {}", registry.checkout_managers.len());
    println!("  code_review: {}", registry.code_review.len());
    println!("  issue_trackers: {}", registry.issue_trackers.len());
    println!("  coding_agents: {}", registry.coding_agents.len());
    println!("  vcs: {}", registry.vcs.len());
    println!(
        "  workspace_manager: {}",
        registry.workspace_manager.is_some()
    );

    // Step 2: Spawn background refresh and wait for first snapshot
    println!("\n=== Step 2: Background refresh ===");
    let criteria = RepoCriteria { repo_slug };
    println!("  repo_criteria: {:?}", criteria);

    let registry = Arc::new(registry);
    let handle = RepoRefreshHandle::spawn(
        repo_root.clone(),
        registry,
        criteria,
        Duration::from_secs(60),
    );

    // Wait for the first snapshot
    let mut rx = handle.snapshot_rx.clone();
    rx.changed().await.expect("refresh task stopped");
    let snapshot = rx.borrow().clone();

    if !snapshot.errors.is_empty() {
        println!("  ERRORS:");
        for e in &snapshot.errors {
            println!("    - {e}");
        }
    }

    println!("\n  Checkouts: {}", snapshot.providers.checkouts.len());
    for (i, (_path, co)) in snapshot.providers.checkouts.iter().enumerate() {
        println!(
            "    [{i}] branch={:?} keys={:?}",
            co.branch, co.correlation_keys
        );
    }

    println!(
        "\n  Change Requests: {}",
        snapshot.providers.change_requests.len()
    );
    for (i, (_id, cr)) in snapshot.providers.change_requests.iter().enumerate() {
        println!(
            "    [{i}] title={:?} branch={:?} corr_keys={:?} assoc_keys={:?}",
            cr.title, cr.branch, cr.correlation_keys, cr.association_keys
        );
    }

    println!("\n  Sessions: {}", snapshot.providers.sessions.len());
    for (i, (_id, s)) in snapshot.providers.sessions.iter().enumerate() {
        println!(
            "    [{i}] title={:?} status={:?} keys={:?}",
            s.title, s.status, s.correlation_keys
        );
    }

    println!("\n  Workspaces: {}", snapshot.providers.workspaces.len());
    for (i, (_ref, ws)) in snapshot.providers.workspaces.iter().enumerate() {
        println!(
            "    [{i}] name={:?} dirs={:?} keys={:?}",
            ws.name, ws.directories, ws.correlation_keys
        );
    }

    // Step 3: Show resulting table entries
    println!("\n=== Step 3: Table entries after correlate() ===");
    let section_labels = data::SectionLabels::default();
    let work_items: Vec<_> = snapshot
        .work_items
        .iter()
        .map(|item| correlation_result_to_work_item(item, &snapshot.correlation_groups))
        .collect();
    let table_view = data::group_work_items(&work_items, &snapshot.providers, &section_labels);
    for (i, entry) in table_view.table_entries.iter().enumerate() {
        match entry {
            data::GroupEntry::Header(h) => {
                println!("  [{i}] HEADER: {h}");
            }
            data::GroupEntry::Item(item) => {
                println!(
                    "  [{i}] {:?} desc={:?} branch={:?} co={:?} pr={:?} ses={:?} ws={:?}",
                    item.kind,
                    item.description,
                    item.branch,
                    item.checkout_key(),
                    item.change_request_key,
                    item.session_key,
                    item.workspace_refs
                );
            }
        }
    }
}
