use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write as _,
    time::Duration,
};

use chrono::{DateTime, Utc};
use comfy_table::{presets::UTF8_FULL_CONDENSED, Cell, Table};
use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{
    commands::ExplainedSubjectFact, output::OutputFormat, CliListKind, CliListResponse, Command, CommandAction, CommandValue,
    CrewListResponse, DaemonEvent, EnvironmentInfo, EnvironmentStatus, EvidenceFreshness, FleetHealthResponse, FleetHostStaleness,
    FleetListResponse, FleetObservationAgreement, FleetStaleness, FulfilmentListResponse, FulfilmentRow, HostProvidersResponse,
    HostStatusResponse, NodeId, NodeInfo, PeerConnectionState, ProjectListResponse, RepoProvidersResponse, StatusResponse, StreamKey,
    TopologyResponse,
};

use crate::socket::{DaemonEndpoint, SocketDaemon};

fn format_status_response_human(status: &StatusResponse) -> String {
    if status.repos.is_empty() {
        return "No repository provider status available.\n".into();
    }
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Repo", "Path", "Health", "Unavailable"]);
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
    table.set_header(vec!["Host", "Node", "Local", "Configured", "Status", "Summary", "Repos"]);
    for host in &response.hosts {
        table.add_row(vec![
            Cell::new(host.host_name.as_str()),
            Cell::new(host.node.as_ref().map(node_label).unwrap_or("-")),
            Cell::new(if host.is_local { "yes" } else { "no" }),
            Cell::new(if host.configured { "yes" } else { "no" }),
            Cell::new(match &host.reconnect {
                Some(reconnect) => {
                    format!("reconnecting (attempt {}, next dial in {}s)", reconnect.attempt, reconnect.next_dial_in_seconds)
                }
                None => format_connection_status(&host.connection_status).to_string(),
            }),
            Cell::new(if host.has_summary { "yes" } else { "no" }),
            Cell::new(host.repo_count),
        ]);
    }
    format!("{table}\n")
}

fn format_observation_time(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(at) = at else {
        return "-".to_string();
    };
    let age = now.signed_duration_since(at).num_seconds().max(0);
    format!("{} ({age}s ago)", at.format("%Y-%m-%d %H:%M:%SZ"))
}

fn format_disk_free(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| "-".to_string(), |bytes| format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0)))
}

fn format_sleep_inhibition(health: &flotilla_protocol::SleepInhibitionHealth) -> String {
    match health {
        flotilla_protocol::SleepInhibitionHealth::NotRequired => "not required".to_string(),
        flotilla_protocol::SleepInhibitionHealth::Held => "held".to_string(),
        flotilla_protocol::SleepInhibitionHealth::Acquiring { consecutive_failures, .. } => {
            format!("acquiring ({consecutive_failures} failures)")
        }
        flotilla_protocol::SleepInhibitionHealth::Failed { consecutive_failures, message } => {
            format!("FAILED ({consecutive_failures}): {message}")
        }
    }
}

pub(crate) fn format_fleet_health_human(response: &FleetHealthResponse) -> String {
    let now = Utc::now();
    let mut output = if response.hosts.is_empty() {
        "No hosts known.\n".to_string()
    } else {
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec![
            "Host",
            "Version",
            "Daemon Gen",
            "Uptime",
            "Link",
            "Last Heartbeat",
            "Replica Sync",
            "Replica Gen",
            "Crew",
            "Convoys",
            "Surfaces",
            "Disk Free",
            "Daemon RSS",
            "Blob Sync",
            "Sleep Inhibition",
            "Staleness",
            "Diagnosis",
            "Fulfilments",
        ]);
        for host in &response.hosts {
            let name = if host.is_local { format!("{} (local)", host.host) } else { host.host.to_string() };
            let row = match host.staleness {
                FleetHostStaleness::Current => "current",
                FleetHostStaleness::Stale => "STALE",
                FleetHostStaleness::Unknown => "unknown",
            };
            let mut diagnoses = Vec::new();
            if !host.degraded_conditions.is_empty() {
                diagnoses.push(format!("⚠ DEGRADED: {}", host.degraded_conditions.join("; ")));
            }
            if !host.credential_attention.is_empty() {
                let details = host.credential_attention.iter().map(|attention| attention.message.as_str()).collect::<Vec<_>>().join("; ");
                diagnoses.push(format!("⚠ CREDENTIALS: {details}"));
            }
            if matches!(&host.sleep_inhibition, flotilla_protocol::SleepInhibitionHealth::Failed { .. }) {
                diagnoses.push("⚠ SLEEP INHIBITION FAILED".to_string());
            }
            if host.observation_agreement == FleetObservationAgreement::Disagree {
                diagnoses.push("⚠ DISAGREE".to_string());
            }
            let diagnosis = if diagnoses.is_empty() {
                match host.observation_agreement {
                    FleetObservationAgreement::Unknown => "unknown".to_string(),
                    FleetObservationAgreement::Agree | FleetObservationAgreement::Disagree => "agree".to_string(),
                }
            } else {
                diagnoses.join("; ")
            };
            table.add_row(vec![
                Cell::new(name),
                Cell::new(host.daemon_version.as_deref().unwrap_or("-")),
                Cell::new(host.daemon_generation.as_deref().unwrap_or("-")),
                Cell::new(host.daemon_uptime_seconds.map_or_else(|| "-".to_string(), |seconds| format!("{seconds}s"))),
                Cell::new(format_connection_status(&host.link)),
                Cell::new(format_observation_time(host.heartbeat_at, now)),
                Cell::new(format_observation_time(host.replica_last_sync, now)),
                Cell::new(host.replica_generation.as_deref().unwrap_or("-")),
                Cell::new(host.crew_count),
                Cell::new(host.convoy_count),
                Cell::new(format!(
                    "{} available · {} handled · {} need you",
                    host.surface_states.available, host.surface_states.stalled_handled, host.surface_states.needs_you
                )),
                Cell::new(format_disk_free(host.disk_free_bytes)),
                Cell::new(
                    host.daemon_rss_bytes.map_or_else(|| "-".to_string(), |bytes| format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))),
                ),
                Cell::new(host.blob_sync.as_ref().map_or_else(
                    || "-".to_string(),
                    |sync| match &sync.last_error {
                        Some(error) => format!("{} pending; {error}", sync.pending_count),
                        None => format!("{} pending", sync.pending_count),
                    },
                )),
                Cell::new(format_sleep_inhibition(&host.sleep_inhibition)),
                Cell::new(row),
                Cell::new(diagnosis),
                Cell::new(host.fulfilments.iter().map(format_fulfilment_summary).collect::<Vec<_>>().join("; ")),
            ]);
        }
        format!("{table}\n")
    };
    output.push_str("\nDispatch queue:\n");
    output.push_str(&format_dispatch_queue_human(&response.dispatch_queue));
    output
}

fn format_fulfilment_summary(kind: &FulfilmentRow) -> String {
    let harnesses = kind
        .harnesses
        .iter()
        .map(|(name, facts)| {
            let models = facts
                .models
                .iter()
                .map(|(model, fact)| format!("{model}={} ({})", if fact.usable { "yes" } else { "no" }, fact.source))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{name} {} [{models}]", facts.version)
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}: {} | {}", kind.name, kind.grants.join(", "), harnesses)
}

pub(crate) fn format_fulfilment_list_human(response: &FulfilmentListResponse) -> String {
    if response.kinds.is_empty() {
        return "No fulfilment kinds known.\n".to_string();
    }
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Kind", "Host", "Realisation", "Pool", "Grants", "Harness / Models", "Toolchains", "GUI", "Free Slots"]);
    for kind in &response.kinds {
        let harnesses = kind
            .harnesses
            .iter()
            .map(|(name, facts)| {
                let models = facts
                    .models
                    .iter()
                    .map(|(model, fact)| format!("{model}: {} ({})", if fact.usable { "yes" } else { "no" }, fact.source))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{name} {}: {models}", facts.version)
            })
            .collect::<Vec<_>>()
            .join("; ");
        table.add_row(vec![
            Cell::new(&kind.name),
            Cell::new(&kind.host_ref),
            Cell::new(&kind.realisation),
            Cell::new(&kind.pool),
            Cell::new(kind.grants.join(", ")),
            Cell::new(harnesses),
            Cell::new(kind.toolchains.iter().map(|(name, version)| format!("{name} {version}")).collect::<Vec<_>>().join(", ")),
            Cell::new(kind.gui_session_logged_in.map_or("-", |logged_in| if logged_in { "yes" } else { "no" })),
            Cell::new(kind.free_vessel_slots.map_or_else(|| "unbounded".to_string(), |slots| slots.to_string())),
        ]);
    }
    format!("{table}\n")
}

fn format_dispatch_queue_human(response: &flotilla_protocol::DispatchQueueResponse) -> String {
    if response.entries.is_empty() {
        return "No dispatchable issues.\n".to_string();
    }
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Project", "Issue", "Ready For", "Attention", "Title"]);
    for entry in &response.entries {
        table.add_row(vec![
            Cell::new(format!("{}/{}", entry.namespace, entry.project)),
            Cell::new(format!("{}#{}", entry.issue.source.scope, entry.issue.id)),
            Cell::new(format!("{}s", entry.age_seconds)),
            Cell::new(if entry.attention { "! stale" } else { "" }),
            Cell::new(&entry.title),
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
    table.set_header(vec!["Project", "Display Name", "Repositories", "Issue Source", "Workflow", "Conflict", "Address"]);
    for project in &response.projects {
        let repository_count = project.repositories.len();
        let repositories = if repository_count <= 3 {
            project
                .repositories
                .iter()
                .map(|repository| repository.slug.as_deref().unwrap_or(flotilla_protocol::UNKNOWN_REPOSITORY_LABEL))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            format!("{repository_count} repositories")
        };
        let issue_source = match project.issue_sources.as_slice() {
            [] => "-".to_string(),
            [source] => format!("{} / {}", source.service.trim_end_matches('/'), source.scope),
            sources => format!("{} sources", sources.len()),
        };
        let stale_marker = if project.declaration_stale { " (stale)" } else { "" };
        table.add_row(vec![
            Cell::new(format!("{}/{}", project.namespace, project.name)),
            Cell::new(&project.display_name),
            Cell::new(repositories),
            Cell::new(issue_source),
            Cell::new(&project.default_workflow_ref),
            Cell::new(match &project.declaration_refused {
                Some(message) => format!("DeclarationRefused{stale_marker}: {message}"),
                None if !project.conflicts.is_empty() => format!("! {}", project.conflicts.join(", ")),
                None => String::new(),
            }),
            Cell::new(project.address.human_label()),
        ]);
    }
    format!("{table}\n")
}

fn format_cli_list_human(response: &CliListResponse) -> String {
    if response.items.is_empty() {
        return match response.list_kind {
            CliListKind::Repo => "No repositories available.\n".into(),
            CliListKind::Checkout => "No active checkouts found.\n".into(),
            CliListKind::Cr => "No open change requests found.\n".into(),
            CliListKind::Agent => "No active agent sessions found.\n".into(),
            CliListKind::Workspace => "No active workspaces found.\n".into(),
        };
    }
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Repository", "Reference", "Name", "Status", "Provider"]);
    for item in &response.items {
        table.add_row(vec![
            Cell::new(item.repo.as_deref().unwrap_or("-")),
            Cell::new(&item.reference),
            Cell::new(&item.name),
            Cell::new(&item.status),
            Cell::new(item.provider.as_deref().unwrap_or("-")),
        ]);
    }
    format!("{table}\n")
}

fn format_issue_page_human(page: &flotilla_protocol::issue_query::IssueResultPage) -> String {
    if page.items.is_empty() {
        return "No open issues found.\n".into();
    }
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Issue", "Title", "Labels"]);
    for issue in &page.items {
        table.add_row(vec![
            Cell::new(format!("{}#{}", issue.reference.source.scope, issue.reference.id)),
            Cell::new(&issue.title),
            Cell::new(issue.labels.join(", ")),
        ]);
    }
    let mut output = format!("{table}\n");
    if page.has_more {
        output.push_str("More issues available.\n");
    }
    output
}

fn format_host_status_human(response: &HostStatusResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Host: {}\n", response.host_name));
    out.push_str(&format!("Node: {}\n", node_label(&response.node)));
    out.push_str(&format!("Status: {}\n", format_connection_status(&response.connection_status)));
    out.push_str(&format!("Configured: {}\n", if response.configured { "yes" } else { "no" }));
    out.push_str(&format!("Repositories: {}\n", response.repo_count));
    if let Some(sync) = &response.blob_sync {
        out.push_str(&format!("Blob sync: {} pending\n", sync.pending_count));
        if let Some(error) = &sync.last_error {
            out.push_str(&format!("Blob sync error: {error}\n"));
        }
    }

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
            Cell::new(provider.disabled_reason.as_ref().map_or_else(
                || if provider.healthy { "ok".to_string() } else { "error".to_string() },
                |reason| format!("disabled: {reason}"),
            )),
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
            Cell::new(
                route.last_attempt.map(|attempt| attempt.format("%Y-%m-%d %H:%M:%SZ").to_string()).unwrap_or_else(|| "-".to_string()),
            ),
            Cell::new(route.last_error.as_deref().unwrap_or("-")),
            Cell::new(fallbacks),
        ]);
    }
    out.push_str(&table.to_string());
    out.push('\n');
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopologyOutputFormat {
    Human,
    Json,
    Dot,
}

impl From<OutputFormat> for TopologyOutputFormat {
    fn from(format: OutputFormat) -> Self {
        match format {
            OutputFormat::Human => Self::Human,
            OutputFormat::Json => Self::Json,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TopologyGraphNode {
    label: String,
    local: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TopologyGraphEdgeKind {
    Direct,
    Routed,
    Fallback,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TopologyGraphEdge {
    from: NodeId,
    to: NodeId,
    kind: TopologyGraphEdgeKind,
    connected: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TopologyGraph {
    nodes: BTreeMap<NodeId, TopologyGraphNode>,
    edges: BTreeSet<TopologyGraphEdge>,
}

impl From<&TopologyResponse> for TopologyGraph {
    fn from(response: &TopologyResponse) -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(response.local_node.node_id.clone(), TopologyGraphNode {
            label: response.local_node.display_name.clone(),
            local: true,
        });

        let mut edges = BTreeSet::new();
        for route in &response.routes {
            insert_graph_node(&mut nodes, &route.target);
            insert_graph_node(&mut nodes, &route.next_hop);

            edges.insert(TopologyGraphEdge {
                from: if route.direct { response.local_node.node_id.clone() } else { route.next_hop.node_id.clone() },
                to: route.target.node_id.clone(),
                kind: if route.direct { TopologyGraphEdgeKind::Direct } else { TopologyGraphEdgeKind::Routed },
                connected: Some(route.connected),
            });

            for fallback in &route.fallbacks {
                insert_graph_node(&mut nodes, fallback);
                edges.insert(TopologyGraphEdge {
                    from: if fallback.node_id == route.target.node_id {
                        response.local_node.node_id.clone()
                    } else {
                        fallback.node_id.clone()
                    },
                    to: route.target.node_id.clone(),
                    kind: TopologyGraphEdgeKind::Fallback,
                    connected: None,
                });
            }
        }

        Self { nodes, edges }
    }
}

fn insert_graph_node(nodes: &mut BTreeMap<NodeId, TopologyGraphNode>, node: &NodeInfo) {
    nodes.entry(node.node_id.clone()).or_insert_with(|| TopologyGraphNode { label: node.display_name.clone(), local: false });
}

fn dot_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn format_topology_dot(response: &TopologyResponse) -> String {
    let graph = TopologyGraph::from(response);
    let mut out = String::from("digraph topology {\n  graph [rankdir=LR];\n  node [shape=ellipse];\n");

    for (node_id, node) in graph.nodes {
        let local_attribute = if node.local { ", shape=doublecircle" } else { "" };
        writeln!(out, "  {} [label={}{}];", dot_quote(node_id.as_str()), dot_quote(&node.label), local_attribute)
            .expect("writing to a string cannot fail");
    }

    for edge in graph.edges {
        let attributes = match (edge.kind, edge.connected) {
            (TopologyGraphEdgeKind::Direct, Some(true)) => "label=\"direct\"",
            (TopologyGraphEdgeKind::Direct, Some(false)) => "label=\"direct, disconnected\", color=red, style=dashed",
            (TopologyGraphEdgeKind::Routed, Some(true)) => "label=\"route\"",
            (TopologyGraphEdgeKind::Routed, Some(false)) => "label=\"route, disconnected\", color=red, style=dashed",
            (TopologyGraphEdgeKind::Fallback, _) => "label=\"fallback\", style=dotted",
            (_, None) => unreachable!("only fallback edges omit connection state"),
        };
        writeln!(out, "  {} -> {} [{attributes}];", dot_quote(edge.from.as_str()), dot_quote(edge.to.as_str()))
            .expect("writing to a string cannot fail");
    }

    out.push_str("}\n");
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
        table.set_header(vec!["Convoy", "Subjects", "Vessel", "Crew", "State", "Surface", "Attention", "Host", "Placement", "Staleness"]);
        for row in &response.rows {
            let vessel = match &row.authority {
                Some(authority) => format!("{} ({authority})", row.vessel),
                None => row.vessel.clone(),
            };
            table.add_row(vec![
                Cell::new(&row.convoy),
                Cell::new({
                    let mut grouped = std::collections::BTreeMap::<flotilla_protocol::Relationship, Vec<&str>>::new();
                    for subject in &row.subjects {
                        grouped.entry(subject.relationship).or_default().push(&subject.short);
                    }
                    grouped
                        .into_iter()
                        .map(|(relationship, references)| format!("{} {}", relationship.as_str().replace('_', " "), references.join(", ")))
                        .collect::<Vec<_>>()
                        .join(" · ")
                }),
                Cell::new(vessel),
                Cell::new(&row.crew),
                Cell::new(&row.crew_state),
                Cell::new(if row.convoy_ref.is_some() { row.surface_state.label() } else { "-" }),
                Cell::new(row.attention.map_or_else(|| "-".to_string(), |attention| attention.to_string())),
                Cell::new(row.host.as_str()),
                Cell::new(row.placement_decision.as_ref().map_or_else(
                    || "-".to_string(),
                    |decision| {
                        let refusals = if decision.refused_candidates.is_empty() {
                            String::new()
                        } else {
                            format!("; {} refused", decision.refused_candidates.len())
                        };
                        let viable = if decision.viable_not_selected.is_empty() {
                            String::new()
                        } else {
                            format!("; {} viable not selected", decision.viable_not_selected.len())
                        };
                        format!("{} on {}{refusals}{viable}", decision.policy_name, decision.target_host.display_name)
                    },
                )),
                Cell::new(format_fleet_staleness(&row.staleness)),
            ]);
        }
        out.push_str(&table.to_string());
        out.push('\n');
    }

    if !response.declaration_attention.is_empty() {
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec!["Declaration", "Condition", "Attention"]);
        for row in &response.declaration_attention {
            table.add_row(vec![
                Cell::new(format!("{}/{}/{}", row.resource.namespace, row.resource.kind, row.resource.name)),
                Cell::new(row.condition.to_string()),
                Cell::new(&row.message),
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
            let status = replica.message.as_deref().unwrap_or("unreachable");
            table.add_row(vec![
                Cell::new(replica.host.as_str()),
                Cell::new(status),
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
    table.set_header(vec!["Role", "Kind", "State", "Attention", "Adapter", "Model", "Stance"]);
    for member in &response.members {
        table.add_row(vec![
            Cell::new(&member.role),
            Cell::new(&member.kind),
            Cell::new(&member.state),
            Cell::new(member.attention.map_or_else(|| "-".to_string(), |attention| attention.to_string())),
            Cell::new(member.adapter.as_deref().unwrap_or("-")),
            Cell::new(member.model.as_deref().unwrap_or("-")),
            Cell::new(member.stance.as_deref().unwrap_or("-")),
        ]);
    }
    let mut charter = String::new();
    if let Some(error) = &response.project_error {
        let _ = writeln!(charter, "Live Project unavailable: {error}");
    }
    if let Some(project) = &response.project {
        let _ = writeln!(charter, "Live Project: {}/{} ({})", project.namespace, project.name, project.display_name);
        for repository in &project.repositories {
            let roles = repository.roles.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
            let _ = writeln!(
                charter,
                "  {}  alias={}  roles=[{}]  subpath={}  branch={}  remotes=[{}]",
                repository.key,
                repository.alias.as_deref().unwrap_or("-"),
                roles,
                repository.subpath.as_deref().unwrap_or("-"),
                repository.default_branch.as_deref().unwrap_or("-"),
                repository.remotes.join(", ")
            );
        }
    }
    let alerts = response.credential_alerts.iter().map(|alert| format!("Credential attention: {alert}\n")).collect::<String>();
    format!("Convoy: {}  Vessel: {} ({})\n{}\n{alerts}{charter}", response.convoy, response.vessel, response.vessel_ref, table)
}

fn explained_condition_label(condition: Option<&flotilla_protocol::ExplainedCondition>) -> String {
    condition.map_or_else(
        || "missing".to_string(),
        |condition| {
            format!("{} @ {} ({:?})", condition.value, condition.observed_at.as_deref().unwrap_or("unknown"), condition.freshness)
                .to_lowercase()
        },
    )
}

fn explanation_provenance_label(provenance: Option<&flotilla_protocol::ResourceRecordProvenance>) -> String {
    match provenance {
        Some(flotilla_protocol::ResourceRecordProvenance::Local { node_id }) => format!("local:{node_id}"),
        Some(flotilla_protocol::ResourceRecordProvenance::Replica { origin_root, .. }) => format!("replica:{origin_root}"),
        None => "-".to_string(),
    }
}

fn format_subject_fact(fact: Option<&ExplainedSubjectFact>) -> String {
    let value = fact.and_then(|fact| fact.value.as_deref()).unwrap_or("unknown");
    match fact.map_or(EvidenceFreshness::Missing, |fact| fact.freshness) {
        EvidenceFreshness::Fresh => value.to_string(),
        EvidenceFreshness::Stale => format!("{value} (stale)"),
        EvidenceFreshness::Missing => format!("{value} (missing)"),
    }
}

pub(crate) fn format_convoy_explanation_human(explanation: &flotilla_protocol::ConvoyExplanation) -> String {
    let mut output = format!("Convoy: {}/{}\nPhase: {}\n", explanation.namespace, explanation.convoy, explanation.phase);
    for mutation in &explanation.lifecycle_mutations {
        let _ = writeln!(output, "{} by {} at {}", mutation.action, mutation.caller, mutation.at);
    }
    if let Some(message) = explanation.message.as_deref() {
        let _ = writeln!(output, "Message: {message}");
    }
    for (role, needs) in &explanation.role_needs {
        let _ = writeln!(output, "Needs for {role}: {}", if needs.is_empty() { "(none)".to_string() } else { needs.join(", ") });
    }
    for decision in &explanation.allocation {
        let _ = writeln!(output, "Allocation for {}: {} ({})", decision.vessel, decision.roles.join(", "), decision.reason);
        for handoff in &decision.crossed_handoffs {
            let _ = writeln!(output, "  Artifact handoff: {handoff}");
        }
    }
    let mut write_placement = |label: &str, placement: &flotilla_protocol::PlacementDecision| {
        let _ = writeln!(output, "{label}: {} on {}", placement.policy_name, placement.target_host.display_name);
        if !placement.minimal_alternatives.is_empty() {
            let _ = writeln!(output, "Minimal alternatives: {}", placement.minimal_alternatives.join(", "));
        }
        if let Some(reason) = &placement.escalation_reason {
            let _ = writeln!(output, "Escalation: {reason}");
        }
        if let Some(allocation) = &placement.allocation {
            if let Some(reason) = &allocation.reservation_reason {
                let _ = writeln!(output, "Reserved platform capacity used: {reason}");
            }
            for candidate in &allocation.candidates {
                let _ = writeln!(
                    output,
                    "  {} on {}: cost {}, ready {}, sleep until {}, free slots {}, reserved {}, minimal {}, available {}{}",
                    candidate.kind,
                    candidate.host,
                    candidate.cost_class,
                    candidate.host_ready,
                    candidate.sleeping_until.map_or_else(|| "none".to_string(), |until| until.to_rfc3339()),
                    candidate.free_vessel_slots.map_or_else(|| "unbounded".to_string(), |slots| slots.to_string()),
                    candidate.reserved_for_platform,
                    candidate.minimal,
                    candidate.available,
                    if candidate.kind == allocation.chosen_kind { " (chosen)" } else { "" },
                );
            }
        }
    };
    for (vessel, observation) in &explanation.environment_observations {
        println!("Environment for vessel {vessel}:");
        if let Some(limits) = &observation.memory_limits {
            println!("  Memory limit: {} bytes; swap limit: {} bytes", limits.memory_bytes, limits.swap_bytes);
        }
        if let Some(usage) = observation.memory_usage_bytes {
            println!("  Last memory usage: {usage} bytes (observed {})", observation.memory_observed_at.as_deref().unwrap_or("unknown"));
        }
        if let Some(termination) = &observation.termination {
            println!("  {termination}; Docker OOMKilled={}", termination.oom_killed);
            if let Some(evidence) = &termination.evidence {
                println!("  Evidence: {evidence}");
            }
        }
    }
    if explanation.vessel_placements.is_empty() {
        if let Some(placement) = &explanation.placement {
            write_placement("Fulfilment", placement);
        }
    } else {
        for (vessel, placement) in &explanation.vessel_placements {
            write_placement(&format!("Fulfilment for {vessel}"), placement);
        }
    }
    if let Some(stalled) = &explanation.stalled {
        let _ = writeln!(output, "Stalled: {}", serde_json::to_string_pretty(stalled).expect("stall condition serializes"));
    }
    let standing = explanation.settlement.mode == flotilla_protocol::commands::SETTLEMENT_MODE_STANDING;
    let verdict = if explanation.settlement.satisfied { "SATISFIED" } else { "HOLDING" };
    if standing {
        output.push_str("Settlement: STANDING (no exit table)\n");
    } else {
        let _ = writeln!(output, "Settlement: {verdict} ({})", explanation.settlement.mode);
    }
    let _ = writeln!(
        output,
        "Freshness: checkout evidence < {}s; change requests <= {}s",
        explanation.evidence_ttl_seconds, explanation.change_request_stale_after_seconds
    );
    if !explanation.settlement.unmet.is_empty() {
        output.push_str("\nUnmet expectations:\n");
        for unmet in &explanation.settlement.unmet {
            let _ = writeln!(output, "  - {}: {} — {}", unmet.reason, unmet.subject, unmet.detail);
        }
    }

    if !explanation.unclaimed_work.is_empty() {
        output.push_str("\nCrew work needing a settlement claim:\n");
        for work in &explanation.unclaimed_work {
            let _ = writeln!(output, "  - {}/{} ({})", work.vessel, work.role, work.evidence.replace('_', " "));
        }
    }

    output.push_str("\nArtifacts:\n");
    if explanation.artifacts.is_empty() {
        output.push_str("  (none)\n");
    } else {
        for artifact in &explanation.artifacts {
            let _ = writeln!(
                output,
                "  - {}: {}{}",
                artifact.kind,
                artifact.address,
                artifact.view_url.as_ref().map_or_else(String::new, |url| format!(" {url}"))
            );
        }
    }

    output.push_str("\nDecision ledgers:\n");
    if explanation.decision_ledgers.is_empty() {
        output.push_str("  (no settlement claims)\n");
    } else {
        for ledger in &explanation.decision_ledgers {
            if ledger.superseded {
                let _ = writeln!(
                    output,
                    "  - {}/{} superseded claim at={} comment={} message={}",
                    ledger.vessel,
                    ledger.role,
                    ledger.claimed_at.as_deref().unwrap_or("-"),
                    ledger.comment_url.as_deref().unwrap_or("-"),
                    ledger.message.as_deref().unwrap_or("-")
                );
                continue;
            }
            if ledger.missing {
                let detail = ledger.override_principal.as_ref().map_or_else(
                    || "crew completed without a decision ledger".to_string(),
                    |principal| format!("completed by {}/{} with --force", principal.namespace, principal.name),
                );
                let _ = writeln!(
                    output,
                    "  - {}/{} claimed_at={} MISSING ({detail}){}",
                    ledger.vessel,
                    ledger.role,
                    ledger.claimed_at.as_deref().unwrap_or("-"),
                    if ledger.completed_while_crew_active { " — completed while crew active" } else { "" }
                );
            } else {
                let _ = writeln!(
                    output,
                    "  - {}/{} claimed_at={} comment={}",
                    ledger.vessel,
                    ledger.role,
                    ledger.claimed_at.as_deref().unwrap_or("-"),
                    ledger.comment_url.as_deref().unwrap_or("-")
                );
            }
        }
    }

    output.push_str("\nExpected checkouts:\n");
    if explanation.checkouts.is_empty() {
        output.push_str("  (none; artifact-less claim exit)\n");
    } else {
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec!["Checkout", "Observed", "Source", "Landed", "Clean", "Pushed"]);
        for checkout in &explanation.checkouts {
            table.add_row(vec![
                Cell::new(&checkout.name),
                Cell::new(if checkout.observed { "yes" } else { "NO" }),
                Cell::new(explanation_provenance_label(checkout.provenance.as_ref())),
                Cell::new(explained_condition_label(checkout.landed.as_ref())),
                Cell::new(explained_condition_label(checkout.clean.as_ref())),
                Cell::new(explained_condition_label(checkout.pushed.as_ref())),
            ]);
        }
        let _ = writeln!(output, "{table}");
    }

    output.push_str("\nSubjects:\n");
    if explanation.subjects.is_empty() {
        output.push_str("  (none)\n");
    } else {
        let mut grouped =
            std::collections::BTreeMap::<flotilla_protocol::Relationship, Vec<&flotilla_protocol::result_set::ConvoySubjectRow>>::new();
        for subject in &explanation.subjects {
            grouped.entry(subject.relationship).or_default().push(subject);
        }
        for (relationship, subjects) in grouped {
            let references = subjects.iter().map(|subject| subject.short.as_str()).collect::<Vec<_>>().join(", ");
            let _ = writeln!(output, "  {} {references}", relationship.as_str().replace('_', " "));
            for subject in subjects {
                if subject.subject.kind == flotilla_protocol::SubjectKind::ChangeRequest {
                    let observation = explanation.subject_observations.iter().find(|observation| observation.subject == subject.subject);
                    let _ = writeln!(
                        output,
                        "    {} state={} checks={} review={} actionable_at_head={} readiness={}",
                        subject.short,
                        format_subject_fact(observation.map(|o| &o.state)),
                        format_subject_fact(observation.map(|o| &o.checks)),
                        format_subject_fact(observation.map(|o| &o.review)),
                        format_subject_fact(observation.map(|o| &o.review_actionable_at_head)),
                        format_subject_fact(observation.map(|o| &o.readiness))
                    );
                }
                if let Some(url) = &subject.url {
                    let _ = writeln!(output, "    {} {url}", subject.short);
                }
            }
        }
    }

    output.push_str("\nChange requests:\n");
    if explanation.change_requests.is_empty() {
        output.push_str("  (none)\n");
    } else {
        for request in &explanation.change_requests {
            let fields = request.fields.as_ref().map_or_else(|| "missing".to_string(), serde_json::Value::to_string);
            let _ = writeln!(
                output,
                "  - {} started_for={} observed={} source={} observed_at={} freshness={:?}\n    fields={}",
                request.name,
                request.bound,
                request.observed,
                explanation_provenance_label(request.provenance.as_ref()),
                request.observed_at.as_deref().unwrap_or("-"),
                request.freshness,
                fields
            );
            if let Some(error) = &request.observation_error {
                let _ = writeln!(output, "    observation_error={error}");
            }
        }
    }

    output.push_str("\nArmed subscriptions:\n");
    if explanation.subscriptions.is_empty() {
        output.push_str("  (none recorded on this host)\n");
    } else {
        for subscription in &explanation.subscriptions {
            let _ = writeln!(output, "  - {} watcher={}", subscription.id, subscription.watcher);
            for leaf in &subscription.leaves {
                let _ = writeln!(output, "    leaf: {leaf}");
            }
            for firing in &subscription.last_leaf_firings {
                let _ = writeln!(output, "    last firing: {} => {} at {}", firing.leaf, firing.value, firing.fired_at);
            }
        }
    }

    if !explanation.queued_turns.is_empty() {
        output.push_str("\nQueued turns (not submitted):\n");
        for turn in &explanation.queued_turns {
            let _ = writeln!(
                output,
                "  - {} {}/{} revision={} rung={} queued_at={} age={}s{}: {}",
                turn.source,
                turn.vessel,
                turn.role,
                turn.subject_revision,
                match turn.rung {
                    flotilla_protocol::TurnDeliveryRung::WarmSession => "warm-session",
                    flotilla_protocol::TurnDeliveryRung::FreshAgent => "fresh-agent",
                },
                turn.queued_at,
                turn.age_seconds,
                if turn.overdue { " OVERDUE" } else { "" },
                turn.blocking_reason
            );
        }
    }
    output.push_str("\nCrew delivery:\n");
    for (crew, skills) in &explanation.skills {
        writeln!(output, "Skills {crew}: {skills}").expect("write skill explanation");
    }
    if explanation.crew_deliveries.is_empty() {
        output.push_str("  (none)\n");
    } else {
        for delivery in &explanation.crew_deliveries {
            let _ = writeln!(
                output,
                "  - {} role={} last_rung={} delivered_message={} sender={}",
                delivery.session,
                delivery.role,
                delivery.last_delivery_rung.as_deref().unwrap_or("not recorded"),
                delivery.delivered_message_id.as_deref().unwrap_or("-"),
                delivery.sender.as_ref().map(|sender| sender.short_label()).unwrap_or_else(|| "-".to_string())
            );
            if let Some(condition) = &delivery.terminal_condition {
                let _ = writeln!(output, "    {condition}");
            }
            for brief in &delivery.pending_briefs {
                let _ = writeln!(output, "    pending brief: {brief}");
            }
        }
    }
    output
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

fn format_stall_evidence(text: &str, full: bool) -> String {
    if full {
        return text.to_string();
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut abbreviated: String = text.chars().take(160).collect();
    if text.chars().count() > 160 {
        abbreviated.push('…');
    }
    abbreviated
}

/// Format a `CommandValue` as a short human-readable string.
fn format_command_result(result: &flotilla_protocol::commands::CommandValue) -> String {
    use flotilla_protocol::commands::CommandValue;
    match result {
        CommandValue::CrewTurnDelivered { rung } => format!("Crew turn delivered: {rung:?}"),
        CommandValue::Ok => "ok".to_string(),
        CommandValue::CrewFollowUpDelivered => flotilla_protocol::commands::CREW_FOLLOW_UP_INSTRUCTION.to_string(),
        CommandValue::CrewCompletionWaiting { reason, retry_at } => format!("crew completion waiting until {retry_at}: {reason}"),
        CommandValue::ResourceReconciled { message, .. } => message.clone(),
        CommandValue::ConvoyBriefDelivered { displaced: Some(displaced) } => {
            format!("brief delivered now; displaced pending brief:\n{displaced}")
        }
        CommandValue::ConvoyBriefDelivered { displaced: None } => "brief delivered now".to_string(),
        CommandValue::ConvoyBriefQueued { displaced: Some(displaced) } => {
            format!("brief queued for turn end; displaced brief:\n{displaced}")
        }
        CommandValue::ConvoyBriefQueued { displaced: None } => "brief queued for turn end".to_string(),
        CommandValue::ConvoyBriefWithdrawn { withdrawn: Some(withdrawn) } => format!("pending brief withdrawn:\n{withdrawn}"),
        CommandValue::ConvoyBriefWithdrawn { withdrawn: None } => "no pending brief to withdraw".to_string(),
        CommandValue::RepoTracked { path, resolved_from, identity_change } => {
            let mut output = match resolved_from {
                Some(original) => format!("observing checkout: {} (resolved from {})", path.display(), original.display()),
                None => format!("observing checkout: {}", path.display()),
            };
            if let Some(change) = identity_change {
                output.push_str(&format!("\nrepository identity changed: {} → {}", change.previous_display, change.current_display));
            }
            output
        }
        CommandValue::RepoUntracked { path } => format!("stopped observing checkout: {}", path.display()),
        CommandValue::Refreshed { repository_count, identity_changes, .. } => {
            let mut output = format!("refreshed {repository_count} repo(s)");
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
        CommandValue::RepositoryResolved { key: Some(key) } => format!("Repository/{key}"),
        CommandValue::RepositoryResolved { key: None } => "no matching Repository".into(),
        CommandValue::RepoProviders(providers) => format_repo_providers_human(providers),
        // HostList remains a protocol-level query used by host/environment
        // target resolution; keep its formatter for direct query diagnostics
        // even though `host list` now presents the richer fleet-health view.
        CommandValue::HostList(hosts) => format_host_list_human(hosts),
        CommandValue::ProjectList(projects) => format_project_list_human(projects),
        CommandValue::CliList(items) => format_cli_list_human(items),
        CommandValue::DispatchQueue(queue) => format_dispatch_queue_human(queue),
        CommandValue::HostStatus(status) => format_host_status_human(status),
        CommandValue::HostProviders(providers) => format_host_providers_human(providers),
        CommandValue::FleetHealth(fleet) => format_fleet_health_human(fleet),
        CommandValue::FleetPostInstall { report, .. } => serde_json::to_string_pretty(report).expect("JSON report serializes"),
        CommandValue::FulfilmentList(kinds) => format_fulfilment_list_human(kinds),
        CommandValue::FleetList(fleet) => format_fleet_list_human(fleet),
        CommandValue::CrewStalls(stalls) => {
            if stalls.rows.is_empty() {
                return "No stalled crew obligations".to_string();
            }
            let mut output =
                String::from("PROJECT / CONVOY | VESSEL / ROLE | RUNG | SUPERVISOR | AGE | PROPOSE | SHARED CAUSE | EVIDENCE\n");
            let groups: BTreeMap<_, _> = stalls
                .rows
                .iter()
                .map(|row| row.cause_group.as_str())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .enumerate()
                .map(|(index, cause)| (cause, index + 1))
                .collect();
            for row in &stalls.rows {
                let evidence = format_stall_evidence(&row.evidence, stalls.full);
                let supervisor = row
                    .supervisor
                    .clone()
                    .unwrap_or_else(|| format!("none: {}", row.supervisor_absence_reason.as_deref().unwrap_or("no supervisor recorded")));
                let supervisor = format_stall_evidence(&supervisor, stalls.full);
                let proposed = row.proposed_disposition.map(|value| value.to_string()).unwrap_or_else(|| "-".to_string());
                let _ = writeln!(
                    output,
                    "{} / {} | {} / {} | {} | {} | {} | {} | group {} ({}) | {}",
                    row.project_display_name.as_deref().or(row.project.as_deref()).unwrap_or("-"),
                    row.convoy_display_name,
                    if row.vessel.is_empty() { "-" } else { &row.vessel },
                    if row.role.is_empty() { "-" } else { &row.role },
                    row.rung.map(|rung| rung.to_string()).unwrap_or_else(|| "unknown".into()),
                    supervisor,
                    row.age_seconds.map(|age| format!("{age}s")).unwrap_or_else(|| "unknown".into()),
                    proposed,
                    groups[row.cause_group.as_str()],
                    row.shared_cause_count,
                    evidence
                );
                if stalls.full && !row.artifacts.is_empty() {
                    let _ = writeln!(output, "  artifacts: {}", row.artifacts.join(", "));
                }
            }
            output
        }
        CommandValue::CrewList(crew) => format_crew_list_human(crew),
        CommandValue::DaemonLogs { lines } => lines.join("\n"),
        CommandValue::ConvoyExplanation(explanation) => format_convoy_explanation_human(explanation),
        CommandValue::ResourceRead(response) => {
            let mut output = String::new();
            for record in &response.records {
                let name = record
                    .object
                    .as_ref()
                    .and_then(|object| object.pointer("/metadata/name"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<unknown>");
                let origin = match &record.provenance {
                    flotilla_protocol::ResourceRecordProvenance::Local { node_id } => node_id,
                    flotilla_protocol::ResourceRecordProvenance::Replica { origin_root, .. } => origin_root,
                };
                let _ = writeln!(output, "{}/{}/{} origin: {origin}", response.resource_kind, response.namespace, name);
            }
            output.push_str(&flotilla_protocol::output::json_pretty(response));
            output
        }
        CommandValue::ResourceObject(response) => flotilla_protocol::output::json_pretty(&response.value),
        CommandValue::ResourceDeleted(response) => {
            let name = response.value["metadata"]["name"].as_str().unwrap_or("<unknown>");
            let api_version = response.value["apiVersion"].as_str().unwrap_or("<unknown>");
            if let Some(origin_root) = &response.replica_origin {
                format!(
                    "collected replica {api_version}/{}/{}/{name} from {origin_root}\nA newer update from the authority may recreate it.",
                    response.kind, response.namespace,
                )
            } else {
                format!(
                    "deleted {api_version}/{}/{}/{name}\nControllers may recreate code-owned objects.",
                    response.kind, response.namespace,
                )
            }
        }
        CommandValue::ResourceAlreadyDeleted(response) => {
            let name = response.value["metadata"]["name"].as_str().unwrap_or("<unknown>");
            let api_version = response.value["apiVersion"].as_str().unwrap_or("<unknown>");
            format!("already deleted {api_version}/{}/{}/{name}", response.kind, response.namespace)
        }
        CommandValue::ResourceWatchEvent(response) => flotilla_protocol::output::json_pretty(response),
        CommandValue::EnvironmentSpecRead { .. } => "environment spec read".to_string(),
        CommandValue::IssuePage(page) => format_issue_page_human(page),
        CommandValue::IssuesByIds { items } => format!("issues by ids: {} items", items.len()),
        CommandValue::ConvoyCreated { name } => format!("convoy created: {name}"),
        CommandValue::ConvoyAbandoned { name, archives } => {
            let mut output = format!("convoy abandoned: {name}");
            for archive in archives {
                let result = match archive.status {
                    flotilla_protocol::CheckoutArchiveStatus::Archived => "archived".to_string(),
                    flotilla_protocol::CheckoutArchiveStatus::NothingToArchive => "nothing to archive".to_string(),
                    flotilla_protocol::CheckoutArchiveStatus::Failed => match archive.detail.as_deref() {
                        Some(detail) if !detail.is_empty() => format!("failed to archive: {detail}"),
                        _ => "failed to archive".to_string(),
                    },
                };
                output.push_str(&format!("\n  {}: {result}", archive.checkout));
            }
            output
        }
        CommandValue::ConvoyStarted { name, attach_plan, .. } => {
            format!("convoy started: {name}{}", if attach_plan.is_some() { " (crew ready)" } else { "" })
        }
        CommandValue::WorkflowTemplateApplied { name } => format!("workflow template applied: {name}"),
        CommandValue::ProjectAdded { name } => format!("project added: {name}"),
        CommandValue::ProjectApplied { name } => format!("project applied: {name}"),
        CommandValue::ProjectRegistered { name, members } => format!("project registered: {name} ({members} members)"),
        CommandValue::ProjectRefreshed { name, members, converged, changes, operational_entries } => {
            let outcome = if *converged { format!("changed: {}", changes.join(", ")) } else { "already current".to_string() };
            let entries = if operational_entries.is_empty() { String::new() } else { format!("\n{}", operational_entries.join("\n")) };
            format!("project refreshed: {name} ({members} members, {outcome}){entries}")
        }
    }
}

pub(crate) fn format_event_human(event: &flotilla_protocol::DaemonEvent) -> String {
    use flotilla_protocol::{DaemonEvent, PeerConnectionState};
    match event {
        DaemonEvent::RepoDelta(delta) => {
            format!("[repo]     {}: provider delta (seq {})", repo_label(delta.repo.as_deref(), &delta.repo_identity), delta.seq)
        }
        DaemonEvent::RepoRefreshCompleted { repo_identity, repo } => {
            format!("[refresh]  {}: completed", repo_label(repo.as_deref(), repo_identity))
        }
        DaemonEvent::RepoTracked(info) => {
            format!("[repo]     {}: observing checkout", info.name)
        }
        DaemonEvent::RepoUntracked { repo_identity, path } => {
            format!("[repo]     {}: stopped observing checkout", repo_label(path.as_deref(), repo_identity))
        }
        DaemonEvent::CommandStarted { repo_identity, repo, description, .. } => {
            if repo.is_none() && repo_identity.authority.is_empty() && repo_identity.path.is_empty() {
                // Query commands have no repo context — show description only
                format!("[query]    {description}")
            } else {
                format!("[command]  {}: started \"{}\"", repo_label(repo.as_deref(), repo_identity), description)
            }
        }
        DaemonEvent::CommandFinished { node_id, repo_identity, repo, result, .. } => {
            if repo.is_none() && repo_identity.authority.is_empty() && repo_identity.path.is_empty() {
                // Query commands have no repo context — show result directly
                format!("{}\nran on {node_id}", format_command_result(result))
            } else {
                format!(
                    "[command]  {}: finished on {node_id} \u{2192} {}",
                    repo_label(repo.as_deref(), repo_identity),
                    format_command_result(result)
                )
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
        DaemonEvent::LeafFired(fire) => format!("[leaf]      {} fired (value: {})", fire.leaf, fire.value),
    }
}

/// Extract the (stream_key, seq) from a snapshot/delta event, if present.
fn event_stream_seq(event: &DaemonEvent) -> Option<(StreamKey, u64)> {
    match event {
        DaemonEvent::HostSnapshot(snap) => Some((StreamKey::Host { environment_id: snap.environment_id.clone() }, snap.seq)),
        DaemonEvent::HostRemoved { environment_id, seq } => Some((StreamKey::Host { environment_id: environment_id.clone() }, *seq)),
        DaemonEvent::ResultSet(result_set) => Some((StreamKey::Query { query: result_set.query() }, result_set.seq)),
        DaemonEvent::ResultDelta(delta) => Some((StreamKey::Query { query: delta.query() }, delta.seq)),
        DaemonEvent::RepoDelta(_)
        | DaemonEvent::RepoTracked(_)
        | DaemonEvent::RepoRefreshCompleted { .. }
        | DaemonEvent::RepoUntracked { .. }
        | DaemonEvent::CommandStarted { .. }
        | DaemonEvent::CommandFinished { .. }
        | DaemonEvent::CommandStepUpdate { .. }
        | DaemonEvent::PeerStatusChanged { .. }
        | DaemonEvent::LeafFired(_) => None,
    }
}

pub async fn run_status(endpoint: &DaemonEndpoint, format: OutputFormat) -> Result<(), String> {
    let daemon = SocketDaemon::connect_endpoint(endpoint).await.map_err(|e| format!("cannot connect to daemon: {e}"))?;
    let status = daemon.get_status().await?;
    let output = match format {
        OutputFormat::Human => format_status_response_human(&status),
        OutputFormat::Json => flotilla_protocol::output::json_pretty(&status),
    };
    print!("{output}");
    Ok(())
}

pub async fn run_topology(daemon: &dyn DaemonHandle, format: TopologyOutputFormat) -> Result<(), String> {
    let topology = daemon.get_topology().await?;
    let output = match format {
        TopologyOutputFormat::Human => format_topology_human(&topology),
        TopologyOutputFormat::Json => flotilla_protocol::output::json_pretty(&topology),
        TopologyOutputFormat::Dot => format_topology_dot(&topology),
    };
    print!("{output}");
    Ok(())
}

fn format_repo_providers_human(resp: &RepoProvidersResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Repository: {}\n", resp.repository));
    if let Some(path) = &resp.path {
        out.push_str(&format!("Checkout: {}\n", path.display()));
    }
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
            table.add_row(vec![
                Cell::new(&p.category),
                Cell::new(&p.name),
                Cell::new(p.disabled_reason.as_ref().map_or_else(
                    || if p.healthy { "ok".to_string() } else { "error".to_string() },
                    |reason| format!("disabled: {reason}"),
                )),
            ]);
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

pub async fn run_watch(endpoint: &DaemonEndpoint, format: OutputFormat) -> Result<(), String> {
    loop {
        let daemon = flotilla_client::reconnect::connect_with_retry(
            || SocketDaemon::connect_endpoint(endpoint),
            |notice| match notice {
                flotilla_client::reconnect::ReconnectNotice::Attempt { attempt } => {
                    eprintln!("connecting to daemon (attempt {attempt})...");
                }
                flotilla_client::reconnect::ReconnectNotice::Retry { error, delay, .. } => {
                    eprintln!("cannot connect to daemon: {error}; retrying in {:.1}s...", delay.as_secs_f64());
                }
            },
        )
        .await?;
        if let Err(error) = run_watch_connection(daemon, format).await {
            eprintln!("{error}; reconnecting...");
        }
    }
}

async fn run_watch_connection(daemon: std::sync::Arc<dyn DaemonHandle>, format: OutputFormat) -> Result<(), String> {
    // Subscribe before replay so events emitted between replay and the loop
    // are buffered rather than silently dropped.
    let mut rx = daemon.subscribe();

    // Replay current state so the user sees an initial snapshot for every
    // observed repository, matching how the TUI bootstraps.
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
    let mut command_id = daemon.execute(command.clone()).await?;

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
                if let CommandValue::CrewCompletionWaiting { retry_at, .. } = result {
                    if !matches!(command.action, CommandAction::CrewComplete { .. }) {
                        return Err("unexpected completion wait for a non-completion command".into());
                    }
                    // JSON mode retains one final stdout document; wait progress
                    // is diagnostic output, just like lag/disconnection notices.
                    eprintln!("{}", format_command_result(result));
                    // Remote clock skew may shift this sleep; the daemon checks
                    // its own cooldown and all gates again on every re-claim.
                    // This live wait lasts until evidence recovers or the caller
                    // cancels; dropping it leaves no queued retry in the daemon.
                    let delay = retry_at.signed_duration_since(Utc::now()).to_std().unwrap_or_default();
                    tokio::time::sleep(delay.max(Duration::from_millis(10))).await;
                    command_id = daemon.execute(command.clone()).await?;
                    continue;
                }
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
                    CommandValue::Error { .. } => Ok(result),
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
        CommandValue::Error { .. } => Ok(result),
        CommandValue::Cancelled => Err("command cancelled".into()),
        result => Ok(result),
    }
}

#[cfg(test)]
mod tests;
