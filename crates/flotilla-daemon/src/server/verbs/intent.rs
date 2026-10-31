//! The record of intent (#4182 ruling 6, #4197): who invoked which verb on which subjects, with
//! which parameters, and what came of it. A plain kind on the engine store, homed where the verb
//! route ran, named by the caller and the client's invocation id so that a repeat finds it.

use std::collections::BTreeMap;

use flotilla_schema::{KindRegistry, RegistryError, Stored, Timestamp};
use sha2::{Digest, Sha256};

/// How one subject's run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Stored)]
pub enum OutcomeStatus {
    /// The verb ran.
    #[store(code = "dn")]
    Done,
    /// A `when` condition failed against the subject's current row; the detail is the condition.
    #[store(code = "na")]
    NotApplicable,
    /// Policy refused the caller this verb on this subject; the detail is the reason.
    #[store(code = "rf")]
    Refused,
    /// The verb ran and failed, or the subject could not be read; the detail is the error.
    #[store(code = "fl")]
    Failed,
}

/// One subject's outcome. Under `picks: set` each subject's outcome is the one run's.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct OutcomeRecord {
    /// The subject's canonical address.
    pub subject: String,
    pub status: OutcomeStatus,
    /// The failing condition, the refusal reason or the error.
    pub detail: Option<String>,
}

/// One parameter value: verb parameters are flat scalars, and a subject parameter is its address.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum ParamValue {
    Text(String),
    Bool(bool),
    Integer(i64),
    /// An integer above `i64::MAX`.
    Unsigned(u64),
    Float(f64),
}

impl PartialEq for ParamValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Text(left), Self::Text(right)) => left == right,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Integer(left), Self::Integer(right)) => left == right,
            (Self::Unsigned(left), Self::Unsigned(right)) => left == right,
            (Self::Float(left), Self::Float(right)) => left.to_bits() == right.to_bits(),
            _ => false,
        }
    }
}

impl Eq for ParamValue {}

impl ParamValue {
    /// The value of a checked parameter; refused for anything but a string, number or boolean.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, String> {
        match value {
            serde_json::Value::String(text) => Ok(Self::Text(text.clone())),
            serde_json::Value::Bool(flag) => Ok(Self::Bool(*flag)),
            serde_json::Value::Number(number) => number
                .as_i64()
                .map(Self::Integer)
                .or_else(|| number.as_u64().map(Self::Unsigned))
                .or_else(|| number.as_f64().map(Self::Float))
                .ok_or_else(|| format!("number {number} is not representable")),
            other => Err(format!("parameter value {other} is not a string, number or boolean")),
        }
    }
}

/// One verb invocation: kind code `vi`. Created before the verb runs, with no outcomes, and
/// edited once with every outcome when it has run. A verb's answer (an outcome's `result`) is not
/// recorded: a repeat answers the original statuses without it.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "VerbInvocation", code = "vi")]
pub struct VerbInvocation {
    /// Who intended it, from the router's caller identity.
    pub caller: String,
    /// The client's invocation id, unique per caller.
    pub invocation: String,
    /// The native verb's name (`convoy.explain`).
    pub verb: String,
    /// The subjects' canonical addresses, in the order the caller gave them.
    #[store(child)]
    pub subjects: Vec<String>,
    /// The parameters as the caller sent them, read whole.
    #[store(cbor)]
    pub params: BTreeMap<String, ParamValue>,
    pub at: Timestamp,
    #[store(child)]
    pub outcomes: Vec<OutcomeRecord>,
}

/// The record name of `caller`'s invocation `invocation`: a repeat names the same record.
pub fn record_name(caller: &str, invocation: &str) -> String {
    let mut hash = Sha256::new();
    for component in [caller, invocation] {
        hash.update((component.len() as u64).to_be_bytes());
        hash.update(component.as_bytes());
    }
    format!("verb-invocation-{:x}", hash.finalize())
}

/// Registers VerbInvocation into `registry` under its code, `vi`: a plain kind.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register::<VerbInvocation>()
}

#[cfg(test)]
mod tests {
    use std::{env, fs, path::Path};

    use flotilla_schema::render_view;
    use flotilla_schema_sqlite::{schema_ddl, DdlMode};

    fn registry() -> flotilla_schema::KindRegistry {
        let mut registry = flotilla_schema::KindRegistry::new();
        super::register(&mut registry).expect("register");
        registry
    }

    fn check(file: &str, actual: &str) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/verbs/snapshots").join(file);
        if env::var_os("UPDATE_DDL_SNAPSHOT").is_some_and(|value| value == "1") {
            fs::write(&path, actual).expect("write snapshot");
            return;
        }
        let snapshot = fs::read_to_string(&path).expect("read snapshot; run with UPDATE_DDL_SNAPSHOT=1 to create it");
        assert_eq!(actual, snapshot, "{file} changed; review and rerun with UPDATE_DDL_SNAPSHOT=1");
    }

    // Behaviour (CLAUDE.md schema review): VerbInvocation renders deterministic DDL, checked in so
    // the owner's look-over and every later change show as a diff.
    #[test]
    fn verb_invocation_ddl_matches_the_snapshot() {
        check("verb_invocation.sql", &schema_ddl(&registry(), DdlMode::Create));
    }

    // Behaviour (CLAUDE.md schema review): the readable view of VerbInvocation's tables.
    #[test]
    fn verb_invocation_schema_view_matches_the_snapshot() {
        let view = format!(
            "# VerbInvocation store schema view\n\nKind tables only; the shared tables are in `verb_invocation.sql`.\n\n{}",
            render_view(registry().iter())
        );
        check("verb_invocation_schema_view.md", &view);
    }

    // Behaviour (#4182 ruling 7): invocation ids are scoped per caller, and the record name
    // separates its components, so neither a shared id nor a shifted boundary collides.
    #[test]
    fn record_names_are_scoped_per_caller() {
        assert_eq!(super::record_name("socket:uid=1", "a"), super::record_name("socket:uid=1", "a"));
        assert_ne!(super::record_name("socket:uid=1", "a"), super::record_name("bearer", "a"));
        assert_ne!(super::record_name("ab", "c"), super::record_name("a", "bc"));
    }
}
