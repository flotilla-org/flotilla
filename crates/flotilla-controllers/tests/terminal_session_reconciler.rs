use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::{TerminalRuntime, TerminalRuntimeState, TerminalSessionReconciler};
use flotilla_resources::{
    controller::Reconciler, EnvironmentSpec, EnvironmentStatus, EnvironmentStatusPatch, HostDirectEnvironmentSpec, ResourceBackend,
    StatusPatch, TerminalSessionSpec,
};

mod common;
use common::meta;

#[tokio::test]
async fn terminal_session_failure_uses_injected_now_for_stopped_at() {
    let backend = ResourceBackend::InMemory(Default::default());
    let environments = backend.clone().using::<flotilla_resources::Environment>("flotilla");
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let env = environments
        .create(&meta("env-a"), &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec {
                host_ref: "01HXYZ".to_string(),
                repo_default_dir: "/Users/alice/dev/flotilla-repos".to_string(),
            }),
            docker: None,
        })
        .await
        .expect("env create should succeed");
    environments
        .update_status("env-a", &env.metadata.resource_version, &{
            let mut status = EnvironmentStatus::default();
            EnvironmentStatusPatch::MarkReady { docker_container_id: None }.apply(&mut status);
            status
        })
        .await
        .expect("env ready update should succeed");

    let session = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session create should succeed");
    let reconciler = TerminalSessionReconciler::new(Arc::new(FailingTerminalRuntime), backend, "flotilla");
    let deps = reconciler.fetch_dependencies(&session).await.expect("deps should load");
    let now = Utc::now();
    let outcome = reconciler.reconcile(&session, &deps, now);

    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::TerminalSessionStatusPatch::MarkFailed { stopped_at: Some(stopped_at), .. })
            if stopped_at == now
    ));
}

struct FailingTerminalRuntime;

#[async_trait]
impl TerminalRuntime for FailingTerminalRuntime {
    async fn ensure_session(&self, _name: &str, _spec: &TerminalSessionSpec) -> Result<TerminalRuntimeState, String> {
        Err("boom".to_string())
    }

    async fn kill_session(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn a_disappeared_running_session_is_observed_as_stopped() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector { capability: "coding".to_string() },
                brief: flotilla_resources::TerminalBrief { path: ".flotilla/briefs/coder.md".into(), content: "brief".into() },
                context: flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "demo".into(),
                    vessel: "demo-implement".into(),
                },
                message: None,
            },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session");
    let mut status = flotilla_resources::TerminalSessionStatus::default();
    flotilla_resources::TerminalSessionStatusPatch::MarkRunning {
        session_id: "cleat-session".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "codex".into(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    let session = sessions.update_status("term-a", &created.metadata.resource_version, &status).await.expect("running session");
    let reconciler = TerminalSessionReconciler::new(Arc::new(MissingTerminalRuntime), backend, "flotilla");

    let deps = reconciler.fetch_dependencies(&session).await.expect("observe session");
    let now = Utc::now();
    let outcome = reconciler.reconcile(&session, &deps, now);

    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::TerminalSessionStatusPatch::MarkStopped { stopped_at, .. }) if stopped_at == now
    ));
}

struct MissingTerminalRuntime;

#[async_trait]
impl TerminalRuntime for MissingTerminalRuntime {
    async fn ensure_session(&self, _name: &str, _spec: &TerminalSessionSpec) -> Result<TerminalRuntimeState, String> {
        panic!("running sessions should be observed, not ensured")
    }

    async fn session_is_running(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<bool, String> {
        Ok(false)
    }

    async fn kill_session(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn a_message_queued_during_startup_is_delivered_after_the_session_becomes_running() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "reviewer".to_string(),
            source: flotilla_resources::TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector { capability: "review".to_string() },
                brief: flotilla_resources::TerminalBrief { path: ".flotilla/briefs/reviewer.md".into(), content: "brief".into() },
                context: flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "demo".into(),
                    vessel: "demo-review".into(),
                },
                message: Some(flotilla_resources::TerminalCrewMessage {
                    id: "message-new".into(),
                    text: "Review the amended commit".into(),
                }),
            },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session");
    let mut status = flotilla_resources::TerminalSessionStatus::default();
    flotilla_resources::TerminalSessionStatusPatch::MarkRunning {
        session_id: "cleat-session".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "claude".into(),
        delivered_message_id: Some("message-old".into()),
    }
    .apply(&mut status);
    let session = sessions.update_status("term-a", &created.metadata.resource_version, &status).await.expect("running session");
    let runtime = Arc::new(DeliveringTerminalRuntime::default());
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");

    let deps = reconciler.fetch_dependencies(&session).await.expect("observe pending message");
    assert_eq!(runtime.delivered.lock().expect("delivered mutex").as_slice(), &[(
        "cleat-session".to_string(),
        "Review the amended commit".to_string()
    )]);
    let outcome = reconciler.reconcile(&session, &deps, Utc::now());
    assert!(matches!(
        &outcome.patch,
        Some(flotilla_resources::TerminalSessionStatusPatch::MarkMessageDelivered { message_id }) if message_id == "message-new"
    ));
    let mut acknowledged_status = session.status.clone().expect("status");
    outcome.patch.expect("acknowledgement patch").apply(&mut acknowledged_status);
    let acknowledged =
        sessions.update_status("term-a", &session.metadata.resource_version, &acknowledged_status).await.expect("acknowledge message");

    let deps = reconciler.fetch_dependencies(&acknowledged).await.expect("observe acknowledged message");
    assert!(matches!(deps, flotilla_controllers::reconcilers::terminal_session::TerminalDeps::None));
    assert_eq!(runtime.delivered.lock().expect("delivered mutex").len(), 1);
}

#[derive(Default)]
struct DeliveringTerminalRuntime {
    delivered: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl TerminalRuntime for DeliveringTerminalRuntime {
    async fn ensure_session(&self, _name: &str, _spec: &TerminalSessionSpec) -> Result<TerminalRuntimeState, String> {
        panic!("running sessions should not be ensured")
    }

    async fn deliver_message(&self, session_id: &str, _spec: &TerminalSessionSpec, message: &str) -> Result<(), String> {
        self.delivered.lock().expect("delivered mutex").push((session_id.to_string(), message.to_string()));
        Ok(())
    }

    async fn kill_session(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}
