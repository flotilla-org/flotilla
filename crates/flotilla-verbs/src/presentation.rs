//! How a verb is shown, and how it takes the rows it is offered (#4182 rulings 4 and 10).

/// How a verb takes the rows it is offered (#4182 ruling 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Picks {
    /// The cursor row only.
    #[default]
    One,
    /// Any number of rows; the verb runs once per subject.
    Each,
    /// Any number of rows; the verb runs once with them all.
    Set,
}

impl Picks {
    pub const ALL: [Picks; 3] = [Picks::One, Picks::Each, Picks::Set];

    pub fn as_str(self) -> &'static str {
        match self {
            Picks::One => "one",
            Picks::Each => "each",
            Picks::Set => "set",
        }
    }
}

/// Presentation and applicability for one native verb (#4182 ruling 10), declared with the verb
/// by the crate performing its effect. Entries are code until #4198 bakes them from
/// `verbs/*.yaml` beside each declaring crate. A verb registered with the default presentation
/// is listed with a label generated from its name and no `when`.
#[derive(Debug, Clone, Default)]
pub struct Presentation {
    pub label: Option<&'static str>,
    pub help: Option<&'static str>,
    pub key: Option<&'static str>,
    pub default: bool,
    pub picks: Picks,
    /// Conditions over the subject's row that must all hold: a hint for showing the action,
    /// re-checked by the route to fail a stale menu early, never an authorisation (#4267 ruling 4).
    pub when: Vec<&'static str>,
    pub confirm: Option<&'static str>,
}
