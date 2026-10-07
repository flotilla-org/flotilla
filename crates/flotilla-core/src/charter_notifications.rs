//! Render changed charters through the same role-template seam as first turns.
use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use flotilla_resources::{
    CharterBriefInput, CharterBriefRenderer, CharterProseRenderer, CrewSource, ResourceBackend, ResourceError, TerminalSessionSource,
    VESSEL_LABEL,
};

use crate::{
    agent_adapter::{
        append_convoy_work_context, build_convoy_crew_brief_with_options, CrewAssignment, CrewBriefMember, CrewBriefTemplateResolver,
    },
    config::ConfigStore,
};

pub(crate) struct LiveCharterBriefRenderer {
    backend: ResourceBackend,
    config: Arc<ConfigStore>,
}

impl LiveCharterBriefRenderer {
    pub(crate) fn new(backend: ResourceBackend, config: Arc<ConfigStore>) -> Self {
        Self { backend, config }
    }
}

#[async_trait]
impl CharterBriefRenderer for LiveCharterBriefRenderer {
    async fn render(&self, input: CharterBriefInput<'_>) -> Result<String, ResourceError> {
        let Some(convoy) = input.convoy else { return CharterProseRenderer.render(input).await };
        let TerminalSessionSource::Agent { context, .. } = &input.holder.spec.source else {
            return Err(ResourceError::invalid("charter brief requires an agent holder"));
        };
        let vessel = input.holder.metadata.labels.get(VESSEL_LABEL).ok_or_else(|| ResourceError::invalid("holder has no vessel"))?;
        let requirement = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|workflow| workflow.vessels.iter().find(|requirement| requirement.name == *vessel));
        let process = requirement.and_then(|requirement| requirement.crew.iter().find(|process| process.role == input.role));
        let (prompt, template) = match process.map(|process| &process.source) {
            Some(CrewSource::Agent { prompt, brief_template, .. }) => (prompt.as_deref(), brief_template.as_deref()),
            _ => (None, None),
        };
        let mut cascade = input.cascade.clone();
        // The notification's local content and revision come from the same read.
        cascade.charter = input.project.spec.charter_prose.clone();
        cascade.charter_commit = Some(input.revision.to_string());
        let mut options = CrewBriefTemplateResolver::with_config_dir(self.config.base_path().as_path()).render_options(
            template,
            Some(&input.project.metadata.name),
            vec![PathBuf::from(&input.holder.spec.cwd)],
        );
        options.apply_cascade(Some(&cascade), input.role);
        options.has_credential_scope = requirement.is_some_and(|requirement| !requirement.credential_scopes.is_empty());
        let members = requirement
            .into_iter()
            .flat_map(|requirement| requirement.crew.iter())
            .map(|process| CrewBriefMember {
                role: process.role.clone(),
                state: "active".into(),
                is_agent: matches!(process.source, CrewSource::Agent { .. }),
            })
            .collect::<Vec<_>>();
        let assignment = match prompt {
            Some(prompt) => CrewAssignment::Prompt(prompt),
            None if !convoy.spec.issues.is_empty() => CrewAssignment::CarriedIssue,
            None if convoy.spec.change_request.is_some() => CrewAssignment::CarriedChangeRequest,
            None => CrewAssignment::Unassigned,
        };
        let mut brief = build_convoy_crew_brief_with_options(convoy, context, vessel, input.role, assignment, &members, &options)
            .map_err(ResourceError::invalid)?;
        let repositories = requirement
            .and_then(|requirement| requirement.repository_refs.clone())
            .unwrap_or_else(|| convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect());
        let empty_scopes = Default::default();
        let scopes = requirement.map(|requirement| &requirement.credential_scopes).unwrap_or(&empty_scopes);
        append_convoy_work_context(&mut brief.content, convoy, &repositories, scopes);
        let address = format!("{}/{}/{}/{}", input.project.metadata.name, convoy.metadata.name, vessel, input.role);
        let book = flotilla_resources::crew_address_book(&self.backend, &context.namespace, &address).await?;
        brief.content.push('\n');
        brief.content.push_str(&book.render());
        Ok(brief.content)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use flotilla_resources::{
        reconcile_charter_notifications_with_renderer, Convoy, ConvoySpec, InMemoryBackend, InputMeta, Message, MessageInbox, Project,
        ProjectSpec, RoleDefinition, Selector, TerminalBrief, TerminalCrewContext, TerminalSession, TerminalSessionSpec, CONVOY_LABEL,
        ROLE_LABEL,
    };

    use super::*;

    // Changing a charter re-renders its live role template, coalesces pending
    // revisions, and keeps the initial holder's brief and session untouched.
    #[tokio::test]
    async fn latest_charter_message_renders_the_live_role_template() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let config_dir = tempfile::tempdir().expect("config");
        let renderer = LiveCharterBriefRenderer::new(backend.clone(), Arc::new(ConfigStore::with_base(config_dir.path())));
        let projects = backend.definitions::<Project>("flotilla");
        projects
            .create(&InputMeta::builder().name("root".into()).build(), &ProjectSpec::builder().display_name("root".into()).build())
            .await
            .expect("project");
        backend
            .using::<Convoy>("flotilla")
            .create(
                &InputMeta::builder().name("guide".into()).build(),
                &ConvoySpec::builder().workflow_ref("standing".into()).project_ref("root".into()).role("guide".into()).build(),
            )
            .await
            .expect("convoy");
        let holder_spec = TerminalSessionSpec::builder()
            .env_ref("env".into())
            .role("guide".into())
            .cwd("/workspace".into())
            .pool("cleat".into())
            .source(TerminalSessionSource::Agent {
                selector: Selector::for_capability("coding"),
                brief: TerminalBrief {
                    path: "brief.md".into(),
                    content: "frozen first turn".into(),
                    artifact_digest: None,
                    copies: Vec::new(),
                },
                context: Box::new(TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "guide".into(),
                    vessel_ref: "guide-work".into(),
                }),
                message: None,
            })
            .build();
        backend
            .using::<TerminalSession>("flotilla")
            .create(
                &InputMeta::builder()
                    .name("guide-terminal".into())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), "guide".into()),
                        (VESSEL_LABEL.into(), "work".into()),
                        (ROLE_LABEL.into(), "guide".into()),
                    ]))
                    .build(),
                &holder_spec,
            )
            .await
            .expect("holder");
        let inbox = MessageInbox::new(backend.clone(), "flotilla");
        for (revision, template) in [("first", "Old {{ role }}"), ("second", "Newest {{ role }}")] {
            let project = projects.get("root").await.expect("project");
            let mut meta = InputMeta::from(&project.metadata);
            meta.annotations.insert("flotilla.work/charter-revision".into(), revision.into());
            let mut spec = project.spec;
            spec.role_definitions.insert("guide".into(), RoleDefinition { brief_template: Some(template.into()), ..Default::default() });
            projects.apply(&meta, &spec).await.expect("applied revision");
            reconcile_charter_notifications_with_renderer(&inbox, &renderer, Utc::now()).await.expect("render notification");
        }
        let messages = backend.using::<Message>("flotilla").list().await.expect("messages");
        let pending: Vec<_> =
            messages.items.iter().filter(|message| !message.status.as_ref().expect("status").phase.is_terminal()).collect();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].spec.body.contains("Newest guide"));
        assert!(pending[0].spec.body.contains("charter@second"));
        assert!(!pending[0].spec.body.contains("Old guide"));
        assert_eq!(backend.using::<TerminalSession>("flotilla").get("guide-terminal").await.expect("holder").spec, holder_spec);
    }
}
