//! Flotilla-owned settings are split from Drydock's provider configuration.
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Integration {
    #[serde(default, rename = "provider")]
    _provider: BTreeMap<String, Provider>,
}
/// ADR 0047: `[flotilla.provider.<instance>.lab_reach]` is retired (#4299): what
/// a vessel reaches is its composed reach set, not a per-instance setting. The
/// table is accepted and dropped for one roll; remove this record, and the
/// `flotilla` table with it, one roll after #4299.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Provider {
    #[serde(default, rename = "lab_reach")]
    _retired_lab_reach: Option<serde::de::IgnoredAny>,
}

/// Drydock's provider configuration, with Flotilla's own settings removed.
pub fn split_configuration(text: &str) -> Result<String, String> {
    let mut document: toml::Table = toml::from_str(text).map_err(|error| format!("invalid drydock TOML configuration: {error}"))?;
    #[derive(Deserialize)]
    struct Settings {
        #[serde(default, rename = "flotilla")]
        _flotilla: Integration,
    }
    toml::from_str::<Settings>(text).map_err(|error| format!("invalid Flotilla Drydock settings: {error}"))?;
    document.remove("flotilla");
    // Drydock's table was `provider` before its configuration document became
    // `providers` (drydock#129). Accept the old name for one roll; remove after it.
    if let Some(providers) = document.remove("provider") {
        if document.contains_key("providers") {
            return Err("drydock configuration names both `provider` and `providers`".into());
        }
        document.insert("providers".into(), providers);
    }
    toml::to_string(&document).map_err(|_| "invalid drydock configuration".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::generators as gs;
    #[test]
    fn errors_preserve_source_location() {
        for text in ["[provider\n", "[flotilla.provider.daytona.unknown]\nhostname='host'\n"] {
            let error = split_configuration(text).unwrap_err();
            assert!(error.contains("line ") && error.contains("column "), "{error}");
        }
    }

    // #4299: the retired per-instance lab reach is accepted and dropped, never
    // becoming provider or guest settings, whatever it held. The provider
    // table's earlier name is read as drydock's current one.
    #[hegel::test]
    fn the_retired_lab_reach_is_accepted_and_dropped(tc: hegel::TestCase) {
        let port = tc.draw(gs::integers::<u16>());
        let table = tc.draw(gs::sampled_from(vec!["provider", "providers"]));
        let text = format!(
            "[{table}.daytona]\nmodule='daytona'\n[flotilla.provider.daytona.lab_reach]\nhostname='forgejo.lab.example'\nport={port}\n"
        );
        let provider = split_configuration(&text).unwrap();
        assert!(!provider.contains("flotilla") && !provider.contains("lab"), "{provider}");
        assert!(provider.contains("[providers.daytona]"), "{provider}");
    }
}
