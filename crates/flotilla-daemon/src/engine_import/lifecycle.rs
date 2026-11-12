//! The lifecycle importer: Vessel, TerminalSession, Environment, ImageBuild, Clone and Checkout. A
//! placed Vessel is its work's first launch, a lease granted on its pinned host; one never placed
//! is noted, not stored.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_engine_store::{Import, ImportTarget};
use flotilla_provisioning::engine::import::{
    import_checkouts, import_clones, import_environments, import_image_builds, import_terminal_sessions, import_vessels, ImportReport,
};
use flotilla_resources::{
    convoy::engine::engine_name, Checkout, Clone as CloneResource, Convoy, Environment, Host, ImageBuild, TerminalSession, Vessel,
    ACTUATOR_SOURCE_ROOT_ANNOTATION, CONVOY_LABEL,
};
use flotilla_schema::{Identity, Kind};
use flotilla_state_machines::{machines::convoy::Convoy as EngineConvoy, Resource};

use super::{Carried, Source};

pub(super) const KINDS: [&str; 4] = ["environments", "imagebuilds", "clones", "checkouts"];

/// Vessels import first, each as its work's first launch, so the environments and terminal
/// sessions held for an old vessel are held for the launch it became: a launch's number is its
/// home's sequence when it is written. A vessel keeps the old convoy it serves, which its home's
/// vessel requester reads.
pub(super) fn vessels(source: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    for namespace in source.namespaces(&["vessels"]) {
        let vessels: Vec<_> = source
            .owned::<Vessel>("vessels", Some(namespace), "Vessel", target, report)
            .into_iter()
            .map(|record| (record.object, record.home))
            .collect();
        carried.vessels.extend(import_vessels(&vessels, target, report));
    }
}

/// TerminalSessions import before the messages delivered to them, so each delivery names the
/// launch its session became: a launch's number is its home's sequence when it is written.
pub(super) fn terminal_sessions(source: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    for namespace in source.namespaces(&["terminalsessions"]) {
        let sessions: Vec<_> = source
            .owned::<TerminalSession>("terminalsessions", Some(namespace), "TerminalSession", target, report)
            .into_iter()
            .map(|record| (record.object, record.home))
            .collect();
        carried.launches.extend(import_terminal_sessions(&sessions, &carried.vessels, target, report));
    }
}

pub(super) fn lifecycles(source: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    // A namespace with only convoys still notes how its standing generations fold.
    for namespace in source.namespaces(&["environments", "imagebuilds", "clones", "checkouts", "convoys"]) {
        namespace_lifecycles(source, namespace, target, carried, report);
    }
}

/// The home of each host's daemon, from the source's Host records: a Host is homed with the
/// daemon that describes it, so a host held at exactly one home is that home's.
fn host_homes(source: &Source) -> BTreeMap<String, BTreeSet<String>> {
    let mut homes = BTreeMap::<String, BTreeSet<String>>::new();
    for host in source.context::<Host>("hosts", None) {
        homes.entry(host.object.metadata.name).or_default().insert(host.home);
    }
    homes
}

fn host_home(homes: &BTreeMap<String, BTreeSet<String>>, host: &str) -> Result<String, String> {
    match homes.get(host).map(|homes| homes.iter().collect::<Vec<_>>()).as_deref() {
        Some([home]) => Ok((*home).clone()),
        None | Some([]) => Err(format!("builder host `{host}` has no Host record in the source")),
        Some(several) => Err(format!(
            "builder host `{host}` is held at several homes: {}",
            several.iter().map(|home| home.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

fn namespace_lifecycles(source: &Source, namespace: &str, target: &mut Import, carried: &Carried, report: &mut ImportReport) {
    let namespace = Some(namespace);
    let hosts = host_homes(source);
    // Each build's builder host, from any home's copy of the build, for the environments that
    // name it.
    let builders: BTreeMap<String, String> = source
        .context::<ImageBuild>("imagebuilds", namespace)
        .into_iter()
        .filter_map(|build| Some((build.object.metadata.name.clone(), build.object.spec.current()?.host_ref.clone())))
        .collect();
    let build_home = |build: &str| {
        let host = builders.get(build).ok_or_else(|| format!("image build `{build}` has no record in the source"))?;
        host_home(&hosts, host)
    };
    let environments: Vec<_> = source
        .owned::<Environment>("environments", namespace, "Environment", target, report)
        .into_iter()
        .map(|record| (record.object, record.home))
        .collect();
    import_environments(&environments, &carried.vessels, &build_home, target, report);
    let builds: Vec<_> = source
        .owned::<ImageBuild>("imagebuilds", namespace, "ImageBuild", target, report)
        .into_iter()
        .map(|record| (record.object, record.home))
        .collect();
    import_image_builds(&builds, &|host: &str| host_home(&hosts, host), target, report);
    let clones: Vec<_> = source
        .owned::<CloneResource>("clones", namespace, "Clone", target, report)
        .into_iter()
        .map(|record| (record.object, record.home))
        .collect();
    // The launch each clone imported as, by home and old name, for the worktrees that name it.
    let clone_launches = import_clones(&clones, target, report);
    let convoys = source.context::<Convoy>("convoys", namespace);
    let aliases: BTreeMap<_, _> = convoys
        .iter()
        .filter_map(|convoy| {
            engine_name(&convoy.object).ok().map(|name| ((convoy.home.clone(), convoy.object.metadata.name.clone()), name))
        })
        .collect();
    // Each checkout is held for the launch its convoy's record imported as, at the convoy's home,
    // created under that launch's version. One whose convoy was not imported (ended, or staying on
    // the old store) is not: nothing would release it.
    let convoy_code = <Resource<EngineConvoy> as Kind>::CODE;
    for record in
        source.owned::<Checkout>("checkouts", namespace, "Checkout", target, report).into_iter().map(|record| (record.object, record.home))
    {
        // Another home's checkout is that home's to import or drop.
        if !target.imports(&record.1) {
            continue;
        }
        let convoy_home = record.0.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION).unwrap_or(&record.1).clone();
        let holder = |old: &str| {
            let (launch, version) = carried.convoys.get(&(convoy_home.clone(), old.to_owned()))?;
            Some((Identity { kind: convoy_code, name: launch.clone(), home: convoy_home.clone() }, *version))
        };
        if record.0.metadata.labels.get(CONVOY_LABEL).is_some_and(|convoy| holder(convoy).is_none()) {
            report.drop_rows("Checkout", "held for a convoy that was not imported", 1);
            continue;
        }
        import_checkouts(std::slice::from_ref(&record), &holder, &clone_launches, target, report);
    }
    let standing: Vec<_> = aliases.iter().filter(|((home, old), stable)| target.imports(home) && old != *stable).collect();
    if !standing.is_empty() {
        let identities: std::collections::BTreeSet<_> = standing.iter().map(|((home, _), stable)| (home, stable)).collect();
        report.notes.push(format!(
            "{} standing convoy generations map to {} stable identities ({} folded)",
            standing.len(),
            identities.len(),
            standing.len() - identities.len()
        ));
    }
}
