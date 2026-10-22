use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::CrewCommandContext;
use flotilla_resources::{
    CrewWorkPhase, InputMeta, ResourceObject, TerminalSession as ResourceTerminalSession,
    TerminalSessionPhase as ResourceTerminalSessionPhase,
};
use tokio::sync::{Mutex, RwLock};

use super::fixture;
use flotilla_resources::CREDENTIAL_REFS_ANNOTATION;

#[tokio::test]
async fn capabilities_use_live_deliveries_and_supersede_changed_cards() {
    use flotilla_resources::{Environment, EnvironmentSpec, HostDirectEnvironmentSpec, Message, MessageReference, Vessel, VesselSpec};

    use flotilla_capability_contract::{CredentialCapability, SessionCapabilitySource};
    // In-memory port stands in for credential mint/delivery I/O.
    struct Delivery(RwLock<Vec<CredentialCapability>>, RwLock<BTreeMap<String, String>>);
    #[async_trait]
    impl SessionCapabilitySource for Delivery {
        async fn credentials(&self, environment: &str, references: &BTreeSet<String>) -> Result<Vec<CredentialCapability>, String> {
            assert_eq!(environment, "resume-env");
            assert_eq!(references, &BTreeSet::from(["github".into()]));
            Ok(self.0.read().await.clone())
        }
        async fn endpoints(&self, _: &str, _: &str) -> Result<BTreeMap<String, String>, String> {
            Ok(self.1.read().await.clone())
        }
    }
    let (crew, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    backend
        .using::<Vessel>("flotilla")
        .create(
            &InputMeta::builder().name("vessel".into()).build(),
            &VesselSpec {
                convoy_ref: "crew".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
    backend
        .using::<Environment>("flotilla")
        .create(
            &InputMeta::builder().name("resume-env".into()).build(),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/repo".into() }),
                docker: None,
            },
        )
        .await
        .expect("environment");
    let source = Arc::new(Delivery(
        RwLock::new(vec![CredentialCapability::builder()
            .name("github".into())
            .repositories(vec!["flotilla-org/flotilla".into()])
            .permissions(BTreeMap::from([("workflows".into(), "write".into()), ("contents".into(), "write".into())]))
            .build()]),
        RwLock::new(BTreeMap::new()),
    ));
    *crew.capability_source.write().await = Some(source.clone());
    let context = CrewCommandContext::builder()
        .namespace("flotilla".into())
        .convoy("crew".into())
        .vessel_ref("vessel".into())
        .role("coder".into())
        .build();
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("session").await.expect("session");
    let mut meta = InputMeta::from(&session.metadata);
    meta.annotations.insert(CREDENTIAL_REFS_ANNOTATION.into(), "[\"github\"]".into());
    sessions.update(&meta, &session.metadata.resource_version, &session.spec).await.expect("session grant references");
    let healthy = sessions.get("session").await.expect("healthy session");
    let mut status = healthy.status.clone().unwrap_or_default();
    status.phase = ResourceTerminalSessionPhase::Running;
    sessions.update_status("session", &healthy.metadata.resource_version, &status).await.expect("running healthy session");
    // A malformed running session sorts before the healthy session and must
    // not starve it of observations. Its different role cannot receive this card.
    let mut broken_spec = healthy.spec.clone();
    broken_spec.role = "broken".into();
    let broken = sessions
        .create(
            &InputMeta::builder()
                .name("000-malformed".into())
                .annotations(BTreeMap::from([(CREDENTIAL_REFS_ANNOTATION.into(), "not json".into())]))
                .build(),
            &broken_spec,
        )
        .await
        .expect("broken session");
    sessions.update_status("000-malformed", &broken.metadata.resource_version, &status).await.expect("running broken session");
    let first = crew.query_capabilities(&context).await.expect("first live card");
    assert!(first.contains("You can push `.github/workflows`"));
    let session = sessions.get("session").await.expect("session");
    crate::crew_capabilities::observe_card(&backend, "flotilla", &session, &first, &crew.capability_cards).await.expect("launch baseline");
    assert!(backend.using::<Message>("flotilla").list().await.expect("messages").items.is_empty());
    for (index, permissions) in [BTreeMap::from([("contents".into(), "read".into())]), BTreeMap::new()].into_iter().enumerate() {
        source.0.write().await[0].permissions = Some(permissions);
        let card = crew.query_capabilities(&context).await.expect("refreshed card");
        assert!(!card.contains("You can push `.github/workflows`"));
        let session = sessions.get("session").await.expect("session");
        if index == 0 {
            // A concurrent holder status write can race successful publication.
            // Change the source again before retrying: recover the published
            // revision, then supersede it rather than collide or deliver twice.
            let mut status = session.status.clone().unwrap_or_default();
            status.session_id = Some("concurrent-terminal-observation".into());
            sessions.update_status("session", &session.metadata.resource_version, &status).await.expect("concurrent status");
            assert!(crate::crew_capabilities::observe_card(&backend, "flotilla", &session, &card, &crew.capability_cards).await.is_err());
            continue;
        }
        let error = crate::crew_capabilities::refresh_cards(&backend, "flotilla", source.as_ref(), &crew.capability_cards)
            .await
            .expect_err("aggregate error");
        assert!(error.contains("000-malformed"));
        assert!(error.contains("invalid session credential references"));
        assert_eq!(
            sessions.get("session").await.expect("healthy refresh").metadata.annotations["flotilla.work/capabilities-revision"],
            "2"
        );
        let session = sessions.get("session").await.expect("session");
        crate::crew_capabilities::observe_card(&backend, "flotilla", &session, &card, &crew.capability_cards)
            .await
            .expect("supersede recovered revision");
        crate::crew_capabilities::observe_card(
            &backend,
            "flotilla",
            &sessions.get("session").await.expect("session"),
            &card,
            &crew.capability_cards,
        )
        .await
        .expect("duplicate observation");
    }
    source.1.write().await.insert("pinned service".into(), "127.0.0.1:7423".into());
    let card = crew.query_capabilities(&context).await.expect("endpoint change");
    assert!(card.contains("`127.0.0.1:7423` → pinned service"));
    crate::crew_capabilities::observe_card(
        &backend,
        "flotilla",
        &sessions.get("session").await.expect("session"),
        &card,
        &crew.capability_cards,
    )
    .await
    .expect("environment notification");
    source.0.write().await.clear();
    let card = crew.query_capabilities(&context).await.expect("revoked credential");
    assert!(!card.contains("Credential `github`"));
    crate::crew_capabilities::observe_card(
        &backend,
        "flotilla",
        &sessions.get("session").await.expect("session"),
        &card,
        &crew.capability_cards,
    )
    .await
    .expect("revocation notification");
    let messages = backend.using::<Message>("flotilla").list().await.expect("messages").items;
    assert_eq!(messages.len(), 4);
    let latest_revision = |message: &&ResourceObject<Message>| {
        let Some(MessageReference::ControlRecord { revision, .. }) = &message.spec.subject else { return false };
        revision == "capabilities@5"
    };
    let latest = messages.iter().find(latest_revision).expect("superseding delta");
    assert!(messages.iter().any(|message| Some(&message.metadata.name) == latest.spec.supersedes.as_ref()));
    assert_eq!(latest.spec.sender, "system:capabilities");
    assert_eq!(latest.spec.body, crate::crew_capabilities::card_delta(&first, &card));
    // Terminal transport is the I/O boundary. A busy holder gets no input;
    // after its turn boundary only the latest card is submitted, once.
    struct Transport {
        working: AtomicBool,
        submitted: Mutex<Vec<flotilla_agent_connector::Delivery>>,
    }
    #[async_trait]
    impl flotilla_agent_connector::AgentTransport for Transport {
        fn id(&self) -> flotilla_agent_connector::TransportId {
            flotilla_agent_connector::KEYSTROKE.into()
        }
        async fn observe(
            &self,
            _: &flotilla_agent_connector::DeliveryTarget,
            _: Option<&flotilla_agent_connector::SubmissionRef>,
        ) -> Result<flotilla_agent_connector::TransportObservation, String> {
            let working = self.working.load(Ordering::SeqCst);
            Ok(flotilla_agent_connector::TransportObservation {
                ready: !working,
                working,
                evidence: Some("terminal boundary".into()),
                ..Default::default()
            })
        }
        async fn submit(&self, batch: &flotilla_agent_connector::Delivery) -> flotilla_agent_connector::TransportOutcome {
            self.submitted.lock().await.push(batch.clone());
            flotilla_agent_connector::TransportOutcome::Accepted { evidence: "accepted".into() }
        }
        async fn poll(&self, _: &flotilla_agent_connector::Delivery) -> flotilla_agent_connector::TransportOutcome {
            flotilla_agent_connector::TransportOutcome::Accepted { evidence: "accepted".into() }
        }
    }
    let session = sessions.get("session").await.expect("session");
    sessions
        .update_status(
            "session",
            &session.metadata.resource_version,
            &flotilla_resources::TerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                session_id: Some("terminal".into()),
                crew: Some(
                    flotilla_resources::CrewSessionStatus::builder()
                        .id("crew-id".into())
                        .adapter("codex".into())
                        .stance("trusted".into())
                        .build(),
                ),
                ..Default::default()
            },
        )
        .await
        .expect("running holder");
    let transport = Arc::new(Transport { working: AtomicBool::new(true), submitted: Mutex::new(Vec::new()) });
    let router = flotilla_agent_connector::TransportRouter::new().with_transport(transport.clone());
    let inbox = flotilla_messaging::MessageInbox::new(backend, "flotilla");
    inbox.reconcile_delivery(&router, Utc::now()).await.expect("busy holder");
    assert!(transport.submitted.lock().await.is_empty());
    transport.working.store(false, Ordering::SeqCst);
    inbox.reconcile_delivery(&router, Utc::now()).await.expect("turn boundary");
    inbox.reconcile_delivery(&router, Utc::now()).await.expect("idempotent receipt");
    let submitted = transport.submitted.lock().await;
    assert_eq!(submitted.len(), 1);
    // The latest revision is traceable through the submission and stored subject, not a JSON header (#2923).
    assert_eq!(submitted[0].submission.members, vec![latest.metadata.name.clone()]);
    assert!(
        matches!(&latest.spec.subject, Some(flotilla_resources::MessageReference::ControlRecord { revision, .. }) if revision == "capabilities@5")
    );
    assert_eq!(submitted[0].text, format!("[from flotilla (system) · capabilities · re TerminalSession session]\n\n{}", latest.spec.body));
}

// Handoffs route to the addressed vessel, preserve typed carries, and admit once per command.
// Generated scenarios cover local and remote receiver homes, latent and completed work,
// duplicate text, and invalid targets. The receiver has no terminal in either initial phase.

// Old annotations remain readable and receive one full card; receipt then establishes a delta baseline.
#[tokio::test]
async fn previous_generation_gets_full_card_once_then_delivered_deltas() {
    use crate::crew_capabilities::{card_delta, observe_card, CapabilityCardTracker};
    use flotilla_resources::{Message, MessagePhase, Vessel, VesselSpec};
    use sha2::{Digest, Sha256};
    let (_, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    backend
        .using::<Vessel>("flotilla")
        .create(
            &InputMeta::builder().name("vessel".into()).build(),
            &VesselSpec {
                convoy_ref: "crew".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("session").await.expect("session");
    let first = "## Your capabilities\n### Credentials\n- known\n";
    let mut meta = InputMeta::from(&session.metadata);
    meta.annotations.insert("flotilla.work/capabilities-digest".into(), format!("{:x}", Sha256::digest(first.as_bytes())));
    meta.annotations.insert("flotilla.work/capabilities-revision".into(), "6".into());
    sessions.update(&meta, &session.metadata.resource_version, &session.spec).await.expect("old annotations");
    let tracker = CapabilityCardTracker::default();
    for _ in 0..2 {
        observe_card(&backend, "flotilla", &sessions.get("session").await.expect("session"), first, &tracker).await.expect("upgrade");
    }
    assert_eq!(sessions.get("session").await.expect("session").metadata.annotations["flotilla.work/capabilities-revision"], "7");
    let messages = backend.using::<Message>("flotilla");
    let all = messages.list().await.expect("messages").items;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].spec.body, first);
    let mut status = all[0].status.clone().expect("status");
    status.phase = MessagePhase::Delivered;
    status.resolved_receiver = Some(flotilla_resources::ResolvedMessageReceiver {
        role_address: None,
        crew_id: "crew-id".into(),
        session: "terminal".into(),
        delivered_at: Utc::now(),
        evidence: "in-memory transport receipt".into(),
        transport: None,
    });
    messages.update_status(&all[0].metadata.name, &all[0].metadata.resource_version, &status).await.expect("receipt");
    let second = "## Your capabilities\n### Credentials\n- known\n- added\n";
    observe_card(&backend, "flotilla", &sessions.get("session").await.expect("session"), second, &tracker).await.expect("delta");
    let all = messages.list().await.expect("messages").items;
    let latest = all.iter().find(|message| message.spec.supersedes.is_some()).expect("delta message");
    assert_eq!(latest.spec.body, card_delta(first, second));
    assert!(!latest.spec.body.contains("known"));
    // Restart without an ephemeral baseline must send a full card on a change,
    // even when an unchanged refresh precedes it and the prior notice is pending.
    let restarted = CapabilityCardTracker::default();
    observe_card(&backend, "flotilla", &sessions.get("session").await.expect("session"), second, &restarted).await.expect("unchanged");
    let third = "## Your capabilities\n### Credentials\n- changed after restart\n";
    observe_card(&backend, "flotilla", &sessions.get("session").await.expect("session"), third, &restarted).await.expect("restart full");
    let session = sessions.get("session").await.expect("session");
    let latest = messages.get(&session.metadata.annotations["flotilla.work/capabilities-message"]).await.expect("latest");
    assert_eq!(latest.spec.body, third);
}

// Reaping a pending Message loses the receipt baseline, so production refresh must publish
// a full card without referencing the deleted predecessor, and unchanged refresh is idempotent.
#[tokio::test]
async fn refresh_after_reaped_message_recovers_with_full_card() {
    use crate::crew_capabilities::{refresh_cards, CapabilityCardTracker};
    use flotilla_capability_contract::{CredentialCapability, SessionCapabilitySource};
    use flotilla_resources::{Environment, EnvironmentSpec, HostDirectEnvironmentSpec, Message, Vessel, VesselSpec};
    // Credential/endpoint delivery is the runtime I/O boundary; resources use the real memory backend.
    struct Delivery(RwLock<BTreeMap<String, String>>);
    #[async_trait]
    impl SessionCapabilitySource for Delivery {
        async fn credentials(&self, _: &str, _: &BTreeSet<String>) -> Result<Vec<CredentialCapability>, String> {
            Ok(vec![])
        }
        async fn endpoints(&self, _: &str, _: &str) -> Result<BTreeMap<String, String>, String> {
            Ok(self.0.read().await.clone())
        }
    }
    let (_, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    backend
        .using::<Vessel>("flotilla")
        .create(
            &InputMeta::builder().name("vessel".into()).build(),
            &VesselSpec {
                convoy_ref: "crew".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
    backend
        .using::<Environment>("flotilla")
        .create(
            &InputMeta::builder().name("resume-env".into()).build(),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/repo".into() }),
                docker: None,
            },
        )
        .await
        .expect("environment");
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("session").await.expect("session");
    sessions
        .update_status(
            "session",
            &session.metadata.resource_version,
            &flotilla_resources::TerminalSessionStatus { phase: ResourceTerminalSessionPhase::Running, ..Default::default() },
        )
        .await
        .expect("running");
    let source = Delivery(RwLock::new(BTreeMap::new()));
    let tracker = CapabilityCardTracker::default();
    refresh_cards(&backend, "flotilla", &source, &tracker).await.expect("initial baseline");
    source.0.write().await.insert("API".into(), "127.0.0.1:1".into());
    refresh_cards(&backend, "flotilla", &source, &tracker).await.expect("pending delta");
    let session = sessions.get("session").await.expect("session");
    let messages = backend.using::<Message>("flotilla");
    let reaped = &session.metadata.annotations["flotilla.work/capabilities-message"];
    assert!(messages.get(reaped).await.expect("delta").spec.body.starts_with("## Capability changes"));
    messages.delete(reaped).await.expect("reap Message");
    source.0.write().await.insert("API".into(), "127.0.0.1:2".into());
    refresh_cards(&backend, "flotilla", &source, &tracker).await.expect("recover reaped Message");
    refresh_cards(&backend, "flotilla", &source, &tracker).await.expect("unchanged refresh");
    let session = sessions.get("session").await.expect("session");
    assert_eq!(session.metadata.annotations["flotilla.work/capabilities-revision"], "3");
    let all = messages.list().await.expect("messages").items;
    assert_eq!(all.len(), 1);
    assert!(all[0].spec.body.starts_with("## Your capabilities"));
    assert!(all[0].spec.body.contains("127.0.0.1:2"));
    assert!(all[0].spec.supersedes.is_none());
}

// A source that fails after a successful delivery (daemon reconstruction gap) must leave the
// published observation untouched: refresh errors rather than publishing an empty card.
#[tokio::test]
async fn failing_source_never_publishes_an_empty_card() {
    use crate::crew_capabilities::{refresh_cards, CapabilityCardTracker};
    use flotilla_capability_contract::{CredentialCapability, SessionCapabilitySource};
    use flotilla_resources::{Environment, EnvironmentSpec, HostDirectEnvironmentSpec, Message, Vessel, VesselSpec};
    // Credential/endpoint delivery is the runtime I/O boundary; resources use the real memory backend.
    struct Delivery(RwLock<BTreeMap<String, String>>, std::sync::atomic::AtomicBool);
    #[async_trait]
    impl SessionCapabilitySource for Delivery {
        async fn credentials(&self, _: &str, _: &BTreeSet<String>) -> Result<Vec<CredentialCapability>, String> {
            if self.1.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("reconstructing".into());
            }
            Ok(vec![])
        }
        async fn endpoints(&self, _: &str, _: &str) -> Result<BTreeMap<String, String>, String> {
            Ok(self.0.read().await.clone())
        }
    }
    let (_, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    backend
        .using::<Vessel>("flotilla")
        .create(
            &InputMeta::builder().name("vessel".into()).build(),
            &VesselSpec {
                convoy_ref: "crew".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
    backend
        .using::<Environment>("flotilla")
        .create(
            &InputMeta::builder().name("resume-env".into()).build(),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/repo".into() }),
                docker: None,
            },
        )
        .await
        .expect("environment");
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("session").await.expect("session");
    sessions
        .update_status(
            "session",
            &session.metadata.resource_version,
            &flotilla_resources::TerminalSessionStatus { phase: ResourceTerminalSessionPhase::Running, ..Default::default() },
        )
        .await
        .expect("running");
    let source = Delivery(RwLock::new(BTreeMap::new()), std::sync::atomic::AtomicBool::new(false));
    let tracker = CapabilityCardTracker::default();
    refresh_cards(&backend, "flotilla", &source, &tracker).await.expect("initial baseline");
    let revision = |session: &flotilla_resources::ResourceObject<ResourceTerminalSession>| {
        session.metadata.annotations["flotilla.work/capabilities-revision"].clone()
    };
    let before = revision(&sessions.get("session").await.expect("session"));
    source.1.store(true, std::sync::atomic::Ordering::SeqCst);
    refresh_cards(&backend, "flotilla", &source, &tracker).await.expect_err("reconstruction gap");
    assert_eq!(revision(&sessions.get("session").await.expect("session")), before);
    let messages = backend.using::<Message>("flotilla").list().await.expect("messages").items;
    assert!(messages.is_empty());
}
