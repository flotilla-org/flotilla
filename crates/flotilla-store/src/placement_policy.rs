use crate::*;

#[allow(async_fn_in_trait)]
pub trait DockerImageSourceStoreExt: Sized {
    /// A literal now; a composed image's once its ImageBuild is built.
    fn resolve(&self) -> Result<String, String>;
}
impl DockerImageSourceStoreExt for DockerImageSource {
    fn resolve(&self) -> Result<String, String> {
        match self {
            Self::Built { image_build_ref, .. } => Err(format!("composed image awaits image build `{image_build_ref}`")),
            Self::Literal(image) if !image.trim().is_empty() => Ok(image.clone()),
            Self::Literal(_) => Err("placement image is empty".to_string()),
        }
    }
}
