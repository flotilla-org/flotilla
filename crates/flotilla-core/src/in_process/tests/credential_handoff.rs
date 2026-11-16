use flotilla_store_memory::engine_home_backend;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use flotilla_config::ConfigStore;
use flotilla_protocol::{CrewCommandContext, HostName};
use flotilla_resources::{
    Convoy as ResourceConvoy, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, CrewWorkPhase, RepositoryKey, Selector,
    TerminalSession as ResourceTerminalSession, VesselRequirement,
};

use super::support::{admit, convoy_engine, designate_fleet, install_engine_messages, test_engine, SessionStagingProbe, TestEngine};
use crate::in_process::{InProcessDaemon, TerminalSessionIdentity};
use crate::testkits::discovery::fake_discovery;
use flotilla_provisioning_testkit::Ending;
use flotilla_resources::terminal_records::{TerminalLaunchRequest, TerminalLaunchSource};

/// A frozen workflow of the one vessel `requirement`.
fn snapshot(requirement: VesselRequirement) -> flotilla_resources::WorkflowSnapshot {
    flotilla_resources::WorkflowSnapshot {
        cascade: None,
        stall_nudges: Default::default(),
        supervision: None,
        exit: None,
        turn_delivery: Default::default(),
        vessels: vec![requirement],
    }
}

#[tokio::test]
async fn contained_codex_to_claude_handoff_stages_credentials_for_the_latent_reviewer() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"two-crew-contained-test\"\n").expect("daemon config");
    let backend = engine_home_backend();
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let _messages = install_engine_messages(&daemon);
    designate_fleet(&backend).await;
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
    let two_crew = admit(
        &daemon,
        "convoy-two-crew",
        ConvoySpec::builder().workflow_ref("implement-review".to_string()).build(),
        ConvoyStatus { workflow_snapshot: Some(snapshot(requirement.clone())), ..Default::default() },
    )
    .await;
    let vessel = engine.vessels.request(&two_crew, "work", "test-host");

    let requested = CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".to_string()),
        convoy: Some(two_crew.clone()),
        vessel_ref: Some(vessel.clone()),
        role: Some("coder".to_string()),
    };
    let before = convoys.get(&two_crew).await.expect("convoy").status.expect("status").crew_work;
    let error =
        daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect_err("missing anchor");
    assert!(error.contains("no active session to anchor"), "{error}");
    let status = convoys.get(&two_crew).await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work, before, "a refused handoff changes no crew's work");
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending);

    let coder_identity = TerminalSessionIdentity::builder()
        .vessel_ref(vessel.clone())
        .convoy(two_crew.clone())
        .vessel("work".to_string())
        .role("coder".to_string())
        .vessel_index(0)
        .crew_index(0)
        .build();
    // The reviewer's session is named for the launch, as the coder's is.
    let reviewer_session = format!("terminal-{vessel}-reviewer");
    let coder = engine.terminals.request(&TerminalLaunchRequest {
        session: coder_identity.name(),
        environment: "contained-env".to_string(),
        vessel: Some(vessel.clone()),
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
        convoy: two_crew.clone(),
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
    let status = convoys.get(&two_crew).await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending);
    assert!(sessions.get(&reviewer_session).await.is_err());

    daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect("handoff to latent reviewer");
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);

    // The latent reviewer's launch, requested by the handoff in the coder's environment; its
    // credentials are delivered at launch from the vessel's requirement.
    let reviewer = probe.engine.terminals.record(&reviewer_session);
    let TerminalLaunchSource::Agent { adapter, brief, .. } = &reviewer.source else {
        panic!("reviewer must be an agent session");
    };
    assert_eq!(adapter.as_deref(), Some("claude-code"));
    assert!(brief.content.contains("- Minted credential repository scope:"));
    assert!(brief.content.contains("  - `github-crew-pr`:\n    - `github.com-flotilla-org-flotilla`"));
    assert_eq!(reviewer.environment, "contained-env");
    assert_eq!(reviewer.vessel.as_deref(), Some(vessel.as_str()));

    // An operator keeps every promise of the work and the convoy is delivered: it is Landed.
    let convoy = convoys.get(&two_crew).await.expect("convoy");
    let engine_convoys = convoy_engine(&daemon);
    let scope = flotilla_resources::convoy::engine::VerdictScope { vessel: "work".to_string(), role: None };
    engine_convoys.keep_by_verdict(&convoy, &scope, "operator", "reviewed").await.expect("promises kept");
    engine_convoys.deliver(&convoy, "claim").await.expect("landed convoy");
    let terminal_status = convoys.get(&two_crew).await.expect("convoy").status.expect("status");
    assert_eq!(terminal_status.phase, flotilla_resources::ConvoyPhase::Landed);
    let error = daemon.crew_handoff_internal(&requested, "reviewer", "Late handoff").await.expect_err("terminal handoff");
    assert!(error.contains("terminal"), "{error}");
    assert_eq!(convoys.get(&two_crew).await.expect("convoy").status.expect("status"), terminal_status);
    assert_eq!(probe.engine.terminals.record(&reviewer_session), reviewer);
}

// Stands in for the vessel controller: when the handoff writes the target's
// brief, the controller has already requested the target's session.
struct ConcurrentSessionCreation {
    engine: TestEngine,
    request: TerminalLaunchRequest,
    ending: Option<Ending<'static>>,
}

#[async_trait::async_trait]
impl flotilla_agent::brief_artifact::BriefArtifactWriter for ConcurrentSessionCreation {
    async fn put_brief(&self, _: &str, _: &str, _: &str, _: &str, _: &[u8], _: Option<&str>) -> Result<String, String> {
        let terminals = &self.engine.terminals;
        let launch = terminals.request(&self.request);
        match self.ending {
            Some(Ending::Ready(identity)) => terminals.ready(&launch, identity, &Default::default()),
            Some(Ending::GaveUp(cause)) => terminals.give_up(&launch, cause),
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
        let backend = engine_home_backend();
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            HostName::new("test-host"),
            backend.clone(),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        let _messages = install_engine_messages(&daemon);
        designate_fleet(&backend).await;
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
        let concurrent = admit(
            &daemon,
            "convoy-concurrent",
            ConvoySpec::builder().workflow_ref("govern".to_string()).build(),
            ConvoyStatus { workflow_snapshot: Some(snapshot(requirement.clone())), ..Default::default() },
        )
        .await;
        let engine = test_engine(&daemon);
        let vessel = engine.vessels.request(&concurrent, "work", "test-host");
        let identity = |role: &str, crew_index| {
            TerminalSessionIdentity::builder()
                .vessel_ref(vessel.clone())
                .convoy(concurrent.clone())
                .vessel("work".to_string())
                .role(role.to_string())
                .vessel_index(0)
                .crew_index(crew_index)
                .build()
        };
        let session_request = |name: String, role: &str, brief: &str| TerminalLaunchRequest {
            session: name,
            environment: "contained-env".to_string(),
            vessel: Some(vessel.clone()),
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
        backend.serve_engine_in("flotilla");
        let terminals = &engine.terminals;
        let governor = terminals.request(&session_request(identity("governor", 0).name(), "governor", "Govern."));
        terminals.ready(&governor, "governor-sid", &Default::default());
        let aide = identity("aide", 1);
        daemon
            .set_brief_artifact_writer(Arc::new(ConcurrentSessionCreation {
                engine: Arc::clone(&engine),
                request: session_request(aide.name(), "aide", "Created by the vessel controller."),
                ending,
            }))
            .await;

        let requested = CrewCommandContext {
            crew_id: None,
            namespace: Some("flotilla".to_string()),
            convoy: Some(concurrent.clone()),
            vessel_ref: Some(vessel.clone()),
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
            messages.iter().any(|message| {
                message.spec.receiver == format!("project:flotilla/convoy:{}/role:aide", concurrent.replace('#', "%23"))
                    && message.spec.body == "Task 1."
            }),
            "the handoff is queued for the aide: {:?}",
            messages.iter().map(|message| (&message.spec.receiver, &message.spec.body)).collect::<Vec<_>>()
        );
        let status = convoys.get(&concurrent).await.expect("convoy").status.expect("status");
        // The aide's work is reopened: it is assigned the task and owns the governor's promises.
        // It reads Working only once its session is live.
        assert!(status.crew_work["work"]["aide"].task_assigned, "the aide's work is reopened");
        assert!(!flotilla_resources::promises::owned(&status, "work", "aide").is_empty(), "the aide owns the handed-off promises");
    }
}
