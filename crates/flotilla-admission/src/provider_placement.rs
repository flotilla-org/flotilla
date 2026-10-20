//! Provider matching is a filter over declarations, never a ranking policy.
use flotilla_protocol::provider::{
    CheckoutStrategy, EgressEnforcement, EnvironmentClass, MaterialMode, ProviderAdvertisement, ProviderReadiness,
};
use flotilla_resources::{DockerCheckoutStrategy, PlacementPolicySpec, ProviderRequirements};

/// Resolve one instance for one vessel's policy. Explicit requirements augment
/// the actual checkout's requirements rather than weakening them.
pub fn match_provider<'a>(
    host: &str,
    spec: &PlacementPolicySpec,
    labelled_instance: Option<&str>,
    advertisements: &'a [ProviderAdvertisement],
) -> Result<&'a ProviderAdvertisement, String> {
    let mut requirements = spec.provider_requirements.clone().unwrap_or_default();
    if let Some(label) = labelled_instance {
        if requirements.instance.as_deref().is_some_and(|instance| instance != label) {
            return Err(format!("{host}: provider instance disagrees with policy label `{label}`"));
        }
        requirements.instance = Some(label.into());
    }
    if spec.provider_requirements.is_some() && requirements.instance.is_none() && requirements.class.is_none() {
        return Err(format!("{host}: provider requirements need an instance or class"));
    }
    let checkout = match (&spec.host_direct, &spec.docker_per_vessel) {
        (Some(_), None) => {
            if requirements.instance.is_none() && requirements.class.is_none() {
                requirements.class = Some(EnvironmentClass::HostDirect);
            }
            Some(CheckoutStrategy::HostWorktree)
        }
        (None, Some(docker)) => {
            if requirements.instance.is_none() && requirements.class.is_none() {
                requirements.class = Some(EnvironmentClass::Container);
            }
            Some(match docker.checkout {
                DockerCheckoutStrategy::WorktreeOnHostAndMount { .. } => CheckoutStrategy::HostWorktree,
                DockerCheckoutStrategy::LocalCloneOnHostAndMount { .. } => CheckoutStrategy::HostClone,
                DockerCheckoutStrategy::FreshCloneInContainer { .. } => CheckoutStrategy::CloneInVessel,
            })
        }
        _ => return Err(format!("{host}: placement must define exactly one execution strategy")),
    };
    // Class fixes input shape (ADR 0058); capabilities still determine behavior.
    // The legacy per-vessel input carrier also serves Dummy and future kinds.
    let incompatible_class = |class| (class == EnvironmentClass::HostDirect) != spec.host_direct.is_some();
    let strategy = if spec.host_direct.is_some() { "host-direct" } else { "per-vessel" };
    if let Some(class) = requirements.class.filter(|class| incompatible_class(*class)) {
        return Err(format!("{host}: provider class {class:?} is incompatible with {strategy} execution strategy"));
    }
    let identity_matches = advertisements
        .iter()
        .filter(|provider| {
            requirements.instance.as_ref().is_none_or(|instance| provider.instance == *instance)
                && requirements.class.is_none_or(|class| provider.kind.as_ref().is_some_and(|kind| kind.class == class))
        })
        .collect::<Vec<_>>();
    if identity_matches.is_empty() {
        return Err(format!("{host}: no advertised provider matches instance {:?} class {:?}", requirements.instance, requirements.class));
    }
    // For every identity selected above, count duplicates across the whole
    // slice, including entries excluded by the class filter. Unrelated names
    // do not affect this policy's selection.
    for provider in &identity_matches {
        if advertisements.iter().filter(|other| other.instance == provider.instance).count() != 1 {
            return Err(format!("{host}: ambiguous provider instance `{}` advertised more than once", provider.instance));
        }
    }
    let mut matching = Vec::new();
    let mut reasons = Vec::new();
    for provider in identity_matches {
        let missing = match provider.kind.as_ref().map(|kind| kind.class).filter(|class| incompatible_class(*class)) {
            Some(class) => Some(format!("class {class:?} is incompatible with {strategy} execution strategy")),
            None => missing_requirement(provider, &requirements, checkout),
        };
        match missing {
            None => matching.push(provider),
            Some(reason) => reasons.push(format!("{host}: {} {reason}", provider.instance)),
        }
    }
    match matching.as_slice() {
        [provider] => Ok(provider),
        [] => Err(reasons.join("; ")),
        _ => Err(format!(
            "{host}: ambiguous provider instances: {}",
            matching.iter().map(|provider| provider.instance.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

fn missing_requirement(
    provider: &ProviderAdvertisement,
    requirements: &ProviderRequirements,
    checkout: Option<CheckoutStrategy>,
) -> Option<String> {
    // When several defects coexist, report the actionable readiness failure
    // first. Both unknown readiness and incomplete identity still fail closed.
    match &provider.readiness {
        ProviderReadiness::Ready => {}
        ProviderReadiness::MissingTool { reason } => return Some(format!("missing tool: {reason}")),
        ProviderReadiness::MissingCredential { reason } => return Some(format!("missing credential: {reason}")),
        ProviderReadiness::Unknown { reason } => return Some(format!("readiness unknown: {reason}")),
    }
    if provider.instance.trim().is_empty() || provider.kind.is_none() {
        return Some("has incomplete identity".into());
    }
    let capabilities = &provider.capabilities;
    let checkouts = checkout.into_iter().chain(requirements.checkout_strategy);
    let mut materials = requirements.material_modes.clone();
    for strategy in checkouts {
        if matches!(strategy, CheckoutStrategy::HostWorktree | CheckoutStrategy::HostClone) {
            materials.push(MaterialMode::Shared);
        }
        if !capabilities.checkout_strategies.contains(&strategy) {
            return Some(format!("ready but no {strategy:?} checkout"));
        }
    }
    for material in materials {
        if !capabilities.material_modes.contains(&material) {
            return Some(format!("ready but no {material:?} material"));
        }
    }
    if requirements.egress_enforcement == Some(EgressEnforcement::Enforced)
        && capabilities.egress_enforcement != EgressEnforcement::Enforced
    {
        return Some("ready but no Enforced egress".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_protocol::provider::{ProviderCapabilities, ProviderKind};
    use hegel::generators as gs;

    fn policy(class: EnvironmentClass) -> PlacementPolicySpec {
        let mut spec: PlacementPolicySpec = serde_json::from_value(serde_json::json!({
            "pool": "cleat", "docker_per_vessel": { "host_ref": "feta",
                "checkout": { "fresh_clone_in_container": { "clone_path": "/workspace" } } }
        }))
        .unwrap();
        if class == EnvironmentClass::HostDirect {
            spec.docker_per_vessel = None;
            spec.host_direct = Some(flotilla_resources::HostDirectPlacementPolicySpec {
                host_ref: "feta".into(),
                checkout: flotilla_resources::HostDirectPlacementPolicyCheckout::Worktree,
            });
        }
        spec.provider_requirements = Some(ProviderRequirements { class: Some(class), ..Default::default() });
        spec
    }
    fn provider(class: EnvironmentClass) -> ProviderAdvertisement {
        ProviderAdvertisement {
            instance: "sandbox".into(),
            kind: Some(ProviderKind::new(class, "open-implementation-name")),
            readiness: ProviderReadiness::Ready,
            capabilities: ProviderCapabilities {
                material_modes: vec![MaterialMode::Frozen, MaterialMode::Pushed, MaterialMode::Shared],
                checkout_strategies: vec![CheckoutStrategy::HostWorktree, CheckoutStrategy::HostClone, CheckoutStrategy::CloneInVessel],
                egress_enforcement: EgressEnforcement::Enforced,
                ..Default::default()
            },
        }
    }

    // ADR 0058: identity or class plus capabilities selects exactly one ready
    // instance. Generated space spans every class, explicit/class selection,
    // empty inventories, duplicate identities and distinct ambiguous instances.
    #[hegel::test]
    fn selects_exactly_one_matching_instance(tc: hegel::TestCase) {
        let classes = [
            EnvironmentClass::HostDirect,
            EnvironmentClass::Container,
            EnvironmentClass::MicroVm,
            EnvironmentClass::CloudSandbox,
            EnvironmentClass::Dummy,
        ];
        let class = classes[tc.draw(gs::integers::<usize>().min_value(0).max_value(4))];
        let explicit = tc.draw(gs::booleans());
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let duplicate = tc.draw(gs::booleans());
        let mut spec = policy(class);
        if explicit {
            spec.provider_requirements.as_mut().unwrap().instance = Some("sandbox".into());
        }
        let providers: Vec<_> = (0..count)
            .map(|i| {
                let mut entry = provider(class);
                if i > 0 && !duplicate {
                    entry.instance = format!("other-{i}");
                }
                entry
            })
            .collect();
        let result = match_provider("feta", &spec, None, &providers);
        let expected = count == 1 || (explicit && count > 1 && !duplicate);
        assert_eq!(result.is_ok(), expected);
        if let Ok(selected) = result {
            assert_eq!(selected.instance, "sandbox");
        }
    }

    // Each material mode, checkout strategy and enforcing egress requirement
    // fails with the host and exact missing piece, irrespective of runtime name.
    #[hegel::test]
    fn missing_capabilities_have_owned_reasons(tc: hegel::TestCase) {
        let case = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
        let mut spec = policy(EnvironmentClass::MicroVm);
        let mut entry = provider(EnvironmentClass::MicroVm);
        let requirement = spec.provider_requirements.as_mut().unwrap();
        let expected = match case {
            0..=2 => {
                let material = [MaterialMode::Frozen, MaterialMode::Pushed, MaterialMode::Shared][case];
                requirement.material_modes.push(material);
                entry.capabilities.material_modes.retain(|mode| *mode != material);
                format!("{material:?} material")
            }
            3..=5 => {
                let strategy = [CheckoutStrategy::HostWorktree, CheckoutStrategy::HostClone, CheckoutStrategy::CloneInVessel][case - 3];
                requirement.checkout_strategy = Some(strategy);
                entry.capabilities.checkout_strategies.retain(|mode| *mode != strategy);
                format!("{strategy:?} checkout")
            }
            _ => {
                requirement.egress_enforcement = Some(EgressEnforcement::Enforced);
                entry.capabilities.egress_enforcement = EgressEnforcement::Advisory;
                "Enforced egress".into()
            }
        };
        let error = match_provider("feta", &spec, None, &[entry]).unwrap_err();
        assert!(error.contains("feta: sandbox ready but no"), "{error}");
        assert!(error.contains(&expected), "{error}");
    }

    // Readiness is a local provider fact: no class, including CloudSandbox,
    // bypasses missing credentials/tools or unknown status. Old records are unknown.
    #[hegel::test]
    fn readiness_fails_closed_on_controlling_host(tc: hegel::TestCase) {
        let state = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let mut entry = provider(EnvironmentClass::CloudSandbox);
        entry.readiness = match state {
            0 => ProviderReadiness::Ready,
            1 => ProviderReadiness::MissingTool { reason: "sandbox CLI absent".into() },
            2 => ProviderReadiness::MissingCredential { reason: "account token absent".into() },
            _ => ProviderReadiness::default(),
        };
        let spec = policy(EnvironmentClass::CloudSandbox);
        let result = match_provider("controller", &spec, None, std::slice::from_ref(&entry)).map(|entry| entry.instance.clone());
        assert_eq!(result.is_ok(), state == 0);
        if let Err(reason) = result {
            assert!(reason.contains("controller: sandbox"));
            if state == 2 {
                assert!(reason.contains("missing credential: account token absent"));
            }
        }
        let mut old = serde_json::to_value(&entry).unwrap();
        old.as_object_mut().unwrap().remove("readiness");
        let old: ProviderAdvertisement = serde_json::from_value(old).unwrap();
        assert!(match_provider("controller", &spec, None, &[old]).unwrap_err().contains("unknown"));
    }

    // Enforcement is a minimum, so advisory and omitted requirements accept
    // either provider declaration. Enforced accepts only enforced providers.
    #[hegel::test]
    fn enforcement_is_a_minimum(tc: hegel::TestCase) {
        let required = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let enforcing = tc.draw(gs::booleans());
        let mut spec = policy(EnvironmentClass::Container);
        spec.provider_requirements.as_mut().unwrap().egress_enforcement = match required {
            0 => None,
            1 => Some(EgressEnforcement::Advisory),
            _ => Some(EgressEnforcement::Enforced),
        };
        let mut entry = provider(EnvironmentClass::Container);
        entry.capabilities.egress_enforcement = if enforcing { EgressEnforcement::Enforced } else { EgressEnforcement::Advisory };
        assert_eq!(match_provider("feta", &spec, None, &[entry]).is_ok(), required != 2 || enforcing);
    }

    // Both host checkout forms require Shared even when a policy omits it.
    #[hegel::test]
    fn host_checkouts_cannot_weaken_shared_requirement(tc: hegel::TestCase) {
        let clone = tc.draw(gs::booleans());
        let mut spec = policy(EnvironmentClass::MicroVm);
        spec.docker_per_vessel.as_mut().unwrap().checkout = if clone {
            DockerCheckoutStrategy::LocalCloneOnHostAndMount { mount_path: "/workspace".into() }
        } else {
            DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".into() }
        };
        let mut entry = provider(EnvironmentClass::MicroVm);
        entry.capabilities.material_modes.retain(|mode| *mode != MaterialMode::Shared);
        assert!(match_provider("feta", &spec, None, &[entry]).unwrap_err().contains("Shared material"));
    }

    // Incomplete identities and conflicting selectors cannot silently select a
    // provider. One example per malformed declaration is sufficient glue coverage.
    #[test]
    fn malformed_and_conflicting_selectors_are_refused() {
        let mut spec = policy(EnvironmentClass::Container);
        let mut entry = provider(EnvironmentClass::Container);
        entry.kind = None;
        assert!(match_provider("feta", &spec, None, &[entry]).is_err());
        let entry = provider(EnvironmentClass::MicroVm);
        assert!(match_provider("feta", &spec, None, &[entry]).unwrap_err().contains("no advertised provider"));
        let entry = provider(EnvironmentClass::Container);
        spec.provider_requirements.as_mut().unwrap().instance = Some("absent".into());
        assert!(match_provider("feta", &spec, None, std::slice::from_ref(&entry)).unwrap_err().contains("absent"));
        spec.provider_requirements.as_mut().unwrap().instance = Some("sandbox".into());
        assert!(match_provider("feta", &spec, Some("different"), std::slice::from_ref(&entry)).unwrap_err().contains("disagrees"));
        spec.provider_requirements = Some(ProviderRequirements::default());
        assert!(match_provider("feta", &spec, None, &[entry]).unwrap_err().contains("instance or class"));
    }
    // A configured identity label can complete an otherwise empty selector;
    // combining a label and class is conjunctive, including every provider class.
    #[hegel::test]
    fn labelled_instance_combines_with_class_requirements(tc: hegel::TestCase) {
        let classes = [
            EnvironmentClass::HostDirect,
            EnvironmentClass::Container,
            EnvironmentClass::MicroVm,
            EnvironmentClass::CloudSandbox,
            EnvironmentClass::Dummy,
        ];
        let class = classes[tc.draw(gs::integers::<usize>().min_value(0).max_value(4))];
        let with_class = tc.draw(gs::booleans());
        let mut spec = policy(class);
        if !with_class {
            spec.provider_requirements = Some(ProviderRequirements::default());
        }
        let entry = provider(class);
        let selected = match_provider("feta", &spec, Some("sandbox"), std::slice::from_ref(&entry)).unwrap();
        assert_eq!(selected.instance, "sandbox");
        assert!(match_provider("feta", &spec, Some("absent"), &[entry]).is_err());
    }

    // Input shapes distinguish host adoption from per-vessel provisioning.
    // Capability matching cannot make a mismatched input shape meaningful.
    #[hegel::test]
    fn class_and_execution_strategy_disagreement_is_explicit(tc: hegel::TestCase) {
        let direct_strategy = tc.draw(gs::booleans());
        let mut spec = policy(if direct_strategy { EnvironmentClass::HostDirect } else { EnvironmentClass::Container });
        let wrong_class = if direct_strategy { EnvironmentClass::Container } else { EnvironmentClass::HostDirect };
        spec.provider_requirements.as_mut().unwrap().class = Some(wrong_class);
        let error = match_provider("feta", &spec, None, &[provider(wrong_class)]).unwrap_err();
        assert!(error.contains("incompatible with"), "{error}");
        assert!(error.contains("feta"), "{error}");
        // The same mismatch is refused when class comes only from the named instance.
        spec.provider_requirements = Some(ProviderRequirements { instance: Some("sandbox".into()), ..Default::default() });
        assert!(match_provider("feta", &spec, None, &[provider(wrong_class)]).is_err());
    }

    // Duplicate names count over the entire advertisement slice, even when
    // one duplicate has a different class and fails the policy's class filter.
    #[hegel::test]
    fn duplicate_identity_across_classes_is_refused(tc: hegel::TestCase) {
        let mut providers = vec![provider(EnvironmentClass::Container), provider(EnvironmentClass::CloudSandbox)];
        if tc.draw(gs::booleans()) {
            providers.reverse();
        }
        let error = match_provider("feta", &policy(EnvironmentClass::Container), None, &providers).unwrap_err();
        assert!(error.contains("ambiguous provider instance `sandbox` advertised more than once"));
    }

    // Repeated Host observations must control the next admission, including
    // malformed/old data. Two independently resolved vessels freeze distinct
    // identities; a selected instance is never borrowed from another vessel.
    #[hegel::test]
    fn host_observations_drive_each_vessels_frozen_selection(tc: hegel::TestCase) {
        use flotilla_resources::{Host, HostSpec, HostStatus, InputMeta, PlacementPolicy, ENVIRONMENT_PROVIDERS_CAPABILITY};
        use flotilla_store::ResourceBackend;
        use flotilla_store_memory::InMemoryBackend;
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
        let observations: Vec<_> = (0..steps).map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(4))).collect();
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let hosts = backend.using::<Host>("flotilla");
            let mut host = hosts
                .create(&InputMeta::builder().name("feta".into()).build(), &HostSpec { display_name: "feta".into(), ..Default::default() })
                .await
                .unwrap();
            let mut spec = policy(EnvironmentClass::CloudSandbox);
            spec.provider_requirements.as_mut().unwrap().instance = Some("sandbox".into());
            let original = backend
                .using::<PlacementPolicy>("flotilla")
                .create(&InputMeta::builder().name("cloud".into()).build(), &spec)
                .await
                .unwrap();
            for observation in observations {
                let mut entry = provider(EnvironmentClass::CloudSandbox);
                if observation == 1 {
                    entry.readiness = ProviderReadiness::MissingCredential { reason: "account token absent".into() };
                }
                let mut advertisements = vec![entry.clone()];
                let value = match observation {
                    2 => {
                        advertisements.push(entry);
                        serde_json::to_value(advertisements).unwrap()
                    }
                    3 => serde_json::json!({"invalid": true}),
                    4 => {
                        let mut value = serde_json::to_value(advertisements).unwrap();
                        value[0].as_object_mut().unwrap().remove("readiness");
                        value
                    }
                    _ => serde_json::to_value(advertisements).unwrap(),
                };
                host = hosts
                    .update_status(
                        "feta",
                        &host.metadata.resource_version,
                        &HostStatus {
                            capabilities: [(ENVIRONMENT_PROVIDERS_CAPABILITY.into(), value)].into_iter().collect(),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                let mut vessel_policy = original.clone();
                let result = crate::support::resolve_placement_provider(&backend, "flotilla", &mut vessel_policy).await;
                assert_eq!(result.is_ok(), observation == 0);
                if let Err(reason) = result {
                    assert!(reason.contains("feta"));
                } else {
                    assert_eq!(vessel_policy.spec.provider_requirements.unwrap().instance.as_deref(), Some("sandbox"));
                    assert_eq!(
                        vessel_policy
                            .metadata
                            .labels
                            .get(flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL)
                            .map(String::as_str),
                        Some("sandbox")
                    );
                }
            }
            let mut other = provider(EnvironmentClass::CloudSandbox);
            other.instance = "other".into();
            hosts
                .update_status(
                    "feta",
                    &host.metadata.resource_version,
                    &HostStatus {
                        capabilities: [(
                            ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                            serde_json::json!([provider(EnvironmentClass::CloudSandbox), other]),
                        )]
                        .into_iter()
                        .collect(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let mut first = original.clone();
            let mut second = original.clone();
            second.spec.provider_requirements.as_mut().unwrap().instance = Some("other".into());
            crate::support::resolve_placement_provider(&backend, "flotilla", &mut first).await.unwrap();
            crate::support::resolve_placement_provider(&backend, "flotilla", &mut second).await.unwrap();
            assert_ne!(first.spec.provider_requirements, second.spec.provider_requirements);
        });
    }
}
