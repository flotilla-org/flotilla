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
