//! Change-request factory for Forgejo pull requests.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::ProviderConfigView;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::Factory;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_api::UnmetRequirement;
use flotilla_forge_forgejo::discovery::bounded_forgejo_provider_config;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::ReqwestHttpClient;
use flotilla_resources::ForgeKind;

use crate::forgejo::ForgejoChangeRequestProvider;
use crate::ChangeRequestTracker;

pub struct ForgejoChangeRequestFactory;

#[async_trait]
impl Factory for ForgejoChangeRequestFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn ChangeRequestTracker;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(
            ProviderCategory::ChangeRequest,
            "forgejo",
            "Forgejo Pull Requests",
            "PR",
            "Pull Requests",
            "pull request",
        )
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn ChangeRequestTracker>, Vec<UnmetRequirement>> {
        if !env.find_origin_forge().is_some_and(|forge| forge.kind == ForgeKind::Forgejo) {
            return Err(vec![UnmetRequirement::MissingRemoteHost("Forgejo origin".into())]);
        }
        let settings = config.load_config_for_probe().await.map_err(|error| vec![UnmetRequirement::MissingConfig(error)])?;
        let provider_config = bounded_forgejo_provider_config(env, config, &settings).await?;
        let slug = env.repo_slug().ok_or_else(|| vec![UnmetRequirement::MissingRemoteHost("origin".into())])?;
        let mut provider = ForgejoChangeRequestProvider::new(Arc::new(ReqwestHttpClient::new()), runner, provider_config, slug);
        if let Some(login) = settings.change_request.operator_login {
            provider = provider.with_operator_login(login);
        }
        Ok(Arc::new(provider))
    }
}
