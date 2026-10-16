//! Materialised state: a unit code enum and the time it was entered.

use serde::{Deserialize, Serialize};

use crate::{
    codec::{DecodeError, Decoder, EncodeError, Encoder},
    layout::Layout,
    time::Timestamp,
    traits::{CodeEnum, Columns},
};

/// A state column under `#[store(state)]`: column `<field>` holds the code and `<field>_since` the
/// time the state was entered. The writer sets `since`; storage keeps what it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct State<E> {
    pub value: E,
    pub since: Timestamp,
}

impl<E> State<E> {
    pub fn new(value: E, since: Timestamp) -> State<E> {
        State { value, since }
    }
}

/// Deliberately not [`crate::Column`]: a `State` field needs `#[store(state)]`, so the attribute
/// names every materialised state in the type's source.
impl<E: CodeEnum> Columns for State<E> {
    const TABLES: usize = 0;

    fn layout(layout: &mut Layout, name: &str, required: bool) {
        E::layout(layout, name, required);
        Timestamp::layout(layout, &Layout::join(name, "since"), required);
    }

    fn encode(&self, e: &mut Encoder) -> Result<(), EncodeError> {
        self.value.encode(e)?;
        self.since.encode(e).map_err(|error| error.at("since"))
    }

    fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
        E::encode_absent(e)?;
        Timestamp::encode_absent(e)
    }

    fn decode(d: &mut Decoder) -> Result<Option<Self>, DecodeError> {
        let value = E::decode(d)?;
        let since = Timestamp::decode(d).map_err(|error| error.at("since"))?;
        match (value, since) {
            (None, None) => Ok(None),
            (Some(value), Some(since)) => Ok(Some(State { value, since })),
            (None, Some(_)) => Err(DecodeError::missing("")),
            (Some(_), None) => Err(DecodeError::missing("since")),
        }
    }

    fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
        E::decode_absent(d)?;
        Timestamp::decode_absent(d).map_err(|error| error.at("since"))
    }
}
