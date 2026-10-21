// Keep scoped tracing capture in its own test process: parallel tests exercise
// the same callsites without subscribers and race their cached interest.
use async_trait::async_trait;
use flotilla_forge_github::forge_budget::{BudgetedRunner, ForgeBudgets};
use flotilla_provider_runtime::{ChannelLabel, CommandRunner};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("logs").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
// Process boundary fake: return headers and costs without contacting GitHub.
struct TraceRunner;
#[async_trait]
impl CommandRunner for TraceRunner {
    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
    ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
        self.run(cmd, args, cwd, label).await.map(|stdout| flotilla_provider_runtime::CommandOutput {
            stdout,
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        true
    }
    async fn run(&self, _: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
        if args.contains(&"fail_spawn") {
            return Err("spawn failed".into());
        }
        Ok(if args.contains(&"graphql") {
            "HTTP/2 200 OK\r\n\r\n{\"data\":{\"rateLimit\":{\"cost\":3}}}"
        } else {
            "HTTP/2 304 Not Modified\r\n\r\n"
        }
        .into())
    }
}
// #3710: each subprocess attempt emits exactly one structured event and
// captures its named caller, with an explicit fallback outside a span.
#[test]
fn every_call_traces_caller_and_unspanned_calls_are_unattributed() {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = LogWriter(logs.clone());
    let subscriber = tracing_subscriber::fmt().without_time().with_ansi(false).with_writer(move || writer.clone()).finish();
    tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let runner = BudgetedRunner { inner: Arc::new(TraceRunner), budgets: ForgeBudgets::default() };
            {
                let span = tracing::info_span!("test_forge_caller");
                let _entered = span.enter();
                runner.run("gh", &["api", "graphql", "secret-token"], Path::new("/"), &ChannelLabel::Default).await.expect("call");
            }
            runner.run("gh", &["api", "repos/org/repo"], Path::new("/"), &ChannelLabel::Default).await.expect("call");
            runner.run("gh", &["api", "fail_spawn"], Path::new("/"), &ChannelLabel::Default).await.expect_err("transport failure");
        });
    });
    let logs = String::from_utf8(logs.lock().expect("logs").clone()).expect("UTF-8");
    assert_eq!(logs.matches("forge call").count(), 3, "{logs}");
    for field in [
        "caller=\"test_forge_caller\"",
        "caller=\"unattributed\"",
        "identity=\"host gh login\"",
        "budget=\"GraphQL\"",
        "charged_cost=3",
        "charged_cost=0",
        "not_modified=true",
        "cost_basis=\"attempt_estimate\"",
    ] {
        assert!(logs.contains(field), "missing {field}: {logs}");
    }
    assert!(!logs.contains("secret-token"));
}
