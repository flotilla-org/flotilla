//! The lifecycle importer: Environment, ImageBuild, Clone, Checkout, TerminalSession, Vessel and
//! ConvoyEnsure. A placed Vessel is a lease granted on its pinned host; one never placed is noted,
//! not stored. An Ensure is bound while the convoy it records is live.

use std::collections::BTreeMap;

use flotilla_core::primitives::ensures::import_ensures;
use flotilla_engine_store::{Import, ImportTarget};
use flotilla_provisioning::engine::{
    image_build::ImageBuildBinding,
    import::{import_checkouts, import_clones, import_environments, import_records, import_vessels, ImportRefusal, ImportReport},
    terminal_session::TerminalSessionBinding,
};
use flotilla_resources::{
    convoy::engine::engine_name, Checkout, Clone as CloneResource, Convoy, ConvoyEnsure, Environment, ImageBuild, TerminalSession, Vessel,
    ACTUATOR_SOURCE_ROOT_ANNOTATION, CONVOY_LABEL,
};
use flotilla_schema::{Identity, Kind};
use flotilla_state_machines::{machines::convoy::Convoy as EngineConvoy, Resource};

use super::{Carried, Source};

pub(super) const KINDS: [&str; 7] = ["environments", "imagebuilds", "clones", "checkouts", "terminalsessions", "vessels", "convoyensures"];

pub(super) fn lifecycles(source: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    for namespace in source.namespaces(&KINDS) {
        namespace_lifecycles(source, namespace, target, carried, report);
    }
}

fn namespace_lifecycles(source: &Source, namespace: &str, target: &mut Import, carried: &Carried, report: &mut ImportReport) {
    let namespace = Some(namespace);
    macro_rules! records {
        ($old:ty, $plural:literal, $kind:literal, $binding:ty) => {{
            let records: Vec<_> = source
                .owned::<$old>($plural, namespace, $kind, target, report)
                .into_iter()
                .map(|record| (record.object, record.home))
                .collect();
            import_records::<$binding>(&records, target, report);
        }};
    }
    let environments: Vec<_> = source
        .owned::<Environment>("environments", namespace, "Environment", target, report)
        .into_iter()
        .map(|record| (record.object, record.home))
        .collect();
    import_environments(&environments, target, report);
    records!(ImageBuild, "imagebuilds", "ImageBuild", ImageBuildBinding);
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
    // Each checkout is created under the engine Convoy its convoy imported as, at the convoy's
    // home. One whose convoy was not imported (ended, or staying on the old store) is not: nothing
    // would release it.
    let convoy_code = <Resource<EngineConvoy> as Kind>::CODE;
    for record in
        source.owned::<Checkout>("checkouts", namespace, "Checkout", target, report).into_iter().map(|record| (record.object, record.home))
    {
        let convoy_home = record.0.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION).unwrap_or(&record.1).clone();
        let holder = |old: &str| {
            let name = aliases.get(&(convoy_home.clone(), old.to_owned()))?;
            let version = carried.convoys.get(&(convoy_home.clone(), name.clone()))?;
            Some((Identity { kind: convoy_code, name: name.clone(), home: convoy_home.clone() }, *version))
        };
        if record.0.metadata.labels.get(CONVOY_LABEL).is_some_and(|convoy| holder(convoy).is_none()) {
            *report.dropped.entry(("Checkout", "held for a convoy that was not imported")).or_default() += 1;
            continue;
        }
        import_checkouts(std::slice::from_ref(&record), &holder, &clone_launches, target, report);
    }
    records!(TerminalSession, "terminalsessions", "TerminalSession", TerminalSessionBinding);
    let mut vessels: Vec<_> = source
        .owned::<Vessel>("vessels", namespace, "Vessel", target, report)
        .into_iter()
        .map(|vessel| (vessel.object, vessel.home))
        .collect();
    for (vessel, home) in &mut vessels {
        let convoy_home = vessel.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION).unwrap_or(home);
        if let Some(stable) = aliases.get(&(convoy_home.clone(), vessel.spec.convoy_ref.clone())) {
            vessel.spec.convoy_ref.clone_from(stable);
        } else {
            report.notes.push(format!(
                "Vessel `{}` records convoy `{}` from home `{convoy_home}`, which is not held or has no mappable identity: left unchanged",
                vessel.metadata.name, vessel.spec.convoy_ref
            ));
        }
    }
    import_vessels(&vessels, target, report);
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
    // Each convoy's liveness, and whether it has a status yet: one with none is a fresh convoy,
    // read as live, and an Ensure bound to it is noted.
    let live: BTreeMap<String, (bool, bool)> = convoys
        .into_iter()
        .map(|convoy| {
            let live = convoy.object.status.as_ref().is_none_or(|status| !status.phase.is_terminal());
            (convoy.object.metadata.name, (live, convoy.object.status.is_some()))
        })
        .collect();
    let mut ensures = Vec::new();
    for ensure in source.owned::<ConvoyEnsure>("convoyensures", namespace, "Ensure", target, report) {
        let (ensure, home) = (ensure.object, ensure.home);
        let convoy = ensure.status.as_ref().and_then(|status| status.convoy_ref.clone());
        let convoy_live = match convoy.as_ref().map(|convoy| (convoy, live.get(convoy))) {
            Some((_, Some((live, true)))) => *live,
            Some((convoy, Some((live, false)))) => {
                report.notes.push(format!(
                    "Ensure `{}` records convoy `{convoy}`, which has no status yet: read as live, imported bound",
                    ensure.metadata.name
                ));
                *live
            }
            Some((convoy, None)) => {
                // The safe direction: unfulfilled, so the first engine pass binds it again.
                report.notes.push(format!(
                    "Ensure `{}` records convoy `{convoy}`, which is not held (or did not decode): imported unfulfilled",
                    ensure.metadata.name
                ));
                false
            }
            None => false,
        };
        ensures.push((ensure, home, convoy_live));
    }
    let (counts, refused) = import_ensures(&ensures, target);
    for (label, count) in counts {
        *report.imported.entry(("Ensure", label)).or_default() += count;
    }
    report.refused.extend(refused.into_iter().map(|(record, reason)| ImportRefusal { kind: "Ensure", record, reason }));
}
