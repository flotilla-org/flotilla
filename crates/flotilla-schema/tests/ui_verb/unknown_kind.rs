use flotilla_schema::Verb;

#[derive(Verb)]
#[verb(name = "galaxy.spin", subject = "galaxy", effect = "write")]
struct Spin {}

fn main() {
    let _ = Spin::declaration();
}
