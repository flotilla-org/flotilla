use std::path::Path;

use flotilla_protocol::CheckoutStatus;

use flotilla_provider_runtime::run;
use flotilla_vcs::vcs::RepositoryRead;
use flotilla_vcs::vcs::Vcs;

pub async fn fetch_checkout_status(
    branch: &str,
    checkout_path: Option<&Path>,
    change_request_id: Option<&str>,
    repo_root: &Path,
    vcs: &dyn Vcs,
    runner: &dyn flotilla_provider_runtime::CommandRunner,
) -> CheckoutStatus {
    let branch_owned = branch.to_string();
    let repo = repo_root.to_path_buf();
    let checkout_path = checkout_path.map(|p| p.to_path_buf());
    let cr_id = change_request_id.map(|s| s.to_string());
    let repo2 = repo_root.to_path_buf();
    let repo_for_base = repo.clone();
    let branch_for_base = branch_owned.clone();

    let (unpushed, uncommitted, change_request) = tokio::join!(
        async {
            let base = async {
                let upstream =
                    vcs.read_repository(&repo_for_base, RepositoryRead::UpstreamOf(&format!("{branch_for_base}@{{upstream}}"))).await;
                if let Ok(ref upstream) = upstream {
                    let upstream = upstream.trim();
                    if !upstream.is_empty() {
                        return Ok(upstream.to_string());
                    }
                }
                let remote_head = vcs.read_repository(&repo_for_base, RepositoryRead::UpstreamOf("origin/HEAD")).await;
                if let Ok(ref remote_head) = remote_head {
                    let remote_head = remote_head.trim();
                    if !remote_head.is_empty() {
                        return Ok(remote_head.to_string());
                    }
                }
                Err(format!(
                    "could not determine base branch (upstream: {}; origin/HEAD: {})",
                    upstream.err().unwrap_or_else(|| "empty".to_string()),
                    remote_head.err().unwrap_or_else(|| "empty".to_string()),
                ))
            }
            .await?;
            vcs.read_repository(&repo_for_base, RepositoryRead::CommitLog(&format!("{base}..{branch_for_base}")))
                .await
                .map_err(|err| format!("commit log {base}..{branch_for_base} failed: {err}"))
        },
        async {
            match &checkout_path {
                Some(path) => vcs.read_repository(path, RepositoryRead::WorkingTreeChanges).await.map(Some),
                None => Ok(None),
            }
        },
        async {
            let Some(ref number) = cr_id else { return Ok(None) };
            let json = run!(runner, "gh", &["pr", "view", number, "--json", "state,mergeCommit"], &repo2)?;
            serde_json::from_str::<serde_json::Value>(&json)
                .map(Some)
                .map_err(|err| format!("gh pr view {number} returned invalid JSON: {err}"))
        },
    );

    let mut info = CheckoutStatus { branch: branch.to_string(), ..Default::default() };
    match unpushed {
        Ok(output) => info.unpushed_commits = output.lines().filter(|line| !line.is_empty()).map(str::to_string).collect(),
        Err(error) => info.unpushed_commits_error = Some(error),
    }
    match uncommitted {
        Ok(Some(output)) => {
            info.uncommitted_files = output.lines().filter(|line| !line.trim().is_empty()).map(str::to_string).collect();
            info.has_uncommitted = !info.uncommitted_files.is_empty();
        }
        Ok(None) => {}
        Err(error) => info.uncommitted_files_error = Some(error),
    }
    match change_request {
        Ok(Some(json)) => {
            info.change_request_status = json.get("state").and_then(serde_json::Value::as_str).map(str::to_string);
            info.merge_commit_sha = json
                .get("mergeCommit")
                .and_then(|value| value.get("oid"))
                .and_then(serde_json::Value::as_str)
                .map(|sha| sha[..7.min(sha.len())].to_string());
        }
        Ok(None) => {}
        Err(error) => info.change_request_error = Some(error),
    }
    info
}
