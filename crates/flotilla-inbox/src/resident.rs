//! Conservative owned-byte accounting for the rebuildable replica. The backing
//! store/log and caller-owned ports/evidence are not replica allocations.
//! `tests/resident_allocations.rs` calibrates these heuristic charges against
//! live Rust allocations; extend its shape coverage when accepted types change.
use std::{
    collections::BTreeMap,
    mem::{size_of, size_of_val},
};

use flotilla_schema::{Address, Timestamp};
use flotilla_state_machines::{
    machines::{
        convoy::{ConvoyRequest, Promise, PromiseId, VesselClass},
        message::*,
    },
    Resource, Transport,
};

/// Independent budget for cached complete inbox orders; overflow uses the store index.
pub const PENDING_BUDGET: usize = 128 * 1024 * 1024;
/// Explicit control setup ceiling within RESIDENT_BUDGET; required live clocks may exceed it.
pub const CONTROL_BUDGET: usize = 96 * 1024 * 1024;
/// Target for cached values and controls; normal total with Pending is 256 MiB.
pub const RESIDENT_BUDGET: usize = 128 * 1024 * 1024;
const _: () = assert!(CONTROL_BUDGET <= RESIDENT_BUDGET);

#[derive(Clone, Debug)]
pub struct Residency {
    pub budget_bytes: usize,
    /// Values plus controls; Pending keys have their independent fields below.
    pub accounted_bytes: usize,
    pub value_bytes: usize,
    pub control_bytes: usize,
    /// Effective setup ceiling; required live clocks may exceed it.
    pub control_budget_bytes: usize,
    pub open_messages: usize,
    pub pending_bytes: usize,
    pub pending_budget_bytes: usize,
    pub pending_messages: usize,
    pub oldest_state_since: Option<Timestamp>,
    pub newest_state_since: Option<Timestamp>,
}

pub(crate) fn address(value: &Address) -> usize {
    // Address clones allocate canonical text, segment Vec and decoded strings.
    // Two segment slots per live segment cover Vec growth; canonical length
    // plus parse-time String growth bounds decoded values/optional subject.
    // A factor of two failed calibration for a 142-byte parsed address; four
    // covers that capacity. Calibration measures freshly parsed and cloned shapes.
    size_of::<Address>() + value.as_str().len() * 4 + size_of_val(value.segments()) * 2
}

pub(crate) fn receiver(value: &Receiver) -> usize {
    size_of::<Receiver>() + value.id.capacity() + value.host.capacity() + 96
}

pub(crate) fn binding(inbox: &Address, value: &InboxBinding) -> usize {
    address(inbox)
        + size_of::<InboxBinding>()
        + 2 * size_of::<usize>()
        + 128
        + match value {
            InboxBinding::Unbound { .. } => 0,
            InboxBinding::Bound { consumers } => {
                if consumers.is_empty() {
                    0
                } else {
                    512 + consumers.iter().map(receiver).sum::<usize>() * 2
                }
            }
        }
}

pub(crate) fn resource(value: &Resource<Message>, receipts: Option<&BTreeMap<Receiver, Transport>>) -> usize {
    // Exhaustive destructuring makes every new spec field a compile error here.
    let MessageSpec {
        sender,
        inbox,
        admitted_seq: _,
        relation: _,
        body,
        references,
        in_reply_to,
        supersedes,
        expectation,
        deadline: _,
        unbound_after: _,
        delivery_condition,
    } = value.spec();
    let heap_address = |value: &Address| address(value) - size_of::<Address>();
    // Home's resource map, accounting map and LRU map: two slots per live row
    // bound BTree node slack. Headers inside Resource are counted only once.
    let mut bytes = 2 * (size_of::<(String, Resource<Message>)>() + size_of::<(String, (usize, u64))>() + size_of::<(u64, String)>())
        + value.name().len() * 3;
    bytes += heap_address(sender) + heap_address(inbox);
    bytes += body.bytes().len()
        + 32
        + size_of::<MessageBody>()
        + match body.value() {
            MessageBody::Text(text) => text.capacity(),
            MessageBody::Request(RequestBody::Convoy(request)) => {
                let ConvoyRequest { project, promises, quota, priority: _ } = request;
                let nodes = |len: usize, slot: usize| if len == 0 { 0 } else { len.saturating_mul(2).max(11) * slot + (len / 5 + 1) * 96 };
                heap_address(project)
                    + nodes(quota.len(), size_of::<(VesselClass, u32)>())
                    + nodes(promises.len(), size_of::<(PromiseId, Promise)>())
                    + promises.values().map(|p| p.kind.capacity() + heap_address(&p.target)).sum::<usize>()
            }
        };
    bytes += references.capacity() * size_of::<MessageReference>();
    for reference in references {
        bytes += heap_address(&reference.address) + reference.revision.capacity();
    }
    for reference in [in_reply_to, supersedes].into_iter().flatten() {
        bytes += reference.name.capacity() + reference.home.capacity();
    }
    if let Expectation::Outcome { condition } = expectation {
        bytes += heap_address(condition);
    }
    if let Some(condition) = delivery_condition {
        bytes += heap_address(condition);
    }
    if let Some(receiver) = value.state().receiver() {
        bytes += receiver.id.capacity() + receiver.host.capacity();
    }
    if let Some(reason) = value.state().drop_reason() {
        match reason {
            DropReason::Superseded { by } => bytes += heap_address(by),
            DropReason::Withdrawn { by, authority } | DropReason::ConsumerUnreachable { by, authority } => {
                bytes += heap_address(by) + heap_address(authority)
            }
            DropReason::Undeliverable | DropReason::Expired => {}
        }
    }
    bytes += receipts.map_or(0, |rows| {
        let nodes = if rows.is_empty() {
            0
        } else {
            rows.len().saturating_mul(2).max(11) * size_of::<(Receiver, Transport)>() + (rows.len() / 5 + 1) * 96
        };
        128 + value.name().len() + nodes + rows.keys().map(|receiver| receiver.id.capacity() + receiver.host.capacity()).sum::<usize>()
    });
    bytes
}

pub(crate) fn pending(inbox: &Address, name: &str, new_inbox: bool) -> usize {
    name.len() + 2 * size_of::<(u64, String)>() + if new_inbox { 2 * (address(inbox) + size_of::<BTreeMap<u64, String>>()) } else { 0 }
}
