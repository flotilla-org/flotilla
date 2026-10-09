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

use crate::providers::{
    forge::github::{github_rate_limit, low_graphql_budget, parse_gh_api_response, response_header},
    ChannelLabel, CommandOutput, CommandProcess, CommandRunner,
};

#[derive(Clone, Default)]
pub struct ForgeBudgets {
    state: Arc<Mutex<BudgetState>>,
}
#[derive(Default)]
struct BudgetState {
    rows: BTreeMap<String, ForgeBudgetRow>,
    deadlines: BTreeMap<String, tokio::time::Instant>,
}
impl BudgetState {
    fn expire_cooldowns(&mut self) {
        let now = Utc::now();
        let monotonic_now = tokio::time::Instant::now();
        self.deadlines.retain(|key, deadline| {
            let active = *deadline > monotonic_now && self.rows.get(key).and_then(|row| row.retry_at).is_some_and(|at| at > now);
            if !active {
                if let Some(row) = self.rows.get_mut(key) {
                    row.retry_at = None;
                }
            }
            active
        });
    }
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
    fn before(&self, cmd: &str, args: &[&str]) -> Result<(), String> {
        let Some((identity, budget)) = Self::key(cmd, args) else { return Ok(()) };
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire_cooldowns();
        if let Some(reset) = state.rows.get(&format!("{identity}/{budget}")).and_then(|row| row.retry_at) {
            return Err(format!("github rate limited (budget={budget}, identity={identity}, reset_at={})", reset.to_rfc3339()));
        }
        Ok(())
    }
    #[cfg(test)]
    fn after(&self, cmd: &str, args: &[&str], raw: &str) {
        let Some((identity, budget)) = Self::key(cmd, args) else { return };
        self.after_for(identity, budget, raw);
    }
    fn after_for(&self, identity: &str, budget: &str, raw: &str) {
        let now = Utc::now();
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire_cooldowns();
        let row = state.rows.entry(format!("{identity}/{budget}")).or_insert_with(|| ForgeBudgetRow {
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
        let retry_at = github_rate_limit(raw, now)
            .map(|limit| limit.retry_at.unwrap_or(now + chrono::Duration::minutes(1)))
            .or_else(|| (budget == "GraphQL").then(|| low_graphql_budget(raw, &document)).flatten());
        if let Some(retry_at) = retry_at {
            row.retry_at = Some(retry_at);
            if let Some(reset) = row.retry_at {
                let delay = reset.signed_duration_since(now).to_std().unwrap_or_default();
                state.deadlines.insert(format!("{identity}/{budget}"), tokio::time::Instant::now() + delay);
            }
        }
    }
    pub fn rows(&self, host: &str) -> Vec<ForgeBudgetRow> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire_cooldowns();
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
struct BudgetAttempt<'a> {
    budgets: &'a ForgeBudgets,
    key: Option<(&'static str, &'static str)>,
}
impl BudgetAttempt<'_> {
    fn finish(mut self, raw: &str) {
        if let Some((identity, budget)) = self.key.take() {
            self.budgets.after_for(identity, budget, raw);
        }
    }
}
impl Drop for BudgetAttempt<'_> {
    fn drop(&mut self) {
        if let Some((identity, budget)) = self.key {
            self.budgets.after_for(identity, budget, "cancelled before response");
        }
    }
}

pub struct BudgetedRunner {
    pub inner: Arc<dyn CommandRunner>,
    pub budgets: ForgeBudgets,
}
#[async_trait]
impl CommandRunner for BudgetedRunner {
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
        self.budgets.before(cmd, args)?;
        let attempt = BudgetAttempt { budgets: &self.budgets, key: ForgeBudgets::key(cmd, args) };
        let result = self.inner.run(cmd, args, cwd, label).await;
        attempt.finish(result.as_ref().map(|value| value.as_str()).unwrap_or_else(|error| error.as_str()));
        result
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        self.budgets.before(cmd, args)?;
        let attempt = BudgetAttempt { budgets: &self.budgets, key: ForgeBudgets::key(cmd, args) };
        let result = self.inner.run_output(cmd, args, cwd, label).await;
        attempt.finish(match &result {
            Ok(output) => &output.stdout,
            Err(error) => error,
        });
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
        self.budgets.before(cmd, args)?;
        let attempt = BudgetAttempt { budgets: &self.budgets, key: ForgeBudgets::key(cmd, args) };
        let result = self.inner.run_with_timeout(cmd, args, cwd, label, timeout).await;
        attempt.finish(result.as_ref().map(|value| value.as_str()).unwrap_or_else(|error| error.as_str()));
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
    // The response comes from the gh/network boundary. Quota exhaustion must
    // suppress subsequent commands until reset, independently for each budget.
    #[test]
    fn rate_limit_cooldown_and_budget_isolation() {
        let budgets = ForgeBudgets::default();
        let reset = Utc::now() + chrono::Duration::hours(1);
        let raw = format!(
            "HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {}\r\n\r\n{{\"message\":\"API rate limit exceeded\"}}",
            reset.timestamp()
        );
        budgets.after("gh", &["api", "graphql"], &raw);
        assert!(budgets.before("gh", &["api", "graphql"]).is_err());
        assert!(budgets.before("gh", &["api", "repos/org/repo/issues"]).is_ok());
        assert!(budgets.before("git", &["status"]).is_ok());
        let rows = budgets.rows("host");
        assert_eq!(rows[0].calls, 1);
        assert_eq!(rows[0].retry_at.unwrap().timestamp(), reset.timestamp());
        budgets.state.lock().unwrap().rows.values_mut().next().unwrap().retry_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(budgets.before("gh", &["api", "graphql"]).is_ok());
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
    }
    // #2928: successful GraphQL responses below the reserve pause subsequent
    // requests until reset, with accounting retained in the shared budget row.
    // Generate both sides of the 100-point reserve and header/body precedence.
    #[hegel::test]
    fn graphql_low_budget_reserve(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let remaining = tc.draw(gs::integers::<u64>().min_value(98).max_value(101));
        let headers = tc.draw(gs::booleans());
        let budgets = ForgeBudgets::default();
        let reset = Utc::now() + chrono::Duration::hours(1);
        let header = if headers {
            format!("X-RateLimit-Remaining: {remaining}\r\nX-RateLimit-Reset: {}\r\n", reset.timestamp())
        } else {
            String::new()
        };
        // Headers are authoritative when body fields disagree.
        let body_remaining = if headers { 5000 } else { remaining };
        let raw = format!(
            "HTTP/2 200 OK\r\n{header}\r\n{{\"data\":{{\"rateLimit\":{{\"cost\":2,\"remaining\":{body_remaining},\"resetAt\":\"{}\"}}}}}}",
            reset.to_rfc3339()
        );
        budgets.after("gh", &["api", "graphql"], &raw);
        assert_eq!(budgets.before("gh", &["api", "graphql"]).is_err(), remaining < 100);
        assert!(budgets.before("gh", &["api", "repos/team/repo/issues"]).is_ok());
        let row = &budgets.rows("host")[0];
        assert_eq!((row.calls, row.reported_cost), (1, 2));
        assert_eq!(row.remaining, Some(remaining));
        if remaining < 100 {
            assert_eq!(row.retry_at.expect("reserve cooldown").timestamp(), reset.timestamp());
        }
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
        let attempt = BudgetAttempt { budgets: &budgets, key: Some(("host gh login", "GraphQL")) };
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

    // Expired reset deadlines disappear from fleet health even without another
    // command or hourly reset; a recovered poisoned diagnostic lock stays usable.
    #[test]
    fn expired_cooldown_is_removed_from_health() {
        let budgets = ForgeBudgets::default();
        budgets.after(
            "gh",
            &["api", "graphql"],
            "HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\n\r\n{\"message\":\"API rate limit exceeded\"}",
        );
        {
            let mut state = budgets.state.lock().unwrap();
            let key = state.rows.keys().next().unwrap().clone();
            state.deadlines.insert(key, tokio::time::Instant::now() - Duration::from_secs(1));
        }
        assert!(budgets.rows("host")[0].retry_at.is_none());
        assert!(budgets.before("gh", &["api", "graphql"]).is_ok());
        let state = budgets.state.clone();
        let _ = std::thread::spawn(move || {
            let _guard = state.lock().unwrap();
            panic!("diagnostic writer interrupted");
        })
        .join();
        assert_eq!(budgets.rows("host")[0].calls, 1);
        budgets.after("gh", &["api", "graphql"], "{}");
        assert_eq!(budgets.rows("host")[0].calls, 2);
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
        let providers = include_str!("providers/mod.rs");
        let runner = providers
            .split("pub trait CommandRunner: Send + Sync {")
            .nth(1)
            .unwrap()
            .split("pub(crate) fn command_timeout_message")
            .next()
            .unwrap();
        let budget = include_str!("forge_budget.rs");
        let wrapper = budget.split("impl CommandRunner for BudgetedRunner {").nth(1).unwrap().split("#[cfg(test)]").next().unwrap();
        assert_eq!(methods(wrapper), methods(runner), "every CommandRunner method needs explicit forwarding");
    }
}
