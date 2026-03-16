pub mod store;
pub mod types;

pub use store::{AttachableRegistry, AttachableStore, SharedAttachableStore};
pub use types::{
    Attachable, AttachableId, AttachableKind, AttachableSet, AttachableSetId, BindingObjectKind, ProviderBinding, TerminalAttachable,
    TerminalPurpose,
};
