//! Translate ephemeral crew-command inputs into receiver-homed Message intents.
use chrono::{DateTime, Utc};

use crate::{
    Convoy, CrewMessageSender, InputMeta, MessageInbox, MessageRelation, MessageSpec, ResourceError, ResourceObject, TerminalCrewMessage,
    TerminalSession, TypedResolver, CONVOY_LABEL, VESSEL_LABEL,
};

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

impl TypedResolver<TerminalSession> {
    /// Test-only fixture entry point for previous-generation sender envelopes.
    /// Production producers construct MessageSpec directly.
    /// Remove after the first fleet roll deploying #2710.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn accept_crew_message(
        &self,
        terminal: &ResourceObject<TerminalSession>,
        message: &TerminalCrewMessage,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let convoy_name =
            terminal.metadata.labels.get(CONVOY_LABEL).ok_or_else(|| ResourceError::invalid("crew message requires convoy context"))?;
        let vessel =
            terminal.metadata.labels.get(VESSEL_LABEL).ok_or_else(|| ResourceError::invalid("crew message requires vessel context"))?;
        let convoy = self.backend.including_replicas::<Convoy>(&self.namespace).get(convoy_name).await?;
        let project = convoy.object.spec.project_ref.as_deref().unwrap_or(&self.namespace);
        let receiver = format!("{project}/{convoy_name}/{vessel}/{}", terminal.spec.role);
        let spec = legacy_message_spec(&receiver, message);
        let name = crate::message_record_name(&receiver, &spec.sender, &message.id);
        MessageInbox::new(self.backend.clone(), &self.namespace).accept(&InputMeta::builder().name(name).build(), &spec, now).await?;
        Ok(())
    }
}
