//! Publication and raw-stream contracts for ordinary local services.
//!
//! A transport adapter authenticates the caller and the pinned host before
//! constructing a [`Session`]. Each connection gets a separate ordered byte
//! stream. Control and diagnostics never enter that stream.

use std::{collections::BTreeSet, fmt, time::Duration};

use async_trait::async_trait;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};

pub mod memory;

/// Fingerprint of a persisted instance key, independent of its current route.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Fingerprint(pub String);

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Namespace(pub String);

/// Host-assigned identity; names and routes are never used as identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PublicationId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Availability {
    Available,
    Unavailable,
    Withdrawn,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Publication {
    pub id: PublicationId,
    pub namespace: Namespace,
    pub name: String,
    pub publisher: Fingerprint,
    pub audience: BTreeSet<Fingerprint>,
    pub generation: u64,
    pub availability: Availability,
}

/// The host enforces this grant, including its audience ceiling and expiry.
#[derive(Clone, Debug)]
pub struct Grant {
    pub grantee: Fingerprint,
    pub namespace: Namespace,
    pub audience_ceiling: BTreeSet<Fingerprint>,
    /// Adapter-defined monotonic tick; the grant is invalid at this tick.
    pub expires_at: u64,
}

/// A session represents a completed, transport-independent identity handshake.
/// The pinned host is checked on every operation. A relay is a separately
/// pinned instance trusted with plaintext, never the asserted caller identity.
#[derive(Clone, Debug)]
pub struct Session {
    pub caller: Fingerprint,
    pub pinned_host: Fingerprint,
    pub via: Option<Fingerprint>,
}

#[derive(Clone, Debug)]
pub struct PublishRequest {
    pub namespace: Namespace,
    pub name: String,
    pub audience: BTreeSet<Fingerprint>,
    /// Reclaiming an identity is explicit; a duplicate display name creates
    /// another identity, while a live incumbent always wins.
    pub reclaim: Option<PublicationId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lease {
    pub id: PublicationId,
    pub generation: u64,
    pub publisher: Fingerprint,
}

pub trait RawStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> RawStream for T {}
pub type ByteStream = Box<dyn RawStream>;

pub struct Published {
    pub lease: Lease,
    /// One accepted raw channel per successful connect.
    pub incoming: mpsc::UnboundedReceiver<ByteStream>,
}

/// Consumer-owned address. It is bound to both a host fingerprint and a
/// publication ID, so reconnecting never substitutes a same-named service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Exposure {
    pub host: Fingerprint,
    pub publication: PublicationId,
    pub local_name: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Error {
    IdentityMismatch,
    UntrustedIntermediary,
    Denied,
    GrantExpired,
    UnknownPublication,
    Unavailable,
    Withdrawn,
    LivePublisher,
    StaleGeneration,
    Deadline,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// Changes include full filtered snapshots, including Unavailable and
/// Withdrawn records. Transport implementations may coalesce intermediate
/// snapshots but must preserve the latest generation and state.
pub type Watch = mpsc::UnboundedReceiver<Vec<Publication>>;

#[async_trait]
pub trait Tender: Send + Sync {
    async fn publish(&self, session: &Session, request: PublishRequest) -> Result<Published, Error>;
    async fn browse(&self, session: &Session) -> Result<Vec<Publication>, Error>;
    async fn watch(&self, session: &Session) -> Result<Watch, Error>;
    async fn connect(&self, session: &Session, id: PublicationId) -> Result<ByteStream, Error>;
    /// Abort a pending open at a caller-chosen bound. An established stream
    /// has no idle timeout; its owner may cancel by dropping it.
    async fn connect_with_deadline(&self, session: &Session, id: PublicationId, deadline: Duration) -> Result<ByteStream, Error> {
        tokio::time::timeout(deadline, self.connect(session, id)).await.map_err(|_| Error::Deadline)?
    }
    async fn expose_local(&self, session: &Session, id: PublicationId) -> Result<Exposure, Error>;
    async fn open_exposure(&self, session: &Session, exposure: &Exposure) -> Result<ByteStream, Error>;
    async fn open_exposure_with_deadline(&self, session: &Session, exposure: &Exposure, deadline: Duration) -> Result<ByteStream, Error> {
        tokio::time::timeout(deadline, self.open_exposure(session, exposure)).await.map_err(|_| Error::Deadline)?
    }
    async fn disconnect(&self, session: &Session, lease: &Lease) -> Result<(), Error>;
    async fn withdraw(&self, session: &Session, lease: &Lease) -> Result<(), Error>;
}
