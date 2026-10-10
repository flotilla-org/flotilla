//! Operation-scoped registry credentials. Material and file ownership stay
//! behind the credential-store port; provider interfaces never carry directories.
use std::{
    any::Any,
    fmt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use flotilla_resources::HostImageAction;

pub trait RegistryAuthMaterial: Send + Sync {
    fn config(&self) -> Result<RegistryConfig, String>;
    fn redact(&self, text: String) -> String {
        text
    }
    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder;
}

/// A private, operation-owned tool configuration. Dropping its owner cleans up,
/// including when an async operation is cancelled.
pub struct RegistryConfig {
    path: PathBuf,
    _owner: Box<dyn Any + Send + Sync>,
}
struct AnonymousConfig(PathBuf);
impl Drop for AnonymousConfig {
    fn drop(&mut self) {
        // Only registry/Buildx operations own these paths. Keep cleanup in Drop
        // so cancellation cannot leave state for a detached async cleanup task.
        // Buildx may create non-secret state beneath this private empty config.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
impl RegistryConfig {
    pub fn new(path: PathBuf, owner: impl Any + Send + Sync) -> Self {
        Self { path, _owner: Box::new(owner) }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Clone)]
pub struct RegistryAuth {
    repository: String,
    action: HostImageAction,
    material: Arc<dyn RegistryAuthMaterial>,
    used: Arc<AtomicBool>,
}
impl fmt::Debug for RegistryAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistryAuth").finish_non_exhaustive()
    }
}
impl RegistryAuth {
    /// The credential store supplies material after grant admission.
    pub fn new(repository: String, action: HostImageAction, material: Arc<dyn RegistryAuthMaterial>) -> Self {
        Self { repository, action, material, used: Arc::new(AtomicBool::new(false)) }
    }
    pub fn validate(&self, reference: &str, action: HostImageAction) -> Result<(), String> {
        if self.action != action
            || !(reference == self.repository
                || reference.strip_prefix(&self.repository).is_some_and(|s| s.starts_with(':') || s.starts_with('@')))
        {
            return Err("registry auth does not admit this repository/action".into());
        }
        Ok(())
    }
    pub fn redact(auth: Option<&Self>, text: String) -> String {
        match auth {
            Some(auth) => auth.material.redact(text),
            None => text,
        }
    }
    /// A claim is irreversible, even if subsequent validation, lowering or I/O
    /// fails. This is fail-closed: retries need a newly admitted handle.
    pub fn claim(&self) -> Result<(), String> {
        self.used
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| "registry auth already used for an operation".into())
    }
    pub fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        self.material.authorize(request)
    }
    pub fn config(auth: Option<&Self>) -> Result<RegistryConfig, String> {
        match auth {
            Some(auth) => {
                auth.claim()?;
                auth.material.config()
            }
            // No file is created and no ambient Docker configuration is read.
            // Authenticated files have exactly one construction site in CredentialStore.
            None => {
                let path = std::env::temp_dir().join(format!("flotilla-anonymous-{}", uuid::Uuid::new_v4()));
                // Establish privacy before a tool can create non-secret state.
                // Credential files still have only the CredentialStore helper.
                #[cfg(unix)]
                let created = {
                    use std::os::unix::fs::DirBuilderExt;
                    std::fs::DirBuilder::new().mode(0o700).create(&path)
                };
                #[cfg(not(unix))]
                let created = std::fs::create_dir(&path);
                created.map_err(|error| error.to_string())?;
                Ok(RegistryConfig::new(path.clone(), AnonymousConfig(path)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::generators as gs;

    // Boundary double: these claims must not read HTTP or disk credential material.
    struct UnreadMaterial;
    impl RegistryAuthMaterial for UnreadMaterial {
        fn config(&self) -> Result<RegistryConfig, String> {
            panic!("claim must not write files")
        }
        fn authorize(&self, _: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
            panic!("claim must not contact a service")
        }
    }

    // Anonymous tool state starts empty and private before the subprocess can
    // write it, and its owned guard removes even tool-created files on drop.
    #[cfg(unix)]
    #[test]
    fn anonymous_config_owns_private_empty_directory() {
        use std::os::unix::fs::PermissionsExt;
        let config = RegistryAuth::config(None).expect("anonymous config");
        let path = config.path().to_path_buf();
        assert_eq!(std::fs::metadata(&path).expect("created directory").permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read_dir(&path).expect("directory").count(), 0);
        std::fs::write(path.join("tool-state"), "non-secret").expect("tool state");
        drop(config);
        assert!(!path.exists(), "owned state is removed");
    }

    // Every clone shares a single operation claim, including concurrent attempts.
    // Generator covers 1..8 contenders and an optional preceding successful claim.
    #[hegel::test]
    fn concurrent_clones_admit_at_most_one_operation(tc: hegel::TestCase) {
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
        let already_used = tc.draw(gs::booleans());
        let auth = RegistryAuth::new("registry.example/image".into(), HostImageAction::ImagePull, Arc::new(UnreadMaterial));
        if already_used {
            auth.claim().expect("preceding operation");
        }
        let admitted = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..count)
                .map(|_| {
                    let auth = auth.clone();
                    scope.spawn(move || auth.claim().is_ok())
                })
                .collect();
            handles.into_iter().map(|h| usize::from(h.join().expect("contender"))).sum::<usize>()
        });
        assert_eq!(admitted, usize::from(!already_used));
    }
}
