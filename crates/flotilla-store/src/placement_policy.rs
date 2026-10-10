use crate::CrewImageBaselineStoreExt;
use crate::*;

#[allow(async_fn_in_trait)]
pub trait DockerImageSourceStoreExt: Sized {
    async fn resolve(&self, baselines: &DefinitionResolver<CrewImageBaseline>) -> Result<String, String>;
}
impl DockerImageSourceStoreExt for DockerImageSource {
    async fn resolve(&self, baselines: &DefinitionResolver<CrewImageBaseline>) -> Result<String, String> {
        match self {
            Self::Composition { composition } => {
                if let Some(identity) = &composition.identity {
                    identity.validate()?;
                    return Ok(identity.registry_digest.as_ref().unwrap_or(&identity.local_image_id).clone());
                }
                composition
                    .baseline_image
                    .clone()
                    .filter(|image| !image.trim().is_empty())
                    .ok_or_else(|| "image composition awaits placement-time build identity".to_string())
            }
            Self::Literal(image) if !image.trim().is_empty() => Ok(image.clone()),
            Self::Literal(_) => Err("placement image is empty".to_string()),
            Self::Baseline { image_baseline_ref: name } => Ok(CrewImageBaseline::resolve(name, baselines).await?.image),
        }
    }
}
