use crate::*;

#[allow(async_fn_in_trait)]
pub trait CrewImageBaselineStoreExt: Resource + Sized {
    async fn resolve(name: &str, baselines: &crate::DefinitionResolver<Self>) -> Result<CrewImageBaselineSpec, String>;
}
impl CrewImageBaselineStoreExt for CrewImageBaseline {
    async fn resolve(name: &str, baselines: &crate::DefinitionResolver<Self>) -> Result<CrewImageBaselineSpec, String> {
        let baseline = baselines.get(name).await.map_err(|error| format!("image-baseline `{name}` missing/unresolved: {error}"))?;
        let unresolved = if baseline.metadata.deletion_timestamp.is_some() {
            Some("baseline is deleted")
        } else if baseline.metadata.merge.as_ref().is_some_and(|merge| !merge.conflicts.is_empty()) {
            Some("baseline has unresolved merge conflicts")
        } else if baseline.spec.image.trim().is_empty() {
            Some("baseline image is empty")
        } else {
            None
        };
        if let Some(reason) = unresolved {
            return Err(format!("image-baseline `{name}` missing/unresolved: {reason}"));
        }
        Ok(baseline.spec)
    }
}
