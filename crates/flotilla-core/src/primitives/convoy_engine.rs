//! The engine-first Convoy writer behind `[primitives]` (#3587 layer 6).
//!
//! [`ConvoyEngine`] implements the old-shaped port
//! ([`flotilla_resources::convoy::engine::EngineConvoys`]) over this home's
//! engine Convoys ([`super::convoys::Convoys`]). Each method maps the old
//! crew and promise names through [`super::convoy_map`], records what the
//! gates take as given (a project's existence, a reference's durability, an
//! operator's authority) in the world, applies the edges under one writer
//! lock, and returns the machine's refusal to the caller. Readers see the
//! engine's promises in the old shape through [`EngineConvoys::promises`]
//! (#3587 layer 8): one keyed read of this home's record, or of the replica
//! of another home's in the decided store. Nothing is written to the old
//! status for them; each edit publishes the convoy's name instead
//! ([`EngineConvoys::changes`]), so its followers run again.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_resources::{
    convoy::{
        engine::{Actor, Admission, EngineConvoys, OldPromises, SkipReason, VerdictScope},
        reconcile::is_ensured,
    },
    promises::{PromiseKind as OldKind, PromiseOperation, PromiseSource as OldSource},
    Convoy as OldConvoy, ConvoyStatusPatch, Forge, ForgeSpec, ResourceObject,
};
use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::convoy::{
        AddSeat, Authority, Convoy, ConvoyEdge, ConvoyLabel, Declare, Deliver, Keep, Progress, Promise, PromiseId, Reassign, Reject,
        Retract, SeatName, Submit,
    },
    Ref, Resource,
};
use flotilla_store::ResourceBackend;

use super::{
    convoy_map::{self as map, Crews},
    convoys::{Convoys, EditError},
};

/// The role a decider outside the convoy is named by in an edge's `by`.
const OPERATOR: &str = "operator";

/// This home's engine-first Convoy writer, in the old shape.
pub struct ConvoyEngine {
    convoys: Arc<Convoys>,
    backend: ResourceBackend,
    clock: Arc<dyn flotilla_resources::Clock>,
    changes: tokio::sync::broadcast::Sender<String>,
}

/// How many changed-convoy names a slow follower may fall behind by; a
/// follower that lags further misses names, and the reconciler's resync
/// covers them.
const CHANGES: usize = 1024;

impl ConvoyEngine {
    pub fn new(convoys: Arc<Convoys>, backend: ResourceBackend, clock: Arc<dyn flotilla_resources::Clock>) -> Self {
        Self { convoys, backend, clock, changes: tokio::sync::broadcast::channel(CHANGES).0 }
    }

    fn changed(&self, name: &str) {
        // No follower is no error: the reconciler may not be running yet.
        let _ = self.changes.send(name.to_owned());
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_datetime(&self.clock.now())
    }

    /// Applies the edges `plan` works out, off the async workers, then
    /// publishes the change whenever any edge applied, a partial edit
    /// included. A refusal comes back as the error; the plan's other output
    /// as the value.
    async fn edit<T: Send + 'static>(
        &self,
        convoy: &ResourceObject<OldConvoy>,
        plan: impl FnOnce(
                &Resource<Convoy>,
                &mut flotilla_state_machines::machines::convoy::MemoryConvoyWorld,
            ) -> Result<(Vec<ConvoyEdge>, T), String>
            + Send
            + 'static,
    ) -> Result<T, String> {
        let convoys = Arc::clone(&self.convoys);
        let name = convoy.metadata.name.clone();
        let now = self.now();
        let edited = tokio::task::spawn_blocking(move || convoys.edit(&name, now, plan)).await.map_err(|error| error.to_string())?;
        let applied = match &edited {
            Ok((applied, _)) => *applied,
            Err(error) => error.applied(),
        };
        if applied > 0 {
            self.changed(&convoy.metadata.name);
        }
        edited.map(|(_, output)| output).map_err(|error: EditError| error.to_string())
    }

    async fn forges(&self, namespace: &str) -> Result<Vec<ForgeSpec>, String> {
        Ok(self
            .backend
            .definitions::<Forge>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect())
    }
}

fn crews(convoy: &ResourceObject<OldConvoy>) -> Crews {
    convoy.status.as_ref().map(Crews::of).unwrap_or_default()
}

fn operator(principal: &str) -> SeatName {
    SeatName::new(OPERATOR, principal)
}

/// The next unused promise id: ids are never reused, retracted ones included.
fn next_id(convoy: &Resource<Convoy>) -> PromiseId {
    PromiseId(convoy.spec().promises.iter().map(|row| row.record.id.0 + 1).max().unwrap_or(0))
}

/// The world key an edge's recorded input is read under.
fn input(convoy: &str, what: &str) -> String {
    format!("{convoy}/{what}")
}

fn open(promise: &Promise) -> bool {
    matches!(promise.progress.value, Progress::Pending | Progress::Submitted)
}

#[async_trait]
impl EngineConvoys for ConvoyEngine {
    fn holds(&self, convoy: &ResourceObject<OldConvoy>) -> bool {
        self.convoys.get(&convoy.metadata.name).is_some()
    }

    async fn admit(&self, convoy: &ResourceObject<OldConvoy>) -> Result<Admission, String> {
        if self.holds(convoy) {
            return Ok(Admission::Held);
        }
        let Some(status) = convoy.status.as_ref().filter(|status| status.workflow_snapshot.is_some()) else {
            return Ok(Admission::Skipped(SkipReason::NotBootstrapped));
        };
        if is_ensured(convoy) {
            return Ok(Admission::Skipped(SkipReason::Standing));
        }
        if let Err(history) = map::undecided(status) {
            return Ok(Admission::Skipped(SkipReason::Decided(history)));
        }
        let request = map::request(convoy)?;
        if let Some(clash) = map::clash(&request) {
            return Ok(Admission::Refused(clash));
        }
        let project = request.project.clone();
        let convoys = Arc::clone(&self.convoys);
        let name = convoy.metadata.name.clone();
        let now = self.now();
        let admitted = tokio::task::spawn_blocking(move || {
            // Old admission read the project; the engine's gate reads that record.
            convoys.world(|world| world.projects.put(&Ref::new(project.as_str()), ()));
            convoys.admit(&name, request, now)
        })
        .await
        .map_err(|error| error.to_string())?;
        match admitted {
            Ok(_) => {
                // The engine holds the promises now: drop bootstrap's seeds from the old status.
                let convoys = self.backend.clone().using::<OldConvoy>(&convoy.metadata.namespace);
                if let Err(error) =
                    flotilla_store::apply_status_patch(&convoys, &convoy.metadata.name, &ConvoyStatusPatch::HandPromisesToEngine).await
                {
                    tracing::warn!(convoy = %convoy.metadata.name, %error, "dropping the seeded promises from the old status failed");
                }
                self.changed(&convoy.metadata.name);
                Ok(Admission::Held)
            }
            Err(error) => match error.refusal() {
                Some(refusal) => Ok(Admission::Refused(format!("{refusal:?}"))),
                None => Err(format!("convoy `{}` engine admission: {error:?}", convoy.metadata.name)),
            },
        }
    }

    async fn promise(
        &self,
        convoy: &ResourceObject<OldConvoy>,
        vessel: &str,
        role: &str,
        operation: &PromiseOperation,
        actor: Actor,
    ) -> Result<(), String> {
        let name = convoy.metadata.name.clone();
        let seat = crews(convoy).seat(vessel, role);
        let target = map::address(convoy)?;
        let submitted = match operation {
            PromiseOperation::Submit { kind, submission, .. } => {
                let forges = if *kind == OldKind::Pr { self.forges(&convoy.metadata.namespace).await? } else { Vec::new() };
                Some(map::submission(&name, role, *kind, submission, &forges)?)
            }
            _ => None,
        };
        let operation = operation.clone();
        self.edit(convoy, move |current, world| {
            let found = |id: &str| map::find(current, &seat, id).cloned();
            // A new promise is named for what it delivers; a label already taken is refused.
            let declare = |id: &str, kind: OldKind, source: OldSource| {
                let label = map::label(&seat, id, kind, source);
                match map::taken(current, &label) {
                    Some(refusal) => Err(refusal),
                    None => Ok(Declare {
                        promise: next_id(current),
                        label,
                        kind: kind.as_str().to_owned(),
                        target: target.clone(),
                        owner: seat.clone(),
                    }),
                }
            };
            // An operator decides with its own authority; a crew's verbs are its seat's.
            let (decider, authority) = match &actor {
                Actor::Operator { principal } => (operator(principal), Authority::Operator),
                Actor::Crew | Actor::Observer => (seat.clone(), Authority::Owner),
            };
            let edges = match (operation, submitted) {
                (PromiseOperation::Declare { id, kind, source }, _) => match found(&id) {
                    Some(_) => Vec::new(),
                    None => vec![declare(&id, kind, source)?.into()],
                },
                (PromiseOperation::Submit { id, kind, source, .. }, Some((address, revision))) => {
                    let mut edges = Vec::new();
                    let (promise, owner) = match found(&id) {
                        Some(promise) => {
                            // The same reference again is the submission already made, as on the
                            // old path, which compared subjects: a kept or submitted promise takes
                            // it as a no-op unless it brings a revision the current one lacks.
                            let current = promise.submissions.iter().last().map(|row| &row.record);
                            let same = current.is_some_and(|last| last.address == address);
                            let newer = current.is_some_and(|last| !revision.is_empty() && last.revision != revision);
                            match promise.progress.value {
                                Progress::Kept if same => return Ok((Vec::new(), ())),
                                Progress::Submitted if same && !newer => return Ok((Vec::new(), ())),
                                _ => {}
                            }
                            (promise.id, promise.owner.clone())
                        }
                        None => {
                            let declared = declare(&id, kind, source)?;
                            let promise = declared.promise;
                            edges.push(declared.into());
                            (promise, seat.clone())
                        }
                    };
                    // Only the owner submits: discovery and an operator submit on its behalf.
                    let by = if actor == Actor::Crew { seat.clone() } else { owner };
                    let durable = input(&format!("{address}@{revision}"), "durable");
                    world.durable.put(&Ref::new(durable.as_str()), true);
                    edges.push(Submit { promise, by, address, revision, durable: Ref::new(durable) }.into());
                    edges
                }
                (PromiseOperation::Submit { id, .. }, None) => return Err(format!("promise `{id}`: the submission has no address")),
                (PromiseOperation::Retract { id, reason }, _) => {
                    let promise = found(&id).ok_or_else(|| format!("unknown promise `{id}`"))?;
                    vec![Retract { promise: promise.id, by: decider, authority, reason }.into()]
                }
                (PromiseOperation::Verdict { id, verdict, .. }, _) => {
                    let promise = found(&id).ok_or_else(|| format!("unknown promise `{id}`"))?;
                    match (verdict.accepted, &actor) {
                        (true, Actor::Operator { .. }) => {
                            vec![Keep::Verdict { promise: promise.id, by: decider, authority, reason: verdict.why }.into()]
                        }
                        (true, _) => {
                            let submission = u32::try_from(promise.submissions.len()).unwrap_or(u32::MAX);
                            vec![Keep::Observed { promise: promise.id, submission }.into()]
                        }
                        (false, _) => vec![Reject { promise: promise.id }.into()],
                    }
                }
            };
            Ok((edges, ()))
        })
        .await
    }

    async fn reassign(&self, convoy: &ResourceObject<OldConvoy>, vessel: &str, from: &str, to: &str) -> Result<(), String> {
        let crews = crews(convoy);
        let (from, to) = (crews.seat(vessel, from), crews.seat(vessel, to));
        let name = convoy.metadata.name.clone();
        self.edit(convoy, move |current, world| {
            let mut edges: Vec<ConvoyEdge> = Vec::new();
            if from == to {
                return Ok((edges, ()));
            }
            if current.spec().seat(&to).is_none() {
                // A handoff to a crew the convoy has no seat for fans it out under the sender.
                let (_, sender) = current.spec().seat(&from).ok_or_else(|| format!("seat {from:?} is not in convoy `{name}`"))?;
                let authority = input(&name, "handoff");
                world.authority.put(&Ref::new(authority.as_str()), true);
                edges.push(
                    AddSeat {
                        seat: to.clone(),
                        parent: from.clone(),
                        supervised_by: sender.supervised_by.clone(),
                        by: from.clone(),
                        authority: Ref::new(authority),
                    }
                    .into(),
                );
            }
            edges.extend(
                current
                    .spec()
                    .promises
                    .iter()
                    .map(|row| &row.record)
                    .filter(|promise| promise.owner == from && open(promise))
                    .map(|promise| Reassign { promise: promise.id, to: to.clone(), by: from.clone(), authority: Authority::Owner }.into()),
            );
            Ok((edges, ()))
        })
        .await
    }

    async fn keep_by_verdict(
        &self,
        convoy: &ResourceObject<OldConvoy>,
        scope: &VerdictScope,
        principal: &str,
        reason: &str,
    ) -> Result<Vec<String>, String> {
        let crews = crews(convoy);
        let scope = scope.clone();
        let by = operator(principal);
        let reason = reason.to_owned();
        self.edit(convoy, move |current, _| {
            let in_scope = |seat: &SeatName| {
                crews
                    .crew(seat)
                    .is_some_and(|(vessel, role)| vessel == scope.vessel && scope.role.as_deref().is_none_or(|named| named == role))
            };
            let promises: Vec<&Promise> =
                current.spec().promises.iter().map(|row| &row.record).filter(|promise| open(promise) && in_scope(&promise.owner)).collect();
            let labels = promises.iter().map(|promise| promise.label.clone()).collect();
            let edges = promises
                .into_iter()
                .map(|promise| {
                    Keep::Verdict { promise: promise.id, by: by.clone(), authority: Authority::Operator, reason: reason.clone() }.into()
                })
                .collect();
            Ok((edges, labels))
        })
        .await
    }

    async fn verdict(
        &self,
        convoy: &ResourceObject<OldConvoy>,
        label: &str,
        keep: bool,
        principal: &str,
        reason: &str,
    ) -> Result<(), String> {
        let (label, by, reason) = (label.to_owned(), operator(principal), reason.to_owned());
        self.edit(convoy, move |current, _| {
            let promises: Vec<&Promise> = current.spec().promises.iter().map(|row| &row.record).collect();
            // An exact label names one promise; a bare old id must name exactly one.
            let promise = match promises.iter().find(|promise| promise.label == label) {
                Some(promise) => *promise,
                None => match promises.iter().filter(|promise| map::old_id(promise) == label).collect::<Vec<_>>().as_slice() {
                    [promise] => **promise,
                    [] => return Err(format!("no promise labelled `{label}`")),
                    several => {
                        let labels = several.iter().map(|promise| format!("`{}`", promise.label)).collect::<Vec<_>>().join(", ");
                        return Err(format!("`{label}` names several promises; give one of the labels {labels}"));
                    }
                },
            };
            let edge: ConvoyEdge = if keep {
                Keep::Verdict { promise: promise.id, by, authority: Authority::Operator, reason }.into()
            } else {
                Retract { promise: promise.id, by, authority: Authority::Operator, reason }.into()
            };
            Ok((vec![edge], ()))
        })
        .await
    }

    async fn deliver(&self, convoy: &ResourceObject<OldConvoy>) -> Result<(), String> {
        self.edit(convoy, |current, _| Ok((if current.label() == ConvoyLabel::Delivered { Vec::new() } else { vec![Deliver.into()] }, ())))
            .await
    }

    async fn abandon(&self, convoy: &ResourceObject<OldConvoy>, principal: &str, reason: &str) -> Result<(), String> {
        let (name, by, reason) = (convoy.metadata.name.clone(), operator(principal), reason.to_owned());
        self.edit(convoy, move |current, world| {
            // An ended engine convoy has nothing left to abandon.
            if current.label() != ConvoyLabel::Open {
                return Ok((Vec::new(), ()));
            }
            let authority = input(&name, "abandon");
            world.authority.put(&Ref::new(authority.as_str()), true);
            Ok((vec![flotilla_state_machines::machines::convoy::Abandon { by, reason, authority: Ref::new(authority) }.into()], ()))
        })
        .await
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.changes.subscribe()
    }

    fn promises(&self, convoy: &ResourceObject<OldConvoy>, home: Option<&str>) -> Option<OldPromises> {
        let engine = match home.filter(|home| *home != self.convoys.home()) {
            None => self.convoys.get(&convoy.metadata.name)?,
            Some(home) => match self.convoys.replica(&convoy.metadata.name, home) {
                Ok(found) => found?,
                Err(error) => {
                    tracing::warn!(convoy = %convoy.metadata.name, %home, %error, "the engine convoy replica did not read");
                    return None;
                }
            },
        };
        Some(map::old_promises(&engine, &crews(convoy)))
    }
}
