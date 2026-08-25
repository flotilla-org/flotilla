use std::collections::HashMap;

use flotilla_protocol::{arg::Arg, EnvironmentId};

use super::{ResolutionContext, ResolvedAction};

// Docker keeps an exec process alive when the client side of `docker exec`
// disappears. Keep a shell on the client side so its EXIT/HUP trap can reap
// the process that owns the controller seat inside the container.
const DOCKER_ATTACH_WRAPPER: &str = r#"
container=$1
workdir=$2
attach_wrapper=$3
shift 3
lease=$$
pid_file=/tmp/flotilla-attach-$lease.pid
cleanup() {
    docker exec "$container" sh -c '
        lease=$1
        pid_file=$2
        pid=$(cat "$pid_file" 2>/dev/null) || exit 0
        if [ -r "/proc/$pid/environ" ] && tr "\000" "\n" < "/proc/$pid/environ" | grep -Fqx "FLOTILLA_ATTACH_LEASE=$lease"; then
            kill -KILL "$pid" 2>/dev/null || true
        fi
        rm -f "$pid_file"
    ' flotilla-attach-cleanup "$lease" "$pid_file" >/dev/null 2>&1 &
    cleanup_pid=$!
    (
        sleep 5
        kill -TERM "$cleanup_pid" 2>/dev/null || exit 0
        sleep 1
        kill -KILL "$cleanup_pid" 2>/dev/null || true
    ) &
    watchdog_pid=$!
    wait "$cleanup_pid" 2>/dev/null || true
    kill "$watchdog_pid" 2>/dev/null || true
    wait "$watchdog_pid" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM
if [ -n "$workdir" ]; then
    set -- exec -it -w "$workdir" -e "FLOTILLA_ATTACH_LEASE=$lease" "$container" sh -c "$attach_wrapper" flotilla-attach "$pid_file" "$@"
else
    set -- exec -it -e "FLOTILLA_ATTACH_LEASE=$lease" "$container" sh -c "$attach_wrapper" flotilla-attach "$pid_file" "$@"
fi
docker "$@"
"#;

pub(super) const DOCKER_ATTACH_INNER_WRAPPER: &str =
    r#"umask 077; set -C; exec 3>"$1" || exit 1; printf '%s\n' "$$" >&3; exec 3>&-; shift; exec "$@""#;

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
    /// Wrap case: pop the inner Command and wrap it in a supervised
    /// `docker exec`. The client-side shell reaps the in-container process if
    /// an upstream SSH/terminal hop disappears without Docker propagating it.
    fn resolve_wrap(&self, env_id: &EnvironmentId, context: &mut ResolutionContext) -> Result<(), String> {
        let container = self.container_name(env_id)?;

        let inner_action = context.actions.pop().ok_or("resolve_wrap: no inner action on stack")?;
        let ResolvedAction::Command(inner_args) = inner_action;

        let mut docker_args = vec![
            Arg::Literal("sh".into()),
            Arg::Literal("-c".into()),
            Arg::Quoted(DOCKER_ATTACH_WRAPPER.into()),
            Arg::Literal("flotilla-docker-attach".into()),
            Arg::Quoted(container.to_string()),
            Arg::Quoted(context.working_directory.take().map_or_else(String::new, |dir| dir.to_string())),
            Arg::Quoted(DOCKER_ATTACH_INNER_WRAPPER.into()),
        ];
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
