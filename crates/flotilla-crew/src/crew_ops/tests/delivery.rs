use crate::crew_ops::MessageAttribution;
use flotilla_resources::ResumeIntent;
use std::sync::atomic::Ordering;

use flotilla_resources::{Convoy as ResourceConvoy, CrewWorkPhase, TerminalSession as ResourceTerminalSession, TerminalSessionSource};

use super::fixture;
use crate::crew_ops::ConvoyResumeOutcome;

#[tokio::test]
async fn resume_restores_work_until_credentials_are_staged() {
    let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Done).await;
    let error = crew
        .convoy_resume_with_sender_internal(
            "flotilla",
            "crew",
            "continue",
            (None, None),
            MessageAttribution::operator(None),
            ResumeIntent::Reopen,
        )
        .await
        .expect_err("credential failure");
    assert_eq!(error, "staging unavailable");
    let convoy = backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("convoy");
    assert_eq!(convoy.status.expect("status").crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
    assert_eq!(
        crew.convoy_resume_with_sender_internal(
            "flotilla",
            "crew",
            "continue",
            (None, None),
            MessageAttribution::operator(None),
            ResumeIntent::Reopen
        )
        .await
        .expect("retry"),
        ConvoyResumeOutcome::Queued { displaced: None }
    );
    let session = backend.using::<ResourceTerminalSession>("flotilla").get("session").await.expect("session");
    assert!(matches!(session.spec.source, TerminalSessionSource::Agent { message: None, .. }));
    let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.body, "continue");
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
}

// Convoy resume stores only the author's text, at both active and completed turn boundaries (#2923).
#[tokio::test]
async fn convoy_resume_body_is_author_text() {
    for phase in [CrewWorkPhase::Working, CrewWorkPhase::Done] {
        let (crew, backend, probe, _config) = fixture(phase).await;
        probe.fail.store(false, Ordering::SeqCst);
        let prompt = "Continue the fix.\n\nKeep this paragraph.";
        crew.resume("flotilla", "crew", prompt, None, None).await.expect("resume");
        let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("messages").items;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].spec.body, prompt);
    }
}

// Working crew without fresh idle evidence queues replacements; withdrawal is idempotent (#2221).
#[hegel::test]
fn queued_resume_replaces_and_withdraws_without_staging(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Include empty sequences, duplicate briefs, empty invalid prompts, and repeated withdrawals.
    let steps = tc.draw(gs::integers::<usize>().min_value(0).max_value(8));
    let operations: Vec<u8> = (0..steps).map(|_| tc.draw(gs::integers::<u8>().min_value(0).max_value(3))).collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Working).await;
        let mut expected = None;
        for operation in operations {
            match operation {
                0 | 1 => {
                    let prompt = if operation == 0 { "first" } else { "second" };
                    assert_eq!(
                        crew.resume("flotilla", "crew", prompt, None, None).await.expect("queue"),
                        ConvoyResumeOutcome::Queued { displaced: expected.clone() }
                    );
                    expected = Some(prompt.to_string());
                }
                2 => {
                    assert_eq!(crew.withdraw_pending_brief("flotilla", "crew").await.expect("withdraw"), expected.take());
                }
                _ => {
                    crew.resume("flotilla", "crew", "", None, None).await.expect_err("empty prompt refused");
                }
            }
            let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
            let pending: Vec<_> =
                records.iter().filter(|message| message.status.as_ref().is_none_or(|status| !status.phase.is_terminal())).collect();
            assert_eq!(pending.first().map(|message| message.spec.body.clone()), expected);
            assert!(pending.len() <= 1, "replacement and withdrawal leave at most one pending operator message");
            assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
            let session = backend.using::<ResourceTerminalSession>("flotilla").get("session").await.expect("session");
            assert!(matches!(session.spec.source, TerminalSessionSource::Agent { message: None, .. }));
        }
    });
}

// #3455: `convoy resume` reports a held batch ahead of its brief instead of
// "queued for turn end". An operator brief supersedes held system turns at
// delivery, and waits behind any other held batch until an operator clears it.
#[hegel::test]
fn queued_resume_names_a_held_batch_ahead_of_it(tc: hegel::TestCase) {
    use flotilla_resources::{
        ControllerRetry, ControllerRetryDisposition, InputMeta, Message, MessagePhase, MessageRelation, MessageSpec, MessageStatus,
    };
    use hegel::generators as gs;
    let phase = if tc.draw(gs::booleans()) { CrewWorkPhase::Working } else { CrewWorkPhase::Done };
    let held = tc.draw(gs::booleans());
    let mixed = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let (crew, backend, probe, _config) = fixture(phase).await;
        probe.fail.store(false, Ordering::SeqCst);
        let messages = backend.using::<Message>("flotilla");
        let now = chrono::Utc::now();
        let members = ["held-0", "held-1"];
        for (index, name) in members.iter().enumerate() {
            let sender = if mixed && index == 1 { "flotilla/other/work/reviewer" } else { "system:turn-rules" };
            let intent = MessageSpec::builder()
                .sender(sender.into())
                .receiver("flotilla/crew/work/coder".into())
                .relation(MessageRelation::System)
                .body("merged-unclaimed".into())
                .build();
            let created = messages.create(&InputMeta::builder().name((*name).into()).build(), &intent).await.expect("message");
            let disposition = if held {
                ControllerRetryDisposition::Terminal { needs: "terminal was not ready before submission".into() }
            } else {
                ControllerRetryDisposition::Retryable { next_attempt_at: now + chrono::Duration::minutes(1) }
            };
            let status = MessageStatus::builder()
                .phase(MessagePhase::Deliverable)
                .since(now)
                .reason("delivery held: terminal was not ready before submission; attempt 3/3".into())
                .retry(ControllerRetry { attempts: 3, first_failure_at: now, disposition })
                .failed_batch_members(members.iter().map(|member| member.to_string()).collect())
                .build();
            messages.update_status(name, &created.metadata.resource_version, &status).await.expect("held status");
        }
        // A working crew queues for its turn end; a completed one is reopened.
        let intent = if phase == CrewWorkPhase::Working { ResumeIntent::Reminder } else { ResumeIntent::Reopen };
        let outcome = crew
            .convoy_resume_with_sender_internal(
                "flotilla",
                "crew",
                "the PR merged; complete",
                (None, None),
                MessageAttribution::operator(None),
                intent,
            )
            .await
            .expect("resume");
        if !held {
            assert_eq!(outcome, ConvoyResumeOutcome::Queued { displaced: None });
            return;
        }
        let ConvoyResumeOutcome::QueuedBehindHold { held, .. } = outcome else { panic!("hold not reported: {outcome:?}") };
        assert!(held.contains("message batch `held-0` is held for flotilla/crew/work/coder (2 messages: held-0, held-1)"), "{held}");
        assert!(held.contains("terminal was not ready before submission"), "{held}");
        assert!(held.contains("flotilla resource fail-message-batch held-0 --namespace flotilla"), "{held}");
        let expected = if mixed { "it waits behind a held batch" } else { "it supersedes held system turns at delivery" };
        assert!(held.starts_with(expected), "{held}");
    });
}
