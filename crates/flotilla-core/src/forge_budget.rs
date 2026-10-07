//! Hourly host-identity accounting at the gh subprocess boundary.
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::ForgeBudgetRow;

use crate::providers::{
    github_api::{github_rate_limit, parse_gh_api_response, response_header},
    ChannelLabel, CommandOutput, CommandProcess, CommandRunner,
};

#[derive(Clone, Default)]
pub struct ForgeBudgets(Arc<Mutex<BTreeMap<String, ForgeBudgetRow>>>, Arc<Mutex<BTreeMap<String, tokio::time::Instant>>>);
impl ForgeBudgets {
    fn key(cmd: &str, args: &[&str]) -> Option<(&'static str, &'static str)> {
        if cmd != "gh" {
            return None;
        }
        match args.first().copied()? {
            "api" => Some(("host gh login", if args.contains(&"graphql") { "GraphQL" } else { "REST" })),
            "pr" | "issue" => Some(("host gh login", "GraphQL")),
            _ => None,
        }
    }
    fn before(&self, cmd: &str, args: &[&str]) -> Result<(), String> {
        let Some((identity, budget)) = Self::key(cmd, args) else { return Ok(()) };
        let rows = self.0.lock().expect("forge budget lock poisoned");
        if let Some(row) = rows.get(&format!("{identity}/{budget}")) {
            if let Some(reset) = row.retry_at.filter(|at| {
                *at > Utc::now()
                    && self
                        .1
                        .lock()
                        .expect("forge cooldown lock poisoned")
                        .get(&format!("{identity}/{budget}"))
                        .is_none_or(|deadline| *deadline > tokio::time::Instant::now())
            }) {
                return Err(format!("github rate limited (budget={budget}, identity={identity}, reset_at={})", reset.to_rfc3339()));
            }
        }
        Ok(())
    }
    fn after(&self, cmd: &str, args: &[&str], raw: &str) {
        let Some((identity, budget)) = Self::key(cmd, args) else { return };
        let now = Utc::now();
        let mut rows = self.0.lock().expect("forge budget lock poisoned");
        let row = rows.entry(format!("{identity}/{budget}")).or_insert_with(|| ForgeBudgetRow {
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
            Some(if response.status == 304 { 0 } else { 1 })
        } else {
            document["data"]["rateLimit"]["cost"].as_u64()
        };
        if let Some(cost) = cost {
            row.reported_cost += cost + 1;
        } else {
            row.unreported_calls += 1;
        }
        row.remaining = response_header(raw, "x-ratelimit-remaining")
            .and_then(|value| value.parse().ok())
            .or_else(|| document["data"]["rateLimit"]["remaining"].as_u64())
            .or(row.remaining);
        if let Some(limit) = github_rate_limit(raw, now) {
            row.retry_at = limit.retry_at.or(Some(now + chrono::Duration::minutes(1)));
            if let Some(reset) = row.retry_at {
                let delay = reset.signed_duration_since(now).to_std().unwrap_or_default();
                self.1
                    .lock()
                    .expect("forge cooldown lock poisoned")
                    .insert(format!("{identity}/{budget}"), tokio::time::Instant::now() + delay);
            }
        }
    }
    pub fn rows(&self, host: &str) -> Vec<ForgeBudgetRow> {
        self.0
            .lock()
            .expect("forge budget lock poisoned")
            .values()
            .cloned()
            .map(|mut row| {
                row.host = host.into();
                row
            })
            .collect()
    }
}
pub struct BudgetedRunner {
    pub inner: Arc<dyn CommandRunner>,
    pub budgets: ForgeBudgets,
}
#[async_trait]
impl CommandRunner for BudgetedRunner {
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
        let result = self.inner.run(cmd, args, cwd, label).await;
        self.budgets.after(cmd, args, result.as_ref().map(|value| value.as_str()).unwrap_or_else(|error| error.as_str()));
        result
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        self.budgets.before(cmd, args)?;
        let result = self.inner.run_output(cmd, args, cwd, label).await;
        match &result {
            Ok(output) => self.budgets.after(cmd, args, &output.stdout),
            Err(error) => self.budgets.after(cmd, args, error),
        }
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
        let result = self.inner.run_with_timeout(cmd, args, cwd, label, timeout).await;
        self.budgets.after(cmd, args, result.as_ref().map(|value| value.as_str()).unwrap_or_else(|error| error.as_str()));
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
        budgets.0.lock().unwrap().values_mut().next().unwrap().retry_at = Some(Utc::now() - chrono::Duration::seconds(1));
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
        budgets.after("gh", &["pr", "view"], "{}");
        let rows = budgets.rows("host");
        assert_eq!(rows[0].reported_cost, cost * count);
        assert_eq!(rows[0].calls, count + 1);
        assert_eq!(rows[0].unreported_calls, 1);
        assert_eq!(rows[0].host, "host");
        budgets.after("gh", &["api", "repos/org/repo"], "HTTP/2 304 Not Modified\r\n\r\n");
        assert_eq!(budgets.rows("host")[1].reported_cost, 0);
        for row in budgets.0.lock().unwrap().values_mut() {
            row.window_start -= chrono::Duration::hours(1);
        }
        budgets.after("gh", &["api", "graphql"], &raw);
        assert_eq!(budgets.rows("host")[0].calls, 1);
        assert_eq!(budgets.rows("host")[0].reported_cost, cost);
    }
}
