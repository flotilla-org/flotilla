//! Conservative owned-byte accounting for the rebuildable replica. The backing
//! store/log and caller-owned ports/evidence are not replica allocations.
use std::mem::size_of;

use flotilla_schema::Address;
use flotilla_state_machines::{machines::message::*, Resource, Transport};

/// Default share of the root daemon's 1 GiB budget. Stored history is not resident.
pub const RESIDENT_BUDGET: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Residency {
    pub budget_bytes: usize,
    pub accounted_bytes: usize,
    pub open_messages: usize,
    pub pending_index_complete: bool,
    pub oldest_state_since: Option<flotilla_schema::Timestamp>,
    pub newest_state_since: Option<flotilla_schema::Timestamp>,
}

pub(crate) fn address(value: &Address) -> usize {
    // Address owns canonical text, decoded segment strings and a segment Vec.
    // Charge slack and an optional artifact subject from the canonical length.
    size_of::<Address>() + value.as_str().len() * 8 + value.segments().len() * 128
}

pub(crate) fn receiver(value: &Receiver) -> usize {
    size_of::<Receiver>() + value.id.capacity() + value.host.capacity() + 96
}

pub(crate) fn binding(inbox: &Address, value: &InboxBinding) -> usize {
    address(inbox)
        + size_of::<InboxBinding>()
        + 128
        + match value {
            InboxBinding::Unbound { .. } => 0,
            InboxBinding::Bound { consumers } => consumers.iter().map(receiver).sum(),
        }
}

pub(crate) fn resource(value: &Resource<Message>, receipts: Option<&std::collections::BTreeMap<Receiver, Transport>>) -> usize {
    let spec = value.spec();
    // Keys in Home, LRU, size metadata and the optional pending index, plus
    // BTree node slack. Counting shared frozen allocations per handle is conservative.
    let mut bytes = size_of::<Resource<Message>>() + 1024 + value.name().len() * 8;
    bytes += address(&spec.sender) + address(&spec.inbox) * 2;
    bytes += spec.body.bytes().len()
        + match spec.body.value() {
            MessageBody::Text(text) => text.capacity(),
            MessageBody::Request(RequestBody::Convoy(request)) => {
                address(&request.project)
                    + request.quota.len() * 128
                    + request.promises.values().map(|p| 128 + p.kind.capacity() + address(&p.target)).sum::<usize>()
            }
        };
    bytes += spec.references.capacity() * size_of::<MessageReference>();
    for reference in &spec.references {
        bytes += address(&reference.address) + reference.revision.capacity();
    }
    for reference in [&spec.in_reply_to, &spec.supersedes].into_iter().flatten() {
        bytes += reference.name.capacity() + reference.home.capacity();
    }
    if let Expectation::Outcome { condition } = &spec.expectation {
        bytes += address(condition);
    }
    if let Some(condition) = &spec.delivery_condition {
        bytes += address(condition);
    }
    if let Some(receiver) = value.state().receiver() {
        bytes += self::receiver(receiver);
    }
    bytes += receipts.map_or(0, |rows| 128 + rows.keys().map(self::receiver).sum::<usize>());
    bytes
}

pub(crate) const WATCH_PAGE_BYTES: usize = 16 * 1024;

pub(crate) fn change(inbox: &Address, change: &crate::Change) -> usize {
    let mut bytes = address(inbox) + size_of::<crate::Change>() + change.message.capacity() + 128;
    match &change.kind {
        crate::ChangeKind::Receipt { consumer, .. } => bytes += receiver(consumer),
        crate::ChangeKind::Message(state) => {
            if let Some(value) = state.receiver() {
                bytes += receiver(value);
            }
            if let Some(reason) = state.drop_reason() {
                match reason {
                    DropReason::Superseded { by } => bytes += address(by),
                    DropReason::Withdrawn { by, authority } | DropReason::ConsumerUnreachable { by, authority } => {
                        bytes += address(by) + address(authority)
                    }
                    _ => {}
                }
            }
        }
    }
    bytes
}
