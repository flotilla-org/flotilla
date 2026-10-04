use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::{collections::BTreeMap, sync::Arc};

use color_eyre::{eyre::eyre, Result};
use flotilla_core::in_process::DEFAULT_PROVISIONING_NAMESPACE;
#[cfg(unix)]
use flotilla_core::{
    ops_entry::{parse_operational_entry, OperationalEntryFile},
    path_context::ExecutionEnvironmentPath,
    providers::{vcs::git_worktree::GitWorktreeStrategy, ProcessCommandRunner},
    repository_inspection::{inspect_project_ops_entries, GitRepositoryInspector, OperationalEntryInventory, RepositoryInspector},
    vcs::{FixedVcsResolver, FlotillaVcs, GitCheckoutStrategy},
};
use flotilla_resources::validate_resource_document;
#[cfg(unix)]
use flotilla_resources::{K8sResourceObject, Project, ReplicationClass, ResourceObject, REGISTERED_RESOURCE_KINDS};
use serde::Deserialize;
use serde_json::Value;

#[cfg(unix)]
const VALIDATION_INSPECTION_HOST: &str = "candidate-validation";

#[cfg(not(unix))]
pub async fn validate_daemon(_socket: &Path, _local_roots: Option<&[PathBuf]>, _skill_catalog: Option<&Path>) -> Result<usize> {
    Err(eyre!("daemon resource-socket validation is only supported on Unix"))
}

/// Query JSON directly over the daemon's resource socket. The command protocol's
/// fingerprint deliberately rejects mixed generations during a fleet roll.
#[cfg(unix)]
pub async fn validate_daemon(socket: &Path, local_roots: Option<&[PathBuf]>, skill_catalog: Option<&Path>) -> Result<usize> {
    let catalog = skill_catalog.map(load_catalog).transpose()?;
    let mut skill_documents = Vec::new();
    let client = reqwest::Client::builder().unix_socket(socket).build()?;
    let base = "http://flotilla.local";
    let discovery = client.get(format!("{base}/apis/flotilla.work/v1")).send().await?;
    let discovered = discovery.status().is_success();
    let mut additional_stores = Vec::new();
    let kind_namespaces = if discovered {
        let document: Value = discovery.json().await?;
        additional_stores = document
            .get("stores")
            .map(|stores| {
                stores
                    .as_array()
                    .ok_or_else(|| eyre!("daemon stores inventory is not an array"))?
                    .iter()
                    .map(|store| store.as_str().map(str::to_string).ok_or_else(|| eyre!("invalid resource store path")))
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        discovery_kind_namespaces(&document)?
    } else if discovery.status() == reqwest::StatusCode::NOT_FOUND {
        // Previous-generation daemons predate discovery. All daemon-managed
        // records in that generation use the default namespace.
        REGISTERED_RESOURCE_KINDS.iter().map(|kind| (kind.plural.to_string(), vec!["flotilla".to_string()])).collect()
    } else {
        return Err(eyre!("daemon kind discovery failed: {}", discovery.status()));
    };

    let mut collections = kind_namespaces.into_iter().map(|(kind, namespaces)| (base.to_string(), kind, namespaces)).collect::<Vec<_>>();
    for prefix in additional_stores {
        // Deliberately fail closed: admitting an unknown store would skip
        // persisted records that this candidate cannot inventory before a roll.
        if prefix != "/observed" {
            return Err(eyre!("unknown advertised resource store {prefix:?}"));
        }
        let store_base = format!("{base}{prefix}");
        let document: Value = client.get(format!("{store_base}/apis/flotilla.work/v1")).send().await?.error_for_status()?.json().await?;
        collections
            .extend(discovery_kind_namespaces(&document)?.into_iter().map(|(kind, namespaces)| (store_base.clone(), kind, namespaces)));
    }

    let mut failed = false;
    let mut count = 0;
    let mut projects = BTreeMap::<String, Vec<ResourceObject<Project>>>::new();
    for (store_base, kind, namespaces) in collections {
        let replication = REGISTERED_RESOURCE_KINDS.iter().find(|entry| entry.plural == kind).map(|entry| entry.replication_class);
        let query = if replication.is_some_and(|class| class != ReplicationClass::None) { "?replicaSources=true" } else { "" };
        for namespace in namespaces {
            if namespace.is_empty() || namespace.contains(['/', '?', '#']) {
                return Err(eyre!("daemon kind discovery returned invalid namespace {namespace:?} for {kind}"));
            }
            let label = format!("{store_base}/{namespace}/{kind}");
            let url = format!("{store_base}/apis/flotilla.work/v1/namespaces/{namespace}/{kind}{query}");
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
                if store_base == base && kind == "projects" {
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
            if store_base == base && catalog.is_some() && matches!(kind.as_str(), "projects" | "crewdefaults") {
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
        let status = response.status();
        let (success, body) =
            if status.is_success() { (Some(response.json::<Value>().await?), None) } else { (None, Some(response.text().await?)) };
        let result = if let Some(document) = success {
            validate_ops_inventory(&serde_json::from_value(document)?)
        } else if ops_inventory_endpoint_absent(status, body.as_deref().unwrap_or_default()) {
            // An absent endpoint (including on older daemons) does not prove
            // a particular version. Candidate-side local inspection is still
            // mandatory; never interpret a 404 as an empty input inventory.
            async {
                let roots = local_roots
                    .ok_or_else(|| eyre!("ops inventory endpoint not found on peer; run the candidate validation on that host"))?;
                if local_inventory.is_none() {
                    let runner = Arc::new(ProcessCommandRunner);
                    // Inspection only reads absolute repository paths. The checkout
                    // strategy is required by the resolver but never creates worktrees.
                    let vcs = FlotillaVcs::new(
                        ExecutionEnvironmentPath::new("/"),
                        runner.clone(),
                        GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new("unused".into(), runner.clone()))),
                    );
                    let inspector =
                        GitRepositoryInspector::new(runner, Arc::new(FixedVcsResolver(Arc::new(vcs))), VALIDATION_INSPECTION_HOST);
                    let paths = inspect_validation_roots(roots, &inspector).await;
                    local_inventory = Some((inspector, paths));
                }
                let (inspector, paths) = local_inventory.as_ref().expect("initialized local inventory");
                validate_project_ops(&registered, paths, inspector).await
            }
            .await
        } else {
            Err(eyre!("{namespace}: cannot inspect operational entries: {}", body.unwrap_or_default()))
        };
        match result {
            Ok(0) => {}
            Ok(entries) => println!("validated {entries} operational entries in {namespace}"),
            Err(error) => {
                eprintln!("{namespace}: {error}");
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

#[cfg(unix)]
fn discovery_kind_namespaces(document: &Value) -> Result<Vec<(String, Vec<String>)>> {
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
        .collect::<Result<Vec<_>>>()
}

/// Whether a daemon lacks the operational-entries inventory endpoint. Previous
/// generations either have no route (404) or reject the kind as unknown (400),
/// and both fall back to candidate-side local inspection. Other errors, such as
/// 422 for an unavailable ops source, are real refusals.
///
/// The 400 match is coupled to the previous generation's wire message for an
/// unregistered kind ("unknown resource kind '<kind>' (supported: ...)").
/// Remove it one roll after every host serves the endpoint.
#[cfg(unix)]
fn ops_inventory_endpoint_absent(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::NOT_FOUND || (status == reqwest::StatusCode::BAD_REQUEST && body.contains("unknown resource kind"))
}

#[cfg(unix)]
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

#[cfg(unix)]
async fn inspect_validation_roots(
    roots: &[PathBuf],
    inspector: &dyn RepositoryInspector,
) -> BTreeMap<flotilla_resources::RepositoryKey, Vec<PathBuf>> {
    let mut paths = BTreeMap::new();
    for root in roots {
        // A root without a derivable identity cannot back any ops member. Report
        // it and continue: a member that needed it still fails as unavailable.
        match inspector.inspect_path(root, None).await {
            Ok(inspection) => paths.entry(inspection.spec.key()).or_insert_with(Vec::new).push(root.clone()),
            Err(error) => eprintln!("{}: not identified for ops validation: {error}", root.display()),
        }
    }
    paths
}

#[cfg(unix)]
async fn validate_project_ops(
    projects: &[ResourceObject<Project>],
    paths: &BTreeMap<flotilla_resources::RepositoryKey, Vec<PathBuf>>,
    inspector: &dyn RepositoryInspector,
) -> Result<usize> {
    validate_ops_inventory(&inspect_project_ops_entries(projects, paths, inspector).await.map_err(|error| eyre!(error))?)
}

#[cfg(unix)]
fn validate_ops_inventory(inventory: &OperationalEntryInventory) -> Result<usize> {
    for unavailable in &inventory.unavailable {
        println!("{unavailable}; validated on the hosts that hold it");
    }
    validate_ops_files(&inventory.entries)
}

pub fn validate_path(path: &Path, skill_catalog: Option<&Path>) -> Result<()> {
    let catalog = skill_catalog.map(load_catalog).transpose()?;
    let mut skill_documents = Vec::new();
    let mut grant_documents = Vec::new();
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
                        Ok(()) => {
                            if document["kind"].as_str() == Some("CredentialGrant") {
                                grant_documents.push(document.clone());
                            }
                            println!("{label}: valid");
                        }
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
    if let Err(error) = validate_grant_documents(&grant_documents) {
        eprintln!("{error}");
        failed = true;
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

/// Validate potential co-selection, including future roles and multi-repository vessels.
/// Namespace boundaries remain independent, as they are at admission.
fn validate_grant_documents(documents: &[Value]) -> Result<()> {
    use std::collections::BTreeMap;

    use flotilla_resources::{validate_matching_grant_permissions, CredentialGrantSpec};

    let mut namespaces = BTreeMap::<String, Vec<(String, CredentialGrantSpec)>>::new();
    for document in documents {
        let namespace = document["metadata"]["namespace"].as_str().unwrap_or(DEFAULT_PROVISIONING_NAMESPACE).to_string();
        // Schema validation reports malformed documents separately. They must
        // not hide composition errors among the remaining valid grants.
        let Some(name) = document["metadata"]["name"].as_str() else { continue };
        let Ok(spec) = serde_json::from_value::<CredentialGrantSpec>(document["spec"].clone()) else { continue };
        namespaces.entry(namespace).or_default().push((name.to_string(), spec));
    }
    let mut errors = Vec::new();
    for (namespace, grants) in namespaces {
        for (index, (name, grant)) in grants.iter().enumerate() {
            for (other_name, other) in &grants[index + 1..] {
                if grant.selector.overlaps(&other.selector) {
                    if let Err(error) = validate_matching_grant_permissions([(name.as_str(), grant), (other_name.as_str(), other)]) {
                        errors.extend(error.lines().map(|message| format!("{namespace}: {message}")));
                    }
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(eyre!("credential grant validation refused:\n{}", errors.join("\n")))
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
        let namespace = document["metadata"]["namespace"].as_str().unwrap_or(DEFAULT_PROVISIONING_NAMESPACE).to_string();
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

#[cfg(all(test, unix))]
mod tests {
    use std::{path::Path, sync::Arc, time::Duration};

    use flotilla_core::{config::ConfigStore, providers::discovery::test_support::fake_discovery};
    use flotilla_daemon::server::DaemonServer;
    use flotilla_protocol::NodeId;
    use flotilla_resources::{validate_resource_document, Convoy, ConvoySpec, ConvoyStatus, InputMeta, Project, ProjectSpec};
    use flotilla_test_support::TestSocketDir;
    use serde_json::Value;

    use super::{
        collect_files, inspect_validation_roots, ops_inventory_endpoint_absent, parse_documents, validate_daemon, validate_grant_documents,
        validate_path, validate_project_ops, VALIDATION_INSPECTION_HOST,
    };

    // Owner ruling #2491: the actual offline manifest command catches the
    // mixed case across files, including a listed empty map, before admission.
    #[test]
    fn manifest_gate_refuses_mixed_grant_permissions() {
        let root = tempfile::tempdir().expect("manifest directory");
        let grant = |name: &str, namespace: &str, role: &str, listed: bool| {
            serde_json::json!({
                "apiVersion":"flotilla.work/v1", "kind":"CredentialGrant",
                "metadata":{"name":name,"namespace":namespace},
                "spec":{"selector":{"projects":["demo"],"roles":[role]},"credentials":["app"],
                    "permissions":if listed { serde_json::json!({"app":{}}) } else { serde_json::json!({}) }}
            })
        };
        let base = grant("base", "flotilla", "governor", false);
        let listed = grant("elevation", "flotilla", "governor", true);
        std::fs::write(root.path().join("base.json"), base.to_string()).expect("base");
        std::fs::write(root.path().join("listed.json"), listed.to_string()).expect("listed");
        assert!(validate_path(root.path(), None).is_err());
        let error = validate_grant_documents(&[base.clone(), listed.clone()]).expect_err("mixed policy refused").to_string();
        for name in ["base", "elevation", "app", "explicit"] {
            assert!(error.contains(name), "{error}");
        }
        // Disjoint roles do not co-select for one crew (admission also forbids
        // different grant sets across co-located roles); namespaces are independent.
        assert!(validate_grant_documents(&[base.clone(), grant("elevation", "flotilla", "coder", true)]).is_ok());
        assert!(validate_grant_documents(&[base.clone(), grant("elevation", "other", "governor", true)]).is_ok());
        // Homogeneous unlisted and explicit policy remains valid.
        assert!(validate_grant_documents(&[base, grant("second", "flotilla", "governor", false)]).is_ok());
        assert!(validate_grant_documents(&[listed, grant("second", "flotilla", "governor", true)]).is_ok());
    }

    // A malformed grant must not hide diagnostics for valid grants; every
    // conflicting pair and credential is reported in one manifest validation pass.
    #[test]
    fn manifest_gate_reports_all_conflicts_despite_malformed_documents() {
        let grant = |name: &str, credentials: &[&str], permissions: Value| {
            serde_json::json!({
                "apiVersion":"flotilla.work/v1", "kind":"CredentialGrant", "metadata":{"name":name},
                "spec":{"selector":{}, "credentials":credentials, "permissions":permissions}
            })
        };
        let documents = vec![
            serde_json::json!({"kind":"CredentialGrant", "metadata":{"name":"malformed"}, "spec":{"selector":{},"credentials":false}}),
            grant("base", &["app", "other"], serde_json::json!({})),
            grant("elevation", &["app", "other"], serde_json::json!({"app":{},"other":{}})),
            grant("second-base", &["extra"], serde_json::json!({})),
            grant("second-elevation", &["extra"], serde_json::json!({"extra":{}})),
        ];
        let error = validate_grant_documents(&documents).expect_err("all valid conflicts are reported").to_string();
        for credential in ["app", "other", "extra"] {
            assert!(error.contains(&format!("credential `{credential}`")), "{error}");
        }
        for grant in ["base", "elevation", "second-base", "second-elevation"] {
            assert!(error.contains(&format!("grant `{grant}`")), "{error}");
        }
        let root = tempfile::tempdir().expect("manifest directory");
        for (index, document) in documents.into_iter().enumerate() {
            std::fs::write(root.path().join(format!("{index}.json")), document.to_string()).expect("manifest");
        }
        assert!(validate_path(root.path(), None).is_err(), "malformed manifests still refuse the overall validation");
    }

    #[test]
    fn previous_generation_daemons_fall_back_to_local_ops_inspection() {
        // A previous-generation daemon rejects the new kind as unknown (live r445 behaviour):
        // the pre-roll check must inspect ops locally, not fail the install.
        let unknown = "invalid resource: unknown resource kind 'operationalentries' (supported: artifacts, convoys)";
        assert!(ops_inventory_endpoint_absent(reqwest::StatusCode::BAD_REQUEST, unknown));
        assert!(ops_inventory_endpoint_absent(reqwest::StatusCode::NOT_FOUND, ""));
        // Real refusals stay refusals.
        assert!(!ops_inventory_endpoint_absent(reqwest::StatusCode::BAD_REQUEST, "invalid namespace"));
        assert!(!ops_inventory_endpoint_absent(reqwest::StatusCode::UNPROCESSABLE_ENTITY, "unknown resource kind"));
        assert!(!ops_inventory_endpoint_absent(reqwest::StatusCode::INTERNAL_SERVER_ERROR, ""));
    }

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
        let observed = server.daemon().observed_resource_backend();
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
        let primary_count = validate_daemon(&socket, Some(&[]), None).await.expect("validate primary store");
        // Persisted observations in a replica-only namespace must participate
        // in the candidate pre-roll gate alongside primary-store records.
        let checkouts = observed.using::<flotilla_resources::Checkout>("observed-only");
        checkouts
            .create(
                &InputMeta::builder().name("remote-checkout".to_string()).build(),
                &flotilla_resources::CheckoutSpec::Observed(flotilla_resources::ObservedCheckoutSpec {
                    repo_ref: flotilla_resources::RepositoryKey("widgets".into()),
                    path: "/srv/widgets".into(),
                    r#ref: "main".into(),
                    host_ref: "remote".into(),
                    is_main: true,
                }),
            )
            .await
            .expect("create observation source");
        observed
            .replica_writer::<flotilla_resources::Checkout>(NodeId::new("remote"), "observed-only")
            .replace(&checkouts.list().await.expect("list observation source"), chrono::Utc::now())
            .await
            .expect("persist observation replica");
        checkouts.delete("remote-checkout").await.expect("remove local observation");
        let checked = validate_daemon(&socket, Some(&[]), None).await.expect("candidate decodes all served kinds and namespaces");
        assert_eq!(checked, primary_count + 1, "the observed replica-only record must be validated");
        // Intended: schema compatibility alone cannot admit a Project whose
        // skill declaration is absent from the candidate's supply catalog.
        let catalog = root.join(".flotilla-skill-catalog.json");
        std::fs::write(&catalog, serde_json::json!([{"source":"source", "repository":"owner/repo", "revision":"1".repeat(40), "name":"research", "path":"skills/research"}]).to_string()).expect("catalog");
        std::fs::write(root.join(".flotilla-sources.json"), serde_json::json!({"schema_version":5,"sources":[{"name":"source","repository":"https://github.com/owner/repo.git","revision":"1".repeat(40)}]}).to_string()).expect("source manifest");
        let error =
            validate_daemon(&socket, Some(&[]), Some(&catalog)).await.expect_err("pre-roll checks registered projects in every namespace");
        assert!(error.to_string().contains("ops") && error.to_string().contains("missing"), "{error}");
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
        let reported = client
            .get("http://flotilla.local/apis/flotilla.work/v1/namespaces/missing/operationalentries")
            .send()
            .await
            .expect("missing-source endpoint");
        assert_eq!(reported.status(), reqwest::StatusCode::OK);
        let reported = reported.json::<serde_json::Value>().await.expect("inventory");
        assert_eq!(reported["entries"], serde_json::json!([]));
        assert!(reported["unavailable"][0].as_str().expect("unavailable member").contains("unavailable-ops"), "{reported}");
        // This host's daemon refuses to load a source it holds no checkout of, so
        // the source is validated on the hosts that do, not refused here.
        validate_daemon(&socket, Some(&[]), None).await.expect("a source without a local checkout is reported, not refused");
        task.abort();
        std::fs::remove_dir_all(root).expect("remove daemon directory");
    }

    #[tokio::test]
    async fn candidate_reads_ops_entries_from_a_detached_checkout_with_ambiguous_remotes() {
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
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git").args(args).current_dir(tmp.as_path()).status().expect("fixture git").success());
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["remote", "add", "github", "https://github.com/example/ops"]);
        git(&["remote", "add", "lab", "https://forge.example.test/lab/ops"]);
        std::fs::write(
            tmp.as_path().join("governor.md"),
            "---\nkind: ensure\nrole: governor\n---\nworkflow: govern\nunknown_option: true\n",
        )
        .expect("ops entry");
        git(&["add", "."]);
        git(&["commit", "-m", "fixture"]);
        git(&["checkout", "--detach"]);
        // The daemon loads committed entries only; working-tree drafts are not inputs.
        std::fs::write(
            tmp.as_path().join("draft.md"),
            "---\nkind: ensure\nrole: governor\n---\nworkflow: govern\ndraft_only_field: true\n",
        )
        .expect("untracked draft");
        std::fs::write(
            tmp.as_path().join("governor.md"),
            "---\nkind: ensure\nrole: governor\n---\nworkflow: govern\nuncommitted_edit: true\n",
        )
        .expect("uncommitted edit");
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
        let inspector = GitRepositoryInspector::new(runner, Arc::new(FixedVcsResolver(Arc::new(vcs))), VALIDATION_INSPECTION_HOST);
        // The roots pass tolerates the unidentifiable checkout; the bootstrap
        // annotation still selects it and its entries are parsed.
        let paths = inspect_validation_roots(std::slice::from_ref(&tmp), &inspector).await;
        let error = validate_project_ops(std::slice::from_ref(&project), &paths, &inspector)
            .await
            .expect_err("candidate parses the selected checkout's entries");
        assert!(error.to_string().contains("unknown_option"), "{error}");
        assert!(!error.to_string().contains("draft_only_field"), "untracked files are not validated: {error}");
        assert!(!error.to_string().contains("uncommitted_edit"), "uncommitted edits are not validated: {error}");
    }

    #[tokio::test]
    async fn candidate_checks_registered_ops_entries_and_fails_on_unknown_fields() {
        flotilla_core::tls::install_default_provider();
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
        let inspector = GitRepositoryInspector::new(runner, Arc::new(FixedVcsResolver(Arc::new(vcs))), VALIDATION_INSPECTION_HOST);
        let error = validate_project_ops(std::slice::from_ref(&project), &std::collections::BTreeMap::new(), &inspector)
            .await
            .expect_err("candidate rejects unknown ops field");
        assert!(error.to_string().contains("governor.md"));
        assert!(error.to_string().contains("unknown_option"));
        // Exercise the actual candidate path against a previous-generation API,
        // including continued checking when a peer cannot provide local roots.
        use std::sync::atomic::{AtomicUsize, Ordering};

        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::UnixListener,
        };
        let socket_dir = TestSocketDir::new();
        let socket = socket_dir.socket_path("old-api.sock");
        let listener = UnixListener::bind(&socket).expect("old daemon socket");
        let document = serde_json::to_value(project.to_k8s_object()).expect("project API record");
        let endpoint_reads = Arc::new(AtomicUsize::new(0));
        let reads = endpoint_reads.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("accept validator");
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut bytes = [0; 1024];
                    let count = stream.read(&mut bytes).await.expect("request bytes");
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&bytes[..count]);
                }
                let request = String::from_utf8(request).expect("HTTP request");
                let path = request.split_whitespace().nth(1).expect("request path").split('?').next().expect("path");
                let (status, body) = if path == "/apis/flotilla.work/v1" {
                    ("200 OK", serde_json::json!({"kinds":["projects"], "namespaces":{"projects":["flotilla","other"]}}))
                } else if path.ends_with("/projects") {
                    let mut project = document.clone();
                    if path.contains("/other/") {
                        project["metadata"]["namespace"] = serde_json::json!("other");
                    }
                    ("200 OK", serde_json::json!({"items":[project]}))
                } else {
                    assert!(path.ends_with("/operationalentries"), "{path}");
                    reads.fetch_add(1, Ordering::SeqCst);
                    ("404 Not Found", serde_json::json!({"error":"unknown endpoint"}))
                };
                let body = body.to_string();
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("old API response");
            }
        });
        let error =
            validate_daemon(&socket, Some(std::slice::from_ref(&tmp)), None).await.expect_err("candidate fallback parses invalid ops");
        assert!(error.to_string().contains("resource validation failed"));
        assert_eq!(endpoint_reads.load(Ordering::SeqCst), 2, "both namespaces inspected by the candidate");
        endpoint_reads.store(0, Ordering::SeqCst);
        let _ = validate_daemon(&socket, None, None).await.expect_err("a peer without inventory must fail closed");
        assert_eq!(endpoint_reads.load(Ordering::SeqCst), 2, "a missing root must not skip the remaining namespace");
        server.abort();

        let mut unavailable = project.clone();
        unavailable.metadata.annotations.clear();
        // This host's daemon cannot load a source it has no checkout of; the
        // hosts holding a checkout validate it at their own install.
        let validated = validate_project_ops(&[unavailable], &std::collections::BTreeMap::new(), &inspector)
            .await
            .expect("a source without a local checkout is reported, not refused");
        assert_eq!(validated, 0);
        let worktree_guard = tempfile::tempdir().expect("worktree directory");
        let mut worktrees = Vec::new();
        for name in ["convoy-a", "convoy-b"] {
            let worktree = worktree_guard.path().join(name);
            assert!(std::process::Command::new("git")
                .args(["worktree", "add", "-b", &format!("convoy/{name}")])
                .arg(&worktree)
                .current_dir(&tmp)
                .status()
                .expect("convoy worktree")
                .success());
            worktrees.push(worktree);
        }
        let mut worktrees_only = project.clone();
        worktrees_only.metadata.annotations.clear();
        let worktree_paths = inspect_validation_roots(&worktrees, &inspector).await;
        let validated = validate_project_ops(&[worktrees_only], &worktree_paths, &inspector)
            .await
            .expect("a host holding only convoy worktrees reports the source unavailable");
        assert_eq!(validated, 0);
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
        let paths = inspect_validation_roots(&[tmp.clone(), duplicate.clone()], &inspector).await;
        let error = validate_project_ops(&[project], &paths, &inspector).await.expect_err("ambiguous main checkouts fail the gate");
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
