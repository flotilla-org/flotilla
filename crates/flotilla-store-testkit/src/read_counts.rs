use flotilla_resources::{InMemoryBackend, ReadObserver};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Debug, Default)]
struct ReadCounts(Mutex<BTreeMap<String, usize>>);

impl ReadObserver for ReadCounts {
    fn read(&self, kind: &str, count: usize) {
        *self.0.lock().expect("read counts").entry(kind.into()).or_default() += count;
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub trait ReadCountsBackendExt {
    fn with_read_counts(self) -> Self;
    fn read_counts(&self) -> BTreeMap<String, usize>;
}
impl ReadCountsBackendExt for InMemoryBackend {
    fn with_read_counts(self) -> Self {
        self.with_read_observer(Arc::new(ReadCounts::default()))
    }
    fn read_counts(&self) -> BTreeMap<String, usize> {
        self.read_observer()
            .and_then(|observer| observer.as_any().downcast_ref::<ReadCounts>())
            .map(|counts| counts.0.lock().expect("read counts").clone())
            .unwrap_or_default()
    }
}
