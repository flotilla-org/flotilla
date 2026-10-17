//! Serde names are the view; storage never reads them.

use flotilla_schema::Kind;

mod plain {
    use flotilla_schema::{State, Stored};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Stored)]
    pub enum Phase {
        #[store(code = "c1")]
        Open,
        #[store(code = "c2")]
        Closed,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Stored)]
    pub enum Event {
        #[store(code = "c1")]
        Opened { by: String },
        #[store(code = "c2")]
        Closed,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Stored)]
    #[store(kind = "Ticket", code = "k1")]
    pub struct Ticket {
        pub title: String,
        #[store(state)]
        pub phase: State<Phase>,
        pub event: Option<Event>,
    }
}

mod renamed {
    use flotilla_schema::{State, Stored};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Stored)]
    #[serde(rename_all = "SCREAMING_SNAKE_CASE")]
    pub enum Phase {
        #[serde(rename = "opened-at-last")]
        #[store(code = "c1")]
        Open,
        #[serde(alias = "shut")]
        #[store(code = "c2")]
        Closed,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Stored)]
    #[serde(tag = "type", rename_all = "kebab-case")]
    pub enum Event {
        #[store(code = "c1")]
        Opened {
            #[serde(rename = "author")]
            by: String,
        },
        #[serde(rename = "done")]
        #[store(code = "c2")]
        Closed,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Stored)]
    #[serde(rename = "TicketV2", rename_all = "camelCase")]
    #[store(kind = "Ticket", code = "k1")]
    pub struct Ticket {
        #[serde(rename = "headline")]
        pub title: String,
        #[store(state)]
        pub phase: State<Phase>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub event: Option<Event>,
    }
}

// Behaviour (#3320 representation): serde renames never change stored names or codes. Two copies
// of a kind differing only in serde attributes produce identical descriptors.
#[test]
fn serde_renames_leave_the_table_set_unchanged() {
    assert_eq!(plain::Ticket::table_set(), renamed::Ticket::table_set());
}
