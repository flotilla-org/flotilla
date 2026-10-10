use chrono::{DateTime, Utc};
use flotilla_resources::{
    legacy_message_spec, Convoy, InputMeta, ResourceError, ResourceObject, TerminalCrewMessage, TerminalSession, CONVOY_LABEL, VESSEL_LABEL,
};
use flotilla_store::{MessageInbox, TypedResolver};

#[async_trait::async_trait]
pub trait LegacyMessageFixture {
    async fn accept_crew_message(
        &self,
        terminal: &ResourceObject<TerminalSession>,
        message: &TerminalCrewMessage,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError>;
}
#[async_trait::async_trait]
impl LegacyMessageFixture for TypedResolver<TerminalSession> {
    /// Test-only fixture entry point for previous-generation sender envelopes.
    /// Production producers construct MessageSpec directly.
    /// Remove after the first fleet roll deploying #2710.
    async fn accept_crew_message(
        &self,
        terminal: &ResourceObject<TerminalSession>,
        message: &TerminalCrewMessage,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let (backend, namespace) = self.context();
        let convoy_name =
            terminal.metadata.labels.get(CONVOY_LABEL).ok_or_else(|| ResourceError::invalid("crew message requires convoy context"))?;
        let vessel =
            terminal.metadata.labels.get(VESSEL_LABEL).ok_or_else(|| ResourceError::invalid("crew message requires vessel context"))?;
        let convoy = backend.including_replicas::<Convoy>(namespace).get(convoy_name).await?;
        let project = convoy.object.spec.project_ref.as_deref().unwrap_or(namespace);
        let receiver = format!("{project}/{convoy_name}/{vessel}/{}", terminal.spec.role);
        let spec = legacy_message_spec(&receiver, message);
        let name = flotilla_resources::message_record_name(&receiver, &spec.sender, &message.id);
        MessageInbox::new(backend.clone(), namespace).accept(&InputMeta::builder().name(name).build(), &spec, now).await?;
        Ok(())
    }
}
