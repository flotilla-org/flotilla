use flotilla_resources::*;

use crate::TypedResolver;

pub async fn ensure_repository(
    repositories: &TypedResolver<Repository>,
    key: &RepositoryKey,
    spec: &RepositorySpec,
) -> Result<ResourceObject<Repository>, ResourceError> {
    let repository = match repositories.create(&InputMeta::builder().name(key.to_string()).build(), spec).await {
        Ok(created) => created,
        Err(ResourceError::Conflict { .. }) => repositories.get(&key.to_string()).await?,
        Err(error) => return Err(error),
    };
    repository.spec.verify_key(key).map_err(ResourceError::invalid)?;
    if repository.spec != *spec {
        if repository.spec.identity() != spec.identity() {
            return Err(ResourceError::invalid(format!("repository key {key} already refers to a different canonical identity")));
        }
        // Identity-only observations are common during provisioning and must not
        // erase provenance supplied by the per-repository config authority.
        if spec.upstream().is_none() && !spec.allows_reviewless_workflows() && spec.verification_commands().is_empty() {
            return Ok(repository);
        }
        return repositories.update(&InputMeta::from(&repository.metadata), &repository.metadata.resource_version, spec).await;
    }
    Ok(repository)
}
