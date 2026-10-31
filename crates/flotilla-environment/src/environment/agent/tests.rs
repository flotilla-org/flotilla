use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

// Scenario: a reconcile reuses the digest without reading unchanged binaries;
// replacing a file with equal-length content invalidates that cache.
#[test]
fn layer_identity_cache_skips_reads_and_detects_atomic_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("cli");
    std::fs::write(&source, b"old").unwrap();
    let entries = vec![(source.clone(), PathBuf::from("/opt/flotilla/cli"), 0o755)];
    let reads = AtomicUsize::new(0);
    let read = |path: &Path| {
        reads.fetch_add(1, Ordering::SeqCst);
        std::fs::read(path)
    };
    let first = snapshot_layer(entries.clone(), vec![], None, read).unwrap().unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert!(snapshot_layer(entries.clone(), vec![], Some(&first.sources), read).unwrap().is_none());
    assert_eq!(reads.load(Ordering::SeqCst), 1, "unchanged layers never read binary bytes");
    let replacement = directory.path().join("replacement");
    std::fs::write(&replacement, b"new").unwrap();
    std::fs::rename(replacement, &source).unwrap();
    let changed = snapshot_layer(entries, vec![], Some(&first.sources), read).unwrap().unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(changed.files[0].2, b"new");
    assert_ne!(changed.sources, first.sources);
}
