use crate::{CheckoutSpec, InputMeta, TerminalSessionSpec, VesselSpec};
#[derive(Debug, Clone)]
pub enum Actuation {
    CreateRepository { key: crate::RepositoryKey, spec: crate::RepositorySpec },
    CreateCheckout { meta: InputMeta, spec: CheckoutSpec },
    // Boxed: the spec dwarfs every other variant.
    CreateTerminalSession { meta: InputMeta, spec: Box<TerminalSessionSpec> },
    RaiseAttention { attention: crate::attentions::Attention },
    ClearAttention { sender: crate::attentions::Sender, subject: crate::attentions::Subject },
    RestartTerminalSession { name: String, replaces: Option<String> },
    PruneTerminalMessages { name: String },
    DeleteTerminalSession { name: String },
    CreateVessel { meta: InputMeta, spec: VesselSpec },
    DeleteVessel { name: String },
    DeleteCheckout { name: String },
}
