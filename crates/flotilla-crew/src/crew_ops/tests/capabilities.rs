use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::CrewCommandContext;
use flotilla_provisioning_testkit::TerminalsFixture;
use flotilla_resources::terminal_records::TerminalLaunchRequest;
use flotilla_resources::{CrewWorkPhase, TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase};
use tokio::sync::RwLock;

use super::{fixture, shaped_fixture};

#[tokio::test]
async fn capabilities_use_live_deliveries_and_supersede_changed_cards() {
    use flotilla_resources::{EnvironmentSpec, HostDirectEnvironmentSpec, Message, MessageReference};

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
    // The vessel's grants come from its convoy's frozen requirement.
    let (crew, backend, _, held) = shaped_fixture(CrewWorkPhase::Working, |snapshot| {
        snapshot.vessels[0].credential_refs = BTreeSet::from(["github".into()]);
    })
    .await;
    held.engine.engine.environments.request(
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
        .convoy(held.launch.clone())
        .vessel_ref(held.vessel.clone())
        .role("coder".into())
        .build();
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let healthy = sessions.get("session").await.expect("healthy session");
    assert_eq!(healthy.status.as_ref().map(|status| status.phase), Some(ResourceTerminalSessionPhase::Running));
    // A running session whose grants can't be read sorts before the healthy session and must
    // not starve it of observations. Its different role cannot receive this card.
    let broken = held.engine.launch_session(&TerminalLaunchRequest {
        session: "000-malformed".into(),
        vessel: Some("missing-vessel".into()),
        role: "broken".into(),
        ..TerminalsFixture::agent(&held.vessel, "broken", "resume-env")
    });
    assert_eq!(
        sessions.get("000-malformed").await.expect("broken session").status.map(|status| status.phase),
        Some(ResourceTerminalSessionPhase::Running),
        "launch {broken} runs"
    );
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
            held.engine.reads(),
            &flotilla_store_testkit::OldStoreAccounts::new(backend.clone()),
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
    // The engine stores the card's subject as the session's seat, with the card's revision.
    let primitives = held.messages().primitives();
    let receiver = flotilla_resources::crew_role_address("flotilla", &held.launch, "work", "coder");
    let inbox = primitives.inbox_for("flotilla", &receiver).await.expect("the coder's inbox");
    let (id, _) = primitives.locate_in(&inbox, &latest.metadata.name).expect("locate").expect("the latest card");
    let stored =
        primitives.open_in(&inbox).expect("open messages").into_iter().find(|(_, message)| message.name() == id).expect("stored card").1;
    let subject = stored.spec().subject().expect("a subject");
    let project = flotilla_address::Address::project("flotilla").expect("project address");
    let seat = flotilla_resources::convoy::engine::launch_address(&project, &held.launch)
        .and_then(|convoy| convoy.child(flotilla_address::Kind::Role, "coder").map_err(|error| error.to_string()))
        .expect("seat address");
    assert_eq!(subject.address, seat);
    // The crew reads it in the old shape, naming the session.
    let latest = primitives.view_to("flotilla", &receiver, &latest.metadata.name).await.expect("view").expect("the latest card").object;
    let Some(MessageReference::ControlRecord { revision: latest_revision, .. }) = &latest.spec.subject else { panic!("revision subject") };
    assert_eq!(latest_revision, &subject.revision);
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
    use flotilla_resources::Message;
    let (crew, backend, _, held) = fixture(CrewWorkPhase::Working).await;
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("session").await.expect("session");
    let unseen = "## Your capabilities\n### Credentials\n- unseen\n";
    let first = "## Your capabilities\n### Credentials\n- known\n";
    let tracker = CapabilityCardTracker::with_message_path(crew.message_path.clone());
    observe_card(&backend, "flotilla", &session, unseen, &tracker).await.expect("baseline");
    for _ in 0..2 {
        observe_card(&backend, "flotilla", &session, first, &tracker).await.expect("full card");
    }
    let messages = backend.using::<Message>("flotilla");
    let all = messages.list().await.expect("messages").items;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].spec.body, first);
    let receiver = flotilla_resources::crew_role_address("flotilla", &held.launch, "work", "coder");
    held.messages().deliver_turn(&receiver, &all[0].metadata.name, "terminal").await;
    let second = "## Your capabilities\n### Credentials\n- known\n- added\n";
    observe_card(&backend, "flotilla", &session, second, &tracker).await.expect("delta");
    let all = messages.list().await.expect("messages").items;
    let latest = all.iter().find(|message| message.spec.supersedes.is_some()).expect("delta message");
    assert_eq!(latest.spec.body, card_delta(first, second));
    assert!(!latest.spec.body.contains("known"));
    // A restarted tracker has no baseline: an unchanged refresh records one, and a change sends a
    // full card under a revision no earlier Message holds.
    let restarted = CapabilityCardTracker::with_message_path(crew.message_path.clone());
    observe_card(&backend, "flotilla", &session, second, &restarted).await.expect("unchanged");
    let third = "## Your capabilities\n### Credentials\n- changed after restart\n";
    observe_card(&backend, "flotilla", &session, third, &restarted).await.expect("restart full");
    let all = messages.list().await.expect("messages").items;
    assert_eq!(all.len(), 3);
    assert!(all.iter().any(|message| message.spec.body == third && message.spec.supersedes.is_none()));
}

// A source that fails after a successful delivery (daemon reconstruction gap) must leave the
// published observation untouched: refresh errors rather than publishing an empty card.
#[tokio::test]
async fn failing_source_never_publishes_an_empty_card() {
    use crate::crew_capabilities::{refresh_cards, CapabilityCardTracker};
    use flotilla_capability_contract::{CredentialCapability, SessionCapabilitySource};
    use flotilla_resources::{EnvironmentSpec, HostDirectEnvironmentSpec, Message};
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
    let (crew, backend, _, held) = fixture(CrewWorkPhase::Working).await;
    held.engine.engine.environments.request(
        "resume-env",
        &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/repo".into() }),
            ..Default::default()
        },
        None,
    );
    let session = backend.using::<ResourceTerminalSession>("flotilla").get("session").await.expect("session");
    assert_eq!(session.status.map(|status| status.phase), Some(ResourceTerminalSessionPhase::Running));
    let source = Delivery(RwLock::new(BTreeMap::new()), std::sync::atomic::AtomicBool::new(false));
    let tracker = CapabilityCardTracker::with_message_path(crew.message_path.clone());
    refresh_cards(
        &backend,
        "flotilla",
        &source,
        &tracker,
        held.engine.reads(),
        &flotilla_store_testkit::OldStoreAccounts::new(backend.clone()),
    )
    .await
    .expect("initial baseline");
    source.1.store(true, std::sync::atomic::Ordering::SeqCst);
    refresh_cards(
        &backend,
        "flotilla",
        &source,
        &tracker,
        held.engine.reads(),
        &flotilla_store_testkit::OldStoreAccounts::new(backend.clone()),
    )
    .await
    .expect_err("reconstruction gap");
    let messages = backend.using::<Message>("flotilla").list().await.expect("messages").items;
    assert!(messages.is_empty());
}

// Capability cards verify tools only once the engine has built the referenced image.
#[tokio::test]
async fn image_capabilities_follow_engine_builds() {
    use flotilla_provisioning_testkit::{image_build_spec, Ending, EngineFixture};
    use flotilla_resources::{DockerEnvironmentSpec, EnvironmentSpec};
    let (_, backend, _, held) = fixture(CrewWorkPhase::Working).await;
    let engine = EngineFixture::default();
    engine.image_builds.host("builder", "amd64");
    let build = engine.image_builds.demand(&image_build_spec());
    engine.environments.request(
        "resume-env",
        &EnvironmentSpec {
            docker: Some(DockerEnvironmentSpec {
                host_ref: "builder".into(),
                image: "composed".into(),
                image_build_ref: Some(build.clone()),
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
    let mut session_spec = session.spec;
    session_spec.source = flotilla_resources::TerminalSessionSource::Tool { command: "true".into() };
    let endpoints = BTreeMap::new();
    let accounts = flotilla_store_testkit::OldStoreAccounts::new(backend.clone());
    let card = || {
        crate::crew_capabilities::session_card(
            &backend,
            "flotilla",
            &held.launch,
            &session_spec,
            &[],
            &endpoints,
            engine.reads(),
            &accounts,
        )
    };
    assert!(!card().await.expect("not launched").contains("tool:verified"));
    engine.image_builds.launch(&build);
    assert!(!card().await.expect("launched").contains("tool:verified"));
    engine.image_builds.finish(&build, Ending::Ready("engine digest"));
    assert!(card().await.expect("built").contains("tool:verified"));
    engine.image_builds.finish(&build, Ending::Lost("engine digest"));
    assert!(!card().await.expect("lost").contains("tool:verified"));
    engine.image_builds.poison();
    assert!(card().await.expect_err("checked read").contains("poisoned"));
}
