use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use color_eyre::Result;
use flotilla_core::{config::ConfigStore, path_context::DaemonHostPath};
use flotilla_protocol::{commands::CommandValue, output::OutputFormat, Command, CommandAction};

use flotilla_tui::cli::args::{ArtifactSubCommand, Cli, CliPaths, ResourceSubCommand, ResourceWatchArgs};

use super::attach::{run_attach_plan, stamp_pane_identity};
use super::daemon::{connect_cli_socket, connect_daemon, host_daemon_socket_required, reset_sigpipe};
use super::manifest::run_manifest_resolution;
use super::targets::{
    inject_repo_context, resolve_command_repositories, resolve_environment_target, resolve_host_target, resolve_optional_host_node,
    set_context_repo,
};
use crate::resource_validate;

pub(crate) async fn run_artifact_command(cli: &Cli, command: ArtifactSubCommand, format: OutputFormat) -> Result<()> {
    let CliPaths { config_dir, state_dir, socket_path } = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let remote = cli.remote_daemon().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let daemon = connect_cli_socket(
        remote.as_ref(),
        &socket_path,
        &config_dir,
        &state_dir,
        host_daemon_socket_required(std::env::var_os(flotilla_core::providers::environment::CONTAINED_DAEMON_REQUIRED_ENV).as_deref()),
    )
    .await
    .map_err(|error| color_eyre::eyre::eyre!(error))?;
    match command {
        ArtifactSubCommand::Put { kind, about, summary, file } => {
            if about.is_none() && kind != "decision-ledger" {
                return Err(color_eyre::eyre::eyre!("--about is required except for decision-ledger"));
            }
            let source_path = tokio::fs::canonicalize(&file).await?;
            let media_type = match file.extension().and_then(|value| value.to_str()).unwrap_or("") {
                "json" => "application/json",
                "md" => "text/markdown",
                "html" | "htm" => "text/html",
                "txt" | "log" => "text/plain",
                "yaml" | "yml" => "application/yaml",
                _ => "application/octet-stream",
            };
            let summary = summary.into_iter().collect();
            let (address, digest, view_url) = daemon
                .artifact_put(kind, about.unwrap_or_default(), summary, media_type.to_string(), source_path)
                .await
                .map_err(|error| color_eyre::eyre::eyre!(error))?;
            match format {
                OutputFormat::Json => {
                    let mut value = serde_json::json!({"address": address, "digest": digest});
                    if let Some(url) = view_url {
                        value["view_url"] = url.into();
                    }
                    println!("{value}");
                }
                OutputFormat::Human => {
                    println!("{address}\n{digest}");
                    if let Some(url) = view_url {
                        println!("{url}");
                    }
                }
            }
        }
        ArtifactSubCommand::Get { reference, output } => {
            let path = output.unwrap_or_else(|| PathBuf::from(reference.rsplit('/').next().unwrap_or(&reference)));
            let destination = if path.is_absolute() { path.clone() } else { std::env::current_dir()?.join(&path) };
            let (size, view_url) =
                daemon.artifact_get(reference.clone(), destination).await.map_err(|error| color_eyre::eyre::eyre!(error))?;
            match format {
                OutputFormat::Json => {
                    let mut value = serde_json::json!({"path": path, "size": size, "address": reference});
                    if let Some(url) = view_url {
                        value["view_url"] = url.into();
                    }
                    println!("{value}");
                }
                OutputFormat::Human => {
                    println!("{}\n{reference}", path.display());
                    if let Some(url) = view_url {
                        println!("{url}");
                    }
                }
            }
        }
        ArtifactSubCommand::List { convoy, kind, about } => {
            let items = daemon.artifact_list(convoy, kind, about).await.map_err(|error| color_eyre::eyre::eyre!(error))?;
            match format {
                OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&items)?),
                OutputFormat::Human => {
                    for item in items {
                        println!(
                            "artifact/{}\t{}\t{}\t{}\t{}\t{}\t{}",
                            item["metadata"]["name"].as_str().unwrap_or("?"),
                            item["spec"]["convoy"].as_str().unwrap_or("?"),
                            item["spec"]["producer"].as_str().unwrap_or("?"),
                            item["spec"]["kind"].as_str().unwrap_or("?"),
                            item["spec"]["subject"].as_str().unwrap_or("?"),
                            item["spec"]["digest"].as_str().unwrap_or("?"),
                            item["view_url"].as_str().unwrap_or(""),
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) async fn run_control_command(cli: &Cli, mut command: Command, format: OutputFormat) -> Result<()> {
    use std::io::IsTerminal;

    reset_sigpipe();
    let convoy_auto_attach = match &command.action {
        CommandAction::ConvoyStart { intent } => intent.auto_attach,
        _ => flotilla_protocol::ConvoyAutoAttach::Never,
    };
    let daemon = connect_daemon(cli).await?;
    if let Err(message) = resolve_command_repositories(&*daemon, &mut command).await {
        exit_command_error(message, format);
    }
    let result = match flotilla_tui::cli::run_command(&*daemon, command, format).await {
        Ok(result) => result,
        Err(message) => exit_command_error(message, format),
    };
    if matches!(result, CommandValue::Error { .. } | CommandValue::FleetPostInstall { failed: true, .. }) {
        std::process::exit(1);
    }
    if let CommandValue::ConvoyStarted { name, attach_plan: Some(plan), binding } = result {
        if should_exec_convoy_attach(format, std::io::stdin().is_terminal(), convoy_auto_attach) {
            stamp_pane_identity(&name, binding.as_ref()).await;
            return run_attach_plan(&plan);
        }
    }
    Ok(())
}

fn should_exec_convoy_attach(format: OutputFormat, stdin_is_terminal: bool, auto_attach: flotilla_protocol::ConvoyAutoAttach) -> bool {
    matches!(format, OutputFormat::Human)
        && match auto_attach {
            flotilla_protocol::ConvoyAutoAttach::Default => stdin_is_terminal,
            flotilla_protocol::ConvoyAutoAttach::Always => true,
            flotilla_protocol::ConvoyAutoAttach::Never => false,
        }
}

fn exit_command_error(message: String, format: OutputFormat) -> ! {
    match format {
        OutputFormat::Human => eprintln!("error: {message}"),
        OutputFormat::Json => println!("{}", flotilla_protocol::output::json_pretty(&CommandValue::Error { message })),
    }
    std::process::exit(1);
}

fn charter_record_explanation(object: &serde_json::Value) -> serde_json::Value {
    let annotations = &object["metadata"]["annotations"];
    serde_json::json!({
        "kind": object["kind"], "namespace": object["metadata"]["namespace"], "name": object["metadata"]["name"],
        "charter": {
            "source": annotations["flotilla.work/manifest-source"].as_str()
                .or_else(|| annotations["flotilla.work/source-repository"].as_str()),
            "path": annotations["flotilla.work/manifest-path"].as_str()
                .or_else(|| annotations["flotilla.work/source-entry-path"].as_str()),
            "revision": annotations["flotilla.work/manifest-revision"].as_str()
                .or_else(|| annotations["flotilla.work/source-commit"].as_str()),
            "root": annotations["flotilla.work/manifest-reconciler-root"],
            "verification_inputs": annotations[flotilla_core::ops_entry::VERIFICATION_PROVENANCE_ANNOTATION].as_str()
                .and_then(|encoded| serde_json::from_str::<serde_json::Value>(encoded).ok()),
        },
    })
}

pub(crate) async fn run_resource_command(cli: &Cli, command: ResourceSubCommand, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let explain = matches!(&command, ResourceSubCommand::Explain(_));
    match command {
        ResourceSubCommand::Validate { path, from_daemon, host, skill_catalog, skill_sources, skill_probe_tokens } => {
            if from_daemon {
                let paths = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
                let local_roots = if host.is_none() {
                    let config = ConfigStore::new(DaemonHostPath::new(&paths.config_dir), DaemonHostPath::new(&paths.state_dir));
                    Some(
                        config
                            .load_observation_roots()
                            .map_err(|error| color_eyre::eyre::eyre!(error))?
                            .into_iter()
                            .map(|path| path.into_path_buf())
                            .collect::<Vec<_>>(),
                    )
                } else {
                    None
                };
                let socket = if let Some(host) = &host {
                    if host.is_empty() || host.contains(['/', '\\', '\0']) || host == "." || host == ".." {
                        return Err(color_eyre::eyre::eyre!("invalid peer host name: {host}"));
                    }
                    paths.state_dir.join("peers").join(format!("{host}.sock"))
                } else {
                    paths.socket_path
                };
                #[cfg(unix)]
                let result = resource_validate::validate_daemon_with_options(
                    &socket,
                    local_roots.as_deref(),
                    skill_catalog.as_deref(),
                    &resource_validate::frozen::ProbeOptions {
                        sources: skill_sources.or_else(|| skill_catalog.as_ref().and_then(|path| path.parent().map(Path::to_path_buf))),
                        credential_tokens: resource_validate::frozen::load_tokens(skill_probe_tokens.as_deref())?,
                    },
                )
                .await;
                #[cfg(not(unix))]
                let result = {
                    if skill_sources.is_some() || skill_probe_tokens.is_some() {
                        return Err(color_eyre::eyre::eyre!("frozen-reference daemon validation requires Unix"));
                    }
                    resource_validate::validate_daemon(&socket, local_roots.as_deref(), skill_catalog.as_deref()).await
                };
                result.map(|_| ()).map_err(|error| {
                    color_eyre::eyre::eyre!("resource validation on {}: {error:#}", host.as_deref().unwrap_or("local host"))
                })
            } else {
                resource_validate::validate_path(&path.expect("clap requires path without --from-daemon"), skill_catalog.as_deref())
            }
        }
        ResourceSubCommand::List(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            let daemon = connect_daemon(cli).await?;
            let mut response = flotilla_client::resource::ResourceClient::new(Arc::clone(&daemon))
                .list(
                    flotilla_client::resource::ResourceListRequest::builder()
                        .kind(args.kind.clone())
                        .namespace(args.namespace.clone())
                        .maybe_node_id(node_id.clone())
                        .include_replicas(args.include_replicas || !args.local_only)
                        .build(),
                )
                .await
                .map_err(|e| color_eyre::eyre::eyre!(e))?;
            if let Some(project) = args.project.as_deref() {
                let kind = response.plural.as_str();
                if !matches!(kind, "convoys" | "vessels" | "terminalsessions" | "environments" | "checkouts") {
                    return Err(color_eyre::eyre::eyre!("--project is unsupported for resource kind `{kind}`"));
                }
                let names: std::collections::HashSet<_> = if kind == "convoys" {
                    Default::default()
                } else {
                    flotilla_client::resource::ResourceClient::new(Arc::clone(&daemon))
                        .list(
                            flotilla_client::resource::ResourceListRequest::builder()
                                .kind("convoys".to_string())
                                .namespace(args.namespace.clone())
                                .maybe_node_id(node_id.clone())
                                .include_replicas(args.include_replicas || !args.local_only)
                                .build(),
                        )
                        .await
                        .map_err(|e| color_eyre::eyre::eyre!(e))?
                        .records
                        .into_iter()
                        .filter_map(|record| record.object)
                        .filter(|object| object["spec"]["project_ref"].as_str() == Some(project))
                        .filter_map(|object| object["metadata"]["name"].as_str().map(ToOwned::to_owned))
                        .collect()
                };
                response.records.retain(|record| {
                    record.object.as_ref().is_some_and(|object| {
                        if kind == "convoys" {
                            return object["spec"]["project_ref"].as_str() == Some(project);
                        }
                        let convoy = object["spec"]["convoy_ref"]
                            .as_str()
                            .or_else(|| object["metadata"]["labels"][flotilla_resources::CONVOY_LABEL].as_str());
                        convoy.is_some_and(|name| names.contains(name))
                            || object["metadata"]["labels"][flotilla_resources::PROJECT_LABEL].as_str() == Some(project)
                    })
                });
            }
            print_resource_read(response)
        }
        ResourceSubCommand::Get(args) | ResourceSubCommand::Explain(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            let daemon = connect_daemon(cli).await?;
            let response = flotilla_client::resource::ResourceClient::new(Arc::clone(&daemon))
                .get(
                    flotilla_client::resource::ResourceGetRequest::builder()
                        .kind(args.kind)
                        .name(args.name)
                        .namespace(args.namespace)
                        .maybe_node_id(node_id.clone())
                        .build(),
                )
                .await
                .map_err(|e| color_eyre::eyre::eyre!(e))?;
            if explain {
                let explanations: Vec<_> =
                    response.records.iter().filter_map(|record| record.object.as_ref()).map(charter_record_explanation).collect();
                if format == OutputFormat::Json {
                    println!("{}", flotilla_protocol::output::json_pretty(&explanations));
                } else {
                    for explanation in explanations {
                        println!("{}", flotilla_protocol::output::json_pretty(&explanation));
                    }
                }
                Ok(())
            } else {
                print_resource_read(response)
            }
        }
        ResourceSubCommand::ReconcileNow(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            run_control_command(
                cli,
                Command {
                    node_id,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::ResourceReconcileNow { namespace: args.namespace, kind: args.kind, name: args.name },
                },
                format,
            )
            .await
        }
        ResourceSubCommand::Apply(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            let raw = std::fs::read_to_string(&args.file)
                .map_err(|error| color_eyre::eyre::eyre!("read resource document {}: {error}", args.file.display()))?;
            let document: serde_json::Value = serde_yml::from_str(&raw)
                .map_err(|error| color_eyre::eyre::eyre!("parse resource document {}: {error}", args.file.display()))?;
            run_control_command(
                cli,
                Command {
                    node_id,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::ResourceApply { namespace: args.namespace, document },
                },
                format,
            )
            .await
        }
        ResourceSubCommand::Sync(args) => run_manifest_resolution(cli, args, flotilla_protocol::ManifestResolution::Sync, format).await,
        ResourceSubCommand::Adopt(args) => run_manifest_resolution(cli, args, flotilla_protocol::ManifestResolution::Adopt, format).await,
        ResourceSubCommand::FailMessageBatch(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            run_control_command(
                cli,
                Command {
                    node_id,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::MessageFailBatch { namespace: args.namespace, name: args.name, reason: args.reason },
                },
                format,
            )
            .await
        }
        ResourceSubCommand::PatchStatus(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            let raw = std::fs::read_to_string(&args.file)
                .map_err(|error| color_eyre::eyre::eyre!("read status document {}: {error}", args.file.display()))?;
            let status: serde_json::Value = serde_yml::from_str(&raw)
                .map_err(|error| color_eyre::eyre::eyre!("parse status document {}: {error}", args.file.display()))?;
            run_control_command(
                cli,
                Command {
                    node_id,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::ResourceStatusPatch {
                        expected_resource_version: None,
                        namespace: args.namespace,
                        kind: args.kind,
                        name: args.name,
                        status,
                    },
                },
                format,
            )
            .await
        }
        ResourceSubCommand::Delete(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            run_control_command(
                cli,
                Command {
                    node_id,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::ResourceDelete {
                        namespace: args.namespace,
                        kind: args.kind,
                        name: args.name,
                        replica_origin: args.replica.map(flotilla_protocol::NodeId::new),
                    },
                },
                format,
            )
            .await
        }
        ResourceSubCommand::RemoveRemote(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            run_control_command(
                cli,
                Command {
                    node_id,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::RepositoryRemoteRemove { namespace: args.namespace, name: args.name, remote: args.remote },
                },
                format,
            )
            .await
        }
        ResourceSubCommand::DedupSweep(args) => {
            let daemon = connect_daemon(cli).await?;
            let report = flotilla_client::resource::ResourceClient::new(daemon)
                .dedup_single_home_records(&args.namespace)
                .await
                .map_err(color_eyre::eyre::Report::msg)?;
            match format {
                OutputFormat::Json => println!(
                    "{}",
                    flotilla_protocol::output::json_pretty(&serde_json::json!({
                        "inspected_roots": report.inspected_roots,
                        "duplicate_records": report.duplicate_records,
                        "deletions": report.deletions.iter().map(|deletion| serde_json::json!({
                            "kind": deletion.kind,
                            "name": deletion.name,
                            "deleted_root": deletion.deleted_root,
                            "home_root": deletion.home_root,
                        })).collect::<Vec<_>>(),
                    }))
                ),
                OutputFormat::Human => {
                    println!(
                        "inspected {} roots; found {} duplicated records; deleted {} non-home copies",
                        report.inspected_roots,
                        report.duplicate_records,
                        report.deletions.len()
                    );
                    for deletion in report.deletions {
                        println!(
                            "deleted {}/{} from root {} (home: {})",
                            deletion.kind, deletion.name, deletion.deleted_root, deletion.home_root
                        );
                    }
                }
            }
            Ok(())
        }
        ResourceSubCommand::Watch(args) => run_resource_watch(cli, args, format).await,
    }
}

fn print_resource_read(response: flotilla_protocol::ResourceReadEnvelope) -> Result<()> {
    let value = serde_json::to_value(response).map_err(|error| color_eyre::eyre::eyre!("encode resource read: {error}"))?;
    println!("{}", format_resource_value(&value));
    Ok(())
}

fn format_resource_value(value: &serde_json::Value) -> String {
    // Resource JSON is an editable document in both modes. Display labels must
    // never replace canonical references or other stored values (#2803).
    flotilla_protocol::output::json_pretty(value)
}

async fn run_resource_watch(cli: &Cli, args: ResourceWatchArgs, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
    let daemon = connect_daemon(cli).await?;
    let client = flotilla_client::resource::ResourceClient::new(daemon);
    let mut watch = client
        .watch(
            flotilla_client::resource::ResourceWatchRequest::builder()
                .kind(args.kind)
                .namespace(args.namespace)
                .maybe_name(args.name)
                .maybe_node_id(node_id)
                .include_replicas(args.include_replicas)
                .maybe_cursor(args.from_cursor)
                .build(),
        )
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                watch.cancel().await.map_err(|e| color_eyre::eyre::eyre!(e))?;
                return Ok(());
            }
            event = watch.next() => {
                match event.map_err(|e| color_eyre::eyre::eyre!(e))? {
                    Some(response) => print_resource_watch_event(&response, format),
                    None => return Ok(()),
                }
            }
        }
    }
}

fn print_resource_watch_event(response: &flotilla_protocol::ResourceReadEnvelope, format: OutputFormat) {
    match format {
        OutputFormat::Json => println!("{}", flotilla_protocol::output::json_line(response)),
        OutputFormat::Human => {
            println!("{}", flotilla_protocol::output::json_pretty(response));
        }
    }
}

fn confirm_command(
    command: &mut Command,
    interactive: bool,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> Result<bool, String> {
    let CommandAction::MergeChangeRequest { id, confirmed } = &mut command.action else {
        return Ok(true);
    };
    if *confirmed {
        return Ok(true);
    }
    if !interactive {
        return Err("merging a change request non-interactively requires --yes".to_string());
    }

    write!(output, "Merge change request {id} using squash? [y/N] ").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())?;
    let mut response = String::new();
    input.read_line(&mut response).map_err(|error| error.to_string())?;
    if matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        *confirmed = true;
        Ok(true)
    } else {
        writeln!(output, "Merge cancelled.").map_err(|error| error.to_string())?;
        Ok(false)
    }
}

async fn run_confirmed_control_command(cli: &Cli, mut command: Command, format: OutputFormat) -> Result<()> {
    use std::io::IsTerminal;

    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut output = std::io::stderr().lock();
    if !confirm_command(&mut command, interactive, &mut input, &mut output).map_err(color_eyre::eyre::Report::msg)? {
        return Ok(());
    }
    run_control_command(cli, command, format).await
}

pub(crate) async fn dispatch(resolved: flotilla_commands::Resolved, cli: &Cli, format: OutputFormat) -> Result<()> {
    use flotilla_commands::{resolved::HostQueryKind, RepoContext, Resolved};
    reset_sigpipe();
    match resolved {
        Resolved::HostQuery { subject, kind } => {
            let (environment_id, node_id) = resolve_host_target(cli, &subject).await?;
            let action = match kind {
                HostQueryKind::Status => CommandAction::QueryHostStatus { target_environment_id: environment_id },
                HostQueryKind::Providers => CommandAction::QueryHostProviders { target_environment_id: environment_id },
            };
            run_confirmed_control_command(
                cli,
                Command { node_id: Some(node_id), provisioning_target: None, context_repo: None, action },
                format,
            )
            .await
        }
        Resolved::Ready(cmd) => run_confirmed_control_command(cli, cmd, format).await,
        Resolved::NeedsContext { mut command, repo, host } => {
            match repo {
                RepoContext::None => {}
                RepoContext::Required => inject_repo_context(&mut command, cli)?,
                RepoContext::Inferred => set_context_repo(&mut command, cli),
            }
            if command.node_id.is_none() {
                match host {
                    flotilla_commands::HostResolution::Explicit(subject) => {
                        let (_environment_id, node_id) = resolve_host_target(cli, &subject).await?;
                        command.node_id = Some(node_id);
                        command.provisioning_target = Some(flotilla_protocol::ProvisioningTarget::Host { host: subject });
                    }
                    flotilla_commands::HostResolution::ExplicitEnvironment(environment_id) => {
                        let (target, node_id) = resolve_environment_target(cli, &environment_id).await?;
                        command.node_id = Some(node_id);
                        command.provisioning_target = Some(target);
                    }
                    _ => {}
                }
            }
            run_confirmed_control_command(cli, command, format).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_tui::cli::args::SubCommand;

    use clap::Parser;
    use flotilla_protocol::{HostName, NodeId};
    #[test]
    fn default_convoy_start_does_not_auto_attach_non_interactively() {
        assert!(!should_exec_convoy_attach(
            flotilla_protocol::output::OutputFormat::Human,
            false,
            flotilla_protocol::ConvoyAutoAttach::Default,
        ));
        assert!(should_exec_convoy_attach(
            flotilla_protocol::output::OutputFormat::Human,
            true,
            flotilla_protocol::ConvoyAutoAttach::Default,
        ));
        assert!(should_exec_convoy_attach(
            flotilla_protocol::output::OutputFormat::Human,
            false,
            flotilla_protocol::ConvoyAutoAttach::Always,
        ));
        assert!(!should_exec_convoy_attach(
            flotilla_protocol::output::OutputFormat::Json,
            true,
            flotilla_protocol::ConvoyAutoAttach::Always,
        ));
    }

    // Glue: resource explain projects applier-owned provenance from both
    // authored record formats and parses with the same addressing as get.
    #[test]
    fn resource_explain_reports_charter_revision() {
        let cli = Cli::try_parse_from(["flotilla", "resource", "explain", "projects", "app"]).expect("explain parses");
        assert!(matches!(cli.command, Some(SubCommand::Resource { command: ResourceSubCommand::Explain(_) })));
        for (source, path, revision) in
            [("manifest-source", "manifest-path", "manifest-revision"), ("source-repository", "source-entry-path", "source-commit")]
        {
            let annotations = serde_json::json!({ format!("flotilla.work/{source}"): "repo", format!("flotilla.work/{path}"): "input.yaml", format!("flotilla.work/{revision}"): "commit-123" });
            let explanation = super::charter_record_explanation(
                &serde_json::json!({ "kind": "Project", "metadata": { "name": "app", "namespace": "flotilla", "annotations": annotations }}),
            );
            assert_eq!(explanation["charter"]["revision"], "commit-123");
            assert_eq!(explanation["charter"]["path"], "input.yaml");
            assert_eq!(explanation["charter"]["source"], "repo");
        }
    }

    #[cfg(unix)]
    async fn cli_resource_document(
        daemon: &dyn flotilla_core::daemon::DaemonHandle,
        node_id: Option<NodeId>,
        kind: &str,
        name: &str,
    ) -> serde_json::Value {
        let result = daemon
            .execute_query(
                flotilla_protocol::Command::builder()
                    .maybe_node_id(node_id)
                    .action(flotilla_protocol::CommandAction::QueryResourceGet {
                        namespace: "flotilla".into(),
                        kind: kind.into(),
                        name: name.into(),
                    })
                    .build(),
                uuid::Uuid::new_v4(),
            )
            .await
            .expect("resource get");
        let CommandValue::ResourceRead(response) = result else { panic!("resource read: {result:?}") };
        let value = serde_json::to_value(response).expect("read envelope");
        let output = super::format_resource_value(&value);
        let rendered: serde_json::Value = serde_yml::from_str(&output).expect("parse CLI get output");
        // All fields, including metadata, status and nested references, stay canonical.
        assert_eq!(rendered, value);
        let object = &rendered["records"][0]["object"];
        serde_json::json!({
            "apiVersion": object["apiVersion"], "kind": object["kind"],
            "metadata": { "name": object["metadata"]["name"], "namespace": object["metadata"]["namespace"] },
            "spec": object["spec"]
        })
    }

    #[cfg(unix)]
    async fn cli_apply_document(
        daemon: &dyn flotilla_core::daemon::DaemonHandle,
        node_id: Option<NodeId>,
        document: serde_json::Value,
    ) -> CommandValue {
        use flotilla_protocol::{Command, CommandAction, DaemonEvent};
        // Match resource apply's file parser: JSON is decoded through serde_yml.
        let raw = serde_json::to_string(&document).expect("doc.json");
        let document = serde_yml::from_str(&raw).expect("CLI apply file parser");
        let mut events = daemon.subscribe();
        let id = daemon
            .execute(
                Command::builder()
                    .maybe_node_id(node_id)
                    .action(CommandAction::ResourceApply { namespace: "flotilla".into(), document })
                    .build(),
            )
            .await
            .expect("dispatch resource apply");
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if let DaemonEvent::CommandFinished { command_id, result, .. } = events.recv().await.expect("command event") {
                    if command_id == id {
                        break result;
                    }
                }
            }
        })
        .await
        .expect("apply result")
    }

    #[cfg(unix)]
    async fn cli_round_trip_host_reference<T: flotilla_resources::Resource>(
        backend: &flotilla_resources::ResourceBackend,
        daemon: &dyn flotilla_core::daemon::DaemonHandle,
        node_id: Option<NodeId>,
        spec: &T::Spec,
    ) {
        let name = format!("round-trip-{}", T::API_PATHS.kind);
        let resources = backend.using::<T>("flotilla");
        resources
            .create(&flotilla_resources::InputMeta::builder().name(name.clone()).build(), spec)
            .await
            .expect("seed host-reference resource");
        let document = cli_resource_document(daemon, node_id.clone(), T::API_PATHS.kind, &name).await;
        let result = cli_apply_document(daemon, node_id, document).await;
        assert!(matches!(result, CommandValue::ResourceObject(_)), "round trip {}: {result:?}", T::API_PATHS.kind);
        assert_eq!(
            serde_json::to_value(resources.get(&name).await.expect("read back").spec).expect("stored spec"),
            serde_json::to_value(spec).expect("original spec")
        );
    }

    // #2803: get output is an editable resource document. Changing only memory
    // policy must retain canonical host_ref and its loop owner, locally and routed.
    #[cfg(unix)]
    async fn placement_policy_cli_apply_scenario(target: u8, percent: u8) {
        use std::sync::Arc;

        use flotilla_core::{config::ConfigStore, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery};
        use flotilla_daemon::server::test_support::spawn_in_memory_request_topology_stateful;
        use flotilla_resources::{
            Environment, EnvironmentSpec, FulfilmentKind, HostDirectEnvironmentSpec, InputMeta, PlacementPolicy, PlacementPolicySpec,
        };
        let leader_temp = tempfile::tempdir().expect("leader config");
        let follower_temp = tempfile::tempdir().expect("follower config");
        std::fs::write(leader_temp.path().join("daemon.toml"), "machine_id = \"cli-apply-leader\"\n").expect("leader identity");
        std::fs::write(follower_temp.path().join("daemon.toml"), "machine_id = \"cli-apply-follower\"\n").expect("follower identity");
        let leader = InProcessDaemon::new(
            vec![],
            Arc::new(ConfigStore::with_base(leader_temp.path())),
            fake_discovery(false),
            HostName::new(if target == 2 { "kiwi" } else { "feta" }),
        )
        .await;
        let follower = InProcessDaemon::new(
            vec![],
            Arc::new(ConfigStore::with_base(follower_temp.path())),
            fake_discovery(false),
            HostName::new("feta"),
        )
        .await;
        let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("router");
        let home = if target == 2 { &topology.follower } else { &topology.leader };
        let node_id = (target != 0).then(|| home.node_id().clone());
        let host_ref = home.local_host_id().expect("host id").to_string();
        let name = "docker-crew-image-feta";
        let spec: PlacementPolicySpec = serde_json::from_value(serde_json::json!({
            "pool": "cleat", "docker_per_vessel": {
                "host_ref": host_ref, "image": "crew:latest", "agent_adapters": ["codex"],
                "checkout": { "worktree_on_host_and_mount": { "mount_path": "/workspace" } }
            }
        }))
        .expect("policy");
        let policies = home.resource_backend().using::<PlacementPolicy>("flotilla");
        policies.create(&InputMeta::builder().name(name.into()).build(), &spec).await.expect("seed policy");
        // A same-named caller record makes using the wrong store observable.
        if target == 2 {
            let mut decoy = spec.clone();
            let decoy_docker = decoy.docker_per_vessel.as_mut().expect("docker");
            decoy_docker.host_ref = topology.leader.local_host_id().expect("caller id").to_string();
            decoy_docker.memory_policy.host_memory_percent = decoy_percent(percent);
            topology
                .leader
                .resource_backend()
                .using::<PlacementPolicy>("flotilla")
                .create(&InputMeta::builder().name(name.into()).build(), &decoy)
                .await
                .expect("caller policy");
        }
        let mut document = cli_resource_document(&*topology.client, node_id.clone(), "placementpolicy", name).await;
        document["spec"]["docker_per_vessel"]["memory_policy"]["host_memory_percent"] = percent.into();
        for change_host in [false, true] {
            let mut attempted = document.clone();
            if change_host {
                attempted["spec"]["docker_per_vessel"]["host_ref"] = "other-host".into();
            }
            let result = cli_apply_document(&*topology.client, node_id.clone(), attempted).await;
            if change_host {
                assert!(
                    matches!(result, CommandValue::Error { ref message }
                    if message.contains("spec.docker_per_vessel.host_ref") && message.contains("ReconcileLoop")),
                    "{result:?}"
                );
            } else {
                assert!(matches!(result, CommandValue::ResourceObject(_)), "unchanged host_ref must be accepted: {result:?}");
            }
            let stored = policies.get(name).await.expect("stored policy").spec.docker_per_vessel.expect("docker");
            assert_eq!(stored.host_ref, host_ref);
            assert_eq!(stored.memory_policy.host_memory_percent, percent);
            if target == 2 {
                assert_eq!(
                    topology
                        .leader
                        .resource_backend()
                        .using::<PlacementPolicy>("flotilla")
                        .get(name)
                        .await
                        .expect("caller policy")
                        .spec
                        .docker_per_vessel
                        .expect("docker")
                        .memory_policy
                        .host_memory_percent,
                    decoy_percent(percent)
                );
            }
        }
        // Other kinds carrying host refs must also round-trip through the same
        // read envelope, CLI renderer and local/routed apply-command handler.
        cli_round_trip_host_reference::<Environment>(
            &home.resource_backend(),
            &*topology.client,
            node_id.clone(),
            &EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: host_ref.clone(), repo_default_dir: "/workspace".into() }),
                docker: None,
            },
        )
        .await;
        cli_round_trip_host_reference::<FulfilmentKind>(
            &home.resource_backend(),
            &*topology.client,
            node_id,
            &flotilla_resources::FulfilmentKindSpec::from_policy(&spec, "linux").expect("fulfilment kind"),
        )
        .await;
    }

    /// A caller-store percentage that always differs from the applied one, so a
    /// write to the wrong store is observable for every drawn percentage.
    #[cfg(unix)]
    fn decoy_percent(applied: u8) -> u8 {
        if applied == 50 {
            51
        } else {
            50
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn placement_policy_cli_apply_pinned_rows() {
        for target in 0..=2 {
            placement_policy_cli_apply_scenario(target, 80).await;
        }
    }

    #[cfg(unix)]
    #[hegel::test]
    fn generated_placement_policy_cli_apply(tc: hegel::TestCase) {
        // The repo's hegel.toml budgets 12 cases in development/CI, 40 nightly.
        // Hostless local, explicit local, routed peer; valid percentages including
        // min/max, unchanged 50, and the live request 80. Both CLI modes share the
        // same format-independent renderer, so no separate format dimension remains.
        let target = tc.draw(hegel::generators::integers::<u8>().min_value(0).max_value(2));
        let percent = tc.draw(hegel::generators::integers::<u8>().min_value(1).max_value(100));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(placement_policy_cli_apply_scenario(target, percent));
    }

    #[test]
    fn interactive_merge_confirmation_marks_command_confirmed() {
        let mut command = flotilla_protocol::Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: flotilla_protocol::CommandAction::MergeChangeRequest { id: "42".into(), confirmed: false },
        };
        let mut input = std::io::Cursor::new(b"yes\n");
        let mut output = Vec::new();

        let proceed = confirm_command(&mut command, true, &mut input, &mut output).expect("confirmation prompt");

        assert!(proceed);
        assert!(matches!(command.action, flotilla_protocol::CommandAction::MergeChangeRequest { confirmed: true, .. }));
        assert_eq!(String::from_utf8(output).expect("utf8 prompt"), "Merge change request 42 using squash? [y/N] ");
    }

    #[test]
    fn interactive_merge_decline_does_not_dispatch() {
        let mut command = flotilla_protocol::Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: flotilla_protocol::CommandAction::MergeChangeRequest { id: "42".into(), confirmed: false },
        };
        let mut input = std::io::Cursor::new(b"n\n");
        let mut output = Vec::new();

        assert!(!confirm_command(&mut command, true, &mut input, &mut output).expect("confirmation prompt"));
        assert!(matches!(command.action, flotilla_protocol::CommandAction::MergeChangeRequest { confirmed: false, .. }));
        assert_eq!(String::from_utf8(output).expect("utf8 prompt"), "Merge change request 42 using squash? [y/N] Merge cancelled.\n");
    }

    #[test]
    fn non_interactive_merge_requires_yes_flag() {
        let mut command = flotilla_protocol::Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: flotilla_protocol::CommandAction::MergeChangeRequest { id: "42".into(), confirmed: false },
        };

        let error = confirm_command(&mut command, false, &mut std::io::empty(), &mut std::io::sink())
            .expect_err("non-interactive merge must require explicit confirmation");

        assert_eq!(error, "merging a change request non-interactively requires --yes");
    }
}
