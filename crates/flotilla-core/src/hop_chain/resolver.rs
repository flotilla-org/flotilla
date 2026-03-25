use std::sync::Arc;

use super::{
    environment::EnvironmentHopResolver, remote::RemoteHopResolver, terminal::TerminalHopResolver, Hop, HopPlan, ResolutionContext,
    ResolvedAction, ResolvedPlan,
};

/// Decides whether to wrap (nest inner command as argument) or sendkeys
/// (create an execution boundary) at each combination point during resolution.
pub trait CombineStrategy: Send + Sync {
    fn should_wrap(&self, hop: &Hop, context: &ResolutionContext) -> bool;
}

/// Always nests commands as arguments. Matches current SSH wrapping behavior. Default.
pub struct AlwaysWrap;

impl CombineStrategy for AlwaysWrap {
    fn should_wrap(&self, _hop: &Hop, _context: &ResolutionContext) -> bool {
        true
    }
}

/// Always creates execution boundaries. For resolution-level testing only in Phase A.
pub struct AlwaysSendKeys;

impl CombineStrategy for AlwaysSendKeys {
    fn should_wrap(&self, _hop: &Hop, _context: &ResolutionContext) -> bool {
        false
    }
}

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
    pub strategy: Arc<dyn CombineStrategy>,
}

impl HopResolver {
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
                Hop::EnterEnvironment { env_id, .. } => {
                    if context.current_environment.as_ref() == Some(env_id) {
                        continue; // collapse — already inside this environment
                    }
                    if self.strategy.should_wrap(hop, context) {
                        self.environment.resolve_wrap(env_id, context)?;
                    } else {
                        self.environment.resolve_enter(env_id, context)?;
                    }
                    context.nesting_depth += 1;
                    context.current_environment = Some(env_id.clone());
                }
                Hop::RemoteToHost { host } => {
                    if *host == context.current_host {
                        continue; // collapse — already at this host
                    }
                    if self.strategy.should_wrap(hop, context) {
                        self.remote.resolve_wrap(host, context)?;
                    } else {
                        self.remote.resolve_enter(host, context)?;
                    }
                    context.nesting_depth += 1;
                    context.current_host = host.clone();
                }
            }
        }
        Ok(ResolvedPlan(std::mem::take(&mut context.actions)))
    }
}
