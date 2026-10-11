use flotilla_store::{ReadObserver, ResourceBackend};
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
    fn with_read_counts(self) -> ResourceBackend
    where
        Self: Sized;
    /// Number of objects decoded by store reads.
    fn read_counts(&self) -> BTreeMap<String, usize>;
    /// Number of store read operations, including reads returning no objects.
    fn read_calls(&self) -> BTreeMap<String, usize>;
}
fn counts(observer: Option<&dyn ReadObserver>, calls: bool) -> BTreeMap<String, usize> {
    observer
        .and_then(|observer| observer.as_any().downcast_ref::<ReadCounts>())
        .map(
            |counts| {
                if calls {
                    counts.calls.lock().expect("read calls").clone()
                } else {
                    counts.objects.lock().expect("read counts").clone()
                }
            },
        )
        .unwrap_or_default()
}
impl ReadCountsBackendExt for ResourceBackend {
    fn with_read_counts(self) -> ResourceBackend
    where
        Self: Sized,
    {
        self.with_read_observer(Arc::new(ReadCounts::default()))
    }
    fn read_counts(&self) -> BTreeMap<String, usize> {
        counts(self.read_observer(), false)
    }
    fn read_calls(&self) -> BTreeMap<String, usize> {
        counts(self.read_observer(), true)
    }
}

macro_rules! observed_backend {
    ($backend:ty) => {
        impl ReadCountsBackendExt for $backend {
            fn with_read_counts(self) -> ResourceBackend {
                ResourceBackend::new(self.with_read_observer(Arc::new(ReadCounts::default())))
            }
            fn read_counts(&self) -> BTreeMap<String, usize> {
                counts(self.read_observer(), false)
            }
            fn read_calls(&self) -> BTreeMap<String, usize> {
                counts(self.read_observer(), true)
            }
        }
    };
}
observed_backend!(flotilla_store_memory::InMemoryBackend);
observed_backend!(flotilla_store_sqlite::SqliteBackend);
observed_backend!(flotilla_store_http::HttpBackend);
