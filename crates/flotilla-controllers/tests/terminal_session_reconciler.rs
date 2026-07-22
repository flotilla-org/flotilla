use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::{TerminalRuntime, TerminalRuntimeState, TerminalSessionReconciler};
use flotilla_resources::{
    controller::Reconciler, EnvironmentSpec, EnvironmentStatus, EnvironmentStatusPatch, HostDirectEnvironmentSpec, InputMeta,
    ResourceBackend, StatusPatch, TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalSessionPhase,
    TerminalSessionSpec, CONVOY_LABEL, VESSEL_REF_LABEL,
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
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        Err("boom".to_string())
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn terminal_finalizer_kills_the_persisted_session_using_its_spec() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let spec = TerminalSessionSpec {
        env_ref: "host-direct-feta".to_string(),
        role: "coder".to_string(),
        source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
        cwd: "/workspace".to_string(),
        pool: "cleat".to_string(),
    };
    let created = sessions.create(&meta("terminal-convoy-work-coder"), &spec).await.expect("session create");
    let mut status = flotilla_resources::TerminalSessionStatus::default();
    flotilla_resources::TerminalSessionStatusPatch::MarkRunning {
        session_id: "terminal-convoy-work-coder".to_string(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "codex".to_string(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    let session = sessions
        .update_status(&created.metadata.name, &created.metadata.resource_version, &status)
        .await
        .expect("session should be running");
    let runtime = Arc::new(RecordingTerminalRuntime::default());
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");

    reconciler.run_finalizer(&session).await.expect("terminal finalizer should kill the session");

    assert_eq!(runtime.killed.lock().expect("killed mutex").as_slice(), &[("terminal-convoy-work-coder".to_string(), spec)]);
}

#[derive(Default)]
struct RecordingTerminalRuntime {
    killed: Mutex<Vec<(String, TerminalSessionSpec)>>,
}

#[async_trait]
impl TerminalRuntime for RecordingTerminalRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("terminal finalization must not ensure a new session")
    }

    async fn kill_session(&self, session_id: &str, spec: &TerminalSessionSpec) -> Result<(), String> {
        self.killed.lock().expect("killed mutex").push((session_id.to_string(), spec.clone()));
        Ok(())
    }
}

#[tokio::test]
async fn session_provisioning_passes_convoy_and_vessel_tags_to_runtime() {
    let backend = ResourceBackend::InMemory(Default::default());
    let environments = backend.clone().using::<flotilla_resources::Environment>("flotilla");
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let env = environments
        .create(&meta("env-a"), &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host-a".into(), repo_default_dir: "/repos".into() }),
            docker: None,
        })
        .await
        .expect("environment");
    let mut env_status = EnvironmentStatus::default();
    EnvironmentStatusPatch::MarkReady { docker_container_id: None }.apply(&mut env_status);
    environments.update_status("env-a", &env.metadata.resource_version, &env_status).await.expect("ready environment");
    let input = InputMeta::builder()
        .name("term-a".to_string())
        .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "demo".to_string()), (VESSEL_REF_LABEL.to_string(), "demo-work".to_string())]))
        .build();
    let session = sessions
        .create(&input, &TerminalSessionSpec {
            env_ref: "env-a".into(),
            role: "watcher".into(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "tail -f log".into() },
            cwd: "/workspace".into(),
            pool: "cleat".into(),
        })
        .await
        .expect("terminal");
    let runtime = Arc::new(TagRecordingRuntime::default());
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");

    reconciler.fetch_dependencies(&session).await.expect("provisioning dependencies");

    assert_eq!(runtime.tags.lock().expect("tags mutex").as_slice(), &[
        flotilla_resources::TerminalSessionTag::new("convoy", "demo"),
        flotilla_resources::TerminalSessionTag::new("vessel", "demo-work"),
    ]);
}

#[derive(Default)]
struct TagRecordingRuntime {
    tags: Mutex<Vec<flotilla_resources::TerminalSessionTag>>,
}

#[async_trait]
impl TerminalRuntime for TagRecordingRuntime {
    async fn ensure_session(
        &self,
        name: &str,
        _spec: &TerminalSessionSpec,
        tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        *self.tags.lock().expect("tags mutex") = tags.to_vec();
        Ok(TerminalRuntimeState {
            session_id: name.to_string(),
            pid: None,
            started_at: Utc::now(),
            crew: None,
            launch_command: "tail -f log".into(),
            delivered_message_id: None,
        })
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
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
                brief: flotilla_resources::TerminalBrief {
                    path: ".flotilla/briefs/coder.md".into(),
                    content: "brief".into(),
                    copies: Vec::new(),
                },
                context: flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "demo".into(),
                    vessel_ref: "demo-implement".into(),
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
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("running sessions should be observed, not ensured")
    }

    async fn session_is_running(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<bool, String> {
        Ok(false)
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn a_message_queued_during_startup_is_delivered_before_attention_observation() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "reviewer".to_string(),
            source: flotilla_resources::TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector { capability: "review".to_string() },
                brief: flotilla_resources::TerminalBrief {
                    path: ".flotilla/briefs/reviewer.md".into(),
                    content: "brief".into(),
                    copies: Vec::new(),
                },
                context: flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "demo".into(),
                    vessel_ref: "demo-review".into(),
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
    assert!(matches!(deps, flotilla_controllers::reconcilers::terminal_session::TerminalDeps::Attention(_)));
    assert_eq!(runtime.delivered.lock().expect("delivered mutex").len(), 1);
}

#[tokio::test]
async fn terminal_finalizer_cleans_agent_artifacts() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector { capability: "coding".to_string() },
                brief: flotilla_resources::TerminalBrief {
                    path: ".flotilla/briefs/coder.md".into(),
                    content: "brief".into(),
                    copies: vec!["/workspace/repo-a".into()],
                },
                context: flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "demo".into(),
                    vessel_ref: "demo-implement".into(),
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
    let runtime = Arc::new(CleanupRecordingTerminalRuntime::default());
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");

    reconciler.run_finalizer(&session).await.expect("finalizer");

    assert_eq!(runtime.killed.lock().expect("killed mutex").as_slice(), &["cleat-session".to_string()]);
    assert_eq!(runtime.cleaned.lock().expect("cleaned mutex").as_slice(), &[".flotilla/briefs/coder.md".to_string()]);
}

#[derive(Default)]
struct DeliveringTerminalRuntime {
    delivered: Mutex<Vec<(String, String)>>,
}

#[derive(Default)]
struct CleanupRecordingTerminalRuntime {
    killed: Mutex<Vec<String>>,
    cleaned: Mutex<Vec<String>>,
}

#[async_trait]
impl TerminalRuntime for CleanupRecordingTerminalRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("finalizer should not ensure sessions")
    }

    async fn kill_session(&self, session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        self.killed.lock().expect("killed mutex").push(session_id.to_string());
        Ok(())
    }

    async fn cleanup_session_artifacts(&self, spec: &TerminalSessionSpec) -> Result<(), String> {
        if let flotilla_resources::TerminalSessionSource::Agent { brief, .. } = &spec.source {
            self.cleaned.lock().expect("cleaned mutex").push(brief.path.clone());
        }
        Ok(())
    }
}

#[async_trait]
impl TerminalRuntime for DeliveringTerminalRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("running sessions should not be ensured")
    }

    async fn deliver_message(&self, session_id: &str, _spec: &TerminalSessionSpec, message: &str) -> Result<(), String> {
        self.delivered.lock().expect("delivered mutex").push((session_id.to_string(), message.to_string()));
        Ok(())
    }

    async fn observe_attention(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<Option<TerminalAttention>, String> {
        Ok(Some(TerminalAttention { state: TerminalAttentionState::Working, as_of: Utc::now(), source: TerminalAttentionSource::Screen }))
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn stale_hook_attention_decays_to_unobservable_without_changing_phase() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
            cwd: "/workspace".to_string(),
            pool: "hookless".to_string(),
        })
        .await
        .expect("session");
    let mut status = flotilla_resources::TerminalSessionStatus::default();
    flotilla_resources::TerminalSessionStatusPatch::MarkRunning {
        session_id: "session-a".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "cargo test".into(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    status.attention = Some(TerminalAttention {
        state: TerminalAttentionState::Working,
        as_of: Utc::now() - chrono::Duration::seconds(31),
        source: TerminalAttentionSource::Hook,
    });
    let session = sessions.update_status("term-a", &created.metadata.resource_version, &status).await.expect("running session");
    let reconciler = TerminalSessionReconciler::new(Arc::new(HooklessTerminalRuntime), backend, "flotilla");

    let deps = reconciler.fetch_dependencies(&session).await.expect("observe stale attention");
    let now = Utc::now();
    let patch = reconciler.reconcile(&session, &deps, now).patch.expect("decay patch");
    patch.apply(&mut status);

    assert_eq!(status.phase, TerminalSessionPhase::Running);
    assert_eq!(status.attention.expect("attention").state, TerminalAttentionState::Unobservable);
}

struct HooklessTerminalRuntime;

#[async_trait]
impl TerminalRuntime for HooklessTerminalRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("running sessions should not be ensured")
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}
