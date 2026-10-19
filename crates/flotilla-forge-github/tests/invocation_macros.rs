use std::path::Path;

use flotilla_forge_github::{gh_api_get, gh_api_get_with_headers};
use flotilla_provider_runtime::{ChannelLabel, TaskId};

// In-memory gh invocation boundary: return each macro's label.
struct Invocation;
impl Invocation {
    async fn get(&self, _endpoint: &str, _cwd: &Path, label: &ChannelLabel) -> ChannelLabel {
        label.clone()
    }

    async fn get_with_headers(&self, _endpoint: &str, _cwd: &Path, label: &ChannelLabel) -> ChannelLabel {
        label.clone()
    }
}

// #3039: both public macro arms preserve default and explicit labels after extraction.
// Glue: exercise every arm from a consumer crate to verify public expansion paths.
#[tokio::test]
async fn exported_macros_preserve_default_and_explicit_channel_labels() {
    let invocation = Invocation;
    let cwd = Path::new("/");
    assert_eq!(gh_api_get!(invocation, "repos/o/r", cwd), flotilla_provider_runtime::gh_api_label("repos/o/r".into()));
    assert_eq!(gh_api_get_with_headers!(invocation, "repos/o/r", cwd), flotilla_provider_runtime::gh_api_label("repos/o/r".into()));
    assert_eq!(gh_api_get!(invocation, "repos/o/r", cwd, TaskId("task")), flotilla_provider_runtime::gh_api_label("task".into()));
    assert_eq!(
        gh_api_get_with_headers!(invocation, "repos/o/r", cwd, TaskId("task")),
        flotilla_provider_runtime::gh_api_label("task".into())
    );
}
