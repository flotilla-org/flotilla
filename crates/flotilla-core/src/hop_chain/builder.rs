use flotilla_protocol::HostName;

use super::{Arg, Hop, HopPlan};
use crate::attachable::{AttachableId, AttachableStoreApi};

pub struct HopPlanBuilder<'a> {
    local_host: &'a HostName,
}

impl<'a> HopPlanBuilder<'a> {
    pub fn new(local_host: &'a HostName) -> Self {
        Self { local_host }
    }

    /// Build a plan for attaching to a terminal via its AttachableId.
    /// Used by TerminalManager::attach_command() and future `flotilla attach`.
    pub fn build_for_attachable(&self, attachable_id: &AttachableId, store: &dyn AttachableStoreApi) -> Result<HopPlan, String> {
        let registry = store.registry();

        let attachable = registry.attachables.get(attachable_id).ok_or_else(|| format!("attachable not found: {attachable_id}"))?;

        let set = registry.sets.get(&attachable.set_id).ok_or_else(|| format!("attachable set not found: {}", attachable.set_id))?;

        let mut hops = Vec::new();

        if let Some(ref host) = set.host_affinity {
            if host != self.local_host {
                hops.push(Hop::RemoteToHost { host: host.clone() });
            }
        }

        if let Some(ref env_id) = set.environment_id {
            hops.push(Hop::EnterEnvironment { env_id: env_id.clone(), provider: "docker".to_string() });
        }

        hops.push(Hop::AttachTerminal { attachable_id: attachable_id.clone() });

        Ok(HopPlan(hops))
    }

    /// Build a plan for wrapping a prepared pane command for a remote workspace.
    /// Used by CreateWorkspaceFromPreparedTerminal.
    pub fn build_for_prepared_command(&self, target_host: &HostName, command: &[Arg]) -> HopPlan {
        let mut hops = Vec::new();
        if target_host != self.local_host {
            hops.push(Hop::RemoteToHost { host: target_host.clone() });
        }
        hops.push(Hop::RunCommand { command: command.to_vec() });
        HopPlan(hops)
    }
}
