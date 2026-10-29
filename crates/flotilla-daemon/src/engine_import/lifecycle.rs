//! The lifecycle importer: Environment, ImageBuild, Clone, Checkout, TerminalSession, Vessel and
//! ConvoyEnsure. A placed Vessel is a lease granted on its pinned host; one never placed is noted,
//! not stored. An Ensure is bound while the convoy it records is live.

use std::collections::BTreeMap;

use flotilla_core::primitives::ensures::import_ensures;
use flotilla_engine_store::{Import, ImportTarget};
use flotilla_provisioning::engine::{
    checkout::CheckoutBinding,
    clone::CloneBinding,
    environment::EnvironmentBinding,
    image_build::ImageBuildBinding,
    import::{import_records, import_vessels, ImportRefusal, ImportReport},
    terminal_session::TerminalSessionBinding,
};
use flotilla_resources::{
    convoy::engine::engine_name, Checkout, Clone as CloneResource, Convoy, ConvoyEnsure, Environment, ImageBuild, TerminalSession, Vessel,
    ACTUATOR_SOURCE_ROOT_ANNOTATION,
};

use super::{Carried, Source};

pub(super) const KINDS: [&str; 7] = ["environments", "imagebuilds", "clones", "checkouts", "terminalsessions", "vessels", "convoyensures"];

/// Refusals that are deliberate non-imports, counted as dropped. Both kind and complete reason
/// match, so a new refusal (a decode or store failure on a Checkout among them) still refuses.
const EXEMPT_REFUSALS: &[(&str, &str)] = &[("Checkout", "an observed checkout is no engine request")];

fn exempt(refusal: &ImportRefusal) -> bool {
    EXEMPT_REFUSALS.contains(&(refusal.kind, refusal.reason.as_str()))
}

pub(super) fn lifecycles(source: &Source, target: &mut Import, _: &mut Carried, report: &mut ImportReport) {
    for namespace in source.namespaces(&KINDS) {
        namespace_lifecycles(source, namespace, target, report);
    }
    let (exempted, refused): (Vec<_>, Vec<_>) = std::mem::take(&mut report.refused).into_iter().partition(exempt);
    report.refused = refused;
    for refusal in exempted {
        *report
            .dropped
            .entry((refusal.kind, EXEMPT_REFUSALS.iter().find(|(kind, _)| *kind == refusal.kind).map_or("", |(_, reason)| reason)))
            .or_default() += 1;
    }
}

fn namespace_lifecycles(source: &Source, namespace: &str, target: &mut Import, report: &mut ImportReport) {
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
    records!(Environment, "environments", "Environment", EnvironmentBinding);
    records!(ImageBuild, "imagebuilds", "ImageBuild", ImageBuildBinding);
    records!(CloneResource, "clones", "Clone", CloneBinding);
    records!(Checkout, "checkouts", "Checkout", CheckoutBinding);
    records!(TerminalSession, "terminalsessions", "TerminalSession", TerminalSessionBinding);
    let convoys = source.context::<Convoy>("convoys", namespace);
    let aliases: BTreeMap<_, _> = convoys
        .iter()
        .filter_map(|convoy| {
            engine_name(&convoy.object).ok().map(|name| ((convoy.home.clone(), convoy.object.metadata.name.clone()), name))
        })
        .collect();
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
