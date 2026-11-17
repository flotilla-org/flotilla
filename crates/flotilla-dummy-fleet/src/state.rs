//! The fleet's state file. What a dummy fleet stands in for outlives the daemon process: a
//! vendor's backings, a cleat session's terminal and the crew working in it, a host's checkouts
//! and a forge's change requests. The dummy fleet holds them in the daemon's process, so it
//! appends each change to them to one file under the daemon's state directory, and the fleet
//! the next process opens restores them from it. Opening rewrites the file as the fewest changes
//! that restore what it held.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use flotilla_environment::environment::dummy::DummyVendorChange;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::commands::CommandValue;
use serde::{Deserialize, Serialize};

use crate::fleet::Counter;

/// The state file's name in the daemon's state directory.
pub const STATE_FILE: &str = "dummy-fleet.jsonl";

/// One change to the fleet's world, a line of the state file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Event {
    /// A change to a vendor's backings, by the vendor's instance name.
    Vendor {
        instance: String,
        change: DummyVendorChange,
    },
    /// The dummy forge opened a change request; `opened_at` is wall-clock milliseconds.
    ChangeRequest {
        repository: String,
        number: u64,
        branch: String,
        title: String,
        opened_at: i64,
        head_sha: String,
    },
    Checkout {
        environment: String,
        path: PathBuf,
        branch: String,
    },
    CheckoutRemoved {
        environment: String,
        path: PathBuf,
    },
    CheckoutsDestroyed {
        environment: String,
    },
    /// A terminal session started; an agent session hosts a crew.
    Launched {
        session: String,
        environment: String,
        command: String,
        cwd: ExecutionEnvironmentPath,
        crew: Option<LaunchedCrew>,
    },
    Killed {
        session: String,
    },
    SessionsDestroyed {
        environment: String,
    },
    /// A turn delivered to a session, in delivery order, under the key it was sent with and when
    /// it was accepted (wall-clock milliseconds), as cleat records a keyed send.
    Turn {
        session: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(default)]
        at_unix_ms: u64,
    },
    /// The outcome of a crew's command, in the order the crew sent them.
    Acted {
        session: String,
        outcome: Outcome,
    },
    Counted {
        counter: Counter,
    },
    /// Counts carried over, added to those the other events make.
    Counts {
        counts: BTreeMap<Counter, u64>,
    },
}

/// Who a session's crew is and the script it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LaunchedCrew {
    pub(crate) crew_id: String,
    pub(crate) terminal_session: Option<String>,
    pub(crate) namespace: String,
    pub(crate) convoy: String,
    pub(crate) vessel: String,
    pub(crate) role: String,
    pub(crate) script: String,
}

/// What a crew's command came to, as the crew saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// The daemon answered it with `value`, which may itself be an error.
    Done { value: Box<CommandValue> },
    /// The daemon, or the forge, refused it.
    Refused { error: String },
    /// The shared forge opened (or found) the change request at `url`.
    Opened { url: String },
}

/// Appends events to the state file once the fleet has been restored; until then, and for a
/// fleet without one, records nothing.
#[derive(Default)]
pub(crate) struct Journal(Mutex<Option<File>>);

impl Journal {
    pub(crate) fn record(&self, event: &Event) {
        let mut file = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(file) = file.as_mut() else { return };
        let mut line = serde_json::to_string(event).expect("a state event serializes");
        line.push('\n');
        if let Err(error) = file.write_all(line.as_bytes()) {
            tracing::warn!(%error, "dummy fleet could not record a change to its state file");
        }
    }

    /// Record nothing more: the process this fleet stands for has ended.
    pub(crate) fn close(&self) {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    }

    /// Replace the state file with `events` and append to it from now on.
    pub(crate) fn open(&self, path: &Path, events: &[Event]) -> Result<(), String> {
        let rewritten = path.with_extension("jsonl.new");
        let mut file = File::create(&rewritten).map_err(|error| format!("create {}: {error}", rewritten.display()))?;
        for event in events {
            let mut line = serde_json::to_string(event).expect("a state event serializes");
            line.push('\n');
            file.write_all(line.as_bytes()).map_err(|error| format!("write {}: {error}", rewritten.display()))?;
        }
        file.sync_all().map_err(|error| format!("sync {}: {error}", rewritten.display()))?;
        std::fs::rename(&rewritten, path).map_err(|error| format!("replace {}: {error}", path.display()))?;
        let file = std::fs::OpenOptions::new().append(true).open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(file);
        Ok(())
    }
}

/// The events in the state file at `path`; none when there is none. A process that ended while
/// appending leaves a partial last line, which is skipped.
pub(crate) fn load(path: &Path) -> Result<Vec<Event>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let lines = BufReader::new(file).lines().collect::<Result<Vec<_>, _>>().map_err(|error| format!("read {}: {error}", path.display()))?;
    let last = lines.len().saturating_sub(1);
    let mut events = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        match serde_json::from_str(line) {
            Ok(event) => events.push(event),
            Err(error) if index == last => tracing::warn!(%error, "dummy fleet state file ends in a partial change; it is skipped"),
            Err(error) => return Err(format!("{} line {}: {error}", path.display(), index + 1)),
        }
    }
    Ok(events)
}
