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
                                    if let (Some(state), Some(observed)) =
                                        (states.get(&event.id).and_then(std::sync::Weak::upgrade), event_observation(event.event))
                                    {
                                        *state.write().expect("observation lock") = Some(observed);
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
    unavailable: &BTreeMap<String, serde_json::Value>,
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
/// The vessel's state an event reports; an exposure's event reports none.
fn event_observation(event: wire::EventKind) -> Option<ObservationState> {
    Some(match event {
        wire::EventKind::Running { .. } | wire::EventKind::Resumed => ObservationState::Running,
        wire::EventKind::Stopped { cause } | wire::EventKind::Paused { cause } => observation(wire::Observation::Stopped { cause }),
        wire::EventKind::Gone { cause } => observation(wire::Observation::Gone { cause }),
        wire::EventKind::Created => ObservationState::Unknown {
            reason: UnknownReason::ProviderApiError { message: "environment created; awaiting running".into() },
        },
        wire::EventKind::Exposure { .. } => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn until(predicate: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(4), async {
            while !predicate() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    fn event(cursor: &str, id: &str, event: wire::EventKind) -> wire::ObserveRecord {
        wire::ObserveRecord::Event { event: wire::Event { cursor: cursor.into(), id: id.into(), provider: "p".into(), event } }
    }
    // A provider owns one stream across inventory handles, resumes its opaque
    // cursor after disconnect, and never turns unavailable inventory into Gone.
    #[tokio::test]
    async fn shared_stream_resumes_and_preserves_terminal_evidence() {
        let (api, tx) = super::super::memory::MemoryApi::new("p", PathBuf::new());
        let hub = Hub::start(api.clone(), "p".into());
        let first = hub.watch("one".into());
        let second = hub.watch("two".into());
        let again = hub.watch("one".into());
        assert!(Arc::ptr_eq(&first.state, &again.state));
        let record = event("terminal-cursor", "one", wire::EventKind::Gone { cause: wire::GoneCause::LifetimeCapReached });
        tx.send(Box::pin(futures::stream::iter([Ok(record), Err(ApiError::Other("disconnected".into()))])) as wire::ObservationStream)
            .await
            .unwrap();
        until(|| matches!(*first.state.read().unwrap(), Some(ObservationState::Gone { cause: GoneCause::LifetimeCapReached }))).await;
        until(|| api.cursors.lock().unwrap().len() == 2).await;
        assert_eq!(*api.cursors.lock().unwrap(), vec![None, Some("terminal-cursor".into())]);
        assert!(second.state.read().unwrap().is_none());
        assert!(snapshot_state("p", "one", &[], &BTreeMap::new()).is_some());
        let unavailable = BTreeMap::from([("p".into(), serde_json::json!({"kind": "not_found", "reason": "offline"}))]);
        assert!(snapshot_state("p", "one", &[], &unavailable).is_none());
        let snapshot = wire::ObserveRecord::Snapshot { cursor: "gap".into(), environments: vec![], unavailable_providers: unavailable };
        let resumed = event("running", "two", wire::EventKind::Resumed);
        tx.send(Box::pin(futures::stream::iter([Ok(snapshot), Ok(resumed)]).chain(futures::stream::pending())) as wire::ObservationStream)
            .await
            .unwrap();
        until(|| matches!(*second.state.read().unwrap(), Some(ObservationState::Running))).await;

        assert!(matches!(*first.state.read().unwrap(), Some(ObservationState::Gone { .. })));
    }
}
