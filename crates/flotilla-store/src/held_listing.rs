//! What the engine kinds' incrementally held listings share (#3108): ticks for watchers, the
//! changes a pass drains, and the end of a watch.

use std::sync::{Arc, Mutex, PoisonError};

use flotilla_resources::engine_records::Record;
use futures::{stream::BoxStream, StreamExt};

/// The version a held view is handed out at (#3108): `held`, the version it was last handed out
/// at under its key, while it shows the `same`; otherwise `position`, how far the reads it was
/// composed after had read the log and this home's memory, so versions follow the log as a
/// kind's do. A view can be composed from a write its reads have not yet passed, so a changed
/// view never takes a version at or below the one it replaces: a watcher tells a change by its
/// version alone.
pub(crate) fn version(held: Option<u64>, same: bool, position: u64) -> Result<u64, String> {
    match held {
        Some(held) if same => Ok(held),
        Some(held) => held.checked_add(1).map(|next| next.max(position)).ok_or_else(|| format!("a view's version {held} cannot move")),
        None => Ok(position),
    }
}

/// Whether `record` shows what `held` shows: all of it but its version.
pub(crate) fn shows_the_same<R: flotilla_resources::Resource>(record: &Record<R>, held: &Record<R>) -> bool
where
    R::Spec: PartialEq,
    R::Status: PartialEq,
{
    record.name == held.name
        && record.home == held.home
        && record.created_at == held.created_at
        && record.labels == held.labels
        && record.annotations == held.annotations
        && record.spec == held.spec
        && record.status == held.status
}

/// `object`, read at `home` and handed out at `version`, as the engine port's record.
pub(crate) fn view_record<R: flotilla_resources::Resource>(
    object: flotilla_resources::ResourceObject<R>,
    home: String,
    version: u64,
) -> Record<R> {
    Record {
        name: object.metadata.name,
        home,
        version,
        created_at: object.metadata.creation_timestamp,
        labels: object.metadata.labels,
        annotations: object.metadata.annotations,
        spec: object.spec,
        status: object.status,
    }
}

/// A tick per message on `changes`, and one when it lagged: a reader reads everything again.
pub(crate) fn ticks<T: Clone + Send + 'static>(changes: tokio::sync::broadcast::Receiver<T>) -> BoxStream<'static, ()> {
    futures::stream::unfold(changes, |mut changes| async move {
        match changes.recv().await {
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Some(((), changes)),
            Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
        }
    })
    .boxed()
}

/// Hands each change `receiver` holds to `mark`; whether any asked for everything, or changes
/// were missed.
pub(crate) fn drained<T: Clone>(receiver: &mut tokio::sync::broadcast::Receiver<T>, mut mark: impl FnMut(T) -> bool) -> bool {
    let mut all = false;
    loop {
        match receiver.try_recv() {
            Ok(change) => all |= mark(change),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => all = true,
            Err(tokio::sync::broadcast::error::TryRecvError::Empty | tokio::sync::broadcast::error::TryRecvError::Closed) => return all,
        }
    }
}

/// What a held listing's changes named, while they are watched.
pub(crate) trait Watched: Send {
    /// One watch ended: the listing can no longer trust what it holds, and once none is watched it
    /// stops receiving changes.
    fn unwatch(&mut self);
}

/// Ends one watch of a listing's changes when dropped.
pub(crate) struct Unwatched<D: Watched>(pub(crate) Arc<Mutex<D>>);

impl<D: Watched> Drop for Unwatched<D> {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).unwatch();
    }
}

/// `streams` merged, holding `unwatched` until the merged stream is dropped.
pub(crate) fn watched<D: Watched + 'static>(streams: Vec<BoxStream<'static, ()>>, unwatched: Unwatched<D>) -> BoxStream<'static, ()> {
    futures::stream::select_all(streams)
        .map(move |tick| {
            let _watching = &unwatched;
            tick
        })
        .boxed()
}
