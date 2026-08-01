use std::collections::HashMap;

use flotilla_protocol::{arg::Arg, EnvironmentId};

use super::{ResolutionContext, ResolvedAction, SendKeyStep};

/// Resolves a `Hop::EnterEnvironment` into environment-specific actions on the context.
///
/// Two methods for the two combination strategies:
/// - `resolve_wrap`: nest the inner command inside a docker exec invocation
/// - `resolve_enter`: open a docker exec shell, then type the inner command
pub trait EnvironmentHopResolver: Send + Sync {
    fn resolve_wrap(&self, env_id: &EnvironmentId, context: &mut ResolutionContext) -> Result<(), String>;
    fn resolve_enter(&self, env_id: &EnvironmentId, context: &mut ResolutionContext) -> Result<(), String>;
}

/// Docker-based environment hop resolver. Maps environment IDs to container names
/// and wraps/enters commands via `docker exec`.
pub struct DockerEnvironmentHopResolver {
    containers: HashMap<EnvironmentId, String>,
}

impl DockerEnvironmentHopResolver {
    pub fn new(containers: HashMap<EnvironmentId, String>) -> Self {
        Self { containers }
    }

    fn container_name(&self, env_id: &EnvironmentId) -> Result<&str, String> {
        self.containers.get(env_id).map(|s| s.as_str()).ok_or_else(|| format!("unknown environment: {env_id}"))
    }
}

impl EnvironmentHopResolver for DockerEnvironmentHopResolver {
    /// Wrap case: pop the inner Command, wrap it in `docker exec -it <container> ...inner_args`.
    fn resolve_wrap(&self, env_id: &EnvironmentId, context: &mut ResolutionContext) -> Result<(), String> {
        let container = self.container_name(env_id)?;

        let inner_action = context.actions.pop().ok_or("resolve_wrap: no inner action on stack")?;
        let inner_args = match inner_action {
            ResolvedAction::Command(args) => args,
            other => return Err(format!("resolve_wrap: expected Command on stack, got {other:?}")),
        };

        let mut docker_args = vec![Arg::Literal("docker".into()), Arg::Literal("exec".into()), Arg::Literal("-it".into())];
        // Consume working_directory here — it's a container-local path, so it
        // must be passed via `docker exec -w`, not as a `cd` on the host.
        if let Some(dir) = context.working_directory.take() {
            docker_args.push(Arg::Literal("-w".into()));
            docker_args.push(Arg::Quoted(dir.to_string()));
        }
        docker_args.push(Arg::Quoted(container.to_string()));
        docker_args.extend(inner_args);

        context.actions.push(ResolvedAction::Command(docker_args));
        Ok(())
    }

    /// Enter case: push a `docker exec -it <container> /bin/sh` command that creates
    /// an execution boundary, then convert the inner command to SendKeys.
    fn resolve_enter(&self, env_id: &EnvironmentId, context: &mut ResolutionContext) -> Result<(), String> {
        let container = self.container_name(env_id)?;

        let inner_action = context.actions.pop().ok_or("resolve_enter: no inner action on stack")?;
        let inner_args = match inner_action {
            ResolvedAction::Command(args) => args,
            other => return Err(format!("resolve_enter: expected Command on stack, got {other:?}")),
        };

        // Convert inner command to SendKeys (if non-empty)
        if !inner_args.is_empty() {
            let pid_file = format!("/tmp/flotilla-attach-{}.pid", flotilla_protocol::ATTACH_LEASE_PLACEHOLDER);
            let inner_command = flotilla_protocol::arg::flatten(&inner_args, 0);
            let supervised_command = flotilla_protocol::arg::flatten(
                &[
                    Arg::Literal("exec".into()),
                    Arg::Literal("sh".into()),
                    Arg::Literal("-c".into()),
                    Arg::Quoted(format!("echo $$ > {pid_file}; exec {inner_command}")),
                ],
                0,
            );
            context.actions.push(ResolvedAction::SendKeys {
                hop: format!("docker environment '{env_id}'"),
                steps: vec![SendKeyStep::Type { text: supervised_command }, SendKeyStep::WaitForReady],
            });
            context.actions.push(ResolvedAction::Cleanup(vec![
                Arg::Literal("docker".into()),
                Arg::Literal("exec".into()),
                Arg::Quoted(container.to_string()),
                Arg::Literal("sh".into()),
                Arg::Literal("-c".into()),
                Arg::Quoted(format!(
                    "pid=$(cat {pid_file} 2>/dev/null) || exit 0; kill -KILL \"$pid\" 2>/dev/null || true; rm -f {pid_file}"
                )),
            ]));
        }

        // Push docker exec enter command
        let mut docker_args = vec![Arg::Literal("docker".into()), Arg::Literal("exec".into()), Arg::Literal("-it".into())];
        if let Some(dir) = context.working_directory.take() {
            docker_args.push(Arg::Literal("-w".into()));
            docker_args.push(Arg::Quoted(dir.to_string()));
        }
        docker_args.push(Arg::Quoted(container.to_string()));
        docker_args.push(Arg::Literal("/bin/sh".into()));
        context.actions.push(ResolvedAction::Command(docker_args));
        Ok(())
    }
}

/// No-op environment hop resolver that always errors. Used when the hop plan
/// contains no `EnterEnvironment` hops (e.g. non-containerized workflows).
pub struct NoopEnvironmentHopResolver;

impl EnvironmentHopResolver for NoopEnvironmentHopResolver {
    fn resolve_wrap(&self, env_id: &EnvironmentId, _context: &mut ResolutionContext) -> Result<(), String> {
        Err(format!("no environment transport available for environment: {env_id}"))
    }

    fn resolve_enter(&self, env_id: &EnvironmentId, _context: &mut ResolutionContext) -> Result<(), String> {
        Err(format!("no environment transport available for environment: {env_id}"))
    }
}
