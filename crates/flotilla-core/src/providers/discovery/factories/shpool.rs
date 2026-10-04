//! Terminal pool factory for shpool.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    path_context::ExecutionEnvironmentPath,
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        terminal::{shpool::ShpoolTerminalPool, TerminalPool},
        CommandRunner,
    },
};

pub struct ShpoolTerminalPoolFactory;

#[async_trait]
impl Factory for ShpoolTerminalPoolFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn TerminalPool;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "shpool")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>> {
        if env.find_binary("shpool").is_some() {
            let socket_path = config.state_dir().join("shpool/shpool.socket");
            let terminal_env_defaults = super::terminal_env_defaults_from_bag(env);
            let pool =
                ShpoolTerminalPool::create(runner, socket_path, terminal_env_defaults, env.find_env_var("SHELL").map(str::to_owned)).await;
            Ok(Arc::new(pool))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("shpool".into())])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::ShpoolTerminalPoolFactory;
    use crate::{
        config::ConfigStore,
        path_context::ExecutionEnvironmentPath,
        providers::discovery::{test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement},
    };

    #[tokio::test]
    async fn shpool_factory_succeeds_with_binary() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("shpool", "/usr/local/bin/shpool"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ShpoolTerminalPoolFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn shpool_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ShpoolTerminalPoolFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without shpool binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("shpool".into())));
    }

    #[tokio::test]
    async fn shpool_factory_descriptor() {
        let desc = ShpoolTerminalPoolFactory.descriptor();
        assert_eq!(desc.backend, "shpool");
        assert_eq!(desc.implementation, "shpool");
        assert_eq!(desc.display_name, "shpool");
        assert_eq!(desc.abbreviation, "");
        assert_eq!(desc.section_label, "");
        assert_eq!(desc.item_noun, "");
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;
    use crate::providers::{
        discovery::{
            detectors::default_host_detectors,
            run_host_detectors,
            test_support::{DiscoveryMockRunner, TestEnvVars},
            EnvironmentAssertion,
        },
        testing::MockRunner,
    };

    // Glue: injected host environment → detector bag → factory → shpool --cmd.
    // Both a supplied shell (with spaces) and the absent-SHELL fallback must be
    // independent of the daemon process environment.
    #[tokio::test]
    async fn shell_comes_from_discovered_environment() {
        for shell in [Some("/remote/my shell"), None] {
            let vars = TestEnvVars::new(shell.map(|value| ("SHELL", value)).into_iter().chain([("TERM", "xterm-256color")]));
            let discovery_runner = DiscoveryMockRunner::builder().build();
            let bag = run_host_detectors(&default_host_detectors(), &discovery_runner, &vars)
                .await
                .with(EnvironmentAssertion::binary("shpool", "/bin/shpool"));
            // Fake the shpool subprocess boundary, recording the actual argv.
            let runner = Arc::new(MockRunner::new(vec![Ok(r#"{"sessions": []}"#.into()), Ok(String::new()), Ok(String::new())]));
            let dir = tempfile::tempdir().expect("config directory");
            let config = ConfigStore::with_base(dir.path());
            let pool = ShpoolTerminalPoolFactory
                .probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner.clone())
                .await
                .expect("pool");
            pool.ensure_session("session", "", &ExecutionEnvironmentPath::new("/repo"), &vec![], &[]).await.expect("session");
            let calls = runner.calls();
            let args = &calls[1].1;
            let index = args.iter().position(|arg| arg == "--cmd").expect("command argument");
            assert_eq!(
                args[index + 1],
                format!("env TERM='xterm-256color' {}", flotilla_protocol::arg::shell_quote(shell.unwrap_or("/bin/sh")))
            );
        }
    }
}
