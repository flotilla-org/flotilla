//! Edge events: what one state-machine edge did, as the per-home log records it (#3392 G8).
//!
//! A machine's state is a value, the label plus its data (#3319). An edge's event is no row of
//! the kind: it is the header and arguments of the edge's entry in the store's per-home `log`,
//! read to show and audit what happened, never to decide. The header (edge, labels) is columns of
//! that entry; the arguments are one inline CBOR document whose shape can change without a schema
//! change. Who wrote it, when, and the records it read are the entry's change's (#4024).

use serde::{Deserialize, Serialize};

use crate::{code::Code, time::Timestamp, traits::CodeEnum};

/// One edge event: the header and arguments of the edge's `log` entry, and its time,
///
/// ```text
/// cause 'edge', edge, from, to | arguments          change: writer, at, items
///   (header columns)             (inline CBOR)       (the entry's change)
/// ```
///
/// `L` is the machine's state label and `E` its edge names, both code enums (as [`Edge::new`]
/// requires); `P` is the arguments document, any serde type. The edge in `edge` and the
/// document's arguments are the writer's to keep consistent.
///
/// [`Edge::new`]: crate::Edge::new
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EdgeEvent<L, E, P> {
    pub edge: E,
    /// The label the resource left.
    pub from: L,
    /// The label the resource entered: the label of the state the edge writes.
    pub to: L,
    /// When the edge happened: its change's time, and the `since` of a state it enters.
    pub at: Timestamp,
    /// The edge's arguments, logged on its entry.
    pub arguments: P,
}

/// A machine's state value: its label (a code enum, for listing, indexing and drawing) plus any
/// data the state carries. The derive implements it for every code enum (its own label) and for
/// every label-plus-data enum (`#[store(labels = L)]`, label `L`); `State<S>` has `S`'s label.
pub trait Labelled {
    type Label: CodeEnum;
    fn label(&self) -> &Self::Label;

    /// The label's code: what the state's label column stores, and what an edge event's `from`
    /// and `to` name.
    fn label_code(&self) -> Code {
        self.label().code()
    }
}
