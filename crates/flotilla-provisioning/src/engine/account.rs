//! The account kinds on the engine, behind `[primitives]` (#3824 layer 1).
//!
//! [`EngineAccounts`] is the [`AccountRecords`] port over the decided store. This home writes its
//! own bindings through the AccountBinding machine and its own provisions through the plain-kind
//! port; every home's are read from resident maps kept current by following the log, the way
//! the store's replicas receive them. No old-store record is written or read.
//!
//! A binding is never revived (ADR 0064), so on the engine a requester's bindings are named by
//! their opening (`<requester binding name>.<opened_at micros>`): a rebind closes the open one
//! where it is and opens the next under its own name. Readers find a requester's binding by its
//! requester and requirement, never by a fixed name.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use flotilla_engine_store::{
    follow::{Followed, Follower},
    open_home,
    plain::{Plain, PlainError, PlainLog},
    StoreLog,
};
use flotilla_resources::account_access::{
    binding_name, requester_parts, AccessName, AccountBindingSpec, AccountChange, AccountProvisionSpec, AccountRecords, BindingRecord,
    BindingState as OldState, EpochEntry, GrantName, GrantScope, PendingBinding, ProvisionRecord, ProvisionWrite,
};
use flotilla_schema::{Identity, Kind, KindCode, Ref, State, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use flotilla_state_machines::{Home, Resource, WriteError};
use futures::stream::BoxStream;

use crate::stored::account::{
    self as stored, AccountBinding, BindingEdge, BindingSpec, BindingState, Close, ObserveEpoch, ProvisionSpec, Regrant, Revoke,
};

/// How often a change stream looks for new entries in the log.
const POLL: Duration = Duration::from_millis(500);

/// The writer recorded on this home's account writes.
const WRITER: &str = "account-records";

const BINDING: KindCode = <Resource<AccountBinding> as Kind>::CODE;
const PROVISION: KindCode = <ProvisionSpec as Kind>::CODE;

/// The engine's name of a requester's binding opened at `opened_at`.
pub fn engine_binding_name(requester: &str, opened_at: Timestamp) -> String {
    format!("{requester}.{}", opened_at.micros())
}

/// Every home's bindings and provisions, as this store holds them.
struct Resident {
    follower: Follower,
    bindings: Bindings,
    provisions: Provisions,
}

/// The account records of the decided store at one home.
///
/// Reads catch the resident maps up with the log first, synchronously under the store's lock,
/// on the caller's thread: a short read of the entries since the last pass, as the Convoy and
/// Message paths read the store. The change stream does the same every [`POLL`].
pub struct EngineAccounts {
    service: Arc<Service>,
    home: String,
    bindings: Mutex<Home<AccountBinding, StoreLog>>,
    provisions: PlainLog,
    resident: Mutex<Resident>,
    clock: Arc<dyn flotilla_resources::Clock>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn store_error(error: StoreError) -> String {
    error.to_string()
}

impl EngineAccounts {
    /// Opens this home's binding writer over `service`, restored from the store, and reads
    /// every home's account records.
    pub fn open(service: Arc<Service>, clock: Arc<dyn flotilla_resources::Clock>) -> Result<Self, String> {
        let log = StoreLog::new(service.store());
        let home = log.home().to_owned();
        let bindings = open_home::<AccountBinding>(log)?;
        let provisions = PlainLog::new(service.store());
        let resident = Resident { follower: Follower::new([BINDING, PROVISION]), bindings: BTreeMap::new(), provisions: BTreeMap::new() };
        let accounts = Self { service, home, bindings: Mutex::new(bindings), provisions, resident: Mutex::new(resident), clock };
        let homes = accounts.service.homes();
        {
            let mut resident = locked(&accounts.resident);
            accounts
                .service
                .with_store(|store| {
                    let mut read = store.read()?;
                    resident.follower.start(&mut read, &homes)?;
                    for home in &homes {
                        rescan(&mut read, &mut resident, home)?;
                    }
                    Ok(())
                })
                .map_err(store_error)?;
        }
        Ok(accounts)
    }

    /// Brings the resident maps up to the store: every entry written since the last refresh.
    fn refresh(&self) -> Result<MutexGuard<'_, Resident>, String> {
        let mut resident = locked(&self.resident);
        let homes = self.service.homes();
        self.service
            .with_store(|store| {
                let mut read = store.read()?;
                let followed = resident.follower.pass(&mut read, &homes)?;
                for followed in followed {
                    match followed {
                        Followed::Changed(identity) => reload(&mut read, &mut resident, &identity)?,
                        Followed::Rescan(home) => rescan(&mut read, &mut resident, &home)?,
                    }
                }
                Ok(())
            })
            .map_err(store_error)?;
        Ok(resident)
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_datetime(&self.clock.now())
    }

    /// The requester's open binding homed here, if any.
    fn open_binding(&self, requester: &flotilla_schema::Address, access: &AccessName) -> Option<String> {
        locked(&self.bindings)
            .resources()
            .find(|binding| {
                binding.spec().requester == *requester
                    && binding.spec().requirement == *access
                    && matches!(binding.state(), BindingState::Open)
            })
            .map(|binding| binding.name().to_owned())
    }

    fn create(&self, pending: &PendingBinding) -> Result<(), String> {
        let spec = binding_spec(&pending.spec);
        let name = engine_binding_name(&pending.name, spec.opened_at);
        locked(&self.bindings)
            .create(name.clone(), spec, &(), pending.spec.opened_at)
            .map(drop)
            .map_err(|error| format!("open account binding `{name}`: {error:?}"))
    }

    fn apply(&self, binding: &BindingRecord, edge: impl Into<BindingEdge>, now: Timestamp) -> Result<(), String> {
        if binding.home != self.home {
            return Err(format!("account binding `{}` is homed at `{}`; only its home writes it", binding.name, binding.home));
        }
        let mut home = locked(&self.bindings);
        match home.get(&binding.name) {
            Some(current) if current.version().0.to_string() == binding.version => {}
            Some(_) => return Err(format!("account binding `{}` moved since it was read", binding.name)),
            None => return Err(format!("account binding `{}` is not held here", binding.name)),
        }
        home.apply(&binding.name, edge, &(), now).map(drop).map_err(|error| match error {
            WriteError::Refused(refusal) => format!("account binding `{}` refused: {refusal:?}", binding.name),
            other => format!("account binding `{}`: {other:?}", binding.name),
        })
    }
}

/// Reads `identity` again into the resident maps: its current rows, or none once deleted.
fn reload(read: &mut Read<'_>, resident: &mut Resident, identity: &Identity) -> Result<(), StoreError> {
    let key = (identity.home.clone(), identity.name.clone());
    let id = read.resolve(identity.kind, &identity.name, &identity.home)?;
    if identity.kind == BINDING {
        match id.map(|id| read.load::<Resource<AccountBinding>>(id)).transpose()?.flatten() {
            Some(loaded) => resident.bindings.insert(key, loaded.value.with_identity(&loaded.name, loaded.version)),
            None => resident.bindings.remove(&key),
        };
    } else if identity.kind == PROVISION {
        match id.map(|id| read.load::<ProvisionSpec>(id)).transpose()?.flatten() {
            Some(loaded) => {
                resident.provisions.insert(key, Plain { name: loaded.name, home: loaded.home, version: loaded.version, spec: loaded.value })
            }
            None => resident.provisions.remove(&key),
        };
    }
    Ok(())
}

type Bindings = BTreeMap<(String, String), Resource<AccountBinding>>;
type Provisions = BTreeMap<(String, String), Plain<ProvisionSpec>>;

/// Reads every account record homed at `home` into the resident maps, in place of what they held.
fn rescan(read: &mut Read<'_>, resident: &mut Resident, home: &str) -> Result<(), StoreError> {
    let (bindings, provisions) = scan_home(read, home)?;
    resident.bindings.retain(|(owner, _), _| owner != home);
    resident.provisions.retain(|(owner, _), _| owner != home);
    resident.bindings.extend(bindings);
    resident.provisions.extend(provisions);
    Ok(())
}

/// Every account record homed at `home`.
fn scan_home(read: &mut Read<'_>, home: &str) -> Result<(Bindings, Provisions), StoreError> {
    let mut bindings = Bindings::new();
    let mut after = None;
    loop {
        let page = read.scan::<Resource<AccountBinding>>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<Resource<AccountBinding>>(id)? {
                bindings.insert((home.to_owned(), loaded.name.clone()), loaded.value.with_identity(&loaded.name, loaded.version));
            }
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    let mut provisions = Provisions::new();
    let mut after = None;
    loop {
        let page = read.scan::<ProvisionSpec>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<ProvisionSpec>(id)? {
                provisions.insert(
                    (home.to_owned(), loaded.name.clone()),
                    Plain { name: loaded.name, home: loaded.home, version: loaded.version, spec: loaded.value },
                );
            }
        }
        after = page.next;
        if after.is_none() {
            return Ok((bindings, provisions));
        }
    }
}

/// The engine's binding spec of an old-shape binding.
pub fn binding_spec(old: &AccountBindingSpec) -> BindingSpec {
    BindingSpec {
        requester: old.requester.clone(),
        item: old.item.clone(),
        requirement: old.requirement.clone(),
        credential: old.credential.clone(),
        account: old.account.clone(),
        grants: old.grants.clone(),
        candidates: old.candidates.clone(),
        exclusions: old.exclusions.clone(),
        selection: old.selection.clone(),
        epochs: old.epochs.clone(),
        opened_at: old.opened_at,
    }
}

/// The engine's state of an old-shape binding state.
pub fn binding_state(old: &OldState) -> BindingState {
    match old {
        OldState::Open => BindingState::Open,
        OldState::Closed { reason, delivered_until } => BindingState::Closed { reason: reason.clone(), delivered_until: *delivered_until },
        OldState::Revoked { reason, delivered_until } => {
            BindingState::Revoked { reason: reason.clone(), delivered_until: *delivered_until }
        }
    }
}

fn record(binding: &Resource<AccountBinding>, home: &str) -> BindingRecord {
    let spec = binding.spec();
    let stored = binding.stored_state();
    let state = match &stored.value {
        BindingState::Open => OldState::Open,
        BindingState::Closed { reason, delivered_until } => OldState::Closed { reason: reason.clone(), delivered_until: *delivered_until },
        BindingState::Revoked { reason, delivered_until } => {
            OldState::Revoked { reason: reason.clone(), delivered_until: *delivered_until }
        }
    };
    BindingRecord {
        name: binding.name().to_owned(),
        home: home.to_owned(),
        version: binding.version().0.to_string(),
        spec: AccountBindingSpec {
            requester: spec.requester.clone(),
            item: spec.item.clone(),
            requirement: spec.requirement.clone(),
            credential: spec.credential.clone(),
            account: spec.account.clone(),
            grants: spec.grants.clone(),
            candidates: spec.candidates.clone(),
            exclusions: spec.exclusions.clone(),
            selection: spec.selection.clone(),
            epochs: spec.epochs.clone(),
            opened_at: spec.opened_at,
            state: State::new(state, stored.since),
        },
    }
}

/// The engine's provision spec of an old-shape one.
pub fn provision_spec(old: &AccountProvisionSpec) -> ProvisionSpec {
    ProvisionSpec {
        binding: Ref::new(old.binding.name.clone(), old.binding.home.clone()),
        place: old.place.clone(),
        environment_kind: old.environment_kind.clone(),
        plan: old.plan.clone(),
        home: old.home.clone(),
        delivered: old.delivered.clone(),
    }
}

fn provision_record(provision: &Plain<ProvisionSpec>) -> ProvisionRecord {
    let spec = &provision.spec;
    ProvisionRecord {
        name: provision.name.clone(),
        home: provision.home.clone(),
        version: provision.version.to_string(),
        spec: AccountProvisionSpec {
            binding: Ref::new(spec.binding.name.clone(), spec.binding.home.clone()),
            place: spec.place.clone(),
            environment_kind: spec.environment_kind.clone(),
            plan: spec.plan.clone(),
            home: spec.home.clone(),
            delivered: spec.delivered.clone(),
        },
    }
}

fn convoy_of(binding: &Resource<AccountBinding>) -> Option<String> {
    requester_parts(&binding.spec().requester).map(|(convoy, _)| convoy)
}

/// The requester's current binding among `bindings`: its open one, or else its latest opened.
fn current<'a>(
    bindings: impl Iterator<Item = (&'a String, &'a Resource<AccountBinding>)>,
) -> Option<(&'a String, &'a Resource<AccountBinding>)> {
    bindings.max_by_key(|(_, binding)| (matches!(binding.state(), BindingState::Open), binding.spec().opened_at))
}

#[async_trait::async_trait]
impl AccountRecords for EngineAccounts {
    async fn binding(&self, _namespace: &str, name: &str) -> Result<Option<BindingRecord>, String> {
        let resident = self.refresh()?;
        Ok(resident.bindings.iter().find(|((_, held), _)| held == name).map(|((home, _), binding)| record(binding, home)))
    }

    async fn requester_binding(
        &self,
        _namespace: &str,
        convoy: &str,
        role: &str,
        access: &AccessName,
    ) -> Result<Option<BindingRecord>, String> {
        let resident = self.refresh()?;
        let bindings = resident.bindings.iter().filter(|(_, binding)| {
            binding.spec().requirement == *access
                && requester_parts(&binding.spec().requester).is_some_and(|(c, r)| c == convoy && r == role)
        });
        Ok(current(bindings.map(|((home, _), binding)| (home, binding))).map(|(home, binding)| record(binding, home)))
    }

    async fn convoy_bindings(&self, _namespace: &str, convoy: &str, local: bool) -> Result<Vec<BindingRecord>, String> {
        let resident = self.refresh()?;
        Ok(resident
            .bindings
            .iter()
            .filter(|((home, _), binding)| (!local || *home == self.home) && convoy_of(binding).as_deref() == Some(convoy))
            .map(|((home, _), binding)| record(binding, home))
            .collect())
    }

    async fn bound_convoys(&self, _namespace: &str, credentials: &[String]) -> Result<(BTreeSet<String>, BTreeSet<String>), String> {
        let resident = self.refresh()?;
        let local = resident.bindings.iter().filter(|((home, _), _)| *home == self.home).map(|(_, binding)| binding);
        let mut using = BTreeSet::new();
        let mut bound = BTreeSet::new();
        for binding in local {
            let Some(convoy) = convoy_of(binding) else { continue };
            if credentials.contains(&binding.spec().credential.0) {
                using.insert(convoy.clone());
            }
            bound.insert(convoy);
        }
        Ok((using, bound))
    }

    async fn open(&self, _namespace: &str, bindings: Vec<PendingBinding>) -> Result<(), String> {
        for pending in bindings {
            if self.open_binding(&pending.spec.requester, &pending.spec.requirement).is_some() {
                continue;
            }
            self.create(&pending)?;
        }
        Ok(())
    }

    async fn end(&self, _namespace: &str, binding: &BindingRecord, state: OldState, now: Timestamp) -> Result<(), String> {
        match state {
            OldState::Closed { reason, delivered_until } => self.apply(binding, Close { reason, delivered_until }, now),
            OldState::Revoked { reason, delivered_until } => self.apply(binding, Revoke { reason, delivered_until }, now),
            OldState::Open => Err(format!("account binding `{}` cannot be ended as open", binding.name)),
        }
    }

    async fn regrant(&self, _namespace: &str, binding: &BindingRecord, grants: BTreeMap<GrantName, GrantScope>) -> Result<(), String> {
        self.apply(binding, Regrant { grants }, self.now())
    }

    async fn observe_epoch(&self, _namespace: &str, binding: &BindingRecord, epoch: EpochEntry) -> Result<(), String> {
        self.apply(binding, ObserveEpoch { epoch: epoch.epoch, observed_at: epoch.observed_at }, epoch.observed_at)
    }

    async fn rebind(
        &self,
        _namespace: &str,
        opened: PendingBinding,
        delivered_until: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<(), String> {
        // The next binding's name is checked before the open one closes, so a refused rebind
        // leaves the requester bound: the close and the create are two writes.
        let next = engine_binding_name(&opened.name, opened.spec.opened_at);
        if locked(&self.bindings).get(&next).is_some() {
            return Err(format!("account binding `{next}` already exists: a rebind must open after the binding it replaces"));
        }
        if let Some(name) = self.open_binding(&opened.spec.requester, &opened.spec.requirement) {
            locked(&self.bindings)
                .apply(&name, Close { reason: "rebind".to_string(), delivered_until }, &(), now)
                .map_err(|error| format!("close account binding `{name}` for a rebind: {error:?}"))?;
        }
        self.create(&opened)
    }

    async fn provision(&self, _namespace: &str, name: &str) -> Result<Option<ProvisionRecord>, String> {
        let resident = self.refresh()?;
        Ok(resident.provisions.get(&(self.home.clone(), name.to_owned())).map(provision_record))
    }

    async fn binding_provisions(&self, _namespace: &str, bindings: &[String], local: bool) -> Result<Vec<ProvisionRecord>, String> {
        let resident = self.refresh()?;
        Ok(resident
            .provisions
            .iter()
            .filter(|((home, _), provision)| (!local || *home == self.home) && bindings.contains(&provision.spec.binding.name))
            .map(|(_, provision)| provision_record(provision))
            .collect())
    }

    async fn local_provisions(&self, _namespace: &str) -> Result<Vec<ProvisionRecord>, String> {
        let resident = self.refresh()?;
        Ok(resident
            .provisions
            .iter()
            .filter(|((home, _), _)| *home == self.home)
            .map(|(_, provision)| provision_record(provision))
            .collect())
    }

    async fn record_provision(
        &self,
        _namespace: &str,
        name: &str,
        _convoy: &str,
        spec: &AccountProvisionSpec,
        replace: bool,
    ) -> Result<(), String> {
        let spec = provision_spec(spec);
        let now = self.now();
        let written = match self.provisions.get::<ProvisionSpec>(name, &self.home).map_err(|error| error.to_string())? {
            Some(existing) if !replace || existing.spec == spec => return Ok(()),
            Some(existing) => self.provisions.edit(name, existing.version, &spec, WRITER, now).map(drop),
            None => self.provisions.create(name, &spec, WRITER, now).map(drop),
        };
        written.map_err(|error| format!("record account provision `{name}`: {error}"))
    }

    async fn update_provision(
        &self,
        _namespace: &str,
        provision: &ProvisionRecord,
        spec: &AccountProvisionSpec,
    ) -> Result<ProvisionWrite, String> {
        let version = provision.version.parse::<u64>().map_err(|error| format!("provision version: {error}"))?;
        match self.provisions.edit(&provision.name, version, &provision_spec(spec), WRITER, self.now()) {
            Ok(_) => Ok(ProvisionWrite::Written),
            Err(PlainError::Conflict { .. } | PlainError::Missing { .. }) => Ok(ProvisionWrite::Superseded),
            Err(error) => Err(format!("write account provision `{}`: {error}", provision.name)),
        }
    }

    async fn delete_provision(&self, _namespace: &str, name: &str) -> Result<(), String> {
        let Some(existing) = self.provisions.get::<ProvisionSpec>(name, &self.home).map_err(|error| error.to_string())? else {
            return Ok(());
        };
        match self.provisions.delete::<ProvisionSpec>(name, existing.version, WRITER, self.now()) {
            Ok(()) | Err(PlainError::Missing { .. }) => Ok(()),
            Err(error) => Err(format!("remove account provision `{name}`: {error}")),
        }
    }

    async fn changes(&self, _namespace: &str) -> Result<BoxStream<'static, Result<AccountChange, String>>, String> {
        let service = Arc::clone(&self.service);
        let mut follower = Follower::new([BINDING, PROVISION]);
        let homes = service.homes();
        service.with_store(|store| follower.start(&mut store.read()?, &homes)).map_err(store_error)?;
        let stream = futures::stream::unfold(
            (service, follower, std::collections::VecDeque::new()),
            |(service, mut follower, mut queue)| async move {
                loop {
                    if let Some(change) = queue.pop_front() {
                        return Some((Ok(change), (service, follower, queue)));
                    }
                    tokio::time::sleep(POLL).await;
                    let homes = service.homes();
                    let found = service.with_store(|store| {
                        let mut read = store.read()?;
                        let mut changes = Vec::new();
                        for followed in follower.pass(&mut read, &homes)? {
                            match followed {
                                Followed::Changed(identity) => changes.extend(change(&mut read, &identity)?),
                                Followed::Rescan(home) => changes.extend(rescanned(&mut read, &home)?),
                            }
                        }
                        Ok(changes)
                    });
                    match found {
                        Ok(changes) => queue.extend(changes),
                        Err(error) => return Some((Err(error.to_string()), (service, follower, queue))),
                    }
                }
            },
        );
        Ok(Box::pin(stream))
    }
}

/// The change `identity`'s entry makes, as the controllers are woken by it. A provision's convoy
/// is its binding's, while this store holds the binding.
fn change(read: &mut Read<'_>, identity: &Identity) -> Result<Option<AccountChange>, StoreError> {
    let id = read.resolve(identity.kind, &identity.name, &identity.home)?;
    if identity.kind == BINDING {
        let convoy =
            id.map(|id| read.load::<Resource<AccountBinding>>(id)).transpose()?.flatten().and_then(|loaded| convoy_of(&loaded.value));
        return Ok(Some(AccountChange::Binding { name: identity.name.clone(), convoy }));
    }
    match id.map(|id| read.load::<ProvisionSpec>(id)).transpose()?.flatten() {
        Some(loaded) => {
            let convoy = provision_convoy(read, &loaded.value)?;
            Ok(Some(AccountChange::Provision { name: identity.name.clone(), binding: loaded.value.binding.name, convoy }))
        }
        // A removed provision wakes only its place's controller, which finds it gone.
        None => Ok(Some(AccountChange::Provision { name: identity.name.clone(), binding: String::new(), convoy: None })),
    }
}

/// The convoy of a provision's binding, if this store holds the binding.
fn provision_convoy(read: &mut Read<'_>, provision: &ProvisionSpec) -> Result<Option<String>, StoreError> {
    let Some(id) = read.resolve(BINDING, &provision.binding.name, &provision.binding.home)? else { return Ok(None) };
    Ok(read.load::<Resource<AccountBinding>>(id)?.and_then(|loaded| convoy_of(&loaded.value)))
}

/// A change for every account record homed at `home`.
fn rescanned(read: &mut Read<'_>, home: &str) -> Result<Vec<AccountChange>, StoreError> {
    let (bindings, provisions) = scan_home(read, home)?;
    let mut changes: Vec<_> =
        bindings.iter().map(|((_, name), binding)| AccountChange::Binding { name: name.clone(), convoy: convoy_of(binding) }).collect();
    for ((_, name), provision) in &provisions {
        let convoy = provision_convoy(read, &provision.spec)?;
        changes.push(AccountChange::Provision { name: name.clone(), binding: provision.spec.binding.name.clone(), convoy });
    }
    Ok(changes)
}

/// The requester binding name ([`binding_name`]) an engine binding was opened for.
pub fn requester_name(binding: &BindingSpec) -> Option<String> {
    requester_parts(&binding.requester).map(|(convoy, role)| binding_name(&convoy, &role, &binding.requirement))
}

/// Maps the old store's bindings and provisions (each with its home) onto the engine and
/// rehearses them into one temporary store, bindings first, so each provision's reference finds
/// its binding (ADR 0047: the previous generation's records decode). A binding takes its engine
/// name from its requester and opening; a provision is re-pointed at that name. Counted by kind
/// and state; refusals are named.
pub fn rehearse_accounts(
    bindings: &[(flotilla_resources::ResourceObject<flotilla_resources::AccountBinding>, String)],
    provisions: &[(flotilla_resources::ResourceObject<flotilla_resources::AccountProvision>, String)],
    report: &mut super::import::ImportReport,
) {
    use super::import::ImportRefusal;
    use flotilla_state_machines::Version;
    let mut kinds = flotilla_schema::KindRegistry::new();
    let rehearsal = stored::register(&mut kinds)
        .map_err(|error| format!("registry: {error:?}"))
        .and_then(|()| flotilla_engine_store::Rehearsal::open(kinds));
    let mut rehearsal = match rehearsal {
        Ok(rehearsal) => rehearsal,
        Err(reason) => {
            let refuse = |kind, record: &str| ImportRefusal { kind, record: record.to_owned(), reason: reason.clone() };
            report.refused.extend(bindings.iter().map(|(binding, _)| refuse("AccountBinding", &binding.metadata.name)));
            report.refused.extend(provisions.iter().map(|(provision, _)| refuse("AccountProvision", &provision.metadata.name)));
            return;
        }
    };
    let mut renamed = BTreeMap::new();
    let mut taken = BTreeSet::new();
    for (old, home) in bindings {
        let refuse = |reason: String| ImportRefusal { kind: "AccountBinding", record: old.metadata.name.clone(), reason };
        let spec = binding_spec(&old.spec);
        let Some(requester) = requester_name(&spec) else {
            report.refused.push(refuse(format!("requester `{}` names no convoy and role", spec.requester)));
            continue;
        };
        let name = engine_binding_name(&requester, spec.opened_at);
        if !taken.insert((home.clone(), name.clone())) {
            report.refused.push(refuse(format!("another binding of its requester opened at the same time already imports as `{name}`")));
            continue;
        }
        let state = binding_state(&old.spec.state.value);
        let label = match state {
            BindingState::Open => "open",
            BindingState::Closed { .. } => "closed",
            BindingState::Revoked { .. } => "revoked",
        };
        let resource = Resource::<AccountBinding>::stored(name.clone(), spec, State::new(state, old.spec.state.since), Version(1));
        match rehearsal.restore(home, &name, 1, &resource) {
            Ok(()) => {
                *report.imported.entry(("AccountBinding", label)).or_default() += 1;
                renamed.insert((home.clone(), old.metadata.name.clone()), (home.clone(), name));
            }
            Err(reason) => report.refused.push(refuse(reason)),
        }
    }
    for (old, home) in provisions {
        let refuse = |reason: String| ImportRefusal { kind: "AccountProvision", record: old.metadata.name.clone(), reason };
        // A daemon's own records list without an origin root; a provision names its binding's
        // home by node id, so a binding listed as this daemon's is found by name alone.
        let named = renamed.get(&(old.spec.binding.home.clone(), old.spec.binding.name.clone())).or_else(|| {
            let mut local = renamed.iter().filter(|((_, held), _)| *held == old.spec.binding.name).map(|(_, renamed)| renamed);
            local.next().filter(|_| local.next().is_none())
        });
        let Some((binding_home, binding)) = named else {
            report
                .refused
                .push(refuse(format!("names binding `{}` at `{}`, which is not imported", old.spec.binding.name, old.spec.binding.home)));
            continue;
        };
        let mut spec = provision_spec(&old.spec);
        spec.binding = Ref::new(binding.clone(), binding_home.clone());
        let label = if spec.delivered.is_some() { "delivered" } else { "recorded" };
        match rehearsal.holds::<Resource<AccountBinding>>(&spec.binding.name, &spec.binding.home) {
            Ok(true) => {}
            Ok(false) => {
                report.refused.push(refuse(format!("its binding `{}` at `{}` is not restored", spec.binding.name, spec.binding.home)));
                continue;
            }
            Err(reason) => {
                report.refused.push(refuse(reason));
                continue;
            }
        }
        match rehearsal.restore(home, &old.metadata.name, 1, &spec) {
            Ok(()) => *report.imported.entry(("AccountProvision", label)).or_default() += 1,
            Err(reason) => report.refused.push(refuse(reason)),
        }
    }
}
