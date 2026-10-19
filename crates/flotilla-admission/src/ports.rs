//! Host orchestration collaborators required by admission.
//! Environment facts arrive through discovery's `Environments` port.
use async_trait::async_trait;
use flotilla_protocol::PrincipalRef;

#[async_trait]
/// Crew-service teardown required by standing-convoy admission.
pub trait ConvoyTeardown: Send + Sync {
    async fn abandon(&self, namespace: &str, name: &str, reason: &str, principal: Option<&PrincipalRef>) -> Result<(), String>;
    async fn reap(&self, namespace: &str, name: &str, force: bool) -> Result<(), String>;
}
