//! Drydock's provider API behind flotilla's [`ProviderApi`]. Values cross by
//! their wire form: flotilla's types serialize as drydock's do, so a drydock
//! release that keeps the API keeps this adapter, and one that changes it
//! fails here rather than in the provider.
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_environment::environment::drydock::api::{self as flotilla, ApiError, ApiResult, ProtocolErrorKind};
use futures::StreamExt;
use serde::{de::DeserializeOwned, Serialize};

/// A drydock provider API: the embedded daemon, or a client of one.
pub struct DrydockApi(pub Arc<dyn drydock::api::ProviderApi>);

/// `api` as the provider API the drydock environment provider is written against.
pub fn provider_api(api: Arc<dyn drydock::api::ProviderApi>) -> Arc<dyn flotilla::ProviderApi> {
    Arc::new(DrydockApi(api))
}

/// `value` in the other side's types; a value the other side refuses is `invalid`.
fn translate<T: DeserializeOwned>(value: &impl Serialize, invalid: &str) -> ApiResult<T> {
    serde_json::to_value(value).and_then(serde_json::from_value).map_err(|error| ApiError::Other(format!("{invalid}: {error}")))
}
fn to_drydock<T: DeserializeOwned>(value: &impl Serialize) -> ApiResult<T> {
    translate(value, "invalid request")
}
fn convert<T: DeserializeOwned>(value: &impl Serialize) -> ApiResult<T> {
    translate(value, "invalid host response")
}

/// Drydock's error, as flotilla tells its kinds apart.
pub fn api_error(error: drydock::client::Error) -> ApiError {
    use drydock::client::Error;
    use drydock_protocol::ProtocolError;
    match error {
        Error::Protocol(error) => {
            let (kind, reason) = match &error {
                ProtocolError::NotFound { reason } => (ProtocolErrorKind::NotFound, reason.clone()),
                ProtocolError::Refused { reason } => (ProtocolErrorKind::Refused, reason.clone()),
                // Deliberately coarse: the adapter acts only on these two kinds. Any
                // other error is reported by its description, never matched on.
                _ => (ProtocolErrorKind::Other, String::new()),
            };
            ApiError::Protocol { kind, reason, description: error.to_string() }
        }
        Error::NotReady(_) => ApiError::NotReady,
        Error::ReadinessTimeout(_) => ApiError::ReadinessTimeout,
        other => ApiError::Other(other.to_string()),
    }
}

#[async_trait]
impl flotilla::ProviderApi for DrydockApi {
    async fn hello(&self) -> ApiResult<flotilla::Hello> {
        let hello = self.0.hello().await.map_err(api_error)?;
        Ok(flotilla::Hello {
            compatible: hello.is_compatible(),
            host_version: hello.host_version.clone(),
            loadable_modules: convert(&hello.loadable_modules)?,
        })
    }
    async fn provider(&self, provider: &str) -> ApiResult<flotilla::ProviderDeclaration> {
        convert(&self.0.provider(provider).await.map_err(api_error)?)
    }
    async fn prepare(&self, provider: &str, request: &flotilla::PrepareRequest) -> ApiResult<flotilla::Prepared> {
        convert(&self.0.prepare(provider, &to_drydock(request)?).await.map_err(api_error)?)
    }
    async fn provision(&self, provider: &str, request: &flotilla::ProvisionRequest) -> ApiResult<flotilla::Provisioned> {
        Ok(match self.0.provision(provider, &to_drydock(request)?).await.map_err(api_error)? {
            drydock::api::Provisioned::Created(environment) => flotilla::Provisioned::Created(convert(&environment)?),
            drydock::api::Provisioned::Existing(environment) => flotilla::Provisioned::Existing(convert(&environment)?),
        })
    }
    async fn list(&self, provider: &str) -> ApiResult<Vec<flotilla::Environment>> {
        convert(&self.0.list(provider).await.map_err(api_error)?)
    }
    async fn inspect(&self, provider: &str, id: &str) -> ApiResult<flotilla::Environment> {
        convert(&self.0.inspect(provider, id).await.map_err(api_error)?)
    }
    async fn destroy(&self, provider: &str, id: &str) -> ApiResult<()> {
        self.0.destroy(provider, id).await.map_err(api_error)
    }
    async fn observe(&self, since: Option<&str>) -> ApiResult<flotilla::ObservationStream> {
        let records = self.0.observe(since).await.map_err(api_error)?;
        Ok(records.map(|record| record.map_err(api_error).and_then(|record| convert(&record))).boxed())
    }
    async fn put_inward(&self, provider: &str, id: &str, name: &str, request: &flotilla::InwardRequest) -> ApiResult<flotilla::Inward> {
        convert(&self.0.put_inward(provider, id, name, &to_drydock(request)?).await.map_err(api_error)?.into_inner())
    }
    async fn inward(&self, provider: &str, id: &str, name: &str) -> ApiResult<flotilla::Inward> {
        convert(&self.0.inward(provider, id, name).await.map_err(api_error)?)
    }
    async fn put_outward(&self, provider: &str, id: &str, name: &str, request: &flotilla::OutwardRequest) -> ApiResult<flotilla::Outward> {
        convert(&self.0.put_outward(provider, id, name, &to_drydock(request)?).await.map_err(api_error)?.into_inner())
    }
    async fn outward(&self, provider: &str, id: &str, name: &str) -> ApiResult<flotilla::Outward> {
        convert(&self.0.outward(provider, id, name).await.map_err(api_error)?)
    }
}
