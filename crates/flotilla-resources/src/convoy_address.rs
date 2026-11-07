use std::{fmt, str::FromStr};

use sha2::{Digest, Sha256};

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
    }
}
