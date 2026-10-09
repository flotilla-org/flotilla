use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::Utc;
use flotilla_resources::{
    ChangeRequestMergeability, ChangeRequestObservation, ChangeRequestState, ChangeRequestStatus, Checkout, CheckoutIntegrationStatus,
    CheckoutSpec, CheckoutStatus, ConditionValue, Convoy, CrewWorkPhase, IntegrationCondition, LandedEvidence, ObservedChangeRequestState,
    RemoteRefObservation, ResourceObject, CHANGE_REQUEST_ID_LABEL,
};

use crate::{
    providers::{ChannelLabel, CommandRunner},
    vcs::{RepositoryRead, Vcs, VcsCheck},
};

/// Maximum age of checkout evidence used to settle or tear down a convoy.
pub const LANDING_EVIDENCE_TTL: Duration = Duration::from_secs(30);
const CONVOY_ASSOCIATION_UNAVAILABLE: &str = "no change request exists for the checkout ref, but convoy association was not available";

pub fn checkout_observation_lacks_convoy_association(status: &CheckoutIntegrationStatus) -> bool {
    status.landed.details.iter().any(|detail| detail == CONVOY_ASSOCIATION_UNAVAILABLE)
}

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

/// Resolve the change request associated with one of a convoy's checkouts.
///
/// A checkout can probe one change request only when the convoy's subject set
/// identifies exactly one active request in its repository.
pub fn convoy_change_request_id_for_checkout(
    convoy: &ResourceObject<Convoy>,
    checkout: &ResourceObject<Checkout>,
    forges: &[flotilla_resources::ForgeSpec],
) -> Option<String> {
    if let Some(id) = checkout.metadata.labels.get(CHANGE_REQUEST_ID_LABEL) {
        return Some(id.clone());
    }
    let repository = convoy.spec.repositories.iter().find(|repository| repository.repo_ref == *checkout.spec.repo_ref())?;
    let flotilla_protocol::LeafAddress::ChangeRequest { service, scope, .. } =
        flotilla_resources::change_request_address_with_forges(&repository.url, "1", forges).ok()?
    else {
        return None;
    };
    let ids = flotilla_resources::active_change_request_subjects(convoy)
        .ok()?
        .into_iter()
        .filter(|subject| subject.source.service == service && subject.source.scope == scope)
        .map(|subject| subject.id)
        .collect::<BTreeSet<_>>();
    if ids.len() == 1 {
        ids.into_iter().next()
    } else {
        None
    }
}

/// Extract forge PR references carried by a crew claim or ledger text.
/// Only repositories admitted to this convoy may become produced subjects.
pub fn change_request_subjects_from_claim(
    message: &str,
    repositories: &[flotilla_resources::ConvoyRepositorySpec],
    forges: &[flotilla_resources::ForgeSpec],
) -> Vec<flotilla_protocol::Subject> {
    let context = flotilla_resources::convoy_reference_context(repositories, None, None, forges, |_| None);
    let mut found = Vec::new();
    for (start, _) in message.match_indices("https://").chain(message.match_indices("http://")) {
        let tail = &message[start..];
        let end = tail
            .find(|character: char| character.is_whitespace() || matches!(character, ')' | ']' | '>' | '"' | '\''))
            .unwrap_or(tail.len());
        let reference = tail[..end].trim_end_matches(['.', ',', ';', ':']);
        if let Ok(subject) = context.parse(reference) {
            if subject.kind == flotilla_protocol::SubjectKind::ChangeRequest
                && context.repositories.iter().any(|repository| repository.source == subject.source)
                && !found.contains(&subject)
            {
                found.push(subject);
            }
        }
    }
    found
}

/// Inspect a checkout without claiming that a checkout-ref lookup covers its
/// owning convoy. An absent change request therefore cannot produce Landed.
pub async fn inspect_checkout_integration(
    runner: &dyn CommandRunner,
    vcs: &dyn Vcs,
    checkout_path: &Path,
    spec: &CheckoutSpec,
    change_request_id: Option<&str>,
) -> CheckoutIntegrationStatus {
    inspect_checkout_integration_with_association(
        IntegrationProviders { runner, vcs },
        checkout_path,
        spec,
        change_request_id,
        None,
        change_request_id.is_some(),
        false,
    )
    .await
}

/// Inspect a checkout for settlement after the caller has resolved the
/// convoy's associated change request, if any.
pub async fn inspect_convoy_checkout_integration(
    runner: &dyn CommandRunner,
    vcs: &dyn Vcs,
    checkout_path: &Path,
    spec: &CheckoutSpec,
    convoy: &ResourceObject<Convoy>,
    change_request_id: Option<&str>,
    observed_change_request: Option<&ChangeRequestStatus>,
) -> CheckoutIntegrationStatus {
    let observe_remote_ref = convoy.status.as_ref().is_some_and(|status| {
        status.crew_work.values().flat_map(BTreeMap::values).any(|work| work.phase == CrewWorkPhase::Done && work.claim_evidence.is_some())
    });
    inspect_checkout_integration_with_association(
        IntegrationProviders { runner, vcs },
        checkout_path,
        spec,
        change_request_id,
        observed_change_request,
        convoy.status.as_ref().is_some_and(|status| status.branch_subject_scan_at.is_some()) || change_request_id.is_some(),
        observe_remote_ref,
    )
    .await
}

#[derive(Clone, Copy)]
struct IntegrationProviders<'a> {
    runner: &'a dyn CommandRunner,
    vcs: &'a dyn Vcs,
}

async fn inspect_checkout_integration_with_association(
    providers: IntegrationProviders<'_>,
    checkout_path: &Path,
    spec: &CheckoutSpec,
    change_request_id: Option<&str>,
    observed_change_request: Option<&ChangeRequestStatus>,
    convoy_association_complete: bool,
    observe_remote_ref: bool,
) -> CheckoutIntegrationStatus {
    let observed_at = Utc::now().to_rfc3339();
    let clean = inspect_clean(providers.vcs, &observed_at).await;
    let pushed = inspect_pushed(providers.vcs, observed_change_request, &observed_at).await;
    let (landed, landed_evidence, change_request) = inspect_landed(
        providers,
        checkout_path,
        checkout_base_ref_from_spec(spec),
        change_request_id,
        convoy_association_complete,
        &observed_at,
    )
    .await;
    let remote_refs = if observe_remote_ref {
        inspect_remote_ref(providers.vcs, checkout_branch_from_spec(spec), &observed_at).await
    } else {
        BTreeMap::new()
    };
    let head_revision = match providers.vcs.read_repository(checkout_path, RepositoryRead::HeadRevision).await {
        Ok(revision) => Some(revision.trim().to_string()).filter(|revision| !revision.is_empty()),
        Err(error) => {
            tracing::debug!(checkout = %checkout_path.display(), %error, "checkout HEAD observation unavailable");
            None
        }
    };
    CheckoutIntegrationStatus { head_revision, clean, pushed, landed, landed_evidence, change_request, remote_refs }
}

async fn inspect_remote_ref(vcs: &dyn Vcs, branch: &str, observed_at: &str) -> BTreeMap<String, RemoteRefObservation> {
    let remote_ref = if branch.starts_with("refs/") { branch.to_string() } else { format!("refs/heads/{branch}") };
    match vcs.remote_ref_digest("origin", &remote_ref).await {
        Ok(Some(digest)) => {
            BTreeMap::from([(remote_ref, RemoteRefObservation::builder().digest(digest).observed_at(observed_at.to_string()).build())])
        }
        _ => BTreeMap::new(),
    }
}

async fn inspect_clean(vcs: &dyn Vcs, observed_at: &str) -> IntegrationCondition {
    let (value, details) = match vcs.is_clean().await {
        VcsCheck::True(details) => (ConditionValue::True, details),
        VcsCheck::False(details) => (ConditionValue::False, details),
        VcsCheck::Unknown(details) => (ConditionValue::Unknown, details),
    };
    IntegrationCondition::builder().value(value).details(details).observed_at(observed_at.to_string()).build()
}

async fn inspect_pushed(vcs: &dyn Vcs, observed_change_request: Option<&ChangeRequestStatus>, observed_at: &str) -> IntegrationCondition {
    let merged_head = observed_change_request
        .filter(|status| status.state.value == Some(ObservedChangeRequestState::Merged))
        .and_then(|status| status.head_sha.value.as_deref());
    let (value, details) = match vcs.unpushed_commits(merged_head).await {
        VcsCheck::True(details) => (ConditionValue::True, details),
        VcsCheck::False(details) => (ConditionValue::False, details),
        VcsCheck::Unknown(details) => (ConditionValue::Unknown, details),
    };
    IntegrationCondition::builder().value(value).details(details).observed_at(observed_at.to_string()).build()
}

async fn inspect_landed(
    providers: IntegrationProviders<'_>,
    checkout_path: &Path,
    base_ref: Option<&str>,
    change_request_id: Option<&str>,
    convoy_association_complete: bool,
    observed_at: &str,
) -> (IntegrationCondition, Option<LandedEvidence>, Option<ChangeRequestObservation>) {
    // The convoy's branch discovery records whether it searched the forge.
    // Checkout inspection probes only a request already named by its subject
    // set; it never searches the checkout branch for an identity of its own.
    let comparison = compare_branch_to_base(providers.vcs, base_ref).await;
    let Some(id) = change_request_id else {
        return (landed_without_change_request(&comparison, convoy_association_complete, observed_at), None, None);
    };
    let args = vec!["pr", "view", id, "--json", "number,state,mergedAt,baseRefName,mergeable,headRefOid"];
    match providers.runner.run_output("gh", &args, checkout_path, &ChannelLabel::Default).await {
        Ok(output) if output.success() => match serde_json::from_str::<serde_json::Value>(&output.stdout) {
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
                        let landing = match state {
                            ChangeRequestState::Closed => Some(("closed", ConditionValue::True, format!("PR #{number} closed"), false)),
                            ChangeRequestState::Merged => {
                                let merged_head =
                                    item.get("headRefOid").and_then(serde_json::Value::as_str).filter(|value| !value.is_empty());
                                let revision_check = match merged_head {
                                    Some(head) => providers.vcs.head_in_history_of(head).await,
                                    None => Err("merged PR head revision is unavailable".to_string()),
                                };
                                let (value, detail, head_verified) = match revision_check {
                                    Ok(true) => {
                                        (ConditionValue::True, format!("PR #{number} merged; checkout HEAD is in merged PR head"), true)
                                    }
                                    Ok(false) => (
                                        ConditionValue::False,
                                        format!("PR #{number} merged, but checkout HEAD is not in merged PR head"),
                                        false,
                                    ),
                                    Err(error) => (
                                        ConditionValue::Unknown,
                                        format!("PR #{number} merged; checkout revision could not be verified: {error}"),
                                        false,
                                    ),
                                };
                                Some(("merged", value, detail, head_verified))
                            }
                            ChangeRequestState::Open => None,
                        };
                        if let Some((outcome, value, detail, head_verified)) = landing {
                            (
                                IntegrationCondition::builder()
                                    .value(value)
                                    .details(vec![detail])
                                    .observed_at(observed_at.to_string())
                                    .build(),
                                Some(
                                    LandedEvidence::builder()
                                        .change_request_id(number)
                                        .maybe_merged_at(merged_at.map(str::to_string))
                                        .maybe_target_ref(if outcome == "merged" { target_ref.map(str::to_string) } else { None })
                                        .checkout_head_in_merged_head(head_verified)
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
                    None => (landed_without_change_request(&comparison, convoy_association_complete, observed_at), None, None),
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

async fn compare_branch_to_base(vcs: &dyn Vcs, base_ref: Option<&str>) -> BaseComparison {
    match vcs.commits_beyond_base(base_ref).await {
        Ok((base_ref, count)) => BaseComparison::Counted { base_ref, count },
        Err(detail) => BaseComparison::Indeterminate { detail },
    }
}

/// A branch with no visible change request only counts as landed when it has
/// no commits beyond its base: "work exists but no change request is visible
/// yet" is not landed — the observation may simply predate the change request
/// (absence of evidence, not evidence of absence). The nothing-beyond-base
/// case is true only after the forge lookup has also found no associated
/// change request.
fn landed_without_change_request(
    comparison: &BaseComparison,
    convoy_association_complete: bool,
    observed_at: &str,
) -> IntegrationCondition {
    if !convoy_association_complete {
        return IntegrationCondition::builder()
            .value(ConditionValue::Unknown)
            .details(vec![CONVOY_ASSOCIATION_UNAVAILABLE.to_string()])
            .observed_at(observed_at.to_string())
            .build();
    }
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
    use std::sync::Arc;

    use flotilla_resources::{
        CheckoutSpec, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, InMemoryBackend, InputMeta, ObservedCheckoutSpec, ResourceBackend,
    };

    use super::*;
    use crate::{
        providers::{testing::MockRunner, vcs::git_worktree::GitWorktreeStrategy},
        vcs::{FlotillaVcs, GitCheckoutStrategy},
    };
    use flotilla_paths::path_context::ExecutionEnvironmentPath;

    #[test]
    fn claim_links_a_pr_from_a_different_branch_and_forge() {
        let repositories = vec![flotilla_resources::ConvoyRepositorySpec {
            url: "https://forge.example/team/robert/project-map".into(),
            repo_ref: flotilla_protocol::RepositoryKey("project-map".into()),
            source_ref: "main".into(),
            target_ref: "main".into(),
            workspace_slug: "project-map".into(),
            subpaths: Vec::new(),
        }];
        let forge = flotilla_resources::ForgeSpec::builder()
            .forge_id("lab".into())
            .kind(flotilla_resources::ForgeKind::Forgejo)
            .hosts(std::collections::BTreeSet::from(["forge.example".into()]))
            .https_url("https://forge.example/team".into())
            .git_ssh_host("forge.example".into())
            .build();
        let subjects = change_request_subjects_from_claim(
            "Published from another branch: [PR](https://forge.example/team/robert/project-map/pulls/12).",
            &repositories,
            &[forge],
        );
        assert_eq!(subjects.len(), 1);
        assert_eq!(subjects[0].internal().expect("internal reference"), "cr/lab/robert/project-map/12");
        let subjects = change_request_subjects_from_claim(
            "https://forge.example/team/other/repository/pulls/13",
            &repositories,
            &[flotilla_resources::ForgeSpec::builder()
                .forge_id("lab".into())
                .kind(flotilla_resources::ForgeKind::Forgejo)
                .hosts(std::collections::BTreeSet::from(["forge.example".into()]))
                .https_url("https://forge.example/team".into())
                .git_ssh_host("forge.example".into())
                .build()],
        );
        assert!(subjects.is_empty(), "a claim cannot produce a PR in an unadmitted repository");
    }

    #[tokio::test]
    async fn checkout_matches_the_full_aliased_forge_identity() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let repo_ref = flotilla_protocol::RepositoryKey("project-map".into());
        let repository = ConvoyRepositorySpec {
            url: "https://forge.example/team/robert/project-map".into(),
            repo_ref: repo_ref.clone(),
            source_ref: "main".into(),
            target_ref: "main".into(),
            workspace_slug: "project-map".into(),
            subpaths: Vec::new(),
        };
        let mut convoy = backend
            .clone()
            .using::<Convoy>("flotilla")
            .create(
                &InputMeta::builder().name("convoy".into()).build(),
                &ConvoySpec::builder().workflow_ref("work".into()).repositories(vec![repository]).build(),
            )
            .await
            .expect("create convoy");
        let mut status = ConvoyStatus::default();
        status.discover_subject(
            flotilla_protocol::Subject {
                kind: flotilla_protocol::SubjectKind::ChangeRequest,
                source: flotilla_protocol::provider_data::IssueSource { service: "lab".into(), scope: "robert/project-map".into() },
                id: "12".into(),
            },
            flotilla_protocol::Relationship::Produces,
            flotilla_resources::SubjectDiscoverySource::Claim,
            Utc::now(),
        );
        convoy.status = Some(status);
        let checkout = backend
            .using::<Checkout>("flotilla")
            .create(
                &InputMeta::builder().name("checkout".into()).build(),
                &CheckoutSpec::Observed(ObservedCheckoutSpec {
                    r#ref: "feature".into(),
                    path: "/checkout".into(),
                    repo_ref,
                    host_ref: "host".into(),
                    is_main: false,
                }),
            )
            .await
            .expect("create checkout");
        let forge = flotilla_resources::ForgeSpec::builder()
            .forge_id("lab".into())
            .kind(flotilla_resources::ForgeKind::Forgejo)
            .hosts(std::collections::BTreeSet::from(["forge.example".into()]))
            .https_url("https://forge.example/team".into())
            .git_ssh_host("forge.example".into())
            .build();
        assert_eq!(convoy_change_request_id_for_checkout(&convoy, &checkout, &[forge]), Some("12".into()));
        assert_eq!(convoy_change_request_id_for_checkout(&convoy, &checkout, &[]), None);
    }

    fn test_vcs(runner: Arc<MockRunner>) -> FlotillaVcs {
        FlotillaVcs::new(
            ExecutionEnvironmentPath::new("/checkout"),
            runner.clone(),
            GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(".".into(), runner))),
        )
    }

    fn merged_change_request(head_sha: &str) -> ChangeRequestStatus {
        let observed_at = "2026-08-04T12:00:00Z".parse().expect("valid timestamp");
        ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: flotilla_resources::Observation::known(ObservedChangeRequestState::Merged, observed_at),
            head_sha: flotilla_resources::Observation::known(head_sha.to_string(), observed_at),
            checks: flotilla_resources::Observation::unknown(observed_at),
            review: flotilla_resources::ChangeRequestReviewObservation {
                actionable_at_head: flotilla_resources::Observation::unknown(observed_at),
            },
            mergeable: flotilla_resources::Observation::unknown(observed_at),
        }
    }

    struct HeadOnlyRunner;

    #[async_trait::async_trait]
    impl CommandRunner for HeadOnlyRunner {
        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            false
        }
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            if cmd == "git" && args == ["rev-parse", "HEAD"] {
                Ok("new-commit\n".into())
            } else {
                Err("unavailable".into())
            }
        }
        async fn run_output(
            &self,
            cmd: &str,
            args: &[&str],
            cwd: &Path,
            label: &ChannelLabel,
        ) -> Result<crate::providers::CommandOutput, String> {
            self.run(cmd, args, cwd, label).await.map(|stdout| crate::providers::CommandOutput {
                stdout,
                stderr: String::new(),
                exit_code: Some(0),
            })
        }
    }

    #[tokio::test]
    async fn integration_observes_local_commit_even_when_other_evidence_is_unavailable() {
        let runner = Arc::new(HeadOnlyRunner);
        let vcs = FlotillaVcs::new(
            ExecutionEnvironmentPath::new("/checkout"),
            runner.clone(),
            GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(".".into(), runner.clone()))),
        );
        let spec = CheckoutSpec::Observed(
            ObservedCheckoutSpec::builder()
                .r#ref("topic".into())
                .path("/checkout".into())
                .repo_ref(flotilla_protocol::RepositoryKey("repo".into()))
                .host_ref("host".into())
                .is_main(false)
                .build(),
        );
        let observed = inspect_checkout_integration(&*runner, &vcs, Path::new("/checkout"), &spec, None).await;
        assert_eq!(observed.head_revision.as_deref(), Some("new-commit"));
    }

    #[tokio::test]
    async fn git_transport_observes_the_exact_remote_ref_digest() {
        let runner = Arc::new(MockRunner::new(vec![Ok("abc123\trefs/heads/topic\n".into())]));
        let vcs = test_vcs(runner.clone());

        let observations = inspect_remote_ref(&vcs, "topic", "2026-08-04T12:00:00Z").await;

        assert_eq!(observations["refs/heads/topic"].digest, "abc123");
        assert_eq!(observations["refs/heads/topic"].observed_at, "2026-08-04T12:00:00Z");
        assert_eq!(runner.calls()[0].1, vec!["ls-remote", "--refs", "origin", "refs/heads/topic"]);
    }

    #[tokio::test]
    async fn squash_merged_head_is_pushed_even_after_remote_branch_deletion() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let vcs = test_vcs(runner.clone());
        let change_request = merged_change_request("merged-head");

        let pushed = inspect_pushed(&vcs, Some(&change_request), "2026-08-04T12:00:00Z").await;

        assert_eq!(pushed.value, ConditionValue::True);
        assert_eq!(
            runner.calls(),
            vec![(
                "git".to_string(),
                vec!["merge-base".to_string(), "--is-ancestor".to_string(), "HEAD".to_string(), "merged-head".to_string(),]
            )]
        );
    }

    #[tokio::test]
    async fn commit_after_squash_merged_head_remains_unpushed() {
        let runner = Arc::new(MockRunner::new(vec![Err("not an ancestor".into()), Ok("origin/feature\n".into()), Ok("1\n".into())]));
        let vcs = test_vcs(runner.clone());
        let change_request = merged_change_request("merged-head");

        let pushed = inspect_pushed(&vcs, Some(&change_request), "2026-08-04T12:00:00Z").await;

        assert_eq!(pushed.value, ConditionValue::False);
        assert_eq!(pushed.details, vec!["1 unpushed commit"]);
    }

    #[tokio::test]
    async fn squash_merged_branch_without_upstream_is_pushed_when_remote_ref_contains_head() {
        let runner = Arc::new(MockRunner::new(vec![Err("no upstream configured".into()), Ok("  origin/feature\n".into())]));
        let vcs = test_vcs(runner.clone());

        let pushed = inspect_pushed(&vcs, None, "2026-08-04T12:00:00Z").await;

        assert_eq!(pushed.value, ConditionValue::True);
        assert_eq!(runner.calls()[1].1, vec!["branch", "--remotes", "--contains", "HEAD"]);
    }

    #[tokio::test]
    async fn branch_without_upstream_or_containing_remote_reports_unpushed_commit_count() {
        let runner = Arc::new(MockRunner::new(vec![Err("no upstream configured".into()), Ok(String::new()), Ok("2\n".into())]));
        let vcs = test_vcs(runner.clone());

        let pushed = inspect_pushed(&vcs, None, "2026-08-04T12:00:00Z").await;

        assert_eq!(pushed.value, ConditionValue::False);
        assert_eq!(pushed.details, vec!["2 unpushed commits"]);
        assert_eq!(runner.calls()[2].1, vec!["rev-list", "--count", "HEAD", "--not", "--remotes"]);
    }

    #[tokio::test]
    async fn branch_pushed_between_no_upstream_probes_reports_pushed() {
        let runner = Arc::new(MockRunner::new(vec![Err("no upstream configured".into()), Ok(String::new()), Ok("0\n".into())]));
        let vcs = test_vcs(runner.clone());

        let pushed = inspect_pushed(&vcs, None, "2026-08-04T12:00:00Z").await;

        assert_eq!(pushed.value, ConditionValue::True);
    }

    #[tokio::test]
    async fn checkout_without_change_request_keeps_upstream_pushed_probe() {
        let runner = Arc::new(MockRunner::new(vec![Ok("origin/feature\n".into()), Ok("0\n".into())]));
        let vcs = test_vcs(runner.clone());

        let pushed = inspect_pushed(&vcs, None, "2026-08-04T12:00:00Z").await;

        assert_eq!(pushed.value, ConditionValue::True);
        assert_eq!(runner.calls()[0].1, vec!["rev-parse", "--abbrev-ref", "@{upstream}"]);
    }

    #[tokio::test]
    async fn ignored_embedded_repository_without_local_commits_keeps_checkout_clean() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok(String::new()),
            Ok("./.tools/ghostty-src/.git\n".into()),
            Ok(String::new()),
            Err("detached".into()),
            Ok("64daa599c\n".into()),
            Ok("0\n".into()),
            Ok(String::new()),
        ]));
        let vcs = test_vcs(runner.clone());

        let clean = inspect_clean(&vcs, "2026-08-04T12:00:00Z").await;

        assert_eq!(clean.value, ConditionValue::True);
        assert_eq!(runner.calls()[2].1, vec!["check-ignore", "--quiet", "--", ".tools/ghostty-src"]);
    }

    #[tokio::test]
    async fn ignored_embedded_repository_with_local_commits_makes_checkout_unclean() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok(String::new()),
            Ok("./.tools/ghostty-src/.git\n".into()),
            Ok(String::new()),
            Ok("feature/local-work\n".into()),
            Ok("2\n".into()),
            Ok(String::new()),
        ]));
        let vcs = test_vcs(runner.clone());

        let clean = inspect_clean(&vcs, "2026-08-04T12:00:00Z").await;

        assert_eq!(clean.value, ConditionValue::False);
        assert_eq!(clean.details, vec!["embedded repository .tools/ghostty-src/ (branch feature/local-work, 2 local commits)"]);
    }

    async fn landed_with_responses(responses: Vec<Result<String, String>>) -> IntegrationCondition {
        let runner = Arc::new(MockRunner::new(responses));
        let vcs = test_vcs(runner.clone());
        let (landed, _, _) = inspect_landed(
            IntegrationProviders { runner: &*runner, vcs: &vcs },
            Path::new("/checkout"),
            Some("main"),
            Some("1162"),
            true,
            "2026-07-27T00:00:00Z",
        )
        .await;
        landed
    }

    #[tokio::test]
    async fn untouched_branch_is_landed_after_convoy_branch_scan_found_no_change_request() {
        let runner = Arc::new(MockRunner::new(vec![Ok("0".into())]));
        let vcs = test_vcs(runner.clone());
        let (landed, _, _) = inspect_landed(
            IntegrationProviders { runner: &*runner, vcs: &vcs },
            Path::new("/checkout"),
            Some("main"),
            None,
            true,
            "2026-07-27T00:00:00Z",
        )
        .await;

        assert_eq!(landed.value, ConditionValue::True);
        assert_eq!(runner.calls().len(), 1, "checkout inspection does not search for a PR");
    }

    #[tokio::test]
    async fn open_associated_change_request_from_another_branch_holds_landing_when_spec_ref_is_at_base() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok("0".into()),
            Ok(r#"{"number":1338,"state":"OPEN","mergedAt":null,"baseRefName":"main"}"#.into()),
        ]));
        let vcs = test_vcs(runner.clone());
        let (landed, _, change_request) = inspect_landed(
            IntegrationProviders { runner: &*runner, vcs: &vcs },
            Path::new("/checkout"),
            Some("main"),
            Some("1338"),
            true,
            "2026-07-27T00:00:00Z",
        )
        .await;

        assert_eq!(landed.value, ConditionValue::False);
        assert_eq!(change_request.expect("associated change request should be observed").state, ChangeRequestState::Open);
        assert_eq!(runner.calls()[1].1[2], "1338");
    }

    #[tokio::test]
    async fn named_change_request_is_unknown_when_forge_cannot_be_consulted() {
        let landed = landed_with_responses(vec![Ok("0".into()), Err("authentication unavailable".into())]).await;
        assert_eq!(landed.value, ConditionValue::Unknown);
    }

    #[tokio::test]
    async fn checkout_only_lookup_cannot_prove_that_the_convoy_has_no_change_request() {
        let runner = Arc::new(MockRunner::new(vec![Ok("0".into())]));
        let vcs = test_vcs(runner.clone());
        let (landed, _, _) = inspect_landed(
            IntegrationProviders { runner: &*runner, vcs: &vcs },
            Path::new("/checkout"),
            Some("main"),
            None,
            false,
            "2026-07-27T00:00:00Z",
        )
        .await;

        assert_eq!(landed.value, ConditionValue::Unknown);
    }

    #[tokio::test]
    async fn commits_beyond_base_without_change_request_is_not_landed() {
        let runner = Arc::new(MockRunner::new(vec![Ok("2".into())]));
        let vcs = test_vcs(runner.clone());
        let (landed, _, _) = inspect_landed(
            IntegrationProviders { runner: &*runner, vcs: &vcs },
            Path::new("/checkout"),
            Some("main"),
            None,
            true,
            "2026-07-27T00:00:00Z",
        )
        .await;
        assert_eq!(landed.value, ConditionValue::False);
        assert_eq!(runner.calls().len(), 1);
        assert!(landed.details.iter().any(|detail| detail.contains("2 commits beyond origin/main")), "details: {:?}", landed.details);
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
        let runner = Arc::new(MockRunner::new(vec![
            Ok("0".into()),
            Ok(r#"{"number":1071,"state":"MERGED","mergedAt":"2026-07-27T12:00:00Z","baseRefName":"main","headRefOid":"merged-head"}"#
                .into()),
            Ok(String::new()),
        ]));
        let vcs = test_vcs(runner.clone());
        let (landed, evidence, change_request) = inspect_landed(
            IntegrationProviders { runner: &*runner, vcs: &vcs },
            Path::new("/checkout"),
            Some("main"),
            Some("1071"),
            true,
            "2026-07-27T00:00:00Z",
        )
        .await;

        assert_eq!(landed.value, ConditionValue::True);
        assert_eq!(evidence.as_ref().map(|evidence| evidence.change_request_id.as_str()), Some("1071"));
        assert_eq!(evidence.as_ref().and_then(|evidence| evidence.target_ref.as_deref()), Some("main"));
        assert_eq!(change_request.expect("bound change request should be observed").state, ChangeRequestState::Merged);
        assert_eq!(
            runner.calls()[1],
            (
                "gh".to_string(),
                vec![
                    "pr".to_string(),
                    "view".to_string(),
                    "1071".to_string(),
                    "--json".to_string(),
                    "number,state,mergedAt,baseRefName,mergeable,headRefOid".to_string(),
                ],
            )
        );
    }

    #[tokio::test]
    async fn conflicting_change_request_is_part_of_the_integration_observation() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok("2".into()),
            Ok(r#"[{"number": 1162, "state": "OPEN", "mergedAt": null, "baseRefName": "main", "mergeable": "CONFLICTING"}]"#.into()),
        ]));
        let vcs = test_vcs(runner.clone());

        let (_, _, change_request) = inspect_landed(
            IntegrationProviders { runner: &*runner, vcs: &vcs },
            Path::new("/checkout"),
            Some("main"),
            Some("1162"),
            true,
            "2026-07-27T00:00:00Z",
        )
        .await;

        assert_eq!(change_request.expect("open change request should be observed").mergeability, ChangeRequestMergeability::Conflicting);
    }

    #[tokio::test]
    async fn merged_change_request_is_landed() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok("2".into()),
            Ok(r#"[{"number": 1162, "state": "MERGED", "mergedAt": "2026-07-27T00:00:00Z", "baseRefName": "main", "headRefOid": "merged-head"}]"#.into()),
            Ok(String::new()),
        ]));
        let vcs = test_vcs(runner.clone());
        let (landed, evidence, _) = inspect_landed(
            IntegrationProviders { runner: &*runner, vcs: &vcs },
            Path::new("/checkout"),
            Some("main"),
            Some("1162"),
            true,
            "2026-07-27T00:00:00Z",
        )
        .await;
        assert_eq!(landed.value, ConditionValue::True);
        assert!(evidence.expect("merged landing evidence").checkout_head_in_merged_head);
        assert_eq!(runner.calls()[2].1, vec!["merge-base", "--is-ancestor", "HEAD", "merged-head"]);
    }

    #[tokio::test]
    async fn merged_change_request_does_not_land_commit_added_after_merge() {
        let landed = landed_with_responses(vec![
            Ok("3".into()),
            Ok(r#"[{"number": 1162, "state": "MERGED", "mergedAt": "2026-07-27T00:00:00Z", "baseRefName": "main", "headRefOid": "merged-head"}]"#.into()),
            Err(String::new()), // `git merge-base --is-ancestor` exits 1 without stderr.
        ])
        .await;
        assert_eq!(landed.value, ConditionValue::False);
    }

    #[tokio::test]
    async fn missing_merged_pr_head_is_unknown() {
        let landed = landed_with_responses(vec![
            Ok("2".into()),
            Ok(r#"[{"number": 1162, "state": "MERGED", "mergedAt": "2026-07-27T00:00:00Z", "baseRefName": "main"}]"#.into()),
        ])
        .await;
        assert_eq!(landed.value, ConditionValue::Unknown);
    }

    #[tokio::test]
    async fn unavailable_merged_pr_commit_is_unknown() {
        let landed = landed_with_responses(vec![
            Ok("2".into()),
            Ok(r#"[{"number": 1162, "state": "MERGED", "mergedAt": "2026-07-27T00:00:00Z", "headRefOid": "missing-head"}]"#.into()),
            Err("fatal: Not a valid commit name missing-head".into()),
        ])
        .await;
        assert_eq!(landed.value, ConditionValue::Unknown);
        assert!(landed.details[0].contains("could not be verified"));
    }

    #[tokio::test]
    async fn closed_unmerged_change_request_keeps_landed_without_verified_head() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok("2".into()),
            Ok(r#"[{"number": 1162, "state": "CLOSED", "mergedAt": null, "baseRefName": "main"}]"#.into()),
        ]));
        let vcs = test_vcs(runner.clone());
        let (landed, evidence, _) = inspect_landed(
            IntegrationProviders { runner: &*runner, vcs: &vcs },
            Path::new("/checkout"),
            Some("main"),
            Some("1162"),
            true,
            "2026-07-27T00:00:00Z",
        )
        .await;
        assert_eq!(landed.value, ConditionValue::True);
        assert!(!evidence.expect("closed change request evidence").checkout_head_in_merged_head);
        assert_eq!(runner.calls().len(), 2, "closed PR must not trigger a Git ancestry check");
    }

    #[tokio::test]
    async fn indeterminate_base_without_change_request_is_unknown() {
        let runner = Arc::new(MockRunner::new(vec![Err("fatal: ambiguous argument".into())]));
        let vcs = test_vcs(runner.clone());
        let (landed, _, _) = inspect_landed(
            IntegrationProviders { runner: &*runner, vcs: &vcs },
            Path::new("/checkout"),
            None,
            None,
            true,
            "2026-07-27T00:00:00Z",
        )
        .await;
        assert_eq!(landed.value, ConditionValue::Unknown);
    }
}
