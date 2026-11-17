//! An agent's activity on its AgentSession comes from two observers: its harness's hooks and an
//! adapter's reading of its screen. A hook reports the agent itself, so it stands over a screen read
//! that disagrees for a while; the screen still records an agent waiting on a person, and an idle
//! agent working again.

use std::sync::Arc;

use flotilla_engine_messages::{MessagePrimitives, HOOK_STANDS_FOR, SCREEN_CAUSE};
use flotilla_engine_messages_testkit::Served;
use flotilla_provisioning_testkit::EngineBackend;
use flotilla_resources::SystemClock;
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::machines::agent_session::Activity;

const HOME: &str = "home-a";

fn primitives(dir: &tempfile::TempDir) -> Served<MessagePrimitives> {
    let service = Arc::new(
        Service::open(&dir.path().join("decided.sqlite"), HOME, flotilla_engine_store::registry().expect("registry")).expect("store"),
    );
    let home = EngineBackend::open(HOME);
    Served::holding(MessagePrimitives::open(HOME, service, home.backend.clone(), Arc::new(SystemClock)).expect("primitives"), home)
}

fn at(seconds: i64) -> Timestamp {
    Timestamp::from_micros(1_000_000_000_000 + seconds * 1_000_000)
}

/// The session's current activity and its cause.
fn current(primitives: &MessagePrimitives, name: &str) -> Option<(Activity, String)> {
    let observation = primitives.sessions().get(name)?.state().observation()?;
    Some((observation.activity, observation.activity_cause))
}

#[test]
fn a_screen_read_records_activity_on_a_session_no_hook_has_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir);
    let sessions = primitives.sessions();
    assert!(sessions.observe_screen("codex", Activity::Working, at(0)).unwrap());
    assert_eq!(current(&primitives, "codex"), Some((Activity::Working, SCREEN_CAUSE.to_owned())));
    assert!(sessions.observe_screen("codex", Activity::NeedsInput, at(1)).unwrap());
    assert!(sessions.observe_screen("codex", Activity::Idle, at(2)).unwrap());
    assert_eq!(current(&primitives, "codex"), Some((Activity::Idle, SCREEN_CAUSE.to_owned())));
    assert!(!sessions.observe_screen("codex", Activity::Idle, at(3)).unwrap(), "an unchanged reading writes nothing");
}

#[test]
fn a_fresh_hook_stands_over_a_disagreeing_screen_read_except_input_and_renewed_work() {
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir);
    let sessions = primitives.sessions();
    let stands = HOOK_STANDS_FOR.as_secs() as i64;

    sessions.observe("agent", Activity::Working, "PreToolUse".into(), at(0)).unwrap();
    assert!(!sessions.observe_screen("agent", Activity::Idle, at(1)).unwrap());
    assert_eq!(current(&primitives, "agent"), Some((Activity::Working, "PreToolUse".to_owned())));
    // A permission prompt on screen is the agent waiting on a person, whatever the hook said.
    assert!(sessions.observe_screen("agent", Activity::NeedsInput, at(2)).unwrap());
    assert_eq!(current(&primitives, "agent"), Some((Activity::NeedsInput, SCREEN_CAUSE.to_owned())));

    // An idle agent the screen shows working again is working.
    sessions.observe("other", Activity::Idle, "Stop".into(), at(10)).unwrap();
    assert!(sessions.observe_screen("other", Activity::Working, at(11)).unwrap());
    assert_eq!(current(&primitives, "other"), Some((Activity::Working, SCREEN_CAUSE.to_owned())));

    // Once the hook's run is older than it stands for, the screen's reading is recorded.
    sessions.observe("late", Activity::Working, "UserPromptSubmit".into(), at(0)).unwrap();
    assert!(!sessions.observe_screen("late", Activity::Idle, at(stands - 1)).unwrap());
    assert!(sessions.observe_screen("late", Activity::Idle, at(stands)).unwrap());
    assert_eq!(current(&primitives, "late"), Some((Activity::Idle, SCREEN_CAUSE.to_owned())));
}

// Another home's AgentSession reads as current until that home has been silent too long, and says
// when that is; hearing from the home again is a change its readers are told of.
#[tokio::test]
async fn another_homes_session_is_current_until_its_home_falls_silent() {
    use flotilla_resources::agent_activity::{AgentSessionChange, AgentSessionReads, ProgressFacts, HOME_HEARD_WITHIN};
    use futures::StreamExt;

    let open = |dir: &tempfile::TempDir, home: &str| {
        Arc::new(
            Service::open(&dir.path().join("decided.sqlite"), home, flotilla_engine_store::registry().expect("registry")).expect("store"),
        )
    };
    let (here_dir, there_dir) = (tempfile::tempdir().expect("tempdir"), tempfile::tempdir().expect("tempdir"));
    let (here, there) = (open(&here_dir, HOME), open(&there_dir, "home-b"));
    let backend = EngineBackend::open("home-b");
    let writer = Served::holding(
        MessagePrimitives::open("home-b", Arc::clone(&there), backend.backend.clone(), Arc::new(SystemClock)).expect("primitives"),
        backend,
    );
    let start = at(0).to_datetime().expect("a time");
    let clock = Arc::new(flotilla_store_testkit::VirtualClock::new(start));
    let reads = flotilla_core::in_process::DecidedAgentSessions::new(
        Arc::clone(&here),
        Arc::clone(&clock) as Arc<dyn flotilla_resources::Clock>,
        Arc::new(ProgressFacts::new(start)),
    );
    let mut changes = reads.changes().expect("changes");
    // Waits until the readers are told home-b was heard from.
    async fn until_heard(changes: &mut futures::stream::BoxStream<'static, AgentSessionChange>) {
        loop {
            let change = tokio::time::timeout(std::time::Duration::from_secs(10), changes.next()).await;
            if matches!(change.expect("told in time").expect("a change"), AgentSessionChange::Heard { origin } if origin == "home-b") {
                return;
            }
        }
    }

    writer.sessions().observe_screen("agent", Activity::Working, at(0)).expect("observe");
    flotilla_provisioning_testkit::replicate_home_at(&there, "home-b", &here, start);
    until_heard(&mut changes).await;
    let view = reads.read("agent", Some("home-b")).expect("read").expect("held");
    assert!(view.current);
    assert_eq!(view.stale_at, Some(start + HOME_HEARD_WITHIN));

    clock.set(start + HOME_HEARD_WITHIN);
    let view = reads.read("agent", Some("home-b")).expect("read").expect("held");
    assert!(!view.current, "a home silent too long is not current");
    assert_eq!(view.stale_at, None);

    let again = start + HOME_HEARD_WITHIN + chrono::Duration::seconds(1);
    flotilla_provisioning_testkit::replicate_home_at(&there, "home-b", &here, again);
    until_heard(&mut changes).await;
    let view = reads.read("agent", Some("home-b")).expect("read").expect("held");
    assert!(view.current, "a home heard from again is current");
    assert_eq!(view.stale_at, Some(again + HOME_HEARD_WITHIN));
}
