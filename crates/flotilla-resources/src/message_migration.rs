//! Translate ephemeral crew-command inputs into receiver-homed Message intents.

use crate::{CrewMessageSender, MessageRelation, MessageSpec, TerminalCrewMessage};

/// Map crew-command sender attribution to a Message address and relation.
pub fn legacy_message_sender(sender: &CrewMessageSender) -> (String, MessageRelation) {
    match sender {
        CrewMessageSender::OperatorResume { principal } | CrewMessageSender::OperatorFollowUp { principal } => (
            format!(
                "principal:{}",
                principal.as_ref().map_or(flotilla_protocol::PrincipalRef::IMPLICIT_NAME, |principal| principal.name.as_str())
            ),
            MessageRelation::Supervisor,
        ),
        CrewMessageSender::Governor { name } => (legacy_actor_address(name), MessageRelation::Supervisor),
        CrewMessageSender::Bosun { name } => (legacy_actor_address(name), MessageRelation::Supervisor),
        CrewMessageSender::Handoff { from } => (legacy_actor_address(from), MessageRelation::Peer),
        CrewMessageSender::FlotillaNudge => ("system:nudge".into(), MessageRelation::System),
        CrewMessageSender::FlotillaTurn { .. } => ("system:turn-rules".into(), MessageRelation::System),
        CrewMessageSender::FlotillaEscalation { .. } => ("system:stall-judge".into(), MessageRelation::Supervisor),
        CrewMessageSender::Unknown => ("system:legacy".into(), MessageRelation::System),
    }
}

fn legacy_actor_address(actor: &str) -> String {
    if let Some((crew, convoy)) = actor.split_once(" in ") {
        if let Some((role, vessel)) = crew.split_once('@') {
            return if let Some((convoy, project)) = convoy.split_once('@') {
                format!("{project}/{convoy}/{vessel}/{role}")
            } else {
                format!("{convoy}/{vessel}/{role}")
            };
        }
    }
    if let Some((role, project)) = actor.split_once('@') {
        return format!("{project}/{role}");
    }
    format!("principal:{}", actor.replace([' ', '/', '@'], "_"))
}

pub fn legacy_message_spec(receiver: &str, message: &TerminalCrewMessage) -> MessageSpec {
    let (mut sender, relation) = legacy_message_sender(&message.sender);
    if let [project, convoy, vessel, _] = receiver.split('/').collect::<Vec<_>>().as_slice() {
        // Old bosun/handoff headers sometimes omit convoy context but retain
        // role@vessel. Governor role@project is a declared project role instead.
        match &message.sender {
            CrewMessageSender::Bosun { name } | CrewMessageSender::Handoff { from: name } if !name.contains(" in ") => {
                if let Some((role, sender_vessel)) = name.split_once('@') {
                    sender = format!("{project}/{convoy}/{sender_vessel}/{role}");
                }
            }
            _ => {}
        }
        let context = crate::MessageAddressContext { project: (*project).into(), convoy: (*convoy).into(), vessel: (*vessel).into() };
        if let Ok(qualified) = crate::qualify_message_address(&sender, &context) {
            sender = qualified;
        }
    }
    MessageSpec::builder().sender(sender).receiver(receiver.into()).relation(relation).body(message.text.clone()).build()
}
