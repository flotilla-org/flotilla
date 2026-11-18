//! Attentions on the engine: ordinary Messages from a fleet role to the person role of their
//! context, about one subject, expecting nothing. Raising sends one, or supersedes an open one
//! whose reason changed; clearing withdraws it on its subject's authority; a person's receipt
//! settles it, and answers every raise of an episode that began before.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_address::{Address, Kind};
use flotilla_inbox::InboxBackend;
use flotilla_resources::{
    attentions::{self, Attentions, Fact, OpenAttention, Sender, Subject},
    convoy::engine::{split_launch, standing_name},
    Convoy, ConvoyEnsure, Project, ResourceError, ResourceObject,
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
    roles::FleetRoles,
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

/// The old-shaped subject an engine address names, its convoy named by the record `records` gives
/// for the convoy's address, else by its segment.
fn subject_of(address: &Address, records: &BTreeMap<Address, String>) -> Option<Subject> {
    let name = address.name().to_owned();
    let convoy = || {
        let at = convoy_of(address)?;
        Some(records.get(&at).cloned().unwrap_or_else(|| at.name().to_owned()))
    };
    match (address.kind(), address.get(Kind::Convoy)) {
        (Kind::Convoy, _) => Some(Subject::Convoy { convoy: convoy()? }),
        (Kind::Vessel, Some(_)) => Some(Subject::Vessel { convoy: convoy()?, vessel: name }),
        (Kind::Role, Some(_)) => Some(Subject::ConvoyRole { convoy: convoy()?, role: name }),
        (Kind::Role, None) => Some(Subject::StandingRole { project: address.get(Kind::Project)?.to_owned(), role: name }),
        _ => None,
    }
}

/// The convoy `address` is in, or is.
fn convoy_of(address: &Address) -> Option<Address> {
    let mut at = address.clone();
    while at.kind() != Kind::Convoy {
        at = at.parent()?;
    }
    Some(at)
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
        self.attentions_among(namespace, &roles, messages.into_iter()).await
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
        self.attentions_among(namespace, &roles, self.pending_in(&inbox)?.into_iter()).await
    }

    /// The person inbox a convoy's attentions are sent to, as [`Self::cause`] names it: its
    /// dispatching principal's role in its project. None when the convoy is not held here.
    async fn convoy_attention_inbox(&self, namespace: &str, convoy: &str, roles: &FleetRoles) -> Result<Option<Address>, String> {
        let convoy = match self.backend()?.including_replicas::<Convoy>(namespace).get(convoy).await {
            Ok(convoy) => convoy.object,
            Err(ResourceError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        let principal = format!("principal:{}", convoy.spec.dispatching_principal_ref.name);
        self.person_inbox(namespace, roles, &principal, convoy.spec.project_ref.as_deref()).await.map(Some)
    }
}

impl MessagePrimitives {
    /// The attentions among `messages`: from an attention's sender, about one subject, in text.
    /// A launch is addressed by its one address (#3108 R4), whose segment is not a standing
    /// launch's record name, so each convoy subject is named by [`convoy_record`]: one listing of
    /// the ensure declarations when any subject is a launch.
    async fn attentions_among(
        &self,
        namespace: &str,
        roles: &FleetRoles,
        messages: impl Iterator<Item = (String, Resource<Message>)>,
    ) -> Result<Vec<Held>, String> {
        let attentions: Vec<_> = messages
            .filter_map(|(home, message)| {
                let spec = message.spec();
                let sender = Sender::from_role(roles.role_of(&spec.sender)?)?;
                let subject_address = spec.subject()?.address.clone();
                // An attention's body is always text; a typed request is no attention.
                let MessageBody::Text(reason) = spec.body.value() else { return None };
                Some((home, sender, subject_address, reason.clone(), message))
            })
            .collect();
        let convoys: BTreeSet<Address> = attentions.iter().filter_map(|(_, _, subject, _, _)| convoy_of(subject)).collect();
        let standing = if convoys.iter().any(|convoy| split_launch(convoy.name()).is_some()) {
            self.standing_roles(namespace).await?
        } else {
            BTreeSet::new()
        };
        let records =
            convoys.into_iter().map(|convoy| Ok((convoy.clone(), convoy_record(&convoy, &standing)?))).collect::<Result<_, String>>()?;
        Ok(attentions
            .into_iter()
            .filter_map(|(home, sender, subject_address, reason, message)| {
                Some(Held {
                    reason,
                    home,
                    sender,
                    sender_address: message.spec().sender.clone(),
                    subject: subject_of(&subject_address, &records)?,
                    subject_address,
                    message,
                })
            })
            .collect())
    }

    /// The standing roles the ensures declare, by project and role. An attention may be about a
    /// generation never launched, or retired, so a role is told standing by its declaration, not
    /// by a launch.
    async fn standing_roles(&self, namespace: &str) -> Result<BTreeSet<(String, String)>, String> {
        let ensures = self.backend()?.definitions::<ConvoyEnsure>(namespace).list().await.map_err(|error| error.to_string())?;
        Ok(ensures.into_iter().map(|ensure| (ensure.spec.project_ref, ensure.spec.role)).collect())
    }
}

/// The convoy record launch address `convoy` names: `project:P/convoy:R%23n` is launch `n` of
/// standing role `R` when `standing` declares it in `P`, else the record its segment names.
fn convoy_record(convoy: &Address, standing: &BTreeSet<(String, String)>) -> Result<String, String> {
    let segment = convoy.name();
    if let (Some((of, n)), Some(project)) = (split_launch(segment), convoy.get(Kind::Project)) {
        if standing.contains(&(project.to_owned(), of.to_owned())) {
            return Ok(format!("{}#{n}", standing_name(project, of)?));
        }
    }
    Ok(segment.to_owned())
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

    /// `principal`'s role in `project`, on the project's declaration read by its name.
    pub(crate) async fn person_inbox(
        &self,
        namespace: &str,
        roles: &FleetRoles,
        principal: &str,
        project: Option<&str>,
    ) -> Result<Address, String> {
        let declared: Option<ResourceObject<Project>> = match project {
            Some(project) => self.keyed_reads().declared::<Project>(namespace, project).await.map_err(|error| error.to_string())?,
            None => None,
        };
        // The roles the project's own charter declares the principal holds.
        let held = declared
            .into_iter()
            .flat_map(|declared| declared.spec.role_definitions)
            .filter(|(_, definition)| definition.principal.as_deref() == Some(principal))
            .map(|(role, _)| role)
            .collect();
        roles.principal_role(principal, project, held)
    }

    /// The engine's cause and person inbox for a fact, read from the records it names.
    async fn cause(&self, namespace: &str, roles: &FleetRoles, fact: &Fact) -> Result<(Cause, Address), String> {
        let Some(name) = fact.convoy() else {
            let Fact::Refused { ensure, project, role } = fact else { unreachable!("only a refused admission names no convoy") };
            // No generation was admitted: the principal admission would have stamped hears it,
            // in the role's project, as the other standing attentions are addressed.
            let principal = format!("principal:{}", flotilla_protocol::PrincipalRef::implicit_for_namespace(namespace).name);
            let inbox = self.person_inbox(namespace, roles, &principal, Some(project)).await?;
            return Ok((Cause::Refused { role: self.standing_role(namespace, ensure, project, role).await? }, inbox));
        };
        let convoy = self
            .keyed_reads()
            .convoy(namespace, name)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("convoy `{name}` is not in this home's records"))?;
        let held = Read {
            address: super::convoy_reference_address(name, convoy.object.spec.project_ref.as_deref().unwrap_or(namespace))?,
            revision: convoy.object.metadata.resource_version.clone(),
        };
        let principal = format!("principal:{}", convoy.object.spec.dispatching_principal_ref.name);
        let inbox = self.person_inbox(namespace, roles, &principal, convoy.object.spec.project_ref.as_deref()).await?;
        let cause = match fact {
            Fact::Hold { .. } => Cause::Hold { convoy: held },
            Fact::ReclaimRefused { .. } => Cause::ReclaimRefused { convoy: held },
            Fact::Drift { ensure, project, role, .. } => {
                Cause::Drift { role: self.standing_role(namespace, ensure, project, role).await?, convoy: held }
            }
            Fact::Refused { .. } => unreachable!("a refused admission names no convoy"),
            Fact::CredentialRefresh { vessel, .. } | Fact::SessionLeftRunning { vessel, .. } => {
                // A vessel's view is versioned by its launch, so its newest launch is its revision.
                let launch = self
                    .keyed_reads()
                    .vessel(name, &convoy.home, vessel)
                    .await?
                    .ok_or_else(|| format!("convoy `{name}` has no vessel `{vessel}`"))?;
                let vessel = Read { address: child(&held.address, Kind::Vessel, vessel)?, revision: launch.name };
                match fact {
                    Fact::CredentialRefresh { .. } => Cause::CredentialRefresh { vessel },
                    _ => Cause::SessionLeftRunning { vessel },
                }
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
