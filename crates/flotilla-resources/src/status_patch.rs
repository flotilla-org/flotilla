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
pub trait StatusPatch<S>: Send + Sync {
    fn apply(&self, status: &mut S);
}

pub enum NoStatusPatch {}

impl StatusPatch<()> for NoStatusPatch {
    fn apply(&self, _: &mut ()) {
        match *self {}
    }
}
