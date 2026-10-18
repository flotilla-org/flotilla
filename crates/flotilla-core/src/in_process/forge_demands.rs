use super::InProcessDaemon;
use flotilla_observation::forge_demands::ForgeDemandService;
impl InProcessDaemon {
    fn forge_demand_service(&self) -> ForgeDemandService {
        ForgeDemandService {
            backend: self.resource_backend(),
            change_requests: self.change_request_query_port.clone(),
            issues: self.issue_query_port.clone(),
        }
    }
    pub async fn refresh_forge_read_demands(&self) -> Result<(), String> {
        let Ok(_guard) = self.forge_demand_refresh.try_lock() else { return Ok(()) };
        self.forge_demand_service().refresh_forge_read_demands(&self.provisioning_namespace_for_forge().await).await
    }
    pub async fn service_forge_read_demand(&self, namespace: &str, name: &str) -> Result<(), String> {
        self.forge_demand_service().service_forge_read_demand(namespace, name).await
    }
}
