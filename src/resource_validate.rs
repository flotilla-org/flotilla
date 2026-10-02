use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use color_eyre::{eyre::eyre, Result};
use flotilla_core::{
    ops_entry::{parse_operational_entry, OperationalEntryFile},
    path_context::ExecutionEnvironmentPath,
    providers::{vcs::git_worktree::GitWorktreeStrategy, ProcessCommandRunner},
    repository_inspection::{inspect_project_ops_entries, GitRepositoryInspector, RepositoryInspector},
    vcs::{FixedVcsResolver, FlotillaVcs, GitCheckoutStrategy},
};
use flotilla_resources::{
    validate_resource_document, K8sResourceObject, Project, ReplicationClass, ResourceObject, REGISTERED_RESOURCE_KINDS,
};
use serde::Deserialize;
use serde_json::Value;

/// Query JSON directly over the daemon's resource socket. The command protocol's
/// fingerprint deliberately rejects mixed generations during a fleet roll.
pub async fn validate_daemon(socket: &Path, local_roots: Option<&[PathBuf]>, skill_catalog: Option<&Path>) -> Result<usize> {
    let catalog = skill_catalog.map(load_catalog).transpose()?;
    let mut skill_documents = Vec::new();
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
    let mut projects = BTreeMap::<String, Vec<ResourceObject<Project>>>::new();
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
                if kind == "projects" {
                    match serde_json::from_value::<K8sResourceObject<Project>>(item.clone())
                        .map_err(|error| error.to_string())
                        .and_then(|object| ResourceObject::from_k8s_object(object).map_err(|error| error.to_string()))
                    {
                        Ok(project) => projects.entry(namespace.clone()).or_default().push(project),
                        Err(error) => {
                            eprintln!("{label}/{name}: {error}");
                            failed = true;
                        }
                    }
                }
                if let Err(error) = validate_resource_document(item) {
                    eprintln!("{label}/{name}: {error}");
                    failed = true;
                }
            }
            if catalog.is_some() && matches!(kind.as_str(), "projects" | "crewdefaults") {
                // Schema validation checks every stored provenance above. Skill
                // policy must use the merged definition view, just like admission.
                let merged: Value = client
                    .get(format!("{base}/apis/flotilla.work/v1/namespaces/{namespace}/{kind}?includeReplicas=true"))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                skill_documents.extend(
                    merged
                        .get("items")
                        .and_then(Value::as_array)
                        .ok_or_else(|| eyre!("{label}: merged list has no items"))?
                        .iter()
                        .cloned(),
                );
            }
        }
    }
    // Build the old-endpoint fallback lazily, once for all namespaces.
    let mut local_inventory = None;
    for (namespace, registered) in projects {
        let response = client.get(format!("{base}/apis/flotilla.work/v1/namespaces/{namespace}/operationalentries")).send().await?;
        let result = if response.status().is_success() {
            let document: Value = response.json().await?;
            let files: Vec<OperationalEntryFile> =
                serde_json::from_value(document.get("entries").cloned().ok_or_else(|| eyre!("ops inventory has no entries"))?)?;
            validate_ops_files(&files)
        } else if response.status() == reqwest::StatusCode::NOT_FOUND {
            // An absent endpoint (including on older daemons) does not prove
            // a particular version. Candidate-side local inspection is still
            // mandatory; never interpret a 404 as an empty input inventory.
            let roots =
                local_roots.ok_or_else(|| eyre!("ops inventory endpoint not found on peer; run the candidate validation on that host"))?;
            if local_inventory.is_none() {
                let runner = Arc::new(ProcessCommandRunner);
                let vcs = FlotillaVcs::new(
                    ExecutionEnvironmentPath::new("/"),
                    runner.clone(),
                    GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new("unused".into(), runner.clone()))),
                );
                let inspector = GitRepositoryInspector::new(runner, Arc::new(FixedVcsResolver(Arc::new(vcs))), "validation");
                let paths = inspect_validation_roots(roots, &inspector).await?;
                local_inventory = Some((inspector, paths));
            }
            let (inspector, paths) = local_inventory.as_ref().expect("initialized local inventory");
            let files = inspect_project_ops_entries(&registered, paths, inspector).await.map_err(|error| eyre!(error))?;
            validate_ops_files(&files)
        } else {
            Err(eyre!("{namespace}: cannot inspect operational entries: {}", response.text().await?))
        };
        match result {
            Ok(entries) => println!("validated {entries} operational entries in {namespace}"),
            Err(error) => {
                eprintln!("{error}");
                failed = true;
            }
        }
    }
    if failed {
        Err(eyre!("resource validation failed after checking {count} stored records"))
    } else {
        if let Some(catalog) = &catalog {
            validate_skill_documents(catalog, &skill_documents)?;
        }
        println!("validated {count} stored records");
        Ok(count)
    }
}

fn validate_ops_files(files: &[OperationalEntryFile]) -> Result<usize> {
    let mut count = 0;
    let mut errors = Vec::new();
    for file in files {
        match parse_operational_entry(&file.contents) {
            Ok(Some(_)) => count += 1,
            Ok(None) => {}
            Err(error) => errors.push(format!("{}: {error}", file.path)),
        }
    }
    if errors.is_empty() {
        Ok(count)
    } else {
        Err(eyre!("operational entry validation refused:\n{}", errors.join("\n")))
    }
}

async fn inspect_validation_roots(
    roots: &[PathBuf],
    inspector: &dyn RepositoryInspector,
) -> Result<BTreeMap<flotilla_resources::RepositoryKey, Vec<PathBuf>>> {
    let mut paths = BTreeMap::new();
    for root in roots {
        let inspection = inspector.inspect_path(root, None).await.map_err(|error| eyre!(error))?;
        paths.entry(inspection.spec.key()).or_insert_with(Vec::new).push(root.clone());
    }
    Ok(paths)
}

#[cfg(test)]
async fn validate_project_ops(
    projects: &[ResourceObject<Project>],
    roots: &[PathBuf],
    inspector: &dyn RepositoryInspector,
) -> Result<usize> {
    let paths = inspect_validation_roots(roots, inspector).await?;
    let files = inspect_project_ops_entries(projects, &paths, inspector).await.map_err(|error| eyre!(error))?;
    validate_ops_files(&files)
}

pub fn validate_path(path: &Path, skill_catalog: Option<&Path>) -> Result<()> {
    let catalog = skill_catalog.map(load_catalog).transpose()?;
    let mut skill_documents = Vec::new();
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
                    if catalog.is_some() && matches!(document["kind"].as_str(), Some("Project" | "CrewDefaults")) {
                        skill_documents.push(document.clone());
                    }
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
        if let Some(catalog) = &catalog {
            validate_skill_documents(catalog, &skill_documents)?;
        }
        Ok(())
    }
}

fn load_catalog(path: &Path) -> Result<Vec<flotilla_resources::SkillCatalogEntry>> {
    let catalog: Vec<flotilla_resources::SkillCatalogEntry> = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let manifest = serde_json::from_str(&std::fs::read_to_string(path.with_file_name(".flotilla-sources.json"))?)?;
    flotilla_resources::crew_defaults::validate_catalog(&catalog, &manifest).map_err(|error| eyre!(error))?;
    Ok(catalog)
}

fn validate_skill_documents(catalog: &[flotilla_resources::SkillCatalogEntry], documents: &[Value]) -> Result<()> {
    use std::collections::BTreeMap;

    use flotilla_resources::{crew_defaults::check_skill_declarations, CrewDefaultsSpec, ProjectSpec};
    let mut namespaces = BTreeMap::<String, (BTreeMap<String, Vec<CrewDefaultsSpec>>, Vec<ProjectSpec>)>::new();
    for document in documents {
        let namespace = document["metadata"]["namespace"].as_str().unwrap_or("flotilla").to_string();
        let (defaults, projects) = namespaces.entry(namespace).or_default();
        match document["kind"].as_str() {
            Some("CrewDefaults") => {
                defaults
                    .entry(document["metadata"]["name"].as_str().ok_or_else(|| eyre!("CrewDefaults name missing"))?.to_string())
                    .or_default()
                    .push(serde_json::from_value(document["spec"].clone())?);
            }
            Some("Project") => projects.push(serde_json::from_value(document["spec"].clone())?),
            _ => {}
        }
    }
    for (namespace, (defaults, projects)) in namespaces {
        if defaults.len() > 1 {
            return Err(eyre!("{namespace}: skill admission requires at most one CrewDefaults"));
        }
        let defaults = if defaults.is_empty() { vec![CrewDefaultsSpec::default()] } else { defaults.into_values().flatten().collect() };
        for defaults in &defaults {
            check_skill_declarations(catalog, defaults, &projects).map_err(|error| eyre!("{namespace}: {error}"))?;
        }
    }
    println!("validated crew skill declarations against the candidate catalog");
    Ok(())
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
    use flotilla_test_support::TestSocketDir;

    use super::{collect_files, parse_documents, validate_daemon, validate_project_ops};

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
        let socket_dir = TestSocketDir::new();
        let socket = socket_dir.socket_path("daemon.sock");
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
                &ProjectSpec::builder()
                    .display_name("Ops".to_string())
                    .default_workflow_ref("default".to_string())
                    .skills(std::collections::BTreeMap::from([("governor".to_string(), vec!["missing".to_string()])]))
                    .build(),
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
        let client = reqwest::Client::builder().unix_socket(socket.as_path()).build().expect("resource socket client");
        let inventory = client
            .get("http://flotilla.local/apis/flotilla.work/v1/namespaces/flotilla/operationalentries")
            .send()
            .await
            .expect("raw ops endpoint");
        assert_eq!(inventory.status(), reqwest::StatusCode::OK);
        assert_eq!(inventory.json::<serde_json::Value>().await.expect("inventory")["entries"], serde_json::json!([]));
        backend
            .using::<Project>("missing")
            .create(
                &InputMeta::builder().name("unavailable".to_string()).build(),
                &ProjectSpec::builder()
                    .display_name("Unavailable".to_string())
                    .default_workflow_ref("default".to_string())
                    .repositories(vec![flotilla_resources::ProjectRepositorySpec {
                        repo: flotilla_resources::RepositoryKey("unavailable-ops".into()),
                        alias: None,
                        roles: std::collections::BTreeSet::from([flotilla_resources::ProjectRepositoryRole::Ops]),
                        subpath: None,
                        default_branch: None,
                    }])
                    .build(),
            )
            .await
            .expect("project with missing ops source");
        let refused = client
            .get("http://flotilla.local/apis/flotilla.work/v1/namespaces/missing/operationalentries")
            .send()
            .await
            .expect("missing-source endpoint");
        assert_eq!(refused.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(refused.text().await.expect("error body").contains("unavailable-ops"));
        backend.using::<Project>("missing").delete("unavailable").await.expect("remove refused fixture");
        let checked = validate_daemon(&socket, Some(&[]), None).await.expect("candidate decodes all served kinds and namespaces");
        assert!(checked >= 4, "expected default, non-default, and replica-only records; got {checked}");
        // Intended: schema compatibility alone cannot admit a Project whose
        // skill declaration is absent from the candidate's supply catalog.
        let catalog = root.join(".flotilla-skill-catalog.json");
        std::fs::write(&catalog, serde_json::json!([{"source":"source", "repository":"owner/repo", "revision":"1".repeat(40), "name":"research", "path":"skills/research"}]).to_string()).expect("catalog");
        std::fs::write(root.join(".flotilla-sources.json"), serde_json::json!({"schema_version":5,"sources":[{"name":"source","repository":"https://github.com/owner/repo.git","revision":"1".repeat(40)}]}).to_string()).expect("source manifest");
        let error = validate_daemon(&socket, Some(&[]), Some(&catalog)).await.expect_err("pre-roll checks registered projects in every namespace");
        assert!(error.to_string().contains("ops") && error.to_string().contains("missing"), "{error}");
        task.abort();
        std::fs::remove_dir_all(root).expect("remove daemon directory");
    }

    #[tokio::test]
    async fn candidate_checks_registered_ops_entries_and_fails_on_unknown_fields() {
        use std::sync::Arc;

        use flotilla_core::{
            path_context::ExecutionEnvironmentPath,
            providers::{vcs::git_worktree::GitWorktreeStrategy, ProcessCommandRunner},
            repository_inspection::GitRepositoryInspector,
            vcs::{FixedVcsResolver, FlotillaVcs, GitCheckoutStrategy},
        };
        use flotilla_resources::{ProjectRepositoryRole, ProjectRepositorySpec, RepositorySpec};
        let tmp_guard = tempfile::tempdir().expect("temporary ops repository");
        let tmp = tmp_guard.path().to_path_buf();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
            vec!["remote", "add", "origin", "https://github.com/example/ops"],
        ] {
            assert!(std::process::Command::new("git").args(args).current_dir(tmp.as_path()).status().expect("fixture git").success());
        }
        std::fs::write(
            tmp.as_path().join("governor.md"),
            "---\nkind: ensure\nrole: governor\n---\nworkflow: govern\nunknown_option: true\n",
        )
        .expect("invalid ops entry");
        assert!(std::process::Command::new("git").args(["add", "."]).current_dir(tmp.as_path()).status().expect("stage fixture").success());
        assert!(std::process::Command::new("git")
            .args(["commit", "-m", "fixture"])
            .current_dir(tmp.as_path())
            .status()
            .expect("commit fixture")
            .success());
        let spec = RepositorySpec::remote("https://github.com/example/ops").expect("repository");
        let backend = flotilla_resources::ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::default());
        let project = backend
            .using::<Project>("flotilla")
            .create(
                &InputMeta::builder()
                    .name("demo".to_string())
                    .annotations(std::collections::BTreeMap::from([
                        (
                            flotilla_core::project_declaration::BOOTSTRAP_PATH_ANNOTATION.into(),
                            tmp.as_path().to_string_lossy().into_owned(),
                        ),
                        (flotilla_core::project_declaration::BOOTSTRAP_REPOSITORY_ANNOTATION.into(), spec.key().to_string()),
                    ]))
                    .build(),
                &ProjectSpec::builder()
                    .display_name("Demo".to_string())
                    .default_workflow_ref("govern".to_string())
                    .repositories(vec![ProjectRepositorySpec {
                        repo: spec.key(),
                        alias: Some("ops".into()),
                        roles: std::collections::BTreeSet::from([ProjectRepositoryRole::Ops]),
                        subpath: None,
                        default_branch: None,
                    }])
                    .build(),
            )
            .await
            .expect("registered project");
        let runner = Arc::new(ProcessCommandRunner);
        let vcs = FlotillaVcs::new(
            ExecutionEnvironmentPath::new(tmp.as_path()),
            runner.clone(),
            GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new("unused".into(), runner.clone()))),
        );
        let inspector = GitRepositoryInspector::new(runner, Arc::new(FixedVcsResolver(Arc::new(vcs))), "local");
        let error =
            validate_project_ops(std::slice::from_ref(&project), &[], &inspector).await.expect_err("candidate rejects unknown ops field");
        assert!(error.to_string().contains("governor.md"));
        assert!(error.to_string().contains("unknown_option"));
        let mut unavailable = project.clone();
        unavailable.metadata.annotations.clear();
        let error = validate_project_ops(&[unavailable], &[], &inspector).await.expect_err("missing source fails the gate");
        assert!(error.to_string().contains("no checkout available"), "{error}");
        let duplicate_guard = tempfile::tempdir().expect("duplicate checkout directory");
        let duplicate = duplicate_guard.path().to_path_buf();
        assert!(std::process::Command::new("git")
            .args(["clone", "--local"])
            .arg(&tmp)
            .arg(&duplicate)
            .status()
            .expect("duplicate main checkout")
            .success());
        assert!(std::process::Command::new("git")
            .args(["remote", "set-url", "origin", "https://github.com/example/ops"])
            .current_dir(&duplicate)
            .status()
            .expect("same repository identity")
            .success());
        let error = validate_project_ops(&[project], &[tmp.clone(), duplicate.clone()], &inspector)
            .await
            .expect_err("ambiguous main checkouts fail the gate");
        assert!(error.to_string().contains("no unambiguous main checkout"), "{error}");
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
    #[test]
    fn pre_roll_refuses_multiple_defaults_in_one_namespace() {
        // Intended: the offline pre-roll gate enforces admission's singleton.
        let first = serde_json::json!({"kind":"CrewDefaults", "metadata":{"name":"one", "namespace":"fleet"}, "spec":{}});
        let second = serde_json::json!({"kind":"CrewDefaults", "metadata":{"name":"two", "namespace":"fleet"}, "spec":{}});
        assert!(super::validate_skill_documents(&[], &[first, second])
            .expect_err("ambiguous fleet defaults")
            .to_string()
            .contains("at most one CrewDefaults"));
    }

    #[test]
    fn pre_roll_resolves_all_projects_with_their_namespace_defaults() {
        // Intended: a valid project cannot mask another project's missing import;
        // defaults in one namespace never apply to a project in another.
        let catalog = vec![flotilla_resources::SkillCatalogEntry::builder()
            .source("source".into())
            .repository("owner/repo".into())
            .revision("1".repeat(40))
            .name("research".into())
            .path("skills/research".into())
            .build()];
        let defaults = serde_json::json!({"kind":"CrewDefaults", "metadata":{"name":"fleet", "namespace":"first"}, "spec":{"skills":{"coder":["research"]}}});
        let good = serde_json::json!({"kind":"Project", "metadata":{"name":"good", "namespace":"first"}, "spec":{"display_name":"good","default_workflow_ref":"work"}});
        let bad = serde_json::json!({"kind":"Project", "metadata":{"name":"bad", "namespace":"second"}, "spec":{"display_name":"bad","default_workflow_ref":"work", "skills":{"coder":["missing"]}}});
        assert!(super::validate_skill_documents(&catalog, &[defaults.clone(), good.clone()]).is_ok());
        let error = super::validate_skill_documents(&catalog, &[defaults, good, bad]).expect_err("every registered project is checked");
        assert!(error.to_string().contains("second") && error.to_string().contains("missing"));
    }
}
