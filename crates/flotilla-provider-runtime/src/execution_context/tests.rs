use hegel::generators as gs;

use super::*;
use crate::host_environment::HostSupply;

const CLAUDE: AccountState = AccountState::new("claude-code")
    .homes(&["CLAUDE_CONFIG_DIR"])
    .secrets(&["CLAUDE_CODE_OAUTH_TOKEN"])
    .observation_defaults(&[("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")]);
const GH: AccountState = AccountState::new("gh").homes(&["GH_CONFIG_DIR"]).secrets(&["GH_TOKEN"]);

fn accounts() -> AccountVariables {
    AccountVariables::from_families(&[CLAUDE, GH])
}

fn rust() -> HostEnvironment {
    HostEnvironment::new(vec![HostSupply {
        name: "rust".into(),
        path: vec!["~/.cargo/bin".into()],
        env: vec![("CARGO_HOME".into(), "~/.cargo".into()), ("RUSTUP_HOME".into(), "~/.rustup".into())],
    }])
    .expect("valid supplies")
}

fn preparer(host: HostEnvironment, home: Option<&str>) -> ExecutionPreparer {
    ExecutionPreparer::new(host, accounts(), home).with_generation(vec!["/state/vessel/bin".into()])
}

fn bound() -> BoundLayer {
    BoundLayer::new(
        vec!["/state/credentials/github".into()],
        [
            ("GITHUB_TOKEN_FILE".to_string(), "/state/credentials/github/token".to_string()),
            ("CLAUDE_CONFIG_DIR".to_string(), "/state/agent-homes/claude".to_string()),
        ],
    )
    .expect("bound layer")
}

// A probe and a crew on one environment get byte-identical base layers, with
// every supply path and value absolute against the target's discovered home:
// no `~` survives preparation. The generator draws homes with and without a
// trailing slash and supplies whose entries are `~`, `~/...` or absolute.
#[hegel::test]
fn probe_and_crew_share_one_absolute_base_layer(tc: hegel::TestCase) {
    let home = ["/home/u", "/home/u/", "/Users/x y"][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
    let entries = ["~", "~/.cargo/bin", "/opt/tool/bin", "~/it's $(here)"];
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let supplies = (0..count)
        .map(|index| {
            let entry = entries[tc.draw(gs::integers::<usize>().min_value(0).max_value(3))];
            let value = entries[tc.draw(gs::integers::<usize>().min_value(0).max_value(3))];
            HostSupply { name: format!("s{index}"), path: vec![entry.into()], env: vec![(format!("TOOL_{index}"), value.into())] }
        })
        .collect();
    let preparer = preparer(HostEnvironment::new(supplies).expect("valid supplies"), Some(home));

    let probe = preparer.observation().expect("observation");
    let crew = preparer.prepare(Some(bound())).expect("crew");
    assert_eq!(probe.base(), crew.base());
    assert_eq!(probe.base().environment(), crew.base().environment());
    for (name, value) in crew.base().environment() {
        assert!(!value.contains('~'), "{name}={value} keeps a `~`");
    }
    assert!(crew.base().path().split(':').all(|entry| entry.starts_with('/')));

    // The crew renders the same base: its variables verbatim, and its PATH is
    // the base PATH with the bound wrappers inserted after the generation.
    let mut launch = vec![("CLAUDE_CONFIG_DIR".to_string(), "/state/agent-homes/claude".to_string())];
    crew.render_crew(&mut launch, &accounts()).expect("render");
    for (name, value) in crew.base().variables() {
        assert_eq!(launch.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str()), Some(value));
    }
    let path = launch.iter().find(|(name, _)| name == "PATH").expect("PATH").1.clone();
    let without_bound = path.split(':').filter(|entry| *entry != "/state/credentials/github").collect::<Vec<_>>().join(":");
    assert_eq!(without_bound, crew.base().path());
}

// An unknown or relative home makes the environment not ready, with a named
// reason; nothing guesses a home.
#[test]
fn an_unknown_home_is_not_ready() {
    assert_eq!(
        preparer(rust(), None).base().expect_err("unknown home").for_environment("b1"),
        "b1 is not ready: the target user's home is unknown"
    );
    assert!(matches!(
        preparer(rust(), Some("home/u")).observation().expect_err("relative home"),
        PreparationError::NotReady(reason) if reason.contains("is not absolute")
    ));
}

// Supplies describe installations only: one that sets a family's account
// variable is a host configuration error.
#[test]
fn a_supply_setting_a_family_account_variable_is_refused() {
    let host = HostEnvironment::new(vec![HostSupply {
        name: "claude".into(),
        path: Vec::new(),
        env: vec![("CLAUDE_CONFIG_DIR".into(), "~/.claude".into())],
    }])
    .expect("well-formed supply");
    let expected = "supply `claude` sets CLAUDE_CONFIG_DIR, which carries `claude-code` account state";
    assert_eq!(host.check_account_state(&accounts()).expect_err("refused"), expected);
    assert_eq!(preparer(host, Some("/home/u")).base().expect_err("refused"), PreparationError::HostConfiguration(expected.into()));
}

// A crew's account variables come only from its bound layer, and the launch
// may not set what the base layer sets. A refused rendering changes nothing.
#[test]
fn crew_rendering_takes_account_state_only_from_the_bound_layer() {
    let crew = preparer(rust(), Some("/home/u")).prepare(Some(bound())).expect("crew");
    for (launch, error) in [
        (("GH_TOKEN", "ambient"), "launch on b1 refused: the launch sets GH_TOKEN, `gh` account state that no bound plan supplies"),
        (
            ("CLAUDE_CONFIG_DIR", "/home/u/.claude"),
            "launch on b1 refused: the launch sets CLAUDE_CONFIG_DIR, `claude-code` account state that no bound plan supplies",
        ),
        (("CARGO_HOME", "/elsewhere"), "host configuration error for b1: supply `rust` sets CARGO_HOME, which flotilla sets"),
    ] {
        let mut env = vec![(launch.0.to_string(), launch.1.to_string())];
        assert_eq!(crew.render_crew(&mut env, &accounts()).expect_err("refused").for_environment("b1"), error);
        assert_eq!(env, vec![(launch.0.to_string(), launch.1.to_string())]);
    }
    let unbound = preparer(rust(), Some("/home/u")).prepare(None).expect("unbound");
    let mut env = vec![("CLAUDE_CONFIG_DIR".to_string(), "/state/agent-homes/claude".to_string())];
    assert!(unbound.render_crew(&mut env, &accounts()).is_err(), "with no bound layer no account variable is admitted");

    let mut env = vec![("FLOTILLA_CREW_ROLE".to_string(), "coder".to_string())];
    crew.render_crew(&mut env, &accounts()).expect("render");
    assert!(env.contains(&("GITHUB_TOKEN_FILE".to_string(), "/state/credentials/github/token".to_string())));
    assert!(env.contains(&("CLAUDE_CONFIG_DIR".to_string(), "/state/agent-homes/claude".to_string())));
    assert!(env.contains(&("RUSTUP_HOME".to_string(), "/home/u/.rustup".to_string())));
    assert!(env.contains(&(
        "PATH".to_string(),
        "/state/vessel/bin:/state/credentials/github:/home/u/.cargo/bin:/usr/local/bin:/usr/bin:/bin".to_string()
    )));
}

// An observation inherits the base layer and the families' defaults, never
// an account variable; each family home is left for the executor's
// throwaway root.
#[test]
fn an_observation_carries_the_base_layer_and_no_account_state() {
    let observation =
        preparer(rust(), Some("/home/u")).with_locale(|name| (name == "LANG").then_some("en_GB.UTF-8")).observation().expect("ok");
    assert_eq!(
        observation.assignments(),
        [
            "PATH=/state/vessel/bin:/home/u/.cargo/bin:/usr/local/bin:/usr/bin:/bin",
            "CARGO_HOME=/home/u/.cargo",
            "LANG=en_GB.UTF-8",
            "RUSTUP_HOME=/home/u/.rustup",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1",
        ]
    );
    assert_eq!(observation.account_homes(), ["CLAUDE_CONFIG_DIR", "GH_CONFIG_DIR"]);
}

// A declared base (an image's environment) keeps its PATH and variables but
// not its home, and refuses account state just as a supply does.
#[test]
fn a_declared_base_drops_home_and_refuses_account_state() {
    let declared =
        |pairs: &[(&str, &str)]| BaseLayer::declared(pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())), &accounts());
    let base = declared(&[("PATH", "/opt/bin:/usr/bin"), ("HOME", "/root"), ("JAVA_HOME", "/opt/java")]).expect("declared");
    assert_eq!(
        base.environment(),
        [("PATH".to_string(), "/opt/bin:/usr/bin".to_string()), ("JAVA_HOME".to_string(), "/opt/java".to_string())]
    );
    assert_eq!(declared(&[]).expect("empty").path(), "/usr/local/bin:/usr/bin:/bin");
    assert!(declared(&[("GH_TOKEN", "x")]).expect_err("refused").contains("`gh` account state"));
}

// Credential delivery prepends its wrapper directories to the runner's PATH;
// the bound layer keeps only those directories, and the other variables.
#[test]
fn delivered_material_becomes_the_bound_layer() {
    let base = preparer(rust(), Some("/home/u")).base().expect("base");
    let delivered = [
        ("PATH".to_string(), format!("/state/credentials/github:{}", base.path())),
        ("GITHUB_TOKEN_FILE".to_string(), "/state/credentials/github/token".to_string()),
    ];
    let bound = BoundLayer::delivered(&delivered, &base).expect("bound");
    assert_eq!(bound.path(), ["/state/credentials/github"]);
    assert_eq!(bound.variables().collect::<Vec<_>>(), [("GITHUB_TOKEN_FILE", "/state/credentials/github/token")]);
    assert!(BoundLayer::new(vec!["relative".into()], []).is_err());
    assert!(BoundLayer::new(Vec::new(), [("HOME".to_string(), "/x".to_string())]).is_err());
}

// Harness settings are non-secret settings only: one naming a variable
// flotilla composes (`PATH`, `HOME`, `FLOTILLA_*`) is refused, as a supply
// naming one is, so no rendering can carry two `PATH`s.
#[test]
fn a_harness_setting_cannot_name_a_composed_variable() {
    for name in ["PATH", "HOME", "FLOTILLA_ENVIRONMENT_ID"] {
        let preparer = preparer(rust(), Some("/home/u")).with_settings(&[(name.to_string(), "/x".to_string())]);
        assert_eq!(
            preparer.base().expect_err("refused"),
            PreparationError::HostConfiguration(format!("harness setting {name} is a variable flotilla composes"))
        );
    }
}

// A bound variable the launch also sets must agree with the bound layer,
// whether or not a family declares it; a launch PATH is replaced.
#[test]
fn a_launch_cannot_override_a_bound_variable() {
    let crew = preparer(rust(), Some("/home/u")).prepare(Some(bound())).expect("crew");
    let mut env = vec![("GITHUB_TOKEN_FILE".to_string(), "/elsewhere/token".to_string())];
    assert_eq!(
        crew.render_crew(&mut env, &accounts()).expect_err("refused"),
        PreparationError::Refused("the launch sets GITHUB_TOKEN_FILE, which its bound plan sets differently".into())
    );
    let mut env = vec![
        ("GITHUB_TOKEN_FILE".to_string(), "/state/credentials/github/token".to_string()),
        ("PATH".to_string(), "/launch/bin".to_string()),
    ];
    crew.render_crew(&mut env, &accounts()).expect("agreeing launch");
    assert_eq!(env.iter().filter(|(name, _)| name == "PATH").map(|(_, value)| value.as_str()).collect::<Vec<_>>(), [crew.crew_path()]);
}
