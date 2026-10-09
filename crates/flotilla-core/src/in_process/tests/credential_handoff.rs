use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use flotilla_protocol::{CrewCommandContext, HostName};
use flotilla_resources::{
    Convoy as ResourceConvoy, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, InMemoryBackend, RepositoryKey,
    ResourceBackend, Selector, TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase,
    TerminalSessionSource, TerminalSessionSpec as ResourceTerminalSessionSpec, TerminalSessionStatus as ResourceTerminalSessionStatus,
    Vessel, VesselRequirement, CREDENTIAL_REFS_ANNOTATION,
};

use super::support::{test_meta, SessionStagingProbe};
use crate::config::ConfigStore;
use crate::in_process::crew_ops::terminal_meta_with_vessel_credentials;
use crate::in_process::{InProcessDaemon, TerminalSessionIdentity, CREDENTIAL_SCOPES_ANNOTATION};
use crate::providers::discovery::test_support::fake_discovery;

#[tokio::test]
async fn contained_codex_to_claude_handoff_stages_credentials_for_the_latent_reviewer() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"two-crew-contained-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
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
                placement_policy_ref: "contained".to_string(),
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
    let coder_meta = terminal_meta_with_vessel_credentials(coder_identity.input_meta(), &requirement);
    let coder = backend
        .clone()
        .using::<ResourceTerminalSession>("flotilla")
        .create(
            &coder_meta,
            &ResourceTerminalSessionSpec {
                env_ref: "contained-env".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: Some("codex".to_string()), model: None },
                    brief: flotilla_resources::TerminalBrief {
                        artifact_digest: None,
                        path: ".flotilla/briefs/coder.md".to_string(),
                        content: "Implement the issue.".to_string(),
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "convoy-two-crew".to_string(),
                        vessel_ref: "convoy-two-crew-work".to_string(),
                    }),
                    message: None,
                },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: "contained".to_string(),
            },
        )
        .await
        .expect("eager coder terminal");

    let reviewer_identity = TerminalSessionIdentity::builder()
        .vessel_ref("convoy-two-crew-work".to_string())
        .convoy("convoy-two-crew".to_string())
        .vessel("work".to_string())
        .role("reviewer".to_string())
        .vessel_index(0)
        .crew_index(1)
        .build();
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    let failed_reviewer = sessions
        .create(&reviewer_identity.input_meta(), &ResourceTerminalSessionSpec { role: "reviewer".to_string(), ..coder.spec.clone() })
        .await
        .expect("failed reviewer target");
    let failed_reviewer = sessions
        .update_status(
            &failed_reviewer.metadata.name,
            &failed_reviewer.metadata.resource_version,
            &ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Failed, ..Default::default() },
        )
        .await
        .expect("failed phase");
    let error = daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect_err("failed target");
    assert!(error.contains("failed provisioning"), "{error}");
    let status = convoys.get("convoy-two-crew").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending);
    sessions.delete(&failed_reviewer.metadata.name).await.expect("remove failed target");
    let probe = Arc::new(SessionStagingProbe {
        backend: backend.clone(),
        session: coder.metadata.name.clone(),
        environment: "contained-env".to_string(),
        fail_next: std::sync::atomic::AtomicBool::new(true),
        invalidate_next: std::sync::atomic::AtomicBool::new(false),
        staged: std::sync::atomic::AtomicUsize::new(0),
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

    let reviewer = backend
        .using::<ResourceTerminalSession>("flotilla")
        .get("terminal-convoy-two-crew-work-reviewer")
        .await
        .expect("latent reviewer terminal");
    let TerminalSessionSource::Agent { selector, brief, .. } = &reviewer.spec.source else {
        panic!("reviewer must be an agent session");
    };
    assert_eq!(selector.adapter.as_deref(), Some("claude-code"));
    assert!(brief.content.contains("- Minted credential repository scope:"));
    assert!(brief.content.contains("  - `github-crew-pr`:\n    - `github.com-flotilla-org-flotilla`"));
    assert_eq!(reviewer.spec.env_ref, "contained-env");
    assert_eq!(reviewer.metadata.annotations.get(CREDENTIAL_REFS_ANNOTATION), Some(&r#"["claude-max","github-crew-pr"]"#.to_string()));
    assert_eq!(
        reviewer.metadata.annotations.get(CREDENTIAL_SCOPES_ANNOTATION),
        Some(&r#"{"claude-max":["github.com-flotilla-org-flotilla"],"github-crew-pr":["github.com-flotilla-org-flotilla"]}"#.to_string())
    );
    assert_eq!(reviewer.metadata.annotations, coder_meta.annotations);

    let convoy = convoys.get("convoy-two-crew").await.expect("convoy");
    let mut terminal_status = convoy.status.expect("status");
    terminal_status.phase = flotilla_resources::ConvoyPhase::Landed;
    convoys.update_status("convoy-two-crew", &convoy.metadata.resource_version, &terminal_status).await.expect("landed convoy");
    let error = daemon.crew_handoff_internal(&requested, "reviewer", "Late handoff").await.expect_err("terminal handoff");
    assert!(error.contains("terminal"), "{error}");
    assert_eq!(convoys.get("convoy-two-crew").await.expect("convoy").status.expect("status"), terminal_status);
    let reviewer_after = sessions.get(&reviewer.metadata.name).await.expect("reviewer");
    assert_eq!(reviewer_after.metadata.resource_version, reviewer.metadata.resource_version);
}
