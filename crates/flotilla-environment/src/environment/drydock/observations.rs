//! One resumable stream per provider; incomplete snapshots never assert absence.
use super::*;
use futures::StreamExt;
type State = Arc<RwLock<Option<ObservationState>>>;
type States = Arc<RwLock<HashMap<String, std::sync::Weak<RwLock<Option<ObservationState>>>>>>;
pub(super) struct Hub {
    states: States,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Hub {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[derive(Clone)]
pub(super) struct Watch {
    pub state: State,
    _hub: Arc<Hub>,
}
impl Hub {
    pub fn watch(self: &Arc<Self>, backing: String) -> Watch {
        let mut states = self.states.write().expect("observation states");
        states.retain(|_, state| state.strong_count() > 0);
        let state = states.get(&backing).and_then(std::sync::Weak::upgrade).unwrap_or_else(|| Arc::new(RwLock::new(None)));
        states.insert(backing, Arc::downgrade(&state));
        Watch { state, _hub: self.clone() }
    }
    pub fn start(api: Arc<dyn ProviderApi>, provider: String) -> Arc<Self> {
        let states = Arc::new(RwLock::new(HashMap::<String, std::sync::Weak<RwLock<Option<ObservationState>>>>::new()));
        let updated = states.clone();
        let task = tokio::spawn(async move {
            let mut cursor = None;
            loop {
                if let Ok(mut records) = api.observe(cursor.as_deref()).await {
                    while let Some(Ok(record)) = records.next().await {
                        let states = updated.read().expect("observation states");
                        match record {
                            wire::ObserveRecord::Event { event } => {
                                cursor = Some(event.cursor);
                                if event.provider == provider {
                                    if let Some(state) = states.get(&event.id).and_then(std::sync::Weak::upgrade) {
                                        *state.write().expect("observation lock") = Some(event_observation(event.event));
                                    }
                                }
                            }
                            wire::ObserveRecord::Snapshot { cursor: next, environments, unavailable_providers } => {
                                cursor = Some(next);
                                for (backing, state) in states.iter() {
                                    if let Some(state) = state.upgrade() {
                                        let next = snapshot_state(&provider, backing, &environments, &unavailable_providers);
                                        if let Some(next) = next {
                                            *state.write().expect("observation lock") = Some(next);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                for state in updated.read().expect("observation states").values().filter_map(std::sync::Weak::upgrade) {
                    let mut current = state.write().expect("observation lock");
                    // A definitive terminal event remains useful across an API
                    // outage. Other states fall back to inspect while resuming.
                    if !matches!(*current, Some(ObservationState::Gone { .. })) {
                        *current = None;
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        Arc::new(Self { states, task })
    }
}
fn snapshot_state(
    provider: &str,
    backing: &str,
    environments: &[wire::Environment],
    unavailable: &BTreeMap<String, wire::ProtocolError>,
) -> Option<ObservationState> {
    if unavailable.contains_key(provider) {
        return None;
    }
    Some(
        environments
            .iter()
            .find(|e| e.provider == provider && e.id == backing)
            .map(|e| observation(e.state.clone()))
            .unwrap_or(ObservationState::Gone { cause: GoneCause::MissingOnList }),
    )
}
fn event_observation(event: wire::EventKind) -> ObservationState {
    match event {
        wire::EventKind::Running { .. } | wire::EventKind::Resumed => ObservationState::Running,
        wire::EventKind::Stopped { cause } | wire::EventKind::Paused { cause } => observation(wire::Observation::Stopped { cause }),
        wire::EventKind::Gone { cause } => observation(wire::Observation::Gone { cause }),
        wire::EventKind::Created => ObservationState::Unknown {
            reason: UnknownReason::ProviderApiError { message: "environment created; awaiting running".into() },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::drydock::client::{Error, WireResponse};
    struct Api {
        cursors: std::sync::Mutex<Vec<Option<String>>>,
        streams: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<::drydock::api::ObservationStream>>,
    }
    #[async_trait]
    impl ProviderApi for Api {
        async fn hello(&self) -> Result<wire::Hello, Error> {
            Err(Error::Transport)
        }
        async fn observe(&self, since: Option<&str>) -> Result<::drydock::api::ObservationStream, Error> {
            self.cursors.lock().unwrap().push(since.map(str::to_owned));
            self.streams.lock().await.recv().await.ok_or(Error::Transport)
        }
        async fn request(
            &self,
            _: http::Method,
            _: &[&str],
            _: &[(&str, &str)],
            _: Option<&serde_json::Value>,
        ) -> Result<WireResponse, Error> {
            Err(Error::Transport)
        }
        async fn relay(&self, _: &str, _: &str) -> Result<rigging_stream::ByteStream, Error> {
            Err(Error::Transport)
        }
        async fn exec(&self, _: &str, _: &str, _: &wire::ExecRequest) -> Result<rigging_stream::ByteStream, Error> {
            Err(Error::Transport)
        }
    }
    async fn until(predicate: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(4), async {
            while !predicate() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    // A provider owns one stream across inventory handles, resumes its opaque
    // cursor after disconnect, and never turns unavailable inventory into Gone.
    #[tokio::test]
    async fn shared_stream_resumes_and_preserves_terminal_evidence() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let api = Arc::new(Api { cursors: Default::default(), streams: tokio::sync::Mutex::new(rx) });
        let hub = Hub::start(api.clone(), "p".into());
        let first = hub.watch("one".into());
        let second = hub.watch("two".into());
        let again = hub.watch("one".into());
        assert!(Arc::ptr_eq(&first.state, &again.state));
        let record = wire::ObserveRecord::Event {
            event: wire::Event {
                cursor: "terminal-cursor".into(),
                id: "one".into(),
                provider: "p".into(),
                label: None,
                timestamp: serde_json::from_value(serde_json::json!("2026-10-22T00:00:00Z")).unwrap(),
                timestamp_source: wire::TimestampSource::Provider,
                event: wire::EventKind::Gone { cause: wire::GoneCause::LifetimeCapReached },
            },
        };
        tx.send(Box::pin(futures::stream::iter([Ok(record), Err(Error::Transport)])) as ::drydock::api::ObservationStream).await.unwrap();
        until(|| matches!(*first.state.read().unwrap(), Some(ObservationState::Gone { cause: GoneCause::LifetimeCapReached }))).await;
        until(|| api.cursors.lock().unwrap().len() == 2).await;
        assert_eq!(*api.cursors.lock().unwrap(), vec![None, Some("terminal-cursor".into())]);
        assert!(second.state.read().unwrap().is_none());
        assert!(snapshot_state("p", "one", &[], &BTreeMap::new()).is_some());
        let unavailable = BTreeMap::from([("p".into(), wire::ProtocolError::NotFound { reason: "offline".into() })]);
        assert!(snapshot_state("p", "one", &[], &unavailable).is_none());
        let snapshot = wire::ObserveRecord::Snapshot { cursor: "gap".into(), environments: vec![], unavailable_providers: unavailable };
        let resumed = wire::ObserveRecord::Event {
            event: wire::Event {
                cursor: "running".into(),
                id: "two".into(),
                provider: "p".into(),
                label: None,
                timestamp: serde_json::from_value(serde_json::json!("2026-10-22T00:00:00Z")).unwrap(),
                timestamp_source: wire::TimestampSource::Provider,
                event: wire::EventKind::Resumed,
            },
        };
        tx.send(Box::pin(futures::stream::iter([Ok(snapshot), Ok(resumed)]).chain(futures::stream::pending()))
            as ::drydock::api::ObservationStream)
            .await
            .unwrap();
        until(|| matches!(*second.state.read().unwrap(), Some(ObservationState::Running))).await;

        assert!(matches!(*first.state.read().unwrap(), Some(ObservationState::Gone { .. })));
    }
}
