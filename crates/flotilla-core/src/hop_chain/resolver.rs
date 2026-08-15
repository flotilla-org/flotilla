use std::sync::Arc;

use super::{
    environment::EnvironmentHopResolver, remote::RemoteHopResolver, terminal::TerminalHopResolver, Hop, HopPlan, ResolutionContext,
    ResolvedAction, ResolvedPlan,
};

/// Composes per-hop resolvers into the full resolution algorithm.
///
/// Walks the hop plan inside-out (last hop first). Each hop type delegates
/// to the appropriate per-hop resolver which mutates the `ResolutionContext`:
/// - `RunCommand`: pushes a `Command` action directly
/// - `AttachTerminal`: delegates to `TerminalHopResolver`
/// - `EnterEnvironment`: delegates to `EnvironmentHopResolver` (wrap or enter based on strategy)
/// - `RemoteToHost`: delegates to `RemoteHopResolver` (wrap or enter based on strategy)
pub struct HopResolver {
    pub remote: Arc<dyn RemoteHopResolver>,
    pub environment: Arc<dyn EnvironmentHopResolver>,
    pub terminal: Arc<dyn TerminalHopResolver>,
}

impl HopResolver {
    pub fn new(
        remote: Arc<dyn RemoteHopResolver>,
        environment: Arc<dyn EnvironmentHopResolver>,
        terminal: Arc<dyn TerminalHopResolver>,
    ) -> Self {
        Self { remote, environment, terminal }
    }

    pub fn resolve(&self, plan: &HopPlan, context: &mut ResolutionContext) -> Result<ResolvedPlan, String> {
        // Walk inside-out (reverse order)
        for hop in plan.0.iter().rev() {
            match hop {
                Hop::RunCommand { command } => {
                    context.actions.push(ResolvedAction::Command(command.clone()));
                }
                Hop::AttachTerminal { attachable_id } => {
                    self.terminal.resolve(attachable_id, context)?;
                }
                // Phase C: `provider` field is unused — a single EnvironmentHopResolver
                // handles all environments. Phase D will use it to route to provider-specific
                // resolvers when multiple environment backends coexist.
                Hop::EnterEnvironment { env_id, .. } => {
                    if context.current_environment.as_ref() == Some(env_id) {
                        continue; // collapse — already inside this environment
                    }
                    self.environment.resolve_wrap(env_id, context)?;
                    context.nesting_depth += 1;
                    context.current_environment = Some(env_id.clone());
                }
                Hop::RemoteToHost { host } => {
                    if *host == context.current_host {
                        continue; // collapse — already at this host
                    }
                    self.remote.resolve_wrap(host, context)?;
                    context.nesting_depth += 1;
                    context.current_host = host.clone();
                }
            }
        }
        Ok(ResolvedPlan(std::mem::take(&mut context.actions)))
    }
}
