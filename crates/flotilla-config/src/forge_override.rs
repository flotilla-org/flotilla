//! Fleet-local provider substitutions; these never redefine forge identity.
use serde::{Deserialize, Serialize};

/// Only the Forgejo adapter currently supports rehearsal substitution.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum ForgeOverride {
    Forgejo { endpoint_url: String, token_path: String },
}

impl ForgeOverride {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Forgejo { endpoint_url, token_path } => {
                let url = url::Url::parse(endpoint_url).map_err(|_| "forge override endpoint_url must be an HTTPS URL")?;
                if url.scheme() != "https"
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                    || endpoint_url.contains(char::is_whitespace)
                    || endpoint_url.starts_with("https:///")
                {
                    return Err("forge override endpoint_url requires HTTPS, a host and optional mount path".into());
                }
                if !std::path::Path::new(token_path).is_absolute() {
                    return Err("forge override token_path must be absolute".into());
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::generators as gs;

    // Bindings are opt-in daemon configuration; previous daemon configs have
    // none. A binding accepts HTTPS mounts and absolute token files only.
    #[hegel::test]
    fn bindings_validate_execution_without_redefining_identity(tc: hegel::TestCase) {
        // Empty, root and nested mount paths, URL suffix hazards, and relative
        // credential paths cross the valid/invalid boundaries explicitly.
        let endpoints = [
            "https://fake",
            "https://fake/github",
            "https://fake/lab/nested/",
            "http://fake",
            "https:///mount",
            "https://fake?q=1",
            "https://fake#fragment",
        ];
        let endpoint = tc.draw(gs::integers::<usize>().min_value(0).max_value(endpoints.len() - 1));
        let absolute = tc.draw(gs::booleans());
        let binding = ForgeOverride::Forgejo {
            endpoint_url: endpoints[endpoint].into(),
            token_path: if absolute { "/tokens/fake" } else { "tokens/fake" }.into(),
        };
        assert_eq!(binding.validate().is_ok(), endpoint < 3 && absolute);
        let old: crate::DaemonConfig = toml::from_str("machine_id = 'copied'").expect("old daemon config");
        assert!(old.forge_overrides.is_empty());
    }
}
