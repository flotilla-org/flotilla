use std::collections::{BTreeMap, HashSet};

use chrono::{Duration, Utc};
use flotilla_protocol::CallerCrew;
use flotilla_resources::{
    artifact_record_name, Artifact, ArtifactSpec, Convoy, InputMeta, OwnerReference, Resource, ResourceBackend, ResourceError,
    ResourceObject, TerminalSession, TerminalSessionSource,
};

use crate::blob_store::{BlobDigest, BlobStore};

const DEFAULT_RETENTION_DAYS: u64 = 30;
const MAX_SUMMARY_BYTES: usize = 4096;

#[derive(Debug, bon::Builder)]
pub struct ArtifactPutInput {
    pub kind: String,
    pub subject: String,
    #[builder(default)]
    pub summary: BTreeMap<String, serde_json::Value>,
    pub media_type: String,
    pub body: Vec<u8>,
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
        if input.summary.values().any(|value| !value.is_string() && !value.is_number() && !value.is_boolean()) {
            return Err("artifact summary values must be string, number, or boolean scalars".into());
        }
        if serde_json::to_vec(&input.summary).map_err(|error| error.to_string())?.len() > MAX_SUMMARY_BYTES {
            return Err("artifact summary exceeds 4096 bytes".into());
        }
        let digest = self.blobs.put(&input.body).await?;
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
            .size(input.body.len() as u64)
            .media_type(input.media_type)
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
        Ok(session.spec.role.clone())
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
        let digest = if let Ok(digest) = BlobDigest::parse(reference) {
            digest
        } else {
            let name = reference.strip_prefix("artifact/").unwrap_or(reference);
            let object = self.backend.including_replicas::<Artifact>(self.namespace).get(name).await.map_err(|error| error.to_string())?;
            BlobDigest::parse(&object.object.spec.digest)?
        };
        self.blobs.get(&digest).await?.ok_or_else(|| format!("artifact blob {} is unavailable", digest.as_str()))
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

    use super::*;
    use crate::blob_store::MemoryBlobStore;

    fn input(subject: &str, summary: BTreeMap<String, serde_json::Value>, body: &[u8]) -> ArtifactPutInput {
        ArtifactPutInput::builder()
            .kind("review-round".to_string())
            .subject(subject.to_string())
            .summary(summary)
            .media_type("text/plain".to_string())
            .body(body.to_vec())
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
                        brief: TerminalBrief { path: "brief.md".into(), content: String::new(), copies: vec![] },
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
