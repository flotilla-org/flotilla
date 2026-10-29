//! The Demand importer: each namespace's open demands become the attentions their writers send,
//! admitted at the demand's home after the messages imported there.

use flotilla_core::primitives::{
    convert::Conversion,
    demand_import::{import_demands, DemandRecords, OldDemand},
    import::{write_messages, ImportRefusal as MessageRefusal, ImportReport as MessageReport, Imported},
};
use flotilla_engine_store::{Import, ImportTarget};
use flotilla_provisioning::engine::import::{ImportRefusal, ImportReport};
use flotilla_resources::{Convoy, ConvoyEnsure, Demand, TerminalSession, Vessel};

use super::{
    messages::{fleet_roles, old_store_records},
    Carried, Source,
};

const KIND: &str = "Demand";

pub(super) fn attentions(source: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    for namespace in source.namespaces(&["demands"]) {
        namespace_attentions(source, namespace, target, carried, report);
    }
}

fn namespace_attentions(source: &Source, namespace: &str, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    let demands: Vec<_> = source
        .owned::<Demand>("demands", Some(namespace), KIND, target, report)
        .into_iter()
        .filter(|demand| target.imports(&demand.home))
        .map(|demand| (demand.object, demand.home))
        .collect();
    if demands.is_empty() {
        return;
    }
    let roles = match fleet_roles(source, namespace) {
        Ok(roles) => roles,
        Err(reason) => {
            report.refused.push(ImportRefusal { kind: KIND, record: "FleetDesignation".into(), reason });
            return;
        }
    };
    let convoys = source.context::<Convoy>("convoys", Some(namespace));
    let ensures = source.context::<ConvoyEnsure>("convoyensures", Some(namespace));
    let vessels = source.context::<Vessel>("vessels", Some(namespace));
    let terminals = source.context::<TerminalSession>("terminalsessions", Some(namespace));
    let records = old_store_records(source, namespace, terminals.iter().map(|terminal| terminal.object.clone()).collect());
    let conversion = Conversion { roles: &roles, forges: &records.forges, records: &records };
    let named = DemandRecords {
        convoys: convoys.iter().map(|convoy| (convoy.object.metadata.name.clone(), &convoy.object)).collect(),
        ensures: ensures.iter().map(|ensure| (ensure.object.metadata.name.clone(), &ensure.object)).collect(),
        vessels: vessels.iter().map(|vessel| &vessel.object).collect(),
        terminals: terminals.iter().map(|terminal| (terminal.object.metadata.name.clone(), &terminal.object)).collect(),
    };
    let old: Vec<_> = demands.iter().map(|(record, home)| OldDemand { record, home: home.clone() }).collect();
    let next = |home: &str| carried.admitted.get(home).copied().unwrap_or_default() + 1;
    let results = import_demands(&old, &next, &conversion, &named);
    let imported: Vec<_> = results
        .iter()
        .filter_map(|import| match &import.result {
            Ok(Imported::Message(message)) => Some((**message).clone()),
            _ => None,
        })
        .collect();
    let failed = write_messages(&imported, target);
    let failure = |record: &str| failed.iter().find(|refusal| refusal.record == record);
    for message in imported.iter().filter(|message| failure(&message.record).is_none()) {
        let admitted = carried.admitted.entry(message.home.clone()).or_default();
        *admitted = (*admitted).max(message.resource.version().0);
    }
    let mut counted = MessageReport::default();
    for import in &results {
        match (&import.result, import.folded_into.as_deref().and_then(failure)) {
            (Ok(Imported::Message(message)), _) if failure(&message.record).is_some() => {}
            (Ok(Imported::Dropped(dropped)), Some(refusal)) => {
                counted.refused.push(MessageRefusal { record: dropped.record.clone(), reason: refusal.reason.clone() })
            }
            (result, _) => counted.record(result),
        }
    }
    counted.refused.extend(failed.iter().cloned());
    for (label, count) in counted.imported {
        *report.imported.entry((KIND, label)).or_default() += count;
    }
    for (reason, count) in counted.dropped {
        *report.dropped.entry((KIND, reason)).or_default() += count;
    }
    report.refused.extend(counted.refused.into_iter().map(|refusal| ImportRefusal {
        kind: KIND,
        record: refusal.record,
        reason: refusal.reason,
    }));
}
