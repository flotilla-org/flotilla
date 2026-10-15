#[cfg(test)]
pub(super) mod tests {
    use chrono::Utc;
    use flotilla_projections::dispatch_board::DispatchBoardCache;
    use flotilla_protocol::{DispatchBoardIssue, DispatchBoardPullRequest, DispatchBoardRepository, IssueSource, IssueState};
    use std::{sync::Arc, time::Duration};
    fn source(scope: &str) -> IssueSource {
        IssueSource { service: "https://github.com".into(), scope: scope.into() }
    }
    const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
    pub(crate) fn board(source: &IssueSource, count: usize) -> DispatchBoardRepository {
        DispatchBoardRepository {
            source: source.clone(),
            observed_at: Utc::now(),
            age_seconds: 0,
            refresh_error: None,
            issues: (0..count)
                .map(|id| {
                    DispatchBoardIssue::builder()
                        .id(id.to_string())
                        .title(format!("Issue {id}"))
                        .state(IssueState::Open)
                        .url(format!("https://github.com/{}/issues/{id}", source.scope))
                        .updated_at(Utc::now().to_rfc3339())
                        .labels(vec!["ready".into()])
                        .blocked_by(vec![])
                        .pull_requests(vec![])
                        .build()
                })
                .collect(),
            pull_requests: (0..count)
                .map(|id| {
                    DispatchBoardPullRequest::builder()
                        .id(id.to_string())
                        .url(format!("https://github.com/{}/pull/{id}", source.scope))
                        .state("open".into())
                        .ci("success".into())
                        .build()
                })
                .collect(),
        }
    } // The real board watchdog allows ForgeReads to finish its earlier deadline
      // and expose the last-good board with the observation error, rather than
      // racing both 300s deadlines and replacing it with a tracker timeout.
    #[tokio::test(start_paused = true)]
    async fn layered_deadlines_preserve_observation_timeout_and_recover() {
        use crate::forge_observation::ForgeReads;
        use flotilla_resources::Clock;
        use flotilla_store::ResourceBackend;
        use flotilla_store_memory::InMemoryBackend;
        use flotilla_store_testkit::VirtualClock;
        let cache = DispatchBoardCache::default();
        let source = source("org/shared");
        let backend = ResourceBackend::new(InMemoryBackend::default()).with_local_root(flotilla_protocol::NodeId::new("owner"));
        let clock = Arc::new(VirtualClock::new(Utc::now()));
        let mut reads = ForgeReads::new(backend, "flotilla".into()).with_clock(clock.clone());
        reads.allow_stale = true;
        let snapshot = board(&source, 1);
        reads.read(&source, flotilla_resources::ForgeReadRequest::Board, || async { Ok(snapshot.clone()) }).await.unwrap();
        clock.advance(chrono::Duration::seconds(60));
        let slow_reads = reads.clone();
        let slow_source = source.clone();
        let (entered, started) = tokio::sync::oneshot::channel();
        cache
            .read(&source, move || async move {
                // Model request/resource-store setup before the load timer.
                // Equal layered deadlines cannot tolerate this small overhead.
                tokio::time::sleep(Duration::from_secs(1)).await;
                let result = slow_reads
                    .read(&slow_source, flotilla_resources::ForgeReadRequest::Board, || async {
                        entered.send(()).unwrap();
                        std::future::pending::<Result<DispatchBoardRepository, String>>().await
                    })
                    .await;
                let mut board = result?;
                board.refresh_error = slow_reads
                    .status(&flotilla_resources::forge_read_name(&slow_source, &flotilla_resources::ForgeReadRequest::Board))
                    .await?
                    .and_then(|status| status.error);
                Ok(board)
            })
            .await
            .expect_err("cold cache");
        started.await.unwrap();
        tokio::time::advance(crate::forge_observation::LOAD_TIMEOUT - Duration::from_secs(1)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }
        let cached = cache.read(&source, || async { panic!("cached timeout") }).await.unwrap();
        assert_eq!(cached.issues, snapshot.issues);
        assert_eq!(cached.refresh_error.as_deref(), Some("forge observation timed out"));
        clock.advance(chrono::Duration::seconds(60));
        tokio::time::advance(REFRESH_INTERVAL).await;
        let recovered = snapshot.clone();
        let refresh_source = source.clone();
        cache
            .read(&source, move || async move {
                reads.read(&refresh_source, flotilla_resources::ForgeReadRequest::Board, || async { Ok(recovered) }).await
            })
            .await
            .unwrap();
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }
        let cached = cache.read(&source, || async { panic!("cached recovery") }).await.unwrap();
        assert!(cached.refresh_error.is_none());
        assert_eq!(cached.issues, snapshot.issues);
        assert!(clock.now() > snapshot.observed_at);
    }
}
