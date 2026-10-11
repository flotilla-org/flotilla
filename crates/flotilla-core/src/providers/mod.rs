pub mod ai_utility;
pub mod change_request;
pub mod coding_agent;
pub mod discovery;
pub mod environment;
pub(crate) mod github_observation;
pub(crate) mod github_poll;
pub mod issue_tracker;
pub mod registry;
pub mod types;

#[cfg(test)]
pub(crate) mod github_test_support {
    use std::{path::PathBuf, sync::Arc};

    use crate::providers::forge::github::GhApi;
    use crate::testkits::replay;
    use flotilla_provider_runtime::CommandRunner;

    pub fn repo_root_for_recording() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("CARGO_MANIFEST_DIR should have a parent")
            .parent()
            .expect("CARGO_MANIFEST_DIR should have a grandparent")
            .to_path_buf()
    }

    pub fn build_api_and_runner(session: &replay::Session) -> (Arc<dyn GhApi>, Arc<dyn CommandRunner>) {
        let runner = replay::test_runner(session);
        let api = replay::test_gh_api(session);
        (api, runner)
    }
}

pub mod forge;
