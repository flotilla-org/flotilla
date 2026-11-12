use std::sync::Arc;

use color_eyre::Result;
use flotilla_protocol::{commands::CommandValue, output::OutputFormat, Command, CommandAction};

use crate::cli::args::{Cli, EnsureSubCommand, ManifestSubCommand, ResourceManifestResolutionArgs};

use super::connection::{cli_surface_from, connect_daemon, reset_sigpipe};
use super::resource::run_control_command;
use super::targets::resolve_optional_host_node;

pub async fn run_manifest_command(cli: &Cli, command: ManifestSubCommand, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let ManifestSubCommand::Status { root, namespace, host } = command;
    let node_id = resolve_optional_host_node(cli, host.as_deref()).await?;
    let daemon = connect_daemon(cli).await?;
    let response = flotilla_client::resource::ResourceClient::new(Arc::clone(&daemon))
        .manifest_roots(namespace, node_id)
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
    let reason = match row["state"]["removed_in"].as_str() {
        Some(revision) if reason.is_empty() => format!("removed in {revision}"),
        Some(revision) => format!("removed in {revision}: {reason}"),
        None => reason.to_string(),
    };
    let reason = row["applied_revision"].as_str().map(|revision| format!("{reason} (applied {revision})")).unwrap_or(reason);
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

/// The ensures whose current generation (newest live convoy ensured from them) was admitted under
/// another configuration than they declare now. Drift is derived from the generation, never held.
fn drifted_ensure_names(
    ensures: impl IntoIterator<Item = serde_json::Value>,
    convoys: impl IntoIterator<Item = serde_json::Value>,
) -> Result<std::collections::BTreeSet<String>> {
    use flotilla_resources::{Convoy, ConvoyEnsure, K8sResourceObject, ResourceObject, ENSURED_FROM_ANNOTATION};
    let mut current = std::collections::BTreeMap::<String, ResourceObject<Convoy>>::new();
    for object in convoys {
        let convoy = ResourceObject::from_k8s_object(serde_json::from_value::<K8sResourceObject<Convoy>>(object)?)?;
        let Some(ensure) = convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION).cloned() else { continue };
        if convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            continue;
        }
        if current.get(&ensure).is_none_or(|held| held.spec.generation < convoy.spec.generation) {
            current.insert(ensure, convoy);
        }
    }
    let mut names = std::collections::BTreeSet::new();
    for object in ensures {
        let ensure = ResourceObject::from_k8s_object(serde_json::from_value::<K8sResourceObject<ConvoyEnsure>>(object)?)?;
        let Some(generation) = current.get(&ensure.metadata.name) else { continue };
        let admitted = generation.status.as_ref().and_then(|status| status.ensure_admission.as_ref());
        if !flotilla_resources::config_changes(&ensure.spec, admitted).is_empty() {
            // Replicas share namespace/name identity. Authority routing resolves
            // the active generation and rejects ambiguous multiple admissions.
            names.insert(ensure.metadata.name);
        }
    }
    Ok(names)
}

pub async fn run_ensure_command(cli: &Cli, command: EnsureSubCommand, format: OutputFormat) -> Result<()> {
    use flotilla_client::resource::{ResourceClient, ResourceListRequest};
    let EnsureSubCommand::Roll { name, drifted, namespace, host } = command;
    let node_id = resolve_optional_host_node(cli, host.as_deref()).await?;
    let daemon = connect_daemon(cli).await?;
    let names = if drifted {
        let client = ResourceClient::new(daemon.clone());
        let list = |kind: &str| {
            client.list(
                ResourceListRequest::builder()
                    .kind(kind.to_string())
                    .namespace(namespace.clone())
                    .maybe_node_id(node_id.clone())
                    .include_replicas(true)
                    .build(),
            )
        };
        let ensures = list("ConvoyEnsure").await.map_err(|error| color_eyre::eyre::eyre!(error))?;
        let convoys = list("Convoy").await.map_err(|error| color_eyre::eyre::eyre!(error))?;
        drifted_ensure_names(
            ensures.records.into_iter().filter_map(|record| record.object),
            convoys.records.into_iter().filter_map(|record| record.object),
        )?
    } else {
        std::collections::BTreeSet::from([name.expect("clap requires a name without --drifted")])
    };
    let total = names.len();
    let mut errors = Vec::new();
    for name in names {
        let result = crate::cli::run_command(
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

pub async fn run_manifest_resolution(
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
    use flotilla_store_memory::InMemoryBackend;

    // Behaviour (#3108): an ensure is drifted when its current generation, the newest live convoy
    // ensured from it, was admitted under another configuration; older or ended generations and
    // replica copies do not count twice, and a malformed record is an error, not a silent skip.
    #[tokio::test]
    async fn drifted_batch_selection_reads_each_roles_current_generation() {
        use flotilla_resources::{
            Convoy, ConvoyEnsure, ConvoyEnsureSpec, ConvoyPhase, ConvoySpec, InputMeta, ResourceObject, ENSURED_FROM_ANNOTATION,
        };
        use flotilla_store::ResourceBackend;
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let declared = |role: &str, workflow: &str| {
            ConvoyEnsureSpec::builder()
                .project_ref("demo".to_string())
                .role(role.to_string())
                .workflow_ref(workflow.to_string())
                .repositories(vec![])
                .build()
        };
        let mut ensures = Vec::new();
        for role in ["governor", "scout"] {
            let ensure = backend
                .using::<ConvoyEnsure>("flotilla")
                .create(&InputMeta::builder().name(role.to_string()).build(), &declared(role, "standing"))
                .await
                .expect("create ensure");
            ensures.push(serde_json::to_value(ensure.to_k8s_object()).expect("ensure record"));
        }
        let generation = |name: &str, ensure: &str, generation: u64, phase: ConvoyPhase, admitted: &ConvoyEnsureSpec| {
            let backend = backend.clone();
            let (name, ensure, admitted) = (name.to_string(), ensure.to_string(), admitted.clone());
            async move {
                let mut convoy: ResourceObject<Convoy> = backend
                    .using::<Convoy>("flotilla")
                    .create(
                        &InputMeta::builder()
                            .name(name)
                            .annotations(std::collections::BTreeMap::from([(ENSURED_FROM_ANNOTATION.to_string(), ensure)]))
                            .build(),
                        &ConvoySpec::builder().workflow_ref("standing".into()).generation(generation).build(),
                    )
                    .await
                    .expect("create convoy");
                let status = convoy.status.get_or_insert_with(Default::default);
                status.phase = phase;
                status.ensure_admission = Some(admitted);
                serde_json::to_value(convoy.to_k8s_object()).expect("convoy record")
            }
        };
        let convoys = vec![
            // The governor's current generation predates a workflow change; an ended newer one does not count.
            generation("governor-1", "governor", 1, ConvoyPhase::Active, &declared("governor", "older")).await,
            generation("governor-2", "governor", 2, ConvoyPhase::Failed, &declared("governor", "standing")).await,
            // The scout's current generation is its newest live one, admitted as declared.
            generation("scout-1", "scout", 1, ConvoyPhase::Active, &declared("scout", "older")).await,
            generation("scout-2", "scout", 2, ConvoyPhase::Active, &declared("scout", "standing")).await,
        ];
        let replicated = ensures.iter().chain(ensures.iter()).cloned();
        assert_eq!(
            super::drifted_ensure_names(replicated, convoys.clone()).expect("typed selection"),
            std::collections::BTreeSet::from(["governor".to_string()])
        );
        let mut malformed = convoys[0].clone();
        malformed["status"]["ensure_admission"] = serde_json::json!("invalid admission");
        assert!(super::drifted_ensure_names(ensures, [malformed]).is_err(), "malformed records must not silently disappear");
    }

    #[test]
    fn manifest_status_warns_that_failed_adoption_may_have_written_source() {
        let row = serde_json::json!({
            "root": "manifest-123",
            "document": {"path": "policy.yaml", "kind": "FulfilmentKind", "namespace": "flotilla", "name": "adopt-me"},
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
            "manifest-123\tpolicy.yaml\tFulfilmentKind/flotilla/adopt-me\trefused\t\tlive spec changed while adopting; adoption may have rewritten the source file; inspect it before retrying with a new token"
        );
    }

    // Behaviour (#3992): a removed document's row names the revision that removed it.
    #[test]
    fn manifest_status_names_the_revision_that_removed_a_document() {
        let row = |state: serde_json::Value| {
            serde_json::json!({
                "root": "manifest-123",
                "document": {"path": "grant.json", "kind": "CredentialGrant", "namespace": "flotilla", "name": "grant"},
                "state": state,
                "pending_resolution": null
            })
        };
        assert_eq!(
            super::format_manifest_status_row(&row(serde_json::json!({"phase": "deleted", "removed_in": "f451954"})), None),
            "manifest-123\tgrant.json\tCredentialGrant/flotilla/grant\tdeleted\t\tremoved in f451954"
        );
        let orphaned = serde_json::json!({"phase": "orphaned", "reason": "live spec edited", "removed_in": "f451954"});
        assert_eq!(
            super::format_manifest_status_row(&row(orphaned), None),
            "manifest-123\tgrant.json\tCredentialGrant/flotilla/grant\torphaned\t\tremoved in f451954: live spec edited"
        );
    }

    #[test]
    fn manifest_status_does_not_warn_for_failed_sync() {
        let row = serde_json::json!({
            "root": "manifest-123",
            "document": {"path": "policy.yaml", "kind": "FulfilmentKind", "namespace": "flotilla", "name": "adopt-me"},
            "state": {
                "phase": "refused",
                "reason": "live spec changed while syncing",
                "resolution_outcome": {"failed": "live spec changed while syncing"}
            },
            "pending_resolution": null
        });

        assert_eq!(
            super::format_manifest_status_row(&row, Some(flotilla_resources::ResolutionAction::Sync)),
            "manifest-123\tpolicy.yaml\tFulfilmentKind/flotilla/adopt-me\trefused\t\tlive spec changed while syncing"
        );
    }
}
