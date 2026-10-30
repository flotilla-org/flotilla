//! Common declarations for injected contained test environments. Lifecycle state,
//! command boundaries and adapter-specific capabilities belong to each fixture.
pub use flotilla_environment::environment::{
    EnvironmentClass, EnvironmentInput, ProviderCapabilities, ProviderKind, ProviderReadiness, VesselAgent,
};

pub fn injected_kind() -> ProviderKind {
    ProviderKind::new(EnvironmentClass::Dummy, "injected")
}
pub fn injected_capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        material_modes: vec![
            flotilla_environment::environment::MaterialMode::Frozen,
            flotilla_environment::environment::MaterialMode::Pushed,
            flotilla_environment::environment::MaterialMode::Shared,
        ],
        checkout_strategies: vec![
            flotilla_environment::environment::CheckoutStrategy::HostWorktree,
            flotilla_environment::environment::CheckoutStrategy::HostClone,
            flotilla_environment::environment::CheckoutStrategy::CloneInVessel,
        ],
        workspace_root: "/workspace".into(),
        reference_root: Some("/ref/repo".into()),
        receives_host_tools: true,
        per_vessel: true,
        ..Default::default()
    }
}

/// Shared sync declaration methods inside an EnvironmentProvider test impl.
#[macro_export]
macro_rules! injected_environment_provider {
    () => {
        fn detect_readiness<'life0, 'async_trait>(
            &'life0 self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = $crate::environment::ProviderReadiness> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async { $crate::environment::ProviderReadiness::Ready })
        }
        fn kind(&self) -> $crate::environment::ProviderKind {
            $crate::environment::injected_kind()
        }
        fn capabilities(&self) -> $crate::environment::ProviderCapabilities {
            $crate::environment::injected_capabilities()
        }
        fn legacy_input(&self) -> Option<$crate::environment::EnvironmentInput> {
            Some($crate::environment::EnvironmentInput::Docker)
        }
    };
}

/// Common identity/root projection for preceding contained handle fixtures.
#[macro_export]
macro_rules! injected_environment_handle {
    () => {
        $crate::injected_environment_handle!(agent);
        /// The injected runner stands for the vessel, its agent included.
        fn agent(&self) -> std::sync::Arc<dyn $crate::environment::VesselAgent> {
            $crate::environment::runner_agent(self.runner())
        }
    };
    // For a handle that supplies its own agent.
    (agent) => {
        fn capabilities(&self) -> $crate::environment::ProviderCapabilities {
            $crate::environment::injected_capabilities()
        }
        fn backing_id(&self) -> &str {
            self.container_name().unwrap_or_else(|| self.id().as_str())
        }
        fn provider_descriptor(&self) -> Option<flotilla_resources::EnvironmentProviderDescriptor> {
            None
        }
    };
}

/// The in-process agent over an injected runner.
pub fn runner_agent(runner: std::sync::Arc<dyn flotilla_provider_runtime::CommandRunner>) -> std::sync::Arc<dyn VesselAgent> {
    std::sync::Arc::new(flotilla_environment::environment::vessel_ops::RunnerVesselAgent(runner))
}

/// Declared provider facts for in-memory admission fixtures. No process probes
/// run: these fixtures model a ready host-direct and contained endpoint.
pub fn ready_placement_advertisements() -> Vec<flotilla_environment::environment::ProviderAdvertisement> {
    [EnvironmentClass::HostDirect, EnvironmentClass::Container]
        .into_iter()
        .enumerate()
        .map(|(index, class)| flotilla_environment::environment::ProviderAdvertisement {
            instance: format!("fixture-{index}"),
            kind: Some(ProviderKind::new(class, "fixture")),
            capabilities: injected_capabilities(),
            readiness: ProviderReadiness::Ready,
        })
        .collect()
}
