//! Terminal pool factory for cleat.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::ProviderConfigView;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::Factory;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_api::UnmetRequirement;

use crate::cleat::{CleatTerminalPool, DEFAULT_CONTROLLER_IDLE};
use crate::TerminalPool;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::CommandRunner;

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
        config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>> {
        if let Some(binary) = env.find_binary("cleat") {
            let config = config.load_config_for_probe().await.map_err(|error| vec![UnmetRequirement::MissingConfig(error)])?;
            let window = config.terminal_pool.controller_idle_ms.map(Duration::from_millis).unwrap_or(DEFAULT_CONTROLLER_IDLE);
            // Cleat's VT engine chooses the child's terminal identity at launch.
            Ok(Arc::new(CleatTerminalPool::new(runner, binary.as_path().display().to_string(), env).with_controller_idle(window)))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("cleat".into())])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::CleatTerminalPoolFactory;
    use crate::test_support::TestConfig as ConfigStore;
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_discovery_api::Factory;
    use flotilla_discovery_api::UnmetRequirement;
    use flotilla_discovery_testkit::DiscoveryMockRunner;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;

    // #2706: factory discovery supplies the execution host baseline, while
    // Cleat owns terminal identity and ambient harness values never transfer.
    #[tokio::test]
    async fn session_factory_uses_declared_host_baseline_and_leaves_vt_identity_to_cleat() {
        use flotilla_provider_runtime::CommandRunner;
        use flotilla_replay_testkit::testing::MockRunner;

        for outer_identity in [None, Some(("screen-256color", "truecolor"))] {
            let mut bag = EnvironmentBag::new()
                .with(EnvironmentAssertion::binary("cleat", "/usr/local/bin/cleat"))
                .with(EnvironmentAssertion::env_var("HOME", "/execution/home"))
                .with(EnvironmentAssertion::env_var("PATH", "/execution/bin:/usr/bin:/bin"))
                .with(EnvironmentAssertion::env_var("NO_COLOR", "1"))
                .with(EnvironmentAssertion::env_var("CLAUDE_CODE_MESSAGING_TOKEN", "fake-unrelated-token"));
            if let Some((term, colorterm)) = outer_identity {
                bag = bag
                    .with(EnvironmentAssertion::env_var("TERM", term))
                    .with(EnvironmentAssertion::env_var("TERM_PROGRAM", "ghostty"))
                    .with(EnvironmentAssertion::env_var("TERM_PROGRAM_VERSION", "1.2.3"))
                    .with(EnvironmentAssertion::env_var("COLORTERM", colorterm));
            }
            let dir = tempfile::tempdir().expect("tempdir");
            let config = ConfigStore::with_base(dir.path());
            let runner = Arc::new(MockRunner::new(vec![Ok("[]".into()), Ok("--env-clear --env".into()), Ok("{}".into())]));
            let pool = CleatTerminalPoolFactory
                .probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner.clone() as Arc<dyn CommandRunner>)
                .await
                .expect("cleat pool");
            pool.ensure_session("session", "codex", &ExecutionEnvironmentPath::new("/repo"), &vec![], &[])
                .await
                .expect("launch with declared environment");
            let calls = runner.calls();
            assert_eq!(calls.len(), 3);
            for (cmd, args) in &calls {
                assert_eq!(cmd, "/usr/bin/env");
                assert_eq!(&args[..4], &["-i", "HOME=/execution/home", "PATH=/execution/bin:/usr/bin:/bin", "/usr/local/bin/cleat"]);
                assert!(!args
                    .iter()
                    .any(|arg| arg.starts_with("NO_COLOR=") || arg.starts_with("CLAUDE_CODE_") || arg.starts_with("TERM=")));
            }
            assert!(calls[2].1.iter().any(|arg| arg == "--env-clear"));
            let env = calls[2].1.windows(2).filter(|args| args[0] == "--env").map(|args| args[1].as_str()).collect::<Vec<_>>();
            assert_eq!(env, vec!["HOME=/execution/home", "PATH=/execution/bin:/usr/bin:/bin"]);
        }
    }

    // Glue: discovery forwards the configured atomic guard window to cleat.
    #[tokio::test]
    async fn session_factory_configures_controller_idle_window() {
        use flotilla_replay_testkit::testing::MockRunner;
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("cleat", "/tools/cleat"));
        let mut config = ConfigStore::with_base("/config");
        config.1.terminal_pool.controller_idle_ms = Some(2500);
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let pool = CleatTerminalPoolFactory
            .probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner.clone())
            .await
            .expect("configured cleat pool");
        pool.deliver("agent", "turn").await.expect("atomic turn submission");
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.windows(2).any(|args| args == ["--controller-idle", "2500ms"]));
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
