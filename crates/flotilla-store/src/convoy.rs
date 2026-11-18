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

/// Each named standing role's current generation: its newest live convoy, by the ConvoyEnsure name
/// the convoy was ensured from. A role with no live generation is absent. Every host reads the same
/// generations, so the answer needs no status of the ensure's own (#3108).
pub async fn current_generations(
    backend: &crate::ResourceBackend,
    namespace: &str,
    ensures: impl IntoIterator<Item = String>,
) -> Result<BTreeMap<String, flotilla_resources::ResourceObject<flotilla_resources::Convoy>>, flotilla_resources::ResourceError> {
    let names: Vec<String> = ensures.into_iter().collect();
    if names.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows = backend.query_declared::<flotilla_resources::Convoy>(namespace, &generations_query(names)).await?.items;
    Ok(newest_live_generations(rows.iter().map(|row| &row.object)).into_iter().map(|(ensure, convoy)| (ensure, convoy.clone())).collect())
}

/// The live convoys ensured from the named ConvoyEnsures: what [`current_generations`] reads, for
/// a reader that folds it into a wider query.
pub fn generations_query(ensures: Vec<String>) -> crate::StoreQuery {
    let mut query = live_convoys_query();
    query.field_values.insert(format!("metadata.annotations.\"{}\"", flotilla_resources::ENSURED_FROM_ANNOTATION), ensures);
    query
}

/// Each standing role's current generation among `convoys`: its newest live convoy, by the
/// ConvoyEnsure name it was ensured from.
pub fn newest_live_generations<'a>(
    convoys: impl IntoIterator<Item = &'a flotilla_resources::ResourceObject<flotilla_resources::Convoy>>,
) -> BTreeMap<String, &'a flotilla_resources::ResourceObject<flotilla_resources::Convoy>> {
    let mut current = BTreeMap::<String, &flotilla_resources::ResourceObject<flotilla_resources::Convoy>>::new();
    for convoy in convoys {
        let Some(ensure) = convoy.metadata.annotations.get(flotilla_resources::ENSURED_FROM_ANNOTATION) else { continue };
        if convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            continue;
        }
        if current.get(ensure).is_none_or(|held| held.spec.generation < convoy.spec.generation) {
            current.insert(ensure.clone(), convoy);
        }
    }
    current
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
        let identities: Vec<_> = rows.iter().map(|row| ConvoyIdentity::of(&row.object)).collect();
        let selected = self.select_indices(&identities)?;
        Ok(selected
            .into_iter()
            .map(|index| &rows[index])
            .max_by_key(|row| matches!(row.provenance, flotilla_resources::ResourceProvenance::Local)))
    }

    /// The rows among `identities` holding the record this name answers with: every copy of
    /// it, for a caller that weighs them (local or replica, home host); `Err` when the role is
    /// held by several records. [`Self::select`] is this over stored rows.
    pub fn select_indices(&self, identities: &[ConvoyIdentity<'_>]) -> Result<Vec<usize>, String> {
        let in_project = |identity: &ConvoyIdentity<'_>| identity.project.filter(|project| !project.is_empty()) == self.project;
        let holders = || identities.iter().enumerate().filter(|(_, identity)| in_project(identity) && identity.role == Some(self.name));
        let live: Vec<usize> = holders().filter(|(_, identity)| !identity.terminal).map(|(index, _)| index).collect();
        let candidates = if !live.is_empty() {
            live
        } else {
            let ended: Vec<usize> = holders().map(|(index, _)| index).collect();
            if !ended.is_empty() {
                ended
            } else {
                let named =
                    |identity: &ConvoyIdentity<'_>| identity.record == self.name && (self.project.is_none() || in_project(identity));
                identities.iter().enumerate().filter(|(_, identity)| named(identity)).map(|(index, _)| index).collect()
            }
        };
        let records: std::collections::BTreeSet<&str> = candidates.iter().map(|index| identities[*index].record).collect();
        if records.len() > 1 {
            let records = records.into_iter().collect::<Vec<_>>().join(", ");
            return Err(format!("convoy `{self}` is held by several records; name one by its record: {records}"));
        }
        Ok(candidates)
    }

    /// The convoy this plain name answers with in `namespace`: [`Self::lookup_address`]'s, else
    /// [`Self::newest_launch`], so a name whose launches have all retired still names its last.
    pub async fn lookup(
        &self,
        backend: &crate::ResourceBackend,
        namespace: &str,
    ) -> Result<Option<flotilla_resources::ReadResourceObject<flotilla_resources::Convoy>>, String> {
        match self.lookup_address(backend, namespace).await? {
            Some(found) => Ok(Some(found)),
            None => self.newest_launch(backend, namespace).await,
        }
    }

    /// The newest launch holding this name's role in its project, a retired one included: what a
    /// plain name names once no live or unretired launch answers for it.
    pub async fn newest_launch(
        &self,
        backend: &crate::ResourceBackend,
        namespace: &str,
    ) -> Result<Option<flotilla_resources::ReadResourceObject<flotilla_resources::Convoy>>, String> {
        let mut labels = BTreeMap::from([(flotilla_resources::ROLE_LABEL.to_owned(), self.name.to_owned())]);
        if let Some(project) = self.project {
            labels.insert(flotilla_resources::PROJECT_LABEL.to_owned(), project.to_owned());
        }
        backend
            .including_replicas::<flotilla_resources::Convoy>(namespace)
            .newest_labelled(&labels)
            .await
            .map_err(|error| error.to_string())
    }

    /// What a convoy address names in `namespace`, read through one indexed query: a live or
    /// unretired holder of the role, never a retired one; where that selects none, the record of
    /// the name read by name, which gives a retired one too.
    pub async fn lookup_address(
        &self,
        backend: &crate::ResourceBackend,
        namespace: &str,
    ) -> Result<Option<flotilla_resources::ReadResourceObject<flotilla_resources::Convoy>>, String> {
        let rows =
            backend.query_declared::<flotilla_resources::Convoy>(namespace, &self.query()).await.map_err(|error| error.to_string())?;
        if let Some(selected) = self.select(&rows.items)? {
            return Ok(Some(selected.clone()));
        }
        let copies = backend
            .including_replicas::<flotilla_resources::Convoy>(namespace)
            .get_all(self.name)
            .await
            .map_err(|error| error.to_string())?
            .items;
        Ok(self.select(&copies)?.cloned())
    }
}

/// What [`ConvoyName::select_indices`] reads of a convoy row, from a stored convoy or a projected
/// one (the routing table's aggregator rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvoyIdentity<'a> {
    pub record: &'a str,
    pub role: Option<&'a str>,
    /// The project label; empty for a convoy of no project.
    pub project: Option<&'a str>,
    pub terminal: bool,
}

impl<'a> ConvoyIdentity<'a> {
    pub fn of(convoy: &'a flotilla_resources::ResourceObject<flotilla_resources::Convoy>) -> Self {
        let label = |name| convoy.metadata.labels.get(name).map(String::as_str);
        Self {
            record: &convoy.metadata.name,
            role: label(flotilla_resources::ROLE_LABEL),
            project: label(flotilla_resources::PROJECT_LABEL),
            terminal: convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()),
        }
    }

    /// The convoy `document` is, as a resource table reads it.
    pub fn of_document(document: &'a serde_json::Value) -> Option<Self> {
        let metadata = &document["metadata"];
        let label = |name: &str| metadata["labels"][name].as_str();
        let phase = &document["status"]["phase"];
        Some(Self {
            record: metadata["name"].as_str()?,
            role: label(flotilla_resources::ROLE_LABEL),
            project: label(flotilla_resources::PROJECT_LABEL),
            terminal: !phase.is_null()
                && <flotilla_resources::ConvoyPhase as serde::Deserialize>::deserialize(phase).is_ok_and(|phase| phase.is_terminal()),
        })
    }

    /// The convoy's canonical address, the one [`ConvoyName`] selects it by: a live convoy's role
    /// under its project, as the CLI prints it, and an ended convoy's record, since its role names
    /// any live holder ahead of it. A record named like a role in its project would name that
    /// role's holder; record names are `convoy-<uuid>` and `ensure-<hash>`, which no role is.
    pub fn address(&self) -> Option<flotilla_address::Address> {
        let name = self.role.filter(|role| !self.terminal && !role.is_empty()).unwrap_or(self.record);
        match self.project.filter(|project| !project.is_empty()) {
            Some(project) => {
                flotilla_address::Address::project(project).and_then(|project| project.child(flotilla_address::Kind::Convoy, name)).ok()
            }
            None => flotilla_address::Address::convoy(name).ok(),
        }
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

/// Registers the convoy subject table (#4355): `convoys` rows, each addressed as
/// [`ConvoyName`] selects it ([`ConvoyIdentity::address`]), so a row's address resolves to that
/// row's convoy.
pub fn register_subjects(registry: &mut flotilla_verbs::VerbRegistry) -> Result<(), flotilla_verbs::Duplicate> {
    registry.register_subject(flotilla_verbs::SubjectTable {
        kind: flotilla_address::Kind::Convoy,
        table: <flotilla_resources::Convoy as flotilla_resources::Resource>::API_PATHS.plural,
        address: |document| ConvoyIdentity::of_document(document)?.address(),
    })
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
        let convoy = ConvoyName::of(address).lookup_address(backend, namespace).await?;
        return Ok(convoy.map(|convoy| convoy.object.metadata.name));
    }
    Ok(flotilla_address::record_name(address))
}

/// Writes `patch` to this home's notes on convoy `name` as its readers now see it (#3108 C3'), and
/// returns the convoy with the notes as they now stand. See [`flotilla_resources::convoy::engine::note`].
pub async fn note_convoy<P: flotilla_resources::StatusPatch<flotilla_resources::ConvoyStatus> + ?Sized>(
    path: &flotilla_resources::convoy::engine::EngineConvoyPath,
    convoys: &crate::TypedResolver<flotilla_resources::Convoy>,
    name: &str,
    patch: &P,
) -> Result<flotilla_resources::ResourceObject<flotilla_resources::Convoy>, String> {
    let convoy = convoys.get(name).await.map_err(|error| error.to_string())?;
    flotilla_resources::convoy::engine::note(path, &convoy, patch)
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
            .create(
                &InputMeta::builder().name("missing".into()).build(),
                &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("ns".into()).build(),
            )
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
                .create(
                    &InputMeta::builder().name(name.into()).build(),
                    &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("ns".into()).build(),
                )
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

    /// Creates convoy `record` holding `role` in `project`, labelled as admission labels it.
    async fn convoy(backend: &crate::ResourceBackend, record: &str, role: &str, project: &str, phase: Option<ConvoyPhase>) {
        let convoys = backend.clone().using::<Convoy>("ns");
        let labels = BTreeMap::from([(ROLE_LABEL.to_owned(), role.to_owned()), (PROJECT_LABEL.to_owned(), project.to_owned())]);
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(project.to_owned()).role(role.to_owned()).build();
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
        convoy(&backend, "convoy-0123", "reviewer", "flotilla", None).await;
        convoy(&backend, "convoy-0456", "lander", "other", None).await;
        for (address, record) in [
            ("project:flotilla/convoy:reviewer", Some("convoy-0123")),
            ("project:flotilla/convoy:convoy-0123", Some("convoy-0123")),
            ("project:other/convoy:lander", Some("convoy-0456")),
            ("convoy:lander", None),
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
        convoy(&backend, "lander", "coder", "flotilla", None).await;
        convoy(&backend, "convoy-1", "lander", "flotilla", Some(ConvoyPhase::Landed)).await;
        convoy(&backend, "convoy-2", "lander", "flotilla", Some(ConvoyPhase::Active)).await;
        assert_eq!(lookup(&backend, "project:flotilla/convoy:lander").await, Ok(Some("convoy-2".into())));
        assert_eq!(lookup(&backend, "project:flotilla/convoy:coder").await, Ok(Some("lander".into())));

        convoy(&backend, "convoy-3", "reviewer", "flotilla", Some(ConvoyPhase::Landed)).await;
        assert_eq!(lookup(&backend, "project:flotilla/convoy:reviewer").await, Ok(Some("convoy-3".into())));
        convoy(&backend, "convoy-4", "reviewer", "flotilla", Some(ConvoyPhase::Abandoned)).await;
        let ambiguous = lookup(&backend, "project:flotilla/convoy:reviewer").await.unwrap_err();
        assert_eq!(ambiguous, "convoy `reviewer@flotilla` is held by several records; name one by its record: convoy-3, convoy-4");
    }

    // #2767, #4272: routing and the executor select through `select_indices`. Terminal history
    // never displaces a live holder, and a record name still names history. Exhaustive finite
    // generator: with and without a project, all six orders of two ended namesakes and a live
    // holder; a second live holder is refused, naming both live records and neither ended one.
    #[test]
    fn a_live_holder_answers_ahead_of_ended_namesakes_in_any_order() {
        for project in [None, Some("p")] {
            let role = ConvoyName { project, name: "governor" };
            for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
                let names = ["old-a", "old-b", "live"];
                let identities =
                    order.map(|index| ConvoyIdentity { record: names[index], role: Some("governor"), project, terminal: index < 2 });
                let records = |indices: Vec<usize>| indices.into_iter().map(|index| identities[index].record).collect::<Vec<_>>();
                assert_eq!(records(role.select_indices(&identities).unwrap()), ["live"]);
                assert_eq!(records(ConvoyName { project, name: "old-a" }.select_indices(&identities).unwrap()), ["old-a"]);

                let mut ambiguous = Vec::from(identities);
                ambiguous.push(ConvoyIdentity { record: "another-live", role: Some("governor"), project, terminal: false });
                let error = role.select_indices(&ambiguous).unwrap_err();
                let named = error.split(|c: char| !c.is_ascii_alphanumeric() && c != '-').collect::<std::collections::BTreeSet<_>>();
                assert!(named.contains("another-live") && named.contains("live"), "{error}");
                assert!(!named.contains("old-a") && !named.contains("old-b"), "{error}");
            }
            assert_eq!(role.select_indices(&[]), Ok(vec![]));
        }
    }

    // Behaviour (#4199, #4272): a convoy's canonical address, a table row's own, names that
    // convoy and no other. Exhaustive finite generator, with and without a project: a live record
    // named `lander` holding `coder`, two holders of `lander`, a holder of `reviewer` and a convoy
    // of no role, over every live/ended assignment that leaves `lander` at most one live holder.
    #[test]
    fn a_convoys_canonical_address_selects_that_convoy() {
        for project in [None, Some("flotilla")] {
            for ended in 0..16u8 {
                let ended = |bit: u8| ended & (1 << bit) != 0;
                if !ended(0) && !ended(1) {
                    continue;
                }
                let identity = |record, role, terminal| ConvoyIdentity { record, role, project, terminal };
                let identities = [
                    identity("lander", Some("coder"), false),
                    identity("convoy-1", Some("lander"), ended(0)),
                    identity("convoy-2", Some("lander"), ended(1)),
                    identity("convoy-3", Some("reviewer"), ended(2)),
                    identity("convoy-4", None, ended(3)),
                ];
                for convoy in &identities {
                    let address = convoy.address().unwrap();
                    let selected = ConvoyName::of(&address).select_indices(&identities).unwrap();
                    assert_eq!(selected.iter().map(|index| identities[*index].record).collect::<Vec<_>>(), [convoy.record], "{address}");
                }
            }
        }
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
