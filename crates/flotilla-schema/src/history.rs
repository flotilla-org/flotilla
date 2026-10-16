//! Edge history rows: one row per state-machine edge event (#3320 ruling of 2026-10-15).
//!
//! A machine's state is a value, the label plus its data (#3319). The edge log is history only:
//! read to show and audit what happened, never to decide. Each event is a fixed header of columns,
//! indexed for the questions history answers, and one inline CBOR document holding the edge's
//! arguments and gate reads, whose shape can change without a schema change.

use serde::{Deserialize, Serialize};

use crate::{time::Timestamp, traits::CodeEnum, Stored};

/// One edge event, stored in the kind's `#[store(history)]` edge log as
///
/// ```text
/// owner, pos, version | edge, from, to, writer, at | payload
///   (primary key)       (header columns)             (inline CBOR: arguments and gate reads)
/// ```
///
/// `L` is the machine's state label and `E` its edge names, both code enums (as [`Edge::new`]
/// requires); `P` is the document, any serde type.
///
/// [`Edge::new`]: crate::Edge::new
/// The log's table is indexed for one resource's events in order (its primary key), events of one
/// edge in time order (`<table>_by_edge`) and time ranges across resources (`<table>_by_time`).
/// The edge in `edge` and the document's arguments are the writer's to keep consistent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Stored)]
// Clippy reads `order(at)` in both indexes as one attribute given twice.
#[allow(clippy::duplicated_attributes)]
#[store(index(name = "by_edge", eq(edge), order(at)), index(name = "by_time", order(at)))]
pub struct EdgeEvent<L, E, P> {
    pub edge: E,
    /// The label the resource left.
    pub from: L,
    /// The label the resource entered: the label of the state the edge writes.
    pub to: L,
    #[store(intern)]
    pub writer: String,
    pub at: Timestamp,
    #[store(cbor)]
    pub payload: P,
}

/// A machine's state value: its label (a code enum, for listing, indexing and drawing) plus any
/// data the state carries. The derive implements it for every code enum (its own label) and for
/// every label-plus-data enum (`#[store(labels = L)]`, label `L`); `State<S>` has `S`'s label.
pub trait Labelled {
    type Label: CodeEnum;
    fn label(&self) -> &Self::Label;

    /// The label's code: what the state's label column stores, and what an edge event's `from`
    /// and `to` name.
    fn label_code(&self) -> i64 {
        self.label().code()
    }
}
