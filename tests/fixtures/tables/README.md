# Recorded `/tables` responses

`flotilla table`'s tests (`src/cli/table/tests.rs`) render these:

| File | Request |
|---|---|
| `convoys.ndjson` | `GET /tables/convoys/rows?namespace=flotilla&limit=10000` |
| `convoys.json` | `GET /tables/convoys` (the description a view file is checked against) |
| `listing.json` | `GET /tables` |

They were recorded on 2026-11-01 from the daemon's own router, in process, over six convoys seeded with fixed names, roles and phases (one without a status). Each view file under `views/` needs its table's description here as `<table>.json`.

## Re-recording

Add this test to `crates/flotilla-daemon/src/server/http/table_tests.rs`, which already has the `router` and `call` helpers. Run it with `cargo test -p flotilla-daemon --lib record_table_fixtures`, then delete it. Re-recording changes the creation timestamps, which the tests do not pin.

```rust
#[tokio::test]
async fn record_table_fixtures() {
    use flotilla_resources::{ConvoyPhase, ConvoyStatus};
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = backend.using::<Convoy>("flotilla");
    for (name, role, phase) in [
        ("convoy-0a1f", "table-pilot-4106", Some(ConvoyPhase::Active)),
        ("convoy-1b2e", "verbs-table-4197", Some(ConvoyPhase::Landed)),
        ("convoy-2c3d", "presentations-4207", Some(ConvoyPhase::Pending)),
        ("convoy-3d4c", "roll-r912", Some(ConvoyPhase::Failed)),
        ("convoy-4e5b", "trial-fleet-4171", Some(ConvoyPhase::Abandoned)),
        ("convoy-5f6a", "fresh", None),
    ] {
        let labels = BTreeMap::from([("flotilla.work/project".into(), "flotilla".into()), ("flotilla.work/role".into(), role.into())]);
        let meta = InputMeta::builder().name(name.into()).labels(labels).build();
        let created = convoys.create(&meta, &ConvoySpec::builder().workflow_ref("crew-pr".into()).build()).await.unwrap();
        if let Some(phase) = phase {
            let status = ConvoyStatus { phase, ..Default::default() };
            convoys.update_status(name, &created.metadata.resource_version, &status).await.unwrap();
        }
        // Distinct creation times, so the view's newest-first sort is the reverse of name order.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let router = router(backend);
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/tables");
    for (path, file) in [
        ("/tables/convoys/rows?namespace=flotilla&limit=10000", "convoys.ndjson"),
        ("/tables/convoys", "convoys.json"),
        ("/tables", "listing.json"),
    ] {
        let bytes = to_bytes(call(&router, path, false).await.into_body(), 8 * 1024 * 1024).await.unwrap();
        std::fs::write(format!("{dir}/{file}"), &bytes).unwrap();
    }
}
```
