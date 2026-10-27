use crate::CrewImageBaselineStoreExt;
use crate::*;

#[allow(async_fn_in_trait)]
pub trait DockerImageSourceStoreExt: Sized {
    async fn resolve(&self, baselines: &DefinitionResolver<CrewImageBaseline>) -> Result<String, String>;
}
impl DockerImageSourceStoreExt for DockerImageSource {
    async fn resolve(&self, baselines: &DefinitionResolver<CrewImageBaseline>) -> Result<String, String> {
        match self {
            // A composed image has no reference until its ImageBuild is Built;
            // the environment reconciler reads the build's digest.
            Self::Built { image_build_ref, .. } => Err(format!("composed image awaits image build `{image_build_ref}`")),
            Self::Literal(image) if !image.trim().is_empty() => Ok(image.clone()),
            Self::Literal(_) => Err("placement image is empty".to_string()),
            Self::Baseline { image_baseline_ref: name } => Ok(CrewImageBaseline::resolve(name, baselines).await?.image),
        }
    }
}

/// The settings a policy's host realises its vessels with (#3824 layer 4, ADR 0058): the host's
/// provider record of the policy's realisation, `<host>.<realisation>`, local or replicated; else,
/// for a host with no record yet, the settings the policy itself carries. The policy as a source
/// goes with PlacementPolicy (#3824 layer 4b-ii).
pub async fn provider_settings(
    backend: &ResourceBackend,
    namespace: &str,
    policy: &PlacementPolicySpec,
) -> Result<flotilla_resources::environment_provider::EnvironmentProviderSpec, String> {
    use flotilla_resources::environment_provider::{EnvironmentProvider, EnvironmentProviderSpec};
    let carried = EnvironmentProviderSpec::from_policy(policy)?;
    match backend.including_replicas::<EnvironmentProvider>(namespace).get(&carried.record_name()).await {
        Ok(record) if record.object.spec.settings.realisation() == carried.settings.realisation() => Ok(record.object.spec),
        Ok(record) => Err(format!("provider settings `{}` are for another realisation", record.object.metadata.name)),
        Err(ResourceError::NotFound { .. }) => Ok(carried),
        Err(error) => Err(error.to_string()),
    }
}
