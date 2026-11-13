//! Attentions on the engine: ordinary Messages from a fleet role to the person role of their
//! context, about one subject, expecting nothing. Raising sends one, or supersedes an open one
//! whose reason changed; clearing withdraws it on its subject's authority; a person's receipt
//! settles it, and answers every raise of an episode that began before.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_address::{Address, Kind};
use flotilla_inbox::InboxBackend;
use flotilla_resources::{
    attentions::{self, Attentions, Fact, OpenAttention, Sender, Subject},
    Convoy, ConvoyEnsure, Project, ResourceError, ResourceObject, Vessel,
};
use flotilla_schema::Timestamp;
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
    roles::FleetRoles,
    MessagePrimitives, OldStoreRecords,
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
        self.message.since().to_datetime().unwrap_or_else(|| {
            tracing::debug!(message = self.message.name(), "an attention's time is unreadable; it reads as raised now");
            Utc::now()
        })
    }

    fn pending(&self) -> bool {
        *self.message.state() == MessageState::Pending
    }

    /// Whether a person acknowledged it at or after `began_at`: a settled message's time is when
    /// it settled.
    fn answers(&self, began_at: DateTime<Utc>) -> bool {
        matches!(self.message.state(), MessageState::Settled { receiver: Some(receiver), .. } if receiver.kind == ConsumerKind::Person)
            && self.message.since() >= Timestamp::from_datetime(&began_at)
    }
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
    /// The attention messages in the person roles' inboxes, from an attention's sender, about
    /// one subject: those pending, read through each inbox's index, and with `answered_since`
    /// those a person settled at or after it, read down each inbox's settled index to it.
    async fn held(&self, namespace: &str, answered_since: Option<DateTime<Utc>>) -> Result<Vec<Held>, String> {
        // Raising an attention names its sender's fleet role, so an undesignated fleet holds
        // none: clearing one then has nothing to clear, rather than failing the caller.
        let Ok(roles) = self.roles_or_wait(namespace).await? else { return Ok(Vec::new()) };
        let mut messages = Vec::new();
        for inbox in self.person_inboxes(namespace).await? {
            messages.extend(self.pending_in(&inbox)?);
            if let Some(since) = answered_since {
                messages.extend(self.settled_since(&inbox, Timestamp::from_datetime(&since))?);
            }
        }
        Ok(attentions_among(&roles, messages.into_iter()))
    }

    /// The open attentions about `scope`: for a subject in a convoy, only the pending messages
    /// in the inbox that convoy's attentions are sent to, read by that inbox's index, so a
    /// reader such as crew list reads no message history.
    async fn open_about(&self, namespace: &str, scope: Option<&Subject>) -> Result<Vec<Held>, String> {
        let convoy = match scope {
            Some(Subject::Convoy { convoy } | Subject::Vessel { convoy, .. } | Subject::ConvoyRole { convoy, .. }) => convoy,
            _ => return self.held(namespace, None).await,
        };
        let Ok(roles) = self.roles_or_wait(namespace).await? else { return Ok(Vec::new()) };
        let Some(inbox) = self.convoy_attention_inbox(namespace, convoy, &roles).await? else {
            return self.held(namespace, None).await;
        };
        Ok(attentions_among(&roles, self.pending_in(&inbox)?.into_iter()))
    }

    /// The person inbox a convoy's attentions are sent to, as [`Self::cause`] names it: its
    /// dispatching principal's role in its project. None when the convoy is not held here.
    async fn convoy_attention_inbox(&self, namespace: &str, convoy: &str, roles: &FleetRoles) -> Result<Option<Address>, String> {
        let convoy = match self.backend()?.including_replicas::<Convoy>(namespace).get(convoy).await {
            Ok(convoy) => convoy.object,
            Err(ResourceError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        let projects = match convoy.spec.project_ref.as_deref() {
            Some(project) => match self.backend()?.definitions::<Project>(namespace).get(project).await {
                Ok(project) => vec![project],
                Err(ResourceError::NotFound { .. }) => Vec::new(),
                Err(error) => return Err(error.to_string()),
            },
            None => Vec::new(),
        };
        let principal = format!("principal:{}", convoy.spec.dispatching_principal_ref.name);
        let project = convoy.spec.project_ref.clone();
        let records = OldStoreRecords::from_records(namespace, Vec::new(), vec![convoy], Vec::new(), Vec::new(), Vec::new(), projects);
        let conversion = Conversion { roles, forges: &records.forges, records: &records };
        conversion.person(&principal, project.as_deref()).map(Some)
    }
}

/// The attentions among `messages`: from an attention's sender, about one subject, in text.
fn attentions_among(roles: &FleetRoles, messages: impl Iterator<Item = (String, Resource<Message>)>) -> Vec<Held> {
    messages
        .filter_map(|(home, message)| {
            let spec = message.spec();
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
        .collect()
}

impl MessagePrimitives {
    /// The attentions from `sender` about `subject`: pending, and with `answered_since` those
    /// answered at or after it.
    async fn about(
        &self,
        namespace: &str,
        sender: Sender,
        subject: &Subject,
        answered_since: Option<DateTime<Utc>>,
    ) -> Result<Vec<Held>, String> {
        let held = self.held(namespace, answered_since).await?;
        Ok(held.into_iter().filter(|held| held.sender == sender && &held.subject == subject).collect())
    }

    /// `principal`'s role in `project`, on the project's declaration read into `records`.
    async fn person_inbox(
        &self,
        records: &mut OldStoreRecords,
        roles: &FleetRoles,
        principal: &str,
        project: Option<&str>,
    ) -> Result<Address, String> {
        self.spell(records, |records| Conversion { roles, forges: &[], records }.person(principal, project))
            .await
            .map_err(|error| error.to_string())?
    }

    /// The engine's cause and person inbox for a fact, read from the records it names.
    async fn cause(&self, namespace: &str, roles: &FleetRoles, fact: &Fact) -> Result<(Cause, Address), String> {
        let mut records = OldStoreRecords::empty(namespace);
        let Some(name) = fact.convoy() else {
            let Fact::Refused { ensure, project, role } = fact else { unreachable!("only a refused admission names no convoy") };
            // No generation was admitted: the principal admission would have stamped hears it,
            // in the role's project, as the other standing attentions are addressed.
            let principal = format!("principal:{}", flotilla_protocol::PrincipalRef::implicit_for_namespace(namespace).name);
            let inbox = self.person_inbox(&mut records, roles, &principal, Some(project)).await?;
            return Ok((Cause::Refused { role: self.standing_role(namespace, ensure, project, role).await? }, inbox));
        };
        let convoy = self
            .spell(&mut records, |records| records.convoy(name).cloned())
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("convoy `{name}` is not in this home's records"))?;
        let held = Read {
            address: super::convoy_address(name, convoy.spec.project_ref.as_deref())?,
            revision: convoy.metadata.resource_version.clone(),
        };
        let principal = format!("principal:{}", convoy.spec.dispatching_principal_ref.name);
        let inbox = self.person_inbox(&mut records, roles, &principal, convoy.spec.project_ref.as_deref()).await?;
        let cause = match fact {
            Fact::Hold { .. } => Cause::Hold { convoy: held },
            Fact::ReclaimRefused { .. } => Cause::ReclaimRefused { convoy: held },
            Fact::Drift { ensure, project, role, .. } => {
                Cause::Drift { role: self.standing_role(namespace, ensure, project, role).await?, convoy: held }
            }
            Fact::Refused { .. } => unreachable!("a refused admission names no convoy"),
            Fact::CredentialRefresh { vessel, .. } => {
                let record = self
                    .backend()?
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
            Fact::Stalled { role, .. } => {
                Cause::Stalled { role: Read { address: child(&held.address, Kind::Role, role)?, revision: held.revision } }
            }
        };
        Ok((cause, inbox))
    }

    /// The standing role `ensure` declares, read at its declaration's revision.
    async fn standing_role(&self, namespace: &str, ensure: &str, project: &str, role: &str) -> Result<Read, String> {
        let ensure: ResourceObject<ConvoyEnsure> =
            self.backend()?.definitions::<ConvoyEnsure>(namespace).get(ensure).await.map_err(|error| error.to_string())?;
        let project = Address::project(project).map_err(|error| format!("project `{project}`: {error}"))?;
        Ok(Read { address: child(&project, Kind::Role, role)?, revision: ensure.metadata.resource_version })
    }

    /// Withdraws the open ones homed here, each on its subject's authority; the others stay.
    async fn withdraw_open(&self, held: Vec<Held>) -> Result<(), String> {
        for held in held.into_iter().filter(Held::pending) {
            if held.home != self.home {
                tracing::debug!(message = held.message.name(), home = %held.home, "an attention homed elsewhere is cleared there");
                continue;
            }
            // The withdraw gate asks that the sender holds authority over the subject; the writer of
            // an attention does, and this home's world is told so before the edge.
            let authority = held.subject_address.clone();
            self.world.update(|world| world.authority.put(&flotilla_state_machines::Ref::new(authority.as_str()), true));
            let reason = DropReason::Withdrawn { by: held.sender_address.clone(), authority };
            self.inbox.drop(held.message.name(), reason).await.map_err(|error| error.to_string())?;
            let _ = self.attention_changes.send(());
        }
        Ok(())
    }
}

#[async_trait]
impl Attentions for MessagePrimitives {
    async fn raise(&self, namespace: &str, attention: &attentions::Attention) -> Result<(), String> {
        let (sender, subject) = (attention.fact.sender(), attention.fact.subject());
        let about = self.about(namespace, sender, &subject, Some(attention.began_at)).await?;
        if about.iter().any(|held| held.answers(attention.began_at)) {
            return Ok(());
        }
        let open: Vec<Held> = about.into_iter().filter(Held::pending).collect();
        if open.iter().any(|held| held.reason() == attention.reason) {
            return Ok(());
        }
        let roles = self.roles(namespace).await?;
        let (cause, inbox) = self.cause(namespace, &roles, &attention.fact).await?;
        let mut spec = Attention { cause, reason: attention.reason.clone() }.spec(&roles, inbox)?;
        let replaced: Vec<&Held> = open.iter().filter(|held| held.home == self.home).collect();
        if let Some(older) = replaced.first() {
            // The supersede gate asks that the sender saw the message it replaces, at its version.
            let reference = MessageRef::new(older.message.name(), older.home.clone());
            self.world.update(|world| world.sent.put_at(&sent(&reference), (), Version(older.message.version().0)));
            spec.supersedes = Some(reference);
        }
        let id = self.inbox.enqueue(spec).await.map_err(|error| error.to_string())?;
        for older in replaced {
            self.supersede(older.message.name(), &id, false).await;
        }
        let _ = self.attention_changes.send(());
        Ok(())
    }

    async fn clear(&self, namespace: &str, sender: Sender, subject: &Subject) -> Result<(), String> {
        self.withdraw_open(self.about(namespace, sender, subject, None).await?).await
    }

    async fn clear_within(&self, namespace: &str, scope: &Subject) -> Result<(), String> {
        let within = self.held(namespace, None).await?.into_iter().filter(|held| held.subject.within(scope)).collect();
        self.withdraw_open(within).await
    }

    async fn open(&self, namespace: &str, from: Option<Sender>, scope: Option<&Subject>) -> Result<Vec<OpenAttention>, String> {
        Ok(self
            .open_about(namespace, scope)
            .await?
            .into_iter()
            .filter(|held| {
                held.pending() && from.is_none_or(|from| from == held.sender) && scope.is_none_or(|scope| held.subject.within(scope))
            })
            .map(|held| OpenAttention { sender: held.sender, reason: held.reason(), raised_at: held.raised_at(), subject: held.subject })
            .collect())
    }

    async fn acknowledged(
        &self,
        namespace: &str,
        sender: Sender,
        subject: &Subject,
        began_at: DateTime<Utc>,
    ) -> Result<Option<String>, String> {
        let about = self.about(namespace, sender, subject, Some(began_at)).await?;
        let latest = about.iter().filter(|held| held.answers(began_at)).max_by_key(|held| (held.message.since(), held.message.name()));
        Ok(latest.map(Held::reason))
    }

    fn changes(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        Some(self.attention_changes.subscribe())
    }

    fn acknowledgements(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        Some(self.acknowledgements.subscribe())
    }
}
