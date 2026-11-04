use std::{collections::BTreeSet, fmt, str::FromStr};

use sha2::{Digest, Sha256};

pub fn convoy_disambiguation_address(role: &str, project: Option<&str>) -> String {
    format!("{role}@{}", project.unwrap_or_default())
}
pub fn parse_role_address(value: &str) -> Result<(&str, Option<&str>), String> {
    match value.split_once('@') {
        Some((role, project)) if !role.is_empty() && !project.contains('@') => Ok((role, Some(project))),
        Some(_) => Err(format!("invalid convoy address `{value}`: expected role@project")),
        None if value.is_empty() => Err("convoy role cannot be empty".to_string()),
        None => Ok((value, None)),
    }
}
pub struct ConvoyAddressIdentity<'a> {
    pub record_name: &'a str,
    pub role: Option<&'a str>,
    pub project: Option<&'a str>,
    pub terminal: bool,
}
pub fn resolve_convoy_candidate_indices(identities: &[ConvoyAddressIdentity<'_>], address: &str) -> Result<Vec<usize>, String> {
    let exact = identities
        .iter()
        .enumerate()
        .filter_map(|(index, identity)| (identity.record_name == address).then_some(index))
        .collect::<Vec<_>>();
    if !exact.is_empty() {
        return Ok(exact);
    }

    let (role, project) = parse_role_address(address)?;
    let matching = identities
        .iter()
        .enumerate()
        .filter_map(|(index, identity)| {
            (identity.role == Some(role) && project.is_none_or(|project| identity.project.unwrap_or_default() == project)).then_some(index)
        })
        .collect::<Vec<_>>();
    let (live, terminal): (Vec<_>, Vec<_>) = matching.into_iter().partition(|index| !identities[*index].terminal);
    let candidates = if live.is_empty() { terminal } else { live };
    let record_names = candidates.iter().map(|index| identities[*index].record_name).collect::<BTreeSet<_>>();
    if record_names.len() <= 1 {
        return Ok(candidates);
    }

    let address_options = candidates
        .iter()
        .filter_map(|index| identities[*index].role.map(|role| convoy_disambiguation_address(role, identities[*index].project)))
        .collect::<BTreeSet<_>>();
    if candidates.iter().all(|index| identities[*index].terminal) && address_options.len() == 1 {
        return Err(format!(
            "convoy address `{address}` matches multiple terminal records; use an exact record name: {}",
            record_names.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if address_options.len() > 1 {
        return Err(format!(
            "convoy role `{role}` is ambiguous; use one of: {}",
            address_options.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    Err(format!(
        "convoy address `{address}` matches multiple records; use an exact record name: {}",
        record_names.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

pub fn convoy_record_name() -> String {
    format!("convoy-{}", uuid::Uuid::new_v4().simple())
}

pub fn convoy_ensure_name(project: &str, role: &str) -> String {
    let digest = Sha256::digest(format!("{project}\0{role}").as_bytes());
    format!("ensure-{digest:x}")
}

pub fn convoy_address(role: &str, project: Option<&str>) -> String {
    project.map_or_else(|| role.to_string(), |project| format!("{role}@{project}"))
}

/// The stable human-facing address of a convoy role, independent of any one
/// generation's resource record name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoleAddress {
    pub project: String,
    pub role: String,
}

impl FromStr for RoleAddress {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((role, project)) = value.split_once('@') else {
            return Err(format!("invalid role address `{value}`: expected role@project"));
        };
        if role.is_empty() || project.is_empty() || project.contains('@') {
            return Err(format!("invalid role address `{value}`: expected role@project"));
        }
        Ok(Self { project: project.to_string(), role: role.to_string() })
    }
}

impl fmt::Display for RoleAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.role, self.project)
    }
}

#[cfg(test)]
mod naming_tests {
    use super::*;
    // A qualified role address preserves the project/role pair and refuses incomplete or ambiguous input.
    #[test]
    fn qualified_role_address_is_a_typed_project_role_pair() {
        assert_eq!(
            RoleAddress::from_str("governor@andamento"),
            Ok(RoleAddress { project: "andamento".to_string(), role: "governor".to_string() })
        );
        for value in ["governor", "@andamento", "governor@", "governor@andamento@extra"] {
            assert!(RoleAddress::from_str(value).is_err(), "{value} must not produce a qualified address");
        }
    }

    // Naming retains the projectless spelling independently of disambiguation spelling.
    #[test]
    fn role_names_preserve_projectless_and_scoped_spelling() {
        assert_eq!(convoy_address("coder", None), "coder");
        assert_eq!(convoy_address("coder", Some("fleet")), "coder@fleet");
        assert_eq!(convoy_disambiguation_address("coder", None), "coder@");
    }
}
