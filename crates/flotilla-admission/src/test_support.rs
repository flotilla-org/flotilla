use crate::{
    convoy_admission::ConvoyStartTask,
    repository_inspection::{RepositoryInspection, RepositoryInspector},
};
use async_trait::async_trait;
use flotilla_resources::RepositorySpec;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};

// Shared boundary stand-in for repository inspection and asynchronous worker
// submission. Tests supply an inspector or retain queued tasks without a process.
#[derive(Default)]
pub(crate) struct TestProjectOperations {
    pub inspector: Option<Arc<dyn RepositoryInspector>>,
    pub identity_resolutions: AtomicUsize,
    pub available: AtomicBool,
    pub inspections: AtomicUsize,
    pub tasks: std::sync::Mutex<Vec<ConvoyStartTask>>,
}
#[async_trait]
impl crate::project_ops::ProjectOperations for TestProjectOperations {
    // Stand-in for Git inspection I/O; checkout persistence uses real stores.
    async fn inspect_adopted_checkout(
        &self,
        path: &Path,
        repository_url: Option<&str>,
        git_ref: Option<&str>,
    ) -> Result<RepositoryInspection, String> {
        self.inspections.fetch_add(1, Ordering::SeqCst);
        let url = repository_url.expect("explicit repository");
        Ok(RepositoryInspection {
            spec: RepositorySpec::remote(url)?,
            checkout: crate::repository_inspection::LocalCheckoutInspection {
                path: path.into(),
                host_ref: "host".into(),
                git_ref: git_ref.unwrap().into(),
                is_main: false,
            },
            transport_url: Some(url.into()),
            replaces_prior_repository: false,
        })
    }
    async fn project_add(&self, _: &str, _: Option<&str>, _: Option<&str>, _: Option<&str>) -> Result<String, String> {
        panic!("no project edits")
    }
    async fn project_refresh(&self, _: &str) -> Result<(usize, bool, Vec<String>, Vec<String>), String> {
        panic!("no project edits")
    }
    async fn project_register(&self, _: &str) -> Result<(String, usize), String> {
        panic!("no project edits")
    }
    async fn image_check(
        &self,
        _: &flotilla_protocol::image_check::ImageCheckRequest,
    ) -> Result<flotilla_protocol::image_check::ImageCheckReport, String> {
        panic!("no image checks")
    }
    async fn resolve_repository_remote(&self, remote: &str) -> Result<RepositorySpec, String> {
        RepositorySpec::remote(remote)
    }
    async fn roll_convoy_ensure(&self, _: &str, _: &str) -> Result<String, String> {
        panic!("no ensure roll")
    }
    fn spawn_convoy_start(&self, task: ConvoyStartTask) -> bool {
        if !self.available.load(Ordering::SeqCst) {
            return false;
        }
        self.tasks.lock().unwrap().push(task);
        true
    }

    fn remember_bootstrap_checkout(&self, _inspection: &RepositoryInspection) -> Result<(), String> {
        Ok(())
    }
    async fn repository_inspector(&self) -> Result<Arc<dyn RepositoryInspector>, String> {
        self.inspector.clone().ok_or_else(|| "unexpected repository_inspector".into())
    }
    async fn inspect_repository_path(&self, _path: &Path, _remote: Option<&str>) -> Result<RepositoryInspection, String> {
        Err("unexpected inspect_repository_path".into())
    }
    async fn resolve_forge_identity_in(&self, _namespace: &str, spec: RepositorySpec) -> Result<RepositorySpec, String> {
        self.identity_resolutions.fetch_add(1, Ordering::SeqCst);
        Ok(spec)
    }
    async fn reap_ensured_convoy(&self, _namespace: &str, _ensure_name: &str, _convoy_name: &str, _force: bool) -> Result<(), String> {
        Err("unexpected reap_ensured_convoy".into())
    }
}

impl TestProjectOperations {
    pub fn with_inspector(inspector: Arc<dyn RepositoryInspector>) -> Self {
        Self { inspector: Some(inspector), ..Self::default() }
    }
}
