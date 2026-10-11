use super::*;
use flotilla_protocol::CrewCommandContext;
use flotilla_resources::{ChangeRequest, ChangeRequestSpec, Message, MessageRelation, MessageSpec, Vessel, VesselSpec};

fn delta(before: &BTreeMap<String, usize>, after: BTreeMap<String, usize>) -> BTreeMap<String, usize> {
    after
        .into_iter()
        .map(|(kind, count)| {
            let count = count - before.get(&kind).copied().unwrap_or_default();
            (kind, count)
        })
        .filter(|(_, count)| *count != 0)
        .collect()
}

// #3042: unrelated convoy/message/PR rows must not change either decoded-object
// counts or store operations for a request. Empty and populated stores run the
// same real service scenarios, rather than measuring execution time.
#[tokio::test]
async fn interactive_crew_reads_ignore_unrelated_resources() {
    for request in ["routing", "orientation", "complete", "teardown", "withdraw", "pending"] {
        let mut counts = Vec::new();
        for unrelated in [0, 32] {
            let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
            backend
                .using::<Vessel>("flotilla")
                .create(
                    &InputMeta::builder().name("vessel".into()).build(),
                    &VesselSpec {
                        convoy_ref: "crew".into(),
                        vessel_name: "work".into(),
                        placement_policy_ref: "test".into(),
                        adopted_checkout_refs: BTreeMap::new(),
                    },
                )
                .await
                .expect("vessel");
            for n in 0..unrelated {
                let name = format!("unrelated-{n}");
                backend
                    .using::<ResourceConvoy>("flotilla")
                    .create(
                        &InputMeta::builder().name(name.clone()).build(),
                        &ConvoySpec::builder().workflow_ref("workflow".into()).role("other".into()).build(),
                    )
                    .await
                    .expect("unrelated convoy");
                backend
                    .using::<Message>("flotilla")
                    .create(
                        &InputMeta::builder().name(name.clone()).build(),
                        &MessageSpec::builder()
                            .sender("flotilla/other".into())
                            .receiver("flotilla/other-convoy/work/coder".into())
                            .relation(MessageRelation::Supervisor)
                            .body("other".into())
                            .build(),
                    )
                    .await
                    .expect("unrelated message");
                backend
                    .using::<ResourceTerminalSession>("flotilla")
                    .create(
                        &InputMeta::builder().name(format!("terminal-{n}")).build(),
                        &TerminalSessionSpec::builder()
                            .env_ref("other".into())
                            .role("coder".into())
                            .source(TerminalSessionSource::Tool { command: "sh".into() })
                            .cwd("/other".into())
                            .pool("other".into())
                            .build(),
                    )
                    .await
                    .expect("unrelated terminal");
                backend
                    .using::<flotilla_resources::Artifact>("flotilla")
                    .create(
                        &InputMeta::builder().name(format!("artifact-{n}")).build(),
                        &flotilla_resources::ArtifactSpec::builder()
                            .convoy("other".into())
                            .producer("other".into())
                            .kind("explainer".into())
                            .subject("other".into())
                            .digest("digest".into())
                            .size(0)
                            .media_type("text/markdown".into())
                            .expires_at(chrono::Utc::now() + chrono::Duration::days(1))
                            .build(),
                    )
                    .await
                    .expect("unrelated artifact");
                backend
                    .using::<ChangeRequest>("flotilla")
                    .create(
                        &InputMeta::builder().name(name).build(),
                        &ChangeRequestSpec::builder()
                            .service("github".into())
                            .scope("other/repo".into())
                            .number(n + 100)
                            .observing_authority("other".into())
                            .build(),
                    )
                    .await
                    .expect("unrelated PR");
            }
            if request == "pending" {
                let sessions = backend.using::<ResourceTerminalSession>("flotilla");
                let session = sessions.get("session").await.expect("session");
                sessions
                    .update_status(
                        "session",
                        &session.metadata.resource_version,
                        &flotilla_resources::TerminalSessionStatus {
                            completion_pending: Some(flotilla_resources::CrewCompletionPending {
                                message: None,
                                disposition: None,
                                decision_ledger_ref: None,
                                force: false,
                                principal_ref: None,
                                attempted_at: chrono::Utc::now(),
                                authority: "host".into(),
                                last_error: "waiting".into(),
                            }),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("pending status");
            }
            let context = CrewCommandContext {
                crew_id: None,
                namespace: Some("flotilla".into()),
                convoy: Some("crew".into()),
                vessel_ref: Some("vessel".into()),
                role: Some("coder".into()),
            };
            let before_objects = backend.read_counts();
            let before_calls = backend.read_calls();
            match request {
                "routing" => {
                    assert_eq!(
                        crew.resolve_crew_routing_context(&context).await.expect("routing").session_name.as_deref(),
                        Some("session")
                    );
                }
                "orientation" => {
                    assert_eq!(crew.crew_list_internal(&context).await.expect("orientation").members.len(), 1);
                }
                "complete" => {
                    crew.complete(&context, None, None, Some("https://example.test/ledger".into()), false, None).await.expect("completion");
                }
                "teardown" => {
                    crew.teardown("flotilla", "crew", false).await.expect("teardown");
                }
                "withdraw" => {
                    assert_eq!(crew.withdraw_pending_brief("flotilla", "crew").await.expect("withdraw"), None);
                }
                "pending" => {
                    assert_eq!(crew.pending_crew_completions().await.expect("pending").len(), 1);
                }
                _ => unreachable!(),
            }
            counts.push((delta(&before_objects, backend.read_counts()), delta(&before_calls, backend.read_calls())));
        }
        assert_eq!(counts[0], counts[1], "{request}: unrelated resources cannot increase reads or decoding");
        if request == "routing" {
            assert_eq!(counts[0].1.get("TerminalSession"), Some(&1));
        }
        if request == "complete" {
            assert_eq!(counts[0].1.get("ChangeRequest"), Some(&1));
            assert_eq!(counts[0].1.get("Artifact"), Some(&1));
        }
    }
}
