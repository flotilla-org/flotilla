//! Image builds as the engine holds them (#3108, ADR 0062): the port every image build reader,
//! demander and retrier uses, served over the decided store.
//!
//! A build is demanded at each home that admits work needing it (an ImageBuildDemand, written
//! only there) and launched at its builder (each launch `<build>#<n>`, written only there). One
//! launch serves the demands of every home for the same build. A launch that gave up is launched
//! again only on the operator's retry, at the builder; one released once nothing demanded it is
//! launched again by the next demand.

use flotilla_protocol::LifecycleReading;

use crate::ImageBuildSpec;

/// One launch of a build as a reader sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageBuildRecord {
    /// The build, [`crate::image_build_name`] of its spec.
    pub name: String,
    /// This launch, `<name>#<n>`.
    pub launch: String,
    /// The builder's home, which holds the launch.
    pub home: String,
    /// What it builds; `host_ref` is the builder host.
    pub spec: ImageBuildSpec,
    pub reading: LifecycleReading,
    /// The tries of this launch that failed.
    pub failures: u32,
}

impl ImageBuildRecord {
    /// The built image's digest, once it is built.
    pub fn digest(&self) -> Option<&str> {
        match &self.reading {
            LifecycleReading::Ready { identity } => Some(identity),
            _ => None,
        }
    }

    /// Whether this launch has ended: given up, or released once nothing demanded it.
    pub fn ended(&self) -> bool {
        !matches!(self.reading, LifecycleReading::InProgress | LifecycleReading::Retrying { .. } | LifecycleReading::Ready { .. })
    }
}

/// What changed among the builds' launches, at any home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageBuildChange {
    /// A launch of the build of this name changed.
    Changed(String),
    /// Any of them may have changed: a home's log was read afresh.
    Rescanned,
}

/// The image build port. Every read is keyed or indexed.
pub trait ImageBuildRecords: Send + Sync {
    /// The builds' launch changes from now on, as their records are written or replicated here.
    /// Where none are followed, the channel is closed.
    fn changes(&self) -> tokio::sync::broadcast::Receiver<ImageBuildChange> {
        tokio::sync::broadcast::channel(1).1
    }
    /// The newest launch of build `name`, at whichever home builds it; `None` before its builder
    /// has launched it.
    fn get(&self, name: &str) -> Result<Option<ImageBuildRecord>, String>;
    /// What this home demands under `name`, if it does.
    fn demanded(&self, name: &str) -> Result<Option<ImageBuildSpec>, String>;
    /// Demands `spec` under `name` at this home: `true` when written, `false` when this home
    /// already demands it. A different demand under the name is refused.
    fn demand(&self, name: &str, spec: &ImageBuildSpec) -> Result<bool, String>;
    /// This home's demand under `name` is withdrawn; withdrawing none is done.
    fn withdraw(&self, name: &str) -> Result<(), String>;
    /// The builder's home for build `name`, as a demand at any home names it.
    fn builder_home(&self, name: &str) -> Result<Option<String>, String>;
    /// At the builder: launches build `name` again once its newest launch gave up, returning
    /// the new launch. Refused while that launch has not ended, or here when this home is not
    /// its builder.
    fn retry(&self, name: &str) -> Result<String, String>;
}
