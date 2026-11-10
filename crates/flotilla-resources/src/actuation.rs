#[derive(Debug, Clone)]
pub enum Actuation {
    CreateRepository { key: crate::RepositoryKey, spec: crate::RepositorySpec },
    RaiseAttention { attention: crate::attentions::Attention },
    ClearAttention { sender: crate::attentions::Sender, subject: crate::attentions::Subject },
}
