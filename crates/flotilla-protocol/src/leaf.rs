use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Leaf {
    pub address: LeafAddress,
    pub field_path: String,
    pub operator: LeafOperator,
    pub literal: String,
}

impl FromStr for Leaf {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let mut parts = input.split_whitespace();
        let address = parts.next().ok_or_else(|| leaf_syntax_error(input))?.parse()?;
        let field_path = parts.next().ok_or_else(|| leaf_syntax_error(input))?.to_string();
        let operator = parts.next().ok_or_else(|| leaf_syntax_error(input))?.parse()?;
        let literal = parts.collect::<Vec<_>>().join(" ");
        if literal.is_empty() {
            return Err(leaf_syntax_error(input));
        }
        let literal = literal
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| literal.strip_prefix('\'').and_then(|value| value.strip_suffix('\'')))
            .unwrap_or(&literal)
            .to_string();
        Ok(Self { address, field_path, operator, literal })
    }
}

fn leaf_syntax_error(input: &str) -> String {
    format!("invalid leaf `{input}`; expected `<kind>/<name> <field-path> <operator> <literal>`")
}

impl fmt::Display for Leaf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {} {}", self.address, self.field_path, self.operator, self.literal)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LeafAddress {
    Convoy { name: String },
    Vessel { name: String },
    Work { convoy: String, work: String },
    ChangeRequest { service: String, scope: String, number: u64 },
}

impl LeafAddress {
    pub fn kind(&self) -> LeafKind {
        match self {
            Self::Convoy { .. } => LeafKind::Convoy,
            Self::Vessel { .. } => LeafKind::Vessel,
            Self::Work { .. } => LeafKind::Work,
            Self::ChangeRequest { .. } => LeafKind::ChangeRequest,
        }
    }
}

impl FromStr for LeafAddress {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parts = value.split('/').collect::<Vec<_>>();
        match parts.as_slice() {
            ["convoy", name] if !name.is_empty() => Ok(Self::Convoy { name: (*name).to_string() }),
            ["vessel", name] if !name.is_empty() => Ok(Self::Vessel { name: (*name).to_string() }),
            ["work", convoy, work] if !convoy.is_empty() && !work.is_empty() => {
                Ok(Self::Work { convoy: (*convoy).to_string(), work: (*work).to_string() })
            }
            ["cr", service, scope @ .., number] if !service.is_empty() && !scope.is_empty() => {
                let number = number.parse().map_err(|_| format!("invalid change request number `{number}` in leaf address `{value}`"))?;
                Ok(Self::ChangeRequest { service: (*service).to_string(), scope: scope.join("/"), number })
            }
            _ => Err(format!(
                "invalid leaf address `{value}`; expected convoy/<name>, vessel/<name>, work/<convoy>/<work>, or cr/<service>/<scope>/<number>"
            )),
        }
    }
}

impl fmt::Display for LeafAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Convoy { name } => write!(f, "convoy/{name}"),
            Self::Vessel { name } => write!(f, "vessel/{name}"),
            Self::Work { convoy, work } => write!(f, "work/{convoy}/{work}"),
            Self::ChangeRequest { service, scope, number } => write!(f, "cr/{service}/{scope}/{number}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeafKind {
    Convoy,
    Vessel,
    Work,
    ChangeRequest,
}

impl fmt::Display for LeafKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Convoy => "convoy",
            Self::Vessel => "vessel",
            Self::Work => "work",
            Self::ChangeRequest => "cr",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LeafOperator {
    #[serde(rename = "==")]
    Equal,
    #[serde(rename = "!=")]
    NotEqual,
    #[serde(rename = "<")]
    LessThan,
    #[serde(rename = "<=")]
    LessThanOrEqual,
    #[serde(rename = ">")]
    GreaterThan,
    #[serde(rename = ">=")]
    GreaterThanOrEqual,
}

impl FromStr for LeafOperator {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "==" => Ok(Self::Equal),
            "!=" => Ok(Self::NotEqual),
            "<" => Ok(Self::LessThan),
            "<=" => Ok(Self::LessThanOrEqual),
            ">" => Ok(Self::GreaterThan),
            ">=" => Ok(Self::GreaterThanOrEqual),
            _ => Err(format!("unknown leaf operator `{value}`; admitted operators: ==, !=, <, <=, >, >=")),
        }
    }
}

impl fmt::Display for LeafOperator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Equal => "==",
            Self::NotEqual => "!=",
            Self::LessThan => "<",
            Self::LessThanOrEqual => "<=",
            Self::GreaterThan => ">",
            Self::GreaterThanOrEqual => ">=",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitSubscriptionRequest {
    pub namespace: String,
    pub leaves: Vec<Leaf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness_demand: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeafFire {
    pub subscription_id: uuid::Uuid,
    /// Daemon-internal delivery address; never crosses the wire.
    #[serde(skip)]
    pub watcher_id: uuid::Uuid,
    pub leaf: Leaf,
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_wait_leaf() {
        let leaf: Leaf = "convoy/demo .status.phase == Landed".parse().expect("parse leaf");
        assert_eq!(leaf.address, LeafAddress::Convoy { name: "demo".to_string() });
        assert_eq!(leaf.field_path, ".status.phase");
        assert_eq!(leaf.operator, LeafOperator::Equal);
        assert_eq!(leaf.literal, "Landed");
        assert_eq!(leaf.to_string(), "convoy/demo .status.phase == Landed");
    }

    #[test]
    fn parses_work_claim_address_and_quoted_literal() {
        let leaf: Leaf = "work/demo/implement .latest-claim.disposition == 'changes pushed'".parse().expect("parse claim leaf");
        assert_eq!(leaf.address, LeafAddress::Work { convoy: "demo".to_string(), work: "implement".to_string() });
        assert_eq!(leaf.literal, "changes pushed");
    }

    #[test]
    fn parses_subject_keyed_change_request_address() {
        let leaf: Leaf = "cr/github.com/flotilla-org/flotilla/1363 .state == merged".parse().expect("parse CR leaf");
        assert_eq!(leaf.address, LeafAddress::ChangeRequest {
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 1363,
        });
        assert_eq!(leaf.to_string(), "cr/github.com/flotilla-org/flotilla/1363 .state == merged");
    }

    #[test]
    fn rejects_change_request_collection_and_query_addresses() {
        for address in [
            "cr/github.com/flotilla-org/flotilla",
            "cr/github.com/flotilla-org/flotilla/*",
            "cr/github.com/flotilla-org/flotilla?state=open",
        ] {
            assert!(address.parse::<LeafAddress>().is_err(), "collection or query address must be rejected: {address}");
        }
    }
}
