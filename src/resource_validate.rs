use std::path::{Path, PathBuf};

use color_eyre::{eyre::eyre, Result};
use flotilla_resources::{validate_resource_document, ReplicationClass, REGISTERED_RESOURCE_KINDS};
use serde::Deserialize;
use serde_json::Value;

/// Query JSON directly over the daemon's resource socket. The command protocol's
/// fingerprint deliberately rejects mixed generations during a fleet roll.
pub async fn validate_daemon(socket: &Path) -> Result<usize> {
    let client = reqwest::Client::builder().unix_socket(socket).build()?;
    let base = "http://flotilla.local";
    let discovery = client.get(format!("{base}/apis/flotilla.work/v1")).send().await?;
    let discovered = discovery.status().is_success();
    let kind_namespaces = if discovered {
        let document: Value = discovery.json().await?;
        let kinds = document
            .get("kinds")
            .and_then(Value::as_array)
            .ok_or_else(|| eyre!("daemon kind discovery response has no kinds array"))?
            .iter()
            .map(|kind| kind.as_str().map(str::to_string).ok_or_else(|| eyre!("daemon kind discovery contains a non-string kind")))
            .collect::<Result<Vec<_>>>()?;
        let namespaces = document
            .get("namespaces")
            .and_then(Value::as_object)
            .ok_or_else(|| eyre!("daemon kind discovery response has no namespace inventory"))?;
        kinds
            .into_iter()
            .map(|kind| {
                let found = namespaces
                    .get(&kind)
                    .and_then(Value::as_array)
                    .ok_or_else(|| eyre!("daemon kind discovery has no namespaces for {kind}"))?
                    .iter()
                    .map(|namespace| namespace.as_str().map(str::to_string).ok_or_else(|| eyre!("invalid namespace for {kind}")))
                    .collect::<Result<Vec<_>>>()?;
                Ok((kind, found))
            })
            .collect::<Result<Vec<_>>>()?
    } else if discovery.status() == reqwest::StatusCode::NOT_FOUND {
        // Previous-generation daemons predate discovery. All daemon-managed
        // records in that generation use the default namespace.
        REGISTERED_RESOURCE_KINDS.iter().map(|kind| (kind.plural.to_string(), vec!["flotilla".to_string()])).collect()
    } else {
        return Err(eyre!("daemon kind discovery failed: {}", discovery.status()));
    };

    let mut failed = false;
    let mut count = 0;
    for (kind, namespaces) in kind_namespaces {
        let replication = REGISTERED_RESOURCE_KINDS.iter().find(|entry| entry.plural == kind).map(|entry| entry.replication_class);
        let query = if replication.is_some_and(|class| class != ReplicationClass::None) { "?replicaSources=true" } else { "" };
        for namespace in namespaces {
            if namespace.is_empty() || namespace.contains(['/', '?', '#']) {
                return Err(eyre!("daemon kind discovery returned invalid namespace {namespace:?} for {kind}"));
            }
            let label = format!("{namespace}/{kind}");
            let url = format!("{base}/apis/flotilla.work/v1/namespaces/{namespace}/{kind}{query}");
            let response = client.get(url).send().await.map_err(|error| eyre!("list {label}: {error}"))?;
            if response.status() == reqwest::StatusCode::BAD_REQUEST && !discovered {
                let message = response.text().await?;
                if message.contains("unknown resource kind") {
                    // A candidate may know a kind that the old daemon does not serve.
                    continue;
                }
                eprintln!("{label}: daemon list failed: {message}");
                failed = true;
                continue;
            }
            if !response.status().is_success() {
                eprintln!("{label}: daemon list failed: {}", response.text().await?);
                failed = true;
                continue;
            }
            let document: Value = response.json().await.map_err(|error| eyre!("decode {label} list: {error}"))?;
            let items = document.get("items").and_then(Value::as_array).ok_or_else(|| eyre!("{label}: daemon list has no items array"))?;
            for item in items {
                count += 1;
                let name = item.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("<unnamed>");
                if let Err(error) = validate_resource_document(item) {
                    eprintln!("{label}/{name}: {error}");
                    failed = true;
                }
            }
        }
    }
    if failed {
        Err(eyre!("resource validation failed after checking {count} stored records"))
    } else {
        println!("validated {count} stored records");
        Ok(count)
    }
}

pub fn validate_path(path: &Path) -> Result<()> {
    let mut files = Vec::new();
    collect_files(path, &mut files)?;
    if files.is_empty() {
        return Err(eyre!("no JSON or YAML resource documents found under {}", path.display()));
    }
    files.sort();
    let mut failed = false;
    for file in &files {
        let content = match std::fs::read_to_string(file) {
            Ok(content) => content,
            Err(error) => {
                eprintln!("{}: {error}", file.display());
                failed = true;
                continue;
            }
        };
        let documents = parse_documents(file, &content);
        match documents {
            Ok(documents) => {
                for (index, document) in documents.iter().enumerate() {
                    let label = format!("{}#{}", file.display(), index + 1);
                    match validate_resource_document(document) {
                        Ok(()) => println!("{label}: valid"),
                        Err(error) => {
                            eprintln!("{label}: {error}");
                            failed = true;
                        }
                    }
                }
            }
            Err(error) => {
                eprintln!("{}: {error}", file.display());
                failed = true;
            }
        }
    }
    if failed {
        Err(eyre!("resource validation failed"))
    } else {
        Ok(())
    }
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let file_type = std::fs::symlink_metadata(path)?.file_type();
    if file_type.is_file() {
        files.push(path.to_path_buf());
    } else if file_type.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let candidate = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                collect_files(&candidate, files)?;
            } else if file_type.is_file() && matches!(candidate.extension().and_then(|ext| ext.to_str()), Some("json" | "yaml" | "yml")) {
                files.push(candidate);
            }
        }
    } else {
        return Err(eyre!("resource path is not a regular file or directory: {}", path.display()));
    }
    Ok(())
}

fn parse_documents(path: &Path, content: &str) -> Result<Vec<Value>> {
    if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
        return Ok(vec![serde_json::from_str(content)?]);
    }
    let mut documents = Vec::new();
    for document in serde_yml::Deserializer::from_str(content) {
        let value = Value::deserialize(document)?;
        if !value.is_null() {
            documents.push(value);
        }
    }
    if documents.is_empty() {
        Err(eyre!("no resource documents"))
    } else {
        Ok(documents)
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc, time::Duration};

    use flotilla_core::{config::ConfigStore, providers::discovery::test_support::fake_discovery};
    use flotilla_daemon::server::DaemonServer;
    use flotilla_protocol::NodeId;
    use flotilla_resources::{validate_resource_document, Convoy, ConvoySpec, ConvoyStatus, InputMeta, Project, ProjectSpec};

    use super::{collect_files, parse_documents, validate_daemon};

    #[test]
    fn reports_the_nested_field_of_a_stale_manifest() {
        let yaml = "apiVersion: flotilla.work/v1\nkind: CredentialSpec\nmetadata:\n  name: lab\nspec:\n  consumer:\n    adapter: forgejo\n    server_url: https://forge.example\n    username: bot\n  source: {}\n  lifecycle: {}\n";
        let document = &parse_documents(Path::new("credential.yaml"), yaml).expect("parse yaml")[0];
        let error = validate_resource_document(document).expect_err("old Forgejo shape must fail");
        assert!(error.to_string().contains("spec.consumer"), "{error}");
        assert!(error.to_string().contains("forge_ref"), "{error}");
    }

    #[test]
    fn parses_multiple_yaml_documents() {
        let yaml = "kind: PlacementPolicy\n---\nkind: Forge\n";
        assert_eq!(parse_documents(Path::new("resources.yaml"), yaml).expect("parse yaml").len(), 2);
    }

    #[test]
    fn reports_a_renamed_status_field() {
        let document = serde_json::json!({
            "apiVersion": "flotilla.work/v1", "kind": "Usage",
            "metadata": {"name": "quota"},
            "spec": {"provider": "codex", "account": "test"},
            "status": {"observed_on": "2026-09-28T00:00:00Z"}
        });
        let error = validate_resource_document(&document).expect_err("renamed status field must fail");
        assert!(error.to_string().contains("status."), "{error}");
        assert!(error.to_string().contains("observed_at"), "{error}");
    }

    #[test]
    fn reports_an_unknown_field_in_embedded_status() {
        let mut status = serde_json::to_value(ConvoyStatus::default()).expect("encode convoy status");
        status["workflow_snapshot"] = serde_json::json!({"vessels": [{"name": "work", "crew": [], "retired_field": true}]});
        let document = serde_json::json!({
            "apiVersion": "flotilla.work/v1", "kind": "Convoy",
            "metadata": {"name": "sample"},
            "spec": serde_json::to_value(ConvoySpec::builder().workflow_ref("default".to_string()).build()).expect("encode convoy spec"),
            "status": status
        });
        let error = validate_resource_document(&document).expect_err("unknown nested status field must fail");
        assert!(error.to_string().contains("status.workflow_snapshot.vessels[0]"), "{error}");
        assert!(error.to_string().contains("retired_field"), "{error}");
    }

    #[tokio::test]
    async fn validates_mixed_kinds_from_an_in_process_daemon() {
        flotilla_core::tls::install_default_provider();
        let root = std::env::temp_dir().join(format!("flotilla-live-validate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("config")).expect("create daemon directory");
        std::fs::write(root.join("config/daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon identity");
        let config = Arc::new(ConfigStore::with_base(root.join("config")));
        let socket = root.join("daemon.sock");
        let server = DaemonServer::new(vec![], config, fake_discovery(false), socket.clone(), Duration::from_secs(30))
            .await
            .expect("build in-process daemon server");
        let backend = server.daemon().resource_backend().clone();
        backend
            .using::<Project>("flotilla")
            .create(
                &InputMeta::builder().name("sample-project".to_string()).build(),
                &ProjectSpec::builder().display_name("Sample".to_string()).default_workflow_ref("default".to_string()).build(),
            )
            .await
            .expect("create project");
        backend
            .using::<Convoy>("flotilla")
            .create(
                &InputMeta::builder().name("sample-convoy".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("default".to_string()).build(),
            )
            .await
            .expect("create convoy");
        backend
            .using::<Project>("ops")
            .create(
                &InputMeta::builder().name("ops-project".to_string()).build(),
                &ProjectSpec::builder().display_name("Ops".to_string()).default_workflow_ref("default".to_string()).build(),
            )
            .await
            .expect("create project outside default namespace");
        let replicas = backend.using::<Convoy>("replicas");
        replicas
            .create(
                &InputMeta::builder().name("replica-only".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("default".to_string()).build(),
            )
            .await
            .expect("create replica source");
        let source = replicas.list().await.expect("list replica source");
        backend
            .replica_writer::<Convoy>(NodeId::new("other-root"), "replicas")
            .replace(&source, chrono::Utc::now())
            .await
            .expect("write replica source");
        replicas.delete("replica-only").await.expect("remove local replica source");
        let task = tokio::spawn(async move { server.run().await });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let checked = validate_daemon(&socket).await.expect("candidate decodes all served kinds and namespaces");
        assert!(checked >= 4, "expected default, non-default, and replica-only records; got {checked}");
        task.abort();
        std::fs::remove_dir_all(root).expect("remove daemon directory");
    }

    #[cfg(unix)]
    #[test]
    fn skips_symlinked_directories() {
        let root = std::env::temp_dir().join(format!("flotilla-validate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).expect("create test directory");
        let manifest = root.join("resource.yaml");
        std::fs::write(&manifest, "kind: PlacementPolicy\n").expect("write manifest");
        std::os::unix::fs::symlink(&root, root.join("loop")).expect("create directory symlink");

        let mut files = Vec::new();
        collect_files(&root, &mut files).expect("collect files");
        assert_eq!(files, vec![manifest]);

        std::fs::remove_dir_all(&root).expect("remove test directory");
    }
}
