//! Common declarations for injected contained test environments. Lifecycle state,
//! command boundaries and adapter-specific capabilities belong to each fixture.
pub use flotilla_environment::environment::{EnvironmentClass, EnvironmentInput, ProviderCapabilities, ProviderKind};

pub fn injected_kind() -> ProviderKind {
    ProviderKind::new(EnvironmentClass::Dummy, "injected")
}
pub fn injected_capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
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
        fn capabilities(&self) -> $crate::environment::ProviderCapabilities {
            $crate::environment::injected_capabilities()
        }
        fn backing_id(&self) -> &str {
            self.container_name().unwrap_or_else(|| self.id().as_str())
        }
    };
}
