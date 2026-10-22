//! Resumable observations; incomplete snapshots never assert that a backing is gone.
use super::*;
use futures::StreamExt;
pub(super) struct Watch {
    pub state: Arc<RwLock<Option<ObservationState>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Watch {
    pub fn start(api: Arc<dyn ProviderApi>, provider: String, backing: String) -> Self {
        let state = Arc::new(RwLock::new(None));
        let updated = state.clone();
        let task = tokio::spawn(async move {
            let mut cursor = None;
            loop {
                if let Ok(mut records) = api.observe(cursor.as_deref()).await {
                    while let Some(Ok(record)) = records.next().await {
                        let next = match record {
                            wire::ObserveRecord::Event { event } => {
                                cursor = Some(event.cursor.clone());
                                (event.provider == provider && event.id == backing).then(|| event_observation(event.event))
                            }
                            wire::ObserveRecord::Snapshot { cursor: next, environments, unavailable_providers } => {
                                cursor = Some(next);
                                if unavailable_providers.contains_key(&provider) {
                                    None
                                } else {
                                    Some(
                                        environments
                                            .into_iter()
                                            .find(|e| e.provider == provider && e.id == backing)
                                            .map(|e| observation(e.state))
                                            .unwrap_or(ObservationState::Gone { cause: GoneCause::MissingOnList }),
                                    )
                                }
                            }
                        };
                        if let Some(next) = next {
                            *updated.write().expect("observation lock") = Some(next);
                        }
                    }
                }
                {
                    let mut current = updated.write().expect("observation lock");
                    if !matches!(*current, Some(ObservationState::Gone { .. })) {
                        *current = Some(ObservationState::Unknown { reason: UnknownReason::HostUnreachable });
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        Self { state, task }
    }
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
