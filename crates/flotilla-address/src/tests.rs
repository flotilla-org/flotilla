use hegel::generators as gs;

use crate::*;

/// Characters a value may hold: unreserved, every reserved byte the grammar
/// itself uses (`/ : @ % . "` and space), JSON escapes (`\`, tab, newline),
/// quotes, and multi-byte UTF-8.
const VALUE_ALPHABET: &str = "aZ09-._~/:@%?#\" '\\\t\n,()$éß中🚀";

fn value(tc: &hegel::TestCase, min: usize) -> String {
    tc.draw(gs::text().alphabet(VALUE_ALPHABET).min_size(min).max_size(8))
}

fn draw_index(tc: &hegel::TestCase, len: usize) -> usize {
    tc.draw(gs::integers::<usize>().min_value(0).max_value(len - 1))
}

/// A forge: the built-in `github`, or a Forgejo forge with a drawn id.
fn forge(tc: &hegel::TestCase) -> Forge {
    if tc.draw(gs::booleans()) {
        Forge::github()
    } else {
        let id = tc.draw(gs::from_regex("[a-z0-9]([a-z0-9-]{0,6}[a-z0-9])?").fullmatch(true));
        Forge::new(id, ForgeKind::Forgejo).expect("drawn forge ids are DNS labels")
    }
}

/// Values for a segment of `kind`, spanning each kind's admitted space:
/// numbers across the u64 range, both SHA lengths, and free text otherwise.
fn segment_value(tc: &hegel::TestCase, kind: Kind) -> String {
    match kind {
        Kind::ChangeRequest | Kind::Issue => tc.draw(gs::integers::<u64>().min_value(0).max_value(u64::MAX)).to_string(),
        Kind::Commit => {
            let length = if tc.draw(gs::booleans()) { 40 } else { 64 };
            tc.draw(gs::text().alphabet("0123456789abcdef").min_size(length).max_size(length))
        }
        // Usage values are trimmed and lowercase; the leading letter keeps
        // them non-empty once trimmed.
        Kind::Usage | Kind::Account => format!("u{}", value(tc, 0).to_lowercase()).trim_end().to_string(),
        _ => value(tc, 1),
    }
}

/// An address of any shape: a walk from a random root through children the
/// shape grammar admits, ending in an artifact subject when the walk reaches
/// one. Covers every kind, the root convoy, nested project/convoy/role and
/// repositories on both forge kinds.
fn address(tc: &hegel::TestCase) -> Address {
    let roots = [Kind::Fleet, Kind::Project, Kind::Convoy, Kind::Forge, Kind::Usage, Kind::Host, Kind::Account];
    let root = roots[draw_index(tc, roots.len())];
    let mut address = match root {
        Kind::Forge => {
            let forge = forge(tc);
            if tc.draw(gs::booleans()) {
                Address::forge(&forge)
            } else {
                let repository = format!("{}/{}", value(tc, 1).replace('/', "-"), value(tc, 1).replace('/', "-"));
                Address::repository(&forge, &repository).expect("owner/repo")
            }
        }
        kind => Address::root(kind, &segment_value(tc, kind)).expect("root"),
    };
    let depth = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    for _ in 0..depth {
        let children =
            Kind::ALL.into_iter().filter(|kind| *kind != Kind::Repo && kind.may_follow(Some(address.kind()))).collect::<Vec<_>>();
        if children.is_empty() || address.kind() == Kind::Artifact {
            break;
        }
        let kind = children[draw_index(tc, children.len())];
        address = if kind == Kind::Artifact {
            address.artifact(&value(tc, 1), &value(tc, 1)).expect("artifact")
        } else {
            address.child(kind, &segment_value(tc, kind)).expect("child")
        };
    }
    if address.kind() == Kind::Artifact {
        return address;
    }
    address
}

fn path(tc: &hegel::TestCase) -> FieldPath {
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    FieldPath::new((0..count).map(|_| value(tc, 0)).collect())
}

fn operator(tc: &hegel::TestCase) -> Operator {
    Operator::ALL[draw_index(tc, Operator::ALL.len())]
}

// ADR 0066 §4: canonical text parses with no lookups, and print-then-parse
// returns exactly what went in.
#[hegel::test(test_cases = 300)]
fn address_print_then_parse_is_identity(tc: hegel::TestCase) {
    let address = address(&tc);
    let text = address.to_string();
    let parsed: Address = text.parse().unwrap_or_else(|error| panic!("{text} must parse: {error}"));
    assert_eq!(parsed, address);
    assert_eq!(parsed.segments(), address.segments(), "decoded values are exactly the values built");
    assert_eq!(parsed.of(), address.of());
    // Canonical text is already resolved: resolving it changes nothing.
    assert_eq!(resolve(&text, &BuiltinForges).as_ref(), Ok(&address));
}

// ADR 0066 §4: one address has exactly one spelling. Lowercasing an escape or
// escaping a bare character yields text the strict parser refuses.
#[hegel::test(test_cases = 200)]
fn address_other_spellings_are_refused(tc: hegel::TestCase) {
    let text = address(&tc).to_string();
    if let Some(index) = text.find('%') {
        let escape = &text[index..index + 3];
        if escape != escape.to_lowercase() {
            let lowered = format!("{}{}{}", &text[..index], escape.to_lowercase(), &text[index + 3..]);
            assert!(lowered.parse::<Address>().is_err(), "{lowered} must be refused");
        }
    }
    let value_start = text.find(':').expect("a segment") + 1;
    if text[value_start..].starts_with(|c: char| c.is_ascii_alphanumeric()) {
        let byte = text.as_bytes()[value_start];
        let escaped = format!("{}%{byte:02X}{}", &text[..value_start], &text[value_start + 1..]);
        assert!(escaped.parse::<Address>().is_err(), "{escaped} must be refused");
    }
}

// ADR 0066 §4: X is under P exactly when X = P or X starts with `P/`, and the
// stored range bound `[P/, P0)` selects the same addresses.
#[hegel::test(test_cases = 300)]
fn prefix_matching_is_on_segment_boundaries(tc: hegel::TestCase) {
    let address = address(&tc);
    let mut prefixes = vec![address.clone()];
    while let Some(parent) = prefixes.last().and_then(Address::parent) {
        prefixes.push(parent);
    }
    let other = address_or_sibling(&tc, &address);
    for prefix in &prefixes {
        assert!(address.is_under(prefix), "{address} is under {prefix}");
        let (low, high) = prefix.descendant_bounds();
        let in_range = other.as_str() >= low.as_str() && other.as_str() < high.as_str();
        assert_eq!(other.is_under(prefix), &other == prefix || in_range, "{other} against {prefix}");
    }
}

/// The address itself or a sibling whose last value extends or shares a
/// prefix with the original, as `convoy:demo2` does with `convoy:demo`.
fn address_or_sibling(tc: &hegel::TestCase, address: &Address) -> Address {
    let Some(parent) = address.parent() else { return address.clone() };
    if address.kind() == Kind::Artifact || address.kind() == Kind::Repo || !tc.draw(gs::booleans()) {
        return address.clone();
    }
    let sibling = format!("{}{}", address.name(), segment_value(tc, address.kind()));
    parent.child(address.kind(), &sibling).unwrap_or_else(|_| address.clone())
}

// ADR 0066 §4: conditions print canonically and parse back exactly, whatever
// their path segments and literal hold.
#[hegel::test(test_cases = 300)]
fn condition_print_then_parse_is_identity(tc: hegel::TestCase) {
    let condition = Condition { address: address(&tc), path: path(&tc), operator: operator(&tc), literal: value(&tc, 0) };
    let text = condition.to_string();
    assert_eq!(text.parse::<Condition>().as_ref(), Ok(&condition), "{text}");
    assert_eq!(Condition::resolve(&text, &BuiltinForges).as_ref(), Ok(&condition), "canonical text is valid lenient input");
}

// ADR 0066 §4: templates share the path and literal rules.
#[hegel::test(test_cases = 300)]
fn template_print_then_parse_is_identity(tc: hegel::TestCase) {
    let name = tc.draw(gs::from_regex("[a-z][a-z0-9_-]{0,6}").fullmatch(true));
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let arguments = (0..count).map(|_| value(&tc, 0)).collect();
    let template = Template {
        subject: Variable::new(name, arguments).expect("variable"),
        path: path(&tc),
        operator: operator(&tc),
        literal: value(&tc, 0),
    };
    let text = template.to_string();
    assert_eq!(text.parse::<Template>().as_ref(), Ok(&template), "{text}");
    assert_eq!(Template::resolve(&text).as_ref(), Ok(&template), "canonical text is valid lenient input");
}

// ADR 0066 §4: a condition has one spelling. Quoting a bare word, a JSON
// escape `serde_json` would not write, or a doubled space is refused by the
// strict parser, though lenient input accepts each.
#[test]
fn condition_other_spellings_are_refused() {
    for text in [
        r#"convoy:demo ."status" == x"#,
        r#"convoy:demo .status == "x""#,
        r#"convoy:demo .status == "\u0061 b""#,
        "convoy:demo  .status == x",
        "convoy:demo .status == x ",
        "convoy:demo .status == a b",
    ] {
        assert!(text.parse::<Condition>().is_err(), "{text} must be refused");
        assert!(Condition::resolve(text, &BuiltinForges).is_ok(), "{text} is valid lenient input");
    }
}

// `@` separates a reference's revision; it is always encoded inside an
// address, so a subject or revision holding `@` still round-trips.
#[test]
fn references_split_at_the_unencoded_at() {
    let artifact = convoy_role("demo", "coder").artifact("note", "a@b").expect("artifact");
    let reference = Reference::new(artifact, Revision::new("rev@1").expect("revision"));
    assert!(reference.to_string().ends_with("of:a%40b@rev%401"));
    assert_eq!(reference.to_string().parse::<Reference>(), Ok(reference));
}

// A reference's text and serde forms both round-trip.
#[hegel::test(test_cases = 200)]
fn reference_round_trips(tc: hegel::TestCase) {
    let reference = Reference::new(address(&tc), Revision::new(value(&tc, 1)).expect("revision"));
    assert_eq!(reference.to_string().parse::<Reference>().as_ref(), Ok(&reference));
    let json = serde_json::to_string(&reference).expect("serialize");
    assert_eq!(serde_json::from_str::<Reference>(&json).expect("deserialize"), reference);
}

fn github_cr(number: u64) -> Address {
    Address::change_request(&Forge::github(), "flotilla-org/flotilla", number).expect("cr")
}

fn convoy_role(convoy: &str, role: &str) -> Address {
    Address::project("flotilla")
        .and_then(|project| project.child(Kind::Convoy, convoy))
        .and_then(|c| c.child(Kind::Role, role))
        .expect("role")
}

fn round_trips_condition(literal: &str) {
    let condition = Condition {
        address: Address::convoy("demo").expect("convoy"),
        path: FieldPath::new(vec!["status".into()]),
        operator: Operator::Equal,
        literal: literal.into(),
    };
    assert_eq!(condition.to_string().parse::<Condition>(), Ok(condition));
}

/// ADR 0066 appendix: each round-trip failure found in the old parsers is a
/// fixed case, and print-then-parse must return exactly the input.
mod appendix {
    use super::*;

    #[test]
    fn leaf_literals_keep_spaces_tabs_and_quotes() {
        for literal in ["a  b", " a", "a\tb", "\"quoted\"", "'q'", ""] {
            round_trips_condition(literal);
        }
    }

    #[test]
    fn leaf_field_paths_with_spaces_and_the_empty_path_round_trip() {
        for segments in [vec!["a b".to_string()], vec![], vec!["labels".into(), "good first issue".into()]] {
            let path = FieldPath::new(segments);
            assert_eq!(path.to_string().parse::<FieldPath>().as_ref(), Ok(&path));
        }
        assert_eq!(FieldPath::new(vec!["labels".into(), "good first issue".into()]).to_string(), r#".labels."good first issue""#);
    }

    #[test]
    fn convoy_names_with_spaces_round_trip() {
        let convoy = Address::convoy("my convoy").expect("convoy");
        assert_eq!(convoy.as_str(), "convoy:my%20convoy");
        assert_eq!(convoy.as_str().parse::<Address>().as_ref(), Ok(&convoy));
    }

    // A forge named by a host or URL is input only, and a forge id cannot hold
    // `/`, so a service cannot silently re-parse as a shifted scope.
    #[test]
    fn services_with_slashes_or_schemes_are_refused_not_shifted() {
        for service in ["forge.example/team", "https://forge.example"] {
            assert!(Forge::new(service, ForgeKind::Forgejo).is_err());
            assert!(resolve(&format!("cr/{service}/o/r/1"), &BuiltinForges).is_err());
        }
        let encoded = format!("forge:{}/repo:o%2Fr/cr:1", encode("forge.example/team"));
        assert!(matches!(encoded.parse::<Address>(), Err(Error::InvalidValue { kind: Kind::Forge, .. })));
    }

    #[test]
    fn artifact_producers_with_slashes_stay_one_value() {
        let artifact = convoy_role("demo", "p/q").artifact("review-round", "subject").expect("artifact");
        let parsed: Address = artifact.as_str().parse().expect("parse");
        assert_eq!(parsed.get(Kind::Role), Some("p/q"));
        assert_eq!(parsed.name(), "review-round");
        assert_eq!(parsed.of(), Some("subject"));
    }

    #[test]
    fn empty_scopes_and_empty_artifact_subjects_are_refused() {
        assert!(resolve("cr/github//1", &BuiltinForges).is_err());
        assert!("forge:github/repo:/cr:1".parse::<Address>().is_err());
        assert!(convoy_role("demo", "coder").artifact("review-round", "").is_err());
        assert!("project:flotilla/convoy:demo/role:coder/artifact:review-round/of:".parse::<Address>().is_err());
        assert!("project:flotilla/convoy:demo/role:coder/artifact:review-round".parse::<Address>().is_err());
    }

    #[test]
    fn numbers_with_leading_zeros_or_signs_are_refused() {
        for number in ["007", "+7", "-7", "", "7a", "18446744073709551616"] {
            assert!(format!("forge:github/repo:o%2Fr/cr:{number}").parse::<Address>().is_err(), "{number}");
            assert!(resolve(&format!("cr/github/o/r/{number}"), &BuiltinForges).is_err(), "{number}");
        }
        assert_eq!(github_cr(7).as_str(), "forge:github/repo:flotilla-org%2Fflotilla/cr:7");
    }

    #[test]
    fn templates_keep_literals_and_need_no_leading_dot_workaround() {
        for literal in ["a  b", "\"q\""] {
            let template: Template = format!("$cr .state == {}", serde_json::to_string(literal).expect("json")).parse().expect("template");
            assert_eq!(template.literal, literal);
            assert_eq!(template.to_string().parse::<Template>(), Ok(template));
        }
        assert!("$cr state == merged".parse::<Template>().is_err(), "a field path starts with `.`");
    }

    #[test]
    fn template_arguments_may_hold_dots_commas_and_whitespace() {
        let subject = Variable::new("artifact", vec!["p.q".into(), "k,1".into(), "a b".into()]).expect("variable");
        let template = Template { subject, path: FieldPath::default(), operator: Operator::Equal, literal: "x".into() };
        assert_eq!(template.to_string(), r#"$artifact("p.q","k,1","a b") . == x"#);
        assert_eq!(template.to_string().parse::<Template>(), Ok(template));
    }
}

/// ADR 0066 §3: normalisation happens once, in the constructor; one test per
/// rule.
mod normalisation {
    use super::*;

    /// A lookup standing in for Forge resources: one Forgejo forge, `lab`,
    /// claiming two hosts.
    struct Lab;

    impl ForgeLookup for Lab {
        fn forge_by_id(&self, id: &str) -> Option<Forge> {
            (id == "lab").then(|| Forge::new("lab", ForgeKind::Forgejo).expect("forge"))
        }

        fn forge_by_host(&self, host: &str) -> Option<Forge> {
            matches!(host, "forgejo.lab.example" | "lab-ssh").then(|| Forge::new("lab", ForgeKind::Forgejo).expect("forge"))
        }
    }

    #[test]
    fn a_forge_is_named_by_its_id_and_github_is_built_in() {
        let expected = github_cr(42);
        for input in [
            "cr/github/flotilla-org/flotilla/42",
            "cr/github.com/flotilla-org/flotilla/42",
            "https://github.com/flotilla-org/flotilla/pull/42",
            "forge:github.com/repo:flotilla-org%2Fflotilla/cr:42",
        ] {
            assert_eq!(resolve(input, &BuiltinForges).as_ref(), Ok(&expected), "{input}");
        }
        assert_eq!(
            resolve("https://forgejo.lab.example/team/repo/pulls/3", &Lab).map(|a| a.to_string()).as_deref(),
            Ok("forge:lab/repo:team%2Frepo/cr:3")
        );
    }

    #[test]
    fn a_host_with_no_forge_is_refused_naming_the_host() {
        assert_eq!(resolve("https://forge.example/o/r/pulls/1", &BuiltinForges), Err(Error::UnresolvedForge("forge.example".into())));
        assert_eq!(resolve("cr/lab/o/r/1", &BuiltinForges), Err(Error::UnresolvedForge("lab".into())));
        assert_eq!(resolve("forge:lab.example/repo:o%2Fr", &BuiltinForges), Err(Error::UnresolvedForge("lab.example".into())));
        // Canonical text needs no lookup, even for a forge the lookup does not know.
        assert_eq!(resolve("forge:lab/repo:o%2Fr", &BuiltinForges).map(|a| a.to_string()).as_deref(), Ok("forge:lab/repo:o%2Fr"));
    }

    #[test]
    fn the_github_forge_id_is_only_public_github() {
        assert!(Forge::new("github", ForgeKind::Forgejo).is_err());
        assert_eq!(Forge::new("github", ForgeKind::Github), Ok(Forge::github()));
    }

    #[test]
    fn forge_ids_are_lowercase_dns_labels() {
        for id in ["Lab", "-lab", "lab-", "", "l.ab", "l_ab"] {
            assert!(Forge::new(id, ForgeKind::Forgejo).is_err(), "{id}");
        }
    }

    // GitHub and Forgejo both look up owner and repo case-insensitively.
    #[test]
    fn github_and_forgejo_lowercase_owner_and_repo() {
        assert_eq!(Address::change_request(&Forge::github(), "Flotilla-Org/Flotilla", 1).expect("cr"), github_cr(1));
        let lab = Forge::new("lab", ForgeKind::Forgejo).expect("forge");
        assert_eq!(Address::repository(&lab, "Team/Repo").expect("repo").as_str(), "forge:lab/repo:team%2Frepo");
        assert_eq!(
            resolve("forge:lab/repo:Team%2FRepo/issue:4", &Lab).map(|a| a.to_string()).as_deref(),
            Ok("forge:lab/repo:team%2Frepo/issue:4"),
            "lenient canonical-shaped input takes the forge's case rule"
        );
        assert!("forge:github/repo:Flotilla-Org%2Fflotilla".parse::<Address>().is_err(), "stored GitHub text is lowercase");
        assert!(Address::root(Kind::Forge, "github").and_then(|forge| forge.child(Kind::Repo, "o/r")).is_err(), "repo needs its forge");
    }

    #[test]
    fn mixed_case_urls_and_ssh_remotes_resolve_to_one_address() {
        let repository = Address::repository(&Forge::github(), "flotilla-org/flotilla").expect("repo");
        for input in [
            "https://GitHub.com/Flotilla-Org/Flotilla",
            "https://github.com/flotilla-org/flotilla.git",
            "git@github.com:Flotilla-Org/Flotilla.git",
            "ssh://git@github.com/flotilla-org/flotilla",
        ] {
            assert_eq!(resolve(input, &BuiltinForges).as_ref(), Ok(&repository), "{input}");
        }
        assert_eq!(resolve("git@lab-ssh:Team/Repo.git", &Lab).map(|a| a.to_string()).as_deref(), Ok("forge:lab/repo:team%2Frepo"));
    }

    #[test]
    fn commits_are_full_lowercase_shas() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let commit = resolve(&format!("https://github.com/o/r/commit/{}", sha.to_uppercase()), &BuiltinForges).expect("commit");
        assert_eq!(commit.as_str(), format!("forge:github/repo:o%2Fr/commit:{sha}"));
        assert!(format!("forge:github/repo:o%2Fr/commit:{}", &sha[..12]).parse::<Address>().is_err());
    }

    // Strict parsing knows no forge kind but `github`'s, so another forge's
    // repository case is not checked there; `resolve` applies its rule.
    #[test]
    fn strict_parse_checks_case_only_for_github_and_resolve_normalises_known_forges() {
        let typed = "forge:lab/repo:Team%2FRepo";
        assert_eq!(typed.parse::<Address>().map(|a| a.to_string()).as_deref(), Ok(typed));
        assert_eq!(resolve(typed, &Lab).map(|a| a.to_string()).as_deref(), Ok("forge:lab/repo:team%2Frepo"));
        assert!("forge:github/repo:Team%2FRepo".parse::<Address>().is_err());
    }

    // Usage records compare providers and accounts trimmed and lowercased, so
    // the address does too: one account has one address and one record.
    #[test]
    fn usage_providers_and_accounts_are_trimmed_and_lowercased() {
        let usage = Address::usage(" Claude", "Team@Example.com ").expect("usage");
        assert_eq!(usage.as_str(), "usage:claude/account:team%40example.com");
        assert!("usage:Claude/account:team".parse::<Address>().is_err());
        assert!(Address::root(Kind::Usage, "Claude").is_err());
        assert_eq!(resolve("usage:Claude/account:Team", &BuiltinForges).map(|a| a.to_string()).as_deref(), Ok("usage:claude/account:team"));
        assert_eq!(resolve("usage/Claude/Team", &BuiltinForges).map(|a| a.to_string()).as_deref(), Ok("usage:claude/account:team"));
    }

    #[test]
    fn browser_urls_past_the_number_resolve() {
        for suffix in ["/files", "/commits", "/"] {
            let input = format!("https://github.com/flotilla-org/flotilla/pull/42{suffix}");
            assert_eq!(resolve(&input, &BuiltinForges).as_ref(), Ok(&github_cr(42)), "{input}");
        }
    }

    #[test]
    fn short_forms_resolve_to_typed_segments() {
        assert_eq!(resolve("convoy/demo", &BuiltinForges).map(|a| a.to_string()).as_deref(), Ok("convoy:demo"));
        assert_eq!(resolve("usage/claude/team", &BuiltinForges).map(|a| a.to_string()).as_deref(), Ok("usage:claude/account:team"));
        assert_eq!(
            resolve("artifact/demo/coder/decision-ledger/a/b", &BuiltinForges).map(|a| a.to_string()).as_deref(),
            Ok("convoy:demo/role:coder/artifact:decision-ledger/of:a%2Fb")
        );
    }

    #[test]
    fn lenient_conditions_accept_the_old_text() {
        let condition =
            Condition::resolve("cr/github.com/Flotilla-Org/flotilla/1363   .state  ==  merged", &BuiltinForges).expect("condition");
        assert_eq!(condition.to_string(), "forge:github/repo:flotilla-org%2Fflotilla/cr:1363 .state == merged");
        let quoted = Condition::resolve("convoy/demo .latest-claim.disposition == 'changes pushed'", &BuiltinForges).expect("condition");
        assert_eq!(quoted.literal, "changes pushed");
        assert_eq!(quoted.to_string(), r#"convoy:demo .latest-claim.disposition == "changes pushed""#);
        let unquoted = Condition::resolve("convoy/demo .status.phase == changes pushed", &BuiltinForges).expect("condition");
        assert_eq!(unquoted.literal, "changes pushed");
    }

    // Stored templates and conditions written by the old parsers may hold
    // path segments with any non-whitespace character; they still decode,
    // and print quoted.
    #[test]
    fn lenient_paths_accept_old_segments() {
        let template = Template::resolve("$issue.labels.kind:bug == true").expect("template");
        assert_eq!(template.path.segments(), ["labels", "kind:bug"]);
        assert_eq!(template.to_string(), r#"$issue .labels."kind:bug" == true"#);
        assert!("$issue .labels.kind:bug == true".parse::<Template>().is_err(), "strict input quotes it");
    }

    #[test]
    fn lenient_templates_accept_the_old_text() {
        let template = Template::resolve("$cr.state == merged").expect("template");
        assert_eq!(template.to_string(), "$cr .state == merged");
        let artifact = Template::resolve("$artifact(coder,decision-ledger,convoy).summary.outcome == done").expect("template");
        assert_eq!(artifact.subject.arguments(), ["coder", "decision-ledger", "convoy"]);
        assert_eq!(artifact.to_string(), "$artifact(coder,decision-ledger,convoy) .summary.outcome == done");
    }
}

/// ADR 0066 §4: the shape grammar.
mod shape {
    use super::*;

    #[test]
    fn of_is_valid_only_directly_after_artifact() {
        assert_eq!("convoy:demo/of:x".parse::<Address>(), Err(Error::OfNotAfterArtifact));
        assert_eq!("convoy:demo/role:r/artifact:k/of:x/role:r".parse::<Address>(), Err(Error::OfNotLast));
        assert!("of:x".parse::<Address>().is_err());
    }

    #[test]
    fn unknown_kinds_and_misplaced_kinds_are_refused() {
        assert_eq!("work:demo".parse::<Address>(), Err(Error::UnknownKind("work".into())));
        assert_eq!("cr:1".parse::<Address>(), Err(Error::Shape { kind: Kind::ChangeRequest, parent: None }));
        assert!("project:p/forge:github".parse::<Address>().is_err());
        assert!("".parse::<Address>().is_err());
    }

    #[test]
    fn adr_examples_parse() {
        for text in [
            "project:flotilla/convoy:demo/role:coder",
            "project:flotilla/role:gui-approver",
            "project:flotilla/convoy:demo",
            "forge:github/repo:flotilla-org%2Fflotilla",
            "forge:github/repo:flotilla-org%2Fflotilla/cr:42",
            "forge:github/repo:flotilla-org%2Fflotilla/issue:7",
            "forge:github/repo:flotilla-org%2Fflotilla/commit:0123456789abcdef0123456789abcdef01234567",
            "usage:claude/account:team",
        ] {
            assert_eq!(text.parse::<Address>().map(|a| a.to_string()).as_deref(), Ok(text));
        }
    }

    // ADR 0066 §8: an artifact whose subject is an address holds that
    // address's canonical text as one value.
    #[test]
    fn artifact_subjects_that_are_addresses_nest_as_one_value() {
        let commit = Address::repository(&Forge::github(), "flotilla-org/flotilla")
            .and_then(|repo| repo.child(Kind::Commit, "0123456789abcdef0123456789abcdef01234567"))
            .expect("commit");
        let artifact = convoy_role("demo", "reviewer").artifact("review-round", commit.as_str()).expect("artifact");
        assert_eq!(
            artifact.as_str(),
            "project:flotilla/convoy:demo/role:reviewer/artifact:review-round/of:forge%3Agithub%2Frepo%3Aflotilla-org%252Fflotilla%2Fcommit%3A0123456789abcdef0123456789abcdef01234567"
        );
        assert_eq!(artifact.of().map(str::parse::<Address>), Some(Ok(commit)));
    }

    #[test]
    fn convoy_demo2_is_not_under_convoy_demo() {
        let demo = Address::convoy("demo").expect("convoy");
        let demo2 = Address::convoy("demo2").expect("convoy");
        let role = demo.child(Kind::Role, "coder").expect("role");
        assert!(!demo2.is_under(&demo));
        assert!(role.is_under(&demo));
        assert!(demo.is_under(&demo));
        assert!(!demo.is_under(&role));
    }

    #[test]
    fn a_message_sits_under_its_inbox_role() {
        let inbox = Address::project("fleet").and_then(|p| p.child(Kind::Role, "turn-rules")).expect("role");
        let message = Address::message(&inbox, "message-0a1b").expect("message");
        assert_eq!(message.as_str(), "project:fleet/role:turn-rules/message:message-0a1b");
        assert_eq!(message.parent(), Some(inbox.clone()));
        assert_eq!(message.as_str().parse::<Address>().expect("parses"), message);
        assert!(Address::message(&Address::project("fleet").expect("project"), "x").is_err());
        assert!("message:x".parse::<Address>().is_err());
        assert!(message.child(Kind::Message, "y").is_err());
    }
}

// Fleet roles and their messages preserve arbitrary nonempty names, reserved
// bytes and Unicode through canonical print/parse and lenient escape spelling.
#[hegel::test]
fn fleet_roles_and_messages_round_trip(tc: hegel::TestCase) {
    let fleet = value(&tc, 1);
    let role = value(&tc, 1);
    let id = value(&tc, 1);
    let inbox = Address::fleet(&fleet).expect("fleet").child(Kind::Role, &role).expect("role");
    for address in [inbox.clone(), Address::message(&inbox, &id).expect("message")] {
        assert_eq!(address.get(Kind::Fleet), Some(fleet.as_str()));
        assert_eq!(address.to_string().parse::<Address>().expect("canonical"), address);
        let lenient = address.to_string().replace("%2F", "%2f").replace("%3A", "%3a");
        assert_eq!(resolve(&format!(" {lenient} "), &BuiltinForges).expect("lenient"), address);
    }
}

// Fleet is a root whose only direct child is a role; empty names are refused.
#[test]
fn fleet_shape_and_short_input() {
    assert_eq!(resolve("fleet/lab", &BuiltinForges).expect("short").as_str(), "fleet:lab");
    assert_eq!(
        resolve(" fleet:La%2fb/role:op%3aer/message:x%2fy ", &BuiltinForges).expect("lenient message").as_str(),
        "fleet:La%2Fb/role:op%3Aer/message:x%2Fy",
    );
    for invalid in ["fleet:", "fleet:lab/role:", "project:p/fleet:lab", "fleet:lab/convoy:c", "fleet:lab/message:m", "role:operator"] {
        assert!(invalid.parse::<Address>().is_err(), "{invalid}");
    }
    assert_eq!(record_name(&Address::fleet("lab").expect("fleet")), None);
}
