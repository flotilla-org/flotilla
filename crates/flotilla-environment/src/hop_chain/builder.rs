use flotilla_protocol::HostName;

use super::{Arg, Hop, HopPlan};

pub struct HopPlanBuilder<'a> {
    local_host: &'a HostName,
}

impl<'a> HopPlanBuilder<'a> {
    pub fn new(local_host: &'a HostName) -> Self {
        Self { local_host }
    }

    /// Wrap a TerminalSession command for execution on its target host.
    pub fn build_for_prepared_command(&self, target_host: &HostName, command: &[Arg]) -> HopPlan {
        let mut hops = Vec::new();
        if target_host != self.local_host {
            hops.push(Hop::RemoteToHost { host: target_host.clone() });
        }
        hops.push(Hop::RunCommand { command: command.to_vec() });
        HopPlan(hops)
    }
}
