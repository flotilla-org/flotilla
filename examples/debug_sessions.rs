//! Debug example: exercises the full session + correlation pipeline.
//!
//! Run with: cargo run --example debug_sessions -- --repo-root ~/dev/reticulate

use std::{path::PathBuf, sync::Arc, time::Duration};

use flotilla_core::{
    attachable::shared_file_backed_attachable_store,
    config::ConfigStore,
    convert::correlation_result_to_work_item,
    data,
    providers::{
        discovery::{self, detectors, FactoryRegistry, ProcessEnvVars},
        types::RepoCriteria,
        CommandRunner, ProcessCommandRunner,
    },
    refresh::RepoRefreshHandle,
};

#[tokio::main]
async fn main() {
    let repo_root = std::env::args().skip_while(|a| a != "--repo-root").nth(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));

    println!("Repo root: {}", repo_root.display());

    // Step 1: Build registry (same as app startup)
    println!("\n=== Step 1: Build ProviderRegistry ===");
    let config = ConfigStore::new();
    let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);

    let host_dets = detectors::default_host_detectors();
    let repo_dets = detectors::default_repo_detectors();
    let host_bag = discovery::run_host_detectors(&host_dets, &*runner, &ProcessEnvVars).await;
    let factories = FactoryRegistry::default_all();
    let attachable_store = shared_file_backed_attachable_store(config.base_path());

    let result = discovery::discover_providers(
        &host_bag,
        &repo_root,
        &repo_dets,
        &factories,
        &config,
        Arc::clone(&runner),
        Arc::clone(&attachable_store),
        &ProcessEnvVars,
    )
    .await;
    let registry = result.registry;
    let repo_slug = result.repo_slug;
    println!("  checkout_managers: {}", registry.checkout_managers.len());
    println!("  change_requests: {}", registry.change_requests.len());
    println!("  issue_trackers: {}", registry.issue_trackers.len());
    println!("  cloud_agents: {}", registry.cloud_agents.len());
    println!("  vcs: {}", registry.vcs.len());
    println!("  workspace_managers: {}", !registry.workspace_managers.is_empty());

    // Step 2: Spawn background refresh and wait for first snapshot
    println!("\n=== Step 2: Background refresh ===");
    let criteria = RepoCriteria { repo_slug };
    println!("  repo_criteria: {:?}", criteria);

    let registry = Arc::new(registry);
    let agent_state_store = flotilla_core::agents::shared_in_memory_agent_state_store();
    let handle =
        RepoRefreshHandle::spawn(repo_root.clone(), registry, criteria, attachable_store, agent_state_store, Duration::from_secs(60));

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
        println!("    [{i}] branch={:?} keys={:?}", co.branch, co.correlation_keys);
    }

    println!("\n  Change Requests: {}", snapshot.providers.change_requests.len());
    for (i, (_id, cr)) in snapshot.providers.change_requests.iter().enumerate() {
        println!(
            "    [{i}] title={:?} branch={:?} corr_keys={:?} assoc_keys={:?}",
            cr.title, cr.branch, cr.correlation_keys, cr.association_keys
        );
    }

    println!("\n  Sessions: {}", snapshot.providers.sessions.len());
    for (i, (_id, s)) in snapshot.providers.sessions.iter().enumerate() {
        println!("    [{i}] title={:?} status={:?} keys={:?}", s.title, s.status, s.correlation_keys);
    }

    println!("\n  Workspaces: {}", snapshot.providers.workspaces.len());
    for (i, (_ref, ws)) in snapshot.providers.workspaces.iter().enumerate() {
        println!("    [{i}] name={:?} dirs={:?} keys={:?}", ws.name, ws.directories, ws.correlation_keys);
    }

    // Step 3: Show resulting table entries
    println!("\n=== Step 3: Table entries after correlate() ===");
    let section_labels = data::SectionLabels::default();
    let work_items: Vec<_> = snapshot
        .work_items
        .iter()
        .map(|item| correlation_result_to_work_item(item, &snapshot.correlation_groups, &flotilla_core::HostName::local()))
        .collect();
    let table_view = data::group_work_items(&work_items, &snapshot.providers, &section_labels, &repo_root);
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
