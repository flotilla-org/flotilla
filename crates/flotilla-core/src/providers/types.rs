// Re-export provider data types from the protocol crate.
// These are the canonical definitions; core uses them via this re-export.
pub use flotilla_protocol::{
    AheadBehind, ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession, CommitInfo, Issue, IssueChangeset, SessionStatus,
    WorkingTreeStatus, Workspace,
};
