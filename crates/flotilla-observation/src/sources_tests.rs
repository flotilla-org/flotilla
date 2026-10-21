use super::*;
use chrono::TimeZone;
use flotilla_resources::{ConvoySpec, ConvoyStatus, ObjectMeta, VesselRequirement};
use flotilla_store_memory::InMemoryBackend;
// #2543: scope waits use the latest deadline independently of batch ordering;
// hard errors and untimed limits remain substantive refusals for their subjects.
#[hegel::test]
fn observation_cooldown_is_deterministic(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Deadlines span expired, current and future resets; duplicates exercise the
    // lowest-subject tie break. Generated permutations, their reversals and fresh HashMaps vary order.
    let now = Utc.timestamp_opt(1800000000, 0).single().expect("now");
    let offsets: Vec<i64> = (0..tc.draw(gs::integers::<usize>().min_value(0).max_value(8)))
        .map(|_| tc.draw(gs::integers::<i64>().min_value(-2).max_value(2)))
        .collect();
    let timed = |number: u64, offset: Option<i64>| ObservationError::RateLimited {
        budget: "test".into(),
        retry_at: offset.map(|offset| now + chrono::Duration::seconds(offset)),
        detail: format!("subject-{number}"),
    };
    let expected = offsets
        .iter()
        .enumerate()
        .max_by_key(|(index, offset)| (**offset, std::cmp::Reverse(*index)))
        .map(|(index, offset)| timed(index as u64, Some(*offset)));
    for reverse in [false, true] {
        let mut entries: Vec<_> =
            offsets.iter().enumerate().map(|(index, offset)| (index as u64, Err(timed(index as u64, Some(*offset))))).collect();
        entries.push((100, Err(ObservationError::Failed("hard refusal".into()))));
        entries.push((101, Err(timed(101, None))));
        for index in 0..entries.len() {
            let other = tc.draw(gs::integers::<usize>().min_value(0).max_value(entries.len() - 1));
            entries.swap(index, other);
        }
        if reverse {
            entries.reverse();
        }
        let result = Ok(entries.into_iter().collect::<BoundObservations>());
        let actual = observation_rate_limit_error(&result);
        assert_eq!(actual, expected.as_ref());
        if let Some(error) = actual {
            assert_eq!(
                observation_during_cooldown(&result, 100, error).expect_err("hard refusal"),
                ObservationError::Failed("hard refusal".into())
            );
            assert_eq!(observation_during_cooldown(&result, 101, error).expect_err("untimed limit"), timed(101, None));
            for number in 0..offsets.len() as u64 {
                assert_eq!(observation_during_cooldown(&result, number, error).expect_err("scope cooldown"), *error);
            }
            assert_eq!(observation_during_cooldown(&result, 102, error).expect_err("new subject waits"), *error);
            let offset = *offsets.iter().max().expect("timed limits");
            assert_eq!(
                observation_cache_delay(error.retry_at(), now),
                if offset > 0 { Duration::from_secs(offset as u64) } else { OBSERVATION_CACHE_FALLBACK_DELAY }
            );
        }
    }
    assert!(observation_rate_limit_error(&Ok(BoundObservations::new())).is_none());
}

struct NoCooldownForgeReads;
#[async_trait]
impl ChangeRequestQueryPort for NoCooldownForgeReads {
    async fn discover_repository_change_request(
        &self,
        _namespace: &str,
        _repository: &RepositorySpec,
    ) -> Result<Arc<dyn ChangeRequestTracker>, String> {
        panic!("cooldown must prevent forge discovery and reads")
    }
}

// #2543: normal, fresh completion, and Landing/batch reads all respect the same
// cached scope cooldown, including new subjects and prior successful outcomes.
#[tokio::test(start_paused = true)]
async fn distinct_cooldowns_block_all_observation_reads() {
    let source =
        ProviderChangeRequestObservationSource::new(ResourceBackend::new(InMemoryBackend::default()), Arc::new(NoCooldownForgeReads));
    let now = Utc::now();
    let timed = |seconds| ObservationError::RateLimited {
        budget: "test".into(),
        retry_at: Some(now + chrono::Duration::seconds(seconds)),
        detail: format!("reset-{seconds}"),
    };
    let controlling = timed(60);
    let successful = flotilla_resources::ChangeRequestStatus {
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        state: Default::default(),
        head_sha: Default::default(),
        checks: Default::default(),
        review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Default::default() },
        mergeable: Default::default(),
    };
    let result = Ok([
        (1, Err(timed(10))),
        (2, Err(controlling.clone())),
        (3, Ok(successful)),
        (4, Err(ObservationError::Failed("hard refusal".into()))),
    ]
    .into_iter()
    .collect());
    let expires_at = tokio::time::Instant::now()
        + observation_cache_delay(observation_rate_limit_error(&result).and_then(ObservationError::retry_at), now);
    source.cache.lock().await.insert(
        ("flotilla".into(), "github.com".into(), "team/repo".into()),
        Arc::new(Mutex::new(Some(
            CachedObservation::builder()
                .expires_at(expires_at)
                .queried([1, 2, 3, 4].into_iter().collect())
                .next_history_start(0)
                .result(result)
                .build(),
        ))),
    );
    for seconds in [0, 11, 48] {
        tokio::time::advance(Duration::from_secs(seconds)).await;
        for number in [1, 2, 3, 4, 5] {
            let subject = ChangeRequestKey::new(
                "flotilla",
                flotilla_resources::address::stored_change_request_address("github.com", "team/repo", number, &[]).expect("address"),
            )
            .expect("key");
            let expected = if number == 4 { ObservationError::Failed("hard refusal".into()) } else { controlling.clone() };
            assert_eq!(source.observe(&subject).await.expect_err("ordinary read waits"), expected);
            assert_eq!(source.observe_for_completion(&subject).await.expect_err("completion waits"), expected);
            assert_eq!(source.observe_group(std::slice::from_ref(&subject), &subject).await.expect_err("batch waits"), expected);
        }
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    let subject = ChangeRequestKey::new(
        "flotilla",
        flotilla_resources::address::stored_change_request_address("github.com", "team/repo", 1, &[]).expect("address"),
    )
    .expect("key");
    let expired = source.observe_for_completion(&subject).await.expect_err("no repository after cache expires");
    assert!(matches!(expired, ObservationError::Failed(_)), "cache expires at controlling deadline");
}

// UTC-to-monotonic conversion must preserve a future header deadline and keep
// expired/zero deadlines from causing a burst of concurrent cache misses.
#[tokio::test(start_paused = true)]
async fn observation_cache_deadline_crosses_both_clocks() {
    let wall = chrono::DateTime::parse_from_rfc3339("2026-10-03T12:00:00Z").expect("fixed UTC timestamp").with_timezone(&chrono::Utc);
    let retry = wall + chrono::Duration::seconds(60);
    let expires = tokio::time::Instant::now() + observation_cache_delay(Some(retry), wall);
    tokio::time::advance(std::time::Duration::from_secs(59)).await;
    assert!(tokio::time::Instant::now() < expires);
    assert_eq!(observation_cache_delay(Some(retry), wall + chrono::Duration::seconds(59)), std::time::Duration::from_secs(1));
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert_eq!(tokio::time::Instant::now(), expires);
    for now in [retry, retry + chrono::Duration::seconds(1)] {
        assert_eq!(observation_cache_delay(Some(retry), now), std::time::Duration::from_secs(9));
    }
}

// An issue key names its installation by forge id, whichever stored spelling
// it came from; its issue source is that Forge's HTTPS root, which reads
// back to the same address. A host no Forge declares makes no key (ADR 0066 §3).
#[test]
fn standalone_issue_source_lookup_round_trips_installation_identity() {
    use flotilla_resources::address::stored_issue_address;

    let lab = flotilla_resources::ForgeSpec::builder()
        .forge_id("lab".into())
        .kind(flotilla_resources::ForgeKind::Forgejo)
        .hosts(BTreeSet::from(["forgejo.example".into()]))
        .https_url("https://forgejo.example/lab".into())
        .git_ssh_host("forgejo.example".into())
        .build();
    let mut stage = lab.clone();
    stage.forge_id = "stage".into();
    stage.https_url = "https://forgejo.example/stage".into();
    let forges = [lab, stage];
    for (service, forge, expected) in [
        ("lab", "lab", "https://forgejo.example/lab"),
        ("stage", "stage", "https://forgejo.example/stage"),
        ("forgejo.example%2flab", "lab", "https://forgejo.example/lab"),
        ("forgejo.example%2fstage", "stage", "https://forgejo.example/stage"),
    ] {
        let address = stored_issue_address(service, "Team/Repo", 12, &forges).expect("address");
        let subject = crate::issue_observer::IssueKey::new("flotilla", address).expect("key");
        assert_eq!(subject.forge_id(), forge, "{service}");
        let source = issue_source_for_subject(&subject, &forges).expect("source");
        assert_eq!(source.service, expected);
        let reference = flotilla_protocol::IssueRef { source, id: "12".into() };
        let address = flotilla_resources::address::issue_reference_address(&reference, &forges).expect("round trip");
        assert_eq!(&address, subject.address(), "{service}");
    }
    for undeclared in ["forgejo.example%2flab", "http%3a%2f%2fforgejo.example%2flab", "host%3aforgejo"] {
        assert!(stored_issue_address(undeclared, "Team/Repo", 12, &[]).is_err(), "{undeclared}");
    }
    let orphaned =
        crate::issue_observer::IssueKey::new("flotilla", stored_issue_address("lab", "Team/Repo", 12, &[]).expect("address")).expect("key");
    assert!(issue_source_for_subject(&orphaned, &[]).expect_err("missing Forge").contains("no Forge declaration"));
}

// The issue source of a declared non-GitHub Forge is its declared HTTPS root,
// not a URL guessed from its forge id or host; public GitHub needs no Forge.
#[test]
fn issue_source_is_the_declared_forge_https_root() {
    use flotilla_resources::address::stored_issue_address;

    let lab = flotilla_resources::ForgeSpec::builder()
        .forge_id("lab".into())
        .kind(flotilla_resources::ForgeKind::Forgejo)
        .hosts(BTreeSet::from(["git.lab.example".into()]))
        .https_url("https://code.lab.example/root".into())
        .git_ssh_host("git.lab.example".into())
        .build();
    let forges = [lab];
    let subject =
        crate::issue_observer::IssueKey::new("flotilla", stored_issue_address("lab", "team/repo", 3, &forges).expect("forge-id address"))
            .expect("key");
    let source = issue_source_for_subject(&subject, &forges).expect("source");
    assert_eq!(source.service, "https://code.lab.example/root");
    assert_eq!(source.scope, "team/repo");

    let github = crate::issue_observer::IssueKey::new(
        "flotilla",
        stored_issue_address("github.com", "Flotilla-Org/Flotilla", 3, &[]).expect("github address"),
    )
    .expect("key");
    assert_eq!(issue_source_for_subject(&github, &forges).expect("github source").service, "https://github.com");
}

#[test]
fn bound_change_request_identity_uses_matching_declared_or_discovered_subject() {
    let requested = ChangeRequestKey::new(
        "flotilla",
        flotilla_resources::address::stored_change_request_address("github.com", "team/repo", 42, &[]).expect("address"),
    )
    .expect("key");
    let subject = flotilla_protocol::Subject {
        kind: flotilla_protocol::SubjectKind::ChangeRequest,
        source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "team/repo".into() },
        id: "42".into(),
    };
    let spec = ConvoySpec::builder().workflow_ref("review".to_string()).build();
    let mut status = ConvoyStatus::default();
    status.discover_subject(
        subject.clone(),
        flotilla_protocol::Relationship::Produces,
        flotilla_resources::SubjectDiscoverySource::Claim,
        Utc::now(),
    );
    status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
        cascade: None,
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels: vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(Vec::new())
            .credential_refs(BTreeSet::from(["github-crew-pr".to_string()]))
            .build()],
    });
    let mut convoy = ResourceObject::<ResourceConvoy> {
        metadata: ObjectMeta {
            name: "review-convoy".to_string(),
            namespace: "flotilla".to_string(),
            resource_version: "1".to_string(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: Utc::now(),
            merge: None,
        },
        spec,
        status: Some(status),
    };
    let bound = convoy_change_request_credential_refs(&convoy, &requested, &[]).expect("active PR subjects");
    assert_eq!(bound.numbers, BTreeSet::from([42]));
    assert_eq!(bound.credentials_by_number[&42], BTreeSet::from(["github-crew-pr".to_string()]));
    let unrelated = ChangeRequestKey::new(
        "flotilla",
        flotilla_resources::address::stored_change_request_address("github.com", "team/other", 42, &[]).expect("address"),
    )
    .expect("key");
    assert!(convoy_change_request_credential_refs(&convoy, &unrelated, &[]).expect("other scope").numbers.is_empty());
    convoy.status.as_mut().expect("status").discover_subject(
        subject,
        flotilla_protocol::Relationship::Supersedes,
        flotilla_resources::SubjectDiscoverySource::Operator,
        Utc::now(),
    );
    assert!(convoy_change_request_credential_refs(&convoy, &requested, &[]).expect("superseded PR").numbers.is_empty());
}

#[test]
fn observation_service_matching_preserves_http_and_authority_port() {
    assert!(forge_service_matches("http://forgejo.local:3000", "forgejo.local:3000"));
    assert!(forge_service_matches("https://github.com/", "github.com"));
    assert!(!forge_service_matches("http://forgejo.local:3000", "forgejo.local:3001"));
}

// A Forge-identity repository's subjects carry its forge_ref, a DNS label that
// can never equal the web authority (#3206 found these were never observed).
#[test]
fn forge_identity_subjects_resolve_their_repository() {
    let forge = flotilla_resources::ForgeSpec {
        forge_id: "lab".into(),
        kind: flotilla_resources::ForgeKind::Forgejo,
        hosts: ["forgejo.lab.example".to_string()].into(),
        https_url: "https://forgejo.lab.example".into(),
        git_ssh_host: "forgejo.lab.example".into(),
    };
    let spec =
        RepositorySpec::remote("https://forgejo.lab.example/team/app.git").and_then(|spec| spec.on_forge(&forge)).expect("forge spec");
    assert!(repository_observes_subject(&spec, "lab", "team/app"));
    assert!(repository_observes_subject(&spec, "forgejo.lab.example", "team/app"), "web authority still matches");
    assert!(!repository_observes_subject(&spec, "lab", "team/other"));
    assert!(!repository_observes_subject(&spec, "elsewhere", "team/app"));
    let remote = RepositorySpec::remote("https://github.com/team/app.git").expect("remote spec");
    assert!(repository_observes_subject(&remote, "github.com", "team/app"));
    assert!(!repository_observes_subject(&remote, "lab", "team/app"));
}

// Forge boundary fake: record the bound batch without making network calls.
struct BoundBatchTracker(Arc<Mutex<Vec<Vec<u64>>>>);
#[async_trait]
impl ChangeRequestTracker for BoundBatchTracker {
    async fn open_in_browser(&self, _: &str) -> Result<(), String> {
        unreachable!()
    }
    async fn close_change_request(&self, _: &str) -> Result<(), String> {
        unreachable!()
    }
    async fn merge_change_request(&self, _: &str) -> Result<(), String> {
        unreachable!()
    }
    async fn list_merged_branch_names(&self, _: usize) -> Result<Vec<String>, String> {
        unreachable!()
    }
    async fn list_change_requests(&self, _: usize) -> Result<Vec<(String, flotilla_protocol::ChangeRequest)>, String> {
        unreachable!()
    }
    async fn get_change_request(&self, _: &str) -> Result<(String, flotilla_protocol::ChangeRequest), String> {
        unreachable!()
    }
    async fn observe_bound(
        &self,
        numbers: &[u64],
        _: &flotilla_change_request::CrewGithubLoginsByRequest,
    ) -> Result<BoundObservations, ObservationError> {
        self.0.lock().await.push(numbers.to_vec());
        Ok(numbers
            .iter()
            .map(|number| {
                (
                    *number,
                    Ok(flotilla_change_request::observation::parse_gh_observation(
                        r#"{"state":"OPEN","headRefOid":"head","statusCheckRollup":[]}"#,
                        Utc::now(),
                    )
                    .expect("status")),
                )
            })
            .collect())
    }
}
struct BoundBatchReads(Arc<Mutex<Vec<Vec<u64>>>>);
#[async_trait]
impl ChangeRequestQueryPort for BoundBatchReads {
    async fn discover_repository_change_request(&self, _: &str, _: &RepositorySpec) -> Result<Arc<dyn ChangeRequestTracker>, String> {
        Ok(Arc::new(BoundBatchTracker(self.0.clone())))
    }
}
// #3710: a selected merged record is absent from a sibling's bound batch and
// can be read directly without a provider call. Closed requests still enter it.
#[tokio::test]
async fn merged_record_excluded_from_bound_batch() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let repository = RepositorySpec::remote("https://github.com/org/repo.git").expect("repository");
    backend
        .using::<Repository>("ops")
        .create(&flotilla_resources::InputMeta::builder().name(repository.key().to_string()).build(), &repository)
        .await
        .expect("repository");
    let records = backend.using::<ChangeRequest>("ops");
    for (number, state) in [(41, ObservedChangeRequestState::Merged), (43, ObservedChangeRequestState::Closed)] {
        let spec = flotilla_resources::ChangeRequestSpec::builder()
            .service("github.com".into())
            .scope("org/repo".into())
            .number(number)
            .observing_authority("node".into())
            .build();
        let record =
            records.create(&flotilla_resources::InputMeta::builder().name(format!("pr-{number}")).build(), &spec).await.expect("record");
        records
            .update_status(
                &record.metadata.name,
                &record.metadata.resource_version,
                &flotilla_resources::ChangeRequestStatus {
                    state: flotilla_resources::Observation::known(state, Utc::now()),
                    ..flotilla_change_request::observation::parse_gh_observation(
                        r#"{"state":"OPEN","headRefOid":"head","statusCheckRollup":[]}"#,
                        Utc::now(),
                    )
                    .expect("status")
                },
            )
            .await
            .expect("state");
    }
    let batches = Arc::new(Mutex::new(Vec::new()));
    let source = ProviderChangeRequestObservationSource::new(backend, Arc::new(BoundBatchReads(batches.clone())));
    let subjects = [41, 42, 43].map(|number| {
        ChangeRequestKey::new(
            "ops",
            flotilla_resources::address::stored_change_request_address("github.com", "org/repo", number, &[]).expect("address"),
        )
        .expect("subject")
    });
    source.observe_group(&subjects, &subjects[1]).await.expect("sibling read");
    assert_eq!(*batches.lock().await, vec![vec![42, 43]]);
    assert_eq!(source.observe(&subjects[0]).await.expect("merged record").state.value, Some(ObservedChangeRequestState::Merged));
    assert_eq!(batches.lock().await.len(), 1);
}
