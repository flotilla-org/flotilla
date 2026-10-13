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
