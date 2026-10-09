//! Credential-free crew launch probe, discovered only by an isolated canary.

use super::*;

pub(super) struct FleetCanaryAdapter;

#[async_trait]
impl AgentAdapter for FleetCanaryAdapter {
    fn id(&self) -> &'static str {
        "fleet-canary"
    }

    async fn prepare(&self, _cwd: &ExecutionEnvironmentPath, _brief: &TerminalBrief) -> Result<(), String> {
        // The committed scratch repository supplies the probe; it writes only
        // container-local /tmp files, so no checkout cleanup is necessary.
        Ok(())
    }

    fn launch(&self, request: &AgentLaunchRequest) -> Result<AgentLaunchPlan, String> {
        if request.model.is_some() {
            return Err("fleet-canary adapter does not use a model".into());
        }
        Ok(AgentLaunchPlan {
            command: "bash .flotilla/fleet-canary-agent.sh".into(),
            env: request.environment.clone(),
            stance: TRUSTED_IMPLICIT_STANCE.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery_api::EnvironmentAssertion;

    // The probe must be unavailable to ordinary daemons and preserve all
    // seam-selected launch values when explicitly enabled.
    #[test]
    fn canary_is_opt_in_and_preserves_the_selected_environment() {
        let runner = Arc::new(crate::providers::ProcessCommandRunner);
        let ordinary = AgentAdapterRegistry::discover(&EnvironmentBag::new(), runner.clone());
        assert!(ordinary.get("fleet-canary").is_none());
        let disabled = EnvironmentBag::new().with(EnvironmentAssertion::env_var("FLOTILLA_FLEET_CANARY", "0"));
        assert!(AgentAdapterRegistry::discover(&disabled, runner.clone()).get("fleet-canary").is_none());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("FLOTILLA_FLEET_CANARY", "1"));
        let registry = AgentAdapterRegistry::discover(&bag, runner);
        let adapter = registry.get("fleet-canary").expect("enabled canary");
        let mut request = AgentLaunchRequest {
            role: "probe".into(),
            model: None,
            brief: TerminalBrief { path: "unused".into(), content: String::new(), artifact_digest: None, copies: Vec::new() },
            environment: Vec::new(),
            fulfilment_grants: None,
        };
        // Adapter environment contains delivered material only; the terminal
        // pool adds the vessel baseline after the launch plan is prepared.
        request.environment = vec![("CLAUDE_CONFIG_DIR".into(), "/tmp/flotilla-config/canary/crews/probe".into())];
        let plan = adapter.launch(&request).expect("isolated launch");
        assert_eq!(plan.env, request.environment);
        assert_eq!(plan.command, "bash .flotilla/fleet-canary-agent.sh");
        request.model = Some("unused-model".into());
        assert!(adapter.launch(&request).is_err());
    }
}
