//! The person consumer's inbox through the CLI (#3938): the messages waiting
//! in every person role's inbox, and the acknowledgement or answer that is a
//! person's receipt. Listing a message writes nothing.

use flotilla_daemon_api::action_events::ActionEvents;
use flotilla_messaging::{Intent, IntentReference};
use flotilla_protocol::{Command, CommandAction, CommandValue, PersonInboxItem, PersonInboxResponse};
use flotilla_resources::MessageRelation;
use flotilla_state_machines::machines::message::{Expectation, MessageBody};

use super::InProcessDaemon;
use flotilla_engine_messages::view::record_name;

impl InProcessDaemon {
    pub(super) async fn execute_action_person(&self, id: u64, command: &Command) -> Result<u64, String> {
        let events = ActionEvents { node_id: &self.node_id, sink: &self.event_sink };
        let identity = events.start(id, command.description().to_string());
        let result = self.person_action(&command.action).await.unwrap_or_else(|message| CommandValue::Error { message });
        events.finish(id, identity, result);
        Ok(id)
    }

    async fn person_action(&self, action: &CommandAction) -> Result<CommandValue, String> {
        let primitives = self.message_primitives();
        let namespace = match action {
            CommandAction::PersonInbox { namespace }
            | CommandAction::PersonAcknowledge { namespace, .. }
            | CommandAction::PersonAnswer { namespace, .. } => match namespace {
                Some(namespace) => namespace.clone(),
                None => self.provisioning_namespace().await,
            },
            _ => return Err("a person action selected the wrong handler".into()),
        };
        match action {
            CommandAction::PersonInbox { .. } => {
                let items = primitives
                    .person_messages(&namespace)
                    .await?
                    .into_iter()
                    .map(|(home, message)| {
                        let spec = message.spec();
                        PersonInboxItem {
                            message: record_name(message.name()).unwrap_or_else(|| message.name().to_owned()),
                            inbox: spec.inbox.to_string(),
                            sender: spec.sender.to_string(),
                            home,
                            expects: match &spec.expectation {
                                Expectation::Nothing => "nothing".into(),
                                Expectation::Reply => "a reply".into(),
                                Expectation::Outcome { condition } => format!("the outcome {condition}"),
                            },
                            body: match spec.body.value() {
                                MessageBody::Text(text) => text.clone(),
                                MessageBody::Request(request) => format!("{request:?}"),
                            },
                        }
                    })
                    .collect();
                Ok(CommandValue::PersonInbox(Box::new(PersonInboxResponse { items })))
            }
            CommandAction::PersonAcknowledge { message, .. } => {
                primitives.acknowledge(&namespace, message).await?;
                Ok(CommandValue::Ok)
            }
            CommandAction::PersonAnswer { message, body, .. } => {
                let original = primitives.acknowledge(&namespace, message).await?;
                let original = original.spec();
                // The person answers as the role the message was addressed to, back to its sender.
                let references = original
                    .references
                    .iter()
                    .map(|reference| {
                        let revision = flotilla_address::Revision::new(reference.revision.clone()).map_err(|error| error.to_string())?;
                        let mut answered = IntentReference::new(reference.address.clone(), revision);
                        answered.subject = reference.subject;
                        Ok(answered)
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                let mut reply = Intent::new(original.inbox.clone(), original.sender.clone(), MessageRelation::Supervisor, body.clone());
                reply.references = references;
                reply.in_reply_to = Some(message.clone());
                reply.principal = primitives.person_holding(&namespace, &original.inbox).await?;
                let name = flotilla_address::message_record_name(reply.inbox.as_str(), reply.sender.as_str(), &format!("answer:{message}"));
                // One answer per message: repeating it is a no-op, and a different one is refused
                // rather than dropped by the engine's keyed admission.
                if let Some(sent) = primitives.find_stored(&name, &reply.inbox).await.map_err(|error| error.to_string())? {
                    if !matches!(sent.resource.spec().body.value(), MessageBody::Text(text) if *text == reply.body) {
                        return Err(format!("message `{message}` was already answered as `{name}`; one answer per message"));
                    }
                    return Ok(CommandValue::Ok);
                }
                self.publish_message(&namespace, &name, &reply).await?;
                Ok(CommandValue::Ok)
            }
            _ => Err("a person action selected the wrong handler".into()),
        }
    }
}
