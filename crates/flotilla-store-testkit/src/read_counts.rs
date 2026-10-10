use flotilla_store::{InMemoryBackend, ReadObserver};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Debug, Default)]
struct ReadCounts {
    objects: Mutex<BTreeMap<String, usize>>,
    calls: Mutex<BTreeMap<String, usize>>,
}

impl ReadObserver for ReadCounts {
    fn read(&self, kind: &str, count: usize) {
        *self.objects.lock().expect("read counts").entry(kind.into()).or_default() += count;
        *self.calls.lock().expect("read calls").entry(kind.into()).or_default() += 1;
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub trait ReadCountsBackendExt {
    fn with_read_counts(self) -> Self;
    /// Number of objects decoded by store reads.
    fn read_counts(&self) -> BTreeMap<String, usize>;
    /// Number of store read operations, including reads returning no objects.
    fn read_calls(&self) -> BTreeMap<String, usize>;
}
impl ReadCountsBackendExt for InMemoryBackend {
    fn with_read_counts(self) -> Self {
        self.with_read_observer(Arc::new(ReadCounts::default()))
    }
    fn read_counts(&self) -> BTreeMap<String, usize> {
        self.read_observer()
            .and_then(|observer| observer.as_any().downcast_ref::<ReadCounts>())
            .map(|counts| counts.objects.lock().expect("read counts").clone())
            .unwrap_or_default()
    }
    fn read_calls(&self) -> BTreeMap<String, usize> {
        self.read_observer()
            .and_then(|observer| observer.as_any().downcast_ref::<ReadCounts>())
            .map(|counts| counts.calls.lock().expect("read calls").clone())
            .unwrap_or_default()
    }
}
