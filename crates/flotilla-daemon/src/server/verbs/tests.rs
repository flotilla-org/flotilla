//! The catalog's load check.
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_address::{Address, Kind};
use flotilla_protocol::table::Row;
use flotilla_schema::{Table, Verb, VerbDeclaration};
use serde_json::{Map, Value};

use super::catalog::{Catalog, NativeVerb, Picks, Presentation, Subject, Subjects};
use crate::server::tables;

#[derive(Verb)]
#[verb(name = "convoy.wipe", subject = "convoy", effect = "destructive", invocation)]
#[allow(dead_code)]
struct Wipe {
    #[param(options("soft", "hard"), default = "soft")]
    mode: Option<String>,
}

#[derive(Verb)]
#[verb(name = "convoy.peek", subject = "convoy", effect = "read")]
struct Peek {}

#[derive(Verb)]
#[verb(name = "vessel.peek", subject = "vessel", effect = "read")]
struct PeekVessel {}

#[derive(Verb)]
#[verb(name = "convoy.move", subject = "convoy", effect = "write")]
#[allow(dead_code)]
struct Move {
    to: Address,
}

struct Handler(fn() -> VerbDeclaration);

#[async_trait]
impl NativeVerb for Handler {
    fn declaration(&self) -> VerbDeclaration {
        (self.0)()
    }

    async fn run(&self, _subjects: &[Subject], _params: &Map<String, Value>, _invocation: &str) -> Result<Option<Value>, String> {
        Ok(None)
    }
}

struct Convoys;

#[async_trait]
impl Subjects for Convoys {
    fn table(&self, kind: Kind) -> Option<Table> {
        (kind == Kind::Convoy).then(|| tables::descriptor("convoys"))
    }

    async fn row(&self, _address: &Address) -> Result<Option<Row>, String> {
        Ok(None)
    }
}

fn broken(catalog: &Catalog, name: &str) -> Option<String> {
    catalog.get(name).expect("listed").broken.clone()
}

// Behaviour (#4182 rulings 2 revised, 7 and 10): the load check lists a verb that cannot run or
// whose presentation is wrong as broken, with every reason, rather than dropping it.
#[test]
fn the_load_check_lists_broken_verbs_with_their_reasons() {
    let natives: Vec<Arc<dyn NativeVerb>> = vec![
        Arc::new(Handler(Wipe::declaration)),
        Arc::new(Handler(Peek::declaration)),
        Arc::new(Handler(PeekVessel::declaration)),
        Arc::new(Handler(Move::declaration)),
    ];
    let presentations = vec![
        Presentation { verb: "convoy.wipe", when: vec![".status.nothing == x", "status.phase"], ..Presentation::default() },
        Presentation {
            verb: "convoy.peek",
            default: true,
            picks: Picks::Set,
            confirm: Some("Peek {metadata.name}?"),
            ..Presentation::default()
        },
        Presentation { verb: "convoy.move", default: true, ..Presentation::default() },
    ];
    let catalog = Catalog::new(natives, presentations, &Convoys);
    let wipe = broken(&catalog, "convoy.wipe").expect("broken");
    assert!(wipe.contains("`status.nothing` is not a column of `convoys`"), "{wipe}");
    assert!(wipe.contains("condition `status.phase`"), "{wipe}");
    assert!(wipe.contains("a destructive verb needs a confirm"), "{wipe}");
    let peek = broken(&catalog, "convoy.peek").expect("broken");
    // Defaults are claimed in name order: `convoy.move` holds the convoy default.
    assert_eq!(peek, "the verb takes one subject at a time, so it cannot pick a set; another `convoy` verb is already the default");
    assert_eq!(broken(&catalog, "convoy.move").expect("broken"), "parameter `to` needs #[param(subject = \"<address kind>\")]");
    assert_eq!(broken(&catalog, "vessel.peek").expect("broken"), "no table serves `vessel` subjects");
    let (verbs, params) = catalog.rows();
    assert_eq!(verbs.len(), 4);
    let peek = verbs.iter().find(|row| row["name"] == "convoy.peek").expect("row");
    assert_eq!(peek["reads"], serde_json::json!(["metadata.name"]));
    assert_eq!(peek["label"], "Peek");
    let mode = params.iter().find(|row| row["name"] == "mode").expect("param");
    assert_eq!((&mode["type"], &mode["default"], &mode["options"]), (&"enum".into(), &"soft".into(), &serde_json::json!(["soft", "hard"])));
}

// Behaviour (#4197 done-when): the built-in verbs pass the load check, and every built-in
// presentation names a native verb.
#[test]
fn the_built_in_verbs_are_not_broken() {
    let natives = super::natives(None);
    let names: Vec<_> = natives.iter().map(|native| native.declaration().name).collect();
    for presentation in super::presentations() {
        assert!(names.contains(&presentation.verb), "{} names no native verb", presentation.verb);
    }
    let catalog = Catalog::new(natives, super::presentations(), &Convoys);
    for entry in catalog.entries() {
        assert_eq!(entry.broken, None, "{}", entry.declaration.name);
    }
    assert!(catalog.get("convoy.explain").is_some());
}
