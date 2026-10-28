//! The realisation settings that are code constants (#3824 layer 4; the 2026-10-27 ruling). No
//! kind or record holds them: a Docker vessel's checkout, cwd, env and pull policy and a
//! host-direct vessel's checkout are the values every live policy carries. A cloud sandbox's
//! settings are dropped, and the Docker memory share becomes a Host capacity (layer 4c).
//!
//! Until admission and realisation read them (layer 4b-iii), the placement import compares each
//! policy's settings with them, so a host relying on other values is named before the roll.

use crate::{DockerCheckoutStrategy, DockerImagePullPolicy, HostDirectPlacementPolicyCheckout, PlacementPolicySpec};

/// Where a Docker vessel's checkout is mounted, and its working directory.
pub const DOCKER_WORKSPACE: &str = "/workspace";

/// A Docker vessel's image pull policy.
pub const DOCKER_PULL_POLICY: DockerImagePullPolicy = DockerImagePullPolicy::Always;

/// A Docker vessel's checkout: a worktree on its host, mounted at [`DOCKER_WORKSPACE`].
pub fn docker_checkout() -> DockerCheckoutStrategy {
    DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: DOCKER_WORKSPACE.to_owned() }
}

/// A host-direct vessel's checkout: a worktree in place.
pub fn host_direct_checkout() -> HostDirectPlacementPolicyCheckout {
    HostDirectPlacementPolicyCheckout::Worktree
}

/// The settings of `spec` that differ from the constants, by their names in a policy. A Docker
/// policy's memory share is not compared: it becomes its host's capacity, not a constant.
pub fn differing_settings(spec: &PlacementPolicySpec) -> Vec<&'static str> {
    let mut differing = Vec::new();
    if let Some(docker) = &spec.docker_per_vessel {
        if docker.checkout != docker_checkout() {
            differing.push("checkout");
        }
        // An unset cwd is not the constant: a vessel with no repositories would start in `/`.
        if docker.default_cwd.as_deref() != Some(DOCKER_WORKSPACE) {
            differing.push("default_cwd");
        }
        if !docker.env.is_empty() {
            differing.push("env");
        }
        if docker.pull_policy != DOCKER_PULL_POLICY {
            differing.push("pull_policy");
        }
    }
    if spec.host_direct.as_ref().is_some_and(|host| host.checkout != host_direct_checkout()) {
        differing.push("checkout");
    }
    differing
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour (the 2026-10-27 ruling): the shipped crew-image policies carry exactly the
    // constants, so the settings they declare need no carrier after the move.
    #[test]
    fn the_shipped_crew_image_settings_are_the_constants() {
        let policy: PlacementPolicySpec = serde_json::from_value(serde_json::json!({
            "pool": "cleat",
            "docker_per_vessel": {
                "host_ref": "h",
                "memory_policy": {"host_memory_percent": 50, "expected_concurrent_crews": 4, "swap_bytes": 0},
                "pull_policy": "always",
                "default_cwd": "/workspace",
                "env": {},
                "checkout": {"worktree_on_host_and_mount": {"mount_path": "/workspace"}}
            }
        }))
        .expect("policy");
        assert_eq!(differing_settings(&policy), Vec::<&str>::new());
        let mut other = policy.clone();
        let docker = other.docker_per_vessel.as_mut().expect("docker");
        docker.pull_policy = DockerImagePullPolicy::Never;
        docker.env.insert("A".into(), "b".into());
        assert_eq!(differing_settings(&other), ["env", "pull_policy"]);
        let mut other = policy.clone();
        let docker = other.docker_per_vessel.as_mut().expect("docker");
        docker.checkout = DockerCheckoutStrategy::FreshCloneInContainer { clone_path: DOCKER_WORKSPACE.into() };
        docker.default_cwd = None;
        assert_eq!(differing_settings(&other), ["checkout", "default_cwd"]);
    }
}
