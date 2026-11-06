use std::collections::BTreeMap;

/// Indexed live inventory, including records whose initial status is not written yet.
/// Keep retained terminal bodies out of event-driven readers.
pub fn live_convoys_query() -> crate::StoreQuery {
    crate::StoreQuery {
        any_of: Some(vec![
            crate::StoreQuery {
                field_values: BTreeMap::from([(
                    "status.phase".into(),
                    ["Pending", "Active", "Interrupted", "Landing", "Anchored"].map(String::from).to_vec(),
                )]),
                ..Default::default()
            },
            crate::StoreQuery { missing_fields: std::collections::BTreeSet::from(["status.phase".into()]), ..Default::default() },
        ]),
        include_replicas: true,
        ..Default::default()
    }
}

/// What a convoy address names exactly (#4272): the convoy holding role `name` in `project`, or
/// in no project when `project` is `None`; failing that, the record `name`, which is how a convoy
/// row's own address names it. Nothing is completed: a role without a project names a convoy of
/// no project, and a record named like a role does not answer for the role while a convoy holds
/// it. A record name is its record's key in the namespace, so without a project it names that
/// record wherever it sits, and under a project only there.
///
/// This lookup exists because a standing convoy's address names its role while its record is
/// `convoy-<uuid>` or `ensure-<hash>` (#4267 ruling 3); it goes once the engine names a convoy by
/// its address (#3108).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvoyName<'a> {
    pub project: Option<&'a str>,
    pub name: &'a str,
}

impl<'a> ConvoyName<'a> {
    /// The convoy an ADR 0066 convoy address names: its leaf under its project, if any.
    pub fn of(address: &'a flotilla_address::Address) -> Self {
        Self { project: address.get(flotilla_address::Kind::Project), name: address.name() }
    }

    /// The convoy `<name>@<project>` names, or `<name>` (and `<name>@`) of no project.
    pub fn parse(text: &'a str) -> Self {
        match text.split_once('@') {
            Some((name, project)) => Self { project: (!project.is_empty()).then_some(project), name },
            None => Self { project: None, name: text },
        }
    }

    /// The indexed selection of every convoy this name may answer with: the convoys holding the
    /// role in the project, and the record of that name.
    pub fn query(&self) -> crate::StoreQuery {
        let mut labels = BTreeMap::from([(flotilla_resources::ROLE_LABEL.to_owned(), self.name.to_owned())]);
        if let Some(project) = self.project {
            labels.insert(flotilla_resources::PROJECT_LABEL.to_owned(), project.to_owned());
        }
        crate::StoreQuery {
            any_of: Some(vec![crate::StoreQuery::names([self.name.to_owned()], true), crate::StoreQuery::labels(&labels, true, true)]),
            include_replicas: true,
            ..Default::default()
        }
    }

    /// The convoy this name answers with among `rows`, which hold at least what [`Self::query`]
    /// selects. A live holder of the role is preferred to ended ones, and a local row to a
    /// replica of the same record; `Err` when the role is held by several records.
    pub fn select<'r>(
        &self,
        rows: &'r [flotilla_resources::ReadResourceObject<flotilla_resources::Convoy>],
    ) -> Result<Option<&'r flotilla_resources::ReadResourceObject<flotilla_resources::Convoy>>, String> {
        let in_project = |row: &&flotilla_resources::ReadResourceObject<flotilla_resources::Convoy>| {
            let project = row.object.metadata.labels.get(flotilla_resources::PROJECT_LABEL).filter(|project| !project.is_empty());
            project.map(String::as_str) == self.project
        };
        let holders: Vec<_> = rows
            .iter()
            .filter(in_project)
            .filter(|row| row.object.metadata.labels.get(flotilla_resources::ROLE_LABEL).map(String::as_str) == Some(self.name))
            .collect();
        let terminal = |row: &&flotilla_resources::ReadResourceObject<flotilla_resources::Convoy>| {
            row.object.status.as_ref().is_some_and(|status| status.phase.is_terminal())
        };
        let (live, ended): (Vec<_>, Vec<_>) = holders.into_iter().partition(|row| !terminal(row));
        let candidates = if !live.is_empty() {
            live
        } else if !ended.is_empty() {
            ended
        } else {
            let named = |row: &&flotilla_resources::ReadResourceObject<flotilla_resources::Convoy>| row.object.metadata.name == self.name;
            rows.iter().filter(named).filter(|row| self.project.is_none() || in_project(row)).collect()
        };
        let records: std::collections::BTreeSet<&str> = candidates.iter().map(|row| row.object.metadata.name.as_str()).collect();
        if records.len() > 1 {
            let records = records.into_iter().collect::<Vec<_>>().join(", ");
            return Err(format!("convoy `{self}` is held by several records; name one by its record: {records}"));
        }
        Ok(candidates.into_iter().max_by_key(|row| matches!(row.provenance, flotilla_resources::ResourceProvenance::Local)))
    }

    /// The convoy this name answers with in `namespace`, read through one indexed query.
    pub async fn lookup(
        &self,
        backend: &crate::ResourceBackend,
        namespace: &str,
    ) -> Result<Option<flotilla_resources::ReadResourceObject<flotilla_resources::Convoy>>, String> {
        let rows =
            backend.query_declared::<flotilla_resources::Convoy>(namespace, &self.query()).await.map_err(|error| error.to_string())?;
        Ok(self.select(&rows.items)?.cloned())
    }
}

impl std::fmt::Display for ConvoyName<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.project {
            Some(project) => write!(formatter, "{}@{project}", self.name),
            None => formatter.write_str(self.name),
        }
    }
}

/// The record name of the convoy `<name>[@<project>]` names in `namespace`, by [`ConvoyName`]:
/// exact, so a crew command's client completes the name from its context first (#4272).
pub async fn resolve_convoy_record(backend: &crate::ResourceBackend, namespace: &str, name: &str) -> Result<String, String> {
    let convoy = ConvoyName::parse(name);
    match convoy.lookup(backend, namespace).await? {
        Some(convoy) => Ok(convoy.object.metadata.name),
        None => Err(format!("no convoy matches `{convoy}`")),
    }
}

/// The record name of the subject at `address` in `namespace`, or `None` when there is no such
/// subject: [`flotilla_address::record_name`], except for a convoy, whose address names its role
/// and is looked up by [`ConvoyName`] until #3108.
pub async fn subject_record_name(
    backend: &crate::ResourceBackend,
    namespace: &str,
    address: &flotilla_address::Address,
) -> Result<Option<String>, String> {
    if address.kind() == flotilla_address::Kind::Convoy {
        let convoy = ConvoyName::of(address).lookup(backend, namespace).await?;
        return Ok(convoy.map(|convoy| convoy.object.metadata.name));
    }
    Ok(flotilla_address::record_name(address))
}

#[cfg(test)]
mod live_query_tests {
    use super::*;
    use flotilla_resources::{Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, InputMeta};

    // Every serialized phase agrees with is_terminal; missing status is live.
    // The exhaustive name match makes a new variant a compile-time prompt to
    // extend this matrix, rather than silently leaving its query semantics untested.
    #[tokio::test]
    async fn live_query_matches_phase_terminality() {
        let backend = crate::test_store::memory_backend();
        let convoys = backend.using::<Convoy>("ns");
        let mut expected = std::collections::BTreeSet::from(["missing".to_string()]);
        convoys
            .create(&InputMeta::builder().name("missing".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
            .await
            .expect("missing status");
        for phase in [
            ConvoyPhase::Pending,
            ConvoyPhase::Active,
            ConvoyPhase::Interrupted,
            ConvoyPhase::Landing,
            ConvoyPhase::Landed,
            ConvoyPhase::Failed,
            ConvoyPhase::Abandoned,
        ] {
            let name = match phase {
                ConvoyPhase::Pending => "pending",
                ConvoyPhase::Active => "active",
                ConvoyPhase::Interrupted => "interrupted",
                ConvoyPhase::Landing => "landing",
                ConvoyPhase::Landed => "landed",
                ConvoyPhase::Failed => "failed",
                ConvoyPhase::Abandoned => "abandoned",
            };
            let created = convoys
                .create(&InputMeta::builder().name(name.into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
                .await
                .expect("phase record");
            convoys
                .update_status(name, &created.metadata.resource_version, &ConvoyStatus { phase, ..Default::default() })
                .await
                .expect("phase status");
            if !phase.is_terminal() {
                expected.insert(name.into());
            }
        }
        let selected = backend.query_declared::<Convoy>("ns", &live_convoys_query()).await.expect("live query");
        let actual = selected.items.into_iter().map(|row| row.object.metadata.name).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(actual, expected);
    }
}

#[cfg(test)]
mod lookup_tests {
    use super::*;
    use flotilla_resources::{Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, InputMeta, PROJECT_LABEL, ROLE_LABEL};

    /// Creates convoy `record` holding `role` in `project`, labelled as admission labels it: a
    /// convoy of no project carries an empty project label.
    async fn convoy(backend: &crate::ResourceBackend, record: &str, role: &str, project: Option<&str>, phase: Option<ConvoyPhase>) {
        let convoys = backend.clone().using::<Convoy>("ns");
        let labels =
            BTreeMap::from([(ROLE_LABEL.to_owned(), role.to_owned()), (PROJECT_LABEL.to_owned(), project.unwrap_or_default().to_owned())]);
        let spec = ConvoySpec::builder()
            .workflow_ref("workflow".into())
            .maybe_project_ref(project.map(str::to_owned))
            .role(role.to_owned())
            .build();
        let created = convoys.create(&InputMeta::builder().name(record.into()).labels(labels).build(), &spec).await.unwrap();
        if let Some(phase) = phase {
            let status = ConvoyStatus { phase, ..Default::default() };
            convoys.update_status(record, &created.metadata.resource_version, &status).await.unwrap();
        }
    }

    async fn lookup(backend: &crate::ResourceBackend, address: &str) -> Result<Option<String>, String> {
        let address: flotilla_address::Address = address.parse().unwrap();
        subject_record_name(backend, "ns", &address).await
    }

    // Behaviour (#4234, #4272): a convoy address names, exactly, the convoy holding its role in its
    // project, or the record of that name, as a convoy row's own address names it. Nothing is
    // completed: the role under another project, or under none, names nothing, and so does the
    // record under another project. A record name alone is the record's key, so it names it.
    #[tokio::test]
    async fn a_convoy_address_names_a_role_or_a_record_in_its_project() {
        let backend = crate::test_store::memory_backend();
        convoy(&backend, "convoy-0123", "reviewer", Some("flotilla"), None).await;
        convoy(&backend, "convoy-0456", "lander", None, None).await;
        for (address, record) in [
            ("project:flotilla/convoy:reviewer", Some("convoy-0123")),
            ("project:flotilla/convoy:convoy-0123", Some("convoy-0123")),
            ("convoy:lander", Some("convoy-0456")),
            ("convoy:convoy-0456", Some("convoy-0456")),
            ("convoy:reviewer", None),
            ("convoy:convoy-0123", Some("convoy-0123")),
            ("project:other/convoy:reviewer", None),
            ("project:other/convoy:convoy-0123", None),
            ("project:flotilla/convoy:lander", None),
        ] {
            assert_eq!(lookup(&backend, address).await, Ok(record.map(str::to_owned)), "{address}");
        }
    }

    // Behaviour (#4272): a record named like a role does not answer for that role while a convoy
    // holds it. A live holder answers ahead of ended ones; ended holders answer when none is live,
    // and several ended records of the role are listed rather than one guessed.
    #[tokio::test]
    async fn the_role_answers_ahead_of_a_record_named_like_it() {
        let backend = crate::test_store::memory_backend();
        convoy(&backend, "lander", "coder", Some("flotilla"), None).await;
        convoy(&backend, "convoy-1", "lander", Some("flotilla"), Some(ConvoyPhase::Landed)).await;
        convoy(&backend, "convoy-2", "lander", Some("flotilla"), Some(ConvoyPhase::Active)).await;
        assert_eq!(lookup(&backend, "project:flotilla/convoy:lander").await, Ok(Some("convoy-2".into())));
        assert_eq!(lookup(&backend, "project:flotilla/convoy:coder").await, Ok(Some("lander".into())));

        convoy(&backend, "convoy-3", "reviewer", Some("flotilla"), Some(ConvoyPhase::Landed)).await;
        assert_eq!(lookup(&backend, "project:flotilla/convoy:reviewer").await, Ok(Some("convoy-3".into())));
        convoy(&backend, "convoy-4", "reviewer", Some("flotilla"), Some(ConvoyPhase::Abandoned)).await;
        let ambiguous = lookup(&backend, "project:flotilla/convoy:reviewer").await.unwrap_err();
        assert_eq!(ambiguous, "convoy `reviewer@flotilla` is held by several records; name one by its record: convoy-3, convoy-4");
    }

    // The text spelling names the same convoy as the address: `<name>@<project>`, or `<name>` (and
    // `<name>@`) of no project.
    #[test]
    fn the_text_spelling_parses_as_name_and_project() {
        assert_eq!(ConvoyName::parse("lander@flotilla"), ConvoyName { project: Some("flotilla"), name: "lander" });
        assert_eq!(ConvoyName::parse("lander"), ConvoyName { project: None, name: "lander" });
        assert_eq!(ConvoyName::parse("lander@"), ConvoyName { project: None, name: "lander" });
        let address: flotilla_address::Address = "project:flotilla/convoy:lander".parse().unwrap();
        assert_eq!(ConvoyName::of(&address), ConvoyName::parse("lander@flotilla"));
    }
}
