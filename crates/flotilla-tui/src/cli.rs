use std::{collections::HashMap, fmt::Write as _, path::Path};

use chrono::{DateTime, Utc};
use comfy_table::{presets::UTF8_FULL_CONDENSED, Cell, Table};
use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{
    output::OutputFormat, Command, CommandValue, CrewListResponse, DaemonEvent, EnvironmentInfo, EnvironmentStatus, FleetHealthResponse,
    FleetHostStaleness, FleetListResponse, FleetObservationAgreement, FleetStaleness, HostProvidersResponse, HostStatusResponse, NodeInfo,
    PeerConnectionState, ProjectListResponse, RepoProvidersResponse, StatusResponse, StreamKey, TopologyResponse,
};

use crate::socket::SocketDaemon;

fn format_status_response_human(status: &StatusResponse) -> String {
    if status.repos.is_empty() {
        return "No repos tracked.\n".into();
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
            "Disk Free",
            "Sleep Inhibition",
            "Staleness",
            "Diagnosis",
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
                Cell::new(format_disk_free(host.disk_free_bytes)),
                Cell::new(format_sleep_inhibition(&host.sleep_inhibition)),
                Cell::new(row),
                Cell::new(diagnosis),
            ]);
        }
        format!("{table}\n")
    };
    output.push_str("\nDispatch queue:\n");
    output.push_str(&format_dispatch_queue_human(&response.dispatch_queue));
    output
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
            Cell::new(if project.conflicts.is_empty() { String::new() } else { format!("! {}", project.conflicts.join(", ")) }),
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
        table.set_header(vec!["Convoy", "Vessel", "Crew", "State", "Attention", "Host", "Placement", "Staleness"]);
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

    if response.replicas.iter().any(|replica| !replica.reachable || replica.skipped_records > 0) {
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec!["Replica", "Status", "Last Sync", "Generation"]);
        for replica in &response.replicas {
            if replica.reachable && replica.skipped_records == 0 {
                continue;
            }
            let parse_skew = || {
                let noun = if replica.skipped_records == 1 { "record" } else { "records" };
                format!(
                    "skipped {} {noun}: {}",
                    replica.skipped_records,
                    replica.first_parse_error.as_deref().unwrap_or("unknown parse error")
                )
            };
            let status = if !replica.reachable {
                let unreachable = replica.message.as_deref().unwrap_or("unreachable");
                if replica.skipped_records > 0 {
                    format!("{unreachable}; last sync {}", parse_skew())
                } else {
                    unreachable.to_string()
                }
            } else {
                parse_skew()
            };
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
    format!("Convoy: {}  Vessel: {} ({})\n{}\n", response.convoy, response.vessel, response.vessel_ref, table)
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

pub(crate) fn format_convoy_explanation_human(explanation: &flotilla_protocol::ConvoyExplanation) -> String {
    let mut output = format!("Convoy: {}/{}\nPhase: {}\n", explanation.namespace, explanation.convoy, explanation.phase);
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

    output.push_str("\nDecision ledgers:\n");
    if explanation.decision_ledgers.is_empty() {
        output.push_str("  (no settlement claims)\n");
    } else {
        for ledger in &explanation.decision_ledgers {
            if ledger.missing {
                let _ = writeln!(
                    output,
                    "  - {}/{} claimed_at={} MISSING (flagged; claim accepted)",
                    ledger.vessel,
                    ledger.role,
                    ledger.claimed_at.as_deref().unwrap_or("-")
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

    output.push_str("\nChange requests:\n");
    if explanation.change_requests.is_empty() {
        output.push_str("  (none)\n");
    } else {
        for request in &explanation.change_requests {
            let fields = request.fields.as_ref().map_or_else(|| "missing".to_string(), serde_json::Value::to_string);
            let _ = writeln!(
                output,
                "  - {} bound={} observed={} source={} observed_at={} freshness={:?}\n    fields={}",
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

    output.push_str("\nCrew delivery:\n");
    if explanation.crew_deliveries.is_empty() {
        output.push_str("  (none)\n");
    } else {
        for delivery in &explanation.crew_deliveries {
            let _ = writeln!(
                output,
                "  - {} role={} last_rung={} delivered_message={}",
                delivery.session,
                delivery.role,
                delivery.last_delivery_rung.as_deref().unwrap_or("not recorded"),
                delivery.delivered_message_id.as_deref().unwrap_or("-")
            );
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

/// Format a `CommandValue` as a short human-readable string.
fn format_command_result(result: &flotilla_protocol::commands::CommandValue) -> String {
    use flotilla_protocol::commands::CommandValue;
    match result {
        CommandValue::Ok => "ok".to_string(),
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
        CommandValue::RepoProviders(providers) => format_repo_providers_human(providers),
        // HostList remains a protocol-level query used by host/environment
        // target resolution; keep its formatter for direct query diagnostics
        // even though `host list` now presents the richer fleet-health view.
        CommandValue::HostList(hosts) => format_host_list_human(hosts),
        CommandValue::ProjectList(projects) => format_project_list_human(projects),
        CommandValue::DispatchQueue(queue) => format_dispatch_queue_human(queue),
        CommandValue::HostStatus(status) => format_host_status_human(status),
        CommandValue::HostProviders(providers) => format_host_providers_human(providers),
        CommandValue::FleetHealth(fleet) => format_fleet_health_human(fleet),
        CommandValue::FleetList(fleet) => format_fleet_list_human(fleet),
        CommandValue::CrewList(crew) => format_crew_list_human(crew),
        CommandValue::FleetReplicaSnapshot(_) => "fleet replica snapshot".to_string(),
        CommandValue::DaemonLogs { lines } => lines.join("\n"),
        CommandValue::ConvoyExplanation(explanation) => format_convoy_explanation_human(explanation),
        CommandValue::ResourceRead(response) => flotilla_protocol::output::json_pretty(response),
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
        CommandValue::IssuePage(page) => format!("issue page: {} items, has_more={}", page.items.len(), page.has_more),
        CommandValue::IssuesByIds { items } => format!("issues by ids: {} items", items.len()),
        CommandValue::ConvoyCreated { name } => format!("convoy created: {name}"),
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
        DaemonEvent::RepoSnapshot(snapshot) => {
            format!(
                "[repo]     {}: provider snapshot (seq {})",
                repo_label(snapshot.repo.as_deref(), &snapshot.repo_identity),
                snapshot.seq
            )
        }
        DaemonEvent::RepoDelta(delta) => {
            format!("[repo]     {}: provider delta (seq {})", repo_label(delta.repo.as_deref(), &delta.repo_identity), delta.seq)
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
        DaemonEvent::RepoSnapshot(_)
        | DaemonEvent::RepoDelta(_)
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

pub async fn run_watch(socket_path: &Path, format: OutputFormat) -> Result<(), String> {
    loop {
        let daemon = flotilla_client::reconnect::connect_with_retry(
            || SocketDaemon::connect(socket_path),
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
