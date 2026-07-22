use std::path::Path;

use chrono::Utc;
use flotilla_resources::{CheckoutIntegrationStatus, ConditionValue, IntegrationCondition, LandedEvidence};

use crate::providers::{ChannelLabel, CommandRunner};

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
            let files = output
                .stdout
                .lines()
                .filter(|line| {
                    let trimmed = line.trim_start();
                    !trimmed.starts_with("?? .flotilla/briefs/") && !trimmed.starts_with(".flotilla/briefs/")
                })
                .map(str::to_string)
                .collect::<Vec<_>>();
            if files.is_empty() {
                IntegrationCondition::builder().value(ConditionValue::True).observed_at(observed_at.to_string()).build()
            } else {
                IntegrationCondition::builder().value(ConditionValue::False).details(files).observed_at(observed_at.to_string()).build()
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
