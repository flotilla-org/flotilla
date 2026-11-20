//! What holds an ended convoy (#3108 retirement): each hold is a fact on the dependent's own
//! record, written by that record's writer and naming the convoy, found through an index on that
//! reference. Replicas count: the indexes are read at every home this store holds.
//!
//! | Hold | The dependent's reference | Index |
//! |---|---|---|
//! | an unsettled Message to a seat | its inbox, the seat's address | Message `by_inbox`, Pending and Awaiting |
//! | an open AwaitingReply or stall | its subject, the convoy's or a seat's address | AwaitingReply `open_by_subject` |
//! | an open change request it promised | its own pr promise, Submitted until the merge keeps it or the close returns it to Pending | none: the convoy's own record |
//! | a live Checkout it holds | its holder, `convoy:<name>` | Checkout `by_holder` |
//! | a live Vessel of its work | its convoy and work | Vessel `by_launch` |
//! | an AccessDelivery to its crew | its access, a crew member's role address | AccessDelivery `by_access` |
//! | an open AccountBinding of its crew | its requester, a seat's role address | AccountBinding `by_requester` |
//! | a standing generation's end its role has not counted | its ensure, from its admission | StandingRole by name at its home |
//!
//! A change request linked with no pr promise holds nothing, and the change request's own record
//! is never read: the forge observer collects it once nothing demands it. The holds name each
//! dependent `kind:name`, the first a query finds, so a refusal says what it waits for.

use std::collections::BTreeSet;

use flotilla_address::{Address, Kind as AddressKind};
use flotilla_provisioning::stored::{
    account::{AccountBinding, BindingLabel, BindingSpec},
    checkout::{Checkout, CheckoutRequest},
    standing_role::StandingRoleRecord,
    Holder,
};
use flotilla_resources::{
    access_delivery::access_key,
    account_access::requester_address,
    convoy::{
        admission::AdmissionDocument,
        engine::{launch_address, seat_address, split_launch},
    },
    promises::PromiseKind,
    vessel_resource_name, AccessDeliverySpec, ENSURED_FROM_ANNOTATION,
};
use flotilla_schema::{IndexQuery, Kind};
use flotilla_schema_sqlite::{Read, ResourceId, StoreError};
use flotilla_state_machines::{
    machines::{
        awaiting_reply::AwaitingReplySpec,
        convoy::{Convoy, ConvoyId, Progress},
        message::{MessageLabel, MessageSpec},
        vessel::{Vessel, VesselSpec},
    },
    Resource,
};

/// One page of an index read.
const PAGE: u32 = 64;

/// What holds `convoy`, homed at `home`: empty when nothing does.
pub fn holds(read: &mut Read<'_>, convoy: &Resource<Convoy>, home: &str) -> Result<BTreeSet<String>, StoreError> {
    let name = convoy.name();
    let spec = convoy.spec();
    let project = &spec.project;
    let project_name = project.get(AddressKind::Project).unwrap_or_default();
    let unaddressable = |error: String| StoreError::Stream(format!("convoy {name}'s holds: {error}"));
    let mut holds = BTreeSet::new();

    let (convoy_address, seats) = addresses(convoy).map_err(unaddressable)?;
    let roles: BTreeSet<&str> = spec.seats.iter().map(|row| row.record.role.as_str()).collect();

    for seat in &seats {
        for label in [MessageLabel::Pending, MessageLabel::Awaiting] {
            first(read, &MessageSpec::by_inbox(seat, &label), &mut holds, "message")?;
        }
    }
    for subject in seats.iter().chain([&convoy_address]) {
        first(read, &AwaitingReplySpec::open_by_subject(subject), &mut holds, "awaiting-reply")?;
    }

    let pr = PromiseKind::Pr.as_str();
    for row in spec.promises.iter().filter(|row| row.record.kind == pr && row.record.progress.value == Progress::Submitted) {
        let promise = &row.record;
        match promise.submissions.iter().last() {
            Some(submission) => holds.insert(format!("change-request:{}", submission.record.address)),
            None => holds.insert(format!("promise:{}", promise.label)),
        };
    }

    each(read, &CheckoutRequest::by_holder(&Holder::convoy(name)), |read, id| {
        if let Some(checkout) = read.load::<Resource<Checkout>>(id)? {
            if checkout.value.state().live() {
                holds.insert(format!("checkout:{}", checkout.name));
            }
        }
        Ok(())
    })?;

    let admission = AdmissionDocument::decode(name, spec.admission.value().bytes()).map_err(|error| unaddressable(error.to_string()))?;
    let id = ConvoyId::new(name, home);
    for work in &admission.workflow_snapshot.vessels {
        let launch_of = vessel_resource_name(flotilla_resources::convoy::launch_of(name), &work.name);
        each(read, &VesselSpec::by_launch(&id, &launch_of), |read, vessel| {
            if let Some(vessel) = read.load::<Resource<Vessel>>(vessel)? {
                if vessel.value.state().live() {
                    holds.insert(format!("vessel:{}", vessel.name));
                }
            }
            Ok(())
        })?;
    }
    for access in crew_accesses(convoy)? {
        first(read, &AccessDeliverySpec::by_access(&access), &mut holds, "access-delivery")?;
    }

    // A standing generation's end is its role's next strike or reset: it holds until the role's
    // record at its home has counted it.
    if let Some(ensure) = admission.ensure_admission.as_ref().and(admission.provenance.get(ENSURED_FROM_ANNOTATION)) {
        let counted = match read.resolve(StandingRoleRecord::CODE, ensure, home)? {
            Some(id) => read.load::<StandingRoleRecord>(id)?.and_then(|role| role.value.counted),
            None => None,
        };
        if counted.is_none_or(|counted| counted < spec.launch) {
            holds.insert(format!("standing-role:{ensure}"));
        }
    }

    // A binding names its requester with or without the project, as its admission had one.
    for role in &roles {
        for requester in [requester_address(Some(project_name), name, role), requester_address(None, name, role)] {
            let requester = requester.map_err(unaddressable)?;
            each(read, &BindingSpec::by_requester(&requester), |read, id| {
                if let Some(binding) = read.load::<Resource<AccountBinding>>(id)? {
                    if binding.value.label() == BindingLabel::Open {
                        holds.insert(format!("account-binding:{}", binding.name));
                    }
                }
                Ok(())
            })?;
        }
    }
    Ok(holds)
}

/// Names the first resource `query` finds as `<label>:<name>`.
/// `convoy`'s address, its launch's when it is one, and the addresses of its seats.
pub(crate) fn addresses(convoy: &Resource<Convoy>) -> Result<(Address, Vec<Address>), String> {
    let (name, project) = (convoy.name(), &convoy.spec().project);
    let convoy_address = match split_launch(name) {
        Some(_) => launch_address(project, name)?,
        None => project.child(AddressKind::Convoy, name).map_err(|error| error.to_string())?,
    };
    let roles: BTreeSet<&str> = convoy.spec().seats.iter().map(|row| row.record.role.as_str()).collect();
    let seats = roles.into_iter().map(|role| seat_address(project, name, role)).collect::<Result<_, _>>()?;
    Ok((convoy_address, seats))
}

fn first<Q>(read: &mut Read<'_>, query: &IndexQuery<Q>, holds: &mut BTreeSet<String>, label: &str) -> Result<(), StoreError> {
    let page = read.page(query, None, 1)?;
    if let Some(id) = page.ids.first() {
        if let Some((identity, _)) = read.identity(*id)? {
            holds.insert(format!("{label}:{}", identity.name));
        }
    }
    Ok(())
}

/// The access keys of `convoy`'s crew, one per role of each work in its admission: what an
/// AccessDelivery to its crew is keyed by (`by_access`).
pub fn crew_accesses(convoy: &Resource<Convoy>) -> Result<Vec<String>, StoreError> {
    let name = convoy.name();
    let project = convoy.spec().project.get(AddressKind::Project).unwrap_or_default();
    let admission = AdmissionDocument::decode(name, convoy.spec().admission.value().bytes())
        .map_err(|error| StoreError::Stream(format!("convoy {name}'s crew accesses: {error}")))?;
    Ok(admission
        .workflow_snapshot
        .vessels
        .iter()
        .flat_map(|work| work.crew.iter().map(|crew| access_key(project, name, &work.name, &crew.role)))
        .collect())
}

/// Calls `each` with every resource `query` finds, a page at a time.
pub fn each<Q>(
    read: &mut Read<'_>,
    query: &IndexQuery<Q>,
    mut each: impl FnMut(&mut Read<'_>, ResourceId) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let mut after = None;
    loop {
        let page = read.page(query, after.as_ref(), PAGE)?;
        for id in page.ids {
            each(read, id)?;
        }
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(()),
        }
    }
}
