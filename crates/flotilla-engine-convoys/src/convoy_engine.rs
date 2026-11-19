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
use flotilla_protocol::Subject;
use flotilla_resources::{
    address::subject_address,
    convoy::engine::{Actor, EngineConvoys, OldPromises, VerdictScope},
    promises::{PromiseKind as OldKind, PromiseOperation, PromiseSource as OldSource},
    Convoy as OldConvoy, Forge, ForgeSpec, ResourceObject, SubjectDiscoverySource,
};
use flotilla_schema::{reads::carry, Kind, Timestamp};
use flotilla_state_machines::{
    machines::convoy::{
        AbandonCause, AddSeat, Authority, Convoy, ConvoyEdge, ConvoyLabel, ConvoyState, Declare, Deliver, FailSeat, Keep, Link, LinkSource,
        Progress, Promise, PromiseId, Reassign, Reject, Retract, SeatName, Submit, Unlink,
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

    /// The engine record of `convoy` at `home` (`None` for this one): one keyed read.
    fn engine_record(&self, convoy: &ResourceObject<OldConvoy>, home: Option<&str>) -> Result<Option<Resource<Convoy>>, String> {
        // This home named as the convoy's home is deliberately the local read: the in-memory
        // engine is current, and its own records are never replicas here.
        Ok(match home.filter(|home| *home != self.convoys.home()) {
            None => match self.convoys.launch(&convoy.metadata.name) {
                Some(launch) => Some(launch),
                None => self.convoys.get(&map::engine_name(convoy)?),
            },
            Some(home) => self
                .convoys
                .replica(&map::engine_name(convoy)?, &map::project(convoy)?, home)
                .map_err(|error| format!("convoy `{}` at {home}: the engine replica did not read: {error}", convoy.metadata.name))?,
        })
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
        let name = self.target(convoy)?;
        let now = self.now();
        let edited = tokio::task::spawn_blocking(carry(move || convoys.edit(&name, now, plan))).await.map_err(|error| error.to_string())?;
        let applied = match &edited {
            Ok((applied, _)) => *applied,
            Err(error) => error.applied(),
        };
        if applied > 0 {
            self.changed(&convoy.metadata.name);
        }
        edited.map(|(_, output)| output).map_err(|error: EditError| error.to_string())
    }

    /// The launch an edit of `convoy` writes: the launch it is named by, read from the engine, else
    /// (an old record) the newest launch of what it is a launch of.
    fn target(&self, convoy: &ResourceObject<OldConvoy>) -> Result<String, String> {
        if self.convoys.launch(&convoy.metadata.name).is_some() {
            return Ok(convoy.metadata.name.clone());
        }
        map::engine_name(convoy)
    }

    /// The launch held here that `convoy` reads as, when one is.
    fn held(&self, convoy: &ResourceObject<OldConvoy>) -> Option<Resource<Convoy>> {
        if let Some(launch) = self.convoys.launch(&convoy.metadata.name) {
            return Some(launch);
        }
        self.convoys.get(&map::engine_name(convoy).ok()?).filter(|current| holds_generation(convoy, current))
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
fn next_id(convoy: &Resource<Convoy>) -> Result<PromiseId, String> {
    let spec = convoy.spec();
    let next = spec.promises.iter().map(|row| u64::from(row.record.id.0) + 1).max().unwrap_or(0);
    u32::try_from(next).map(PromiseId).map_err(|_| "convoy promise ids exhausted".into())
}

/// The world key an edge's recorded input is read under.
fn input(convoy: &str, what: &str) -> String {
    format!("{convoy}/{what}")
}

fn open(promise: &Promise) -> bool {
    matches!(promise.progress.value, Progress::Pending | Progress::Submitted)
}

// A new standing generation must reach admission when its stable record has
// ended; terminal old records still read the engine view. Admission and routing
// use the same predicate so they cannot disagree about a held generation.
fn holds_generation(convoy: &ResourceObject<OldConvoy>, current: &Resource<Convoy>) -> bool {
    !flotilla_resources::convoy::reconcile::is_ensured(convoy)
        || current.label() == ConvoyLabel::Open
        || convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal())
}

impl ConvoyEngine {
    /// Ends `convoy` without delivering it, for `cause`; an ended convoy is left as it is.
    async fn end(&self, convoy: &ResourceObject<OldConvoy>, principal: &str, reason: &str, cause: AbandonCause) -> Result<(), String> {
        let (name, by, reason) = (map::engine_name(convoy)?, operator(principal), reason.to_owned());
        self.edit(convoy, move |current, world| {
            // An ended engine convoy has nothing left to abandon.
            if current.label() != ConvoyLabel::Open {
                return Ok((Vec::new(), ()));
            }
            let authority = input(&name, if cause == AbandonCause::Failed { "fail" } else { "abandon" });
            world.authority.put(&Ref::new(authority.as_str()), true);
            let abandon = flotilla_state_machines::machines::convoy::Abandon { by, cause, reason, authority: Ref::new(authority) };
            Ok((vec![abandon.into()], ()))
        })
        .await
    }
}

#[async_trait]
impl EngineConvoys for ConvoyEngine {
    fn holds(&self, convoy: &ResourceObject<OldConvoy>) -> bool {
        // Fail closed: route an invalid identity to this writer, whose edits return
        // the naming error, rather than falling back to the legacy writer.
        map::engine_name(convoy).is_err() || self.held(convoy).is_some()
    }

    async fn reserve(&self, launch_of: &str) -> Result<String, String> {
        let (convoys, of, now) = (Arc::clone(&self.convoys), launch_of.to_owned(), self.now());
        tokio::task::spawn_blocking(carry(move || convoys.reserve(&of, now)))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| format!("reserve a launch of `{launch_of}`: {error:?}"))
    }

    async fn admit_reserved(&self, convoy: &ResourceObject<OldConvoy>) -> Result<(), String> {
        let launch = convoy.metadata.name.clone();
        let request = map::request(convoy)?;
        if let Some(clash) = map::clash(&request) {
            return Err(format!("convoy `{launch}`: {clash}"));
        }
        let project = request.project.clone();
        let (convoys, now) = (Arc::clone(&self.convoys), self.now());
        let named = launch.clone();
        let admitted = tokio::task::spawn_blocking(carry(move || {
            // Admission read the project; the engine's gate reads that record.
            convoys.world(|world| world.projects.put(&Ref::new(project.as_str()), ()));
            convoys.create_reserved(&named, request, now)
        }))
        .await
        .map_err(|error| error.to_string())?;
        match admitted {
            Ok(()) => {
                self.changed(&launch);
                Ok(())
            }
            Err(error) => match error.refusal() {
                Some(refusal) => Err(format!("the engine refused convoy `{launch}`: {refusal:?}")),
                None => Err(format!("convoy `{launch}` engine admission: {error:?}")),
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
                        promise: next_id(current)?,
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
                (PromiseOperation::Submit { id, kind, source, .. }, Some((address, mut revision))) => {
                    let mut edges = Vec::new();
                    let (promise, owner) = match found(&id) {
                        Some(promise) => {
                            // The same reference again is the submission already made: a kept
                            // promise takes it as a no-op. A submitted one takes it from its crew
                            // as a submission again at the current revision, when it brings no
                            // newer one: the crew's answer to a turn it was handed, which the stall
                            // judge reads by its time (GitHub run GH2-1). From anyone else it is a
                            // no-op.
                            let current = promise.submissions.iter().last().map(|row| &row.record);
                            let same = current.is_some_and(|last| last.address == address);
                            let newer = current.is_some_and(|last| !revision.is_empty() && last.revision != revision);
                            match promise.progress.value {
                                Progress::Kept if same => return Ok((Vec::new(), ())),
                                Progress::Submitted if same && !newer && actor != Actor::Crew => return Ok((Vec::new(), ())),
                                Progress::Submitted if same && !newer => {
                                    revision = current.map(|last| last.revision.clone()).unwrap_or_default();
                                }
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

    async fn reassign(
        &self,
        convoy: &ResourceObject<OldConvoy>,
        from: (&str, &str),
        to: (&str, &str),
        promises: &[String],
    ) -> Result<(), String> {
        let crews = crews(convoy);
        let (from, to) = (crews.seat(from.0, from.1), crews.seat(to.0, to.1));
        let name = map::engine_name(convoy)?;
        let promises = promises.to_vec();
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
            // A crew keeps its own promises (#3574 decision 3): the handoff moves only those the
            // sender names, each its own and open.
            for id in &promises {
                let promise = current
                    .spec()
                    .promises
                    .iter()
                    .map(|row| &row.record)
                    .find(|promise| promise.owner == from && map::old_id(promise) == *id)
                    .ok_or_else(|| format!("promise `{id}` is not {}'s to reassign", from.role))?;
                if !open(promise) {
                    return Err(format!("promise `{id}` is {:?}, not open, and cannot be reassigned", promise.progress.value));
                }
                edges.push(Reassign { promise: promise.id, to: to.clone(), by: from.clone(), authority: Authority::Owner }.into());
            }
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

    async fn deliver(&self, convoy: &ResourceObject<OldConvoy>, disposition: &str) -> Result<(), String> {
        let disposition = disposition.to_owned();
        self.edit(convoy, move |current, _| {
            Ok((
                // A retired convoy has nothing left to deliver.
                if matches!(current.label(), ConvoyLabel::Delivered | ConvoyLabel::Retired) {
                    Vec::new()
                } else {
                    vec![Deliver { disposition: disposition.clone() }.into()]
                },
                (),
            ))
        })
        .await
    }

    fn delivered(&self, convoy: &ResourceObject<OldConvoy>) -> Option<String> {
        match self.held(convoy)?.state() {
            ConvoyState::Delivered { disposition, .. } => Some(disposition.clone()),
            _ => None,
        }
    }

    async fn abandon(&self, convoy: &ResourceObject<OldConvoy>, principal: &str, reason: &str) -> Result<(), String> {
        self.end(convoy, principal, reason, AbandonCause::Operator).await
    }

    async fn fail(&self, convoy: &ResourceObject<OldConvoy>, principal: &str, reason: &str) -> Result<(), String> {
        self.end(convoy, principal, reason, AbandonCause::Failed).await
    }

    async fn fail_seat(
        &self,
        convoy: &ResourceObject<OldConvoy>,
        vessel: &str,
        role: &str,
        principal: &str,
        reason: &str,
    ) -> Result<(), String> {
        let (name, seat, by, reason) =
            (map::engine_name(convoy)?, crews(convoy).seat(vessel, role), operator(principal), reason.to_owned());
        self.edit(convoy, move |current, world| {
            if current.spec().seat(&seat).is_some_and(|(_, seat)| seat.failed.is_some()) {
                return Ok((Vec::new(), ()));
            }
            let authority = input(&name, "fail-seat");
            world.authority.put(&Ref::new(authority.as_str()), true);
            Ok((vec![FailSeat { seat, reason, by, authority: Ref::new(authority) }.into()], ()))
        })
        .await
    }

    async fn link(
        &self,
        convoy: &ResourceObject<OldConvoy>,
        subject: &Subject,
        source: SubjectDiscoverySource,
        principal: &str,
    ) -> Result<(), String> {
        let source = match source {
            SubjectDiscoverySource::Claim => LinkSource::Claim,
            SubjectDiscoverySource::Operator => LinkSource::Operator,
            SubjectDiscoverySource::Relay => LinkSource::Relay,
            SubjectDiscoverySource::Branch => LinkSource::Branch,
        };
        let address = subject_address(subject, &self.forges(&convoy.metadata.namespace).await?)?;
        let (name, by) = (map::engine_name(convoy)?, operator(principal));
        self.edit(convoy, move |current, world| {
            if current.spec().linked().contains(&&address) {
                return Ok((Vec::new(), ()));
            }
            let authority = input(&name, "link");
            world.authority.put(&Ref::new(authority.as_str()), true);
            Ok((vec![Link { subject: address, source, by, authority: Ref::new(authority) }.into()], ()))
        })
        .await
    }

    async fn unlink(&self, convoy: &ResourceObject<OldConvoy>, subject: &Subject, principal: &str) -> Result<(), String> {
        let address = subject_address(subject, &self.forges(&convoy.metadata.namespace).await?)?;
        let (name, by) = (map::engine_name(convoy)?, operator(principal));
        self.edit(convoy, move |current, world| {
            if !current.spec().linked().contains(&&address) {
                return Ok((Vec::new(), ()));
            }
            let authority = input(&name, "unlink");
            world.authority.put(&Ref::new(authority.as_str()), true);
            Ok((vec![Unlink { subject: address, by, authority: Ref::new(authority) }.into()], ()))
        })
        .await
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.changes.subscribe()
    }

    fn memory(&self) -> &flotilla_resources::convoy_records::ConvoyMemory {
        self.convoys.memory()
    }

    fn promises(&self, convoy: &ResourceObject<OldConvoy>, home: Option<&str>) -> Result<Option<OldPromises>, String> {
        Ok(self.engine_record(convoy, home)?.map(|engine| map::old_promises(&engine, &crews(convoy), None)))
    }

    fn recorded_promises(&self, convoy: &ResourceObject<OldConvoy>, home: Option<&str>) -> Result<Option<OldPromises>, String> {
        let Some(engine) = self.engine_record(convoy, home)? else { return Ok(None) };
        let at = home.unwrap_or(self.convoys.home()).to_owned();
        let history = self
            .convoys
            .service()
            .with_store(|store| {
                let mut read = store.read()?;
                match read.resolve(<Resource<Convoy> as Kind>::CODE, engine.name(), &at)? {
                    Some(id) => crate::convoy_records::promise_history(&mut read, id, &at, &engine).map(Some),
                    None => Ok(None),
                }
            })
            .map_err(|error| format!("convoy `{}` at {at}: its promises' log entries did not read: {error}", convoy.metadata.name))?;
        Ok(Some(map::old_promises(&engine, &crews(convoy), history.as_ref())))
    }
}
