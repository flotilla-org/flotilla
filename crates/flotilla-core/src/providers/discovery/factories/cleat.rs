//! Terminal pool factory for cleat.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    path_context::ExecutionEnvironmentPath,
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        terminal::{cleat::CleatTerminalPool, TerminalPool},
        CommandRunner,
    },
};

pub struct CleatTerminalPoolFactory;

#[async_trait]
impl Factory for CleatTerminalPoolFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn TerminalPool;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "cleat")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>> {
        if let Some(binary) = env.find_binary("cleat") {
            // Cleat's VT engine chooses the child's terminal identity at launch.
            Ok(Arc::new(CleatTerminalPool::new(runner, binary.as_path().display().to_string())))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("cleat".into())])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::CleatTerminalPoolFactory;
    use crate::{
        config::ConfigStore,
        path_context::ExecutionEnvironmentPath,
        providers::discovery::{test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement},
    };

    #[tokio::test]
    async fn session_factory_leaves_terminal_identity_to_cleat() {
        for outer_identity in [None, Some(("screen-256color", "truecolor"))] {
            let mut bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("cleat", "/usr/local/bin/cleat"));
            if let Some((term, colorterm)) = outer_identity {
                bag = bag
                    .with(EnvironmentAssertion::env_var("TERM", term))
                    .with(EnvironmentAssertion::env_var("TERM_PROGRAM", "ghostty"))
                    .with(EnvironmentAssertion::env_var("TERM_PROGRAM_VERSION", "1.2.3"))
                    .with(EnvironmentAssertion::env_var("COLORTERM", colorterm));
            }
            let dir = tempfile::tempdir().expect("tempdir");
            let config = ConfigStore::with_base(dir.path());
            let runner = Arc::new(
                DiscoveryMockRunner::builder()
                    .on_run("/usr/local/bin/cleat", &["list", "--json"], Ok("[]".into()))
                    .on_run(
                        "/usr/local/bin/cleat",
                        &["launch", "--json", "--record", "session", "--cwd", "/repo", "--cmd", "codex"],
                        Ok("{}".into()),
                    )
                    .build(),
            );
            let pool =
                CleatTerminalPoolFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await.expect("cleat pool");
            pool.ensure_session("session", "codex", &ExecutionEnvironmentPath::new("/repo"), &vec![], &[])
                .await
                .expect("launch without discovery terminal identity");
        }
    }

    #[tokio::test]
    async fn session_factory_succeeds_with_binary() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("cleat", "/usr/local/bin/cleat"));
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CleatTerminalPoolFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn session_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CleatTerminalPoolFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("missing binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("cleat".into())));
    }
}
