use std::collections::HashMap;

use flotilla_protocol::{arg::Arg, EnvironmentId};

use super::{ResolutionContext, ResolvedAction};

/// Resolves a `Hop::EnterEnvironment` into environment-specific actions on the context.
///
pub trait EnvironmentHopResolver: Send + Sync {
    fn resolve_wrap(&self, env_id: &EnvironmentId, context: &mut ResolutionContext) -> Result<(), String>;
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
        let ResolvedAction::Command(inner_args) = inner_action;

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
}

/// No-op environment hop resolver that always errors. Used when the hop plan
/// contains no `EnterEnvironment` hops (e.g. non-containerized workflows).
pub struct NoopEnvironmentHopResolver;

impl EnvironmentHopResolver for NoopEnvironmentHopResolver {
    fn resolve_wrap(&self, env_id: &EnvironmentId, _context: &mut ResolutionContext) -> Result<(), String> {
        Err(format!("no environment transport available for environment: {env_id}"))
    }
}
