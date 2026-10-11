use std::path::Path;

use flotilla_provider_runtime::{gh_api_get, gh_api_get_with_headers, http_execute, run, run_output, ChannelLabel, TaskId};

// In-memory invocation boundary: return the label sent by each macro. The GhApi
// macros deliberately need no forge trait dependency in the runtime crate.
struct Invocation;

impl Invocation {
    async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, label: &ChannelLabel) -> ChannelLabel {
        label.clone()
    }

    async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, label: &ChannelLabel) -> ChannelLabel {
        label.clone()
    }

    async fn get(&self, _endpoint: &str, _cwd: &Path, label: &ChannelLabel) -> ChannelLabel {
        label.clone()
    }

    async fn get_with_headers(&self, _endpoint: &str, _cwd: &Path, label: &ChannelLabel) -> ChannelLabel {
        label.clone()
    }

    async fn execute(&self, _request: reqwest::Request, label: &ChannelLabel) -> ChannelLabel {
        label.clone()
    }
}

fn request() -> reqwest::Request {
    reqwest::Request::new(reqwest::Method::GET, "https://example.com/path".parse().unwrap())
}

// Glue: each exported macro has two arms, default labeling and explicit TaskId.
// Calling from an integration crate proves all expansion paths are public and
// resolve through the runtime crate rather than the consumer's module tree.
#[tokio::test]
async fn exported_macros_preserve_default_and_explicit_channel_labels() {
    let invocation = Invocation;
    let cwd = Path::new("/");
    assert_eq!(run!(invocation, "tool", &["list"], cwd), ChannelLabel::Command("tool list".into()));
    assert_eq!(run_output!(invocation, "tool", &["list"], cwd), ChannelLabel::Command("tool list".into()));
    assert_eq!(gh_api_get!(invocation, "repos/o/r", cwd), ChannelLabel::GhApi("repos/o/r".into()));
    assert_eq!(gh_api_get_with_headers!(invocation, "repos/o/r", cwd), ChannelLabel::GhApi("repos/o/r".into()));
    assert_eq!(http_execute!(invocation, request()), ChannelLabel::Http("example.com".into()));
    assert_eq!(run!(invocation, "tool", &["list"], cwd, TaskId("task")), ChannelLabel::Command("task".into()));
    assert_eq!(run_output!(invocation, "tool", &["list"], cwd, TaskId("task")), ChannelLabel::Command("task".into()));
    assert_eq!(gh_api_get!(invocation, "repos/o/r", cwd, TaskId("task")), ChannelLabel::GhApi("task".into()));
    assert_eq!(gh_api_get_with_headers!(invocation, "repos/o/r", cwd, TaskId("task")), ChannelLabel::GhApi("task".into()));
    assert_eq!(http_execute!(invocation, request(), TaskId("task")), ChannelLabel::Http("task".into()));
}
