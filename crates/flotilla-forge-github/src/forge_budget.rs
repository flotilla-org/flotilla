//! Hourly host-identity accounting at the gh subprocess boundary.
//! `host gh login` identifies this host's configured credential slot, not a
//! resolved GitHub username. Account changes within an hour share that slot.
//! CLI subcommands can use either API; only explicit `gh api` calls identify a
//! REST/GraphQL budget. Other gh reads/writes are unclassified CLI attempts.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::ForgeBudgetRow;
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandOutput;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::{CommandLines, CommandProcess};

use crate::github::github_rate_limit;
use crate::github::parse_gh_api_response;
use crate::github::response_header;

#[derive(Clone, Default)]
pub struct ForgeBudgets {
    state: Arc<Mutex<BudgetState>>,
}
#[derive(Default)]
struct BudgetState {
    rows: BTreeMap<String, ForgeBudgetRow>,
    totals: BTreeMap<String, ForgeBudgetRow>,
}
impl ForgeBudgets {
    fn key(cmd: &str, args: &[&str]) -> Option<(&'static str, &'static str)> {
        if cmd != "gh" {
            return None;
        }
        match args.first().copied()? {
            "api" => Some(("host gh login", if args.contains(&"graphql") { "GraphQL" } else { "REST" })),
            "pr" | "issue" => Some(("host gh login", "CLI")),
            _ => None,
        }
    }
    #[cfg(test)]
    fn after(&self, cmd: &str, args: &[&str], raw: &str) {
        let Some((identity, budget)) = Self::key(cmd, args) else { return };
        self.after_for(identity, budget, raw);
    }
    fn after_for(&self, identity: &str, budget: &str, raw: &str) {
        let now = Utc::now();
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let BudgetState { rows, totals } = &mut *state;
        let key = format!("{identity}/{budget}");
        let row = rows.entry(key.clone()).or_insert_with(|| ForgeBudgetRow {
            host: String::new(),
            identity: identity.into(),
            budget: budget.into(),
            window_start: now,
            calls: 0,
            reported_cost: 0,
            unreported_calls: 0,
            remaining: None,
            retry_at: None,
        });
        if now.signed_duration_since(row.window_start).num_seconds() >= 3600 {
            row.window_start = now;
            row.calls = 0;
            row.reported_cost = 0;
            row.unreported_calls = 0;
        }
        row.calls += 1;
        let response = parse_gh_api_response(raw);
        let document: serde_json::Value = serde_json::from_str(if response.status == 0 { raw } else { &response.body }).unwrap_or_default();
        let cost = if budget == "REST" {
            // Conservative attempt accounting: a non-HTTP failure (including
            // spawn/transport errors) may have reached GitHub, so count one.
            Some(if response.status == 304 { 0 } else { 1 })
        } else if budget == "GraphQL" {
            document["data"]["rateLimit"]["cost"].as_u64()
        } else {
            None
        };
        if let Some(cost) = cost {
            row.reported_cost += cost;
        } else {
            row.unreported_calls += 1;
        }
        row.remaining = response_header(raw, "x-ratelimit-remaining")
            .and_then(|value| value.parse().ok())
            .or_else(|| document["data"]["rateLimit"]["remaining"].as_u64())
            .or(row.remaining);
        let totals =
            totals.entry(key.clone()).or_insert_with(|| ForgeBudgetRow { calls: 0, reported_cost: 0, unreported_calls: 0, ..row.clone() });
        totals.calls += 1;
        totals.reported_cost += cost.unwrap_or(0);
        totals.unreported_calls += u64::from(cost.is_none());
        // The latest response's own retry time: set only when GitHub limited the call and said
        // when to retry, and cleared by any response that was not limited.
        row.retry_at = github_rate_limit(raw, now).and_then(|limit| limit.retry_at);
    }
    /// Process lifetime counters; unlike fleet window rows these never reset hourly.
    pub fn counter_rows(&self, host: &str) -> Vec<ForgeBudgetRow> {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .totals
            .values()
            .cloned()
            .map(|mut row| {
                row.host = host.into();
                row
            })
            .collect()
    }
    pub fn rows(&self, host: &str) -> Vec<ForgeBudgetRow> {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .rows
            .values()
            .cloned()
            .map(|mut row| {
                row.host = host.into();
                row
            })
            .collect()
    }
}
/// An interrupted subprocess may already have reached GitHub. Record the
/// attempt without inventing a received response or GraphQL cost.
#[derive(bon::Builder)]
struct BudgetAttempt<'a> {
    budgets: &'a ForgeBudgets,
    key: Option<(&'static str, &'static str)>,
    caller: &'static str,
    command_kind: String,
    trace_pending: bool,
}
impl BudgetAttempt<'_> {
    fn new<'a>(budgets: &'a ForgeBudgets, cmd: &str, args: &[&str]) -> BudgetAttempt<'a> {
        let caller = tracing::Span::current().metadata().map_or("unattributed", |metadata| metadata.name());
        // Only static command categories are logged; arguments may contain secrets.
        let command_kind = match args.first().copied() {
            Some("api") if args.contains(&"graphql") => "api graphql",
            Some("api") => "api REST",
            Some("pr") => "pr",
            Some("issue") => "issue",
            _ => "other",
        }
        .to_string();
        BudgetAttempt::builder()
            .budgets(budgets)
            .maybe_key(ForgeBudgets::key(cmd, args))
            .caller(caller)
            .command_kind(command_kind)
            .trace_pending(cmd == "gh")
            .build()
    }
    fn emit(&self, identity: &str, budget: &str, raw: &str, outcome: &str) {
        let response = parse_gh_api_response(raw);
        // REST/CLI bodies may be large; only GraphQL exposes a cost in JSON.
        let graphql_cost = (budget == "GraphQL")
            .then(|| {
                serde_json::from_str::<serde_json::Value>(if response.status == 0 { raw } else { &response.body })
                    .ok()
                    .and_then(|document| document["data"]["rateLimit"]["cost"].as_u64())
            })
            .flatten();
        let charged_cost = match budget {
            "REST" => Some(u64::from(response.status != 304)),
            "GraphQL" => graphql_cost,
            _ => None,
        };
        let cost_basis = match (budget, response.status, charged_cost) {
            ("REST", 0, _) => "attempt_estimate",
            ("REST", 304, _) => "not_modified",
            ("REST", _, _) => "http_response_estimate",
            ("GraphQL", _, Some(_)) => "reported",
            _ => "unreported",
        };
        tracing::info!(target: "flotilla::forge_call", caller = self.caller, identity, budget,
            command_kind = %self.command_kind, status = %if response.status == 0 { outcome.to_string() } else { response.status.to_string() },
            charged_cost = %charged_cost.map(|cost| cost.to_string()).unwrap_or_else(|| "unknown".into()), cost_basis, not_modified = response.status == 304, "forge call");
    }
    fn finish(mut self, raw: &str, outcome: &str) {
        let key = self.key.take();
        if let Some((identity, budget)) = key {
            self.budgets.after_for(identity, budget, raw);
        }
        if self.trace_pending {
            let (identity, budget) = key.unwrap_or(("host gh login", "CLI"));
            self.emit(identity, budget, raw, outcome);
            self.trace_pending = false;
        }
    }
}
impl Drop for BudgetAttempt<'_> {
    fn drop(&mut self) {
        if let Some((identity, budget)) = self.key {
            self.budgets.after_for(identity, budget, "cancelled before response");
        }
        if self.trace_pending {
            let (identity, budget) = self.key.unwrap_or(("host gh login", "CLI"));
            self.emit(identity, budget, "", "cancelled");
        }
    }
}

pub struct BudgetedRunner {
    pub inner: Arc<dyn CommandRunner>,
    pub budgets: ForgeBudgets,
}
#[async_trait]
impl CommandRunner for BudgetedRunner {
    fn scratch_namespace(&self) -> &str {
        self.inner.scratch_namespace()
    }

    async fn same_filesystem(&self, source: &Path, destination: &Path) -> Result<bool, String> {
        self.inner.same_filesystem(source, destination).await
    }

    async fn writable_scratch_base(&self, preferred: Option<&Path>, fallback: &Path) -> Result<PathBuf, String> {
        self.inner.writable_scratch_base(preferred, fallback).await
    }
    async fn writable_config_base(&self, preferred: Option<&Path>, fallback: &Path) -> Result<PathBuf, String> {
        self.inner.writable_config_base(preferred, fallback).await
    }
    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        self.inner.ensure_file(path, content).await
    }
    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        self.inner.write_file(path, content).await
    }
    async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
        self.inner.write_file_with_mode(path, content, mode).await
    }
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.inner.exists(cmd, args).await
    }
    async fn read_file_to(&self, source: &Path, destination: &Path) -> Result<(), String> {
        self.inner.read_file_to(source, destination).await
    }
    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        self.inner.write_file_from(source, destination).await
    }
    async fn path_exists(&self, path: &Path) -> Result<bool, String> {
        self.inner.path_exists(path).await
    }
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let attempt = BudgetAttempt::new(&self.budgets, cmd, args);
        let result = self.inner.run(cmd, args, cwd, label).await;
        attempt.finish(
            result.as_ref().map(|value| value.as_str()).unwrap_or_else(|error| error.as_str()),
            if result.is_ok() { "success" } else { "error" },
        );
        result
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        let attempt = BudgetAttempt::new(&self.budgets, cmd, args);
        let result = self.inner.run_output(cmd, args, cwd, label).await;
        attempt.finish(
            match &result {
                Ok(output) => &output.stdout,
                Err(error) => error,
            },
            if result.as_ref().is_ok_and(|output| output.success()) { "success" } else { "error" },
        );
        result
    }
    async fn run_output_with_grace(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
        grace: Duration,
    ) -> Result<CommandOutput, String> {
        let attempt = BudgetAttempt::new(&self.budgets, cmd, args);
        let result = self.inner.run_output_with_grace(cmd, args, cwd, label, grace).await;
        attempt.finish(
            match &result {
                Ok(output) => &output.stdout,
                Err(error) => error,
            },
            if result.as_ref().is_ok_and(|output| output.success()) { "success" } else { "error" },
        );
        result
    }
    async fn run_with_timeout(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
        timeout: Duration,
    ) -> Result<String, String> {
        let attempt = BudgetAttempt::new(&self.budgets, cmd, args);
        let result = self.inner.run_with_timeout(cmd, args, cwd, label, timeout).await;
        attempt.finish(
            result.as_ref().map(|value| value.as_str()).unwrap_or_else(|error| error.as_str()),
            if result.is_ok() { "success" } else { "error" },
        );
        result
    }
    async fn spawn_long_lived(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
    ) -> Result<Box<dyn CommandProcess>, String> {
        self.inner.spawn_long_lived(cmd, args, cwd, label).await
    }
    async fn stream_lines(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<Box<dyn CommandLines>, String> {
        self.inner.stream_lines(cmd, args, cwd, label).await
    }
    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        self.inner.run_with_input(cmd, args, cwd, label, input).await
    }
    async fn run_to_file(&self, cmd: &str, args: &[&str], cwd: &Path, destination: &Path) -> Result<(), String> {
        self.inner.run_to_file(cmd, args, cwd, destination).await
    }
    async fn run_from_file(&self, cmd: &str, args: &[&str], cwd: &Path, source: &Path) -> Result<(), String> {
        self.inner.run_from_file(cmd, args, cwd, source).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The response comes from the gh/network boundary. A budget row reports the retry time GitHub
    // gave for its latest limited response, invents none for a limit without one, and gates no
    // later call: the next response replaces what the row says.
    #[test]
    fn retry_time_is_only_what_github_said() {
        let budgets = ForgeBudgets::default();
        let reset = Utc::now() + chrono::Duration::hours(1);
        let limited = format!(
            "HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {}\r\n\r\n{{\"message\":\"API rate limit exceeded\"}}",
            reset.timestamp()
        );
        budgets.after("gh", &["api", "graphql"], &limited);
        assert_eq!(budgets.rows("host")[0].retry_at.map(|at| at.timestamp()), Some(reset.timestamp()));

        budgets.after("gh", &["api", "graphql"], "{\"data\":{\"rateLimit\":{\"cost\":1,\"remaining\":4999}}}");
        assert_eq!(budgets.rows("host")[0].retry_at, None, "a response that was not limited clears the retry time");

        let untimed =
            "HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 4999\r\n\r\n{\"message\":\"You have exceeded a secondary rate limit\"}";
        budgets.after("gh", &["api", "graphql"], untimed);
        let row = &budgets.rows("host")[0];
        assert_eq!(row.retry_at, None, "a limit without Retry-After has no retry time");
        assert_eq!(row.calls, 3);
    }
    // Reported GraphQL points and calls without reported cost are distinct.
    // Generate zero-cost responses, large costs, and duplicate sequential calls.
    #[hegel::test]
    fn generated_forge_cost_accounting(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let cost = tc.draw(gs::integers::<u64>().min_value(0).max_value(1000));
        let count = tc.draw(gs::integers::<u64>().min_value(0).max_value(20));
        let budgets = ForgeBudgets::default();
        let raw = format!("{{\"data\":{{\"rateLimit\":{{\"cost\":{cost},\"remaining\":123}}}}}}");
        for _ in 0..count {
            budgets.after("gh", &["api", "graphql"], &raw);
        }
        budgets.after("gh", &["api", "graphql"], "{}");
        let rows = budgets.rows("host");
        assert_eq!(rows[0].reported_cost, cost * count);
        assert_eq!(rows[0].calls, count + 1);
        assert_eq!(rows[0].unreported_calls, 1);
        assert_eq!(rows[0].host, "host");
        budgets.after("gh", &["api", "repos/org/repo"], "HTTP/2 304 Not Modified\r\n\r\n");
        assert_eq!(budgets.rows("host")[1].reported_cost, 0);
        for row in budgets.state.lock().unwrap().rows.values_mut() {
            row.window_start -= chrono::Duration::hours(1);
        }
        budgets.after("gh", &["api", "graphql"], &raw);
        assert_eq!(budgets.rows("host")[0].calls, 1);
        assert_eq!(budgets.rows("host")[0].reported_cost, cost);
        // Lifetime counters survive the hourly window reset, including unknown costs.
        let total = &budgets.counter_rows("host")[0];
        assert_eq!(total.calls, count + 2);
        assert_eq!(total.reported_cost, cost * (count + 1));
        assert_eq!(total.unreported_calls, 1);
    }
    // Cancellation accounting must survive an already poisoned diagnostics
    // mutex during unwinding, count the attempt, and leave cost unknown.
    #[tokio::test]
    async fn cancelled_attempt_survives_poisoned_budget_lock() {
        let budgets = ForgeBudgets::default();
        let state = budgets.state.clone();
        assert!(std::thread::spawn(move || {
            let _guard = state.lock().unwrap();
            panic!("poison diagnostics lock");
        })
        .join()
        .is_err());
        let attempt = BudgetAttempt::new(&budgets, "gh", &["api", "graphql"]);
        drop(attempt);
        let row = budgets.rows("host").pop().expect("cancelled attempt row");
        assert_eq!(row.calls, 1);
        assert_eq!(row.unreported_calls, 1);
        assert_eq!(row.reported_cost, 0);
    }

    // REST diagnostics conservatively count attempts, including HTTP failures
    // and failures whose transport result cannot prove that no request arrived.
    #[hegel::test]
    fn rest_failure_attempt_accounting(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let status = tc.draw(gs::integers::<u16>().min_value(400).max_value(599));
        let budgets = ForgeBudgets::default();
        budgets.after("gh", &["api", "repos/org/repo"], &format!("HTTP/2 {status} Error\r\n\r\n{{}}"));
        budgets.after("gh", &["api", "repos/org/repo"], "failed to spawn gh");
        budgets.after("gh", &["api", "repos/org/repo"], "connection reset by peer");
        let row = &budgets.rows("host")[0];
        assert_eq!(row.calls, 3);
        assert_eq!(row.reported_cost, 3);
        assert_eq!(row.unreported_calls, 0);
    }

    // CLI subcommands may use REST, GraphQL or both. Their observed attempts
    // must not fabricate either protocol's reported cost. Generate read/write verbs.
    #[hegel::test]
    fn cli_attempts_remain_unclassified(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let verb = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
        let verbs = ["view", "list", "create", "close", "edit", "merge", "reopen"];
        let budgets = ForgeBudgets::default();
        for entity in ["pr", "issue"] {
            budgets.after("gh", &[entity, verbs[verb]], "{}");
        }
        let rows = budgets.rows("host");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].budget, "CLI");
        assert_eq!(rows[0].calls, 2);
        assert_eq!(rows[0].reported_cost, 0);
        assert_eq!(rows[0].unreported_calls, 2);
    }

    // Wrappers must explicitly forward every runner operation, including future
    // methods with defaults. Runtime provisioning tests cover forwarding semantics;
    // this structural contract catches a newly added default silently bypassed here.
    #[test]
    fn runner_wrapper_covers_the_entire_trait() {
        fn methods(source: &str) -> std::collections::BTreeSet<&str> {
            source.split("fn ").skip(1).map(|method| method.split('(').next().unwrap().trim()).collect()
        }
        // Cargo owns the location of the pinned Git dependency; do not assume a checkout/cache layout.
        let output = std::process::Command::new(env!("CARGO"))
            .args(["metadata", "--locked", "--format-version", "1"])
            .output()
            .expect("locate the pinned Rigging source");
        assert!(output.status.success(), "cargo metadata: {}", String::from_utf8_lossy(&output.stderr));
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).expect("Cargo metadata JSON");
        let manifest = metadata["packages"]
            .as_array()
            .expect("packages")
            .iter()
            .find(|package| package["name"] == "rigging-process")
            .expect("pinned rigging-process")["manifest_path"]
            .as_str()
            .expect("manifest path");
        let runtime = std::fs::read_to_string(Path::new(manifest).parent().expect("crate directory").join("src/lib.rs"))
            .expect("Rigging CommandRunner source");
        let runner = runtime
            .split("pub trait CommandRunner: Send + Sync {")
            .nth(1)
            .expect("Rigging bump may have changed CommandRunner's declaration; update the completeness-test parser")
            .split("\n}\n")
            .next()
            .expect("CommandRunner trait body");
        let budget = include_str!("forge_budget.rs");
        let wrapper = budget
            .split("impl CommandRunner for BudgetedRunner {")
            .nth(1)
            .expect("budget wrapper implementation")
            .split("#[cfg(test)]")
            .next()
            .expect("production implementation");
        assert_eq!(methods(wrapper), methods(runner), "every CommandRunner method needs explicit forwarding");
    }
}
