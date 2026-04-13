use std::{fs, path::Path};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::{Certificate, Client, Identity};
use serde::Deserialize;

use crate::{error::ResourceError, http::HttpBackend};

#[derive(Debug, Deserialize)]
pub struct Kubeconfig {
    #[serde(rename = "current-context")]
    current_context: String,
    clusters: Vec<NamedCluster>,
    contexts: Vec<NamedContext>,
    users: Vec<NamedUser>,
}

#[derive(Debug, Deserialize)]
struct NamedCluster {
    name: String,
    cluster: Cluster,
}

#[derive(Debug, Deserialize)]
struct Cluster {
    server: String,
    #[serde(rename = "certificate-authority")]
    certificate_authority: Option<String>,
    #[serde(rename = "certificate-authority-data")]
    certificate_authority_data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NamedContext {
    name: String,
    context: Context,
}

#[derive(Debug, Deserialize)]
struct Context {
    cluster: String,
    user: String,
}

#[derive(Debug, Deserialize)]
struct NamedUser {
    name: String,
    user: User,
}

#[derive(Debug, Deserialize)]
struct User {
    #[serde(rename = "client-certificate")]
    client_certificate: Option<String>,
    #[serde(rename = "client-certificate-data")]
    client_certificate_data: Option<String>,
    #[serde(rename = "client-key")]
    client_key: Option<String>,
    #[serde(rename = "client-key-data")]
    client_key_data: Option<String>,
}

pub fn from_kubeconfig(path: impl AsRef<Path>) -> Result<HttpBackend, ResourceError> {
    let path = path.as_ref();
    let yaml = fs::read_to_string(path).map_err(|err| ResourceError::other(format!("read kubeconfig {}: {err}", path.display())))?;
    let kubeconfig: Kubeconfig =
        serde_yml::from_str(&yaml).map_err(|err| ResourceError::invalid(format!("parse kubeconfig {}: {err}", path.display())))?;

    let context = kubeconfig
        .contexts
        .iter()
        .find(|context| context.name == kubeconfig.current_context)
        .ok_or_else(|| ResourceError::invalid(format!("current-context '{}' not found", kubeconfig.current_context)))?;
    let cluster = kubeconfig
        .clusters
        .iter()
        .find(|cluster| cluster.name == context.context.cluster)
        .ok_or_else(|| ResourceError::invalid(format!("cluster '{}' not found", context.context.cluster)))?;
    let user = kubeconfig
        .users
        .iter()
        .find(|user| user.name == context.context.user)
        .ok_or_else(|| ResourceError::invalid(format!("user '{}' not found", context.context.user)))?;

    let mut builder = Client::builder();
    if let Some(ca_bytes) =
        read_pem_bytes(path, cluster.cluster.certificate_authority.as_deref(), cluster.cluster.certificate_authority_data.as_deref())?
    {
        builder = builder.add_root_certificate(
            Certificate::from_pem(&ca_bytes).map_err(|err| ResourceError::invalid(format!("load kubeconfig CA certificate: {err}")))?,
        );
    }
    let cert_bytes = read_pem_bytes(path, user.user.client_certificate.as_deref(), user.user.client_certificate_data.as_deref())?
        .ok_or_else(|| ResourceError::invalid("kubeconfig user missing client certificate"))?;
    let key_bytes = read_pem_bytes(path, user.user.client_key.as_deref(), user.user.client_key_data.as_deref())?
        .ok_or_else(|| ResourceError::invalid("kubeconfig user missing client key"))?;
    let mut identity_pem = cert_bytes;
    if !identity_pem.ends_with(b"\n") {
        identity_pem.push(b'\n');
    }
    identity_pem.extend_from_slice(&key_bytes);
    builder = builder.identity(
        Identity::from_pem(&identity_pem).map_err(|err| ResourceError::invalid(format!("load kubeconfig client identity: {err}")))?,
    );

    let client = builder.build().map_err(|err| ResourceError::other(format!("build reqwest client from kubeconfig: {err}")))?;
    Ok(HttpBackend::new(client, cluster.cluster.server.clone()))
}

fn read_pem_bytes(kubeconfig_path: &Path, path_value: Option<&str>, data_value: Option<&str>) -> Result<Option<Vec<u8>>, ResourceError> {
    if let Some(path_value) = path_value {
        let resolved = if Path::new(path_value).is_absolute() {
            Path::new(path_value).to_path_buf()
        } else {
            kubeconfig_path.parent().unwrap_or_else(|| Path::new(".")).join(path_value)
        };
        let bytes =
            fs::read(&resolved).map_err(|err| ResourceError::other(format!("read kubeconfig PEM {}: {err}", resolved.display())))?;
        return Ok(Some(bytes));
    }
    if let Some(data_value) = data_value {
        let bytes = STANDARD
            .decode(data_value.as_bytes())
            .map_err(|err| ResourceError::invalid(format!("decode kubeconfig base64 data: {err}")))?;
        return Ok(Some(bytes));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::read_pem_bytes;

    #[test]
    fn reads_pem_bytes_from_relative_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let kubeconfig = temp.path().join("config");
        let cert = temp.path().join("cert.pem");
        fs::write(&kubeconfig, "").expect("write kubeconfig");
        fs::write(&cert, b"pem-bytes").expect("write pem");

        let pem = read_pem_bytes(&kubeconfig, Some("cert.pem"), None).expect("read pem");
        assert_eq!(pem.expect("pem bytes"), b"pem-bytes");
    }

    #[test]
    fn reads_pem_bytes_from_inline_base64_data() {
        let temp = tempfile::tempdir().expect("tempdir");
        let kubeconfig = temp.path().join("config");
        fs::write(&kubeconfig, "").expect("write kubeconfig");

        let pem = read_pem_bytes(&kubeconfig, None, Some("cGVtLWJ5dGVz")).expect("read pem");
        assert_eq!(pem.expect("pem bytes"), b"pem-bytes");
    }
}
