use std::path::Path;

use flotilla_protocol::CheckoutStatus;

use crate::providers::run;

pub async fn fetch_checkout_status(
    branch: &str,
    checkout_path: Option<&Path>,
    change_request_id: Option<&str>,
    repo_root: &Path,
    runner: &dyn crate::providers::CommandRunner,
) -> CheckoutStatus {
    let branch_owned = branch.to_string();
    let repo = repo_root.to_path_buf();
    let checkout_path = checkout_path.map(|p| p.to_path_buf());
    let cr_id = change_request_id.map(|s| s.to_string());
    let repo2 = repo_root.to_path_buf();
    let repo_for_base = repo.clone();
    let branch_for_base = branch_owned.clone();

    let (unpushed_result, uncommitted, pr_info) = tokio::join!(
        async {
            let base = async {
                let upstream =
                    run!(runner, "git", &["rev-parse", "--abbrev-ref", &format!("{branch_for_base}@{{upstream}}")], &repo_for_base);
                if let Ok(ref upstream) = upstream {
                    let upstream = upstream.trim();
                    if !upstream.is_empty() {
                        return Ok(upstream.to_string());
                    }
                }
                let remote_head = run!(runner, "git", &["rev-parse", "--abbrev-ref", "origin/HEAD"], &repo_for_base);
                if let Ok(ref remote_head) = remote_head {
                    let remote_head = remote_head.trim();
                    if !remote_head.is_empty() {
                        return Ok(remote_head.to_string());
                    }
                }
                Err("Could not determine base branch — unpushed commit status unknown".to_string())
            }
            .await;

            match base {
                Ok(base_ref) => Ok(run!(runner, "git", &["log", &format!("{base_ref}..{branch_for_base}"), "--oneline"], &repo_for_base)
                    .unwrap_or_default()),
                Err(warning) => Err(warning),
            }
        },
        async {
            if let Some(path) = &checkout_path {
                run!(runner, "git", &["status", "--porcelain"], path).unwrap_or_default()
            } else {
                String::new()
            }
        },
        async {
            if let Some(ref number) = cr_id {
                run!(runner, "gh", &["pr", "view", number, "--json", "state,mergeCommit"], &repo2).ok()
            } else {
                None
            }
        },
    );

    let mut info = CheckoutStatus { branch: branch.to_string(), ..Default::default() };
    match unpushed_result {
        Ok(output) => info.unpushed_commits = output.lines().filter(|line| !line.is_empty()).map(str::to_string).collect(),
        Err(warning) => info.base_detection_warning = Some(warning),
    }
    info.uncommitted_files = uncommitted.lines().filter(|line| !line.trim().is_empty()).map(str::to_string).collect();
    info.has_uncommitted = !info.uncommitted_files.is_empty();

    if let Some(json) = pr_info.and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok()) {
        info.change_request_status = json.get("state").and_then(serde_json::Value::as_str).map(str::to_string);
        info.merge_commit_sha = json
            .get("mergeCommit")
            .and_then(|value| value.get("oid"))
            .and_then(serde_json::Value::as_str)
            .map(|sha| sha[..7.min(sha.len())].to_string());
    }
    info
}
