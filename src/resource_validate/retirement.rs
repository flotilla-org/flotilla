use std::collections::{BTreeMap, BTreeSet};

use color_eyre::Result;
use flotilla_resources::{
    builtin_workflow_templates, current_builtin_workflow_name, K8sResourceObject, Project, ResourceObject, WorkflowTemplate,
    MANAGED_BY_LABEL,
};
use serde::Serialize;
use serde_json::Value;

/// An advisory snapshot of startup's irreversible retirement effects. Manifests
/// retain authoring metadata and specs, but omit status and server-owned identity.
#[derive(Debug, Serialize)]
pub(super) struct RetirementPreview {
    pub restoration_note: &'static str,
    pub definitions: Vec<Value>,
    pub references: Vec<Value>,
}

pub(super) fn preview(templates: &[Value], projects: &[Value], designations: &[Value]) -> Result<RetirementPreview> {
    let current: BTreeSet<_> = builtin_workflow_templates().into_iter().map(|(name, _)| name).collect();
    let templates = templates
        .iter()
        .map(|document| {
            let object: K8sResourceObject<WorkflowTemplate> = serde_json::from_value(document.clone())?;
            Ok(ResourceObject::from_k8s_object(object)?)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut definitions = Vec::new();
    let mut retired = BTreeSet::new();
    for template in &templates {
        if template.metadata.labels.get(MANAGED_BY_LABEL).is_some_and(|value| value == "builtin")
            && !current.contains(template.metadata.name.as_str())
        {
            retired.insert((template.metadata.namespace.clone(), template.metadata.name.clone()));
            definitions.push(serde_json::json!({
                "apiVersion": "flotilla.work/v1", "kind": "WorkflowTemplate",
                "metadata": {"name": template.metadata.name, "namespace": template.metadata.namespace,
                    "labels": template.metadata.labels, "annotations": template.metadata.annotations},
                "spec": template.spec,
            }));
        }
    }
    let projects = projects
        .iter()
        .map(|document| Ok(ResourceObject::from_k8s_object(serde_json::from_value::<K8sResourceObject<Project>>(document.clone())?)?))
        .collect::<Result<Vec<_>>>()?;
    let mut hierarchies = BTreeMap::new();
    for namespace in retired.iter().map(|(namespace, _)| namespace).collect::<BTreeSet<_>>() {
        // FleetDesignation is a singleton named `fleet`; the merged resolver
        // returns one definition per name (FleetDesignation::validate_spec).
        let fleet = designations
            .iter()
            .find(|d| d["metadata"]["namespace"] == *namespace && d["metadata"]["name"] == flotilla_resources::FLEET_DESIGNATION_NAME)
            .and_then(|d| d["spec"]["project"].as_str())
            .map(str::to_string);
        let declared = projects
            .iter()
            .filter(|p| p.metadata.namespace == *namespace)
            .map(|p| (p.metadata.name.clone(), p.spec.parent.clone()))
            .collect();
        hierarchies.insert(namespace.clone(), flotilla_resources::ProjectHierarchy::new(declared, fleet)?);
    }
    let mut references = Vec::new();
    for project in &projects {
        let name = &project.spec.default_workflow_ref;
        if !retired.contains(&(project.metadata.namespace.clone(), name.clone())) {
            continue;
        }
        let namespace = &project.metadata.namespace;
        let hierarchy = &hierarchies[namespace];
        let mut owners = vec![project.metadata.name.clone()];
        owners.extend(hierarchy.ancestors(&project.metadata.name)?);
        let scoped = owners.iter().map(|owner| flotilla_core::ops_entry::materialized_workflow_name(owner, name)).find(|scoped| {
            templates.iter().any(|t| {
                t.metadata.namespace == *namespace && t.metadata.name == *scoped && !retired.contains(&(namespace.clone(), scoped.clone()))
            })
        });
        let replacement = current_builtin_workflow_name(name);
        let resolution = if scoped.is_some() {
            "project-scoped-definition"
        } else if replacement != name {
            "supported-retired-name-alias"
        } else {
            "no-longer-resolves"
        };
        references.push(serde_json::json!({
            "namespace": namespace, "project": project.metadata.name,
            "default_workflow_ref": name, "resolution": resolution,
            "replacement": scoped.or_else(|| (replacement != name).then(|| replacement.to_string())),
        }));
    }
    definitions.sort_by_key(Value::to_string);
    references.sort_by_key(Value::to_string);
    Ok(RetirementPreview {
        restoration_note:
            "Restore only after daemon rollback: these manifests retain managed-by=builtin; candidate startup will retire them again.",
        definitions,
        references,
    })
}

#[cfg(test)]
mod tests {
    use flotilla_resources::{InputMeta, ProjectSpec, WorkflowTemplateSpec};
    use flotilla_store::{InMemoryBackend, ResourceBackend};

    use super::*;

    // The live orphan and both aliases retire; current builtins and user-owned
    // definitions survive. Preview exports restorable specs without any writes.
    #[tokio::test]
    async fn retirement_preview_preserves_store_and_exports_definitions() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let templates = backend.clone().definitions::<WorkflowTemplate>("fleet");
        let projects = backend.clone().using::<Project>("fleet");
        for (name, builtin) in
            [("orphan", true), ("scratch", true), ("user", false), ("single-agent-contained", true), ("single-agent-trusted", true)]
        {
            let mut meta = InputMeta::builder().name(name.to_string()).build();
            if builtin {
                meta.labels.insert(MANAGED_BY_LABEL.into(), "builtin".into());
            }
            templates.apply(&meta, &WorkflowTemplateSpec::builder().build()).await.expect("seed template");
            projects
                .create(
                    &InputMeta::builder().name(name.to_string()).build(),
                    &ProjectSpec::builder().display_name(name.to_string()).default_workflow_ref(name.to_string()).build(),
                )
                .await
                .expect("seed project");
        }
        let before = templates.list().await.expect("templates");
        let documents: Vec<_> = before.iter().map(|object| serde_json::to_value(object.to_k8s_object()).expect("document")).collect();
        let projects: Vec<_> = projects
            .list()
            .await
            .expect("projects")
            .items
            .iter()
            .map(|object| serde_json::to_value(object.to_k8s_object()).expect("document"))
            .collect();
        let report = preview(&documents, &projects, &[]).expect("preview");
        // Reserved aliases still resolve when no retired record is stored;
        // those references have no retirement effect to report.
        let without_aliases: Vec<_> = documents
            .iter()
            .filter(|d| !matches!(d["metadata"]["name"].as_str(), Some("single-agent-contained" | "single-agent-trusted")))
            .cloned()
            .collect();
        let no_alias_records = preview(&without_aliases, &projects, &[]).expect("missing alias records");
        assert_eq!(no_alias_records.references.len(), 1);
        for alias in ["single-agent-contained", "single-agent-trusted"] {
            assert_eq!(current_builtin_workflow_name(alias), "single-agent");
        }

        assert_eq!(report.definitions.len(), 3);
        assert_eq!(report.references.len(), 3);
        assert_eq!(report.references.iter().filter(|r| r["resolution"] == "supported-retired-name-alias").count(), 2);
        assert!(report.references.iter().any(|r| r["default_workflow_ref"] == "orphan" && r["resolution"] == "no-longer-resolves"));
        assert_eq!(
            serde_json::to_value(&report).expect("report"),
            serde_json::to_value(preview(&documents, &projects, &[]).expect("repeat preview")).expect("repeat report")
        );
        // A surviving Project-scoped definition keeps precedence over a retired global.
        let mut scoped = documents.iter().find(|d| d["metadata"]["name"] == "user").expect("user").clone();
        scoped["metadata"]["name"] = serde_json::json!("orphan--orphan");
        let mut with_scoped = documents.clone();
        with_scoped.push(scoped);
        let scoped_report = preview(&with_scoped, &projects, &[]).expect("scoped preview");
        assert!(scoped_report.references.iter().any(|r| r["default_workflow_ref"] == "orphan"
            && r["resolution"] == "project-scoped-definition"
            && r["replacement"] == "orphan--orphan"));
        // A Project in another namespace is unaffected by retirement here.
        let mut other = projects[0].clone();
        other["metadata"]["namespace"] = serde_json::json!("other");
        assert!(preview(&documents, &[other], &[]).expect("other namespace").references.is_empty());
        for definition in &report.definitions {
            flotilla_store::validate_resource_document(definition).expect("restoration manifest");
            assert!(definition.get("status").is_none());
            assert!(definition["metadata"].get("uid").is_none());
            let original = before.iter().find(|t| t.metadata.name == definition["metadata"]["name"]).expect("original");
            assert_eq!(definition["spec"], serde_json::to_value(&original.spec).expect("spec"));
        }
        assert_eq!(
            documents,
            templates
                .list()
                .await
                .expect("unchanged templates")
                .iter()
                .map(|object| serde_json::to_value(object.to_k8s_object()).expect("document"))
                .collect::<Vec<_>>()
        );
    }

    // Explicit generator spans empty inventories, repeats, namespaces, current
    // builtins, user definitions and retired definitions. Preview never edits inputs.
    #[hegel::test]
    fn ownership_and_namespace_boundaries(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(8));
        let mut documents = Vec::new();
        let mut expected = 0;
        for index in 0..count {
            let builtin = tc.draw(gs::booleans());
            let current = tc.draw(gs::booleans());
            let name = if current { "scratch".to_string() } else { format!("retired-{index}") };
            let namespace = if tc.draw(gs::booleans()) { "one" } else { "two" };
            if builtin && !current {
                expected += 1;
            }
            documents.push(serde_json::json!({"apiVersion":"flotilla.work/v1", "kind":"WorkflowTemplate",
                "metadata":{"name":name,"namespace":namespace,"resourceVersion":"1","creationTimestamp":"2026-10-07T00:00:00Z","labels":{MANAGED_BY_LABEL:if builtin {"builtin"} else {"user"}}},
                "spec":{}}));
        }
        let before = documents.clone();
        let report = preview(&documents, &[], &[]).expect("preview");
        assert_eq!(report.definitions.len(), expected);
        assert_eq!(documents, before);
        assert!(preview(&[serde_json::json!({"spec":false})], &[], &[]).is_err());
    }
}
