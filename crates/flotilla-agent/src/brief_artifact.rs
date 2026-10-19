//! Brief artifact persistence port.
use async_trait::async_trait;
#[async_trait]
pub trait BriefArtifactWriter: Send + Sync {
    async fn put_brief(
        &self,
        namespace: &str,
        convoy: &str,
        role: &str,
        subject: &str,
        content: &[u8],
        charter_commit: Option<&str>,
    ) -> Result<String, String>;
}
