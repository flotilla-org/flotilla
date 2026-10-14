// Not buildable in this repository: it is an `examples/` program for the PR #3216 crate
// (`state_machine_prototype`, prototypes/3108-3209). Copy it there to regenerate pr3216-edges.jsonl.
//! Prints every machine's edge table as JSON lines: {"machine", "initial", "ends", "edges": [[from, edge, to], ...]}.
use state_machine_prototype::kinds::lifecycle::{CloneMachine, ImageBuildMachine};
use state_machine_prototype::kinds::message::Message;
use state_machine_prototype::machine::{Machine, Named};

fn dump<M: Machine>(label: &str) {
    let def = M::definition();
    let mut edges = Vec::new();
    for e in &def.edges {
        for from in &e.from {
            for to in e.reachable_targets() {
                edges.push(format!("[\"{}\",\"{}\",\"{}\"]", from.name(), e.name, to.name()));
            }
        }
    }
    let ends: Vec<String> = def.ends.iter().map(|e| format!("\"{}\"", e.state.name())).collect();
    println!("{{\"machine\":\"{}\",\"initial\":\"{}\",\"ends\":[{}],\"edges\":[{}]}}", label, M::initial().name(), ends.join(","), edges.join(","));
}

fn main() {
    dump::<Message>("message");
    dump::<CloneMachine>("lifecycle(Clone)");
    dump::<ImageBuildMachine>("lifecycle(ImageBuild)");
}
