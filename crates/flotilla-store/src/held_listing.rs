//! What the engine kinds' incrementally held listings share (#3108): ticks for watchers, the
//! changes a pass drains, and the end of a watch.

use std::sync::{Arc, Mutex, PoisonError};

use futures::{stream::BoxStream, StreamExt};

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
