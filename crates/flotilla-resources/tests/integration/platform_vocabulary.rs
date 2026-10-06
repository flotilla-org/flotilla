use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{normalize_project_spec, CapabilityNeed, Platform, ProjectRepositorySpec, ProjectSpec, RepositoryKey};

fn project_with_matrix(platforms: &[&str]) -> ProjectSpec {
    ProjectSpec {
        charter: None,
        parent: None,
        platform_matrix: platforms.iter().map(|platform| platform.to_string()).collect(),
        display_name: "Widgets".to_string(),
        default_workflow_ref: "implement".to_string(),
        role_needs: BTreeMap::new(),
        skills: BTreeMap::new(),
        supervision: None,
        issue_source_bindings: Vec::new(),
        repositories: vec![ProjectRepositorySpec {
            charter_store: None,
            repo: RepositoryKey("acme/widgets".to_string()),
            alias: None,
            roles: BTreeSet::new(),
            subpath: None,
            default_branch: None,
        }],
        dispatch_policy: None,
    }
}

#[test]
fn vocabulary_lists_every_platform_once() {
    // Adding a variant breaks this match; add it to `Platform::ALL` too.
    let position = |platform: Platform| match platform {
        Platform::Linux => 0,
        Platform::Macos => 1,
        Platform::Windows => 2,
    };
    for (index, platform) in Platform::ALL.into_iter().enumerate() {
        assert_eq!(position(platform), index, "{platform} is listed out of place");
        assert_eq!(platform.as_str().parse::<Platform>(), Ok(platform));
        assert_eq!(platform.to_string(), platform.as_str());
    }
}

#[test]
fn macos_and_windows_are_the_reserved_platforms() {
    let reserved = Platform::ALL.into_iter().filter(|platform| platform.is_reserved()).collect::<BTreeSet<_>>();
    assert_eq!(reserved, BTreeSet::from([Platform::Macos, Platform::Windows]));
}

#[test]
fn capability_needs_and_project_matrices_accept_exactly_the_vocabulary() {
    for platform in Platform::ALL {
        let need = format!("platform:{platform}").parse::<CapabilityNeed>().expect("supported platform need");
        assert_eq!(need, CapabilityNeed::Platform(platform.as_str().to_string()));
        normalize_project_spec(project_with_matrix(&[platform.as_str()])).expect("supported platform matrix");
    }
    let matrix = format!("platform:{}", Platform::MATRIX_PLACEHOLDER).parse::<CapabilityNeed>().expect("matrix placeholder need");
    assert_eq!(matrix, CapabilityNeed::matrix_placeholder());

    assert!("freebsd".parse::<Platform>().is_err());
    assert!("platform:freebsd".parse::<CapabilityNeed>().is_err());
    assert!(normalize_project_spec(project_with_matrix(&["freebsd"])).is_err());
    assert!(normalize_project_spec(project_with_matrix(&[Platform::MATRIX_PLACEHOLDER])).is_err());
}
