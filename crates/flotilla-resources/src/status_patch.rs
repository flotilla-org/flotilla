/// Applies a semantic status transition to a resource.
///
/// # Lifecycle timestamps
///
/// Status patches that mutate lifecycle timestamps must classify each legal transition and follow
/// the corresponding contract:
///
/// - A duplicate application of the same transition is a semantic no-op. An established timestamp
///   is kept even if a later reconciliation supplies a newer wall-clock value.
/// - A continuation makes a settled process active again. It preserves the original start timestamp
///   and clears the finish timestamp; settling again records a new finish timestamp.
/// - A new attempt is an explicit transition distinct from continuation. It replaces attempt-scoped
///   timestamps by clearing the old finish timestamp and recording the new attempt's start.
///
/// A resource's lifetime remains represented by its metadata creation timestamp. Attempt history,
/// if needed, belongs in events or conditions rather than additional status timestamp fields.
///
/// # Elided writes
///
/// [`StatusPatch::requires_write`] lets a patch decline to commit an application that changes
/// nothing its readers depend on. A declined application commits nothing, allocates no resource
/// version and emits no watch event; the caller receives the current object.
pub trait StatusPatch<S>: Send + Sync {
    fn apply(&self, status: &mut S);

    /// Payload-free transition name to include when optimistic retries are exhausted.
    fn diagnostic_name(&self) -> Option<&'static str> {
        None
    }

    /// Whether applying this patch to `before`, producing `after`, must be committed.
    /// The default commits every application.
    fn requires_write(&self, _before: &S, _after: &S) -> bool {
        true
    }
}

pub enum NoStatusPatch {}

impl StatusPatch<()> for NoStatusPatch {
    fn apply(&self, _: &mut ()) {
        match *self {}
    }
}
