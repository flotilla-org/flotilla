use std::sync::Arc;

use color_eyre::Result;
use flotilla_protocol::{commands::CommandValue, output::OutputFormat, Command, CommandAction};

use flotilla_tui::cli::args::{Cli, EnsureSubCommand, ManifestSubCommand, ResourceManifestResolutionArgs};

use super::daemon::{cli_surface_from, connect_daemon, reset_sigpipe};
use super::resource::run_control_command;
use super::targets::resolve_optional_host_node;

pub(crate) async fn run_manifest_command(cli: &Cli, command: ManifestSubCommand, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let ManifestSubCommand::Status { root, namespace, host } = command;
    let node_id = resolve_optional_host_node(cli, host.as_deref()).await?;
    let daemon = connect_daemon(cli).await?;
    let response = flotilla_client::resource::ResourceClient::new(Arc::clone(&daemon))
        .list(
            flotilla_client::resource::ResourceListRequest::builder()
                .kind("manifestroots".to_string())
                .namespace(namespace)
                .maybe_node_id(node_id)
                .include_replicas(true)
                .build(),
        )
        .await
        .map_err(|error| color_eyre::eyre::eyre!(error))?;
    let mut rows = Vec::new();
    let mut found = false;
    for record in response.records {
        let Some(object) = record.object else { continue };
        let name = object["metadata"]["name"].as_str().unwrap_or_default();
        if root.as_deref().is_some_and(|root| root != name) {
            continue;
        }
        found = true;
        let spec: flotilla_resources::ManifestRootSpec =
            serde_json::from_value(object["spec"].clone()).map_err(|error| color_eyre::eyre::eyre!("decode ManifestRoot spec: {error}"))?;
        let status: flotilla_resources::ManifestRootStatus = object
            .get("status")
            .filter(|value| !value.is_null())
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .map_err(|error| color_eyre::eyre::eyre!("decode ManifestRoot status: {error}"))?
            .unwrap_or_default();
        rows.push((
            serde_json::json!({
                "root": name, "binding": spec.binding, "applied_revision": status.applied_revision,
                "document": {"path": "<source>"},
                "state": {
                    "phase": if status.stalled.is_some() { "refused" } else if status.applied_revision.is_some() { "applied" } else { "pending" },
                    "reason": status.source_error.as_deref().or_else(|| status.stalled.as_ref().map(|stall| stall.evidence.as_str())),
                },
            }),
            None,
        ));
        for (key, state) in status.documents {
            let pending_resolution =
                spec.resolutions.get(&key).filter(|resolution| state.resolved_token.as_deref() != Some(resolution.token.as_str()));
            let resolution_action = spec
                .resolutions
                .get(&key)
                .filter(|resolution| state.resolved_token.as_deref() == Some(resolution.token.as_str()))
                .map(|resolution| resolution.action);
            rows.push((
                serde_json::json!({
                    "root": name,
                    "document": {"path": key.path, "kind": key.kind, "namespace": key.namespace, "name": key.name},
                    "state": state,
                    "pending_resolution": pending_resolution.map(|resolution| &resolution.action),
                }),
                resolution_action,
            ));
        }
    }
    if root.is_some() && !found {
        return Err(color_eyre::eyre::eyre!("ManifestRoot {} not found", root.unwrap_or_default()));
    }
    if format == OutputFormat::Json {
        println!("{}", flotilla_protocol::output::json_pretty(&rows.iter().map(|(row, _)| row).collect::<Vec<_>>()));
    } else {
        for (row, resolution_action) in rows {
            println!("{}", format_manifest_status_row(&row, resolution_action));
        }
    }
    Ok(())
}

fn format_manifest_status_row(row: &serde_json::Value, resolution_action: Option<flotilla_resources::ResolutionAction>) -> String {
    let key = &row["document"];
    let phase = row["state"]["phase"].as_str().unwrap_or("unknown");
    let reason = row["state"]["reason"].as_str().unwrap_or("");
    let reason =
        row["applied_revision"].as_str().map(|revision| format!("{reason} (applied {revision})")).unwrap_or_else(|| reason.to_string());
    let pending = row["pending_resolution"].as_str().map(|action| format!("pending {action}")).unwrap_or_default();
    let reason = if resolution_action == Some(flotilla_resources::ResolutionAction::Adopt)
        && row["state"]["resolution_outcome"].get("failed").is_some()
    {
        format!("{reason}; adoption may have rewritten the source file; inspect it before retrying with a new token")
    } else {
        reason
    };
    format!(
        "{}\t{}\t{}/{}/{}\t{}\t{}\t{}",
        row["root"].as_str().unwrap_or_default(),
        key["path"].as_str().unwrap_or_default(),
        key["kind"].as_str().unwrap_or_default(),
        key["namespace"].as_str().unwrap_or_default(),
        key["name"].as_str().unwrap_or_default(),
        phase,
        pending,
        reason
    )
}

fn drifted_ensure_names(objects: impl IntoIterator<Item = serde_json::Value>) -> Result<std::collections::BTreeSet<String>> {
    let mut names = std::collections::BTreeSet::new();
    for object in objects {
        let ensure: flotilla_resources::K8sResourceObject<flotilla_resources::ConvoyEnsure> = serde_json::from_value(object)?;
        let ensure = flotilla_resources::ResourceObject::from_k8s_object(ensure)?;
        if ensure.status.as_ref().is_some_and(|status| status.config_drift.is_some()) {
            // Replicas share namespace/name identity. Authority routing resolves
            // the active generation and rejects ambiguous multiple admissions.
            names.insert(ensure.metadata.name);
        }
    }
    Ok(names)
}

pub(crate) async fn run_ensure_command(cli: &Cli, command: EnsureSubCommand, format: OutputFormat) -> Result<()> {
    use flotilla_client::resource::{ResourceClient, ResourceListRequest};
    let EnsureSubCommand::Roll { name, drifted, namespace, host } = command;
    let node_id = resolve_optional_host_node(cli, host.as_deref()).await?;
    let daemon = connect_daemon(cli).await?;
    let names = if drifted {
        let response = ResourceClient::new(daemon.clone())
            .list(
                ResourceListRequest::builder()
                    .kind("ConvoyEnsure".to_string())
                    .namespace(namespace.clone())
                    .maybe_node_id(node_id.clone())
                    .include_replicas(true)
                    .build(),
            )
            .await
            .map_err(|error| color_eyre::eyre::eyre!(error))?;
        drifted_ensure_names(response.records.into_iter().filter_map(|record| record.object))?
    } else {
        std::collections::BTreeSet::from([name.expect("clap requires a name without --drifted")])
    };
    let total = names.len();
    let mut errors = Vec::new();
    for name in names {
        let result = flotilla_tui::cli::run_command(
            &*daemon,
            Command {
                node_id: node_id.clone(),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyEnsureRoll { namespace: namespace.clone(), name: name.clone() },
            },
            format,
        )
        .await;
        match result {
            Ok(CommandValue::Error { message }) => errors.push(format!("{name}: {message}")),
            Err(message) => errors.push(format!("{name}: {message}")),
            Ok(_) => {}
        }
    }
    // A successful request can be a no-op or a driver handoff, not an admission.
    match format {
        OutputFormat::Human => println!("{} successful, {} failed", total - errors.len(), errors.len()),
        OutputFormat::Json => println!("{}", serde_json::json!({"successful": total - errors.len(), "failed": errors.len()})),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(color_eyre::eyre::eyre!(errors.join("\n")))
    }
}

pub(super) async fn run_manifest_resolution(
    cli: &Cli,
    args: ResourceManifestResolutionArgs,
    resolution: flotilla_protocol::ManifestResolution,
    format: OutputFormat,
) -> Result<()> {
    let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
    let principal =
        cli_surface_from(std::env::var("FLOTILLA_CREW_ROLE").ok().as_deref(), std::env::var("FLOTILLA_NAMESPACE").ok().as_deref())
            .principal_ref;
    let requested_by = format!("{}/{}", principal.namespace, principal.name);
    run_control_command(
        cli,
        Command {
            node_id,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ResourceManifestResolve {
                namespace: args.namespace,
                kind: args.kind,
                name: args.name,
                resolution,
                requested_by,
            },
        },
        format,
    )
    .await
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn drifted_batch_selection_decodes_typed_status_and_deduplicates_replicas() {
        use flotilla_resources::{ConvoyEnsure, ConvoyEnsureSpec, InputMeta};
        use flotilla_store::{InMemoryBackend, ResourceBackend};
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut ensure = backend
            .using::<ConvoyEnsure>("flotilla")
            .create(
                &InputMeta::builder().name("governor".to_string()).build(),
                &ConvoyEnsureSpec::builder()
                    .project_ref("demo".to_string())
                    .role("governor".to_string())
                    .workflow_ref("standing".to_string())
                    .repositories(vec![])
                    .build(),
            )
            .await
            .expect("create ensure");
        let healthy = serde_json::to_value(ensure.to_k8s_object()).expect("healthy record");
        ensure.status.get_or_insert_with(Default::default).config_drift =
            Some(flotilla_resources::ConvoyEnsureConfigDrift { changes: vec!["workflow".to_string()], observed_at: chrono::Utc::now() });
        let drifted = serde_json::to_value(ensure.to_k8s_object()).expect("drifted record");
        assert_eq!(
            super::drifted_ensure_names([healthy, drifted.clone(), drifted.clone()]).expect("typed selection"),
            std::collections::BTreeSet::from(["governor".to_string()])
        );
        let mut malformed = drifted;
        malformed["status"]["config_drift"] = serde_json::json!("invalid drift condition");
        assert!(super::drifted_ensure_names([malformed]).is_err(), "malformed records must not silently disappear");
    }

    #[test]
    fn manifest_status_warns_that_failed_adoption_may_have_written_source() {
        let row = serde_json::json!({
            "root": "manifest-123",
            "document": {"path": "policy.yaml", "kind": "PlacementPolicy", "namespace": "flotilla", "name": "adopt-me"},
            "state": {
                "phase": "refused",
                "reason": "live spec changed while adopting",
                "resolved_token": "token-1",
                "resolution_outcome": {"failed": "live spec changed while adopting"}
            },
            "pending_resolution": null
        });

        assert_eq!(
            super::format_manifest_status_row(&row, Some(flotilla_resources::ResolutionAction::Adopt)),
            "manifest-123\tpolicy.yaml\tPlacementPolicy/flotilla/adopt-me\trefused\t\tlive spec changed while adopting; adoption may have rewritten the source file; inspect it before retrying with a new token"
        );
    }

    #[test]
    fn manifest_status_does_not_warn_for_failed_sync() {
        let row = serde_json::json!({
            "root": "manifest-123",
            "document": {"path": "policy.yaml", "kind": "PlacementPolicy", "namespace": "flotilla", "name": "adopt-me"},
            "state": {
                "phase": "refused",
                "reason": "live spec changed while syncing",
                "resolution_outcome": {"failed": "live spec changed while syncing"}
            },
            "pending_resolution": null
        });

        assert_eq!(
            super::format_manifest_status_row(&row, Some(flotilla_resources::ResolutionAction::Sync)),
            "manifest-123\tpolicy.yaml\tPlacementPolicy/flotilla/adopt-me\trefused\t\tlive spec changed while syncing"
        );
    }
}
