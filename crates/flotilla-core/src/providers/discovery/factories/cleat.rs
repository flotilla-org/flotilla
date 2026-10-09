//! Terminal pool factory for cleat.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        terminal::{cleat::CleatTerminalPool, TerminalPool},
        CommandRunner,
    },
};
use flotilla_paths::path_context::ExecutionEnvironmentPath;

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
            Ok(Arc::new(CleatTerminalPool::new(runner, binary.as_path().display().to_string(), env)))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("cleat".into())])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::CleatTerminalPoolFactory;
    use crate::config::ConfigStore;
    use crate::providers::discovery::EnvironmentAssertion;
    use crate::providers::discovery::EnvironmentBag;
    use crate::providers::discovery::Factory;
    use crate::providers::discovery::UnmetRequirement;
    use crate::testkits::discovery::DiscoveryMockRunner;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;

    // #2706: factory discovery supplies the execution host baseline, while
    // Cleat owns terminal identity and ambient harness values never transfer.
    #[tokio::test]
    async fn session_factory_uses_declared_host_baseline_and_leaves_vt_identity_to_cleat() {
        use crate::providers::CommandRunner;
        use crate::testkits::replay::testing::MockRunner;

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
