use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::CrewCommandContext;
use flotilla_resources::{
    CrewWorkPhase, InputMeta, TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase,
};
use tokio::sync::RwLock;

use super::fixture;

#[tokio::test]
async fn capabilities_use_live_deliveries_and_supersede_changed_cards() {
    use flotilla_resources::{EnvironmentSpec, HostDirectEnvironmentSpec, Message, MessageReference, Vessel, VesselSpec};

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
    let (mut crew, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    backend
        .using::<Vessel>("flotilla")
        .create(
            &InputMeta::builder().name("vessel".into()).build(),
            &VesselSpec {
                convoy_ref: "crew".into(),
                vessel_name: "work".into(),
                fulfilment_kind_ref: "policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
    let environments = flotilla_provisioning_testkit::EnvironmentFixture::default();
    Arc::get_mut(&mut crew).expect("unique service").lifecycle_reads = environments.reads.clone();
    environments.request(
        "resume-env",
        &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/repo".into() }),
            ..Default::default()
        },
        None,
    );
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
    // The vessel's grants come from its convoy's frozen requirement.
    let convoys = backend.using::<flotilla_resources::Convoy>("flotilla");
    let convoy = convoys.get("crew").await.expect("convoy");
    let mut convoy_status = convoy.status.clone().expect("convoy status");
    convoy_status.workflow_snapshot.as_mut().expect("snapshot").vessels[0].credential_refs = BTreeSet::from(["github".into()]);
    convoys.update_status("crew", &convoy.metadata.resource_version, &convoy_status).await.expect("vessel grant references");
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let healthy = sessions.get("session").await.expect("healthy session");
    let mut status = healthy.status.clone().unwrap_or_default();
    status.phase = ResourceTerminalSessionPhase::Running;
    sessions.update_status("session", &healthy.metadata.resource_version, &status).await.expect("running healthy session");
    // A running session whose grants can't be read sorts before the healthy session and must
    // not starve it of observations. Its different role cannot receive this card.
    let mut broken_spec = healthy.spec.clone();
    broken_spec.role = "broken".into();
    let flotilla_resources::TerminalSessionSource::Agent { context: broken_context, .. } = &mut broken_spec.source else {
        panic!("agent session")
    };
    broken_context.vessel_ref = "missing-vessel".into();
    let broken = sessions.create(&InputMeta::builder().name("000-malformed".into()).build(), &broken_spec).await.expect("broken session");
    sessions.update_status("000-malformed", &broken.metadata.resource_version, &status).await.expect("running broken session");
    let first = crew.query_capabilities(&context).await.expect("first live card");
    assert!(first.contains("You can push `.github/workflows`"));
    crate::crew_capabilities::observe_launch_card(&backend, "flotilla", "session", &first, &crew.capability_cards)
        .await
        .expect("launch baseline");
    assert!(backend.using::<Message>("flotilla").list().await.expect("messages").items.is_empty());
    for (index, permissions) in [BTreeMap::from([("contents".into(), "read".into())]), BTreeMap::new()].into_iter().enumerate() {
        source.0.write().await[0].permissions = Some(permissions);
        let card = crew.query_capabilities(&context).await.expect("refreshed card");
        assert!(!card.contains("You can push `.github/workflows`"));
        let session = sessions.get("session").await.expect("session");
        if index == 0 {
            crate::crew_capabilities::observe_card(&backend, "flotilla", &session, &card, &crew.capability_cards)
                .await
                .expect("first change notified");
            continue;
        }
        let error = crate::crew_capabilities::refresh_cards(
            &backend,
            "flotilla",
            source.as_ref(),
            &crew.capability_cards,
            environments.reads.clone(),
            &flotilla_store::OldStoreAccounts::new(backend.clone()),
        )
        .await
        .expect_err("aggregate error");
        assert!(error.contains("000-malformed"));
        assert!(error.contains("read vessel missing-vessel"));
        assert_eq!(backend.using::<Message>("flotilla").list().await.expect("messages").items.len(), 2, "the healthy session is notified");
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
    // Each notice supersedes the one before: the latest is the one nothing supersedes.
    let latest = messages
        .iter()
        .find(|message| !messages.iter().any(|other| other.spec.supersedes.as_ref() == Some(&message.metadata.name)))
        .expect("superseding delta");
    assert!(messages.iter().any(|message| Some(&message.metadata.name) == latest.spec.supersedes.as_ref()));
    let Some(MessageReference::ControlRecord { revision: latest_revision, .. }) = &latest.spec.subject else { panic!("revision subject") };
    assert_eq!(latest.spec.sender, "system:capabilities");
    assert_eq!(latest.spec.body, crate::crew_capabilities::card_delta(&first, &card));
    // The latest revision is traceable through the stored subject, not a JSON header (#2923), and
    // its turn text names it.
    assert!(latest_revision.starts_with("capabilities@"));
    assert_eq!(
        flotilla_messaging::frame_message(&latest.spec),
        format!("[from flotilla (system) · capabilities · re TerminalSession session]\n\n{}", latest.spec.body)
    );
}

// Handoffs route to the addressed vessel, preserve typed carries, and admit once per command.
// Generated scenarios cover local and remote receiver homes, latent and completed work,
// duplicate text, and invalid targets. The receiver has no terminal in either initial phase.

// A session launched before this tracker started holds a card it never saw: its first change is
// a full card, and that card's receipt establishes a delta baseline. A restart starts again so.
#[tokio::test]
async fn an_unseen_session_gets_a_full_card_once_then_delivered_deltas() {
    use crate::crew_capabilities::{card_delta, observe_card, CapabilityCardTracker};
    use flotilla_resources::{Message, MessagePhase, Vessel, VesselSpec};
    let (_, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    backend
        .using::<Vessel>("flotilla")
        .create(
            &InputMeta::builder().name("vessel".into()).build(),
            &VesselSpec {
                convoy_ref: "crew".into(),
                vessel_name: "work".into(),
                fulfilment_kind_ref: "policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("session").await.expect("session");
    let unseen = "## Your capabilities\n### Credentials\n- unseen\n";
    let first = "## Your capabilities\n### Credentials\n- known\n";
    let tracker = CapabilityCardTracker::default();
    observe_card(&backend, "flotilla", &session, unseen, &tracker).await.expect("baseline");
    for _ in 0..2 {
        observe_card(&backend, "flotilla", &session, first, &tracker).await.expect("full card");
    }
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
    observe_card(&backend, "flotilla", &session, second, &tracker).await.expect("delta");
    let all = messages.list().await.expect("messages").items;
    let latest = all.iter().find(|message| message.spec.supersedes.is_some()).expect("delta message");
    assert_eq!(latest.spec.body, card_delta(first, second));
    assert!(!latest.spec.body.contains("known"));
    // A restarted tracker has no baseline: an unchanged refresh records one, and a change sends a
    // full card under a revision no earlier Message holds.
    let restarted = CapabilityCardTracker::default();
    observe_card(&backend, "flotilla", &session, second, &restarted).await.expect("unchanged");
    let third = "## Your capabilities\n### Credentials\n- changed after restart\n";
    observe_card(&backend, "flotilla", &session, third, &restarted).await.expect("restart full");
    let all = messages.list().await.expect("messages").items;
    assert_eq!(all.len(), 3);
    assert!(all.iter().any(|message| message.spec.body == third && message.spec.supersedes.is_none()));
}

// Reaping a pending Message loses the receipt baseline, so production refresh must publish
// a full card without referencing the deleted predecessor, and unchanged refresh is idempotent.
#[tokio::test]
async fn refresh_after_reaped_message_recovers_with_full_card() {
    use crate::crew_capabilities::{refresh_cards, CapabilityCardTracker};
    use flotilla_capability_contract::{CredentialCapability, SessionCapabilitySource};
    use flotilla_resources::{EnvironmentSpec, HostDirectEnvironmentSpec, Message, Vessel, VesselSpec};
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
                fulfilment_kind_ref: "policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
    let environments = flotilla_provisioning_testkit::EnvironmentFixture::default();
    environments.request(
        "resume-env",
        &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/repo".into() }),
            ..Default::default()
        },
        None,
    );
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
    refresh_cards(
        &backend,
        "flotilla",
        &source,
        &tracker,
        environments.reads.clone(),
        &flotilla_store::OldStoreAccounts::new(backend.clone()),
    )
    .await
    .expect("initial baseline");
    source.0.write().await.insert("API".into(), "127.0.0.1:1".into());
    refresh_cards(
        &backend,
        "flotilla",
        &source,
        &tracker,
        environments.reads.clone(),
        &flotilla_store::OldStoreAccounts::new(backend.clone()),
    )
    .await
    .expect("first change");
    // A session first seen by a refresh holds a card this tracker never saw: its first change is
    // a full card, whose receipt sets the delta baseline.
    let messages = backend.using::<Message>("flotilla");
    let published = messages.list().await.expect("messages").items;
    let [full] = published.as_slice() else { panic!("one full card: {published:?}") };
    assert!(full.spec.body.starts_with("## Your capabilities"));
    let mut status = full.status.clone().expect("status");
    status.phase = flotilla_resources::MessagePhase::Delivered;
    status.resolved_receiver = Some(flotilla_resources::ResolvedMessageReceiver {
        role_address: None,
        crew_id: "crew-id".into(),
        session: "terminal".into(),
        delivered_at: Utc::now(),
        evidence: "in-memory transport receipt".into(),
        transport: None,
    });
    messages.update_status(&full.metadata.name, &full.metadata.resource_version, &status).await.expect("receipt");
    source.0.write().await.insert("API".into(), "127.0.0.1:2".into());
    refresh_cards(
        &backend,
        "flotilla",
        &source,
        &tracker,
        environments.reads.clone(),
        &flotilla_store::OldStoreAccounts::new(backend.clone()),
    )
    .await
    .expect("pending delta");
    let delta = messages
        .list()
        .await
        .expect("messages")
        .items
        .into_iter()
        .find(|message| message.spec.supersedes.is_some())
        .expect("pending delta");
    assert!(delta.spec.body.starts_with("## Capability changes"));
    messages.delete(&delta.metadata.name).await.expect("reap Message");
    messages.delete(&full.metadata.name).await.expect("reap delivered Message");
    source.0.write().await.insert("API".into(), "127.0.0.1:3".into());
    refresh_cards(
        &backend,
        "flotilla",
        &source,
        &tracker,
        environments.reads.clone(),
        &flotilla_store::OldStoreAccounts::new(backend.clone()),
    )
    .await
    .expect("recover reaped Message");
    refresh_cards(
        &backend,
        "flotilla",
        &source,
        &tracker,
        environments.reads.clone(),
        &flotilla_store::OldStoreAccounts::new(backend.clone()),
    )
    .await
    .expect("unchanged refresh");
    let all = messages.list().await.expect("messages").items;
    assert_eq!(all.len(), 1);
    assert!(all[0].spec.body.starts_with("## Your capabilities"));
    assert!(all[0].spec.body.contains("127.0.0.1:3"));
    assert!(all[0].spec.supersedes.is_none());
}

// A source that fails after a successful delivery (daemon reconstruction gap) must leave the
// published observation untouched: refresh errors rather than publishing an empty card.
#[tokio::test]
async fn failing_source_never_publishes_an_empty_card() {
    use crate::crew_capabilities::{refresh_cards, CapabilityCardTracker};
    use flotilla_capability_contract::{CredentialCapability, SessionCapabilitySource};
    use flotilla_resources::{EnvironmentSpec, HostDirectEnvironmentSpec, Message, Vessel, VesselSpec};
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
                fulfilment_kind_ref: "policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
    let environments = flotilla_provisioning_testkit::EnvironmentFixture::default();
    environments.request(
        "resume-env",
        &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/repo".into() }),
            ..Default::default()
        },
        None,
    );
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
    refresh_cards(
        &backend,
        "flotilla",
        &source,
        &tracker,
        environments.reads.clone(),
        &flotilla_store::OldStoreAccounts::new(backend.clone()),
    )
    .await
    .expect("initial baseline");
    source.1.store(true, std::sync::atomic::Ordering::SeqCst);
    refresh_cards(
        &backend,
        "flotilla",
        &source,
        &tracker,
        environments.reads.clone(),
        &flotilla_store::OldStoreAccounts::new(backend.clone()),
    )
    .await
    .expect_err("reconstruction gap");
    let messages = backend.using::<Message>("flotilla").list().await.expect("messages").items;
    assert!(messages.is_empty());
}

// Capability cards verify tools only after the engine realises the referenced image.
#[tokio::test]
async fn image_capabilities_follow_engine_without_a_copy() {
    use flotilla_provisioning::engine::image_build::ImageBuildBinding;
    use flotilla_provisioning_testkit::{image_build, store_object, Ending, LifecycleFixture};
    use flotilla_resources::{DockerEnvironmentSpec, EnvironmentSpec, ImageBuildPhase};
    let (_, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    let build = image_build(ImageBuildPhase::Built);
    store_object(&backend, build.clone()).await;
    let fixture = LifecycleFixture::<ImageBuildBinding>::new(false);
    fixture.environments.request(
        "resume-env",
        &EnvironmentSpec {
            docker: Some(DockerEnvironmentSpec {
                host_ref: "builder".into(),
                image: "composed".into(),
                image_build_ref: Some(build.metadata.name.clone()),
                declared_agent_adapters: Default::default(),
                required_agent_adapters: Default::default(),
                pull_policy: Default::default(),
                mounts: vec![],
                env: Default::default(),
            }),
            ..Default::default()
        },
        None,
    );
    let session = backend.using::<ResourceTerminalSession>("flotilla").get("session").await.expect("session");
    // A command-source card exercises the same image reader without unrelated vessel-placement setup.
    let mut spec = session.spec;
    spec.source = flotilla_resources::TerminalSessionSource::Tool { command: "true".into() };
    let endpoints = BTreeMap::new();
    let accounts = flotilla_store::OldStoreAccounts::new(backend.clone());
    let card =
        || crate::crew_capabilities::session_card(&backend, "flotilla", "crew", &spec, &[], &endpoints, fixture.reads.clone(), &accounts);
    assert!(card().await.expect("unheld").contains("tool:verified"));
    fixture.request(&build);
    assert!(!card().await.expect("requested").contains("tool:verified"));
    fixture.finish(&build, Ending::Ready("engine digest"));
    assert!(card().await.expect("ready").contains("tool:verified"));
    fixture.finish(&build, Ending::Lost("engine digest"));
    assert!(!card().await.expect("lost").contains("tool:verified"));
    fixture.poison();
    assert!(card().await.expect_err("checked read").contains("poisoned"));
}
