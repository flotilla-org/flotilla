//! Launch identity and restart intent, independent of the terminal runtime's observations.

use chrono::{DateTime, Utc};
use flotilla_protocol::LifecycleReading;

use crate::{ResourceObject, StatusPatch, TerminalSession, TerminalSessionPhase, TerminalSessionStatusPatch};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalLaunch {
    pub name: String,
    pub reading: LifecycleReading,
}

impl TerminalLaunch {
    /// A read view only: lifecycle phase and identity come from the engine; temporary
    /// unobservability remains runtime evidence about the same realised session.
    /// The returned clone must never be published back as stored status.
    pub fn project(&self, obj: &ResourceObject<TerminalSession>) -> ResourceObject<TerminalSession> {
        let mut obj = obj.clone();
        let status = obj.status.get_or_insert_with(Default::default);
        match &self.reading {
            LifecycleReading::InProgress | LifecycleReading::Retrying { .. } => {
                TerminalSessionStatusPatch::MarkStarting.apply(status);
            }
            LifecycleReading::Ready { identity } => {
                if status.session_id.as_deref() != Some(identity) {
                    TerminalSessionStatusPatch::MarkStarting.apply(status);
                }
                if status.phase != TerminalSessionPhase::Lost {
                    status.phase = TerminalSessionPhase::Running;
                    status.message = None;
                }
                status.session_id = Some(identity.clone());
            }
            reading => {
                status.phase = match reading {
                    LifecycleReading::Released => TerminalSessionPhase::Stopped,
                    LifecycleReading::Lost => TerminalSessionPhase::Lost,
                    _ => TerminalSessionPhase::Failed,
                };
                status.message = reading.reason();
            }
        }
        obj
    }
}

/// A local engine writer. A replacement names its predecessor; the next Requested
/// record is the durable intent, and a late request cannot allocate another successor.
pub trait TerminalLaunches: Send + Sync + std::fmt::Debug {
    fn replace(&self, obj: &ResourceObject<TerminalSession>, replaces: &str, at: DateTime<Utc>) -> Result<String, String>;
}

/// Select one source for lifecycle fields, propagating a failed checked read.
pub fn read(
    reads: &dyn crate::lifecycle_reads::LifecycleReads,
    obj: &ResourceObject<TerminalSession>,
    origin: Option<&str>,
) -> Result<(ResourceObject<TerminalSession>, Option<String>), String> {
    match reads.try_terminal_launch(&obj.metadata.name, origin)? {
        Some(launch) => Ok((launch.project(obj), Some(launch.name))),
        None => Ok((obj.clone(), None)),
    }
}

/// Preserve source provenance while selecting that home's lifecycle reading.
pub fn read_source(
    reads: &dyn crate::lifecycle_reads::LifecycleReads,
    source: &crate::ReadResourceObject<TerminalSession>,
) -> Result<(crate::ReadResourceObject<TerminalSession>, Option<String>), String> {
    let origin = match &source.provenance {
        crate::ResourceProvenance::Local => None,
        crate::ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.as_str()),
    };
    let (object, launch) = read(reads, &source.object, origin)?;
    Ok((crate::ReadResourceObject { object, provenance: source.provenance.clone() }, launch))
}
