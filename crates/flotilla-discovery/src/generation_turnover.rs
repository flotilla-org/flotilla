//! Generation turnover fan-out over registered environments. Each local
//! environment is handed to the terminal pools that run there; a pool that
//! offers turnover advances its own backend and reports typed outcomes.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::Arc,
};

use flotilla_protocol::{result_set::CleatEndpoint, EnvironmentId};
use flotilla_resources::{ConditionValue, HostCondition};
use flotilla_terminal::generation::{BuildAssessment, GenerationTurnover, IncomingBuild, RollReport, TurnoverEnvironment};

use crate::environment_manager::{EnvironmentManager, ManagedEnvironmentKind};

/// A terminal pool by the name its sessions record, with its turnover if it offers one.
#[derive(Clone)]
pub struct PoolTurnover {
    pub pool: String,
    pub facility: Option<Arc<dyn GenerationTurnover>>,
}

/// A running TerminalSession in a registered environment.
#[derive(Clone, Debug)]
pub struct RecordedSession {
    pub pool: String,
    pub environment: EnvironmentId,
    pub vessel: String,
    pub endpoint: Option<CleatEndpoint>,
}

#[derive(Clone, Debug, Default)]
pub struct RecordedSessions {
    pub running: Vec<RecordedSession>,
    /// Diagnostics for sessions whose local environment is not registered, by pool.
    pub unavailable: Vec<(String, String)>,
    /// Inventory failures that apply to every pool.
    pub errors: Vec<String>,
}

pub struct AdvanceRequest<'a> {
    pub host: String,
    pub generation: &'a str,
    /// The installed executable, inside its release's `bin`.
    pub incoming: &'a Path,
    /// Where contained environments' shared tools are published.
    pub tools_root: &'a Path,
    pub sessions: &'a RecordedSessions,
    pub previous: Option<&'a RollReport>,
}

pub struct GenerationTurnovers<'a> {
    environments: &'a EnvironmentManager,
    pools: &'a [PoolTurnover],
}

impl<'a> GenerationTurnovers<'a> {
    pub fn new(environments: &'a EnvironmentManager, pools: &'a [PoolTurnover]) -> Self {
        Self { environments, pools }
    }

    fn local(&self, state: &ManagedEnvironmentKind) -> bool {
        match state {
            ManagedEnvironmentKind::Direct(state) => state.host_id.as_ref() == Some(self.environments.local_host_id()),
            ManagedEnvironmentKind::Provisioned(state) => &state.owning_host_id == self.environments.local_host_id(),
        }
    }

    /// Local environments the pool runs in: those that declare it, hold its
    /// recorded endpoints, or (when `required`) the daemon's own environment.
    fn environments(
        &self,
        pool: &str,
        facility: &dyn GenerationTurnover,
        sessions: &RecordedSessions,
        required: bool,
    ) -> (Vec<TurnoverEnvironment>, Vec<String>) {
        let mut endpoints = HashMap::<_, Vec<CleatEndpoint>>::new();
        let mut vessels = HashMap::<_, BTreeSet<String>>::new();
        for session in sessions.running.iter().filter(|session| session.pool == pool) {
            if let Some(endpoint) = &session.endpoint {
                vessels.entry(session.environment.clone()).or_default().insert(session.vessel.clone());
                endpoints.entry(session.environment.clone()).or_default().push(endpoint.clone());
            }
        }
        let mut errors: Vec<_> =
            sessions.unavailable.iter().filter(|(unavailable, _)| unavailable == pool).map(|(_, message)| message.clone()).collect();
        let mut environments = Vec::new();
        for (id, state) in self.environments.managed_environments() {
            let local = self.local(&state);
            let (bag, contained) = match state {
                ManagedEnvironmentKind::Direct(state) => (state.env_bag, false),
                ManagedEnvironmentKind::Provisioned(state) => (state.env_bag, true),
            };
            if !local
                || (!facility.declared(&bag)
                    && !endpoints.contains_key(&id)
                    && !(required && &id == self.environments.local_environment_id()))
            {
                continue;
            }
            let Some(runner) = self.environments.environment_runner(&id) else {
                errors.push(format!("{pool} runner unavailable for {id}"));
                continue;
            };
            environments.push(
                TurnoverEnvironment::builder()
                    .id(id.to_string())
                    .bag(bag)
                    .runner(runner)
                    .contained(contained)
                    .endpoints(endpoints.remove(&id).unwrap_or_default())
                    .vessels(vessels.remove(&id).map_or_else(Vec::new, |vessels| vessels.into_iter().collect()))
                    .build(),
            );
        }
        (environments, errors)
    }

    /// Running sessions in local environments whose pool offers no turnover.
    fn skipped(&self, sessions: &RecordedSessions) -> Vec<String> {
        let local: BTreeSet<_> =
            self.environments.managed_environments().into_iter().filter(|(_, state)| self.local(state)).map(|(id, _)| id).collect();
        let mut counts = BTreeMap::<_, usize>::new();
        for session in &sessions.running {
            let offered = self.pools.iter().any(|pool| pool.pool == session.pool && pool.facility.is_some());
            if !offered && local.contains(&session.environment) {
                *counts.entry((session.environment.clone(), session.pool.clone())).or_default() += 1;
            }
        }
        counts
            .into_iter()
            .map(|((environment, pool), count)| {
                format!("{environment}: terminal pool {pool} offers no generation turnover; {count} running sessions keep their generation")
            })
            .collect()
    }

    /// Hand every local environment's live sessions to the installed generation.
    pub async fn advance(&self, request: AdvanceRequest<'_>) -> RollReport {
        let mut errors = request.sessions.errors.clone();
        let mut plans = Vec::new();
        for pool in self.pools {
            let Some(facility) = &pool.facility else { continue };
            let (environments, pool_errors) = self.environments(&pool.pool, facility.as_ref(), request.sessions, true);
            errors.extend(pool_errors);
            plans.push((facility, environments));
        }
        let contained = plans.iter().any(|(_, environments)| environments.iter().any(|environment| environment.contained));
        let guest = guest_build(request.incoming, request.tools_root, contained, &mut errors);
        let incoming = IncomingBuild { generation: request.generation.to_string(), installed: request.incoming.to_path_buf(), guest };
        let mut report =
            RollReport::builder().host(request.host).generation(request.generation.to_string()).attempts(vec![]).errors(errors).build();
        for (facility, environments) in plans {
            let turnover = facility.advance(&environments, &incoming).await;
            report.attempts.extend(turnover.attempts);
            report.errors.extend(turnover.errors);
            report.information.extend(turnover.information);
        }
        report.information.extend(self.skipped(request.sessions));
        // A registered environment can temporarily lose its runner. Retain its
        // previous failed attempt until a retry actually addresses that target.
        if let Some(previous) = request.previous {
            let registered = self.environments.managed_environments().into_iter().map(|(id, _)| id.to_string()).collect();
            retain_unattempted_failures(previous, &mut report, &registered);
        }
        #[cfg(target_os = "linux")]
        if request.tools_root.exists() {
            if let Err(error) = flotilla_environment::tools::prune_local(request.tools_root) {
                report.errors.push(format!("prune vessel tools: {error}"));
            }
        }
        report
    }

    /// One host condition per offering pool whose build the local environment has.
    pub async fn conditions(
        &self,
        sessions: &RecordedSessions,
        previous: Option<&RollReport>,
        observed_at: chrono::DateTime<chrono::Utc>,
    ) -> Vec<HostCondition> {
        let local = self.environments.local_environment_id();
        let (Some(bag), Some(runner)) = (self.environments.environment_bag(local), self.environments.environment_runner(local)) else {
            tracing::debug!(environment = %local, "local environment unregistered; no generation conditions");
            return Vec::new();
        };
        let host = TurnoverEnvironment::builder().id(local.to_string()).bag(bag).runner(runner).contained(false).build();
        let mut conditions = Vec::new();
        for pool in self.pools {
            let Some(facility) = &pool.facility else { continue };
            let (environments, pool_errors) = self.environments(&pool.pool, facility.as_ref(), sessions, false);
            let errors = sessions.errors.iter().cloned().chain(pool_errors).collect();
            if let Some(assessment) = facility.assess(&host, &environments, previous, errors).await {
                conditions.extend(condition(assessment, facility.condition_type(), observed_at));
            }
        }
        conditions
    }
}

fn guest_build(
    incoming: &Path,
    tools_root: &Path,
    contained: bool,
    errors: &mut Vec<String>,
) -> Result<BTreeMap<String, std::path::PathBuf>, String> {
    if !contained {
        return Err("no contained environment needs a guest build".into());
    }
    #[cfg(unix)]
    {
        use flotilla_environment::guest_tools::{linux_amd64, GuestTools};
        let tools = incoming
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| "incoming cleat has no release root".to_string())
            .and_then(|release| GuestTools::resolve(release, &linux_amd64()));
        // Refresh current-layout shared mounts independently of handover.
        // Legacy targets remain reachable even if this publication fails.
        let published = tools
            .as_ref()
            .map_err(String::clone)
            .and_then(|tools| flotilla_environment::tools::publish(tools_root, tools.into()).map_err(|error| error.to_string()));
        if let Err(error) = published {
            errors.push(format!("publish vessel tools: {error}"));
        }
        tools.map(|tools| tools.files())
    }
    #[cfg(not(unix))]
    {
        let _ = (incoming, tools_root, errors);
        Err("contained delivery requires a supported guest-tool controller".into())
    }
}

/// Generation health as an advisory host condition: degraded only by actionable skew.
pub fn condition(assessment: BuildAssessment, condition_type: &str, observed_at: chrono::DateTime<chrono::Utc>) -> Option<HostCondition> {
    let degraded = !assessment.actionable.is_empty();
    let mut messages = assessment.actionable;
    messages.extend(assessment.information);
    if messages.is_empty() {
        return None;
    }
    Some(
        HostCondition::builder()
            .condition_type(condition_type)
            .value(if degraded { ConditionValue::False } else { ConditionValue::True })
            .reason(if degraded { "InstalledServingMismatch" } else { "ExpectedGenerationSkew" })
            .message(messages.join("; "))
            .observed_at(observed_at)
            .blocks_readiness(false)
            // Terminal build skew does not disable forge reads.
            .blocks_forge_observation(false)
            .build(),
    )
}

fn retain_unattempted_failures(previous: &RollReport, current: &mut RollReport, registered: &BTreeSet<String>) {
    for attempt in &previous.attempts {
        if attempt.error.is_some()
            && registered.contains(&attempt.environment)
            && !current.attempts.iter().any(|new| new.same_target(attempt))
        {
            current.attempts.push(attempt.clone());
        }
    }
}

#[cfg(test)]
mod tests;
