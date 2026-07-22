use std::{fmt, path::Path};

use chrono::Utc;
use flotilla_resources::{CheckoutIntegrationStatus, CheckoutSpec, CheckoutStatus, ConditionValue, IntegrationCondition, LandedEvidence};

use crate::providers::{ChannelLabel, CommandRunner};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedRepository {
    path: String,
    branch: String,
    local_commits: Option<usize>,
    uncommitted_entries: Option<usize>,
}

impl fmt::Display for EmbeddedRepository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let local_commits = self.local_commits.map_or_else(
            || "local commits unknown".to_string(),
            |count| format!("{count} local commit{}", if count == 1 { "" } else { "s" }),
        );
        write!(formatter, "embedded repository {}/ (branch {}, {local_commits}", self.path, self.branch)?;
        if let Some(count) = self.uncommitted_entries.filter(|count| *count > 0) {
            write!(formatter, ", {count} uncommitted entr{}", if count == 1 { "y" } else { "ies" })?;
        }
        write!(formatter, ")")
    }
}

pub fn checkout_branch_from_spec(spec: &CheckoutSpec) -> &str {
    match spec {
        CheckoutSpec::Worktree(spec) => &spec.r#ref,
        CheckoutSpec::FreshClone(spec) => &spec.r#ref,
        CheckoutSpec::Observed(spec) => &spec.r#ref,
    }
}

pub fn checkout_path_from_status_and_spec<'a>(status: Option<&'a CheckoutStatus>, spec: &'a CheckoutSpec) -> Option<&'a str> {
    status.as_ref().and_then(|status| status.path.as_deref()).or_else(|| spec.target_path()).or(match spec {
        CheckoutSpec::Observed(spec) => Some(spec.path.as_str()),
        _ => None,
    })
}

pub async fn inspect_checkout_integration(runner: &dyn CommandRunner, checkout_path: &Path, branch: &str) -> CheckoutIntegrationStatus {
    let observed_at = Utc::now().to_rfc3339();
    let clean = inspect_clean(runner, checkout_path, &observed_at).await;
    let pushed = inspect_pushed(runner, checkout_path, &observed_at).await;
    let (landed, landed_evidence) = inspect_landed(runner, checkout_path, branch, &observed_at).await;
    CheckoutIntegrationStatus { clean, pushed, landed, landed_evidence }
}

async fn inspect_clean(runner: &dyn CommandRunner, checkout_path: &Path, observed_at: &str) -> IntegrationCondition {
    match runner.run_output("git", &["status", "--porcelain"], checkout_path, &ChannelLabel::Noop).await {
        Ok(output) if output.success => {
            let mut details = output
                .stdout
                .lines()
                .filter(|line| {
                    let trimmed = line.trim_start();
                    !trimmed.starts_with("?? .flotilla/briefs/") && !trimmed.starts_with(".flotilla/briefs/")
                })
                .map(str::to_string)
                .collect::<Vec<_>>();
            match inspect_embedded_repositories(runner, checkout_path).await {
                Ok(repositories) => details.extend(repositories.into_iter().map(|repository| repository.to_string())),
                Err(error) => {
                    details.push(error);
                    return IntegrationCondition::builder()
                        .value(ConditionValue::Unknown)
                        .details(details)
                        .observed_at(observed_at.to_string())
                        .build();
                }
            }
            if details.is_empty() {
                IntegrationCondition::builder().value(ConditionValue::True).observed_at(observed_at.to_string()).build()
            } else {
                IntegrationCondition::builder().value(ConditionValue::False).details(details).observed_at(observed_at.to_string()).build()
            }
        }
        Ok(output) => IntegrationCondition::builder()
            .value(ConditionValue::Unknown)
            .details(vec![non_empty_output_or("git status failed", &output.stderr)])
            .observed_at(observed_at.to_string())
            .build(),
        Err(error) => IntegrationCondition::builder()
            .value(ConditionValue::Unknown)
            .details(vec![format!("git status could not run: {error}")])
            .observed_at(observed_at.to_string())
            .build(),
    }
}

pub async fn inspect_embedded_repositories(runner: &dyn CommandRunner, checkout_path: &Path) -> Result<Vec<EmbeddedRepository>, String> {
    let output = runner
        .run_output(
            "find",
            &[".", "-path", "./.git", "-prune", "-o", "-mindepth", "2", "-name", ".git", "-print", "-prune"],
            checkout_path,
            &ChannelLabel::Noop,
        )
        .await
        .map_err(|error| format!("embedded repository scan could not run: {error}"))?;
    if !output.success {
        return Err(non_empty_output_or("embedded repository scan failed", &output.stderr));
    }

    let mut paths = output
        .stdout
        .lines()
        .filter_map(|git_path| Path::new(git_path).parent())
        .filter_map(|repository_path| repository_path.strip_prefix(".").ok())
        .filter(|repository_path| !repository_path.as_os_str().is_empty())
        .map(|repository_path| repository_path.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();

    let mut repositories = Vec::new();
    for path in paths {
        if is_gitlink(runner, checkout_path, &path).await {
            continue;
        }
        repositories.push(inspect_embedded_repository(runner, checkout_path, path).await);
    }
    Ok(repositories)
}

async fn is_gitlink(runner: &dyn CommandRunner, checkout_path: &Path, path: &str) -> bool {
    runner
        .run_output("git", &["ls-files", "--stage", "--", path], checkout_path, &ChannelLabel::Noop)
        .await
        .is_ok_and(|output| output.success && output.stdout.lines().any(|line| line.starts_with("160000 ")))
}

async fn inspect_embedded_repository(runner: &dyn CommandRunner, checkout_path: &Path, path: String) -> EmbeddedRepository {
    let branch =
        match runner.run_output("git", &["-C", &path, "symbolic-ref", "--short", "-q", "HEAD"], checkout_path, &ChannelLabel::Noop).await {
            Ok(output) if output.success && !output.stdout.trim().is_empty() => output.stdout.trim().to_string(),
            _ => match runner.run_output("git", &["-C", &path, "rev-parse", "--short", "HEAD"], checkout_path, &ChannelLabel::Noop).await {
                Ok(output) if output.success && !output.stdout.trim().is_empty() => format!("detached at {}", output.stdout.trim()),
                _ => "unknown".to_string(),
            },
        };
    let local_commits = runner
        .run_output("git", &["-C", &path, "rev-list", "--count", "HEAD", "--not", "--remotes"], checkout_path, &ChannelLabel::Noop)
        .await
        .ok()
        .filter(|output| output.success)
        .and_then(|output| output.stdout.trim().parse().ok());
    let uncommitted_entries = runner
        .run_output("git", &["-C", &path, "status", "--porcelain"], checkout_path, &ChannelLabel::Noop)
        .await
        .ok()
        .filter(|output| output.success)
        .map(|output| output.stdout.lines().count());
    EmbeddedRepository { path, branch, local_commits, uncommitted_entries }
}

async fn inspect_pushed(runner: &dyn CommandRunner, checkout_path: &Path, observed_at: &str) -> IntegrationCondition {
    let upstream = match runner.run_output("git", &["rev-parse", "--abbrev-ref", "@{upstream}"], checkout_path, &ChannelLabel::Noop).await {
        Ok(output) if output.success && !output.stdout.trim().is_empty() => output.stdout.trim().to_string(),
        _ => match runner.run_output("git", &["rev-parse", "--abbrev-ref", "origin/HEAD"], checkout_path, &ChannelLabel::Noop).await {
            Ok(output) if output.success && !output.stdout.trim().is_empty() => output.stdout.trim().to_string(),
            Ok(output) => {
                return IntegrationCondition::builder()
                    .value(ConditionValue::Unknown)
                    .details(vec![non_empty_output_or("could not determine upstream for pushed check", &output.stderr)])
                    .observed_at(observed_at.to_string())
                    .build();
            }
            Err(error) => {
                return IntegrationCondition::builder()
                    .value(ConditionValue::Unknown)
                    .details(vec![format!("could not determine upstream for pushed check: {error}")])
                    .observed_at(observed_at.to_string())
                    .build();
            }
        },
    };
    let range = format!("{upstream}..HEAD");
    match runner.run_output("git", &["rev-list", "--count", &range], checkout_path, &ChannelLabel::Noop).await {
        Ok(output) if output.success => match output.stdout.trim().parse::<usize>() {
            Ok(0) => IntegrationCondition::builder().value(ConditionValue::True).observed_at(observed_at.to_string()).build(),
            Ok(count) => IntegrationCondition::builder()
                .value(ConditionValue::False)
                .details(vec![format!("{count} unpushed commit{}", if count == 1 { "" } else { "s" })])
                .observed_at(observed_at.to_string())
                .build(),
            Err(_) => IntegrationCondition::builder()
                .value(ConditionValue::Unknown)
                .details(vec![format!("could not parse unpushed commit count: {}", output.stdout.trim())])
                .observed_at(observed_at.to_string())
                .build(),
        },
        Ok(output) => IntegrationCondition::builder()
            .value(ConditionValue::Unknown)
            .details(vec![non_empty_output_or("git rev-list failed", &output.stderr)])
            .observed_at(observed_at.to_string())
            .build(),
        Err(error) => IntegrationCondition::builder()
            .value(ConditionValue::Unknown)
            .details(vec![format!("git rev-list could not run: {error}")])
            .observed_at(observed_at.to_string())
            .build(),
    }
}

async fn inspect_landed(
    runner: &dyn CommandRunner,
    checkout_path: &Path,
    branch: &str,
    observed_at: &str,
) -> (IntegrationCondition, Option<LandedEvidence>) {
    match runner
        .run_output(
            "gh",
            &["pr", "list", "--head", branch, "--state", "all", "--json", "number,state,mergedAt", "--limit", "1"],
            checkout_path,
            &ChannelLabel::Noop,
        )
        .await
    {
        Ok(output) if output.success => match serde_json::from_str::<serde_json::Value>(&output.stdout) {
            Ok(serde_json::Value::Array(items)) => match items.first() {
                Some(item) => {
                    let number =
                        item.get("number").and_then(serde_json::Value::as_i64).map(|number| number.to_string()).unwrap_or_default();
                    let state = item.get("state").and_then(serde_json::Value::as_str).unwrap_or("unknown");
                    let merged_at = item.get("mergedAt").and_then(serde_json::Value::as_str).filter(|value| !value.is_empty());
                    if state.eq_ignore_ascii_case("MERGED") || merged_at.is_some() {
                        (
                            IntegrationCondition::builder()
                                .value(ConditionValue::True)
                                .details(vec![format!("PR #{number} merged")])
                                .observed_at(observed_at.to_string())
                                .build(),
                            Some(
                                LandedEvidence::builder().change_request_id(number).maybe_merged_at(merged_at.map(str::to_string)).build(),
                            ),
                        )
                    } else {
                        (
                            IntegrationCondition::builder()
                                .value(ConditionValue::False)
                                .details(vec![format!("PR #{number} {state}, not merged")])
                                .observed_at(observed_at.to_string())
                                .build(),
                            None,
                        )
                    }
                }
                None => (
                    IntegrationCondition::builder()
                        .value(ConditionValue::False)
                        .details(vec!["no PR found for branch".to_string()])
                        .observed_at(observed_at.to_string())
                        .build(),
                    None,
                ),
            },
            Ok(_) | Err(_) => (
                IntegrationCondition::builder()
                    .value(ConditionValue::Unknown)
                    .details(vec!["could not parse gh PR lookup output".to_string()])
                    .observed_at(observed_at.to_string())
                    .build(),
                None,
            ),
        },
        Ok(output) => (
            IntegrationCondition::builder()
                .value(ConditionValue::Unknown)
                .details(vec![non_empty_output_or("gh PR lookup failed", &output.stderr)])
                .observed_at(observed_at.to_string())
                .build(),
            None,
        ),
        Err(error) => (
            IntegrationCondition::builder()
                .value(ConditionValue::Unknown)
                .details(vec![format!("gh PR lookup could not run: {error}")])
                .observed_at(observed_at.to_string())
                .build(),
            None,
        ),
    }
}

fn non_empty_output_or(fallback: &str, output: &str) -> String {
    let output = output.trim();
    if output.is_empty() {
        fallback.to_string()
    } else {
        output.to_string()
    }
}
