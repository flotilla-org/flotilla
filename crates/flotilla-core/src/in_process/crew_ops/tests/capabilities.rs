use super::*;

#[tokio::test]
async fn capabilities_use_live_deliveries_and_supersede_changed_cards() {
    use flotilla_resources::{Environment, EnvironmentSpec, HostDirectEnvironmentSpec, Message, Vessel, VesselSpec};

    use crate::crew_capabilities::{CredentialCapability, SessionCapabilitySource};
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
    let first = crew.crew_capabilities_internal(&context).await.expect("first live card");
    assert!(first.contains("You can push `.github/workflows`"));
    let session = sessions.get("session").await.expect("session");
    crate::crew_capabilities::observe_card(&backend, "flotilla", &session, &first).await.expect("launch baseline");
    assert!(backend.using::<Message>("flotilla").list().await.expect("messages").items.is_empty());
    for (index, permissions) in [BTreeMap::from([("contents".into(), "read".into())]), BTreeMap::new()].into_iter().enumerate() {
        source.0.write().await[0].permissions = Some(permissions);
        let card = crew.crew_capabilities_internal(&context).await.expect("refreshed card");
        assert!(!card.contains("You can push `.github/workflows`"));
        let session = sessions.get("session").await.expect("session");
        if index == 0 {
            // A concurrent holder status write can race successful publication.
            // Change the source again before retrying: recover the published
            // revision, then supersede it rather than collide or deliver twice.
            let mut status = session.status.clone().unwrap_or_default();
            status.session_id = Some("concurrent-terminal-observation".into());
            sessions.update_status("session", &session.metadata.resource_version, &status).await.expect("concurrent status");
            assert!(crate::crew_capabilities::observe_card(&backend, "flotilla", &session, &card).await.is_err());
            continue;
        }
        let error = crate::crew_capabilities::refresh_cards(&backend, "flotilla", source.as_ref()).await.expect_err("aggregate error");
        assert!(error.contains("000-malformed"));
        assert!(error.contains("invalid session credential references"));
        assert_eq!(
            sessions.get("session").await.expect("healthy refresh").metadata.annotations["flotilla.work/capabilities-revision"],
            "2"
        );
        let session = sessions.get("session").await.expect("session");
        crate::crew_capabilities::observe_card(&backend, "flotilla", &session, &card).await.expect("supersede recovered revision");
        crate::crew_capabilities::observe_card(&backend, "flotilla", &sessions.get("session").await.expect("session"), &card)
            .await
            .expect("duplicate observation");
    }
    source.1.write().await.insert("pinned service".into(), "127.0.0.1:7423".into());
    let card = crew.crew_capabilities_internal(&context).await.expect("endpoint change");
    assert!(card.contains("`127.0.0.1:7423` → pinned service"));
    crate::crew_capabilities::observe_card(&backend, "flotilla", &sessions.get("session").await.expect("session"), &card)
        .await
        .expect("environment notification");
    source.0.write().await.clear();
    let card = crew.crew_capabilities_internal(&context).await.expect("revoked credential");
    assert!(!card.contains("Credential `github`"));
    crate::crew_capabilities::observe_card(&backend, "flotilla", &sessions.get("session").await.expect("session"), &card)
        .await
        .expect("revocation notification");
    let messages = backend.using::<Message>("flotilla").list().await.expect("messages").items;
    assert_eq!(messages.len(), 4);
    let latest = messages.iter().find(|message| message.spec.body == card).expect("superseding card");
    assert!(messages.iter().any(|message| Some(&message.metadata.name) == latest.spec.supersedes.as_ref()));
    assert_eq!(latest.spec.sender, "system:capabilities");
    assert_eq!(latest.spec.body, crew.crew_capabilities_internal(&context).await.expect("live card"));
    // Terminal transport is the I/O boundary. A busy holder gets no input;
    // after its turn boundary only the latest card is submitted, once.
    struct Transport {
        working: AtomicBool,
        submitted: Mutex<Vec<flotilla_resources::MessageBatch>>,
    }
    #[async_trait]
    impl flotilla_resources::MessageTransport for Transport {
        async fn observe(
            &self,
            _: &ResourceObject<ResourceTerminalSession>,
            _: Option<&flotilla_resources::MessageSubmission>,
        ) -> Result<flotilla_resources::MessageObservation, String> {
            let working = self.working.load(Ordering::SeqCst);
            Ok(flotilla_resources::MessageObservation {
                ready: !working,
                working,
                evidence: Some("terminal boundary".into()),
                ..Default::default()
            })
        }
        async fn submit(&self, batch: &flotilla_resources::MessageBatch) -> flotilla_resources::MessageTransportOutcome {
            self.submitted.lock().await.push(batch.clone());
            flotilla_resources::MessageTransportOutcome::Accepted { evidence: "accepted".into() }
        }
        async fn poll(&self, _: &flotilla_resources::MessageBatch) -> flotilla_resources::MessageTransportOutcome {
            flotilla_resources::MessageTransportOutcome::Accepted { evidence: "accepted".into() }
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
    let transport = Transport { working: AtomicBool::new(true), submitted: Mutex::new(Vec::new()) };
    let inbox = flotilla_resources::MessageInbox::new(backend, "flotilla");
    inbox.reconcile_delivery(&transport, Utc::now()).await.expect("busy holder");
    assert!(transport.submitted.lock().await.is_empty());
    transport.working.store(false, Ordering::SeqCst);
    inbox.reconcile_delivery(&transport, Utc::now()).await.expect("turn boundary");
    inbox.reconcile_delivery(&transport, Utc::now()).await.expect("idempotent receipt");
    let submitted = transport.submitted.lock().await;
    assert_eq!(submitted.len(), 1);
    // The latest revision is traceable through the submission and stored subject, not a JSON header (#2923).
    assert_eq!(submitted[0].submission.members, vec![latest.metadata.name.clone()]);
    assert!(
        matches!(&latest.spec.subject, Some(flotilla_resources::MessageReference::ControlRecord { revision, .. }) if revision == "capabilities@5")
    );
    assert_eq!(submitted[0].text, format!("[from flotilla (system) · capabilities · re TerminalSession session]\n\n{card}"));
}

// Handoffs route to the addressed vessel, preserve typed carries, and admit once per command.
// Generated scenarios cover local and remote receiver homes, latent and completed work,
// duplicate text, and invalid targets. The receiver has no terminal in either initial phase.
