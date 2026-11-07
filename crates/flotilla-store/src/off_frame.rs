//! Running a large step of an async function without growing its caller's stack frame.

use std::{future::Future, pin::Pin};

/// The future `make` builds, built in this function's own frame and boxed, so the caller's poll
/// frame holds only the box. Awaiting an `async fn` in place reserves room for its whole future in
/// the caller's frame (in a debug build, for every await site), however deep the call; a function
/// split into named steps run through this keeps each frame to its own locals (#3108).
#[inline(never)]
pub fn off_frame<'a, F>(make: impl FnOnce() -> F) -> Pin<Box<dyn Future<Output = F::Output> + Send + 'a>>
where
    F: Future + Send + 'a,
{
    Box::pin(make())
}
