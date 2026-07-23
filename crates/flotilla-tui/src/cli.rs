use std::{collections::HashMap, path::Path};

use comfy_table::{presets::UTF8_FULL_CONDENSED, Cell, Table};
use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{
    output::OutputFormat, Command, CommandValue, CrewListResponse, DaemonEvent, EnvironmentInfo, EnvironmentStatus, FleetListResponse,
    FleetStaleness, HostProvidersResponse, HostStatusResponse, NodeInfo, PeerConnectionState, ProjectListResponse, RepoDetailResponse,
    RepoProvidersResponse, RepoWorkResponse, StatusResponse, StreamKey, TopologyResponse,
};

use crate::socket::SocketDaemon;

fn format_work_items_table(items: &[flotilla_protocol::snapshot::WorkItem]) -> Table {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Kind", "Branch", "Description", "PR", "Session", "Issues"]);
    for item in items {
        table.add_row(vec![
            Cell::new(format!("{:?}", item.kind)),
            Cell::new(item.branch.as_deref().unwrap_or("-")),
            Cell::new(&item.description),
            Cell::new(item.change_request_key.as_deref().unwrap_or("-")),
            Cell::new(item.session_key.as_deref().unwrap_or("-")),
            Cell::new(if item.issue_keys.is_empty() { "-".into() } else { item.issue_keys.join(", ") }),
        ]);
    }
    table
}

fn format_status_response_human(status: &StatusResponse) -> String {
    if status.repos.is_empty() {
        return "No repos tracked.\n".into();
    }
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Repo", "Path", "Work Items", "Errors", "Health", "Unavailable"]);
    for repo in &status.repos {
        let name = repo.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut health: Vec<String> = repo
            .provider_health
            .iter()
            .flat_map(|(cat, providers)| {
                providers.iter().map(move |(name, ok)| format!("{cat}/{name}: {}", if *ok { "ok" } else { "error" }))
            })
            .collect();
        health.sort();
        let health_str = if health.is_empty() { "-".into() } else { health.join(", ") };
        let unavailable = repo
            .unmet_requirements
            .iter()
            .map(|requirement| match &requirement.value {
                Some(value) => format!("{}: {value}", requirement.factory),
                None => format!("{}: {}", requirement.factory, requirement.kind),
            })
            .collect::<Vec<_>>()
            .join(", ");
        table.add_row(vec![
            Cell::new(&name),
            Cell::new(repo.path.display()),
            Cell::new(repo.work_item_count),
            Cell::new(repo.error_count),
            Cell::new(&health_str),
            Cell::new(if unavailable.is_empty() { "-" } else { &unavailable }),
        ]);
    }
    format!("{table}\n")
}

fn format_connection_status(status: &PeerConnectionState) -> &'static str {
    match status {
        PeerConnectionState::Connected => "connected",
        PeerConnectionState::Disconnected => "disconnected",
        PeerConnectionState::Connecting => "connecting",
        PeerConnectionState::Reconnecting => "reconnecting",
        PeerConnectionState::Rejected { .. } => "rejected",
    }
}

fn inventory_is_empty(inventory: &flotilla_protocol::ToolInventory) -> bool {
    inventory.binaries.is_empty() && inventory.sockets.is_empty() && inventory.auth.is_empty() && inventory.env_vars.is_empty()
}

fn environment_status_label(status: &EnvironmentStatus) -> String {
    match status {
        EnvironmentStatus::Building => "building".to_string(),
        EnvironmentStatus::Starting => "starting".to_string(),
        EnvironmentStatus::Running => "running".to_string(),
        EnvironmentStatus::Stopped => "stopped".to_string(),
        EnvironmentStatus::Failed(message) => format!("failed: {message}"),
    }
}

fn format_visible_environments_human(environments: &[EnvironmentInfo]) -> String {
    if environments.is_empty() {
        return String::new();
    }

    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Kind", "Id", "Display Name", "Status", "Image"]);
    for environment in environments {
        match environment {
            EnvironmentInfo::Direct { id, display_name, status, .. } => {
                table.add_row(vec![
                    Cell::new("direct"),
                    Cell::new(id.as_str()),
                    Cell::new(display_name.as_deref().unwrap_or("-")),
                    Cell::new(environment_status_label(status)),
                    Cell::new("-"),
                ]);
            }
            EnvironmentInfo::Provisioned { id, display_name, image, status } => {
                table.add_row(vec![
                    Cell::new("provisioned"),
                    Cell::new(id.as_str()),
                    Cell::new(display_name.as_deref().unwrap_or("-")),
                    Cell::new(environment_status_label(status)),
                    Cell::new(image.as_str()),
                ]);
            }
        }
    }
    format!("Visible Environments:\n{table}\n")
}

fn node_label(node: &NodeInfo) -> &str {
    &node.display_name
}

fn format_host_list_human(response: &flotilla_protocol::HostListResponse) -> String {
    if response.hosts.is_empty() {
        return "No hosts known.\n".into();
    }

    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Host", "Node", "Local", "Configured", "Status", "Summary", "Repos", "Work"]);
    for host in &response.hosts {
        table.add_row(vec![
            Cell::new(host.host_name.as_str()),
            Cell::new(node_label(&host.node)),
            Cell::new(if host.is_local { "yes" } else { "no" }),
            Cell::new(if host.configured { "yes" } else { "no" }),
            Cell::new(format_connection_status(&host.connection_status)),
            Cell::new(if host.has_summary { "yes" } else { "no" }),
            Cell::new(host.repo_count),
            Cell::new(host.work_item_count),
        ]);
    }
    format!("{table}\n")
}

fn format_project_list_human(response: &ProjectListResponse) -> String {
    if response.projects.is_empty() {
        return "No projects known.\n".into();
    }

    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Project", "Display Name", "Repositories", "Issue Source", "Workflow", "Address"]);
    for project in &response.projects {
        let repository_count = project.repositories.len();
        let repositories = if repository_count <= 3 {
            project
                .repositories
                .iter()
                .map(|repository| repository.slug.as_deref().unwrap_or(&repository.key.0))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            format!("{repository_count} repositories")
        };
        let issue_source = project
            .issue_source
            .as_ref()
            .map(|source| format!("{} / {}", source.service.trim_end_matches('/'), source.scope))
            .unwrap_or_else(|| "-".to_string());
        table.add_row(vec![
            Cell::new(format!("{}/{}", project.namespace, project.name)),
            Cell::new(&project.display_name),
            Cell::new(repositories),
            Cell::new(issue_source),
            Cell::new(&project.default_workflow_ref),
            Cell::new(project.address.human_label()),
        ]);
    }
    format!("{table}\n")
}

fn format_host_status_human(response: &HostStatusResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Host: {}\n", response.host_name));
    out.push_str(&format!("Node: {}\n", node_label(&response.node)));
    out.push_str(&format!("Status: {}\n", format_connection_status(&response.connection_status)));
    out.push_str(&format!("Configured: {}\n", if response.configured { "yes" } else { "no" }));
    out.push_str(&format!("Repositories: {}\n", response.repo_count));
    out.push_str(&format!("Work Items: {}\n", response.work_item_count));

    if let Some(summary) = &response.summary {
        out.push_str("\nSystem:\n");
        if let Some(os) = &summary.system.os {
            out.push_str(&format!("  OS: {os}\n"));
        }
        if let Some(arch) = &summary.system.arch {
            out.push_str(&format!("  Arch: {arch}\n"));
        }
        if let Some(cpus) = summary.system.cpu_count {
            out.push_str(&format!("  CPUs: {cpus}\n"));
        }
        if let Some(memory) = summary.system.memory_total_mb {
            out.push_str(&format!("  Memory: {} MB\n", memory));
        }
    }

    out.push_str(&format_visible_environments_human(&response.visible_environments));

    out
}

fn format_host_providers_human(response: &HostProvidersResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Host: {}\n", response.host_name));
    out.push_str(&format!("Node: {}\n", node_label(&response.node)));
    out.push_str(&format!("Status: {}\n", format_connection_status(&response.connection_status)));
    out.push_str(&format!("Configured: {}\n", if response.configured { "yes" } else { "no" }));

    out.push_str("\nInventory:\n");
    if inventory_is_empty(&response.summary.inventory) {
        out.push_str("  No inventory facts.\n");
    } else {
        for fact in &response.summary.inventory.binaries {
            out.push_str(&format!("  binary: {}\n", fact.name));
        }
        for fact in &response.summary.inventory.sockets {
            out.push_str(&format!("  socket: {}\n", fact.name));
        }
        for fact in &response.summary.inventory.auth {
            out.push_str(&format!("  auth: {}\n", fact.name));
        }
        for fact in &response.summary.inventory.env_vars {
            out.push_str(&format!("  env: {}\n", fact.name));
        }
    }

    out.push_str("\nProviders:\n");
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Category", "Name", "Health"]);
    for provider in &response.summary.providers {
        table.add_row(vec![
            Cell::new(&provider.category),
            Cell::new(&provider.name),
            Cell::new(if provider.healthy { "ok" } else { "error" }),
        ]);
    }
    out.push_str(&table.to_string());
    out.push('\n');
    out.push_str(&format_visible_environments_human(&response.visible_environments));
    out
}

fn format_topology_human(response: &TopologyResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Local Node: {}\n", node_label(&response.local_node)));
    if response.routes.is_empty() {
        out.push_str("No routes.\n");
        return out;
    }

    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Target", "Via", "Direct", "Connected", "Last attempt", "Last error", "Fallbacks"]);
    for route in &response.routes {
        let fallbacks = if route.fallbacks.is_empty() {
            "-".to_string()
        } else {
            route.fallbacks.iter().map(node_label).collect::<Vec<_>>().join(", ")
        };
        table.add_row(vec![
            Cell::new(node_label(&route.target)),
            Cell::new(node_label(&route.next_hop)),
            Cell::new(if route.direct { "yes" } else { "no" }),
            Cell::new(if route.connected { "yes" } else { "no" }),
            Cell::new(route.last_attempt.map(|attempt| attempt.format("%H:%M:%S").to_string()).unwrap_or_else(|| "-".to_string())),
            Cell::new(route.last_error.as_deref().unwrap_or("-")),
            Cell::new(fallbacks),
        ]);
    }
    out.push_str(&table.to_string());
    out.push('\n');
    out
}

fn format_fleet_staleness(staleness: &FleetStaleness) -> String {
    match staleness {
        FleetStaleness::Local => "local".to_string(),
        FleetStaleness::Fresh { last_sync } => format!("fresh ({})", last_sync.format("%H:%M:%S")),
        FleetStaleness::Stale { last_sync } => format!("stale ({})", last_sync.format("%H:%M:%S")),
        FleetStaleness::Unreachable { last_sync, message } => match last_sync {
            Some(last_sync) => format!("unreachable ({}, {})", last_sync.format("%H:%M:%S"), message),
            None => format!("unreachable ({message})"),
        },
    }
}

fn format_fleet_list_human(response: &FleetListResponse) -> String {
    let mut out = String::new();
    if response.rows.is_empty() {
        out.push_str("No crew sessions found.\n");
    } else {
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec!["Convoy", "Vessel", "Crew", "State", "Host", "Staleness"]);
        for row in &response.rows {
            let vessel = match &row.authority {
                Some(authority) => format!("{} ({authority})", row.vessel),
                None => row.vessel.clone(),
            };
            table.add_row(vec![
                Cell::new(&row.convoy),
                Cell::new(vessel),
                Cell::new(&row.crew),
                Cell::new(&row.crew_state),
                Cell::new(row.host.as_str()),
                Cell::new(format_fleet_staleness(&row.staleness)),
            ]);
        }
        out.push_str(&table.to_string());
        out.push('\n');
    }

    if response.replicas.iter().any(|replica| !replica.reachable) {
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec!["Replica", "Status", "Last Sync", "Generation"]);
        for replica in &response.replicas {
            if replica.reachable {
                continue;
            }
            table.add_row(vec![
                Cell::new(replica.host.as_str()),
                Cell::new(replica.message.as_deref().unwrap_or("unreachable")),
                Cell::new(replica.last_sync.map(|ts| ts.to_rfc3339()).unwrap_or_else(|| "-".to_string())),
                Cell::new(replica.generation.as_deref().unwrap_or("-")),
            ]);
        }
        out.push_str("\nReplica status:\n");
        out.push_str(&table.to_string());
        out.push('\n');
    }

    out
}

fn format_crew_list_human(response: &CrewListResponse) -> String {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Role", "Kind", "State", "Adapter", "Model", "Stance"]);
    for member in &response.members {
        table.add_row(vec![
            Cell::new(&member.role),
            Cell::new(&member.kind),
            Cell::new(&member.state),
            Cell::new(member.adapter.as_deref().unwrap_or("-")),
            Cell::new(member.model.as_deref().unwrap_or("-")),
            Cell::new(member.stance.as_deref().unwrap_or("-")),
        ]);
    }
    format!("Convoy: {}  Vessel: {} ({})\n{}\n", response.convoy, response.vessel, response.vessel_ref, table)
}

/// Extract a short display name from a repo path (last path component).
/// Falls back to the full path display for root or non-UTF-8 paths,
/// matching `flotilla_core::model::repo_name`.
fn repo_name(path: &std::path::Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string_lossy().to_string())
}

fn repo_label(path: Option<&std::path::Path>, identity: &flotilla_protocol::RepoIdentity) -> String {
    path.map(repo_name).unwrap_or_else(|| identity.path.clone())
}

/// Format a `CommandValue` as a short human-readable string.
fn format_command_result(result: &flotilla_protocol::commands::CommandValue) -> String {
    use flotilla_protocol::commands::CommandValue;
    match result {
        CommandValue::Ok => "ok".to_string(),
        CommandValue::RepoTracked { path, resolved_from, identity_change } => {
            let mut output = match resolved_from {
                Some(original) => format!("repo tracked: {} (resolved from {})", path.display(), original.display()),
                None => format!("repo tracked: {}", path.display()),
            };
            if let Some(change) = identity_change {
                output.push_str(&format!("\nrepository identity changed: {} → {}", change.previous_display, change.current_display));
            }
            output
        }
        CommandValue::RepoUntracked { path } => format!("repo untracked: {}", path.display()),
        CommandValue::Refreshed { repos, identity_changes } => {
            let mut output = format!("refreshed {} repo(s)", repos.len());
            for change in identity_changes {
                output.push_str(&format!("\nrepository identity changed: {} → {}", change.previous_display, change.current_display));
            }
            output
        }
        CommandValue::CheckoutCreated { branch, .. } => format!("checkout created: {branch}"),
        CommandValue::CheckoutRemoved { branch } => format!("checkout removed: {branch}"),
        CommandValue::TerminalPrepared { branch, target_node_id, .. } => format!("terminal prepared: {branch} on {target_node_id}"),
        CommandValue::BranchNameGenerated { name, .. } => format!("branch name: {name}"),
        CommandValue::CheckoutStatus(status) => {
            let mut parts = vec![format!("checkout status: {}", status.branch)];
            if let Some(cr) = &status.change_request_status {
                parts.push(format!("PR: {cr}"));
            }
            if let Some(sha) = &status.merge_commit_sha {
                parts.push(format!("merged via {}", &sha[..sha.len().min(7)]));
            }
            if !status.unpushed_commits.is_empty() {
                parts.push(format!("{} unpushed", status.unpushed_commits.len()));
            }
            if status.has_uncommitted {
                parts.push("uncommitted changes".to_string());
            }
            if let Some(warning) = &status.base_detection_warning {
                parts.push(format!("warning: {warning}"));
            }
            parts.join(", ")
        }
        CommandValue::Error { message } => format!("error: {message}"),
        CommandValue::Cancelled => "cancelled".to_string(),
        CommandValue::PreparedWorkspace(_) | CommandValue::AttachCommandResolved { .. } | CommandValue::CheckoutPathResolved { .. } => {
            "internal step result".to_string()
        }
        CommandValue::RepoDetail(detail) => format_repo_detail_human(detail),
        CommandValue::RepoProviders(providers) => format_repo_providers_human(providers),
        CommandValue::RepoWork(work) => format_repo_work_human(work),
        CommandValue::HostList(hosts) => format_host_list_human(hosts),
        CommandValue::ProjectList(projects) => format_project_list_human(projects),
        CommandValue::HostStatus(status) => format_host_status_human(status),
        CommandValue::HostProviders(providers) => format_host_providers_human(providers),
        CommandValue::FleetList(fleet) => format_fleet_list_human(fleet),
        CommandValue::CrewList(crew) => format_crew_list_human(crew),
        CommandValue::FleetReplicaSnapshot(_) => "fleet replica snapshot".to_string(),
        CommandValue::ResourceList(response) | CommandValue::ResourceObject(response) => {
            flotilla_protocol::output::json_pretty(&response.value)
        }
        CommandValue::ResourceWatchEvent(response) => flotilla_protocol::output::json_pretty(&response.event),
        CommandValue::ImageEnsured { image } => format!("image ensured: {image}"),
        CommandValue::EnvironmentCreated { env_id } => format!("environment created: {env_id}"),
        CommandValue::EnvironmentSpecRead { .. } => "environment spec read".to_string(),
        CommandValue::IssuePage(page) => format!("issue page: {} items, has_more={}", page.items.len(), page.has_more),
        CommandValue::IssuesByIds { items } => format!("issues by ids: {} items", items.len()),
        CommandValue::ConvoyCreated { name } => format!("convoy created: {name}"),
        CommandValue::ConvoyStarted { name, attach_command, .. } => {
            format!("convoy started: {name}{}", if attach_command.is_some() { " (crew ready)" } else { "" })
        }
        CommandValue::WorkflowTemplateApplied { name } => format!("workflow template applied: {name}"),
        CommandValue::ProjectAdded { name } => format!("project added: {name}"),
        CommandValue::ProjectApplied { name } => format!("project applied: {name}"),
    }
}

pub(crate) fn format_event_human(event: &flotilla_protocol::DaemonEvent) -> String {
    use flotilla_protocol::{DaemonEvent, PeerConnectionState};
    match event {
        DaemonEvent::RepoSnapshot(snap) => {
            format!(
                "[snapshot] {}: full snapshot (seq {}, {} work items)",
                repo_label(snap.repo.as_deref(), &snap.repo_identity),
                snap.seq,
                snap.work_items.len()
            )
        }
        DaemonEvent::RepoDelta(delta) => {
            format!(
                "[delta]    {}: delta seq {}\u{2192}{} ({} changes)",
                repo_label(delta.repo.as_deref(), &delta.repo_identity),
                delta.prev_seq,
                delta.seq,
                delta.changes.len()
            )
        }
        DaemonEvent::RepoRefreshCompleted { repo_identity, repo } => {
            format!("[refresh]  {}: completed", repo_label(repo.as_deref(), repo_identity))
        }
        DaemonEvent::RepoTracked(info) => {
            format!("[repo]     {}: tracked", info.name)
        }
        DaemonEvent::RepoUntracked { repo_identity, path } => {
            format!("[repo]     {}: untracked", repo_label(path.as_deref(), repo_identity))
        }
        DaemonEvent::CommandStarted { repo_identity, repo, description, .. } => {
            if repo.is_none() && repo_identity.authority.is_empty() && repo_identity.path.is_empty() {
                // Query commands have no repo context — show description only
                format!("[query]    {description}")
            } else {
                format!("[command]  {}: started \"{}\"", repo_label(repo.as_deref(), repo_identity), description)
            }
        }
        DaemonEvent::CommandFinished { repo_identity, repo, result, .. } => {
            if repo.is_none() && repo_identity.authority.is_empty() && repo_identity.path.is_empty() {
                // Query commands have no repo context — show result directly
                format_command_result(result)
            } else {
                format!("[command]  {}: finished \u{2192} {}", repo_label(repo.as_deref(), repo_identity), format_command_result(result))
            }
        }
        DaemonEvent::CommandStepUpdate { repo_identity, repo, description, step_index, step_count, .. } => {
            format!("[step]     {}: {} ({}/{})", repo_label(repo.as_deref(), repo_identity), description, step_index + 1, step_count)
        }
        DaemonEvent::PeerStatusChanged { node_id, status } => {
            let state = match status {
                PeerConnectionState::Connected => "connected".to_string(),
                PeerConnectionState::Disconnected => "disconnected".to_string(),
                PeerConnectionState::Connecting => "connecting".to_string(),
                PeerConnectionState::Reconnecting => "reconnecting".to_string(),
                PeerConnectionState::Rejected { reason } => format!("rejected: {reason}"),
            };
            format!("[peer]     {node_id}: {state}")
        }
        DaemonEvent::HostSnapshot(snap) => {
            let state = match &snap.connection_status {
                PeerConnectionState::Connected => "connected",
                PeerConnectionState::Disconnected => "disconnected",
                PeerConnectionState::Connecting => "connecting",
                PeerConnectionState::Reconnecting => "reconnecting",
                PeerConnectionState::Rejected { .. } => "rejected",
            };
            format!("[host]     {}: {} (seq {})", node_label(&snap.node), state, snap.seq)
        }
        DaemonEvent::HostRemoved { environment_id, seq } => {
            format!("[host]     {environment_id}: removed (seq {seq})")
        }
        DaemonEvent::ResultSet(result_set) => {
            format!("[query]     {}: full result set (seq {}, {} rows)", result_set.query(), result_set.seq, result_set.rows.len())
        }
        DaemonEvent::ResultDelta(delta) => {
            format!(
                "[query]     {}: delta (seq {}, {} changed, {} removed)",
                delta.query(),
                delta.seq,
                delta.changes.changed_len(),
                delta.changes.removed_len()
            )
        }
    }
}

/// Extract the (stream_key, seq) from a snapshot/delta event, if present.
fn event_stream_seq(event: &DaemonEvent) -> Option<(StreamKey, u64)> {
    match event {
        DaemonEvent::RepoSnapshot(snap) => Some((StreamKey::Repo { identity: snap.repo_identity.clone() }, snap.seq)),
        DaemonEvent::RepoDelta(delta) => Some((StreamKey::Repo { identity: delta.repo_identity.clone() }, delta.seq)),
        DaemonEvent::HostSnapshot(snap) => Some((StreamKey::Host { environment_id: snap.environment_id.clone() }, snap.seq)),
        DaemonEvent::HostRemoved { environment_id, seq } => Some((StreamKey::Host { environment_id: environment_id.clone() }, *seq)),
        DaemonEvent::ResultSet(result_set) => Some((StreamKey::Query { query: result_set.query() }, result_set.seq)),
        DaemonEvent::ResultDelta(delta) => Some((StreamKey::Query { query: delta.query() }, delta.seq)),
        DaemonEvent::RepoTracked(_)
        | DaemonEvent::RepoRefreshCompleted { .. }
        | DaemonEvent::RepoUntracked { .. }
        | DaemonEvent::CommandStarted { .. }
        | DaemonEvent::CommandFinished { .. }
        | DaemonEvent::CommandStepUpdate { .. }
        | DaemonEvent::PeerStatusChanged { .. } => None,
    }
}

pub async fn run_status(socket_path: &Path, format: OutputFormat) -> Result<(), String> {
    let daemon = SocketDaemon::connect(socket_path).await.map_err(|e| format!("cannot connect to daemon: {e}"))?;
    let status = daemon.get_status().await?;
    let output = match format {
        OutputFormat::Human => format_status_response_human(&status),
        OutputFormat::Json => flotilla_protocol::output::json_pretty(&status),
    };
    print!("{output}");
    Ok(())
}

pub async fn run_topology(daemon: &dyn DaemonHandle, format: OutputFormat) -> Result<(), String> {
    let topology = daemon.get_topology().await?;
    let output = match format {
        OutputFormat::Human => format_topology_human(&topology),
        OutputFormat::Json => flotilla_protocol::output::json_pretty(&topology),
    };
    print!("{output}");
    Ok(())
}

fn format_repo_detail_human(detail: &RepoDetailResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Repo: {}\n", detail.path.display()));
    if let Some(slug) = &detail.slug {
        out.push_str(&format!("Slug: {slug}\n"));
    }
    out.push('\n');

    if !detail.work_items.is_empty() {
        let table = format_work_items_table(&detail.work_items);
        out.push_str(&table.to_string());
        out.push('\n');
    }

    if !detail.errors.is_empty() {
        out.push_str("\nErrors:\n");
        for err in &detail.errors {
            out.push_str(&format!("  [{}/{}] {}\n", err.category, err.provider, err.message));
        }
    }
    out
}

fn format_repo_providers_human(resp: &RepoProvidersResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Repo: {}\n", resp.path.display()));
    if let Some(slug) = &resp.slug {
        out.push_str(&format!("Slug: {slug}\n"));
    }

    if !resp.host_discovery.is_empty() {
        out.push_str("\nHost Discovery:\n");
        for entry in &resp.host_discovery {
            let mut details: Vec<String> = entry.detail.iter().map(|(k, v)| format!("{k}={v}")).collect();
            details.sort();
            out.push_str(&format!("  {} ({})\n", entry.kind, details.join(", ")));
        }
    }

    if !resp.repo_discovery.is_empty() {
        out.push_str("\nRepo Discovery:\n");
        for entry in &resp.repo_discovery {
            let mut details: Vec<String> = entry.detail.iter().map(|(k, v)| format!("{k}={v}")).collect();
            details.sort();
            out.push_str(&format!("  {} ({})\n", entry.kind, details.join(", ")));
        }
    }

    if !resp.providers.is_empty() {
        out.push_str("\nProviders:\n");
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec!["Category", "Name", "Health"]);
        for p in &resp.providers {
            table.add_row(vec![Cell::new(&p.category), Cell::new(&p.name), Cell::new(if p.healthy { "ok" } else { "error" })]);
        }
        out.push_str(&table.to_string());
        out.push('\n');
    }

    if !resp.unmet_requirements.is_empty() {
        out.push_str("\nUnmet Requirements:\n");
        for ur in &resp.unmet_requirements {
            match &ur.value {
                Some(value) => out.push_str(&format!("  {}: {} ({value})\n", ur.factory, ur.kind)),
                None => out.push_str(&format!("  {}: {}\n", ur.factory, ur.kind)),
            }
        }
    }
    out
}

fn format_repo_work_human(resp: &RepoWorkResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Repo: {}\n", resp.path.display()));
    if let Some(slug) = &resp.slug {
        out.push_str(&format!("Slug: {slug}\n"));
    }
    out.push('\n');

    if resp.work_items.is_empty() {
        out.push_str("No work items.\n");
    } else {
        let table = format_work_items_table(&resp.work_items);
        out.push_str(&table.to_string());
        out.push('\n');
    }
    out
}

/// Print a batch of bootstrap events and record each stream's highest seq so
/// the live loop can suppress duplicates the broadcast buffer also delivers.
fn print_bootstrap_events(events: &[DaemonEvent], replay_seqs: &mut HashMap<StreamKey, u64>, format: OutputFormat) {
    for event in events {
        if let Some((stream_key, seq)) = event_stream_seq(event) {
            replay_seqs.entry(stream_key).and_modify(|s| *s = (*s).max(seq)).or_insert(seq);
        }
        let line = match format {
            OutputFormat::Human => format_event_human(event),
            OutputFormat::Json => flotilla_protocol::output::json_line(event),
        };
        println!("{line}");
    }
}

pub async fn run_watch(socket_path: &Path, format: OutputFormat) -> Result<(), String> {
    let mut backoff = crate::pm_connect::ReconnectBackoff::default();
    loop {
        match SocketDaemon::connect(socket_path).await {
            Ok(daemon) => {
                backoff.reset();
                if let Err(error) = run_watch_connection(daemon, format).await {
                    eprintln!("{error}; reconnecting...");
                }
            }
            Err(error) if crate::pm_connect::is_incompatible_daemon_error(&error) => return Err(error),
            Err(error) => eprintln!("cannot connect to daemon: {error}; retrying..."),
        }

        tokio::time::sleep(backoff.next_delay()).await;
    }
}

async fn run_watch_connection(daemon: std::sync::Arc<dyn DaemonHandle>, format: OutputFormat) -> Result<(), String> {
    // Subscribe before replay so events emitted between replay and the loop
    // are buffered rather than silently dropped.
    let mut rx = daemon.subscribe();

    // Replay current state so the user sees an initial snapshot for every
    // tracked repo, matching how the TUI bootstraps.
    let mut replay_seqs: HashMap<StreamKey, u64> = HashMap::new();
    match daemon.replay_since(&HashMap::new()).await {
        Ok(events) => print_bootstrap_events(&events, &mut replay_seqs, format),
        Err(e) => {
            eprintln!("warning: failed to replay initial state: {e}");
        }
    }

    // Subscribe to every named query so watch shows the full data plane.
    let cursors: Vec<flotilla_protocol::QueryCursor> = flotilla_protocol::QueryId::ALWAYS_MATERIALIZED
        .iter()
        .cloned()
        .map(|query| flotilla_protocol::QueryCursor { query, since: None })
        .collect();
    let subscriber_id = uuid::Uuid::new_v4();
    match daemon.subscribe_queries(subscriber_id, &cursors).await {
        Ok(events) => print_bootstrap_events(&events, &mut replay_seqs, format),
        Err(e) => {
            eprintln!("warning: failed to subscribe to queries: {e}");
        }
    }

    if matches!(format, OutputFormat::Human) {
        eprintln!("watching events (Ctrl-C to stop)...");
    }

    loop {
        match rx.recv().await {
            Ok(event) => {
                // Skip events already covered by replay to avoid duplicates.
                if let Some((stream_key, seq)) = event_stream_seq(&event) {
                    if let Some(&replay_seq) = replay_seqs.get(&stream_key) {
                        if seq <= replay_seq {
                            continue;
                        }
                    }
                }
                let line = match format {
                    OutputFormat::Human => format_event_human(&event),
                    OutputFormat::Json => flotilla_protocol::output::json_line(&event),
                };
                println!("{line}");
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("warning: skipped {n} events");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("daemon disconnected".to_string()),
        }
    }
}

pub async fn run_command(daemon: &dyn DaemonHandle, command: Command, format: OutputFormat) -> Result<CommandValue, String> {
    if command.action.is_query() {
        return run_query_command(daemon, command, format).await;
    }

    let mut rx = daemon.subscribe();
    let command_id = daemon.execute(command).await?;

    loop {
        match rx.recv().await {
            Ok(ref event @ DaemonEvent::CommandStarted { command_id: id, .. }) if id == command_id => {
                if matches!(format, OutputFormat::Human) {
                    println!("{}", format_event_human(event));
                }
            }
            Ok(event @ DaemonEvent::CommandStepUpdate { command_id: id, .. }) if id == command_id => {
                if matches!(format, OutputFormat::Human) {
                    println!("{}", format_event_human(&event));
                }
            }
            Ok(ref event @ DaemonEvent::CommandFinished { command_id: id, ref result, .. }) if id == command_id => {
                match format {
                    OutputFormat::Human => {
                        println!("{}", format_event_human(event));
                    }
                    OutputFormat::Json => {
                        println!("{}", flotilla_protocol::output::json_pretty(&result));
                    }
                }
                let result = result.clone();
                return match result {
                    CommandValue::Error { message } => Err(message),
                    CommandValue::Cancelled => Err("command cancelled".into()),
                    result => Ok(result),
                };
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                if matches!(format, OutputFormat::Human) {
                    eprintln!("warning: skipped {n} events");
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                return Err("daemon disconnected".into());
            }
        }
    }
}

async fn run_query_command(daemon: &dyn DaemonHandle, command: Command, format: OutputFormat) -> Result<CommandValue, String> {
    let result = daemon.execute_query(command, uuid::Uuid::new_v4()).await?;
    match format {
        OutputFormat::Human => {
            print!("{}", format_command_result(&result));
        }
        OutputFormat::Json => {
            println!("{}", flotilla_protocol::output::json_pretty(&result));
        }
    }
    match result {
        CommandValue::Error { message } => Err(message),
        CommandValue::Cancelled => Err("command cancelled".into()),
        result => Ok(result),
    }
}

#[cfg(test)]
mod tests;
