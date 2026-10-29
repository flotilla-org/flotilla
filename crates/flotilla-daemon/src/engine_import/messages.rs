//! The Message importer: each namespace's messages, every home's, mapped together with the records
//! they name, so a reply or a supersession reads the message it names wherever that is homed;
//! only the messages homed here are written.

use std::collections::BTreeSet;

use flotilla_core::primitives::{
    convert::Conversion,
    import::{import_all, write_messages, Imported, OldMessage, ReadRecords},
    roles::FleetRoles,
    OldStoreRecords,
};
use flotilla_engine_store::{Import, ImportTarget};
use flotilla_provisioning::engine::import::{ImportRefusal, ImportReport};
use flotilla_resources::{Artifact, Convoy, FleetDesignation, Forge, Message, Project, Repository, TerminalSession};
use flotilla_state_machines::machines::message::MessageRef;

use super::{Carried, Source};

const KIND: &str = "Message";

pub(super) fn messages(source: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    for namespace in source.namespaces(&["messages"]) {
        namespace_messages(source, namespace, target, carried, report);
    }
}

fn namespace_messages(source: &Source, namespace: &str, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    let messages: Vec<_> = source
        .owned::<Message>("messages", Some(namespace), KIND, target, report)
        .into_iter()
        .map(|message| (message.object, message.home))
        .collect();
    if messages.is_empty() {
        return;
    }
    // Conversion needs the fleet's roles: without them no message is imported, rather than a partial set.
    let designation = source.context::<FleetDesignation>("fleetdesignations", Some(namespace)).into_iter().next();
    let roles = match designation {
        Some(designation) => FleetRoles::from_designation(&designation.object.spec),
        None if source.of("fleetdesignations", Some(namespace)).next().is_some() => Err("its FleetDesignation does not decode".to_owned()),
        None => Err("no FleetDesignation names the fleet".to_owned()),
    };
    let roles = match roles {
        Ok(roles) => roles,
        Err(reason) => {
            if messages.iter().any(|(_, home)| target.imports(home)) {
                report.refused.push(ImportRefusal { kind: KIND, record: "FleetDesignation".into(), reason });
            }
            return;
        }
    };
    let terminals = source.context::<TerminalSession>("terminalsessions", Some(namespace));
    let sessions = terminals
        .iter()
        .filter_map(|terminal| {
            let status = terminal.object.status.as_ref()?;
            Some((
                (status.crew.as_ref()?.id.clone(), status.session_id.clone()?),
                (terminal.object.metadata.name.clone(), terminal.home.clone()),
            ))
        })
        .collect();
    let records = OldStoreRecords::from_records(
        namespace,
        source.context::<Forge>("forges", Some(namespace)).into_iter().map(|forge| forge.object.spec).collect(),
        source.context::<Convoy>("convoys", Some(namespace)).into_iter().map(|convoy| convoy.object).collect(),
        terminals.into_iter().map(|terminal| terminal.object).collect(),
        source.context::<Artifact>("artifacts", Some(namespace)).into_iter().map(|artifact| artifact.object).collect(),
        source.context::<Repository>("repositories", Some(namespace)).into_iter().map(|repository| repository.object).collect(),
        source.context::<Project>("projects", Some(namespace)).into_iter().map(|project| project.object).collect(),
    );
    let conversion = Conversion { roles: &roles, forges: &records.forges, records: &records };
    let world = ReadRecords {
        messages: messages.iter().map(|(record, home)| (record.metadata.name.clone(), (record, home.clone()))).collect(),
        sessions,
    };
    let local: BTreeSet<&str> =
        messages.iter().filter(|(_, home)| target.imports(home)).map(|(record, _)| record.metadata.name.as_str()).collect();
    let here = |record: &str| local.contains(record);
    let results = import_all(messages.iter().map(|(record, home)| OldMessage { record, home: home.clone() }), &conversion, &world);
    let imported: Vec<_> = results
        .iter()
        .filter_map(|result| match result {
            Ok(Imported::Message(message)) => Some((**message).clone()),
            _ => None,
        })
        .collect();
    for message in &imported {
        carried.messages.insert(
            (namespace.to_owned(), message.record.clone()),
            (MessageRef::new(message.resource.name(), &message.home), message.resource.spec().inbox.clone()),
        );
    }
    let failed = write_messages(&imported, target);
    let mut counted = flotilla_core::primitives::import::ImportReport::default();
    for result in &results {
        match result {
            Ok(Imported::Message(message)) if failed.iter().any(|refusal| refusal.record == message.record) => {}
            Ok(Imported::Message(message)) if !here(&message.record) => {}
            Ok(Imported::Dropped(dropped)) if !here(&dropped.record) => {}
            Err(refusal) if !here(&refusal.record) => {}
            other => counted.record(other),
        }
    }
    counted.refused.extend(failed);
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
