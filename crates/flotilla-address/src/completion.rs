//! Completing a partial address from the caller's context (#4272, owner ruling 2026-11-01).
//!
//! A client completes what its caller typed before sending it; the wire address is exact, and
//! no route or store lookup completes anything. The context is itself an address (for example
//! `project:flotilla/convoy:c/role:coder`), assembled by the client from whatever it knows: the
//! current checkout's project, or a vessel's own convoy and role.

use std::collections::BTreeSet;

use crate::{Address, Kind};

/// Why a partial address did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionError {
    /// Without a context to place it, the partial address matches several addresses; they are
    /// listed for the caller rather than one guessed.
    Ambiguous { partial: Address, matches: Vec<Address> },
}

impl std::fmt::Display for CompletionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompletionError::Ambiguous { partial, matches } => {
                let matches = matches.iter().map(Address::as_str).collect::<Vec<_>>().join(", ");
                write!(formatter, "`{partial}` matches several addresses; name one: {matches}")
            }
        }
    }
}

impl std::error::Error for CompletionError {}

/// Completes `partial` from `context`, or from `candidates` when the context does not place it.
///
/// - A partial address that starts with a kind nothing contains (`project:…`, `forge:…`) is
///   already whole, and is returned as it is.
/// - Otherwise it goes under the deepest ancestor of `context` (the context included) that its
///   first segment may follow: `convoy:lander` under `project:flotilla/convoy:c/role:coder` is
///   `project:flotilla/convoy:lander`.
/// - When no context places it (none is given, or only the root fits), the candidate ending in
///   the partial's segments is taken if there is exactly one; several are an error listing them;
///   none leaves the partial as it is, for the exact lookup to answer.
pub fn complete(partial: &Address, context: Option<&Address>, candidates: &[Address]) -> Result<Address, CompletionError> {
    let first = partial.segments()[0].kind();
    if !Kind::ALL.iter().any(|parent| first.may_follow(Some(*parent))) {
        return Ok(partial.clone());
    }
    if let Some(placed) = context.and_then(|context| place(partial, context)) {
        return Ok(placed);
    }
    let matches: BTreeSet<&Address> = candidates.iter().filter(|candidate| ends_with(candidate, partial)).collect();
    match matches.into_iter().collect::<Vec<_>>().as_slice() {
        [] => Ok(partial.clone()),
        [only] => Ok((*only).clone()),
        several => {
            Err(CompletionError::Ambiguous { partial: partial.clone(), matches: several.iter().map(|&address| address.clone()).collect() })
        }
    }
}

/// `partial` under the deepest ancestor of `context` its first segment may follow, or `None`
/// when only the root fits.
fn place(partial: &Address, context: &Address) -> Option<Address> {
    let first = partial.segments()[0].kind();
    let mut ancestor = Some(context.clone());
    while let Some(parent) = ancestor {
        if first.may_follow(Some(parent.kind())) {
            // Both texts are canonical, so the joined text is the placed address's.
            return format!("{parent}/{partial}").parse().ok();
        }
        ancestor = parent.parent();
    }
    None
}

/// Whether `candidate`'s segments end with `partial`'s, and their artifact subjects agree.
fn ends_with(candidate: &Address, partial: &Address) -> bool {
    candidate.segments().ends_with(partial.segments()) && candidate.of() == partial.of()
}
