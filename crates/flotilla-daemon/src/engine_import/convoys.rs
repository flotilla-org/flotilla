//! The Convoy importer: each convoy homed here, a standing convoy's generations folded into one,
//! with an open stall as the record awaiting its supervision message.

use std::collections::BTreeMap;

use flotilla_address::Address;
use flotilla_core::primitives::convoy_import::{import_all, write_into, ConvoyImportWorld};
use flotilla_engine_store::{Import, ImportTarget};
use flotilla_provisioning::engine::import::{ImportRefusal, ImportReport};
use flotilla_resources::{Convoy, Forge};
use flotilla_state_machines::machines::message::MessageRef;

use super::{Carried, Source};

const KIND: &str = "Convoy";

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
                    Ok(_) => *report.imported.entry((KIND, "open")).or_default() += 1,
                    Err(reason) => report.refused.push(ImportRefusal { kind: KIND, record: convoy.resource.name().to_owned(), reason }),
                }
                if let Some(reason) = &convoy.unrecorded_stall {
                    report.notes.push(format!("Convoy `{}`: an open stall is not recorded: {reason}", convoy.resource.name()));
                }
            }
            if !mapped.stays_old.is_empty() {
                *report.dropped.entry((KIND, "ended or not bootstrapped: stays on the old store")).or_default() += mapped.stays_old.len();
            }
            for refusal in &mapped.refused {
                report.refused.push(ImportRefusal { kind: KIND, record: refused_name(refusal), reason: refusal.to_string() });
            }
            if mapped.folded_generations > 0 {
                report.notes.push(format!("{} standing convoy generations folded into their stable convoys", mapped.folded_generations));
            }
        }
    }
}

fn refused_name(refusal: &flotilla_core::primitives::convoy_import::ConvoyImportRefusal) -> String {
    use flotilla_core::primitives::convoy_import::ConvoyImportRefusal;
    match refusal {
        ConvoyImportRefusal::NoObligations { convoy }
        | ConvoyImportRefusal::StandingAwaitsRuling { convoy }
        | ConvoyImportRefusal::Unmappable { convoy, .. } => convoy.clone(),
    }
}
