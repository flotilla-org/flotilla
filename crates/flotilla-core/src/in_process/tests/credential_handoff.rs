use flotilla_store_memory::InMemoryBackend;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use flotilla_config::ConfigStore;
use flotilla_protocol::{CrewCommandContext, HostName};
use flotilla_resources::{
    Convoy as ResourceConvoy, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, RepositoryKey, Selector,
    TerminalSession as ResourceTerminalSession, Vessel, VesselRequirement,
};
use flotilla_store::ResourceBackend;

use super::support::{test_engine, test_meta, SessionStagingProbe};
use crate::in_process::{InProcessDaemon, TerminalSessionIdentity};
use crate::testkits::discovery::fake_discovery;
use flotilla_provisioning_testkit::{Ending, TerminalsFixture};
use flotilla_resources::terminal_records::{TerminalLaunchRequest, TerminalLaunchSource};

#[tokio::test]
async fn contained_codex_to_claude_handoff_stages_credentials_for_the_latent_reviewer() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"two-crew-contained-test\"\n").expect("daemon config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let engine = test_engine(&daemon);
    let repository = RepositoryKey("github.com-flotilla-org-flotilla".to_string());
    let requirement = VesselRequirement::builder()
        .name("work".to_string())
        .credential_refs(BTreeSet::from(["claude-max".to_string(), "github-crew-pr".to_string()]))
        .credential_scopes(BTreeMap::from([
            ("claude-max".to_string(), BTreeSet::from([repository.clone()])),
            ("github-crew-pr".to_string(), BTreeSet::from([repository])),
        ]))
        .crew(vec![
            CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: Some("codex".to_string()), model: None },
                    prompt: None,
                    brief_template: None,
                })
                .build(),
            CrewSpec::builder()
                .role("reviewer".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code-review".to_string(), adapter: Some("claude-code".to_string()), model: None },
                    prompt: Some("Review the coder's implementation.".to_string()),
                    brief_template: None,
                })
                .build(),
        ])
        .build();

    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .create(&test_meta("convoy-two-crew"), &ConvoySpec::builder().workflow_ref("implement-review".to_string()).build())
        .await
        .expect("convoy");
    convoys
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![requirement.clone()],
                }),
                crew_work: BTreeMap::from([(
                    "work".to_string(),
                    BTreeMap::from([
                        ("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                        ("reviewer".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Pending).build()),
                    ]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("active convoy");
    backend
        .clone()
        .using::<Vessel>("flotilla")
        .create(
            &test_meta("convoy-two-crew-work"),
            &flotilla_resources::VesselSpec {
                convoy_ref: "convoy-two-crew".to_string(),
                vessel_name: "work".to_string(),
                fulfilment_kind_ref: "contained".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");

    let requested = CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".to_string()),
        convoy: Some("convoy-two-crew".to_string()),
        vessel_ref: Some("convoy-two-crew-work".to_string()),
        role: Some("coder".to_string()),
    };
    let error =
        daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect_err("missing anchor");
    assert!(error.contains("no active session to anchor"), "{error}");
    let status = convoys.get("convoy-two-crew").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending);

    let coder_identity = TerminalSessionIdentity::builder()
        .vessel_ref("convoy-two-crew-work".to_string())
        .convoy("convoy-two-crew".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .vessel_index(0)
        .crew_index(0)
        .build();
    let coder = engine.terminals.request(&TerminalLaunchRequest {
        session: coder_identity.name(),
        environment: "contained-env".to_string(),
        vessel: Some("convoy-two-crew-work".to_string()),
        role: "coder".to_string(),
        pool: "contained".to_string(),
        cwd: "/workspace".to_string(),
        env: Default::default(),
        source: TerminalLaunchSource::Agent {
            capability: "code".to_string(),
            adapter: Some("codex".to_string()),
            model: None,
            brief: flotilla_resources::TerminalBrief {
                artifact_digest: None,
                path: ".flotilla/briefs/coder.md".to_string(),
                content: "Implement the issue.".to_string(),
                copies: Vec::new(),
            },
        },
    });
    engine.terminals.ready(&coder, "coder-sid", &Default::default());
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    let probe = Arc::new(SessionStagingProbe {
        backend: backend.clone(),
        session: coder_identity.name(),
        environment: "contained-env".to_string(),
        fail_next: std::sync::atomic::AtomicBool::new(true),
        staged: std::sync::atomic::AtomicUsize::new(0),
        engine,
    });
    daemon.set_work_credential_reconciler(probe.clone()).await;
    let error =
        daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect_err("staging failure");
    assert!(error.contains("credential staging failed"), "{error}");
    let status = convoys.get("convoy-two-crew").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending);
    assert!(sessions.get("terminal-convoy-two-crew-work-reviewer").await.is_err());

    daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect("handoff to latent reviewer");
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);

    // The latent reviewer's launch, requested by the handoff in the coder's environment; its
    // credentials are delivered at launch from the vessel's requirement.
    let reviewer = probe.engine.terminals.record("terminal-convoy-two-crew-work-reviewer");
    let TerminalLaunchSource::Agent { adapter, brief, .. } = &reviewer.source else {
        panic!("reviewer must be an agent session");
    };
    assert_eq!(adapter.as_deref(), Some("claude-code"));
    assert!(brief.content.contains("- Minted credential repository scope:"));
    assert!(brief.content.contains("  - `github-crew-pr`:\n    - `github.com-flotilla-org-flotilla`"));
    assert_eq!(reviewer.environment, "contained-env");
    assert_eq!(reviewer.vessel.as_deref(), Some("convoy-two-crew-work"));

    let convoy = convoys.get("convoy-two-crew").await.expect("convoy");
    let mut terminal_status = convoy.status.expect("status");
    terminal_status.phase = flotilla_resources::ConvoyPhase::Landed;
    convoys.update_status("convoy-two-crew", &convoy.metadata.resource_version, &terminal_status).await.expect("landed convoy");
    let error = daemon.crew_handoff_internal(&requested, "reviewer", "Late handoff").await.expect_err("terminal handoff");
    assert!(error.contains("terminal"), "{error}");
    assert_eq!(convoys.get("convoy-two-crew").await.expect("convoy").status.expect("status"), terminal_status);
    assert_eq!(probe.engine.terminals.record("terminal-convoy-two-crew-work-reviewer"), reviewer);
}

// Stands in for the vessel controller: when the handoff writes the target's
// brief, the controller has already requested the target's session.
struct ConcurrentSessionCreation {
    terminals: Arc<TerminalsFixture>,
    request: TerminalLaunchRequest,
    ending: Option<Ending<'static>>,
}

#[async_trait::async_trait]
impl flotilla_agent::brief_artifact::BriefArtifactWriter for ConcurrentSessionCreation {
    async fn put_brief(&self, _: &str, _: &str, _: &str, _: &str, _: &[u8], _: Option<&str>) -> Result<String, String> {
        let launch = self.terminals.request(&self.request);
        match self.ending {
            Some(Ending::Ready(identity)) => self.terminals.ready(&launch, identity, &Default::default()),
            Some(Ending::GaveUp(cause)) => self.terminals.give_up(&launch, cause),
            Some(_) => unreachable!("the scenarios realise or give up"),
            None => {}
        }
        Ok("sha256:brief".to_string())
    }
}

// Behaviour (#3206): a handoff reopens its target's work, which lets the vessel
// controller create the target's session while the handoff prepares its own.
// The handoff then delivers to that session's launch, unless the engine has given it up.
#[tokio::test]
async fn a_handoff_delivers_to_a_target_session_created_concurrently() {
    for (ending, delivers) in [(None, true), (Some(Ending::Ready("sid")), true), (Some(Ending::GaveUp("launch refused")), false)] {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"concurrent-handoff-test\"\n").expect("daemon config");
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            HostName::new("test-host"),
            backend.clone(),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        let agent = |role: &str| {
            CrewSpec::builder()
                .role(role.to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: Some("codex".to_string()), model: None },
                    prompt: None,
                    brief_template: None,
                })
                .build()
        };
        let requirement = VesselRequirement::builder().name("work".to_string()).crew(vec![agent("governor"), agent("aide")]).build();
        let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
        let convoy = convoys
            .create(&test_meta("convoy-concurrent"), &ConvoySpec::builder().workflow_ref("govern".to_string()).build())
            .await
            .expect("convoy");
        convoys
            .update_status(
                &convoy.metadata.name,
                &convoy.metadata.resource_version,
                &ConvoyStatus {
                    phase: flotilla_resources::ConvoyPhase::Active,
                    workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                        cascade: None,
                        stall_nudges: Default::default(),
                        supervision: None,
                        exit: None,
                        turn_delivery: Default::default(),
                        vessels: vec![requirement.clone()],
                    }),
                    crew_work: BTreeMap::from([(
                        "work".to_string(),
                        BTreeMap::from([
                            ("governor".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                            ("aide".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Pending).build()),
                        ]),
                    )]),
                    ..Default::default()
                },
            )
            .await
            .expect("active convoy");
        backend
            .clone()
            .using::<Vessel>("flotilla")
            .create(
                &test_meta("convoy-concurrent-work"),
                &flotilla_resources::VesselSpec {
                    convoy_ref: "convoy-concurrent".to_string(),
                    vessel_name: "work".to_string(),
                    fulfilment_kind_ref: "contained".to_string(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
            )
            .await
            .expect("vessel");
        let identity = |role: &str, crew_index| {
            TerminalSessionIdentity::builder()
                .vessel_ref("convoy-concurrent-work".to_string())
                .convoy("convoy-concurrent".to_string())
                .vessel("work".to_string())
                .role(role.to_string())
                .vessel_index(0)
                .crew_index(crew_index)
                .build()
        };
        let session_request = |name: String, role: &str, brief: &str| TerminalLaunchRequest {
            session: name,
            environment: "contained-env".to_string(),
            vessel: Some("convoy-concurrent-work".to_string()),
            role: role.to_string(),
            pool: "contained".to_string(),
            cwd: "/workspace".to_string(),
            env: Default::default(),
            source: TerminalLaunchSource::Agent {
                capability: "code".to_string(),
                adapter: Some("codex".to_string()),
                model: None,
                brief: flotilla_resources::TerminalBrief {
                    artifact_digest: None,
                    path: format!(".flotilla/briefs/{role}.md"),
                    content: brief.to_string(),
                    copies: Vec::new(),
                },
            },
        };
        let terminals = Arc::new(TerminalsFixture::default());
        daemon.install_lifecycle_reads(terminals.reads.clone()).expect("engine reads");
        backend.install_engine_records(Arc::new(flotilla_store::TerminalViews::new(backend.clone(), "flotilla", terminals.reads.clone())));
        backend.serve_engine_in("flotilla");
        let governor = terminals.request(&session_request(identity("governor", 0).name(), "governor", "Govern."));
        terminals.ready(&governor, "governor-sid", &Default::default());
        let aide = identity("aide", 1);
        daemon
            .set_brief_artifact_writer(Arc::new(ConcurrentSessionCreation {
                terminals: Arc::clone(&terminals),
                request: session_request(aide.name(), "aide", "Created by the vessel controller."),
                ending,
            }))
            .await;

        let requested = CrewCommandContext {
            crew_id: None,
            namespace: Some("flotilla".to_string()),
            convoy: Some("convoy-concurrent".to_string()),
            vessel_ref: Some("convoy-concurrent-work".to_string()),
            role: Some("governor".to_string()),
        };
        let handed_off = daemon.crew_handoff_internal(&requested, "aide", "Task 1.").await;
        if !delivers {
            assert!(handed_off.is_err(), "a given-up session receives no handoff");
            continue;
        }
        handed_off.expect("handoff to a concurrently created session");

        let target = terminals.record(&aide.name());
        let TerminalLaunchSource::Agent { brief, .. } = &target.source else { panic!("aide must be an agent session") };
        assert_eq!(brief.content, "Created by the vessel controller.", "the controller's session is kept");
        let messages = backend.clone().using::<flotilla_resources::Message>("flotilla").list().await.expect("messages").items;
        assert!(
            messages.iter().any(|message| message.spec.receiver.ends_with("/work/aide") && message.spec.body == "Task 1."),
            "the handoff is queued for the aide: {:?}",
            messages.iter().map(|message| (&message.spec.receiver, &message.spec.body)).collect::<Vec<_>>()
        );
        let status = convoys.get("convoy-concurrent").await.expect("convoy").status.expect("status");
        assert_ne!(status.crew_work["work"]["aide"].phase, CrewWorkPhase::Pending, "the aide's work is reopened");
    }
}
