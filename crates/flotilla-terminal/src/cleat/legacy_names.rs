//! SHIM (#4374): delete this module one roll after the cleat that takes any session name.
//!
//! Until #4374 flotilla named each cleat session by `flotilla_paths::name_component`'s encoding of
//! its name (`work#52` was `work.2352`). A crew launched then still runs under that id after the
//! roll. Until it relaunches, flotilla finds it under the encoded spelling, but only when cleat
//! lists no session under the name itself, and logs each time it does.
//!
//! This is the only place the old spelling is read. A listing reports such a session under its
//! name ([`LegacyNames::names_listed`]); an operation on that name goes to the id cleat lists it
//! under ([`LegacyNames::cleat_id`]).

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::Mutex,
};

use flotilla_paths::name_component::decode_name_component;

#[derive(Default)]
pub(super) struct LegacyNames {
    /// Each session flotilla knows by a name cleat lists only under its old encoded id.
    cleat_ids: Mutex<HashMap<String, String>>,
}

impl LegacyNames {
    /// The name flotilla knows each id in one cleat listing by, in order.
    pub(super) fn names_listed<'a>(&self, ids: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let ids: Vec<&str> = ids.into_iter().collect();
        let listed: HashSet<&str> = ids.iter().copied().collect();
        let mut cleat_ids = self.cleat_ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        ids.into_iter()
            .map(|id| match old_encoding_of(id) {
                Some(name) if !listed.contains(name.as_str()) => {
                    if cleat_ids.insert(name.clone(), id.to_string()).is_none() {
                        tracing::warn!(session = %name, cleat_id = %id, "legacy encoded cleat session; relaunch to retire");
                    }
                    name
                }
                _ => {
                    // Cleat lists the session under its own name: that one wins from now on.
                    cleat_ids.remove(id);
                    id.to_string()
                }
            })
            .collect()
    }

    /// The id cleat holds `name` under: the name itself, unless a listing found it only under its
    /// old encoded id.
    pub(super) fn cleat_id<'a>(&self, name: &'a str) -> Cow<'a, str> {
        let cleat_ids = self.cleat_ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match cleat_ids.get(name) {
            Some(id) => {
                tracing::warn!(session = %name, cleat_id = %id, "legacy encoded cleat session; relaunch to retire");
                Cow::Owned(id.clone())
            }
            None => Cow::Borrowed(name),
        }
    }
}

/// The name `id` is the old encoding of, when it encodes anything but itself.
fn old_encoding_of(id: &str) -> Option<String> {
    decode_name_component(id).filter(|name| name != id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_encoded_id_is_listed_under_its_name_and_operated_on_by_its_id() {
        let legacy = LegacyNames::default();
        assert_eq!(legacy.names_listed(["work.2352", "plain"]), ["work#52", "plain"]);
        assert_eq!(legacy.cleat_id("work#52"), "work.2352");
        assert_eq!(legacy.cleat_id("plain"), "plain");
        assert_eq!(legacy.cleat_id("never-listed#1"), "never-listed#1");
    }

    #[test]
    fn a_session_under_the_name_itself_wins() {
        let legacy = LegacyNames::default();
        assert_eq!(legacy.names_listed(["work.2352"]), ["work#52"]);
        // The crew relaunched under its name while the old session lingers.
        assert_eq!(legacy.names_listed(["work#52", "work.2352"]), ["work#52", "work.2352"]);
        assert_eq!(legacy.cleat_id("work#52"), "work#52");
    }

    #[test]
    fn a_name_that_encodes_nothing_else_is_its_own() {
        let legacy = LegacyNames::default();
        // `.` stays `.` unless two lowercase hex digits of a non-plain byte follow.
        assert_eq!(legacy.names_listed(["v1.0", "a.41", "x.2E", "project:p/role:r"]), ["v1.0", "a.41", "x.2E", "project:p/role:r"]);
    }
}
