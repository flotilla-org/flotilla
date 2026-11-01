//! Attentions on the engine: ordinary Messages from a fleet role to the person role of their
//! context, about one subject, expecting nothing. Raising sends one, or supersedes an open one
//! whose reason changed; clearing withdraws it on its subject's authority; a person's receipt
//! settles it.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_address::{Address, Kind};
use flotilla_inbox::InboxBackend;
use flotilla_resources::{
    attentions::{self, Attentions, Fact, OpenAttention, Sender, Subject},
    ConvoyEnsure, ResourceObject, Vessel,
};
use flotilla_state_machines::{
    machines::{
        consumer::ConsumerKind,
        message::{sent, DropReason, Message, MessageBody, MessageRef, MessageState},
    },
    Resource, Version,
};

use super::{
    attention::{Attention, Cause, Read},
    convert::Conversion,
    MessagePrimitives,
};

/// One attention message in this replica, with its home, what it is and the reason it gives.
struct Held {
    home: String,
    sender: Sender,
    sender_address: Address,
    subject: Subject,
    subject_address: Address,
    reason: String,
    message: Resource<Message>,
}

impl Held {
    fn reason(&self) -> String {
        self.reason.clone()
    }

    /// When it was raised; an unreadable time reads as now, so it never counts as long open.
    fn raised_at(&self) -> DateTime<Utc> {
        self.message.since().to_datetime().unwrap_or_else(Utc::now)
    }

    fn pending(&self) -> bool {
        *self.message.state() == MessageState::Pending
    }

    fn settled_by_person(&self) -> bool {
        matches!(self.message.state(), MessageState::Settled { receiver: Some(receiver) } if receiver.kind == ConsumerKind::Person)
    }
}

/// The attention a person acknowledged, when none is open and it is the latest: its writer has not
/// cleared it since.
fn acknowledged(about: &[Held]) -> Option<&Held> {
    if about.iter().any(Held::pending) {
        return None;
    }
    about.iter().max_by_key(|held| (held.message.since(), held.message.name())).filter(|latest| latest.settled_by_person())
}

/// The old-shaped subject an engine address names.
fn subject_of(address: &Address) -> Option<Subject> {
    let name = address.name().to_owned();
    match (address.kind(), address.get(Kind::Convoy)) {
        (Kind::Convoy, _) => Some(Subject::Convoy { convoy: name }),
        (Kind::Vessel, Some(convoy)) => Some(Subject::Vessel { convoy: convoy.to_owned(), vessel: name }),
        (Kind::Role, Some(convoy)) => Some(Subject::ConvoyRole { convoy: convoy.to_owned(), role: name }),
        (Kind::Role, None) => Some(Subject::StandingRole { project: address.get(Kind::Project)?.to_owned(), role: name }),
        _ => None,
    }
}

fn child(parent: &Address, kind: Kind, name: &str) -> Result<Address, String> {
    parent.child(kind, name).map_err(|error| format!("{parent} {kind:?} `{name}`: {error}"))
}

impl MessagePrimitives {
    /// Every attention message in this replica: from an attention's sender to a person role,
    /// about one subject.
    async fn held(&self, namespace: &str) -> Result<Vec<Held>, String> {
        let roles = self.roles(namespace).await?;
        let people = self.person_inboxes(namespace).await?;
        let messages = self.messages()?;
        Ok(messages
            .into_iter()
            .filter_map(|(home, message)| {
                let spec = message.spec();
                if !people.contains(&spec.inbox) {
                    return None;
                }
                let sender = Sender::from_role(roles.role_of(&spec.sender)?)?;
                let subject_address = spec.subject()?.address.clone();
                // An attention's body is always text; a typed request is no attention.
                let MessageBody::Text(reason) = spec.body.value() else { return None };
                let reason = reason.clone();
                Some(Held {
                    reason,
                    home,
                    sender,
                    sender_address: spec.sender.clone(),
                    subject: subject_of(&subject_address)?,
                    subject_address,
                    message,
                })
            })
            .collect())
    }

    async fn about(&self, namespace: &str, sender: Sender, subject: &Subject) -> Result<Vec<Held>, String> {
        Ok(self.held(namespace).await?.into_iter().filter(|held| held.sender == sender && &held.subject == subject).collect())
    }

    /// The engine's cause and person inbox for a fact, read from the records it names.
    async fn cause(&self, namespace: &str, fact: &Fact, conversion: &Conversion<'_>) -> Result<(Cause, Address), String> {
        let records = self.records(namespace).await.map_err(|error| error.to_string())?;
        let name = fact.convoy();
        let convoy = records.convoys.get(name).ok_or_else(|| format!("convoy `{name}` is not in this home's records"))?;
        let held = Read { address: records.convoy_address(name)?, revision: convoy.metadata.resource_version.clone() };
        let principal = format!("principal:{}", convoy.spec.dispatching_principal_ref.name);
        let inbox = conversion.person(&principal, convoy.spec.project_ref.as_deref())?;
        let cause = match fact {
            Fact::Hold { .. } => Cause::Hold { convoy: held },
            Fact::ReclaimRefused { .. } => Cause::ReclaimRefused { convoy: held },
            Fact::Drift { ensure, project, role, .. } => {
                let ensure: ResourceObject<ConvoyEnsure> =
                    self.backend.definitions::<ConvoyEnsure>(namespace).get(ensure).await.map_err(|error| error.to_string())?;
                let project = Address::project(project).map_err(|error| format!("project `{project}`: {error}"))?;
                let role = Read { address: child(&project, Kind::Role, role)?, revision: ensure.metadata.resource_version };
                Cause::Drift { role, convoy: held }
            }
            Fact::CredentialRefresh { vessel, .. } => {
                let record = self
                    .backend
                    .including_replicas::<Vessel>(namespace)
                    .list()
                    .await
                    .map_err(|error| error.to_string())?
                    .items
                    .into_iter()
                    .find(|record| record.object.spec.convoy_ref == name && record.object.spec.vessel_name == *vessel)
                    .ok_or_else(|| format!("convoy `{name}` has no vessel `{vessel}`"))?;
                let address = child(&held.address, Kind::Vessel, vessel)?;
                Cause::CredentialRefresh { vessel: Read { address, revision: record.object.metadata.resource_version } }
            }
            Fact::DeliveryHeld { role, .. } => {
                Cause::DeliveryHeld { role: Read { address: child(&held.address, Kind::Role, role)?, revision: held.revision } }
            }
        };
        Ok((cause, inbox))
    }

    /// Withdraws an attention homed here, on its subject's authority.
    async fn withdraw_attention(&self, held: &Held) -> Result<(), String> {
        let authority = held.subject_address.clone();
        self.world.update(|world| world.authority.put(&flotilla_state_machines::Ref::new(authority.as_str()), true));
        let reason = DropReason::Withdrawn { by: held.sender_address.clone(), authority };
        self.inbox.drop(held.message.name(), reason).await.map_err(|error| error.to_string())
    }
}

#[async_trait]
impl Attentions for MessagePrimitives {
    async fn raise(&self, namespace: &str, attention: &attentions::Attention) -> Result<(), String> {
        let (sender, subject) = (attention.fact.sender(), attention.fact.subject());
        let about = self.about(namespace, sender, &subject).await?;
        if acknowledged(&about).is_some() {
            return Ok(());
        }
        let open: Vec<Held> = about.into_iter().filter(Held::pending).collect();
        if open.iter().any(|held| held.reason() == attention.reason) {
            return Ok(());
        }
        let roles = self.roles(namespace).await?;
        let records = self.records(namespace).await.map_err(|error| error.to_string())?;
        let conversion = Conversion { roles: &roles, forges: &records.forges, records: &*records };
        let (cause, inbox) = self.cause(namespace, &attention.fact, &conversion).await?;
        let mut spec = Attention { cause, reason: attention.reason.clone() }.spec(&roles, inbox)?;
        let replaced: Vec<&Held> = open.iter().filter(|held| held.home == self.home).collect();
        if let Some(older) = replaced.first() {
            let reference = MessageRef::new(older.message.name(), older.home.clone());
            self.world.update(|world| world.sent.put_at(&sent(&reference), (), Version(older.message.version().0)));
            spec.supersedes = Some(reference);
        }
        let id = self.inbox.enqueue(spec).await.map_err(|error| error.to_string())?;
        for older in replaced {
            self.supersede(older.message.name(), &id, false).await;
        }
        Ok(())
    }

    async fn clear(&self, namespace: &str, sender: Sender, subject: &Subject) -> Result<(), String> {
        let about = self.about(namespace, sender, subject).await?;
        // A settled message takes no edge: the end of an acknowledged attention is a successor,
        // withdrawn at once, so the next raise is told again.
        if let Some(seen) = acknowledged(&about).filter(|seen| seen.home == self.home) {
            let mut spec = seen.message.spec().clone();
            spec.admitted_seq = 0;
            spec.supersedes = None;
            let id = self.inbox.enqueue(spec).await.map_err(|error| error.to_string())?;
            let authority = seen.subject_address.clone();
            self.world.update(|world| world.authority.put(&flotilla_state_machines::Ref::new(authority.as_str()), true));
            let reason = DropReason::Withdrawn { by: seen.sender_address.clone(), authority };
            return self.inbox.drop(&id, reason).await.map_err(|error| error.to_string());
        }
        for held in about {
            if !held.pending() {
                continue;
            }
            if held.home != self.home {
                tracing::debug!(message = held.message.name(), home = %held.home, "an attention homed elsewhere is cleared there");
                continue;
            }
            self.withdraw_attention(&held).await?;
        }
        Ok(())
    }

    async fn open(&self, namespace: &str, from: Option<Sender>, scope: Option<&Subject>) -> Result<Vec<OpenAttention>, String> {
        Ok(self
            .held(namespace)
            .await?
            .into_iter()
            .filter(|held| {
                held.pending() && from.is_none_or(|from| from == held.sender) && scope.is_none_or(|scope| held.subject.within(scope))
            })
            .map(|held| OpenAttention { sender: held.sender, reason: held.reason(), raised_at: held.raised_at(), subject: held.subject })
            .collect())
    }

    async fn acknowledged(&self, namespace: &str, sender: Sender, subject: &Subject) -> Result<bool, String> {
        Ok(acknowledged(&self.about(namespace, sender, subject).await?).is_some())
    }
}
