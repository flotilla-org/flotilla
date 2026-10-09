use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use flotilla_protocol::{arg::Arg, commands::AttachMode, result_set::CleatEndpoint};
use serde::Deserialize;

use super::{
    environment::ControlledTerminalEnvironment, ScreenActivity, TerminalEnvVars, TerminalPool, TerminalSession, TerminalSessionLiveness,
    TerminalSessionTag, TerminalSize,
};
use crate::discovery_api::EnvironmentBag;
use crate::providers::run;
use crate::providers::ChannelLabel;
use crate::providers::CommandRunner;
use flotilla_paths::path_context::ExecutionEnvironmentPath;

const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";
const DELIVERY_ENTER_DELAY: Duration = Duration::from_millis(100);
const ENDPOINT_CACHE_TTL: Duration = Duration::from_secs(1);
const RECORDING_PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const RECORDING_PRUNE_SCRIPT: &str = r#"[ -d "$1/$2/sessions" ] || exit 0
pid=$(cat "$1/$2/daemon.pid" 2>/dev/null) || exit 0
case "$pid" in *[!0-9]*|"") exit 0 ;; esac
for marker in "$1/$2/sessions/"*/.flotilla-recovered; do
  [ -f "$marker" ] && [ ! -L "$marker" ] || continue
  [ -n "$(find "$marker" -prune -mtime +6 -print)" ] || continue
  # Cleat and its runner share a PID namespace. PID reuse and EPERM both
  # conservatively retain the recording.
  if signal_error=$(kill -0 "$pid" 2>&1); then continue; fi
  case "$signal_error" in *[Pp]ermiss*|*permitted*) continue ;; esac
  session=${marker%/.flotilla-recovered}
  rm -rf -- "$session"
done"#;
const RECORDING_RETAIN_SCRIPT: &str = r#"session="$1/$2/sessions/$3"
if [ -f "$session/session.cast" ]; then touch "$session/.flotilla-recovered"; fi"#;

#[derive(Debug, Deserialize)]
struct SessionInfo {
    id: String,
    cwd: Option<std::path::PathBuf>,
    cmd: Option<String>,
    status: SessionStatus,
    screen_activity: Option<ScreenActivityWire>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DaemonInfo {
    name: String,
    runtime_root: String,
    alive: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
enum SessionStatus {
    Attached,
    Detached,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScreenActivityWire {
    Active,
    Stable,
}

pub struct CleatTerminalPool {
    runner: Arc<dyn CommandRunner>,
    binary: String,
    environment: ControlledTerminalEnvironment,
    /// Whether this cleat accepts `launch --env-clear` (cleat#318).
    launch_capability: tokio::sync::OnceCell<bool>,
    attach_capability: tokio::sync::OnceCell<()>,
    endpoint_cache: tokio::sync::Mutex<Option<EndpointCache>>,
    last_recording_prune: tokio::sync::Mutex<Instant>,
}

struct EndpointCache {
    updated: Instant,
    endpoints: HashMap<String, Option<CleatEndpoint>>,
}

impl CleatTerminalPool {
    pub fn new(runner: Arc<dyn CommandRunner>, binary: impl Into<String>, bag: &EnvironmentBag) -> Self {
        let environment = ControlledTerminalEnvironment::from_bag(bag);
        Self {
            runner: environment.runner(runner),
            binary: binary.into(),
            environment,
            launch_capability: tokio::sync::OnceCell::new(),
            attach_capability: tokio::sync::OnceCell::new(),
            endpoint_cache: tokio::sync::Mutex::new(None),
            last_recording_prune: tokio::sync::Mutex::new(Instant::now()),
        }
    }

    async fn dead_recording_daemons(runner: &dyn CommandRunner, binary: &str) -> Result<Vec<DaemonInfo>, String> {
        let output = runner.run(binary, &["daemons", "--json"], Path::new("/"), &ChannelLabel::Default).await?;
        let daemons: Vec<DaemonInfo> = serde_json::from_str(&output).map_err(|error| format!("parse cleat daemon list: {error}"))?;
        Ok(daemons.into_iter().filter(|daemon| !daemon.alive && valid_recording_daemon(daemon)).collect())
    }

    async fn prune_retained_recordings_with(runner: &dyn CommandRunner, binary: &str) -> Result<(), String> {
        for daemon in Self::dead_recording_daemons(runner, binary).await? {
            runner
                .run(
                    "sh",
                    &["-c", RECORDING_PRUNE_SCRIPT, "flotilla-prune-cleat-recordings", &daemon.runtime_root, &daemon.name],
                    Path::new("/"),
                    &ChannelLabel::Default,
                )
                .await?;
        }
        Ok(())
    }

    #[cfg(test)]
    async fn prune_retained_recordings(&self) -> Result<(), String> {
        Self::prune_retained_recordings_with(self.runner.as_ref(), &self.binary).await
    }

    async fn prune_recordings_if_due(&self) {
        let mut last = self.last_recording_prune.lock().await;
        if last.elapsed() < RECORDING_PRUNE_INTERVAL {
            return;
        }
        *last = Instant::now();
        drop(last);
        let runner = Arc::clone(&self.runner);
        let binary = self.binary.clone();
        tokio::spawn(async move {
            if let Err(error) = Self::prune_retained_recordings_with(runner.as_ref(), &binary).await {
                tracing::warn!(%error, "prune retained cleat recordings failed");
            }
        });
    }

    fn parse_list_output(json: &str) -> Result<Vec<SessionInfo>, String> {
        serde_json::from_str(json).map_err(|err| format!("parse session list: {err}"))
    }

    fn build_attach_args(&self, session_name: &str, mode: AttachMode) -> Vec<Arg> {
        let mut args = vec![Arg::Literal(self.binary.clone()), Arg::Literal("attach".into()), Arg::Literal("--no-create".into())];
        match mode {
            AttachMode::Default => {}
            AttachMode::PreferTake => args.push(Arg::Literal("--take".into())),
            AttachMode::Strict => args.push(Arg::Literal("--strict".into())),
            AttachMode::Take => args.push(Arg::Literal("--take".into())),
        }
        // Session names are UUIDs (attachable IDs) — always shell-safe, no quoting needed.
        args.push(Arg::Literal(session_name.into()));
        args
    }

    async fn ensure_session_at_size(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
        tags: &[TerminalSessionTag],
        initial_size: Option<TerminalSize>,
    ) -> Result<(), String> {
        let existing = self.list_sessions().await.unwrap_or_default();
        if existing.iter().any(|session| session.session_name == session_name) {
            return Ok(());
        }

        let env_clear = *self
            .launch_capability
            .get_or_try_init(|| async {
                let help = run!(self.runner, &self.binary, &["launch", "--help"], Path::new("/"))
                    .map_err(|_| format!("cleat '{}' cannot verify declared environment support (--env-clear)", self.binary))?;
                let supported = help.split_whitespace().any(|word| word == "--env-clear");
                if !supported {
                    // Until cleat#318 ships, sessions inherit the cleat daemon's environment. The
                    // client itself runs under the controlled envelope, so a daemon it starts is
                    // clean; only a daemon started elsewhere can leak ambient variables.
                    // Remove this fallback once fleet cleat supports --env-clear (flotilla#2756).
                    // The probe result is cached for the pool's lifetime, so this warns once per
                    // pool and an upgraded cleat is only picked up after a daemon restart.
                    tracing::warn!(binary = %self.binary, "cleat lacks --env-clear; launching without clearing inherited environment (cleat#318)");
                }
                Ok::<bool, String>(supported)
            })
            .await?;

        let cwd = cwd.as_path().display().to_string();
        let mut args = vec!["launch"];
        if env_clear {
            args.push("--env-clear");
        }
        args.extend(["--json", "--record", session_name, "--cwd", &cwd, "--cmd", command]);
        let encoded_size = initial_size.map(|size| size.to_string());
        if let Some(size) = &encoded_size {
            args.extend(["--size", size]);
        }
        let encoded_env =
            self.environment.session_environment(env_vars).iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>();
        for variable in &encoded_env {
            args.extend(["--env", variable]);
        }
        let encoded_tags = tags.iter().map(|tag| format!("{}={}", tag.key, tag.value)).collect::<Vec<_>>();
        for tag in &encoded_tags {
            args.push("--tag");
            args.push(tag);
        }
        run!(self.runner, &self.binary, &args, Path::new("/"))?;
        Ok(())
    }
}

fn valid_recording_daemon(daemon: &DaemonInfo) -> bool {
    Path::new(&daemon.runtime_root).is_absolute()
        && !daemon.name.is_empty()
        && daemon.name != "."
        && daemon.name != ".."
        && !daemon.name.contains('/')
}

#[async_trait]
impl TerminalPool for CleatTerminalPool {
    async fn retain_recovered_recording(&self, session_id: &str) -> Result<(), String> {
        if session_id.is_empty() || session_id == "." || session_id == ".." || session_id.contains('/') {
            return Err("invalid recovered cleat session id".to_string());
        }
        // The old ID may have recordings in more than one dead generation.
        for daemon in Self::dead_recording_daemons(self.runner.as_ref(), &self.binary).await? {
            self.runner
                .run(
                    "sh",
                    &["-c", RECORDING_RETAIN_SCRIPT, "flotilla-retain-cleat-recording", &daemon.runtime_root, &daemon.name, session_id],
                    Path::new("/"),
                    &ChannelLabel::Default,
                )
                .await?;
        }
        Ok(())
    }
    async fn session_liveness(&self, session_id: &str) -> Result<TerminalSessionLiveness, String> {
        let output = run!(self.runner, &self.binary, &["list", "--json"], Path::new("/"))?;
        let sessions = Self::parse_list_output(&output)?;
        Ok(match sessions.into_iter().find(|session| session.id == session_id) {
            Some(SessionInfo { error: Some(reason), .. }) => TerminalSessionLiveness::Lost(reason),
            Some(_) => TerminalSessionLiveness::Running,
            None => TerminalSessionLiveness::Absent,
        })
    }
    async fn cleat_endpoint(&self, session_id: &str) -> Result<Option<CleatEndpoint>, String> {
        // Share one physical-daemon inventory across sessions reconciled in the
        // same burst. Expire it quickly so daemon turnover retracts stale facts.
        let mut cache = self.endpoint_cache.lock().await;
        if let Some(snapshot) = cache.as_ref() {
            if snapshot.updated.elapsed() < ENDPOINT_CACHE_TTL {
                return Ok(snapshot.endpoints.get(session_id).cloned().flatten());
            }
        }
        let output = run!(self.runner, &self.binary, &["daemons", "--json"], Path::new("/"))?;
        let daemons: Vec<DaemonInfo> = serde_json::from_str(&output).map_err(|error| format!("parse cleat daemon list: {error}"))?;
        let mut endpoints = HashMap::new();
        for daemon in daemons.into_iter().filter(|daemon| daemon.alive && Path::new(&daemon.runtime_root).is_absolute()) {
            let args = ["--runtime-root", daemon.runtime_root.as_str(), "--server", daemon.name.as_str(), "list", "--json"];
            let output = run!(self.runner, &self.binary, &args, Path::new("/"))?;
            let sessions = Self::parse_list_output(&output)?;
            for session in sessions {
                let endpoint =
                    CleatEndpoint { runtime_root: daemon.runtime_root.clone(), daemon: daemon.name.clone(), session: session.id.clone() };
                // A duplicate session id is ambiguous even if more daemons
                // report it later in the inventory.
                endpoints.entry(session.id).and_modify(|existing| *existing = None).or_insert(Some(endpoint));
            }
        }
        let endpoint = endpoints.get(session_id).cloned().flatten();
        *cache = Some(EndpointCache { updated: Instant::now(), endpoints });
        Ok(endpoint)
    }

    fn tracks_session_liveness(&self) -> bool {
        true
    }

    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
        self.prune_recordings_if_due().await;
        let output = run!(self.runner, &self.binary, &["list", "--json"], Path::new("/"))?;
        let sessions = Self::parse_list_output(&output)?;
        Ok(sessions
            .into_iter()
            .filter(|session| session.error.is_none())
            .map(|session| {
                let status = match session.status {
                    SessionStatus::Attached => flotilla_protocol::TerminalStatus::Running,
                    SessionStatus::Detached => flotilla_protocol::TerminalStatus::Disconnected,
                };
                TerminalSession {
                    session_name: session.id,
                    status,
                    command: session.cmd,
                    working_directory: session.cwd.map(ExecutionEnvironmentPath::new),
                    screen_activity: session.screen_activity.map(|activity| match activity {
                        ScreenActivityWire::Active => ScreenActivity::Active,
                        ScreenActivityWire::Stable => ScreenActivity::Stable,
                    }),
                }
            })
            .collect())
    }

    async fn ensure_session(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
        tags: &[TerminalSessionTag],
    ) -> Result<(), String> {
        self.ensure_session_at_size(session_name, command, cwd, env_vars, tags, None).await
    }

    async fn ensure_session_with_size(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
        tags: &[TerminalSessionTag],
        initial_size: Option<TerminalSize>,
    ) -> Result<(), String> {
        self.ensure_session_at_size(session_name, command, cwd, env_vars, tags, initial_size).await
    }

    fn attach_args(
        &self,
        session_name: &str,
        _command: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
    ) -> Result<Vec<Arg>, String> {
        Ok(self.build_attach_args(session_name, AttachMode::Default))
    }

    async fn preflight_attach(&self, _mode: AttachMode) -> Result<(), String> {
        // These flags arrived with cleat's controller-seat handshake, default
        // degrade-to-watch behavior, and watcher banner. Check before starting
        // the interactive attach so a stale selected pool fails on the primary screen.
        self.attach_capability
            .get_or_try_init(|| async {
                let help = run!(self.runner, &self.binary, &["attach", "--help"], Path::new("/"));
                if help.as_ref().is_ok_and(|help| help.contains("--strict") && help.contains("--take")) {
                    return Ok(());
                }
                let version = run!(self.runner, &self.binary, &["--version"], Path::new("/"))
                    .unwrap_or_else(|error| format!("version unavailable: {error}"));
                Err(format!(
                    "cleat terminal pool binary '{}' lacks controller-seat attach support (--strict/--take); detected {}",
                    self.binary,
                    version.trim()
                ))
            })
            .await
            .copied()
    }

    fn attach_args_for_mode(
        &self,
        session_name: &str,
        _command: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
        mode: AttachMode,
    ) -> Result<Vec<Arg>, String> {
        Ok(self.build_attach_args(session_name, mode))
    }

    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        run!(self.runner, &self.binary, &["kill", session_name], Path::new("/"))?;
        Ok(())
    }

    async fn capture_screen(&self, session_name: &str) -> Result<Option<String>, String> {
        run!(self.runner, &self.binary, &["capture", session_name], Path::new("/")).map(Some)
    }

    async fn deliver(&self, session_name: &str, text: &str) -> Result<(), String> {
        let paste = format!("{BRACKETED_PASTE_START}{text}{BRACKETED_PASTE_END}");
        run!(self.runner, &self.binary, &["send", session_name, &paste, "--no-enter"], Path::new("/"))?;
        tokio::time::sleep(DELIVERY_ENTER_DELAY).await;
        run!(self.runner, &self.binary, &["send-keys", session_name, "Enter"], Path::new("/"))?;
        Ok(())
    }

    async fn retry_delivery(&self, session_name: &str, text: &str) -> Result<(), String> {
        run!(self.runner, &self.binary, &["send-keys", session_name, "C-c"], Path::new("/"))?;
        self.deliver(session_name, text).await
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::*;
    use crate::providers::CommandRunner;
    use crate::testkits::replay::testing::MockRunner;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;

    // Existing command-layout tests exercise their operation after the launch
    // capability has been established. environment_tests covers the real probe.
    fn test_pool(runner: Arc<dyn CommandRunner>, binary: &str) -> CleatTerminalPool {
        let pool = CleatTerminalPool::new(runner, binary, &EnvironmentBag::new());
        pool.launch_capability.set(true).expect("known launch capability");
        pool
    }

    // Check the controlled client envelope, then inspect the logical Cleat
    // operation. The baseline PATH is tested at the environment seam.
    fn logical_calls(runner: &MockRunner) -> Vec<(String, Vec<String>)> {
        runner
            .calls()
            .into_iter()
            .map(|(cmd, args)| {
                assert_eq!(cmd, "/usr/bin/env");
                assert_eq!(&args[..2], &["-i", "PATH=/usr/local/bin:/usr/bin:/bin"]);
                let binary = args[2].clone();
                let mut logical = Vec::new();
                let mut rest = args[3..].iter();
                while let Some(arg) = rest.next() {
                    if arg == "--env" {
                        let value = rest.next().expect("env value");
                        if value == "PATH=/usr/local/bin:/usr/bin:/bin" {
                            continue;
                        }
                        logical.extend([arg.clone(), value.clone()]);
                    } else {
                        logical.push(arg.clone());
                    }
                }
                (binary, logical)
            })
            .collect()
    }

    #[tokio::test]
    async fn recording_prune_never_targets_a_live_daemon_generation() {
        let inventory = r#"[
            {"name":"work@2","runtime_root":"/state/cleat","alive":false},
            {"name":"work@3","runtime_root":"/state/cleat","alive":true}
        ]"#;
        let runner = Arc::new(MockRunner::new(vec![Ok(inventory.into()), Ok(String::new())]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");
        pool.prune_retained_recordings().await.expect("prune retained recordings");
        let calls = logical_calls(&runner);
        assert_eq!(calls.len(), 2, "only the dead generation receives cleanup");
        assert_eq!(calls[0].1, vec!["daemons", "--json"]);
        assert_eq!(calls[1].0, "sh");
        assert!(calls[1].1.iter().any(|arg| arg == "work@2"));
        assert!(!calls[1].1.iter().any(|arg| arg == "work@3"));
    }

    #[test]
    fn expired_recovered_recordings_are_removed_only_after_the_daemon_pid_is_dead() {
        let temp = tempfile::tempdir().expect("tempdir");
        let daemon = temp.path().join("work@2");
        for id in ["expired", "live", "recent"] {
            let session = daemon.join("sessions").join(id);
            std::fs::create_dir_all(&session).expect("session directory");
            std::fs::write(session.join("session.cast"), "recovery evidence").expect("recording");
            std::fs::write(session.join(".flotilla-recovered"), "").expect("retention marker");
        }
        let old = std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60);
        for id in ["expired", "live"] {
            std::fs::File::open(daemon.join("sessions").join(id).join(".flotilla-recovered"))
                .expect("open marker")
                .set_modified(old)
                .expect("age marker");
        }
        std::fs::write(daemon.join("daemon.pid"), std::process::id().to_string()).expect("live pid");
        let run = || {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(RECORDING_PRUNE_SCRIPT)
                .arg("flotilla-prune-cleat-recordings")
                .arg(temp.path())
                .arg("work@2")
                .output()
                .expect("run prune script")
        };
        assert!(run().status.success());
        assert!(daemon.join("sessions/live/session.cast").exists(), "live daemon recording is retained");
        std::fs::write(daemon.join("daemon.pid"), "999999999").expect("dead pid");
        assert!(run().status.success());
        assert!(!daemon.join("sessions/expired").exists(), "expired recovered recording is removed");
        assert!(daemon.join("sessions/recent/session.cast").exists(), "recent recording is retained");
    }

    #[tokio::test]
    async fn direct_endpoint_uses_physical_daemon_that_contains_session() {
        let inventory = r#"[
            {"name":"work@2","runtime_root":"/state/cleat","alive":true},
            {"name":"work@3","runtime_root":"/state/cleat","alive":true}
        ]"#;
        let runner = Arc::new(MockRunner::new(vec![
            Ok(inventory.into()),
            Ok("[]".into()),
            Ok(r#"[{"id":"session-42","cwd":null,"cmd":null,"status":"Detached"}]"#.into()),
        ]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");
        let endpoint = pool.cleat_endpoint("session-42").await.expect("endpoint").expect("physical daemon");
        assert_eq!(endpoint.runtime_root, "/state/cleat");
        assert_eq!(endpoint.daemon, "work@3");
        assert_eq!(endpoint.session, "session-42");
        assert_eq!(logical_calls(&runner)[2].1, vec!["--runtime-root", "/state/cleat", "--server", "work@3", "list", "--json"]);
    }

    #[tokio::test]
    async fn direct_endpoint_shares_inventory_across_sessions() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok(r#"[{"name":"work@3","runtime_root":"/state/cleat","alive":true}]"#.into()),
            Ok(r#"[{"id":"first","cwd":null,"cmd":null,"status":"Detached"},{"id":"second","cwd":null,"cmd":null,"status":"Detached"}]"#
                .into()),
        ]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        assert_eq!(pool.cleat_endpoint("first").await.expect("first").expect("endpoint").session, "first");
        assert_eq!(pool.cleat_endpoint("second").await.expect("second").expect("endpoint").session, "second");
        assert_eq!(logical_calls(&runner).len(), 2);
    }

    #[tokio::test]
    async fn list_sessions_parses_json() {
        let json = r#"[
            {"id":"sess-1","cwd":"/repo","cmd":"bash","status":"Attached","screen_activity":"active"},
            {"id":"sess-2","cwd":"/other","cmd":null,"status":"Detached","screen_activity":"stable"}
        ]"#;
        let pool = test_pool(Arc::new(MockRunner::new(vec![Ok(json.into())])), "cleat");

        let sessions = pool.list_sessions().await.expect("list sessions");

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_name, "sess-1");
        assert_eq!(sessions[0].status, flotilla_protocol::TerminalStatus::Running);
        assert_eq!(sessions[0].command.as_deref(), Some("bash"));
        assert_eq!(sessions[0].working_directory.as_ref().map(|p| p.as_path()), Some(Path::new("/repo")));
        assert_eq!(sessions[0].screen_activity, Some(ScreenActivity::Active));
        assert_eq!(sessions[1].session_name, "sess-2");
        assert_eq!(sessions[1].status, flotilla_protocol::TerminalStatus::Disconnected);
        assert!(sessions[1].command.is_none());
        assert_eq!(sessions[1].working_directory.as_ref().map(|p| p.as_path()), Some(Path::new("/other")));
        assert_eq!(sessions[1].screen_activity, Some(ScreenActivity::Stable));
    }

    #[tokio::test]
    async fn dead_daemon_generation_is_not_a_live_session() {
        let json = r#"[{"id":"sess-1","cwd":"/repo","cmd":"codex","status":"Detached","error":"daemon generation is dead; session is recreatable"}]"#;
        let pool = test_pool(Arc::new(MockRunner::new(vec![Ok(json.into()), Ok(json.into())])), "cleat");

        assert_eq!(
            pool.session_liveness("sess-1").await.expect("liveness"),
            super::TerminalSessionLiveness::Lost("daemon generation is dead; session is recreatable".into())
        );
        assert!(pool.list_sessions().await.expect("list sessions").is_empty());
    }

    #[tokio::test]
    async fn capture_screen_reads_the_rendered_terminal() {
        let runner = Arc::new(MockRunner::new(vec![Ok("Do you trust the contents of this directory?\n".into())]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        assert_eq!(
            pool.capture_screen("terminal-demo-work-coder").await.expect("capture screen").as_deref(),
            Some("Do you trust the contents of this directory?\n")
        );
        assert_eq!(logical_calls(&runner)[0], ("cleat".to_string(), vec!["capture".to_string(), "terminal-demo-work-coder".to_string()]));
    }

    #[tokio::test]
    async fn ensure_launches_recorded_session() {
        let create_json = r#"{"id":"my-session","cwd":"/repo","cmd":"bash","status":"Detached"}"#;
        let runner = Arc::new(MockRunner::new(vec![
            Ok("[]".into()),        // list_sessions: empty (session doesn't exist)
            Ok(create_json.into()), // launch response
        ]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        pool.ensure_session("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![], &[]).await.expect("ensure session");

        let calls = logical_calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "cleat");
        assert_eq!(calls[1].1, vec!["launch", "--env-clear", "--json", "--record", "my-session", "--cwd", "/repo", "--cmd", "bash"]);
    }

    #[tokio::test]
    async fn ensure_launches_without_env_clear_when_cleat_lacks_it() {
        // `--env` in the stub help is deliberately a prefix of `--env-clear`: the probe must match
        // whole words, not substrings.
        let runner = Arc::new(MockRunner::new(vec![Ok("[]".into()), Ok("Usage: cleat launch [OPTIONS] --env".into()), Ok("{}".into())]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat", &EnvironmentBag::new());

        pool.ensure_session("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![], &[]).await.expect("ensure session");

        let calls = logical_calls(&runner);
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1].1, vec!["launch", "--help"]);
        assert_eq!(calls[2].1, vec!["launch", "--json", "--record", "my-session", "--cwd", "/repo", "--cmd", "bash"]);
    }

    #[tokio::test]
    async fn ensure_launches_session_with_requested_initial_size() {
        let runner = Arc::new(MockRunner::new(vec![Ok("[]".into()), Ok("{}".into())]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        pool.ensure_session_with_size(
            "my-session",
            "bash",
            &ExecutionEnvironmentPath::new("/repo"),
            &vec![],
            &[],
            Some(TerminalSize::new(200, 50)),
        )
        .await
        .expect("ensure sized session");

        assert_eq!(
            logical_calls(&runner)[1].1,
            vec!["launch", "--env-clear", "--json", "--record", "my-session", "--cwd", "/repo", "--cmd", "bash", "--size", "200x50",]
        );
    }

    #[tokio::test]
    async fn ensure_session_passes_env_vars_to_launch() {
        let create_json = r#"{"id":"my-session","cwd":"/repo","cmd":"claude","status":"Detached"}"#;
        let runner = Arc::new(MockRunner::new(vec![
            Ok("[]".into()),        // list_sessions: empty
            Ok(create_json.into()), // launch response
        ]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");
        let env = vec![
            ("FOO".to_string(), "bar baz".to_string()),
            ("FLOTILLA_CREW_ID".to_string(), "crew-123".to_string()),
            ("GITHUB_TOKEN_FILE".to_string(), "/run/credentials/github-app/token".to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/run/credentials/gitconfig".to_string()),
        ];

        pool.ensure_session("my-session", "claude", &ExecutionEnvironmentPath::new("/repo"), &env, &[]).await.expect("ensure session");

        let calls = logical_calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "cleat");
        let cmd_idx = calls[1].1.iter().position(|a| a == "--cmd").expect("--cmd present");
        assert_eq!(calls[1].1[cmd_idx + 1], "claude");
        assert!(calls[1].1.windows(2).any(|args| args == ["--env", "FOO=bar baz"]));
        assert!(calls[1].1.windows(2).any(|args| args == ["--env", "FLOTILLA_CREW_ID=crew-123"]));
        assert!(calls[1].1.windows(2).any(|args| args == ["--env", "GITHUB_TOKEN_FILE=/run/credentials/github-app/token"]));
        assert!(calls[1].1.windows(2).any(|args| args == ["--env", "GIT_CONFIG_GLOBAL=/run/credentials/gitconfig"]));
    }

    #[tokio::test]
    async fn ensure_session_tags_convoy_and_vessel() {
        let runner = Arc::new(MockRunner::new(vec![Ok("[]".into()), Ok("{}".into())]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        pool.ensure_session(
            "terminal-demo-coder",
            "codex",
            &ExecutionEnvironmentPath::new("/repo"),
            &vec![],
            &[TerminalSessionTag::new("convoy", "demo"), TerminalSessionTag::new("vessel", "demo-work")],
        )
        .await
        .expect("ensure tagged session");

        assert_eq!(
            logical_calls(&runner)[1].1,
            vec![
                "launch",
                "--env-clear",
                "--json",
                "--record",
                "terminal-demo-coder",
                "--cwd",
                "/repo",
                "--cmd",
                "codex",
                "--tag",
                "convoy=demo",
                "--tag",
                "vessel=demo-work",
            ]
        );
    }

    #[tokio::test]
    async fn ensure_sized_session_preserves_existing_live_session() {
        let list_json = r#"[{"id":"my-session","cwd":"/repo","cmd":"claude","status":"Detached"}]"#;
        let runner = Arc::new(MockRunner::new(vec![
            Ok(list_json.into()), // list_sessions: session exists
        ]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");
        let env = vec![("FOO".to_string(), "bar".to_string())];

        pool.ensure_session_with_size(
            "my-session",
            "claude",
            &ExecutionEnvironmentPath::new("/repo"),
            &env,
            &[],
            Some(TerminalSize::new(200, 50)),
        )
        .await
        .expect("ensure session");

        let calls = logical_calls(&runner);
        assert_eq!(calls.len(), 1, "should only call list, not launch: {calls:?}");
        assert!(calls[0].1.contains(&"list".to_string()), "should be a list call: {:?}", calls[0].1);
    }

    #[tokio::test]
    async fn attach_wraps_command() {
        let pool = test_pool(Arc::new(MockRunner::new(vec![])), "cleat");

        let cmd =
            pool.attach_command("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).await.expect("attach command");

        assert_eq!(cmd, "cleat attach --no-create my-session");
        assert!(!cmd.contains("--cmd"), "should NOT have --cmd");
    }

    #[tokio::test]
    async fn kill_calls_cli() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        pool.kill_session("my-session").await.expect("kill session");

        let calls = logical_calls(&runner);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "cleat");
        assert_eq!(calls[0].1, vec!["kill", "my-session"]);
    }

    #[tokio::test(start_paused = true)]
    async fn delivery_writes_bracketed_paste_then_enter_for_single_and_multiline_messages() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new()), Ok(String::new()), Ok(String::new()), Ok(String::new())]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        let single_started = tokio::time::Instant::now();
        pool.deliver("reviewer-session", "Please review commit abc123").await.expect("deliver single-line message");
        assert_eq!(single_started.elapsed(), DELIVERY_ENTER_DELAY);
        let multiline_started = tokio::time::Instant::now();
        pool.deliver("reviewer-session", "handoff from coder@work\n\nPlease review commit abc123")
            .await
            .expect("deliver multiline message");
        assert_eq!(multiline_started.elapsed(), DELIVERY_ENTER_DELAY);

        let calls = logical_calls(&runner);
        assert_eq!(calls[0].0, "cleat");
        assert_eq!(calls[0].1, vec!["send", "reviewer-session", "\x1b[200~Please review commit abc123\x1b[201~", "--no-enter"]);
        assert_eq!(calls[1].1, vec!["send-keys", "reviewer-session", "Enter"]);
        assert_eq!(
            calls[2].1,
            vec!["send", "reviewer-session", "\x1b[200~handoff from coder@work\n\nPlease review commit abc123\x1b[201~", "--no-enter"]
        );
        assert_eq!(calls[3].1, vec!["send-keys", "reviewer-session", "Enter"]);
    }

    // ── attach_args tests ──────────────────────────────────────────

    #[test]
    fn attach_args_with_command_no_env() {
        let pool = test_pool(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).expect("attach_args");

        assert_eq!(
            args,
            vec![
                Arg::Literal("cleat".into()),
                Arg::Literal("attach".into()),
                Arg::Literal("--no-create".into()),
                Arg::Literal("my-session".into()),
            ]
        );
    }

    #[test]
    fn attach_args_flatten_with_command_no_env() {
        let pool = test_pool(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        assert_eq!(flat, "cleat attach --no-create my-session");
    }

    #[test]
    fn human_take_attach_adds_take_while_watch_omits_it() {
        let pool = test_pool(Arc::new(MockRunner::new(vec![])), "cleat");
        let take = pool
            .attach_args_for_mode("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![], AttachMode::PreferTake)
            .expect("take attach args");
        let watch = pool
            .attach_args_for_mode("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![], AttachMode::Default)
            .expect("watch attach args");

        assert_eq!(flotilla_protocol::arg::flatten(&take, 0), "cleat attach --no-create --take my-session");
        assert_eq!(flotilla_protocol::arg::flatten(&watch, 0), "cleat attach --no-create my-session");
    }

    #[tokio::test]
    async fn attach_preflight_accepts_controller_seat_capabilities() {
        let runner = Arc::new(MockRunner::new(vec![Ok("Options:\n  --strict\n  --take\n".into())]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "/pool/bin/cleat");

        pool.preflight_attach(AttachMode::Default).await.expect("modern cleat should pass preflight");
        pool.preflight_attach(AttachMode::Take).await.expect("capability result should be cached");

        assert_eq!(logical_calls(&runner), [("/pool/bin/cleat".to_string(), vec!["attach".to_string(), "--help".to_string()])]);
    }

    #[tokio::test]
    async fn attach_preflight_names_a_stale_pool_binary_and_version() {
        let runner = Arc::new(MockRunner::new(vec![Ok("Options:\n  --no-create\n".into()), Ok("cleat 0.5.0".into())]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "/pool/bin/cleat");

        let error = pool.preflight_attach(AttachMode::Default).await.expect_err("stale cleat should fail preflight");

        assert!(error.contains("/pool/bin/cleat"), "{error}");
        assert!(error.contains("cleat 0.5.0"), "{error}");
        assert!(error.contains("--strict/--take"), "{error}");
    }

    #[tokio::test]
    async fn attach_preflight_retries_after_a_stale_binary_is_upgraded() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok("Options:\n  --no-create\n".into()),
            Ok("cleat 0.5.0".into()),
            Ok("Options:\n  --strict\n  --take\n".into()),
        ]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "/pool/bin/cleat");

        pool.preflight_attach(AttachMode::Default).await.expect_err("stale cleat should fail preflight");
        pool.preflight_attach(AttachMode::Default).await.expect("upgraded cleat should pass without restarting the daemon");

        assert_eq!(logical_calls(&runner).len(), 3, "failed capability probes must not be cached");
    }

    #[test]
    fn attach_args_empty_command_no_env() {
        let pool = test_pool(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("sess-1", "", &ExecutionEnvironmentPath::new("/home/dev"), &vec![]).expect("attach_args");

        // Same structure regardless of command
        assert_eq!(
            args,
            vec![
                Arg::Literal("cleat".into()),
                Arg::Literal("attach".into()),
                Arg::Literal("--no-create".into()),
                Arg::Literal("sess-1".into()),
            ]
        );
    }

    #[test]
    fn attach_args_with_env_vars() {
        let pool = test_pool(Arc::new(MockRunner::new(vec![])), "cleat");
        let env = vec![("FOO".to_string(), "bar".to_string()), ("BAZ".to_string(), "qu'x".to_string())];
        let args = pool.attach_args("sess", "cmd", &ExecutionEnvironmentPath::new("/wd"), &env).expect("attach_args");

        // Env vars are baked in at ensure_session/launch time — not in attach_args
        assert_eq!(
            args,
            vec![
                Arg::Literal("cleat".into()),
                Arg::Literal("attach".into()),
                Arg::Literal("--no-create".into()),
                Arg::Literal("sess".into()),
            ]
        );
    }

    #[test]
    fn attach_args_with_env_vars_empty_command() {
        let pool = test_pool(Arc::new(MockRunner::new(vec![])), "cleat");
        let env = vec![("KEY".to_string(), "val".to_string())];
        let args = pool.attach_args("sess", "", &ExecutionEnvironmentPath::new("/wd"), &env).expect("attach_args");

        assert_eq!(
            args,
            vec![
                Arg::Literal("cleat".into()),
                Arg::Literal("attach".into()),
                Arg::Literal("--no-create".into()),
                Arg::Literal("sess".into()),
            ]
        );
    }

    #[test]
    fn attach_args_flatten_roundtrip_env_vars() {
        let pool = test_pool(Arc::new(MockRunner::new(vec![])), "cleat");
        let env = vec![("FOO".to_string(), "bar".to_string())];
        let args = pool.attach_args("sess", "bash", &ExecutionEnvironmentPath::new("/wd"), &env).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        // No --cmd, no NestedCommand
        assert_eq!(flat, "cleat attach --no-create sess");
    }

    #[tokio::test]
    async fn ensure_session_preserves_explicit_terminal_identity_and_unrelated_env() {
        let runner = Arc::new(MockRunner::new(vec![Ok("[]".into()), Ok("{}".into())]));
        let pool = test_pool(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");
        let caller_env = vec![
            ("TERM".to_string(), "screen-256color".to_string()),
            ("TERM_PROGRAM".to_string(), "my-terminal".to_string()),
            ("TERM_PROGRAM_VERSION".to_string(), "2.0".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
            ("FOO".to_string(), "bar".to_string()),
        ];

        pool.ensure_session("sess", "claude", &ExecutionEnvironmentPath::new("/repo"), &caller_env, &[]).await.expect("ensure session");

        let calls = logical_calls(&runner);
        let cmd_idx = calls[1].1.iter().position(|a| a == "--cmd").expect("--cmd present");
        assert_eq!(calls[1].1[cmd_idx + 1], "claude");
        let launch_env = calls[1].1.windows(2).filter(|args| args[0] == "--env").map(|args| args[1].clone()).collect::<Vec<_>>();
        let expected_env = caller_env.iter().map(|(name, value)| format!("{name}={value}")).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(launch_env.into_iter().collect::<std::collections::BTreeSet<_>>(), expected_env);
    }

    #[tokio::test]
    async fn retry_delivery_clears_a_stuck_composer_before_resubmitting() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new()), Ok(String::new()), Ok(String::new())]));
        let pool = test_pool(runner.clone(), "cleat");

        pool.retry_delivery("reviewer-session", "Please review commit abc123").await.expect("retry delivery");

        let calls = logical_calls(&runner);
        assert_eq!(calls[0].1, vec!["send-keys", "reviewer-session", "C-c"]);
        assert_eq!(calls[1].1, vec!["send", "reviewer-session", "\x1b[200~Please review commit abc123\x1b[201~", "--no-enter"]);
        assert_eq!(calls[2].1, vec!["send-keys", "reviewer-session", "Enter"]);
    }
}

#[cfg(test)]
#[path = "cleat/environment_tests.rs"]
mod environment_tests;
