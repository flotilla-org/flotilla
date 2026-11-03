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

/// Generation `generation` of a fleet tools tree at `tools`: its directory
/// named by a content digest, and `current` naming it.
fn publish_generation(tools: &Path, generation: usize) -> String {
    let name = format!("{:064x}", generation + 1);
    let bin = tools.join(&name).join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("flotilla"), format!("generation {generation}")).unwrap();
    let staged = tools.join(".current");
    let _ = std::fs::remove_file(&staged);
    std::os::unix::fs::symlink(&name, &staged).unwrap();
    std::fs::rename(staged, tools.join("current")).unwrap();
    name
}

// #4300: the vessel records the identity of the tools layer it holds, so a
// controller that did not push it (a restarted one: a fresh handle on the same
// agent) pushes only a layer that differs from that record. An unchanged layer
// uploads nothing; a new generation lands beside the old ones, which running
// processes may still use, and `current` names it. Generated: sequences of up
// to six pushes, each of generation 0, 1 or 2 (so repeats, upgrades and
// downgrades all occur), each by the same handle or by a fresh one.
#[hegel::test]
fn a_push_uploads_only_a_layer_the_vessel_does_not_record(tc: hegel::TestCase) {
    use super::super::{EnvironmentToolAsset, EnvironmentToolAssetAccess, EnvironmentToolAssetKind};
    use hegel::generators as gs;
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let steps: Vec<(usize, bool)> =
        (0..count).map(|_| (tc.draw(gs::integers::<usize>().min_value(0).max_value(2)), tc.draw(gs::booleans()))).collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let (source, guest) = (directory.path().join("source/tools"), directory.path().join("guest/tools"));
        let agent = memory::MemoryAgent::new();
        let fresh = || Arc::new(AgentRunner::new(agent.clone(), directory.path().join("attach")));
        let tools = vec![EnvironmentTool::new("flotilla", guest.join("current/bin/flotilla")).with_asset(EnvironmentToolAsset::new(
            &source,
            &guest,
            EnvironmentToolAssetKind::Directory,
            EnvironmentToolAssetAccess::ReadOnly,
            "the generation's tools",
        ))];
        let mut runner = fresh();
        let mut recorded = None;
        let mut pushed = std::collections::BTreeSet::new();
        for (generation, restart) in steps {
            if restart {
                runner = fresh();
            }
            let name = publish_generation(&source, generation);
            let before = agent.writes();
            runner.push_tools(&tools).await.unwrap();
            let uploaded = agent.writes() - before;
            assert_eq!(uploaded > 0, recorded != Some(generation), "generation {generation} after {recorded:?}: {uploaded} writes");
            assert_eq!(std::fs::read_link(guest.join("current")).unwrap(), Path::new(&name));
            assert_eq!(std::fs::read_to_string(guest.join("current/bin/flotilla")).unwrap(), format!("generation {generation}"));
            pushed.insert(name);
            for earlier in &pushed {
                assert!(guest.join(earlier).join("bin/flotilla").is_file(), "generation {earlier} stays for running processes");
            }
            recorded = Some(generation);
        }
        let _ = std::fs::remove_file(runner.step_record(&layer_step(&tools)).await.unwrap());
    });
}
