//! Native issue references and tracker mission-field decoding.
use flotilla_protocol::{ClassOfService, IssueRef, MissionFields};
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

pub(crate) fn class(value: &str) -> Result<ClassOfService, String> {
    match value.to_ascii_lowercase().as_str() {
        "expedite" => Ok(ClassOfService::Expedite),
        "standard" => Ok(ClassOfService::Standard),
        "background" => Ok(ClassOfService::Background),
        _ => Err(format!("invalid class of service: {value}")),
    }
}

/// Preserve missing fields for per-attribute fallback. Invalid/duplicate known
/// fields are errors; unrelated organization fields are deliberately ignored.
pub fn parse_mission_fields(values: &[serde_json::Value]) -> Result<MissionFields, String> {
    let mut fields = MissionFields::default();
    let mut seen = BTreeSet::new();
    for value in values {
        let name = value["issue_field_name"].as_str().ok_or("issue field lacks name")?;
        if !matches!(name, "Value" | "Class of service" | "Crew limit") {
            continue;
        }
        if !seen.insert(name) {
            return Err(format!("duplicate mission field {name}"));
        }
        if value["value"].is_null() {
            continue;
        }
        match name {
            "Value" => fields.value = Some(value["value"].as_f64().ok_or("mission Value must be numeric")?.try_into()?),
            "Crew limit" => {
                fields.crew_limit = Some(
                    value["value"]
                        .as_f64()
                        .filter(|v| *v >= 0.0 && *v <= f64::from(u32::MAX) && v.fract() == 0.0)
                        .map(|v| v as u32)
                        .ok_or("mission Crew limit must be a nonnegative u32")?,
                )
            }
            _ => {
                fields.class_of_service = Some(class(
                    value["single_select_option"]["name"]
                        .as_str()
                        .or_else(|| value["value"].as_str())
                        .ok_or("mission class lacks option name")?,
                )?)
            }
        }
    }
    Ok(fields)
}
