use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use flotilla_core::in_process::BriefArtifactWriter;
use flotilla_protocol::CallerCrew;
use flotilla_resources::{
    artifact_record_name, Artifact, ArtifactSpec, Convoy, InputMeta, OwnerReference, Resource, ResourceBackend, ResourceError,
    ResourceObject, TerminalSession, TerminalSessionSource,
};

use crate::blob_store::{BlobDigest, BlobStore, TieredBlobStore};

const DEFAULT_RETENTION_DAYS: u64 = 30;
const MAX_SUMMARY_BYTES: usize = 4096;
const MAX_DECISION_LEDGER_BYTES: usize = 32 * 1024;

pub(crate) fn validate_decision_ledger(body: &[u8]) -> Result<(), String> {
    if body.is_empty() {
        return Err("decision ledger is empty".to_string());
    }
    if body.len() > MAX_DECISION_LEDGER_BYTES {
        return Err(format!("decision ledger exceeds {MAX_DECISION_LEDGER_BYTES} bytes"));
    }
    let text = std::str::from_utf8(body).map_err(|_| "decision ledger must be UTF-8".to_string())?;
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    if lines.next() != Some("## Decision ledger") {
        return Err("decision ledger must start with `## Decision ledger`".to_string());
    }
    let mut entries = 0;
    while let Some(line) = lines.next() {
        entries += 1;
        let prefix = format!("{entries}. **Brief silence:** ");
        if !line.starts_with(&prefix) || line[prefix.len()..].trim().is_empty() {
            return Err(format!("decision ledger entry {entries} must start with `**Brief silence:**`"));
        }
        for field in ["Choice", "Alternative", "If asking were free"] {
            let prefix = format!("- **{field}:** ");
            let value = lines.next().ok_or_else(|| format!("decision ledger entry {entries} is missing `{field}`"))?;
            if !value.starts_with(&prefix) || value[prefix.len()..].trim().is_empty() {
                return Err(format!("decision ledger entry {entries} needs a nonempty `{field}` field"));
            }
        }
    }
    if entries == 0 {
        return Err("decision ledger needs at least one numbered entry".to_string());
    }
    Ok(())
}

pub(crate) fn read_decision_ledger(path: &Path) -> Result<Vec<u8>, String> {
    let size = std::fs::metadata(path).map_err(|error| format!("stat decision ledger: {error}"))?.len();
    if size > MAX_DECISION_LEDGER_BYTES as u64 {
        return Err(format!("decision ledger exceeds {MAX_DECISION_LEDGER_BYTES} bytes"));
    }
    let body = std::fs::read(path).map_err(|error| format!("read decision ledger: {error}"))?;
    validate_decision_ledger(&body)?;
    Ok(body)
}

pub struct SystemBriefArtifactWriter {
    pub backend: ResourceBackend,
    pub blobs: Arc<TieredBlobStore>,
    pub retention_days: u64,
}

#[async_trait]
impl BriefArtifactWriter for SystemBriefArtifactWriter {
    async fn put_brief(&self, namespace: &str, convoy: &str, role: &str, subject: &str, content: &[u8]) -> Result<String, String> {
        let digest = self.blobs.put_with_media_type(content, "text/markdown").await?;
        let name = artifact_record_name(convoy, role, "brief", subject);
        let resolver = self.backend.using::<Artifact>(namespace);
        let prior = match resolver.get(&name).await {
            Ok(prior) => Some(prior),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error.to_string()),
        };
        let expires_at = i64::try_from(self.retention_days)
            .ok()
            .and_then(Duration::try_days)
            .and_then(|duration| Utc::now().checked_add_signed(duration))
            .ok_or_else(|| "brief retention is too large".to_string())?;
        let spec = ArtifactSpec::builder()
            .convoy(convoy.to_string())
            .producer(role.to_string())
            .kind("brief".to_string())
            .subject(subject.to_string())
            .digest(digest.as_str().to_string())
            .size(content.len() as u64)
            .media_type("text/markdown".to_string())
            .expires_at(expires_at)
            .pinned(prior.as_ref().is_some_and(|artifact| artifact.spec.pinned))
            .build();
        let owner = OwnerReference {
            api_version: format!("{}/{}", Convoy::API_PATHS.group, Convoy::API_PATHS.version),
            kind: Convoy::API_PATHS.kind.to_string(),
            name: convoy.to_string(),
            controller: false,
        };
        let meta = InputMeta::builder().name(name).owner_references(vec![owner]).build();
        match prior {
            Some(prior) => resolver.update(&meta, &prior.metadata.resource_version, &spec).await.map_err(|error| error.to_string())?,
            None => resolver.create(&meta, &spec).await.map_err(|error| error.to_string())?,
        };
        Ok(digest.as_str().to_string())
    }
}

#[derive(Debug, bon::Builder)]
pub struct ArtifactPutInput {
    pub kind: String,
    pub subject: String,
    #[builder(default)]
    pub summary: BTreeMap<String, serde_json::Value>,
    pub media_type: String,
    pub body: ArtifactBody,
}

#[derive(Debug)]
pub enum ArtifactBody {
    Bytes(Vec<u8>),
    File(PathBuf),
}

/// The daemon owns both the envelope write and every blob-store operation.
pub struct ArtifactService<'a> {
    pub backend: &'a ResourceBackend,
    pub blobs: &'a dyn BlobStore,
    pub namespace: &'a str,
}

impl ArtifactService<'_> {
    pub async fn put(
        &self,
        caller: &CallerCrew,
        input: ArtifactPutInput,
        retention_days: &BTreeMap<String, u64>,
    ) -> Result<ResourceObject<Artifact>, String> {
        let (name, spec, owner) = self.prepare_put(caller, input, retention_days).await?;
        let resolver = self.backend.using::<Artifact>(self.namespace);
        let prior = match resolver.get(&name).await {
            Ok(prior) => Some(prior),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error.to_string()),
        };
        let meta = InputMeta::builder().name(name).owner_references(vec![owner]).build();
        match prior {
            Some(prior) => resolver.update(&meta, &prior.metadata.resource_version, &spec).await.map_err(|error| error.to_string()),
            None => resolver.create(&meta, &spec).await.map_err(|error| error.to_string()),
        }
    }

    pub async fn prepare_put(
        &self,
        caller: &CallerCrew,
        input: ArtifactPutInput,
        retention_days: &BTreeMap<String, u64>,
    ) -> Result<(String, ArtifactSpec, OwnerReference), String> {
        let producer = self.stamped_role(caller).await?;
        if input.kind.is_empty() || input.subject.is_empty() || input.media_type.is_empty() {
            return Err("artifact kind, subject, and media type must be nonempty".into());
        }
        if input.kind == "decision-ledger" && input.subject != caller.convoy {
            return Err("decision ledger subject must be its convoy".into());
        }
        if input.summary.values().any(|value| !value.is_string() && !value.is_number() && !value.is_boolean()) {
            return Err("artifact summary values must be string, number, or boolean scalars".into());
        }
        if serde_json::to_vec(&input.summary).map_err(|error| error.to_string())?.len() > MAX_SUMMARY_BYTES {
            return Err("artifact summary exceeds 4096 bytes".into());
        }
        if input.kind == "decision-ledger" {
            match &input.body {
                ArtifactBody::Bytes(bytes) => validate_decision_ledger(bytes)?,
                ArtifactBody::File(path) => {
                    read_decision_ledger(path)?;
                }
            }
        }
        let (digest, size) = match &input.body {
            ArtifactBody::Bytes(bytes) => (self.blobs.put_with_media_type(bytes, &input.media_type).await?, bytes.len() as u64),
            ArtifactBody::File(path) => self.blobs.put_file_with_media_type(path, &input.media_type).await?,
        };
        let name = artifact_record_name(&caller.convoy, &producer, &input.kind, &input.subject);
        let prior = match self.backend.including_replicas::<Artifact>(self.namespace).get(&name).await {
            Ok(prior) => Some(prior.object),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error.to_string()),
        };
        let days = retention_days.get(&input.kind).copied().unwrap_or(DEFAULT_RETENTION_DAYS);
        let expires_at = i64::try_from(days)
            .ok()
            .and_then(Duration::try_days)
            .and_then(|duration| Utc::now().checked_add_signed(duration))
            .ok_or_else(|| format!("artifact retention for `{}` is too large", input.kind))?;
        let spec = ArtifactSpec::builder()
            .convoy(caller.convoy.clone())
            .producer(producer)
            .kind(input.kind)
            .subject(input.subject)
            .summary(input.summary)
            .digest(digest.as_str().to_string())
            .size(size)
            .media_type(input.media_type)
            .recorded_at(Utc::now())
            .expires_at(expires_at)
            .pinned(prior.as_ref().is_some_and(|object| object.spec.pinned))
            .build();
        let owner = OwnerReference {
            api_version: format!("{}/{}", Convoy::API_PATHS.group, Convoy::API_PATHS.version),
            kind: Convoy::API_PATHS.kind.to_string(),
            name: caller.convoy.clone(),
            controller: false,
        };
        Ok((name, spec, owner))
    }

    async fn stamped_role(&self, caller: &CallerCrew) -> Result<String, String> {
        Ok(self.caller_session(caller).await?.spec.role)
    }

    pub async fn caller_session(&self, caller: &CallerCrew) -> Result<ResourceObject<TerminalSession>, String> {
        if caller.namespace != self.namespace {
            return Err("crew namespace does not match daemon namespace".into());
        }
        let sessions = self.backend.using::<TerminalSession>(self.namespace).list().await.map_err(|error| error.to_string())?;
        let session = sessions
            .items
            .iter()
            .find(|session| session.status.as_ref().and_then(|status| status.crew.as_ref()).is_some_and(|crew| crew.id == caller.crew_id))
            .ok_or_else(|| "calling process has no active crew session".to_string())?;
        if session.spec.role != caller.role
            || !matches!(&session.spec.source, TerminalSessionSource::Agent { context, .. }
                if context.convoy == caller.convoy && context.vessel_ref == caller.vessel)
        {
            return Err("calling crew identity does not match daemon session".into());
        }
        Ok(session.clone())
    }

    pub async fn list(
        &self,
        convoy: Option<&str>,
        kind: Option<&str>,
        subject: Option<&str>,
    ) -> Result<Vec<ResourceObject<Artifact>>, String> {
        let mut items = self
            .backend
            .including_replicas::<Artifact>(self.namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .map(|item| item.object)
            .filter(|item| convoy.is_none_or(|value| item.spec.convoy == value))
            .filter(|item| kind.is_none_or(|value| item.spec.kind == value))
            .filter(|item| subject.is_none_or(|value| item.spec.subject == value))
            .collect::<Vec<_>>();
        items.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
        Ok(items)
    }

    pub async fn get(&self, reference: &str) -> Result<Vec<u8>, String> {
        let digest = self.resolve_digest(reference).await?;
        self.blobs.get(&digest).await?.ok_or_else(|| format!("artifact blob {} is unavailable", digest.as_str()))
    }

    pub async fn get_to_file(&self, reference: &str, path: &Path) -> Result<u64, String> {
        let digest = self.resolve_digest(reference).await?;
        self.blobs.get_file(&digest, path).await?.ok_or_else(|| format!("artifact blob {} is unavailable", digest.as_str()))
    }

    pub async fn artifact_for_reference(&self, reference: &str) -> Result<Option<ResourceObject<Artifact>>, String> {
        if BlobDigest::parse(reference).is_ok() {
            Ok(None)
        } else {
            let name = reference.strip_prefix("artifact/").unwrap_or(reference);
            let (namespace, name) = name.split_once('/').unwrap_or((self.namespace, name));
            self.backend
                .including_replicas::<Artifact>(namespace)
                .get(name)
                .await
                .map(|item| Some(item.object))
                .map_err(|error| error.to_string())
        }
    }

    async fn resolve_digest(&self, reference: &str) -> Result<BlobDigest, String> {
        let digest = if let Ok(digest) = BlobDigest::parse(reference) {
            digest
        } else {
            let object = self.artifact_for_reference(reference).await?.ok_or("artifact record is unavailable")?;
            BlobDigest::parse(&object.spec.digest)?
        };
        Ok(digest)
    }

    pub async fn reap_expired(&self) -> Result<HashSet<BlobDigest>, String> {
        let resolver = self.backend.using::<Artifact>(self.namespace);
        let now = Utc::now();
        for artifact in resolver.list().await.map_err(|error| error.to_string())?.items {
            if !artifact.spec.pinned && artifact.spec.expires_at <= now {
                resolver.delete(&artifact.metadata.name).await.map_err(|error| error.to_string())?;
            }
        }
        let refs = self.list(None, None, None).await?;
        refs.into_iter().map(|item| BlobDigest::parse(&item.spec.digest)).collect()
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::CallerCrew;
    use flotilla_resources::{
        apply_resource_document, InMemoryBackend, Selector, SqliteBackend, TerminalBrief, TerminalCrewContext, TerminalSessionSpec,
        TerminalSessionStatus,
    };

    use super::{ArtifactBody, *};
    use crate::blob_store::MemoryBlobStore;

    #[tokio::test]
    async fn brief_reprovisions_from_fleet_digest_after_convoy_reap() {
        let home_dir = tempfile::tempdir().expect("home state");
        let remote_dir = tempfile::tempdir().expect("remote state");
        let fleet: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::default());
        let home_blobs = Arc::new(TieredBlobStore::new(home_dir.path(), vec![("fleet".into(), Arc::clone(&fleet))]));
        let remote_blobs = TieredBlobStore::new(remote_dir.path(), vec![("fleet".into(), fleet)]);
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let namespace = "flotilla";
        let convoy = "convoy-cross-host";
        backend
            .using::<Convoy>(namespace)
            .create(
                &InputMeta::builder().name(convoy.to_string()).build(),
                &flotilla_resources::ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
            )
            .await
            .expect("create convoy");
        let writer = SystemBriefArtifactWriter { backend: backend.clone(), blobs: Arc::clone(&home_blobs), retention_days: 3650 };
        let body = b"# crew brief\nExact bytes survive another host.\n";
        let digest = writer.put_brief(namespace, convoy, "coder", convoy, body).await.expect("write admission brief");
        let artifact = backend
            .using::<Artifact>(namespace)
            .get(&artifact_record_name(convoy, "coder", "brief", convoy))
            .await
            .expect("brief envelope");
        assert_eq!(artifact.spec.subject, convoy);
        assert_eq!(artifact.spec.digest, digest);
        assert!(!artifact.metadata.owner_references[0].controller);
        home_blobs.sync_once().await.expect("sync to fleet store");
        backend.using::<Convoy>(namespace).delete(convoy).await.expect("reap convoy");
        assert!(backend.using::<Artifact>(namespace).get(&artifact.metadata.name).await.is_ok());
        let fetched = remote_blobs.get(&BlobDigest::parse(&digest).expect("valid digest")).await.expect("fetch on remote host");
        assert_eq!(fetched.as_deref(), Some(body.as_slice()));
    }

    #[test]
    fn decision_ledger_requires_a_complete_entry_and_bounded_content() {
        let valid = b"## Decision ledger\n\n1. **Brief silence:** The output name\n- **Choice:** report.md\n- **Alternative:** output.md\n- **If asking were free:** Which name?\n";
        assert!(validate_decision_ledger(valid).is_ok());
        assert!(validate_decision_ledger(b"/tmp/ledger.md").is_err());
        assert!(validate_decision_ledger(b"## Decision ledger\nNo decisions beyond the brief.\n").is_err());
        assert!(validate_decision_ledger(&vec![b'x'; MAX_DECISION_LEDGER_BYTES + 1]).is_err());
        let file = tempfile::NamedTempFile::new().expect("ledger file");
        std::fs::write(file.path(), valid).expect("write ledger");
        assert_eq!(read_decision_ledger(file.path()).expect("daemon reads body"), valid);
        std::fs::write(file.path(), b"/tmp/ledger.md").expect("write path text");
        assert!(read_decision_ledger(file.path()).is_err());
    }

    fn input(subject: &str, summary: BTreeMap<String, serde_json::Value>, body: &[u8]) -> ArtifactPutInput {
        ArtifactPutInput::builder()
            .kind("review-round".to_string())
            .subject(subject.to_string())
            .summary(summary)
            .media_type("text/plain".to_string())
            .body(ArtifactBody::Bytes(body.to_vec()))
            .build()
    }

    async fn contract(backend: ResourceBackend) {
        let namespace = "flotilla";
        let sessions = backend.using::<TerminalSession>(namespace);
        let session = sessions
            .create(
                &InputMeta::builder().name("terminal-demo-work-coder".to_string()).build(),
                &TerminalSessionSpec::builder()
                    .env_ref("env".to_string())
                    .role("coder".to_string())
                    .source(TerminalSessionSource::Agent {
                        selector: Selector::for_capability("coding"),
                        brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: String::new(), copies: vec![] },
                        context: Box::new(TerminalCrewContext {
                            namespace: namespace.into(),
                            convoy: "demo".into(),
                            vessel_ref: "demo-work".into(),
                        }),
                        message: None,
                    })
                    .cwd("/work".to_string())
                    .pool("cleat".to_string())
                    .build(),
            )
            .await
            .expect("create session");
        sessions
            .update_status(&session.metadata.name, &session.metadata.resource_version, &TerminalSessionStatus {
                crew: Some(
                    flotilla_resources::CrewSessionStatus::builder()
                        .id("crew-1".to_string())
                        .adapter("codex".to_string())
                        .stance("trusted".to_string())
                        .build(),
                ),
                ..TerminalSessionStatus::default()
            })
            .await
            .expect("mark crew");
        let blobs = MemoryBlobStore::default();
        let retention = BTreeMap::from([("review-round".to_string(), 10)]);
        let service = ArtifactService { backend: &backend, blobs: &blobs, namespace };
        let caller = CallerCrew::builder()
            .namespace(namespace.to_string())
            .convoy("demo".to_string())
            .vessel("demo-work".to_string())
            .role("coder".to_string())
            .crew_id("crew-1".to_string())
            .build();
        for (mut invalid, expected) in [
            (input("head", BTreeMap::new(), b"body"), "kind"),
            (input("", BTreeMap::new(), b"body"), "subject"),
            (input("head", BTreeMap::new(), b"body"), "media type"),
            (input("head", BTreeMap::from([("items".into(), serde_json::json!([1, 2]))]), b"body"), "scalar"),
            (input("head", BTreeMap::from([("large".into(), serde_json::json!("x".repeat(4096)))]), b"body"), "4096"),
        ] {
            match expected {
                "kind" => invalid.kind.clear(),
                "media type" => invalid.media_type.clear(),
                _ => {}
            }
            let error = service.prepare_put(&caller, invalid, &retention).await.expect_err("reject invalid artifact input");
            assert!(error.contains(expected), "unexpected validation error: {error}");
        }
        let first = service
            .put(&caller, input("head-1", BTreeMap::from([("disposition".into(), serde_json::json!("approve"))]), b"one"), &retention)
            .await
            .expect("put first body");
        assert_eq!(first.spec.producer, "coder");
        assert!(!first.metadata.owner_references[0].controller);
        assert_eq!(service.get(&format!("artifact/{}", first.metadata.name)).await.expect("get address"), b"one");
        assert_eq!(service.get(&first.spec.digest).await.expect("get digest"), b"one");
        let second = service.put(&caller, input("head-1", BTreeMap::new(), b"two"), &retention).await.expect("replace latest");
        assert_eq!(second.metadata.name, first.metadata.name);
        assert_eq!(service.list(Some("demo"), Some("review-round"), Some("head-1")).await.expect("list").len(), 1);
        assert_eq!(service.get(&second.metadata.name).await.expect("get latest"), b"two");
        let remote_home = ResourceBackend::InMemory(InMemoryBackend::default());
        let (remote_name, remote_spec, remote_owner) =
            service.prepare_put(&caller, input("head-2", BTreeMap::new(), b"remote"), &retention).await.expect("prepare remote envelope");
        apply_resource_document(
            &remote_home,
            namespace,
            serde_json::json!({
                "apiVersion": "flotilla.work/v1",
                "kind": "Artifact",
                "metadata": {"name": remote_name.clone(), "ownerReferences": [remote_owner]},
                "spec": remote_spec,
            }),
        )
        .await
        .expect("apply envelope on convoy home");
        assert!(matches!(backend.using::<Artifact>(namespace).get(&remote_name).await, Err(ResourceError::NotFound { .. })));
        assert_eq!(remote_home.using::<Artifact>(namespace).get(&remote_name).await.expect("remote owns envelope").spec.producer, "coder");
        let mut spoofed = caller.clone();
        spoofed.role = "reviewer".into();
        assert!(service.put(&spoofed, input("head-1", BTreeMap::new(), b"spoof"), &retention).await.is_err());

        let resolver = backend.using::<Artifact>(namespace);
        let mut expired = second.spec.clone();
        expired.expires_at = Utc::now() - Duration::days(1);
        resolver
            .update(
                &InputMeta::builder().name(second.metadata.name.clone()).owner_references(second.metadata.owner_references.clone()).build(),
                &second.metadata.resource_version,
                &expired,
            )
            .await
            .expect("expire artifact");
        assert!(service.reap_expired().await.expect("reap").is_empty());
        assert!(service.list(None, None, None).await.expect("list after reap").is_empty());

        expired.subject = "pinned-head".into();
        expired.pinned = true;
        let pinned_name = artifact_record_name("demo", "coder", "review-round", "pinned-head");
        resolver.create(&InputMeta::builder().name(pinned_name).build(), &expired).await.expect("create pinned artifact");
        assert_eq!(service.reap_expired().await.expect("reap pinned").len(), 1);
        assert_eq!(service.list(None, None, None).await.expect("list pinned").len(), 1);
    }

    #[tokio::test]
    async fn artifact_contract_in_memory() {
        contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
    }

    #[tokio::test]
    async fn artifact_contract_sqlite() {
        contract(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))).await;
    }
}
