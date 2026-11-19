//! Resuming a reading of a port's records from a [`LogPosition`] (#3108): what its kinds' log
//! entries wrote since, read on the decided store, for the port to name.

use std::collections::BTreeMap;

use flotilla_engine_store::follow::{self, Resumed};
use flotilla_resources::engine_records::{LogPosition, Since};
use flotilla_schema::{Identity, KindCode};
use flotilla_schema_sqlite::stream::Service;

/// The identities written since a position, and each home's sequence they were read through; `Err`
/// holds why they cannot be named.
pub type WrittenSince = Result<(Vec<Identity>, BTreeMap<String, u64>), String>;

/// The identities of `codes` written since `position` on every home `service` holds.
pub(crate) fn followed_since(
    service: &Service,
    codes: impl IntoIterator<Item = KindCode>,
    position: &LogPosition,
) -> Result<WrittenSince, String> {
    match follow::since_on(service, codes, &position.homes).map_err(|error| error.to_string())? {
        Resumed::Followed { changed, through } => Ok(Ok((changed, through))),
        Resumed::Expired(reason) => Ok(Err(reason)),
    }
}

/// What changed since `position` for a port whose records are its kinds' rows, each named as
/// its row: the names `codes`' log entries wrote.
pub(crate) fn logged_since(service: &Service, codes: impl IntoIterator<Item = KindCode>, position: &LogPosition) -> Result<Since, String> {
    Ok(match followed_since(service, codes, position)? {
        Ok((changed, through)) => {
            Since::Changed { names: changed.into_iter().map(|identity| identity.name).collect(), through: LogPosition::logged(through) }
        }
        Err(reason) => Since::Expired(reason),
    })
}
