use std::{collections::BTreeMap, path::PathBuf};

use hegel::generators as gs;

use super::*;
use crate::vocabulary::{HostPathOffer, RelativePath, TreeSources};

fn home(name: &str) -> Need {
    Need {
        name: NeedName::new(name),
        digest: ContentDigest::new(format!("{name}-key")),
        item: Item::Home { key: RelativePath::new(format!("agent-homes/{name}")).expect("key") },
    }
}

fn tree(name: &str, home: &str, sources: TreeSources) -> Need {
    Need {
        name: NeedName::new(name),
        digest: ContentDigest::new(format!("{name}-v1")),
        item: Item::Tree { home: NeedName::new(home), path: RelativePath::new(name).expect("path"), sources },
    }
}

fn sources(controller: bool, url: bool, host_path: Option<&str>) -> TreeSources {
    TreeSources { controller, url, host_path: host_path.map(PathBuf::from) }
}

fn writable_base(access: Access) -> HostPathOffer {
    HostPathOffer { reach: HostReach::WritableBase, access, liveness: Liveness::Live }
}

fn directory(host: &str, guest: &str, liveness: Liveness) -> HostPathOffer {
    HostPathOffer { reach: HostReach::Directory { host: host.into(), guest: guest.into() }, access: Access::ReadOnly, liveness }
}

fn writable_directory(host: &str, guest: &str) -> HostPathOffer {
    HostPathOffer { access: Access::ReadWrite, ..directory(host, guest, Liveness::Live) }
}

const PUSH: Mechanism = Mechanism::AgentPush;
const FETCH: Mechanism = Mechanism::AgentFetch { by: FetchBy::Url };
const RW_LIVE: Mechanism = Mechanism::HostPath { access: Access::ReadWrite, liveness: Liveness::Live };
const RO_LIVE: Mechanism = Mechanism::HostPath { access: Access::ReadOnly, liveness: Liveness::Live };
const RO_SNAPSHOT: Mechanism = Mechanism::HostPath { access: Access::ReadOnly, liveness: Liveness::Snapshot };

/// (need, digest, mechanism, guest source) as it landed.
type Delivered = (&'static str, &'static str, Mechanism, Option<&'static str>);

/// (need, mechanism, action, guest source).
type ExpectedStep = (&'static str, Mechanism, Action, Option<&'static str>);

struct Row {
    rule: &'static str,
    needs: Vec<Need>,
    offers: Vec<HostPathOffer>,
    delivered: Vec<Delivered>,
    costs: CostTable,
    /// (need, mechanism, action, guest source), in plan order.
    expected: Result<Vec<ExpectedStep>, PlanError>,
}

fn row(rule: &'static str, needs: Vec<Need>, offers: Vec<HostPathOffer>) -> Row {
    Row { rule, needs, offers, delivered: Vec::new(), costs: CostTable::default(), expected: Ok(Vec::new()) }
}

impl Row {
    fn delivered(mut self, delivered: Vec<Delivered>) -> Self {
        self.delivered = delivered;
        self
    }

    fn costs(mut self, costs: CostTable) -> Self {
        self.costs = costs;
        self
    }

    fn plans(mut self, steps: Vec<ExpectedStep>) -> Self {
        self.expected = Ok(steps);
        self
    }

    fn fails(mut self, error: PlanError) -> Self {
        self.expected = Err(error);
        self
    }
}

// Behaviour (#3964 contract 2, #3956 Q8): one row per lowering rule. Each need is
// lowered to the cheapest mechanism that meets its requirements, under the cost
// policy table; landed content is kept unless its home is delivered again.
#[test]
fn needs_lower_to_the_cheapest_mechanism_meeting_their_requirements() {
    use Action::{Deliver, Keep};
    let rows = vec![
        row("a home on an agent-only provider is created by push", vec![home("home")], vec![]).plans(vec![("home", PUSH, Deliver, None)]),
        row("a home on durable host storage is reached in place", vec![home("home")], vec![writable_base(Access::ReadWrite)])
            .plans(vec![("home", RW_LIVE, Deliver, None)]),
        row("a read-only writable base cannot hold a home", vec![home("home")], vec![writable_base(Access::ReadOnly)])
            .plans(vec![("home", PUSH, Deliver, None)]),
        row("a host directory never holds a home", vec![home("home")], vec![directory("/host", "/guest", Liveness::Live)])
            .plans(vec![("home", PUSH, Deliver, None)]),
        row("a tree with a URL is fetched by the agent", vec![home("home"), tree("skills", "home", sources(false, true, None))], vec![])
            .plans(vec![("home", PUSH, Deliver, None), ("skills", FETCH, Deliver, None)]),
        row("a tree the controller holds is pushed", vec![home("home"), tree("account", "home", sources(true, false, None))], vec![])
            .plans(vec![("home", PUSH, Deliver, None), ("account", PUSH, Deliver, None)]),
        row("fetch is cheaper than push by default", vec![home("home"), tree("skills", "home", sources(true, true, None))], vec![])
            .plans(vec![("home", PUSH, Deliver, None), ("skills", FETCH, Deliver, None)]),
        row("the cost table, not code, orders mechanisms", vec![home("home"), tree("skills", "home", sources(true, true, None))], vec![])
            .costs(CostTable { agent_push: 1, ..CostTable::default() })
            .plans(vec![("home", PUSH, Deliver, None), ("skills", PUSH, Deliver, None)]),
        row(
            "a host copy inside an offered directory is reached where it lies",
            vec![home("home"), tree("skills", "home", sources(true, true, Some("/host/skills/a")))],
            vec![directory("/host/skills", "/run/skills", Liveness::Live)],
        )
        .plans(vec![("home", PUSH, Deliver, None), ("skills", RO_LIVE, Deliver, Some("/run/skills/a"))]),
        row(
            "a live host path beats a snapshot of the same tree",
            vec![home("home"), tree("skills", "home", sources(false, false, Some("/host/skills/a")))],
            vec![directory("/host", "/snap", Liveness::Snapshot), directory("/host/skills", "/live", Liveness::Live)],
        )
        .plans(vec![("home", PUSH, Deliver, None), ("skills", RO_LIVE, Deliver, Some("/live/a"))]),
        row(
            "a snapshot host path still beats fetch",
            vec![home("home"), tree("skills", "home", sources(false, true, Some("/host/skills/a")))],
            vec![directory("/host", "/snap", Liveness::Snapshot)],
        )
        .plans(vec![("home", PUSH, Deliver, None), ("skills", RO_SNAPSHOT, Deliver, Some("/snap/skills/a"))]),
        row(
            "a writable directory never serves a read-only tree",
            vec![home("home"), tree("skills", "home", sources(false, true, Some("/host/skills/a")))],
            vec![writable_directory("/host/skills", "/run/skills")],
        )
        .plans(vec![("home", PUSH, Deliver, None), ("skills", FETCH, Deliver, None)]),
        row(
            "a tree whose guest path moved is delivered again",
            vec![home("home"), tree("skills", "home", sources(false, false, Some("/host/skills/a")))],
            vec![directory("/host/skills", "/run/new", Liveness::Live)],
        )
        .delivered(vec![("home", "home-key", PUSH, None), ("skills", "skills-v1", RO_LIVE, Some("/run/old/a"))])
        .plans(vec![("home", PUSH, Keep, None), ("skills", RO_LIVE, Deliver, Some("/run/new/a"))]),
        row(
            "a host copy outside every offered directory falls back to fetch",
            vec![home("home"), tree("skills", "home", sources(false, true, Some("/elsewhere/a")))],
            vec![directory("/host", "/guest", Liveness::Live)],
        )
        .plans(vec![("home", PUSH, Deliver, None), ("skills", FETCH, Deliver, None)]),
        row(
            "a tree no mechanism can supply has no plan",
            vec![home("home"), tree("skills", "home", sources(false, false, Some("/elsewhere/a")))],
            vec![directory("/host", "/guest", Liveness::Live)],
        )
        .fails(PlanError::NoMechanism(NeedName::new("skills"))),
        row(
            "landed content under the same mechanism is kept",
            vec![home("home"), tree("skills", "home", sources(false, true, None))],
            vec![],
        )
        .delivered(vec![("home", "home-key", PUSH, None), ("skills", "skills-v1", FETCH, None)])
        .plans(vec![("home", PUSH, Keep, None), ("skills", FETCH, Keep, None)]),
        row("new content is delivered again", vec![home("home"), tree("skills", "home", sources(false, true, None))], vec![])
            .delivered(vec![("home", "home-key", PUSH, None), ("skills", "skills-v0", FETCH, None)])
            .plans(vec![("home", PUSH, Keep, None), ("skills", FETCH, Deliver, None)]),
        row(
            "content landed by another mechanism is delivered again",
            vec![home("home"), tree("skills", "home", sources(false, true, None))],
            vec![writable_base(Access::ReadWrite)],
        )
        .delivered(vec![("home", "home-key", PUSH, None), ("skills", "skills-v1", FETCH, None)])
        .plans(vec![("home", RW_LIVE, Deliver, None), ("skills", FETCH, Deliver, None)]),
        row(
            "a tree whose home is delivered again is delivered again",
            vec![home("home"), tree("skills", "home", sources(false, true, None))],
            vec![],
        )
        .delivered(vec![("home", "home-v0", PUSH, None), ("skills", "skills-v1", FETCH, None)])
        .plans(vec![("home", PUSH, Deliver, None), ("skills", FETCH, Deliver, None)]),
        row("a tree must be placed in a home", vec![home("home"), tree("skills", "account", sources(false, true, None))], vec![])
            .fails(PlanError::NoHome { tree: NeedName::new("skills"), home: NeedName::new("account") }),
        row(
            "a tree is never placed in another tree",
            vec![home("home"), tree("a", "home", sources(true, false, None)), tree("b", "a", sources(true, false, None))],
            vec![],
        )
        .fails(PlanError::NoHome { tree: NeedName::new("b"), home: NeedName::new("a") }),
        row("a need is declared once", vec![home("home"), home("home")], vec![]).fails(PlanError::DuplicateNeed(NeedName::new("home"))),
        row("an access with no needs has an empty plan", vec![], vec![writable_base(Access::ReadWrite)]).plans(vec![]),
    ];
    for row in rows {
        let delivered = row
            .delivered
            .iter()
            .map(|(need, digest, mechanism, source)| {
                (
                    NeedName::new(*need),
                    Landed { digest: ContentDigest::new(*digest), mechanism: *mechanism, guest_source: source.map(PathBuf::from) },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let actual = plan(&row.needs, &ProviderMechanisms { host_paths: row.offers.clone() }, &delivered, &row.costs).map(|plan| {
            plan.steps.into_iter().map(|step| (step.need.to_string(), step.mechanism, step.action, step.guest_source)).collect::<Vec<_>>()
        });
        let expected = row.expected.clone().map(|steps| {
            steps
                .into_iter()
                .map(|(need, mechanism, action, source)| (need.to_string(), mechanism, action, source.map(PathBuf::from)))
                .collect::<Vec<_>>()
        });
        assert_eq!(actual, expected, "{}", row.rule);
    }
}

// Behaviour (#3956 Q8, #3964 contract 3): agent fetch and agent push are
// mandatory, so every need whose content the controller holds or a URL names
// has a plan on any provider; and planning again over what that plan landed
// keeps everything, so a re-run is a no-op.
//
// Generator: one home and zero to four trees, each with every combination of
// controller, URL and host copy (inside or outside the offered directory), over
// providers offering nothing, a read-only or read-write writable base, a live or
// snapshot read-only directory and a writable one, under every ordering of the cost table.
#[hegel::test]
fn planning_again_over_what_landed_keeps_everything(tc: hegel::TestCase) {
    let mut needs = vec![home("home")];
    let trees = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    for index in 0..trees {
        let controller = tc.draw(gs::booleans());
        let url = tc.draw(gs::booleans());
        let host = match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
            0 => None,
            1 => Some("/host/tree"),
            _ => Some("/elsewhere/tree"),
        };
        needs.push(tree(&format!("tree-{index}"), "home", sources(controller, url, host)));
    }
    let mut offers = Vec::new();
    if tc.draw(gs::booleans()) {
        offers.push(writable_base(if tc.draw(gs::booleans()) { Access::ReadWrite } else { Access::ReadOnly }));
    }
    if tc.draw(gs::booleans()) {
        offers.push(directory("/host", "/guest", if tc.draw(gs::booleans()) { Liveness::Live } else { Liveness::Snapshot }));
    }
    if tc.draw(gs::booleans()) {
        offers.push(writable_directory("/host", "/writable"));
    }
    let costs = CostTable {
        host_path_live: tc.draw(gs::integers::<u32>().min_value(0).max_value(3)),
        host_path_snapshot: tc.draw(gs::integers::<u32>().min_value(0).max_value(3)),
        agent_fetch_url: tc.draw(gs::integers::<u32>().min_value(0).max_value(3)),
        agent_push: tc.draw(gs::integers::<u32>().min_value(0).max_value(3)),
    };
    let mechanisms = ProviderMechanisms { host_paths: offers };
    // Mandatory mechanisms: a tree the controller holds or a URL names is always suppliable.
    let mandatory = needs.iter().all(|need| match &need.item {
        Item::Home { .. } => true,
        Item::Tree { sources, .. } => sources.controller || sources.url,
    });
    let first = match plan(&needs, &mechanisms, &BTreeMap::new(), &costs) {
        Ok(first) => first,
        Err(error) => {
            assert!(!mandatory, "agent fetch and push always yield a plan: {error}");
            return;
        }
    };
    assert!(first.steps.iter().all(|step| step.action == Action::Deliver), "nothing has landed, so everything is delivered");
    // Every chosen mechanism meets its need's requirements.
    for step in &first.steps {
        match &needs.iter().find(|need| need.name == step.need).expect("planned need").item {
            Item::Home { .. } => assert!(
                matches!(step.mechanism, Mechanism::AgentPush | Mechanism::HostPath { access: Access::ReadWrite, .. })
                    && step.guest_source.is_none(),
                "a home is writable: {step:?}"
            ),
            Item::Tree { .. } => assert!(
                !matches!(step.mechanism, Mechanism::HostPath { .. })
                    || matches!(step.mechanism, Mechanism::HostPath { access: Access::ReadOnly, .. }) && step.guest_source.is_some(),
                "a host-path tree is read-only where the guest sees it: {step:?}"
            ),
        }
    }
    let again = plan(&needs, &mechanisms, &first.landed(), &costs).expect("the same needs plan again");
    assert_eq!(again.landed(), first.landed(), "the same mechanisms are chosen");
    assert!(again.steps.iter().all(|step| step.action == Action::Keep), "{again:?}");
}
