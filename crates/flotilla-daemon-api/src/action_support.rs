/// Shared command-event identity for actions without a repository target.
/// Lives in the daemon API so every capability can publish those events.
pub fn empty_repo_identity() -> flotilla_protocol::RepoIdentity {
    flotilla_protocol::RepoIdentity { authority: String::new(), path: String::new() }
}
