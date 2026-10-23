//! Flotilla-owned settings are split from Drydock's provider configuration.
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LabReach {
    pub hostname: String,
    pub port: u16,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Integration {
    #[serde(default)]
    provider: BTreeMap<String, Provider>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Provider {
    lab_reach: Option<LabReach>,
}

pub fn split_configuration(text: &str) -> Result<(String, BTreeMap<String, LabReach>), String> {
    let mut document: toml::Table = toml::from_str(text).map_err(|_| "invalid drydock TOML configuration")?;
    let integration: Integration = document
        .remove("flotilla")
        .map(|value| value.try_into())
        .transpose()
        .map_err(|_| "invalid Flotilla Drydock settings")?
        .unwrap_or_default();
    let mut reach = BTreeMap::new();
    for (name, provider) in integration.provider {
        if let Some(lab) = provider.lab_reach {
            if lab.hostname.is_empty() || lab.hostname.contains(['\n', '\r', ' ', '\t']) || lab.port == 0 {
                return Err("lab reach requires a hostname and nonzero port".into());
            }
            reach.insert(name, lab);
        }
    }
    Ok((toml::to_string(&document).map_err(|_| "invalid drydock configuration")?, reach))
}

#[cfg(test)]
mod tests {
    use super::*;
    // #3765 B2: controller reach is host-local and never becomes provider or
    // guest settings. Generated ports cover invalid zero and the valid range.
    #[hegel::test]
    fn lab_reach_is_split_from_provider_settings(tc: hegel::TestCase) {
        let port = tc.draw(hegel::generators::integers::<u16>());
        let text = format!("[provider.daytona]\nmodule='daytona'\n[flotilla.provider.daytona.lab_reach]\nhostname='forgejo.lab.flotilla.work'\nport={port}\n");
        let result = split_configuration(&text);
        if port == 0 {
            assert!(result.is_err());
        } else {
            let (provider, reach) = result.unwrap();
            assert!(!provider.contains("flotilla"));
            assert!(provider.contains("daytona"));
            assert_eq!(reach["daytona"].port, port);
        }
    }
}
