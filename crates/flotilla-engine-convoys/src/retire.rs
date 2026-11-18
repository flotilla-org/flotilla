//! Retiring this home's ended convoys (#3108 retirement): the convoy's home is Retire's one
//! writer. It tries an ended convoy when it ends, and again whenever a record of a hold kind
//! naming it is written, so no timer runs. The ended convoys still held here are kept in memory,
//! and a write of a hold kind is mapped to the convoy its record names; when nothing is held,
//! no record is read for it. A dependent deleted outright names nothing any more, so its
//! deletion tries every held convoy.

use std::{collections::BTreeSet, sync::Arc};

use flotilla_engine_store::{follow::Followed, watch::EngineWatch};
use flotilla_provisioning::stored::{account::AccountBinding, checkout::Checkout, standing_role::StandingRoleRecord};
use flotilla_resources::{
    access_delivery::convoy_of_access, account_access::requester_parts, convoy::engine::convoy_records, AccessDeliverySpec, Clock,
};
use flotilla_schema::{reads, Identity, Kind, KindCode, Timestamp};
use flotilla_schema_sqlite::{Read, StoreError};
use flotilla_state_machines::{
    machines::{
        awaiting_reply::AwaitingReply,
        convoy::{Convoy, ConvoyLabel},
        message::Message,
        vessel::Vessel,
    },
    Resource,
};

use super::convoys::{Convoys, Retirement};

/// The kinds whose writes can retire a convoy: the convoy's own, which ends it and keeps or
/// returns its pr promise, and each other hold's.
fn followed_kinds() -> [KindCode; 8] {
    [
        <StandingRoleRecord as Kind>::CODE,
        <Resource<Convoy> as Kind>::CODE,
        <Resource<Message> as Kind>::CODE,
        <Resource<AwaitingReply> as Kind>::CODE,
        <Resource<Checkout> as Kind>::CODE,
        <Resource<Vessel> as Kind>::CODE,
        <AccessDeliverySpec as Kind>::CODE,
        <Resource<AccountBinding> as Kind>::CODE,
    ]
}

/// Retires this home's convoys as they end and as their holds release, from the store's log.
/// Every ended convoy homed here is tried once at the start.
pub fn follow(convoys: Arc<Convoys>, clock: Arc<dyn Clock>) -> Result<tokio::task::JoinHandle<()>, String> {
    let caller = reads::caller(FOLLOWER);
    let service = convoys.service();
    let mut watch = EngineWatch::start(Arc::clone(&service), followed_kinds()).map_err(|error| error.to_string())?;
    let home = convoys.home().to_owned();
    let ended = {
        let home = home.clone();
        reads::within(&caller, || service.with_store(move |store| ended_here(&mut store.read()?, &home)))
            .map_err(|error| error.to_string())?
    };
    // Tried before the follower starts, so what is held at the start is known when it does.
    let mut held = BTreeSet::new();
    keep(&mut held, reads::within(&caller, || retire_each(&convoys, Timestamp::from_datetime(&clock.now()), ended)));
    Ok(tokio::spawn(reads::attributed(caller, async move {
        loop {
            let (home, waiting) = (home.clone(), held.clone());
            match watch.next(move |read, followed| candidates(read, followed, &home, &waiting)).await {
                Ok(found) => {
                    let names = found.names(&held);
                    try_retire(&convoys, &clock, names, &mut held).await
                }
                Err(error) => tracing::warn!(%error, "the convoys' holds could not be read from the log; retrying"),
            }
        }
    })))
}

/// The caller the follower's reads count for.
const FOLLOWER: &str = "Convoy retirement";

/// What one pass of the log found to try.
#[derive(Default)]
struct Found {
    /// Convoys homed here, by name.
    convoys: BTreeSet<String>,
    /// Every held convoy is to be tried.
    every: bool,
}

impl Found {
    /// The convoys to try: those found, or every held one.
    fn names(self, held: &BTreeSet<String>) -> BTreeSet<String> {
        if self.every {
            return self.convoys.into_iter().chain(held.iter().cloned()).collect();
        }
        self.convoys
    }
}

/// Tries each of `names`, keeping the ones still held in `held`.
async fn try_retire(convoys: &Arc<Convoys>, clock: &Arc<dyn Clock>, names: BTreeSet<String>, held: &mut BTreeSet<String>) {
    if names.is_empty() {
        return;
    }
    let (convoys, now) = (Arc::clone(convoys), Timestamp::from_datetime(&clock.now()));
    match tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || retire_each(&convoys, now, names))).await {
        Ok(tried) => keep(held, tried),
        Err(error) => tracing::warn!(%error, "retiring convoys did not finish"),
    }
}

/// Tries to retire each of `names`.
fn retire_each(convoys: &Convoys, now: Timestamp, names: BTreeSet<String>) -> Vec<(Result<Retirement, String>, String)> {
    names.into_iter().map(|name| (convoys.retire(&name, now), name)).collect()
}

/// Keeps the convoys `tried` found still held in `held`.
fn keep(held: &mut BTreeSet<String>, tried: Vec<(Result<Retirement, String>, String)>) {
    for (retirement, name) in tried {
        match retirement {
            Ok(Retirement::Held(_)) => {
                held.insert(name);
            }
            Ok(Retirement::NotEnded | Retirement::Retired) => {
                held.remove(&name);
            }
            // Kept: the next write of a hold kind tries it again.
            Err(error) => {
                tracing::warn!(convoy = %name, %error, "a convoy could not be retired");
                held.insert(name);
            }
        }
    }
}

/// Every launch homed here that has ended and not retired.
fn ended_here(read: &mut Read<'_>, home: &str) -> Result<BTreeSet<String>, StoreError> {
    let mut ended = BTreeSet::new();
    let mut after = None;
    loop {
        let page = read.scan::<Resource<Convoy>>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<Resource<Convoy>>(id)? {
                if matches!(loaded.value.label(), ConvoyLabel::Delivered | ConvoyLabel::Abandoned) {
                    ended.insert(loaded.name);
                }
            }
        }
        after = page.next;
        if after.is_none() {
            return Ok(ended);
        }
    }
}

/// What `followed` may have retired: each convoy homed here that was written, and each held one
/// that a written dependent names. A rescan, or a dependent gone without a trace, tries every
/// held one. Nothing is read while nothing is held.
fn candidates(read: &mut Read<'_>, followed: Vec<Followed>, home: &str, held: &BTreeSet<String>) -> Result<Found, StoreError> {
    let convoy = <Resource<Convoy> as Kind>::CODE;
    let mut found = Found::default();
    for followed in followed {
        let identity = match followed {
            Followed::Changed(identity) => identity,
            Followed::Rescan(_) => {
                found.every = true;
                continue;
            }
        };
        if identity.kind == convoy {
            if identity.home == home {
                found.convoys.insert(identity.name);
            }
            continue;
        }
        if held.is_empty() || found.every {
            continue;
        }
        // A role's record names its ensure, not its generations; it is written once per end.
        if identity.kind == <StandingRoleRecord as Kind>::CODE {
            found.every |= identity.home == home;
            continue;
        }
        match named(read, &identity, home)? {
            Some(names) => found.convoys.extend(names.into_iter().filter(|name| held.contains(name))),
            None => found.every = true,
        }
    }
    Ok(found)
}

/// The convoys dependent `identity` names; `None` when it is gone.
fn named(read: &mut Read<'_>, identity: &Identity, home: &str) -> Result<Option<Vec<String>>, StoreError> {
    let Some(id) = read.resolve(identity.kind, &identity.name, &identity.home)? else { return Ok(None) };
    let kind = identity.kind;
    let names = if kind == <Resource<Message> as Kind>::CODE {
        read.load::<Resource<Message>>(id)?.map(|loaded| convoy_records(&loaded.value.spec().inbox))
    } else if kind == <Resource<AwaitingReply> as Kind>::CODE {
        read.load::<Resource<AwaitingReply>>(id)?.map(|loaded| loaded.value.spec().subject.as_ref().map(convoy_records).unwrap_or_default())
    } else if kind == <Resource<Checkout> as Kind>::CODE {
        read.load::<Resource<Checkout>>(id)?.map(|loaded| {
            let holder = loaded.value.spec().holder.as_ref().and_then(|holder| holder.0.strip_prefix("convoy:").map(str::to_owned));
            holder.into_iter().collect()
        })
    } else if kind == <Resource<Vessel> as Kind>::CODE {
        read.load::<Resource<Vessel>>(id)?.map(|loaded| {
            let convoy = &loaded.value.spec().convoy;
            if convoy.home == home {
                vec![convoy.name.clone()]
            } else {
                Vec::new()
            }
        })
    } else if kind == <AccessDeliverySpec as Kind>::CODE {
        read.load::<AccessDeliverySpec>(id)?.map(|loaded| convoy_of_access(&loaded.value.access).into_iter().collect())
    } else if kind == <Resource<AccountBinding> as Kind>::CODE {
        read.load::<Resource<AccountBinding>>(id)?
            .map(|loaded| requester_parts(&loaded.value.spec().requester).map(|(convoy, _)| convoy).into_iter().collect())
    } else {
        Some(Vec::new())
    };
    Ok(names)
}
