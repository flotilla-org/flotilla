use std::{
    fmt,
    path::{Path, PathBuf},
};

use chrono::Utc;
use flotilla_resources::{
    ChangeRequestMergeability, ChangeRequestObservation, ChangeRequestState, CheckoutIntegrationStatus, CheckoutSpec, CheckoutStatus,
    ConditionValue, IntegrationCondition, LandedEvidence,
};

use crate::providers::{ChannelLabel, CommandRunner};

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
struct EmbeddedRepository {
    path: PathBuf,
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
        write!(formatter, "embedded repository {}/ (branch {}, {local_commits}", self.path.display(), self.branch)?;
        if let Some(count) = self.uncommitted_entries.filter(|count| *count > 0) {
            write!(formatter, ", {count} uncommitted entr{}", if count == 1 { "y" } else { "ies" })?;
        }
        write!(formatter, ")")
    }
}

fn checkout_branch_from_spec(spec: &CheckoutSpec) -> &str {
    match spec {
        CheckoutSpec::Worktree(spec) => &spec.r#ref,
        CheckoutSpec::FreshClone(spec) => &spec.r#ref,
        CheckoutSpec::Observed(spec) => &spec.r#ref,
    }
}

fn checkout_base_ref_from_spec(spec: &CheckoutSpec) -> Option<&str> {
    match spec {
        CheckoutSpec::Worktree(spec) => spec.base_ref.as_deref(),
        CheckoutSpec::FreshClone(spec) => spec.base_ref.as_deref(),
        CheckoutSpec::Observed(spec) if spec.is_main => Some(&spec.r#ref),
        CheckoutSpec::Observed(_) => None,
    }
}

pub fn checkout_path_from_status_and_spec<'a>(status: Option<&'a CheckoutStatus>, spec: &'a CheckoutSpec) -> Option<&'a str> {
    status.as_ref().and_then(|status| status.path.as_deref()).or_else(|| spec.target_path()).or(match spec {
        CheckoutSpec::Observed(spec) => Some(spec.path.as_str()),
        _ => None,
    })
}

pub async fn inspect_checkout_integration(
    runner: &dyn CommandRunner,
    checkout_path: &Path,
    spec: &CheckoutSpec,
    change_request_id: Option<&str>,
) -> CheckoutIntegrationStatus {
    let observed_at = Utc::now().to_rfc3339();
    let clean = inspect_clean(runner, checkout_path, &observed_at).await;
    let pushed = inspect_pushed(runner, checkout_path, &observed_at).await;
    let (landed, landed_evidence, change_request) = inspect_landed(
        runner,
        checkout_path,
        checkout_branch_from_spec(spec),
        checkout_base_ref_from_spec(spec),
        change_request_id,
        &observed_at,
    )
    .await;
    CheckoutIntegrationStatus { clean, pushed, landed, landed_evidence, change_request }
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

async fn inspect_embedded_repositories(runner: &dyn CommandRunner, checkout_path: &Path) -> Result<Vec<EmbeddedRepository>, String> {
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
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();

    let mut repositories = Vec::new();
    for path in paths {
        repositories.push(inspect_embedded_repository(runner, checkout_path, path).await);
    }
    Ok(repositories)
}

async fn inspect_embedded_repository(runner: &dyn CommandRunner, checkout_path: &Path, path: PathBuf) -> EmbeddedRepository {
    let path_arg = path.to_string_lossy();
    let branch = match runner
        .run_output("git", &["-C", &path_arg, "symbolic-ref", "--short", "-q", "HEAD"], checkout_path, &ChannelLabel::Noop)
        .await
    {
        Ok(output) if output.success && !output.stdout.trim().is_empty() => output.stdout.trim().to_string(),
        _ => match runner.run_output("git", &["-C", &path_arg, "rev-parse", "--short", "HEAD"], checkout_path, &ChannelLabel::Noop).await {
            Ok(output) if output.success && !output.stdout.trim().is_empty() => format!("detached at {}", output.stdout.trim()),
            _ => "unknown".to_string(),
        },
    };
    let local_commits = runner
        .run_output(
            "git",
            &["-C", &path_arg, "rev-list", "--count", "HEAD", "--all", "--not", "--remotes"],
            checkout_path,
            &ChannelLabel::Noop,
        )
        .await
        .ok()
        .filter(|output| output.success)
        .and_then(|output| output.stdout.trim().parse().ok());
    let uncommitted_entries = runner
        .run_output("git", &["-C", &path_arg, "status", "--porcelain"], checkout_path, &ChannelLabel::Noop)
        .await
        .ok()
        .filter(|output| output.success)
        .map(|output| output.stdout.lines().count());
    EmbeddedRepository::builder()
        .path(path)
        .branch(branch)
        .maybe_local_commits(local_commits)
        .maybe_uncommitted_entries(uncommitted_entries)
        .build()
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
    base_ref: Option<&str>,
    change_request_id: Option<&str>,
    observed_at: &str,
) -> (IntegrationCondition, Option<LandedEvidence>, Option<ChangeRequestObservation>) {
    // Git evidence alone cannot prove that no change request remains
    // outstanding. In particular, a crew may publish its work from a branch
    // other than the checkout's provisioned ref. Always consult the forge;
    // failure to do so leaves the condition unknown.
    let comparison = compare_branch_to_base(runner, checkout_path, base_ref).await;
    let args = match change_request_id {
        Some(id) => vec!["pr", "view", id, "--json", "number,state,mergedAt,baseRefName,mergeable"],
        None => {
            vec!["pr", "list", "--head", branch, "--state", "all", "--json", "number,state,mergedAt,baseRefName,mergeable", "--limit", "1"]
        }
    };
    match runner.run_output("gh", &args, checkout_path, &ChannelLabel::Noop).await {
        Ok(output) if output.success => match serde_json::from_str::<serde_json::Value>(&output.stdout) {
            Ok(value) => {
                let item = match value {
                    serde_json::Value::Array(items) => items.into_iter().next(),
                    serde_json::Value::Object(_) => Some(value),
                    _ => None,
                };
                match item {
                    Some(item) => {
                        let number =
                            item.get("number").and_then(serde_json::Value::as_i64).map(|number| number.to_string()).unwrap_or_default();
                        let state = item.get("state").and_then(serde_json::Value::as_str).unwrap_or("unknown");
                        let merged_at = item.get("mergedAt").and_then(serde_json::Value::as_str).filter(|value| !value.is_empty());
                        let target_ref = item.get("baseRefName").and_then(serde_json::Value::as_str).filter(|value| !value.is_empty());
                        let state = if state.eq_ignore_ascii_case("MERGED") || merged_at.is_some() {
                            ChangeRequestState::Merged
                        } else if state.eq_ignore_ascii_case("CLOSED") {
                            ChangeRequestState::Closed
                        } else {
                            ChangeRequestState::Open
                        };
                        let mergeability = match item.get("mergeable").and_then(serde_json::Value::as_str) {
                            Some(value) if value.eq_ignore_ascii_case("MERGEABLE") => ChangeRequestMergeability::Mergeable,
                            Some(value) if value.eq_ignore_ascii_case("CONFLICTING") => ChangeRequestMergeability::Conflicting,
                            _ => ChangeRequestMergeability::Unknown,
                        };
                        let observation = ChangeRequestObservation::builder()
                            .id(number.clone())
                            .state(state)
                            .mergeability(mergeability)
                            .maybe_target_ref(target_ref.map(str::to_string))
                            .observed_at(observed_at.to_string())
                            .build();
                        if state != ChangeRequestState::Open {
                            let outcome = if state == ChangeRequestState::Closed { "closed" } else { "merged" };
                            (
                                IntegrationCondition::builder()
                                    .value(ConditionValue::True)
                                    .details(vec![format!("PR #{number} {outcome}")])
                                    .observed_at(observed_at.to_string())
                                    .build(),
                                Some(
                                    LandedEvidence::builder()
                                        .change_request_id(number)
                                        .maybe_merged_at(merged_at.map(str::to_string))
                                        .maybe_target_ref(if outcome == "merged" { target_ref.map(str::to_string) } else { None })
                                        .build(),
                                ),
                                Some(observation),
                            )
                        } else {
                            (
                                IntegrationCondition::builder()
                                    .value(ConditionValue::False)
                                    .details(vec![format!("PR #{number} OPEN, not merged")])
                                    .observed_at(observed_at.to_string())
                                    .build(),
                                None,
                                Some(observation),
                            )
                        }
                    }
                    None => (landed_without_change_request(&comparison, observed_at), None, None),
                }
            }
            Err(_) => (
                IntegrationCondition::builder()
                    .value(ConditionValue::Unknown)
                    .details(vec!["could not parse gh PR lookup output".to_string()])
                    .observed_at(observed_at.to_string())
                    .build(),
                None,
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
            None,
        ),
        Err(error) => (
            IntegrationCondition::builder()
                .value(ConditionValue::Unknown)
                .details(vec![format!("gh PR lookup could not run: {error}")])
                .observed_at(observed_at.to_string())
                .build(),
            None,
            None,
        ),
    }
}

enum BaseComparison {
    /// Base resolved and the commits beyond it counted.
    Counted { base_ref: String, count: usize },
    /// Base could not be resolved or compared.
    Indeterminate { detail: String },
}

async fn compare_branch_to_base(runner: &dyn CommandRunner, checkout_path: &Path, base_ref: Option<&str>) -> BaseComparison {
    let base_ref = match base_ref {
        Some(base_ref) => base_ref.to_string(),
        None => match runner.run_output("git", &["rev-parse", "--abbrev-ref", "origin/HEAD"], checkout_path, &ChannelLabel::Noop).await {
            Ok(output) if output.success && !output.stdout.trim().is_empty() => output.stdout.trim().to_string(),
            Ok(output) => {
                return BaseComparison::Indeterminate {
                    detail: non_empty_output_or("the base ref could not be determined", &output.stderr),
                };
            }
            Err(error) => return BaseComparison::Indeterminate { detail: format!("the base ref could not be determined: {error}") },
        },
    };
    let range = format!("{base_ref}..HEAD");
    match runner.run_output("git", &["rev-list", "--count", &range], checkout_path, &ChannelLabel::Noop).await {
        Ok(output) if output.success => match output.stdout.trim().parse::<usize>() {
            Ok(count) => BaseComparison::Counted { base_ref, count },
            Err(_) => BaseComparison::Indeterminate {
                detail: format!("could not parse commit count beyond {base_ref}: {}", output.stdout.trim()),
            },
        },
        Ok(output) => BaseComparison::Indeterminate {
            detail: non_empty_output_or(&format!("could not compare branch with base ref {base_ref}"), &output.stderr),
        },
        Err(error) => BaseComparison::Indeterminate { detail: format!("could not compare branch with base ref {base_ref}: {error}") },
    }
}

/// A branch with no visible change request only counts as landed when it has
/// no commits beyond its base: "work exists but no change request is visible
/// yet" is not landed — the observation may simply predate the change request
/// (absence of evidence, not evidence of absence). The nothing-beyond-base
/// case is true only after the forge lookup has also found no associated
/// change request.
fn landed_without_change_request(comparison: &BaseComparison, observed_at: &str) -> IntegrationCondition {
    match comparison {
        BaseComparison::Counted { base_ref, count: 0 } => IntegrationCondition::builder()
            .value(ConditionValue::True)
            .details(vec![format!("no change request exists; branch has no commits beyond {base_ref}")])
            .observed_at(observed_at.to_string())
            .build(),
        BaseComparison::Counted { base_ref, count } => IntegrationCondition::builder()
            .value(ConditionValue::False)
            .details(vec![format!("no change request exists; {count} commit{} beyond {base_ref}", if *count == 1 { "" } else { "s" })])
            .observed_at(observed_at.to_string())
            .build(),
        BaseComparison::Indeterminate { detail } => IntegrationCondition::builder()
            .value(ConditionValue::Unknown)
            .details(vec![format!("no change request exists and {detail}")])
            .observed_at(observed_at.to_string())
            .build(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::testing::MockRunner;

    async fn landed_with_responses(responses: Vec<Result<String, String>>) -> IntegrationCondition {
        let runner = MockRunner::new(responses);
        let (landed, _, _) = inspect_landed(&runner, Path::new("/checkout"), "feature/x", Some("main"), None, "2026-07-27T00:00:00Z").await;
        landed
    }

    #[tokio::test]
    async fn untouched_branch_is_landed_after_forge_reports_no_change_request() {
        let runner = MockRunner::new(vec![Ok("0".into()), Ok("[]".into())]);
        let (landed, _, _) = inspect_landed(&runner, Path::new("/checkout"), "feature/x", Some("main"), None, "2026-07-27T00:00:00Z").await;

        assert_eq!(landed.value, ConditionValue::True);
        assert_eq!(runner.calls()[1].0, "gh");
    }

    #[tokio::test]
    async fn open_associated_change_request_from_another_branch_holds_landing_when_spec_ref_is_at_base() {
        let runner =
            MockRunner::new(vec![Ok("0".into()), Ok(r#"{"number":1338,"state":"OPEN","mergedAt":null,"baseRefName":"main"}"#.into())]);
        let (landed, _, change_request) =
            inspect_landed(&runner, Path::new("/checkout"), "provisioned-ref", Some("main"), Some("1338"), "2026-07-27T00:00:00Z").await;

        assert_eq!(landed.value, ConditionValue::False);
        assert_eq!(change_request.expect("associated change request should be observed").state, ChangeRequestState::Open);
        assert_eq!(runner.calls()[1].1[2], "1338");
    }

    #[tokio::test]
    async fn untouched_branch_is_unknown_when_forge_cannot_be_consulted() {
        let landed = landed_with_responses(vec![Ok("0".into()), Err("authentication unavailable".into())]).await;
        assert_eq!(landed.value, ConditionValue::Unknown);
    }

    #[tokio::test]
    async fn commits_beyond_base_without_change_request_is_not_landed() {
        // The branch has commits and gh finds no PR: work exists that no
        // visible change request accounts for — the observation may simply
        // predate the change request, so the branch must not count as landed.
        let landed = landed_with_responses(vec![Ok("2".into()), Ok("[]".into())]).await;
        assert_eq!(landed.value, ConditionValue::False);
        assert!(landed.details.iter().any(|detail| detail.contains("2 commits beyond main")), "details: {:?}", landed.details);
    }

    #[tokio::test]
    async fn open_change_request_is_not_landed() {
        let landed = landed_with_responses(vec![
            Ok("2".into()),
            Ok(r#"[{"number": 1162, "state": "OPEN", "mergedAt": null, "baseRefName": "main"}]"#.into()),
        ])
        .await;
        assert_eq!(landed.value, ConditionValue::False);
    }

    #[tokio::test]
    async fn bound_change_request_landing_is_keyed_by_id_instead_of_branch() {
        let runner = MockRunner::new(vec![
            Ok("0".into()),
            Ok(r#"{"number":1071,"state":"MERGED","mergedAt":"2026-07-27T12:00:00Z","baseRefName":"main"}"#.into()),
        ]);
        let (landed, evidence, change_request) =
            inspect_landed(&runner, Path::new("/checkout"), "renamed-head", Some("main"), Some("1071"), "2026-07-27T00:00:00Z").await;

        assert_eq!(landed.value, ConditionValue::True);
        assert_eq!(evidence.as_ref().map(|evidence| evidence.change_request_id.as_str()), Some("1071"));
        assert_eq!(evidence.as_ref().and_then(|evidence| evidence.target_ref.as_deref()), Some("main"));
        assert_eq!(change_request.expect("bound change request should be observed").state, ChangeRequestState::Merged);
        assert_eq!(
            runner.calls()[1],
            ("gh".to_string(), vec![
                "pr".to_string(),
                "view".to_string(),
                "1071".to_string(),
                "--json".to_string(),
                "number,state,mergedAt,baseRefName,mergeable".to_string(),
            ],)
        );
    }

    #[tokio::test]
    async fn conflicting_change_request_is_part_of_the_integration_observation() {
        let runner = MockRunner::new(vec![
            Ok("2".into()),
            Ok(r#"[{"number": 1162, "state": "OPEN", "mergedAt": null, "baseRefName": "main", "mergeable": "CONFLICTING"}]"#.into()),
        ]);

        let (_, _, change_request) =
            inspect_landed(&runner, Path::new("/checkout"), "feature/x", Some("main"), None, "2026-07-27T00:00:00Z").await;

        assert_eq!(change_request.expect("open change request should be observed").mergeability, ChangeRequestMergeability::Conflicting);
    }

    #[tokio::test]
    async fn merged_change_request_is_landed() {
        let landed = landed_with_responses(vec![
            Ok("2".into()),
            Ok(r#"[{"number": 1162, "state": "MERGED", "mergedAt": "2026-07-27T00:00:00Z", "baseRefName": "main"}]"#.into()),
        ])
        .await;
        assert_eq!(landed.value, ConditionValue::True);
    }

    #[tokio::test]
    async fn indeterminate_base_without_change_request_is_unknown() {
        let runner = MockRunner::new(vec![Err("fatal: ambiguous argument".into()), Ok("[]".into())]);
        let (landed, _, _) = inspect_landed(&runner, Path::new("/checkout"), "feature/x", None, None, "2026-07-27T00:00:00Z").await;
        assert_eq!(landed.value, ConditionValue::Unknown);
    }
}
