use serde_json::Value;

use crate::{error::ResourceError, http::HttpBackend};

pub async fn ensure_namespace(backend: &HttpBackend, name: &str) -> Result<(), ResourceError> {
    let get_url = format!("{}/api/v1/namespaces/{}", backend.base_url.trim_end_matches('/'), name);
    let response = backend.http.get(&get_url).send().await.map_err(|err| ResourceError::other(format!("GET namespace: {err}")))?;
    if response.status().is_success() {
        return Ok(());
    }
    if response.status() != reqwest::StatusCode::NOT_FOUND {
        return Err(ResourceError::other(format!("GET namespace returned {}", response.status())));
    }

    let post_url = format!("{}/api/v1/namespaces", backend.base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": { "name": name }
    });
    let response =
        backend.http.post(post_url).json(&body).send().await.map_err(|err| ResourceError::other(format!("CREATE namespace: {err}")))?;
    if response.status().is_success() || response.status() == reqwest::StatusCode::CONFLICT {
        Ok(())
    } else {
        Err(ResourceError::other(format!("CREATE namespace returned {}", response.status())))
    }
}

pub async fn ensure_crd(backend: &HttpBackend, crd_yaml: &str) -> Result<(), ResourceError> {
    let mut body: Value = serde_yml::from_str(crd_yaml).map_err(|err| ResourceError::invalid(format!("parse CRD YAML: {err}")))?;
    let name = body
        .get("metadata")
        .and_then(|metadata| metadata.get("name"))
        .and_then(Value::as_str)
        .ok_or_else(|| ResourceError::invalid("CRD YAML missing metadata.name"))?;
    let object_url = format!("{}/apis/apiextensions.k8s.io/v1/customresourcedefinitions/{}", backend.base_url.trim_end_matches('/'), name);
    let response = backend.http.get(&object_url).send().await.map_err(|err| ResourceError::other(format!("GET CRD: {err}")))?;

    if response.status().is_success() {
        let existing: Value = response.json().await.map_err(|err| ResourceError::other(format!("decode existing CRD: {err}")))?;
        if let Some(resource_version) =
            existing.get("metadata").and_then(|metadata| metadata.get("resourceVersion")).and_then(Value::as_str)
        {
            body["metadata"]["resourceVersion"] = Value::String(resource_version.to_string());
        }
        let response =
            backend.http.put(&object_url).json(&body).send().await.map_err(|err| ResourceError::other(format!("UPDATE CRD: {err}")))?;
        if response.status().is_success() {
            return Ok(());
        }
        return Err(ResourceError::other(format!("UPDATE CRD returned {}", response.status())));
    }

    if response.status() != reqwest::StatusCode::NOT_FOUND {
        return Err(ResourceError::other(format!("GET CRD returned {}", response.status())));
    }

    let collection_url = format!("{}/apis/apiextensions.k8s.io/v1/customresourcedefinitions", backend.base_url.trim_end_matches('/'));
    let response =
        backend.http.post(collection_url).json(&body).send().await.map_err(|err| ResourceError::other(format!("CREATE CRD: {err}")))?;
    if response.status().is_success() || response.status() == reqwest::StatusCode::CONFLICT {
        Ok(())
    } else {
        Err(ResourceError::other(format!("CREATE CRD returned {}", response.status())))
    }
}
