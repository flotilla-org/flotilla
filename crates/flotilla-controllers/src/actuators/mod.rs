use flotilla_environment::environment::{ProvisionedMount, ProvisionedMountMode};
use flotilla_resources::{EnvironmentMount, EnvironmentMountMode};

pub fn provisioned_mount(mount: &EnvironmentMount) -> ProvisionedMount {
    let mode = match mount.mode {
        EnvironmentMountMode::Ro => ProvisionedMountMode::Ro,
        EnvironmentMountMode::Rw => ProvisionedMountMode::Rw,
    };
    ProvisionedMount::new(&mount.source_path, &mount.target_path, mode)
}
