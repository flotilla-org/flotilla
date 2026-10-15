//! Native issue references and tracker mission-field decoding.
use flotilla_protocol::{IssueRef, MissionFields, Priority};
use std::collections::BTreeSet;

pub fn issue_ref_from_url(url: &str) -> Result<IssueRef, String> {
    let url = url::Url::parse(url).map_err(|e| e.to_string())?;
    let parts = url.path_segments().ok_or("issue URL lacks path")?.collect::<Vec<_>>();
    if parts.len() != 4 || parts[2] != "issues" {
        return Err("invalid native issue URL".into());
    }
    Ok(IssueRef {
        source: flotilla_protocol::IssueSource { service: url.origin().ascii_serialization(), scope: format!("{}/{}", parts[0], parts[1]) },
        id: parts[3].into(),
    })
}

/// Priority names are case-insensitive, with surrounding whitespace ignored.
pub fn priority(value: &str) -> Result<Priority, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "urgent" => Ok(Priority::Urgent),
        "high" => Ok(Priority::High),
        "medium" => Ok(Priority::Medium),
        "low" => Ok(Priority::Low),
        _ => Err(format!("invalid priority: {value}")),
    }
}

/// Missing/null fields fall back; invalid or duplicate known fields fail closed.
/// Retired mission fields and unrelated organization fields are ignored.
pub fn parse_mission_fields(values: &[serde_json::Value]) -> Result<MissionFields, String> {
    let mut fields = MissionFields::default();
    let mut seen = BTreeSet::new();
    for value in values {
        let name = value["issue_field_name"].as_str().ok_or("issue field lacks name")?;
        if !matches!(name, "Priority" | "Weight") {
            continue;
        }
        if !seen.insert(name) {
            return Err(format!("duplicate mission field {name}"));
        }
        if value["value"].is_null() {
            continue;
        }
        match name {
            "Weight" => fields.weight = Some(value["value"].as_f64().ok_or("Weight must be numeric")?.try_into()?),
            _ => {
                fields.priority = Some(priority(
                    value["single_select_option"]["name"]
                        .as_str()
                        .or_else(|| value["value"].as_str())
                        .ok_or("Priority lacks option name")?,
                )?)
            }
        }
    }
    Ok(fields)
}
