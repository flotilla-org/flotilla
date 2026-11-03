use std::{fmt, str::FromStr};

use crate::Error;

/// The closed set of segment kinds (ADR 0066 §4).
///
/// Adding a kind changes the parser and the canonical printer, so it is a
/// stored-shape change made deliberately, never an extension point. `of` is
/// not a kind: it is the artifact address's subject segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A flotilla Project: `project:flotilla`.
    Project,
    /// A fleet, independent of its charter Project.
    Fleet,
    /// A host place that advertises provider instances.
    Host,
    /// An instance under a host or provider account.
    Provider,
    /// A convoy, at the root or under its project.
    Convoy,
    /// A convoy's vessel.
    Vessel,
    /// A standing role under a fleet or project, or a convoy role.
    Role,
    /// An artifact slot under its producing role; always followed by `of:`.
    Artifact,
    /// A message, under the inbox (role) that receives it.
    Message,
    /// A forge, named by its `forge_id`.
    Forge,
    /// A repository on a forge: one value holding `owner/repo`.
    Repo,
    /// A change request (pull request) number.
    ChangeRequest,
    /// An issue number.
    Issue,
    /// A comment on a repository's issues or change requests.
    Comment,
    /// A commit, by its full SHA.
    Commit,
    /// A Git ref, by its full name (`refs/heads/main`).
    Ref,
    /// A usage provider; the root of an account's usage.
    Usage,
    /// A provider account place, or an account under a usage provider.
    Account,
}

impl Kind {
    pub const ALL: [Kind; 18] = [
        Kind::Host,
        Kind::Provider,
        Kind::Project,
        Kind::Fleet,
        Kind::Convoy,
        Kind::Vessel,
        Kind::Role,
        Kind::Artifact,
        Kind::Message,
        Kind::Forge,
        Kind::Repo,
        Kind::ChangeRequest,
        Kind::Issue,
        Kind::Comment,
        Kind::Commit,
        Kind::Ref,
        Kind::Usage,
        Kind::Account,
    ];

    /// The segment token, as printed before `:`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Kind::Host => "host",
            Kind::Provider => "provider",
            Kind::Project => "project",
            Kind::Fleet => "fleet",
            Kind::Convoy => "convoy",
            Kind::Vessel => "vessel",
            Kind::Role => "role",
            Kind::Artifact => "artifact",
            Kind::Message => "message",
            Kind::Forge => "forge",
            Kind::Repo => "repo",
            Kind::ChangeRequest => "cr",
            Kind::Issue => "issue",
            Kind::Comment => "comment",
            Kind::Commit => "commit",
            Kind::Ref => "ref",
            Kind::Usage => "usage",
            Kind::Account => "account",
        }
    }

    /// Whether a segment of this kind may directly follow `parent`, or start
    /// an address when `parent` is `None`. This is the whole shape grammar.
    pub fn may_follow(self, parent: Option<Kind>) -> bool {
        match self {
            Kind::Fleet | Kind::Project | Kind::Forge | Kind::Usage | Kind::Host => parent.is_none(),
            // A convoy without a Project is valid today (`ConvoySpec.project_ref`
            // is optional); a convoy with one sits under it.
            Kind::Convoy => matches!(parent, None | Some(Kind::Project)),
            Kind::Vessel => parent == Some(Kind::Convoy),
            Kind::Role => matches!(parent, Some(Kind::Fleet | Kind::Project | Kind::Convoy)),
            Kind::Artifact => parent == Some(Kind::Role),
            // A message sits under its inbox, which is always a role.
            Kind::Message => parent == Some(Kind::Role),
            Kind::Repo => parent == Some(Kind::Forge),
            Kind::ChangeRequest | Kind::Issue | Kind::Comment | Kind::Commit | Kind::Ref => parent == Some(Kind::Repo),
            Kind::Account => matches!(parent, None | Some(Kind::Usage)),
            Kind::Provider => matches!(parent, Some(Kind::Host | Kind::Account)),
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Kind {
    type Err = Error;

    fn from_str(token: &str) -> Result<Self, Self::Err> {
        Kind::ALL.into_iter().find(|kind| kind.as_str() == token).ok_or_else(|| Error::UnknownKind(token.to_string()))
    }
}
