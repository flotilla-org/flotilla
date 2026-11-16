//! The lifecycle views name their home from the readings they serve, whichever kinds those
//! readings hold: here, environments alone, with no terminal sessions.

use std::sync::Arc;

use flotilla_provisioning_testkit::EnvironmentFixture;
use flotilla_resources::{engine_records::KindRecords, Environment, EnvironmentSpec, HostDirectEnvironmentSpec};
use flotilla_store::LifecycleViews;

#[tokio::test]
async fn views_of_readings_with_no_terminals_name_their_home() {
    let fixture = EnvironmentFixture::at("alpha");
    let views = Arc::new(LifecycleViews::new(fixture.reads.clone()));
    assert_eq!(KindRecords::<Environment>::home(&*views), "alpha");

    let spec = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: "alpha".into(), repo_default_dir: "/work".into() }),
        ..EnvironmentSpec::default()
    };
    fixture.request("workspace", &spec, None);
    let held = KindRecords::<Environment>::get(&*views, "workspace").await.expect("get").expect("the requested environment");
    assert_eq!(held.home, KindRecords::<Environment>::home(&*views), "this home's record is this home's");
}
