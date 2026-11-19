//! Host collaborators required by crew operations.
use std::collections::BTreeMap;

use async_trait::async_trait;

/// Stages a Work's credentials into its environment before crew delivery.
#[async_trait]
pub trait WorkCredentialReconciler: Send + Sync {
    async fn reconcile(&self, namespace: &str, environment_ref: &str) -> Result<(), String>;

    async fn ledger_delivery_environment(&self, _namespace: &str, _environment_ref: &str) -> Result<BTreeMap<String, String>, String> {
        Err("credential delivery record is unavailable".to_string())
    }
}

/// The composition root's slot for [`flotilla_messaging::EngineMessages`]: set once, at startup,
/// after the engine's store opens.
pub type MessagePath = std::sync::Arc<std::sync::OnceLock<std::sync::Arc<dyn flotilla_messaging::EngineMessages>>>;

/// The Message path in `path`; a read before startup installs it is refused.
pub(crate) fn installed_messages(
    path: &MessagePath,
) -> Result<std::sync::Arc<dyn flotilla_messaging::EngineMessages>, flotilla_resources::ResourceError> {
    path.get().cloned().ok_or_else(|| flotilla_resources::ResourceError::other("the Message path is not installed"))
}

/// Fresh inspection requested by an explicit convoy deletion before its gate reads the evidence
/// (#3108): the checkout's home inspects it and records the inspection on its launch; this
/// returns once the request is answered.
#[async_trait]
pub trait CheckoutIntegrationInspector: Send + Sync {
    async fn inspect_now(&self, namespace: &str, checkout: &flotilla_resources::checkout_records::CheckoutRecord) -> Result<(), String>;
}
