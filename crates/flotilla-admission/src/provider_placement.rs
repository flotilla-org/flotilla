//! Provider matching lives with the realisations it matches, in `flotilla_resources`.
pub use flotilla_resources::provider_placement::match_provider;

#[cfg(test)]
mod tests {
    use flotilla_protocol::provider::{CheckoutStrategy, EnvironmentClass, MaterialMode, ProviderAdvertisement, ProviderReadiness};
    use flotilla_protocol::provider::{EgressEnforcement, ProviderCapabilities, ProviderKind};
    use flotilla_resources::provider_placement::{checkout_for, match_provider};
    use flotilla_resources::FulfilmentRealisation;
    use flotilla_resources::{FulfilmentKind, FulfilmentKindSpec};
    use hegel::generators as gs;

    const CLASSES: [EnvironmentClass; 5] = [
        EnvironmentClass::HostDirect,
        EnvironmentClass::Container,
        EnvironmentClass::MicroVm,
        EnvironmentClass::CloudSandbox,
        EnvironmentClass::Dummy,
    ];

    fn realisation(class: EnvironmentClass) -> FulfilmentRealisation {
        match class {
            EnvironmentClass::HostDirect => FulfilmentRealisation::HostDirect,
            EnvironmentClass::CloudSandbox => {
                FulfilmentRealisation::CloudSandboxPerVessel { image: "image@sha256:digest".into(), size: None }
            }
            _ => FulfilmentRealisation::DockerPerVessel { image: None },
        }
    }

    fn provider(class: EnvironmentClass) -> ProviderAdvertisement {
        ProviderAdvertisement {
            instance: "sandbox".into(),
            kind: Some(ProviderKind::new(class, "open-implementation-name")),
            readiness: ProviderReadiness::Ready,
            capabilities: ProviderCapabilities {
                workspace_root: "/workspace".into(),
                material_modes: vec![MaterialMode::Frozen, MaterialMode::Pushed, MaterialMode::Shared],
                checkout_strategies: vec![CheckoutStrategy::HostWorktree, CheckoutStrategy::HostClone, CheckoutStrategy::CloneInVessel],
                egress_enforcement: EgressEnforcement::Enforced,
                ..Default::default()
            },
        }
    }

    // #3765: cloud admission requires exactly one controller, validates the
    // guest's POSIX root, and freezes that root without a metadata label.
    #[hegel::test]
    fn cloud_controller_selection_is_unique_and_root_is_absolute(tc: hegel::TestCase) {
        use flotilla_resources::{Host, HostSpec, HostStatus, InputMeta, ENVIRONMENT_PROVIDERS_CAPABILITY};
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let absolute = tc.draw(gs::booleans());
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let backend = flotilla_store::ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
            for i in 0..count {
                let hosts = backend.using::<Host>("flotilla");
                let host = hosts
                    .create(
                        &InputMeta::builder().name(format!("controller-{i}")).build(),
                        &HostSpec { display_name: format!("controller-{i}"), ..Default::default() },
                    )
                    .await
                    .unwrap();
                let mut declaration = provider(EnvironmentClass::CloudSandbox);
                declaration.capabilities.workspace_root = if absolute { "/guest/work" } else { "relative/work" }.into();
                hosts
                    .update_status(
                        &host.metadata.name,
                        &host.metadata.resource_version,
                        &HostStatus {
                            capabilities: [(ENVIRONMENT_PROVIDERS_CAPABILITY.into(), serde_json::to_value(vec![declaration]).unwrap())]
                                .into_iter()
                                .collect(),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
            }
            let spec = FulfilmentKindSpec::builder()
                .host_ref(String::new())
                .pool("cloud".into())
                .realisation(realisation(EnvironmentClass::CloudSandbox))
                .build();
            let mut kind = backend
                .using::<FulfilmentKind>("flotilla")
                .create(&InputMeta::builder().name("cloud".into()).build(), &spec)
                .await
                .unwrap();
            let result = crate::support::resolve_placement_provider(&backend, "flotilla", &mut kind).await;
            assert_eq!(result.is_ok(), count == 1 && absolute);
            if count == 1 && absolute {
                assert_eq!(result.unwrap().as_deref(), Some("/guest/work"));
            }
            assert!(!kind.metadata.labels.contains_key("flotilla.work/provider-workspace-root"));
        });
    }

    // ADR 0058: a frozen instance, or else the realisation's classes, selects
    // exactly one ready instance. The generated space spans every class, frozen
    // and class selection, empty inventories, duplicates and ambiguous instances.
    #[hegel::test]
    fn selects_exactly_one_matching_instance(tc: hegel::TestCase) {
        let class = CLASSES[tc.draw(gs::integers::<usize>().min_value(0).max_value(4))];
        let frozen = tc.draw(gs::booleans());
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let duplicate = tc.draw(gs::booleans());
        let providers: Vec<_> = (0..count)
            .map(|i| {
                let mut entry = provider(class);
                if i > 0 && !duplicate {
                    entry.instance = format!("other-{i}");
                }
                entry
            })
            .collect();
        let result = match_provider("feta", &realisation(class), frozen.then_some("sandbox"), &providers);
        let expected = count == 1 || (frozen && count > 1 && !duplicate);
        assert_eq!(result.is_ok(), expected);
        if let Ok(selected) = result {
            assert_eq!(selected.instance, "sandbox");
        }
    }

    // Each realisation's checkout, and the Shared material a host checkout
    // needs, fail with the host and the exact missing piece.
    #[hegel::test]
    fn missing_capabilities_have_owned_reasons(tc: hegel::TestCase) {
        let class = CLASSES[tc.draw(gs::integers::<usize>().min_value(0).max_value(4))];
        let shared = tc.draw(gs::booleans());
        let mut entry = provider(class);
        let checkout = checkout_for(&realisation(class));
        let expected = if shared && checkout == CheckoutStrategy::HostWorktree {
            entry.capabilities.material_modes.retain(|mode| *mode != MaterialMode::Shared);
            "Shared material".to_string()
        } else {
            entry.capabilities.checkout_strategies.retain(|mode| *mode != checkout);
            format!("{checkout:?} checkout")
        };
        let error = match_provider("feta", &realisation(class), None, &[entry]).unwrap_err();
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
        let cloud = realisation(EnvironmentClass::CloudSandbox);
        let result = match_provider("controller", &cloud, None, std::slice::from_ref(&entry)).map(|entry| entry.instance.clone());
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
        assert!(match_provider("controller", &cloud, None, &[old]).unwrap_err().contains("unknown"));
    }

    // Incomplete identities and absent instances cannot silently select a provider.
    #[test]
    fn malformed_and_absent_selectors_are_refused() {
        let docker = realisation(EnvironmentClass::Container);
        let mut entry = provider(EnvironmentClass::Container);
        entry.kind = None;
        assert!(match_provider("feta", &docker, None, std::slice::from_ref(&entry)).is_err());
        assert!(match_provider("feta", &docker, Some("sandbox"), &[entry]).unwrap_err().contains("incomplete identity"));
        let entry = provider(EnvironmentClass::HostDirect);
        assert!(match_provider("feta", &docker, None, std::slice::from_ref(&entry)).unwrap_err().contains("no advertised provider"));
        assert!(match_provider("feta", &docker, Some("absent"), &[entry]).unwrap_err().contains("absent"));
    }

    // A frozen instance whose class cannot realise the kind is refused, naming the host.
    #[hegel::test]
    fn class_and_realisation_disagreement_is_explicit(tc: hegel::TestCase) {
        let direct = tc.draw(gs::booleans());
        let kind = realisation(if direct { EnvironmentClass::HostDirect } else { EnvironmentClass::Container });
        let wrong = if direct { EnvironmentClass::Container } else { EnvironmentClass::HostDirect };
        let error = match_provider("feta", &kind, Some("sandbox"), &[provider(wrong)]).unwrap_err();
        assert!(error.contains("cannot realise"), "{error}");
        assert!(error.contains("feta"), "{error}");
    }

    // Duplicate names count over the entire advertisement slice, even when
    // one duplicate has a class the realisation does not admit.
    #[hegel::test]
    fn duplicate_identity_across_classes_is_refused(tc: hegel::TestCase) {
        let mut providers = vec![provider(EnvironmentClass::Container), provider(EnvironmentClass::CloudSandbox)];
        if tc.draw(gs::booleans()) {
            providers.reverse();
        }
        let error = match_provider("feta", &realisation(EnvironmentClass::Container), None, &providers).unwrap_err();
        assert!(error.contains("ambiguous provider instance `sandbox` advertised more than once"));
    }

    // Repeated Host observations control the next admission, including
    // malformed and old data; each resolution freezes the instance it chose.
    #[hegel::test]
    fn host_observations_drive_each_vessels_frozen_selection(tc: hegel::TestCase) {
        use flotilla_resources::{Host, HostSpec, HostStatus, InputMeta, ENVIRONMENT_PROVIDERS_CAPABILITY};
        use flotilla_store::ResourceBackend;
        use flotilla_store_memory::InMemoryBackend;
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
        let observations: Vec<_> = (0..steps).map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(4))).collect();
        let label = flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL;
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let hosts = backend.using::<Host>("flotilla");
            let mut host = hosts
                .create(&InputMeta::builder().name("feta".into()).build(), &HostSpec { display_name: "feta".into(), ..Default::default() })
                .await
                .unwrap();
            let spec = FulfilmentKindSpec::builder()
                .host_ref("feta".into())
                .pool("cleat".into())
                .realisation(realisation(EnvironmentClass::Container))
                .build();
            let original = backend
                .using::<FulfilmentKind>("flotilla")
                .create(&InputMeta::builder().name("docker".into()).build(), &spec)
                .await
                .unwrap();
            for observation in observations {
                let mut entry = provider(EnvironmentClass::Container);
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
                let mut vessel_kind = original.clone();
                let result = crate::support::resolve_placement_provider(&backend, "flotilla", &mut vessel_kind).await;
                assert_eq!(result.is_ok(), observation == 0);
                if let Err(reason) = result {
                    assert!(reason.contains("feta"));
                } else {
                    assert_eq!(vessel_kind.metadata.labels.get(label).map(String::as_str), Some("sandbox"));
                }
            }
            let mut other = provider(EnvironmentClass::Container);
            other.instance = "other".into();
            hosts
                .update_status(
                    "feta",
                    &host.metadata.resource_version,
                    &HostStatus {
                        capabilities: [(
                            ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                            serde_json::json!([provider(EnvironmentClass::Container), other]),
                        )]
                        .into_iter()
                        .collect(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let mut first = original.clone();
            first.metadata.labels.insert(label.into(), "sandbox".into());
            let mut second = original.clone();
            second.metadata.labels.insert(label.into(), "other".into());
            crate::support::resolve_placement_provider(&backend, "flotilla", &mut first).await.unwrap();
            crate::support::resolve_placement_provider(&backend, "flotilla", &mut second).await.unwrap();
            assert_ne!(first.metadata.labels.get(label), second.metadata.labels.get(label));
            let mut unfrozen = original.clone();
            assert!(crate::support::resolve_placement_provider(&backend, "flotilla", &mut unfrozen)
                .await
                .unwrap_err()
                .contains("ambiguous"));
        });
    }
}
