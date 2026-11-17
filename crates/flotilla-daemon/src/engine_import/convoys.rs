//! The Convoy importer: each convoy homed here as a launch of its engine name, a standing role's
//! generations as its successive launches,
//! with an open stall as the record awaiting its supervision message.

use std::collections::BTreeMap;

use flotilla_address::Address;
use flotilla_admission::{reconcilers::convoy_ensure::generation_end, standing::fold};
use flotilla_engine_convoys::convoy_import::{import_all, write_into, ConvoyImportWorld};
use flotilla_engine_store::{Import, ImportTarget};
use flotilla_provisioning::engine::{
    import::{created, ImportRefusal, ImportReport},
    standing_roles::record,
};
use flotilla_resources::{standing_roles::RunningValues, Convoy, Forge, ResourceObject, ENSURED_FROM_ANNOTATION};
use flotilla_state_machines::machines::message::MessageRef;

use super::{Carried, Source};

const KIND: &str = "Convoy";
const STANDING_ROLE: &str = "StandingRole";

/// The engine messages the Message importer mapped, by old record name.
struct Messages<'a> {
    namespace: &'a str,
    carried: &'a Carried,
}

impl ConvoyImportWorld for Messages<'_> {
    fn message(&self, record: &str) -> Option<(MessageRef, Address)> {
        self.carried.messages.get(&(self.namespace.to_owned(), record.to_owned())).cloned()
    }
}

pub(super) fn convoys(source: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    let mut written_convoys = Vec::new();
    for namespace in source.namespaces(&["convoys"]) {
        let mut homes = BTreeMap::<String, Vec<_>>::new();
        for convoy in source.owned::<Convoy>("convoys", Some(namespace), KIND, target, report) {
            if target.imports(&convoy.home) {
                homes.entry(convoy.home).or_default().push(convoy.object);
            }
        }
        let forges: Vec<_> = source.context::<Forge>("forges", Some(namespace)).into_iter().map(|forge| forge.object.spec).collect();
        let world = Messages { namespace, carried };
        for (home, convoys) in homes {
            let mapped = import_all(&convoys, &home, &forges, &world);
            for convoy in &mapped.imported {
                match write_into(target, convoy) {
                    Ok(written) => {
                        report.import_row(KIND, "open");
                        written_convoys
                            .push(((convoy.home.clone(), convoy.record.clone()), (written.name().to_owned(), written.version().0)));
                    }
                    Err(reason) => report.refused.push(ImportRefusal { kind: KIND, record: convoy.resource.name().to_owned(), reason }),
                }
                if let Some(reason) = &convoy.unrecorded_stall {
                    report.notes.push(format!("Convoy `{}`: an open stall is not recorded: {reason}", convoy.resource.name()));
                }
            }
            // Dropped by the import; the old resources.sqlite stays on disk.
            let mut stays_old = BTreeMap::<&'static str, usize>::new();
            for (_, reason) in &mapped.stays_old {
                *stays_old.entry(reason.label()).or_default() += 1;
            }
            for (reason, count) in stays_old {
                report.drop_rows(KIND, reason, count);
            }
            for refusal in &mapped.refused {
                report.refused.push(ImportRefusal { kind: KIND, record: refused_name(refusal), reason: refusal.to_string() });
            }
            standing_roles(&convoys, &home, target, report);
        }
    }
    carried.convoys.extend(written_convoys);
}

/// Seeds each standing role homed at `home` with its generations' ends folded oldest first, as
/// its ensure pass would have folded them: the strikes and backoff its dropped ended generations
/// carried survive them.
fn standing_roles(convoys: &[ResourceObject<Convoy>], home: &str, target: &mut Import, report: &mut ImportReport) {
    let mut roles = BTreeMap::<&str, Vec<&ResourceObject<Convoy>>>::new();
    for convoy in convoys {
        if let Some(ensure) = convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION) {
            roles.entry(ensure).or_default().push(convoy);
        }
    }
    'roles: for (ensure, mut generations) in roles {
        generations.sort_by_key(|convoy| convoy.spec.generation);
        let mut values = RunningValues::default();
        // The record is created as the last generation it counts was.
        let mut counted = None;
        for convoy in generations {
            match generation_end(convoy) {
                Ok(Some(end)) => {
                    values = fold(&values, &end);
                    counted = Some(convoy);
                }
                Ok(None) => {}
                Err(reason) => {
                    report.refused.push(ImportRefusal { kind: STANDING_ROLE, record: ensure.to_owned(), reason });
                    continue 'roles;
                }
            }
        }
        let Some(counted) = counted else { continue };
        match target.write(home, ensure, &created(counted), &record(&values)) {
            Ok(()) => *report.imported.entry((STANDING_ROLE, "folded")).or_default() += 1,
            Err(reason) => report.refused.push(ImportRefusal { kind: STANDING_ROLE, record: ensure.to_owned(), reason }),
        }
    }
}

fn refused_name(refusal: &flotilla_engine_convoys::convoy_import::ConvoyImportRefusal) -> String {
    use flotilla_engine_convoys::convoy_import::ConvoyImportRefusal;
    match refusal {
        ConvoyImportRefusal::NoObligations { convoy }
        | ConvoyImportRefusal::StandingAwaitsRuling { convoy }
        | ConvoyImportRefusal::Unmappable { convoy, .. } => convoy.clone(),
    }
}
