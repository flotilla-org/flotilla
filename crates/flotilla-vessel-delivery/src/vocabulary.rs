//! The lowering vocabulary (#3956 Q8): needs with content identity, and the
//! mechanisms a provider instance declares for delivering them.

use std::{
    fmt,
    path::{Component, Path, PathBuf},
};

/// The identity of one access: the relation between an occupant and the vessel it
/// is aboard (#2914). Opaque to delivery. Until the #3108 cut-over the caller
/// passes the crew role address `project/convoy/vessel/role`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccessKey(String);

impl AccessKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccessKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Names one need within an access, stable across re-plans (`home`, `skills`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NeedName(String);

impl NeedName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NeedName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Content identity: two equal digests name the same content, so content that
/// has landed under one digest never needs delivering again.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentDigest(String);

impl ContentDigest {
    pub fn new(digest: impl Into<String>) -> Self {
        Self(digest.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A non-empty path of normal components only, joined under a base without
/// ever leaving it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelativePath(PathBuf);

impl RelativePath {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if path.as_os_str().is_empty() || !path.components().all(|part| matches!(part, Component::Normal(_))) {
            return Err(format!("`{}` is not a relative path of normal components", path.display()));
        }
        Ok(Self(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn under(&self, base: &Path) -> PathBuf {
        base.join(&self.0)
    }
}

/// One item an access needs aboard its vessel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Need {
    pub name: NeedName,
    /// The content identity. For a home this identifies its key, since a home's
    /// contents belong to its occupant rather than to delivery.
    pub digest: ContentDigest,
    pub item: Item,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// A writable directory at `key` under the target's base for `base`.
    Home { key: RelativePath, base: HomeBase },
    /// A tree placed read-only at `path` inside the home need named `home`.
    Tree { home: NeedName, path: RelativePath, sources: TreeSources },
}

/// Which of the target's bases a home lives under (#4012).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HomeBase {
    /// The durable home base, which outlives a reboot: a harness home, holding the
    /// occupant's own state and transcripts.
    Durable,
    /// The writable base, which on a host may be a tmpfs: material such as a token
    /// file, which a reboot may clear and the next delivery puts back.
    Runtime,
}

/// Where a tree's content can come from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeSources {
    /// The controller holds the bytes, or can obtain them where the content's authority
    /// and reachability allow, so the agent can be pushed them.
    pub controller: bool,
    /// The agent can fetch the tree by URL, such as a repository at a pinned revision.
    pub url: bool,
    /// The tree already exists at this path on the controller's host.
    pub host_path: Option<PathBuf>,
    /// The content is addressed by its digest at sources of this visibility, such as a
    /// pinned upstream release, so a vessel that can reach them may fetch it itself.
    pub digest: Option<Visibility>,
}

/// Who may read an artifact at its sources (#3956 ruling, 2026-10-27). Declared by
/// the artifact's source, never inferred from what kind of artifact it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Visibility {
    /// Anyone can read it, without credentials.
    Public,
    /// Only holders of the named authority's credentials can read it. Those
    /// credentials never enter a vessel, so the vessel never fetches it.
    Private { authority: String },
}

/// Guest access to a host path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Access {
    ReadOnly,
    ReadWrite,
}

/// Whether the guest sees host changes as they happen or a copy taken once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Liveness {
    Live,
    Snapshot,
}

/// How an agent fetches content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FetchBy {
    Url,
    /// By content digest into the agent's verified store (drydock#86), from sources
    /// the vessel can reach.
    Digest,
}

/// The mechanism a plan chose for one need, recorded in the layer's status and
/// shown by `convoy explain`. Provider volumes are a further variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mechanism {
    /// The guest reaches the content where it lies on host storage.
    HostPath { access: Access, liveness: Liveness },
    /// The vessel agent fetches the content itself.
    AgentFetch { by: FetchBy },
    /// The controller pushes the content through the vessel agent.
    AgentPush,
}

impl fmt::Display for Mechanism {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostPath { access, liveness } => {
                let access = match access {
                    Access::ReadOnly => "ro",
                    Access::ReadWrite => "rw",
                };
                let liveness = match liveness {
                    Liveness::Live => "live",
                    Liveness::Snapshot => "snapshot",
                };
                write!(f, "host-path({access}, {liveness})")
            }
            Self::AgentFetch { by: FetchBy::Url } => f.write_str("agent-fetch(url)"),
            Self::AgentFetch { by: FetchBy::Digest } => f.write_str("agent-fetch(digest)"),
            Self::AgentPush => f.write_str("agent-push"),
        }
    }
}

impl std::str::FromStr for Mechanism {
    type Err = String;

    /// Parses the form [`Mechanism`]'s `Display` writes.
    fn from_str(text: &str) -> Result<Self, String> {
        let host_path = |access, liveness| Self::HostPath { access, liveness };
        Ok(match text {
            "host-path(ro, live)" => host_path(Access::ReadOnly, Liveness::Live),
            "host-path(ro, snapshot)" => host_path(Access::ReadOnly, Liveness::Snapshot),
            "host-path(rw, live)" => host_path(Access::ReadWrite, Liveness::Live),
            "host-path(rw, snapshot)" => host_path(Access::ReadWrite, Liveness::Snapshot),
            "agent-fetch(url)" => Self::AgentFetch { by: FetchBy::Url },
            "agent-fetch(digest)" => Self::AgentFetch { by: FetchBy::Digest },
            "agent-push" => Self::AgentPush,
            other => return Err(format!("unknown delivery mechanism `{other}`")),
        })
    }
}

/// What a host-path offer reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostReach {
    /// The target's bases are themselves host storage, as on an SSH host whose
    /// agent runs on the host's own filesystem.
    WritableBase,
    /// The controller-host directory `host` is visible in the guest at `guest`.
    Directory { host: PathBuf, guest: PathBuf },
}

/// One host-path capability a provider instance declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPathOffer {
    pub reach: HostReach,
    pub access: Access,
    pub liveness: Liveness,
}

/// What a vessel can reach, declared by its place (provider capability and the
/// environment's egress policy), never inferred from the provider's name. The
/// default reaches nothing beyond its controller, so content is pushed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reachability {
    /// The vessel reaches public sources, such as upstream release downloads.
    pub vessel_public: bool,
}

/// The optional mechanisms a provider instance declares. Agent fetch and agent
/// push are mandatory for every vessel agent and so are never declared.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderMechanisms {
    pub host_paths: Vec<HostPathOffer>,
    pub reach: Reachability,
}
