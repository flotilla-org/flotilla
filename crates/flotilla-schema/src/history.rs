//! Edge events: what one state-machine edge did, as the per-home log records it (#3392 G8).
//!
//! A machine's state is a value, the label plus its data (#3319). An edge's event is no row of
//! the kind: it is the header and payload of the edge's entry in the store's per-home `log`,
//! read to show and audit what happened, never to decide. The header (edge, labels, writer and
//! time) is columns of that entry; the payload, the edge's arguments and recorded reads, is one
//! inline CBOR document whose shape can change without a schema change.

use serde::{Deserialize, Serialize};

use crate::{time::Timestamp, traits::CodeEnum};

/// One edge event: the header and payload of the edge's `log` entry,
///
/// ```text
/// cause 'edge', edge, from, to, writer, at | payload
///   (header columns)                         (inline CBOR: arguments and recorded reads)
/// ```
///
/// `L` is the machine's state label and `E` its edge names, both code enums (as [`Edge::new`]
/// requires); `P` is the document, any serde type. The edge in `edge` and the document's
/// arguments are the writer's to keep consistent.
///
/// [`Edge::new`]: crate::Edge::new
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EdgeEvent<L, E, P> {
    pub edge: E,
    /// The label the resource left.
    pub from: L,
    /// The label the resource entered: the label of the state the edge writes.
    pub to: L,
    pub writer: String,
    pub at: Timestamp,
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
